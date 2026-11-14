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
use crate::seeds;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use iyuu_core::downloader::Downloader;
use iyuu_core::iyuu::IyuuClient;
use iyuu_core::models::{IyuuSite, RunTrigger};
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
        // The catalogue is served from the database; `POST /sites/refresh` is the
        // only thing that spends quota on `/reseed/sites/index`.
        .route("/sites", get(list_sites))
        .route("/sites/refresh", post(refresh_sites))
        // Cooperative sites + token activation. A distinct concept from the
        // catalogue: these are the PT accounts that activate the token.
        .route("/partners", get(list_partners))
        .route("/partners/bind", post(bind_partner))
        .route("/partners/activated", get(check_activation))
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
        // 「种子管理」视图：主种子 + 它的辅种子。分组是推导出来的，不落库。
        .route("/seeds", get(list_seeds))
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
    // Detect "a token was just saved" *before* overwriting the state: that is the
    // moment the catalogue becomes fetchable, and waiting for the user to find the
    // 刷新 button leaves an empty site picker that looks like a bug.
    let token_newly_set =
        !cfg.iyuu.token.trim().is_empty() && !state.db.catalogue_initialised().unwrap_or(false);

    cfg.save(&state.db)?;
    *state.config.write() = cfg.clone();
    state.push_log("配置已更新");

    // One attempt, best-effort, in the background: a slow IYUU must not hold up
    // the save response, and a failure is recoverable with the 刷新 button.
    if token_newly_set {
        let s = state.clone();
        tokio::spawn(async move {
            crate::runner::bootstrap_site_catalogue(s).await;
        });
    }

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
    // Seeded with the day's spend: the probe is metered like any other request, so
    // reporting the resulting position is only honest if it starts from the truth.
    let client = IyuuClient::build(token, crate::config::IyuuConfig::base_url_for(tier), tier)?
        .with_usage(&state.db.load_api_usage()?);

    // The probe is `/reseed/sites/index` and nothing else, because it is the only
    // endpoint that distinguishes the states a user actually needs told apart:
    //
    //   ok                 → token works and is activated
    //   IyuuInactive       → token is fine but no cooperative site is bound yet
    //   IyuuToken          → token itself is rejected
    //
    // The earlier version probed `/sites/recommend` and treated its result as
    // "the sites this token has bound". That endpoint lists the **cooperative
    // sites that could activate the token**, answers even for an unbound token,
    // and has nothing to do with which sites the user reseeds into. Using it as
    // an auth verdict meant an unactivated token reported success and then failed
    // every real run.
    let sites = match client.sites().await {
        Ok(s) => s,
        // Nothing is wrong with the token — it just needs activating. Returning
        // an error here would be technically right and practically misleading.
        Err(iyuu_core::CoreError::IyuuInactive(msg)) => {
            persist_usage(&state, &client);
            return Ok(Json(json!({
                "ok": false,
                "tier": tier.as_str(),
                "activated": false,
                "message": format!(
                    "Token 有效，但尚未激活：需要在「合作站点」里绑定一个 PT 站账号（IYUU 返回：{msg}）"
                ),
                "quota": client.quota(),
            })));
        }
        // Rate limiting is not a verdict on the token: the request never got as
        // far as being judged, so "check your token" would be wrong advice.
        Err(iyuu_core::CoreError::RateLimited(msg)) => {
            return Err(ServerError::RateLimited(msg));
        }
        Err(e) => {
            return Err(ServerError::IyuuAuth(format!(
                "此 Token 无法在 IYUU 上使用：请检查 Token 是否正确（原始信息：{e}）"
            )));
        }
    };

    // Keep the catalogue this probe just downloaded. Discarding it would leave the
    // user in the worst possible state: a verified, activated token and still an
    // empty site picker. It is the same data the bootstrap and 刷新 would fetch, so
    // throwing it away means spending a second request for nothing.
    persist_usage(&state, &client);
    if let Err(e) = state.db.replace_sites(&sites) {
        tracing::warn!(error = %e, "测试连接成功，但写入站点目录失败");
    } else {
        state.push_log(format!("站点目录已更新：{} 个站点", sites.len()));
    }

    Ok(Json(json!({
        "ok": true,
        "tier": tier.as_str(),
        "activated": true,
        "site_count": sites.len(),
        "message": format!("Token 已激活，IYUU 已适配 {} 个辅种站点", sites.len()),
        // One probe request was spent; report the resulting position so the UI can
        // warn instead of the user discovering it from a failed run.
        "quota": client.quota(),
    })))
}

