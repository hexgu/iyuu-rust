//! HTTP API.
//!
//! Conventions:
//!  * errors are `{error: {kind, message}}` with a status that distinguishes
//!    "you must fix a credential" from "try again later",
//!  * secrets are write-only — they can be set but are never returned,
//!  * long operations return a job id / run id instead of blocking the request.

use crate::config::ConfigPatch;
use crate::error::{Result, ServerError};
use crate::runner::{self, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use iyuu_core::downloader::Downloader;
use iyuu_core::iyuu::IyuuClient;
use iyuu_core::models::RunTrigger;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        // status / meta
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/status", get(status))
        // settings
        .route("/config", get(get_config).put(put_config))
        // connectivity tests
        .route("/test/iyuu", post(test_iyuu))
        .route("/test/downloader", post(test_downloader))
        .route("/sites", get(list_sites))
        .route("/sites/bound", get(list_bound_sites))
        // runs
        .route("/runs", get(list_runs))
        .route("/runs", post(start_run))
        .route("/runs/progress", get(get_progress))
        // The client does not necessarily know the run id (a run is started
        // asynchronously), so "cancel whatever is running" is the useful shape.
        .route("/runs/current/cancel", post(cancel_run))
        // attempts
        .route("/attempts", get(list_attempts))
        .route("/attempts/stats", get(attempt_stats))
        .route("/attempts/{id}/requeue", post(requeue_attempt))
        .route("/attempts/{id}", delete(delete_attempt))
        // downloader introspection
        .route("/downloader/torrents", get(list_torrents))
        .route("/downloader/summary", get(downloader_summary))
        .with_state(state);

    Router::new().nest("/api", api)
}

