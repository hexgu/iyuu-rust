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
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use iyuu_core::downloader::Downloader;
use iyuu_core::iyuu::IyuuClient;
use iyuu_core::models::RunTrigger;
use iyuu_core::ratelimit::ApiTier;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        // status / meta
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/status", get(status))
        // IYUU quota + tier
        .route("/quota", get(get_quota))
        // settings
        .route("/config", get(get_config).put(put_config))
        // connectivity tests
        .route("/test/iyuu", post(test_iyuu))
        .route("/test/downloader", post(test_downloader))
        .route("/sites", get(list_sites))
        .route("/sites/bound", get(list_bound_sites))
        // Credentials are edited one site at a time: the settings page used to
        // submit the whole catalogue in one patch, which meant one site's
        // re-save could not be reasoned about on its own.
        .route(
            "/sites/{sid}/credentials",
            put(put_site_credentials).delete(delete_site_credentials),
        )
        // Bulk import of AutoSeed's site export, so migrating does not mean
        // re-typing every passkey. `apply=false` is a dry run.
        .route("/sites/import", post(import_sites))
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

// Health flags are shipped as flat booleans on purpose: the dashboard renders
// one indicator per field, and every client binding reads them by name.
#[expect(
    clippy::struct_excessive_bools,
    reason = "flat health indicators consumed directly by the dashboard"
)]
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
    /// Declared membership tier (`free` / `vip`).
    iyuu_tier: String,
    /// `false` until the user answers the tier question. The UI uses this to
    /// funnel them into the setup dialog instead of running with a guessed tier.
    iyuu_tier_chosen: bool,
    /// Requests already spent today, against the tier's daily allowance.
    quota_used: u32,
    quota_daily: u32,
    /// Present while IYUU has us in a rate-limit cooldown.
    quota_blocked_until: Option<String>,
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

    let quota = current_quota(&state)?;

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
        iyuu_tier: cfg.iyuu.tier().as_str().to_string(),
        iyuu_tier_chosen: cfg.iyuu.tier_chosen(),
        quota_used: quota.used_requests,
        quota_daily: quota.daily_requests,
        quota_blocked_until: quota
            .blocked_until
            .filter(|at| *at > chrono::Utc::now())
            .map(|d| d.to_rfc3339()),
    }))
}

// ------------------------------------------------------------------- quota

/// Current quota position, preferring the live client.
///
/// The live client is preferred because it holds state a fresh snapshot cannot
/// reconstruct: an active rate-limit cooldown and this run's spend. With no
/// client yet (nothing has run since startup) the persisted counters are used, so
/// the UI shows the day's real position instead of an empty widget.
fn current_quota(state: &AppState) -> Result<iyuu_core::ratelimit::QuotaSnapshot> {
    if let Some(client) = state.iyuu.read().clone() {
        return Ok(client.quota());
    }
    let usage = state.db.load_api_usage()?;
    let today = iyuu_core::ratelimit::quota_day(chrono::Utc::now());
    let usage = usage.for_day(&today);
    Ok(iyuu_core::ratelimit::QuotaSnapshot::new(
        state.config_snapshot().iyuu.tier(),
        chrono::Utc::now(),
        usage.used_requests,
        usage.used_hashes,
        None,
    ))
}

/// IYUU quota position for the declared tier.
///
/// No host is included, by design: the API base is an environment-level
/// deployment detail and the browser has no business knowing it. The tier is the
/// part a user can act on, and it is enough to explain every number here.
async fn get_quota(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let snapshot = current_quota(&state)?;

    let mut body = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("tier_chosen".into(), Value::Bool(cfg.iyuu.tier_chosen()));
        // Enough for the UI to render "500 → 2500" without hardcoding the plan
        // table in the bundle. Hosts are intentionally absent.
        obj.insert(
            "tiers".into(),
            json!([
                {
                    "id": "free",
                    "label": "普通用户",
                    "daily_requests": iyuu_core::ratelimit::ApiTier::Free.daily_requests(),
                    "daily_hashes": iyuu_core::ratelimit::ApiTier::Free.daily_hashes(),
                },
                {
                    "id": "vip",
                    "label": "SVIP 会员",
                    "daily_requests": iyuu_core::ratelimit::ApiTier::Vip.daily_requests(),
                    "daily_hashes": iyuu_core::ratelimit::ApiTier::Vip.daily_hashes(),
                },
            ]),
        );
    }

    Ok(Json(body))
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
    // Validate the raw patch first: `apply_patch` canonicalizes the tier, so a
    // typo would otherwise be silently rewritten into a valid-but-wrong value.
    crate::config::AppConfig::validate_patch(&patch)?;

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
    /// Required for a new token: the tier decides the server, and the server
    /// cannot be inferred from the token.
    tier: Option<String>,
}