#[derive(Deserialize, Default)]
struct TestDownloaderBody {
    kind: Option<String>,
    base_url: Option<String>,
    /// `password` / `api_key`. Absent means "use the saved mode".
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    api_key: Option<String>,
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
    let auth = match body.auth.as_deref() {
        Some(raw) => crate::config::DownloaderAuthMode::parse(raw).ok_or_else(|| {
            ServerError::Config(format!(
                "未知的下载器认证方式「{raw}」，可选 password / api_key"
            ))
        })?,
        None => cfg.downloader.auth_mode(),
    };
    let username = body
        .username
        .unwrap_or_else(|| cfg.downloader.username.clone());
    // An empty password means "use the saved one", matching the patch semantics.
    let password = body
        .password
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| cfg.downloader.password.clone());
    // Same rule for the key: blank means "keep the saved key". Testing a mode
    // without any key at all is a setup mistake worth naming, not a 403 later.
    let api_key = match auth {
        crate::config::DownloaderAuthMode::ApiKey => {
            let key = body
                .api_key
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| cfg.downloader.api_key.trim().to_string());
            if key.is_empty() {
                return Err(ServerError::Config("请先填写 qBittorrent API Key".into()));
            }
            if kind != iyuu_core::models::DownloaderKind::Qbittorrent {
                return Err(ServerError::Config(
                    "API Key 认证仅 qBittorrent ≥ 5.2 支持".into(),
                ));
            }
            Some(key)
        }
        crate::config::DownloaderAuthMode::Password => None,
    };
    let extra = iyuu_core::downloader::DownloaderExtra {
        transmission_rpc_path: body
            .rpc_path
            .or_else(|| cfg.downloader.extra.transmission_rpc_path.clone()),
    };

    let dl = iyuu_core::downloader::build(
        kind,
        "test",
        &base_url,
        &username,
        &password,
        api_key.as_deref(),
        &extra,
    )
    .await?;
    let version = dl.version().await?;
    let torrents = dl.list_torrents().await?;
    let complete = torrents.iter().filter(|t| t.is_verified_complete()).count();

    Ok(Json(json!({
        "ok": true,
        "kind": kind.as_str(),
        "auth": auth.as_str(),
        "version": version,
        "torrent_count": torrents.len(),
        "complete_count": complete,
    })))
}

/// Save a throwaway client's spend into the shared quota ledger.
///
/// Every endpoint that builds its own [`IyuuClient`] (`refresh`, `test`, `bind`)
/// spends real quota, and those clients are dropped at the end of the request —
/// so without this the ledger never learns about them and the next run plans
/// against budget the account no longer has. `runner::build_engine` does the same
/// for runs.
fn persist_usage(state: &AppState, client: &IyuuClient) {
    let usage = client.usage();
    if let Err(e) = state.db.save_api_usage(&usage) {
        // Losing a counter costs at most one redundant request; it must not turn
        // a successful action into an error.
        tracing::warn!(error = %e, "保存 API 用量失败");
    }
}

fn site_json(s: &IyuuSite, cfg: &crate::config::AppConfig) -> Value {
    let has_cred = cfg.credentials.get(&s.id.to_string()).is_some_and(|c| {
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
}

/// The cross-seed catalogue, read from the database.
///
/// No network call, deliberately. `/reseed/sites/index` costs quota, and this
/// endpoint is hit every time the 站点凭据 page or the add-site dialog opens —
/// fetching on read is what made an earlier version spend a request per page
/// view. `POST /sites/refresh` is the only path that refetches: the user's
/// explicit action, behind a button.
///
/// `initialised: false` tells the UI that no fetch has ever succeeded, so it can
/// prompt "点刷新拉取" instead of rendering an unexplained empty list. It does not
/// fetch on the UI's behalf: a page load must not spend quota.
async fn list_sites(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let sites = state.db.load_sites()?;
    let out: Vec<Value> = sites.iter().map(|s| site_json(s, &cfg)).collect();
    Ok(Json(json!({
        "sites": out,
        "initialised": state.db.catalogue_initialised()?,
        "synced_at": state.db.catalogue_synced_at()?,
    })))
}

/// Fetch `/reseed/sites/index` and persist it — the user's 刷新 button.
///
/// The only endpoint that spends quota on the catalogue. Rows are replaced
/// wholesale (see [`crate::db::Db::replace_sites`]) so a site IYUU has dropped
/// does not linger and keep failing to produce download URLs.
async fn refresh_sites(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let client = metered_client_for(&state, &cfg)?;
    let sites = client.sites().await?;
    persist_usage(&state, &client);
    state.db.replace_sites(&sites)?;
    state.push_log(format!("站点目录已刷新：{} 个站点", sites.len()));

    let out: Vec<Value> = sites.iter().map(|s| site_json(s, &cfg)).collect();
    Ok(Json(json!({
        "sites": out,
        "initialised": true,
        "synced_at": state.db.catalogue_synced_at()?,
        "count": out.len(),
        "quota": client.quota(),
    })))
}

/// Cooperative sites: the PT accounts that can **activate** this token.
///
/// Not the reseed catalogue. Binding one of these (`POST /partners/bind`) is a
/// prerequisite for every other endpoint — an unactivated token answers
/// `403 用户未绑定推荐站点`.
async fn list_partners(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let client = metered_client_for(&state, &cfg)?;
    let partners = client.partner_sites().await?;
    persist_usage(&state, &client);
    // Cross-referenced with the catalogue so the UI can show a domain: the
    // `sites/recommend` payload carries no `base_url` of its own.
    let catalogue = state.db.load_sites()?;
    let by_id: std::collections::HashMap<i64, &IyuuSite> =
        catalogue.iter().map(|s| (s.id, s)).collect();

    let out: Vec<Value> = partners
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "site": p.site,
                "nickname": p.nickname,
                "bind_check": p.bind_check,
                "reseed_check": p.reseed_check,
                "needs_uid": p.needs_uid(),
                "base_url": by_id.get(&p.id).map_or("", |s| s.base_url.as_str()),
            })
        })
        .collect();

    Ok(Json(json!({
        "partners": out,
        "count": out.len(),
        "quota": client.quota(),
    })))
}