// --------------------------------------------------------------------- meta

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn version() -> Json<Value> {
    Json(json!({
        "name": "iyuu-reseed",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Serialize)]
struct StatusResponse {
    configured: bool,
    iyuu_ok: bool,
    downloader_ok: bool,
    running: bool,
    pending_verification: i64,
    verified_seeding: i64,
    uptime_secs: i64,
    last_run: Option<serde_json::Value>,
    /// Next scheduled discovery run, ISO-8601. Absent when scheduling is off.
    next_run_at: Option<String>,
    schedule_enabled: bool,
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusResponse>> {
    let cfg = state.config_snapshot();
    let running = state.is_running.load(std::sync::atomic::Ordering::SeqCst);

    let (pending_added, pending_verifying) = runner::pending_counts(&state)?;
    let seeding = state.db.count_attempts(Some("seeding"))?;

    let last_run = state
        .db
        .list_runs(1)?
        .into_iter()
        .next()
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null));

    let next_run_at = crate::scheduler::next_run_at(&cfg.schedule);

    Ok(Json(StatusResponse {
        configured: !cfg.iyuu.token.is_empty(),
        iyuu_ok: !cfg.iyuu.token.is_empty(),
        downloader_ok: !cfg.downloader.base_url.is_empty(),
        running,
        pending_verification: pending_added + pending_verifying,
        verified_seeding: seeding,
        uptime_secs: (chrono::Utc::now() - state.started_at).num_seconds(),
        last_run,
        next_run_at: next_run_at.map(|d| d.to_rfc3339()),
        schedule_enabled: cfg.schedule.enabled,
    }))
}

// ----------------------------------------------------------------- settings

async fn get_config(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config_snapshot();
    Json(serde_json::to_value(cfg.public_view()).unwrap_or(Value::Null))
}

async fn put_config(
    State(state): State<AppState>,
    Json(patch): Json<ConfigPatch>,
) -> Result<Json<Value>> {
    let mut cfg = state.config_snapshot();
    cfg.apply_patch(patch);
    cfg.validate()?;
    cfg.save(&state.db)?;
    *state.config.write() = cfg.clone();
    state.push_log("配置已更新");

    Ok(Json(json!({
        "ok": true,
        "config": serde_json::to_value(cfg.public_view()).unwrap_or(Value::Null),
    })))
}

#[derive(Deserialize, Default)]
struct TestIyuuBody {
    /// Test a token without saving it first.
    token: Option<String>,
}

async fn test_iyuu(
    State(state): State<AppState>,
    Json(body): Json<TestIyuuBody>,
) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let token = body
        .token
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(cfg.iyuu.token);

    if token.trim().is_empty() {
        return Err(ServerError::Config("尚未配置 IYUU Token".into()));
    }

    let client = match cfg.iyuu.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(base) => IyuuClient::with_base_url(token, base)?,
        None => IyuuClient::new(token)?,
    };

    // `sites()` both proves the token works and fills the site cache.
    let sites = client.sites(true).await?;
    let bound = client.recommended_sites().await.unwrap_or_default();

    Ok(Json(json!({
        "ok": true,
        "site_count": sites.len(),
        "bound_count": bound.len(),
        "bound_sites": bound.iter().map(|s| json!({
            "id": s.id,
            "site": s.site,
            "nickname": s.nickname,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize, Default)]
struct TestDownloaderBody {
    kind: Option<String>,
    base_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
    rpc_path: Option<String>,
}

async fn test_downloader(
    State(state): State<AppState>,
    Json(body): Json<TestDownloaderBody>,
) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let kind = match body.kind.as_deref() {
        Some(k) => iyuu_core::models::DownloaderKind::parse(k)
            .ok_or_else(|| ServerError::Config(format!("未知下载器类型：{k}")))?,
        None => cfg.downloader.kind()?,
    };

    let base_url = body.base_url.unwrap_or_else(|| cfg.downloader.base_url.clone());
    let username = body.username.unwrap_or_else(|| cfg.downloader.username.clone());
    // An empty password means "use the saved one", matching the patch semantics.
    let password = body
        .password
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| cfg.downloader.password.clone());
    let extra = iyuu_core::downloader::DownloaderExtra {
        transmission_rpc_path: body
            .rpc_path
            .or_else(|| cfg.downloader.extra.transmission_rpc_path.clone()),
    };

    let dl = iyuu_core::downloader::build(kind, "test", &base_url, &username, &password, &extra)
        .await?;
    let version = dl.version().await?;
    let torrents = dl.list_torrents().await?;
    let complete = torrents.iter().filter(|t| t.is_verified_complete()).count();

    Ok(Json(json!({
        "ok": true,
        "kind": kind.as_str(),
        "version": version,
        "torrent_count": torrents.len(),
        "complete_count": complete,
    })))
}

async fn list_sites(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    if cfg.iyuu.token.is_empty() {
        return Err(ServerError::IyuuAuth("尚未配置 IYUU Token".into()));
    }
    let client = match cfg.iyuu.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(base) => IyuuClient::with_base_url(cfg.iyuu.token.clone(), base)?,
        None => IyuuClient::new(cfg.iyuu.token.clone())?,
    };
    let sites = client.sites(false).await?;
    // Mark which sites already have credentials so the UI can highlight gaps.
    let creds = cfg.credentials;
    let out: Vec<Value> = sites
        .iter()
        .map(|s| {
            let has_cred = creds
                .get(&s.id.to_string())
                .map(|c| {
                    c.passkey.as_deref().map(|v| !v.is_empty()).unwrap_or(false)
                        || c.cookie.as_deref().map(|v| !v.is_empty()).unwrap_or(false)
                        || c.api_key.as_deref().map(|v| !v.is_empty()).unwrap_or(false)
                        || c.rsskey.as_deref().map(|v| !v.is_empty()).unwrap_or(false)
                })
                .unwrap_or(false);
            json!({
                "id": s.id,
                "site": s.site,
                "nickname": s.nickname,
                "base_url": s.base_url,
                "is_https": s.is_https,
                "reseed_check": s.reseed_check,
                "cookie_required": s.cookie_required,
                "has_credentials": has_cred,
            })
        })
        .collect();
    Ok(Json(json!({"sites": out})))
}

async fn list_bound_sites(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    if cfg.iyuu.token.is_empty() {
        return Err(ServerError::IyuuAuth("尚未配置 IYUU Token".into()));
    }
    let client = match cfg.iyuu.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(base) => IyuuClient::with_base_url(cfg.iyuu.token.clone(), base)?,
        None => IyuuClient::new(cfg.iyuu.token.clone())?,
    };
    let bound = client.recommended_sites().await?;
    Ok(Json(json!({
        "sites": bound.iter().map(|s| json!({
            "id": s.id, "site": s.site, "nickname": s.nickname,
        })).collect::<Vec<_>>()
    })))
}

// --------------------------------------------------------------------- runs

async fn list_runs(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>> {
    let runs = state.db.list_runs(q.limit.unwrap_or(20).clamp(1, 200))?;
    Ok(Json(serde_json::to_value(runs).unwrap_or(Value::Null)))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

#[derive(Deserialize, Default)]
struct StartRunBody {
    /// `manual` (default), `schedule`, or `api`.
    trigger: Option<String>,
}

async fn start_run(
    State(state): State<AppState>,
    body: Option<Json<StartRunBody>>,
) -> Result<(StatusCode, Json<Value>)> {
    if state.is_running.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ServerError::Config("已有任务正在运行".into()));
    }

    let trigger = body
        .and_then(|b| b.0.trigger)
        .map(|t| RunTrigger::parse(&t))
        .unwrap_or(RunTrigger::Manual);

    // Validate cheaply so the caller gets an immediate 400 rather than a run id
    // for a run that will fail one second later.
    let cfg = state.config_snapshot();
    if cfg.iyuu.token.trim().is_empty() {
        return Err(ServerError::Config("尚未配置 IYUU Token".into()));
    }
    cfg.downloader.kind()?;

    let state2 = state.clone();
    let run_id_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let holder = run_id_holder.clone();

    // Spawn: a discovery run can take minutes and must not hold the request.
    // `execute_discovery` owns the run gate and the `is_running` flag, so there
    // is exactly one place that decides whether a run may start.
    tokio::spawn(async move {
        match runner::execute_discovery(&state2, trigger).await {
            Ok(id) => {
                *holder.lock().await = Some(id);
            }
            Err(e) => {
                state2.push_log(format!("任务失败：{e}"));
            }
        }
    });

    // Return immediately with the progress-endpoint contract; the client polls
    // /runs/progress for the live state.
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted": true}))))
}

async fn get_progress(State(state): State<AppState>) -> Json<Value> {
    let p = state.progress.read().clone();
    Json(serde_json::to_value(p).unwrap_or(Value::Null))
}

async fn cancel_run(State(state): State<AppState>) -> Json<Value> {
    let cancelled = if let Some(token) = state.cancel.read().as_ref() {
        token.cancel();
        true
    } else {
        false
    };
    if cancelled {
        state.push_log("收到取消请求，正在停止…");
    }
    Json(json!({"cancelled": cancelled}))
}

// ----------------------------------------------------------------- attempts

#[derive(Deserialize)]
struct AttemptsQuery {
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_attempts(
    State(state): State<AppState>,
    Query(q): Query<AttemptsQuery>,
) -> Result<Json<Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let attempts = state.db.list_attempts(q.state.as_deref(), limit, offset)?;
    let total = state.db.count_attempts(q.state.as_deref())?;
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "items": attempts,
    })))
}