async fn test_iyuu(
    State(state): State<AppState>,
    Json(body): Json<TestIyuuBody>,
) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let token = body
        .token
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| cfg.iyuu.token.clone());

    if token.trim().is_empty() {
        return Err(ServerError::Config("尚未配置 IYUU Token".into()));
    }

    // The tier only decides which host to call and which budget to assume — it
    // is **not** a credential, and it cannot be verified by probing. Both hosts
    // front the same service, so a successful response says nothing about
    // membership. Treat it as a self-declared preference and verify only the token.
    let tier = body
        .tier
        .as_deref()
        .map_or_else(|| cfg.iyuu.tier(), ApiTier::parse);

    // Host comes from the tier (plus any environment override), never from the
    // request: the browser cannot choose the server. `base_url_for` is used rather
    // than `for_tier` so an operator's mirror override is honoured here too — a
    // probe that bypassed it would reach the real IYUU while every real run goes
    // through the mirror.
    let client = IyuuClient::build(token, crate::config::IyuuConfig::base_url_for(tier), tier)?;

    // `sites()` proves the host answers and fills the catalogue.
    let sites = client.sites(true).await?;

    // `recommended_sites()` is the token-scoped call: it is what actually proves
    // the token is real, so its failure is fatal here rather than defaulted away.
    // Swallowing it is what let an invalid token report success.
    //
    // Rate limiting is reported separately because it is not a verdict on the
    // token — the request never got as far as being judged, and telling the user
    // to "check your token" because of a throttle would be wrong advice.
    let bound = match client.recommended_sites().await {
        Ok(list) => list,
        Err(iyuu_core::CoreError::RateLimited(msg)) => {
            return Err(ServerError::RateLimited(msg));
        }
        Err(e) => {
            return Err(ServerError::IyuuAuth(format!(
                "此 Token 无法在 IYUU 上使用：请检查 Token 是否正确（原始信息：{e}）"
            )));
        }
    };

    // A valid token with no bound sites cannot produce candidates, so a "success"
    // here would be a setup that silently adds nothing.
    if bound.is_empty() {
        return Err(ServerError::IyuuAuth(
            "此 Token 尚未在 IYUU 绑定任何站点，辅种查询结果会为空。请先在 IYUU 里完成站点绑定"
                .into(),
        ));
    }

    Ok(Json(json!({
        "ok": true,
        "tier": tier.as_str(),
        "site_count": sites.len(),
        "bound_count": bound.len(),
        "bound_sites": bound.iter().map(|s| json!({
            "id": s.id,
            "site": s.site,
            "nickname": s.nickname,
        })).collect::<Vec<_>>(),
        // Two probe requests were spent; report the resulting position so the UI
        // can warn instead of the user discovering it from a failed run.
        "quota": client.quota(),
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

    let base_url = body
        .base_url
        .unwrap_or_else(|| cfg.downloader.base_url.clone());
    let username = body
        .username
        .unwrap_or_else(|| cfg.downloader.username.clone());
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

    let dl =
        iyuu_core::downloader::build(kind, "test", &base_url, &username, &password, &extra).await?;
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
    let client = client_for(&cfg)?;
    let sites = client.sites(false).await?;
    // Mark which sites already have credentials so the UI can highlight gaps.
    let creds = cfg.credentials;
    let out: Vec<Value> = sites
        .iter()
        .map(|s| {
            let has_cred = creds.get(&s.id.to_string()).is_some_and(|c| {
                c.passkey.as_deref().is_some_and(|v| !v.is_empty())
                    || c.cookie.as_deref().is_some_and(|v| !v.is_empty())
                    || c.api_key.as_deref().is_some_and(|v| !v.is_empty())
                    || c.rsskey.as_deref().is_some_and(|v| !v.is_empty())
            });
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

/// Build a throwaway client from the saved config, for read-only endpoints.
///
/// Config-driven on purpose: `list_sites` must query the same server a run would,
/// otherwise a VIP user's site list could come from one host while the reseed
/// query goes to another.
fn client_for(cfg: &crate::config::AppConfig) -> Result<IyuuClient> {
    if cfg.iyuu.token.trim().is_empty() {
        return Err(ServerError::IyuuAuth("尚未配置 IYUU Token".into()));
    }
    if !cfg.iyuu.tier_chosen() {
        return Err(ServerError::Config(
            "请先选择 IYUU 会员类型（普通用户 / SVIP）".into(),
        ));
    }
    Ok(IyuuClient::build(
        cfg.iyuu.token.clone(),
        cfg.iyuu.effective_base_url(),
        cfg.iyuu.tier(),
    )?)
}

async fn list_bound_sites(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let client = client_for(&cfg)?;
    let bound = client.recommended_sites().await?;
    Ok(Json(json!({
        "sites": bound.iter().map(|s| json!({
            "id": s.id, "site": s.site, "nickname": s.nickname,
        })).collect::<Vec<_>>()
    })))
}

/// Save (or update) the credentials for exactly one site.
///
/// Separate from `PUT /config` because the credentials page edits one site at a
/// time: sending the whole map meant a page that had been open for a while
/// would rewrite every other site's entry with stale values. Merging server-side
/// (see [`iyuu_core::iyuu::SiteCredentials::merge_from`]) also lets the blank
/// secret fields in the form keep meaning "unchanged".
async fn put_site_credentials(
    State(state): State<AppState>,
    Path(sid): Path<String>,
    Json(patch): Json<iyuu_core::iyuu::SiteCredentials>,
) -> Result<Json<Value>> {
    let sid = parse_site_id(&sid)?;
    if patch.is_empty() {
        return Err(ServerError::Config("凭据不能为空".into()));
    }

    let mut cfg = state.config_snapshot();
    let entry = cfg.credentials.entry(sid.to_string()).or_default();
    entry.merge_from(patch);

    // Refuse an entry that ends up with nothing usable: the UI would show it as
    // configured while the engine has no way to download from the site.
    if !entry.has_any_credential() {
        return Err(ServerError::Config("凭据不能为空".into()));
    }

    cfg.save(&state.db)?;
    *state.config.write() = cfg.clone();
    state.push_log(format!("站点 {sid} 凭据已更新"));

    Ok(Json(json!({
        "ok": true,
        "config": serde_json::to_value(cfg.public_view()).unwrap_or(Value::Null),
    })))
}

/// Remove one site's credentials (delete its entry entirely, so the site stops
/// being targeted by reseed runs).
async fn delete_site_credentials(
    State(state): State<AppState>,
    Path(sid): Path<String>,
) -> Result<Json<Value>> {
    let sid = parse_site_id(&sid)?;

    let mut cfg = state.config_snapshot();
    if cfg.credentials.remove(&sid.to_string()).is_none() {
        return Err(ServerError::NotFound(format!("站点 {sid} 未配置凭据")));
    }

    cfg.save(&state.db)?;
    *state.config.write() = cfg.clone();
    state.push_log(format!("站点 {sid} 凭据已删除"));

    Ok(Json(json!({
        "ok": true,
        "config": serde_json::to_value(cfg.public_view()).unwrap_or(Value::Null),
    })))
}

/// Body of `POST /sites/import`.
#[derive(Deserialize)]
struct ImportBody {
    /// The `AutoSeed` export, either as a JSON string or as already-parsed JSON.
    ///
    /// A string is what a `<input type="file">` upload in the browser naturally
    /// produces, and it keeps the parse error attributable to the file. Pre-parsed
    /// JSON is accepted too so `curl` can post the file directly.
    data: Value,
    /// `false` = dry run: report what would change, write nothing.
    #[serde(default = "default_true")]
    apply: bool,
}

fn default_true() -> bool {
    true
}

/// Import `AutoSeed`'s site export into the credential map.
///
/// Design choices worth stating, because they are what makes this safe to run on
/// a file the user did not read:
///
/// * **Disabled sites are reported, not imported.** `AutoSeed`'s `disabled` flag
///   means the user turned that site off on purpose; importing it as a reseed
///   target would silently start seeding where they had said no.
/// * **Rows with no credential are ignored.** That is the whole catalogue, which
///   is the thing this app is not supposed to show.
/// * **Existing entries are replaced, not merged.** The backup is the authoritative
///   copy of what the user had; merging would keep a stale value that the file was
///   supposed to correct. The count is reported so an overwrite is never silent.
/// * **Nothing secret is echoed back.** The response carries site names and which
///   *fields* were imported, never values.
async fn import_sites(
    State(state): State<AppState>,
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>> {
    // Accept either a JSON string (browser file upload) or the parsed array/object
    // (curl). Two shapes, because both are reasonable and guessing wrong would
    // produce a confusing 422.
    let file: crate::importer::BackupFile = match &body.data {
        Value::String(raw) => serde_json::from_str(raw).map_err(|e| {
            ServerError::Config(format!(
                "无法解析备份文件：{e}（应为 AutoSeed 导出的 JSON）"
            ))
        })?,
        other => serde_json::from_value(other.clone()).map_err(|e| {
            ServerError::Config(format!(
                "无法解析备份文件：{e}（应为 AutoSeed 导出的 JSON）"
            ))
        })?,
    };

    let rows = file.into_sites();
    if rows.is_empty() {
        return Err(ServerError::Config(
            "备份文件里没有站点记录（应为 AutoSeed 导出的 JSON 数组）".into(),
        ));
    }

    let mut cfg = state.config_snapshot();
    let mut out = crate::importer::ImportOutcome::default();
    // Site names are metadata, not secrets — they make the report readable
    // ("M-Team was skipped") instead of a wall of ids.
    let mut details: Vec<Value> = Vec::new();

    for row in &rows {
        let Some(sid) = row.sid.filter(|id| *id > 0) else {
            out.skipped_unkeyed += 1;
            continue;
        };
        let Some(creds) = crate::importer::credentials_from(row) else {
            out.skipped_empty += 1;
            continue;
        };

        let fields = crate::importer::field_names(&creds);
        let name = row
            .nickname
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(row.site.as_deref())
            .unwrap_or_default()
            .to_string();

        // Disabled in AutoSeed = the user does not want it. Report, don't apply.
        if row.disabled == Some(1) {
            out.skipped_disabled.push(sid);
            details.push(json!({
                "sid": sid, "name": name, "fields": fields, "status": "disabled",
            }));
            continue;
        }

        let existed = cfg.credentials.contains_key(&sid.to_string());
        if existed {
            out.replaced += 1;
        }
        cfg.credentials.insert(sid.to_string(), creds);
        out.imported.push(sid);
        details.push(json!({
            "sid": sid,
            "name": name,
            "fields": fields,
            "status": if existed { "replaced" } else { "imported" },
        }));
    }

    let summary = out.summary();

    // A dry run must not touch the stored config — that is the point of asking
    // for `apply: false`.
    let config = if body.apply {
        cfg.save(&state.db)?;
        *state.config.write() = cfg.clone();
        state.push_log(&summary);
        json!(serde_json::to_value(cfg.public_view()).unwrap_or(Value::Null))
    } else {
        // The current config, unchanged, so the UI can keep rendering.
        json!(serde_json::to_value(state.config_snapshot().public_view()).unwrap_or(Value::Null))
    };

    Ok(Json(json!({
        "ok": true,
        "applied": body.apply,
        "summary": summary,
        "imported": out.imported,
        "skipped_disabled": out.skipped_disabled,
        "replaced": out.replaced,
        "skipped_empty": out.skipped_empty,
        "skipped_unkeyed": out.skipped_unkeyed + out.skipped_invalid,
        "details": details,
        "config": config,
    })))
}

fn parse_site_id(raw: &str) -> Result<i64> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| ServerError::Config(format!("站点 ID「{raw}」不是数字")))
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
        .map_or(RunTrigger::Manual, |t| RunTrigger::parse(&t));

    // Validate cheaply so the caller gets an immediate 400 rather than a run id
    // for a run that will fail one second later.
    let cfg = state.config_snapshot();
    if cfg.iyuu.token.trim().is_empty() {
        return Err(ServerError::Config("尚未配置 IYUU Token".into()));
    }
    cfg.downloader.kind()?;

    // Refuse to start with no IYUU budget. Every discovery run spends at least
    // one request, so with the day's allowance gone the run can only waste a
    // downloader scan and then fail — better to say so now, with the reset time,
    // than after two minutes of work.
    let quota = current_quota(&state)?;
    let blocked = quota.blocked_until.filter(|at| *at > chrono::Utc::now());
    if let Some(until) = blocked {
        return Err(ServerError::RateLimited(format!(
            "IYUU 限流冷却中，{} 后才可再次请求",
            until.with_timezone(&chrono::Local).format("%m-%d %H:%M")
        )));
    }
    if quota.remaining_requests == 0 {
        let reset = quota.reset_at.map_or_else(
            || "明日".to_string(),
            |t| {
                t.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            },
        );
        return Err(ServerError::RateLimited(format!(
            "今日 IYUU 额度已用尽（{}/{} 次，{}），{reset} 后重置。\
             需要更大额度可考虑 IYUU 的 SVIP 会员服务器",
            quota.used_requests, quota.daily_requests, quota.tier_label
        )));
    }

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

async fn delete_attempt(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Value>> {
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
            "checking" => matches!(t.phase(), iyuu_core::models::TorrentPhase::Checking),
            "error" => matches!(
                t.phase(),
                iyuu_core::models::TorrentPhase::Error
                    | iyuu_core::models::TorrentPhase::MissingFiles
            ),
            _ => true,
        })
        .take(usize::try_from(limit).unwrap_or(0))
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
    items.sort_by_key(|v| {
        -(v.get("size")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0))
    });

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