/// Body of `POST /partners/bind`.
#[derive(Deserialize)]
struct BindBody {
    /// Cooperative site slug (`hdhome`), as listed by `GET /partners`.
    site: String,
    /// Site id from the same listing. Optional because it can be resolved from
    /// the catalogue by slug — but an explicit one still works when the catalogue
    /// is stale.
    sid: Option<i64>,
    /// The site's numeric user id.
    uid: String,
    /// The site **passkey**, in clear. Hashed to sha1 before it leaves this
    /// process: IYUU stores only the digest, so sending the raw value would both
    /// fail and needlessly transmit the secret.
    passkey: String,
}

/// Bind a cooperative-site account — i.e. **activate the IYUU token**.
///
/// Two details that are easy to get wrong, and the reason this is a dedicated
/// endpoint rather than a proxy:
///
/// * `passkey` must go over the wire as **sha1 hex**. The reference client does
///   `sha1($passkey)`; the raw passkey is rejected.
/// * `token` is a **form field** here in addition to the `token` header. The
///   documented body includes it, and the call is about linking that token to the
///   site account.
async fn bind_partner(
    State(state): State<AppState>,
    Json(body): Json<BindBody>,
) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    if cfg.iyuu.token.trim().is_empty() {
        return Err(ServerError::IyuuAuth("尚未配置 IYUU Token".into()));
    }

    let site = body.site.trim().to_string();
    if site.is_empty() {
        return Err(ServerError::Config("请选择要绑定的合作站点".into()));
    }
    let uid = body.uid.trim().to_string();
    if uid.is_empty() {
        return Err(ServerError::Config("站点用户数字 ID 不能为空".into()));
    }
    let passkey = body.passkey.trim().to_string();
    if passkey.is_empty() {
        return Err(ServerError::Config("站点密钥（passkey）不能为空".into()));
    }

    // Validate before hashing so a pasted URL or username fails loudly instead of
    // being hashed into a value IYUU rejects with an opaque message.
    let user_id = uid
        .parse::<i64>()
        .map_err(|_| ServerError::Config(format!("站点用户 ID 必须是纯数字（收到「{uid}」）")))?;

    // Prefer the explicit sid; otherwise resolve the slug against the catalogue.
    let sid = match body.sid {
        Some(id) if id > 0 => Some(id),
        _ => state
            .db
            .load_sites()?
            .into_iter()
            .find(|s| s.site.eq_ignore_ascii_case(&site))
            .map(|s| s.id),
    };

    let passkey_sha1 = {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(passkey.as_bytes());
        hex::encode(h.finalize())
    };

    let client = metered_client_for(&state, &cfg)?;
    let req = iyuu_core::iyuu::BindRequest {
        token: &cfg.iyuu.token,
        site: &site,
        sid,
        id: user_id,
        passkey: &passkey_sha1,
    };
    client.bind(&req).await?;
    persist_usage(&state, &client);
    state.push_log(format!("已绑定合作站点「{site}」"));

    // Activation is exactly the precondition the catalogue endpoint needs, so a
    // successful bind is the right moment to fetch it — otherwise the user is left
    // with "activated" and still nothing to pick from in 添加辅种站点.
    let needs_catalogue = !state.db.catalogue_initialised().unwrap_or(false);

    // Verify rather than declare success: the response is only "accepted".
    let result = match client.is_activated().await {
        Ok(activated) => {
            if activated {
                if needs_catalogue {
                    match client.sites().await {
                        Ok(sites) => {
                            let n = sites.len();
                            if let Err(e) = state.db.replace_sites(&sites) {
                                tracing::warn!(error = %e, "绑定后写入站点目录失败");
                            } else {
                                state.push_log(format!("已顺带拉取站点目录：{n} 个站点"));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "绑定后拉取站点目录失败");
                        }
                    }
                }
                json!({
                    "ok": true,
                    "activated": true,
                    "message": "合作站点已绑定，IYUU Token 已激活",
                    "quota": client.quota(),
                })
            } else {
                json!({
                    "ok": true,
                    "activated": false,
                    "message": "绑定请求已被接受，但 Token 仍显示未激活：请确认站点用户 ID 与密钥正确",
                    "quota": client.quota(),
                })
            }
        }
        // A throttled verification is not a failed bind — the binding itself
        // succeeded, we just cannot confirm it yet.
        Err(iyuu_core::CoreError::RateLimited(msg)) => json!({
            "ok": true,
            "activated": Value::Null,
            "message": format!("绑定请求已被接受，但校验激活状态时被限流（{msg}），稍后可点「检查激活状态」"),
            "quota": client.quota(),
        }),
        Err(e) => json!({
            "ok": true,
            "activated": Value::Null,
            "message": format!("绑定请求已被接受，但校验激活状态失败：{e}"),
            "quota": client.quota(),
        }),
    };

    Ok(Json(result))
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