async fn attempt_stats(State(state): State<AppState>) -> Result<Json<Value>> {
    let counts = state.db.state_counts()?;
    Ok(Json(json!({
        "counts": counts.into_iter().collect::<std::collections::HashMap<_, _>>(),
    })))
}

async fn requeue_attempt(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    if state.db.get_attempt(id)?.is_none() {
        return Err(ServerError::NotFound(format!("辅种记录 {id}")));
    }
    state.db.requeue_attempt(id)?;
    state.push_log(format!("记录 #{id} 已重新加入校验队列"));
    Ok(Json(json!({"ok": true})))
}

async fn delete_attempt(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    if state.db.get_attempt(id)?.is_none() {
        return Err(ServerError::NotFound(format!("辅种记录 {id}")));
    }
    state.db.delete_attempt(id)?;
    Ok(Json(json!({"ok": true})))
}

// --------------------------------------------------------------- downloader

async fn with_downloader<T, F, Fut>(state: &AppState, f: F) -> Result<T>
where
    F: FnOnce(Arc<dyn Downloader>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let ctx = runner::build_engine(state).await?;
    f(ctx.downloader).await
}

#[derive(Deserialize)]
struct TorrentsQuery {
    filter: Option<String>,
    limit: Option<i64>,
}

async fn list_torrents(
    State(state): State<AppState>,
    Query(q): Query<TorrentsQuery>,
) -> Result<Json<Value>> {
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let filter = q.filter.unwrap_or_default();

    let torrents = with_downloader(&state, |dl| async move {
        dl.list_torrents().await.map_err(ServerError::from)
    })
    .await?;

    let mut items: Vec<Value> = torrents
        .iter()
        .filter(|t| match filter.as_str() {
            "complete" => t.is_verified_complete(),
            "incomplete" => !t.is_verified_complete(),
            "checking" => matches!(
                t.phase(),
                iyuu_core::models::TorrentPhase::Checking
            ),
            "error" => matches!(
                t.phase(),
                iyuu_core::models::TorrentPhase::Error
                    | iyuu_core::models::TorrentPhase::MissingFiles
            ),
            _ => true,
        })
        .take(limit as usize)
        .map(|t| {
            json!({
                "hash": t.hash,
                "name": t.name,
                "size": t.total_size,
                "progress": t.progress,
                "state": t.state,
                "phase": t.phase(),
                "category": t.category,
                "tags": t.tags,
                "save_path": t.save_path,
                "seeding_time": t.seeding_time,
                "ratio": t.ratio,
                "uploaded": t.uploaded,
            })
        })
        .collect();

    // Stable ordering by size, biggest first: the interesting torrents are the
    // big ones, and it always beats "random client order".
    items.sort_by_key(|v| -(v.get("size").and_then(|s| s.as_i64()).unwrap_or(0)));

    Ok(Json(json!({"items": items, "total": torrents.len()})))
}

async fn downloader_summary(State(state): State<AppState>) -> Result<Json<Value>> {
    let torrents = with_downloader(&state, |dl| async move {
        dl.list_torrents().await.map_err(ServerError::from)
    })
    .await?;

    let total = torrents.len();
    let complete = torrents.iter().filter(|t| t.is_verified_complete()).count();
    let checking = torrents
        .iter()
        .filter(|t| matches!(t.phase(), iyuu_core::models::TorrentPhase::Checking))
        .count();
    let errors = torrents
        .iter()
        .filter(|t| {
            matches!(
                t.phase(),
                iyuu_core::models::TorrentPhase::Error
                    | iyuu_core::models::TorrentPhase::MissingFiles
            )
        })
        .count();
    let total_size: i64 = torrents.iter().map(|t| t.total_size).sum();
    let uploaded: i64 = torrents.iter().map(|t| t.uploaded).sum();

    Ok(Json(json!({
        "total": total,
        "complete": complete,
        "checking": checking,
        "errors": errors,
        "total_size": total_size,
        "uploaded": uploaded,
    })))
}