/// Same as [`client_for`], but seeded with the day's persisted spend.
///
/// Any endpoint that actually sends a request must use this: a fresh client starts
/// at zero, and a page that shows "500 left" after 400 were already spent is worse
/// than showing nothing.
fn metered_client_for(state: &AppState, cfg: &crate::config::AppConfig) -> Result<IyuuClient> {
    let usage = state.db.load_api_usage()?;
    Ok(client_for(cfg)?.with_usage(&usage))
}

/// Whether the token is activated right now, without spending more than one
/// request.
///
/// The UI's 「检查激活状态」button. Cheap enough to be a user action and honest
/// enough to be believed: it asks the catalogue endpoint, which is the call IYUU
/// gates behind activation.
async fn check_activation(State(state): State<AppState>) -> Result<Json<Value>> {
    let cfg = state.config_snapshot();
    let client = metered_client_for(&state, &cfg)?;
    let activated = client.is_activated().await?;
    persist_usage(&state, &client);
    Ok(Json(json!({
        "activated": activated,
        "message": if activated {
            "Token 已激活，可以查询辅种"
        } else {
            "Token 尚未激活：请在下方绑定一个合作站点的账号"
        },
        "quota": client.quota(),
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

    // Without the catalogue the engine has no download-URL templates, so every
    // candidate would fail one at a time. Say it now, at the click, with the fix.
    if !state.db.catalogue_initialised()? {
        return Err(ServerError::Config(
            "还没有 IYUU 站点目录：请到「站点凭据 → 添加站点」点刷新拉取一次（若提示未激活，先在设置里绑定合作站点）"
                .into(),
        ));
    }

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

// ----------------------------------------------------------------- seeds

#[derive(Deserialize)]
struct SeedsQuery {
    filter: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// 主种子 + 辅种子分组视图。
///
/// 每次请求都重新聚合：下载器快照是实时读的，辅种记录是库里的，两者之间没有缓存
/// 可以不一致。分页在聚合之后做——组数远小于记录数，省下的那点内存不值得让分页
/// 把一组拆到两页里。
async fn list_seeds(
    State(state): State<AppState>,
    Query(q): Query<SeedsQuery>,
) -> Result<Json<Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let torrents = with_downloader(&state, |dl| async move {
        dl.list_torrents().await.map_err(ServerError::from)
    })
    .await?;
    let attempts = state.db.all_attempts()?;

    let groups = seeds::build_groups(&torrents, &attempts);
    // counts 取自过滤之前：顶部数字要回答「一共还有多少问题」，而不是「这页筛出来多少」。
    let counts = seeds::summarize(&groups);

    let filter = q.filter.unwrap_or_default();
    let needle = q.q.unwrap_or_default();
    let filtered: Vec<_> = groups
        .into_iter()
        .filter(|g| seeds::matches(g, &filter, &needle))
        .collect();
    let total = filtered.len();

    let skip = usize::try_from(offset).unwrap_or(0);
    let take = usize::try_from(limit).unwrap_or(0);
    let items: Vec<_> = filtered.into_iter().skip(skip).take(take).collect();

    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "counts": counts,
        "items": items,
    })))
}

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
