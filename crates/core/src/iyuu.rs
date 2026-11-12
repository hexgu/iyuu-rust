//! IYUU 辅种 API client.
//!
//! Endpoints implemented (against the host for the account's tier — see
//! [`ApiTier`]):
//!   GET  /reseed/sites/index          — supported sites (with `download_page` template)
//!   GET  /reseed/sites/recommend      — recommended sites (binding-oriented)
//!   POST /reseed/sites/reportExisting — report held site ids, get `sid_sha1`
//!   POST /reseed/index/index          — THE cross-seed query
//!   POST /reseed/users/bind           — bind a site account to the IYUU token
//!   GET  /spider/torrent/total        — site indexed torrent count
//!
//! Wire quirks that matter and are easy to get wrong:
//!  * `hash` is a **JSON array of lowercase hex strings**, sorted ascending, with
//!    no spaces. A `sha1` of that exact JSON string must accompany it, so the
//!    bytes we sign must be byte-identical to the bytes we send. We build the
//!    string once and reuse it for both.
//!  * `sid_sha1` is obtained from `reportExisting` and is valid ~7 days. It
//!    filters the response to sites the user actually holds, saving bandwidth.
//!    If it expires the server answers with a message containing
//!    "站点哈希值 require", in which case we re-report and retry exactly once.
//!  * Responses are `{code, data, msg}` where `code == 0` is success for most
//!    endpoints but `200` is success for the `/spider/*` ones. The reference
//!    implementations only accept 0, so we accept both.
//!  * The API is **metered per day** and rejects bursts. Every call is charged
//!    against a local quota mirror first ([`QuotaSnapshot`]) and paced by the
//!    tier's interval, so a run cannot spend the budget by accident. When IYUU
//!    does reject us, the client enters a cooldown instead of retrying into a
//!    ban — see [`crate::ratelimit`].

use crate::error::{CoreError, Result};
use crate::models::{IyuuSite, SeedCandidate};
use crate::ratelimit::{
    ApiTier, HASHES_PER_REQUEST, QuotaBudget, QuotaSnapshot, QuotaUsage, looks_rate_limited,
    rate_limit_cooldown,
};
use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Version string advertised to the API. The server uses it for compatibility
/// gating, so keep it in sync with what upstream clients send.
pub const IYUU_API_VERSION: &str = "8.2.0";
/// Default host, kept for callers that predate tier selection.
pub const DEFAULT_BASE_URL: &str = ApiTier::FREE_BASE_URL;

/// Chunk size for `hash` batches. The documented/reference limit is 200; the
/// server rejects larger batches, and a rejection loses the whole chunk, so we
/// stay at the documented value.
pub const HASH_CHUNK_SIZE: usize = HASHES_PER_REQUEST;

/// `usize` → `i64` for API counters. Saturates rather than wrapping so an
/// impossible count never turns into a negative number.
fn len_as_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[derive(Debug, Deserialize)]
struct SitesData {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    sites: Vec<IyuuSite>,
}

#[derive(Debug, Deserialize)]
struct SidSha1Data {
    sid_sha1: String,
}

#[derive(Debug, Deserialize)]
struct ReseedData {
    #[serde(flatten)]
    per_hash: HashMap<String, HashEntry>,
}

#[derive(Debug, Deserialize)]
struct HashEntry {
    #[serde(default)]
    torrent: Vec<SeedCandidate>,
}

#[derive(Debug, Serialize)]
pub struct BindRequest<'a> {
    pub token: &'a str,
    pub site: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<i64>,
    pub id: i64,
    pub passkey: &'a str,
}

/// Cached server state that is expensive or rate-limited to obtain.
#[derive(Default)]
struct Cache {
    sites: Option<Vec<IyuuSite>>,
    sites_fetched_at: Option<Instant>,
    sid_sha1: Option<String>,
    sid_sha1_at: Option<Instant>,
}

/// Local mirror of the account's daily usage, plus pacing/back-off state.
///
/// Held in one lock so a check-and-charge cannot interleave with another
/// worker's check-and-charge. Without that, two concurrent run paths could both
/// see "1 request left" and both send.
struct QuotaState {
    tier: ApiTier,
    /// Quota day (`YYYY-MM-DD`, UTC+8) the counters below belong to.
    day: String,
    used_requests: u32,
    used_hashes: u32,
    /// Set while we have been told to back off. Nothing is sent until it passes.
    blocked_until: Option<chrono::DateTime<Utc>>,
    /// When the last request was actually put on the wire, for pacing.
    last_call: Option<Instant>,
    /// Requests we refused locally because the budget was gone; surfaced to the
    /// UI so "0 added" is explainable rather than mysterious.
    denied_requests: u32,
}

impl QuotaState {
    fn new(tier: ApiTier, usage: &QuotaUsage) -> Self {
        let day = crate::ratelimit::quota_day(Utc::now());
        // `for_day` drops a record from an earlier day, so a stale row in the
        // database cannot suppress today's budget.
        let usage = usage.for_day(&day);
        Self {
            tier,
            day,
            used_requests: usage.used_requests,
            used_hashes: usage.used_hashes,
            blocked_until: None,
            last_call: None,
            denied_requests: 0,
        }
    }

    /// Export the counters for persistence.
    fn usage(&self) -> QuotaUsage {
        QuotaUsage {
            day: self.day.clone(),
            used_requests: self.used_requests,
            used_hashes: self.used_hashes,
        }
    }

    /// Budget available right now: the daily remainder, minus any active back-off.
    ///
    /// Deliberately not clamped per run. A run should use as much of the day's
    /// allowance as its torrent list needs — splitting it into smaller batches
    /// only means more runs, and the account gets the same daily total either
    /// way. The only real ceiling is IYUU's own counter.
    fn budget(&self) -> QuotaBudget {
        if self.blocked_until.is_some_and(|at| at > Utc::now()) {
            return QuotaBudget::new(0, 0);
        }
        QuotaBudget::new(
            self.tier
                .daily_requests()
                .saturating_sub(self.used_requests),
            self.tier.daily_hashes().saturating_sub(self.used_hashes),
        )
    }

    /// Roll the counters over when the quota day changes.
    ///
    /// IYUU resets on its own boundary; we cannot know the exact second, so we
    /// mirror a UTC+8 midnight. Being early only costs a request that the server
    /// may still reject (and then we back off again); being late would silently
    /// under-use the budget.
    fn roll_over(&mut self, now: chrono::DateTime<Utc>) {
        let today = crate::ratelimit::quota_day(now);
        if today != self.day {
            self.day = today;
            self.used_requests = 0;
            self.used_hashes = 0;
            self.denied_requests = 0;
        }
        // An expired block must not keep suppressing requests.
        if self.blocked_until.is_some_and(|at| at <= now) {
            self.blocked_until = None;
        }
    }

    fn snapshot(&self) -> QuotaSnapshot {
        let mut snap = QuotaSnapshot::new(
            self.tier,
            Utc::now(),
            self.used_requests,
            self.used_hashes,
            self.blocked_until,
        );
        snap.denied_requests = self.denied_requests;
        snap
    }
}

/// `sid_sha1` is documented as valid for 7 days; we refresh at 6 to leave slack.
const SID_SHA1_TTL: Duration = Duration::from_hours(144);
/// Site list changes rarely; a day is plenty and avoids a request per run.
const SITES_TTL: Duration = Duration::from_hours(24);

pub struct IyuuClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    cache: RwLock<Cache>,
    quota: RwLock<QuotaState>,
}

impl IyuuClient {
    /// Build a client against the public (free-tier) IYUU API.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// Build a client for an explicit tier, using that tier's host.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn for_tier(token: impl Into<String>, tier: ApiTier) -> Result<Self> {
        Self::build(token, tier.base_url(), tier)
    }

    /// Build a client against an explicit base URL (tests, mirrors, and a user
    /// who overrides the host). Tier still decides the *budget* — a custom host
    /// does not buy extra quota, since the meter lives on the account.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        Self::build(token, base_url, ApiTier::Free)
    }

    /// Build a client with both an explicit host and an explicit tier.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn build(
        token: impl Into<String>,
        base_url: impl Into<String>,
        tier: ApiTier,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(8)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            cache: RwLock::new(Cache::default()),
            quota: RwLock::new(QuotaState::new(tier, &QuotaUsage::default())),
        })
    }

    /// Seed the quota mirror from usage persisted earlier today (or from an
    /// earlier day, which is ignored).
    ///
    /// The client object is recreated on every run, so without this a restart
    /// mid-day would hand the next run a full budget the account does not have —
    /// and the server would start rejecting us.
    #[must_use]
    pub fn with_usage(self, usage: &QuotaUsage) -> Self {
        {
            let mut guard = self.quota.write();
            *guard = QuotaState::new(guard.tier, usage);
        }
        self
    }

    /// Current counters, for persisting between runs.
    #[must_use]
    pub fn usage(&self) -> QuotaUsage {
        self.quota.read().usage()
    }

    /// Share one client (and therefore one connection pool + cache + quota
    /// mirror) across the whole process. Every reseed run reuses it.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Current quota mirror, for the API and the UI.
    #[must_use]
    pub fn quota(&self) -> QuotaSnapshot {
        let mut guard = self.quota.write();
        guard.roll_over(Utc::now());
        guard.snapshot()
    }

    /// Budget available right now, honouring an active back-off and any per-run
    /// cap. Read through the state object so the run cap is included — asking the
    /// snapshot alone would ignore it.
    #[must_use]
    pub fn budget(&self) -> QuotaBudget {
        let mut guard = self.quota.write();
        guard.roll_over(Utc::now());
        guard.budget()
    }

    /// The host this client talks to — shown in the UI so a wrong tier is
    /// visible rather than mysterious.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn token_header(&self) -> Result<reqwest::header::HeaderValue> {
        reqwest::header::HeaderValue::from_str(&self.token)
            .map_err(|_| CoreError::Config("IYUU token contains invalid header characters".into()))
    }

    /// Charge one request against the local quota, then wait out the tier's
    /// pacing interval.
    ///
    /// Charge-before-send, not after: a request that times out still consumed
    /// server-side quota, and under-counting is the failure that leads to an
    /// actual ban. `hashes` is how many info-hashes this request carries (0 for
    /// non-reseed endpoints), so the two budgets stay in step.
    ///
    /// Errors with [`CoreError::RateLimited`] when the local mirror already says
    /// "no", which is why callers must run this *before* building a request.
    async fn spend(&self, what: &str, hashes: usize) -> Result<()> {
        let wait = {
            let mut guard = self.quota.write();
            guard.roll_over(Utc::now());

            if guard.blocked_until.is_some_and(|at| at > Utc::now()) {
                let until = guard.blocked_until.unwrap_or_else(Utc::now);
                let msg = format!(
                    "IYUU 接口处于限流冷却中，{} 前不再请求（{what}）",
                    until.format("%H:%M:%S")
                );
                guard.denied_requests = guard.denied_requests.saturating_add(1);
                return Err(CoreError::RateLimited(msg));
            }

            if guard.used_requests >= guard.tier.daily_requests() {
                guard.denied_requests = guard.denied_requests.saturating_add(1);
                return Err(CoreError::RateLimited(format!(
                    "今日请求次数已用尽（{}/{}），明日 00:00（UTC+8）后重置",
                    guard.used_requests,
                    guard.tier.daily_requests(),
                )));
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "hash counts per request are capped at HASHES_PER_REQUEST"
            )]
            let hashes = hashes.min(HASHES_PER_REQUEST) as u32;
            guard.used_requests = guard.used_requests.saturating_add(1);
            guard.used_hashes = guard.used_hashes.saturating_add(hashes);

            // Pace: sleep at least the tier interval since the previous call.
            let interval = guard.tier.request_interval();
            let wait = guard
                .last_call
                .map(|at| interval.saturating_sub(at.elapsed()))
                .filter(|d| !d.is_zero());
            guard.last_call = Some(Instant::now());
            wait
        };

        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
        Ok(())
    }

    /// Put the client into back-off after a rate-limit rejection, and translate
    /// the error. A cooldown is the only correct response: retrying into a
    /// throttle is how an account gets a longer ban than the quota itself.
    fn trip_breaker(&self, msg: &str) -> CoreError {
        let mut guard = self.quota.write();
        guard.blocked_until = Some(Utc::now() + rate_limit_cooldown());
        CoreError::RateLimited(format!(
            "IYUU 判定请求超限，已暂停 {} 分钟：{msg}",
            rate_limit_cooldown().num_minutes()
        ))
    }

    /// Whether a breaker is currently active.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.quota
            .read()
            .blocked_until
            .is_some_and(|at| at > Utc::now())
    }

    /// Clear an active cooldown.
    ///
    /// Used by the settings test endpoint: a connectivity probe is an explicit,
    /// one-off user action, not part of a run. If the probe is short-circuited by a
    /// cooldown left over from an earlier run, the user gets "rate limited" for a
    /// request that was never sent — and no way to find out whether their token
    /// works. A probe therefore attempts exactly one request and does not inherit
    /// the run's breaker.
    pub fn clear_cooldown(&self) {
        self.quota.write().blocked_until = None;
    }

    /// Unwrap the `{code, data, msg}` envelope, mapping the API's own error
    /// codes into a typed error that callers can react to.
    ///
    /// A quota/rate-limit message does not just produce an error — it trips the
    /// breaker, so every later call in this process stops instead of hammering a
    /// service that has already told us to go away.
    fn unwrap_envelope<T>(&self, status: reqwest::StatusCode, body: &str, what: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        // Deserialize the envelope generically first: `#[derive(Deserialize)]`
        // on a struct with a `#[serde(default)]` field demands `T: Default`,
        // which callers' payload types do not implement. Going through
        // `serde_json::Value` sidesteps that and also lets us read the code and
        // message from a response whose data does not match `T` (the usual case
        // for API errors).
        let raw: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            let preview: String = body.chars().take(200).collect();
            CoreError::Upstream {
                what: what.to_string(),
                code: i64::from(status.as_u16()),
                msg: format!("响应解析失败({e})，前缀：{preview}"),
            }
        })?;

        let code = raw
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        let msg = raw
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Success is 0 for /reseed/*, 200 for /spider/*. Accept either.
        if code == 0 || code == 200 {
            let data = raw.get("data").cloned().unwrap_or(serde_json::Value::Null);
            if data.is_null() {
                return Err(CoreError::Upstream {
                    what: what.to_string(),
                    code,
                    msg: "成功响应但 data 为空".into(),
                });
            }
            return serde_json::from_value(data).map_err(|e| CoreError::Upstream {
                what: what.to_string(),
                code,
                msg: format!("响应数据结构不符：{e}"),
            });
        }

        // Token problems deserve their own variant — the UI shows a distinct
        // hint for them (bind your sites / check the token).
        if msg.contains("token") || msg.contains("Token") {
            return Err(CoreError::IyuuToken(msg));
        }
        // Checked after the token branch on purpose: "token 超出有效期" is a
        // credential problem, and telling the user to wait half an hour would be
        // the wrong advice.
        if looks_rate_limited(&msg) {
            return Err(self.trip_breaker(&msg));
        }
        Err(CoreError::Upstream {
            what: what.to_string(),
            code,
            msg,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<String> {
        let resp = req.send().await.map_err(|e| CoreError::Http {
            what: what.to_string(),
            source: e,
        })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| CoreError::Http {
            what: what.to_string(),
            source: e,
        })?;
        if !status.is_success() {
            // Still try to surface the API's own message, it is far more useful
            // than "403 Forbidden".
            if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body) {
                let code = raw
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(-1);
                let msg = raw
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let msg = if msg.is_empty() { body } else { msg };
                // A 429 (or an over-quota message on any status) means stop,
                // not retry. Checking the body as well covers deployments that
                // answer 403 with a quota message instead of 429.
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || looks_rate_limited(&msg) {
                    return Err(self.trip_breaker(&msg));
                }
                return Err(CoreError::Upstream {
                    what: what.to_string(),
                    code,
                    msg,
                });
            }
            return Err(CoreError::Upstream {
                what: what.to_string(),
                code: i64::from(status.as_u16()),
                msg: body.chars().take(300).collect(),
            });
        }
        Ok(body)
    }

    // ---------------------------------------------------------------- sites

    /// All sites IYUU supports for cross-seeding. Cached for a day.
    ///
    /// # Errors
    /// Returns an error when the request fails or the response envelope reports
    /// a non-success code.
    pub async fn sites(&self, force: bool) -> Result<Vec<IyuuSite>> {
        if !force {
            let guard = self.cache.read();
            if let (Some(sites), Some(at)) = (&guard.sites, guard.sites_fetched_at)
                && at.elapsed() < SITES_TTL
            {
                return Ok(sites.clone());
            }
        }

        self.spend("获取站点列表", 0).await?;
        let req = self
            .http
            .get(self.url("/reseed/sites/index"))
            .header("token", self.token_header()?);
        let body = self.send(req, "获取站点列表").await?;
        let data: SitesData =
            self.unwrap_envelope(reqwest::StatusCode::OK, &body, "获取站点列表")?;

        // `count` is the server's total, which may exceed `sites.len()` when the
        // account is bound to only part of the catalogue. Log the mismatch
        // instead of silently discarding the field.
        if data.count != len_as_i64(data.sites.len()) {
            tracing::debug!(
                reported = data.count,
                received = data.sites.len(),
                "站点列表数量与服务器报告不一致"
            );
        }

        let mut guard = self.cache.write();
        guard.sites = Some(data.sites.clone());
        guard.sites_fetched_at = Some(Instant::now());
        Ok(data.sites)
    }

    /// Site list keyed by sid, for O(1) lookup during a run.
    ///
    /// # Errors
    /// Propagates [`IyuuClient::sites`]'s failure.
    pub async fn sites_by_id(&self) -> Result<HashMap<i64, IyuuSite>> {
        Ok(self
            .sites(false)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect())
    }

    /// Sites the user has bound ("recommended" set in IYUU's vocabulary).
    ///
    /// # Errors
    /// Returns an error when the request fails or the response cannot be
    /// decoded.
    pub async fn recommended_sites(&self) -> Result<Vec<IyuuSite>> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            count: i64,
            #[serde(default)]
            list: Vec<IyuuSite>,
        }

        self.spend("获取推荐站点", 0).await?;
        let req = self
            .http
            .get(self.url("/reseed/sites/recommend"))
            .header("token", self.token_header()?);
        let body = self.send(req, "获取推荐站点").await?;
        let data: Wrapper = self.unwrap_envelope(reqwest::StatusCode::OK, &body, "获取推荐站点")?;
        if data.count != len_as_i64(data.list.len()) {
            tracing::debug!(
                reported = data.count,
                received = data.list.len(),
                "推荐站点数量与服务器报告不一致"
            );
        }
        Ok(data.list)
    }

    // ------------------------------------------------------- sid_sha1 cache

    /// Report which sites we hold, and get the `sid_sha1` filter token back.
    ///
    /// `sid_list` should be the sids we intend to seed on. Passing every bound
    /// site is correct: the server filters *its* answer down to those, so a
    /// wider list only costs response bytes, never correctness.
    /// # Errors
    /// Returns [`CoreError::Config`] when `sid_list` is empty, and otherwise
    /// propagates request or envelope failures.
    pub async fn report_existing(&self, sid_list: &[i64]) -> Result<String> {
        if sid_list.is_empty() {
            return Err(CoreError::Config(
                "无可汇报的站点，请先在 IYUU 绑定站点".into(),
            ));
        }
        self.spend("汇报站点", 0).await?;
        let req = self
            .http
            .post(self.url("/reseed/sites/reportExisting"))
            .header("token", self.token_header()?)
            .json(&serde_json::json!({ "sid_list": sid_list }));
        let body = self.send(req, "汇报站点").await?;
        let data: SidSha1Data = self.unwrap_envelope(reqwest::StatusCode::OK, &body, "汇报站点")?;
        if data.sid_sha1.is_empty() {
            return Err(CoreError::Upstream {
                what: "汇报站点".into(),
                code: 0,
                msg: "返回的 sid_sha1 为空".into(),
            });
        }

        let mut guard = self.cache.write();
        guard.sid_sha1 = Some(data.sid_sha1.clone());
        guard.sid_sha1_at = Some(Instant::now());
        Ok(data.sid_sha1)
    }

    /// Return a usable `sid_sha1`, fetching one if the cache is cold or stale.
    ///
    /// # Errors
    /// Propagates [`IyuuClient::report_existing`]'s failure.
    pub async fn ensure_sid_sha1(&self, sid_list: &[i64]) -> Result<String> {
        {
            let guard = self.cache.read();
            if let (Some(v), Some(at)) = (&guard.sid_sha1, guard.sid_sha1_at)
                && at.elapsed() < SID_SHA1_TTL
            {
                return Ok(v.clone());
            }
        }
        self.report_existing(sid_list).await
    }

    pub fn invalidate_sid_sha1(&self) {
        let mut guard = self.cache.write();
        guard.sid_sha1 = None;
        guard.sid_sha1_at = None;
    }

    // ------------------------------------------------------------- the query

    /// Cross-seed lookup for one batch (<= 200 hashes).
    ///
    /// Returns a map of `info_hash -> candidates`, and only includes hashes that
    /// actually have candidates (the server omits empty ones).
    ///
    /// # Errors
    /// Returns an error when the request fails, the envelope reports a
    /// non-success code, or `sid_sha1` expired (caller should refresh and
    /// retry).
    pub async fn reseed_query(
        &self,
        hashes: &[String],
        sid_sha1: &str,
    ) -> Result<HashMap<String, Vec<SeedCandidate>>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        if hashes.len() > HASH_CHUNK_SIZE {
            return Err(CoreError::Config(format!(
                "单次查询最多 {HASH_CHUNK_SIZE} 个 hash，收到 {}",
                hashes.len()
            )));
        }

        // The signed payload: sorted, lowercase, compact JSON array. Sorted is a
        // server-side requirement; the sha1 must sign these exact bytes.
        let mut sorted: Vec<String> = hashes
            .iter()
            .map(|h| crate::bencode::normalize_hash(h))
            .collect();
        sorted.sort();
        sorted.dedup();
        let hash_json = serde_json::to_string(&sorted)?;
        let sha1_hex = {
            let mut h = Sha1::new();
            h.update(hash_json.as_bytes());
            hex::encode(h.finalize())
        };

        let timestamp = chrono::Utc::now().timestamp().to_string();
        let form = [
            ("hash", hash_json.as_str()),
            ("sha1", sha1_hex.as_str()),
            ("sid_sha1", sid_sha1),
            ("timestamp", timestamp.as_str()),
            ("version", IYUU_API_VERSION),
        ];

        self.spend("查询辅种", sorted.len()).await?;
        let req = self
            .http
            .post(self.url("/reseed/index/index"))
            .header("token", self.token_header()?)
            .form(&form);
        let body = self.send(req, "查询辅种").await?;

        match self.unwrap_envelope::<ReseedData>(reqwest::StatusCode::OK, &body, "查询辅种") {
            Ok(data) => Ok(data
                .per_hash
                .into_iter()
                .map(|(k, v)| (crate::bencode::normalize_hash(&k), v.torrent))
                .filter(|(_, v)| !v.is_empty())
                .collect()),
            Err(CoreError::Upstream { msg, .. })
                if msg.contains("站点哈希值") || msg.contains("sid_sha1") =>
            {
                // Expired/invalid filter token. Signal so the caller can
                // re-report and retry — we do NOT retry inside this function
                // because the caller owns the sid_list.
                Err(CoreError::SidSha1Expired(msg))
            }
            Err(e) => Err(e),
        }
    }

    /// Convenience wrapper: query with automatic chunking, local budget
    /// enforcement, and one `sid_sha1` refresh-and-retry. Returns per-hash
    /// candidates plus non-fatal warnings.
    ///
    /// **Quota behaviour.** A run is capped at whatever the local mirror says is
    /// left, expressed in whole batches: a query costs one request whether it
    /// carries 1 hash or 200, so sending a 3-hash batch out of a nearly spent
    /// budget wastes a request. When the cap bites, the remainder is dropped with
    /// a warning naming it — silently returning fewer results would look like "no
    /// other sites have these torrents", which is the wrong conclusion.
    ///
    /// Exhaustion never fails the run. Partial results are real results, and a
    /// run that dies with an error throws away the candidates it did find.
    ///
    /// # Errors
    /// Returns an error only when `sid_sha1` cannot be obtained at all (a
    /// credential problem, not a quota one).
    pub async fn reseed_query_all(
        &self,
        hashes: &[String],
        sid_list: &[i64],
    ) -> Result<(HashMap<String, Vec<SeedCandidate>>, Vec<String>)> {
        let mut out: HashMap<String, Vec<SeedCandidate>> = HashMap::new();
        let mut warnings = Vec::new();
        if hashes.is_empty() {
            return Ok((out, warnings));
        }

        // `sid_sha1` costs a request of its own when the cache is cold, so it is
        // fetched before the budget is read — otherwise the first run of the day
        // would overrun by exactly one request.
        let mut sid_sha1 = match self.ensure_sid_sha1(sid_list).await {
            Ok(v) => v,
            Err(CoreError::RateLimited(msg)) => {
                warnings.push(format!("配额不足，本次未查询任何辅种：{msg}"));
                return Ok((out, warnings));
            }
            Err(e) => return Err(e),
        };

        let budget = self.budget();
        let affordable = budget.batches();
        let total_batches = hashes.len().div_ceil(HASH_CHUNK_SIZE);

        if total_batches == 0 {
            return Ok((out, warnings));
        }
        if affordable < total_batches {
            let covered = budget.hashes_affordable();
            warnings.push(format!(
                "本次 {total_batches} 个批次超出今日剩余额度（可查 {affordable} 批 / {covered} 个种子），\
                 已优先查询前 {affordable} 个批次，其余 {} 个种子留待下次",
                hashes.len().saturating_sub(covered)
            ));
            tracing::warn!(
                total_batches,
                affordable,
                requested = hashes.len(),
                "本地配额不足，本次查询已截断"
            );
        }

        let mut refreshed = false;

        for chunk in hashes.chunks(HASH_CHUNK_SIZE).take(affordable) {
            loop {
                match self.reseed_query(chunk, &sid_sha1).await {
                    Ok(map) => {
                        for (k, mut v) in map {
                            out.entry(k).or_default().append(&mut v);
                        }
                        break;
                    }
                    Err(CoreError::SidSha1Expired(_)) if !refreshed => {
                        // One refresh per run is enough: if the new token is
                        // also rejected, the token itself is the problem.
                        refreshed = true;
                        self.invalidate_sid_sha1();
                        match self.report_existing(sid_list).await {
                            Ok(v) => sid_sha1 = v,
                            // Re-reporting costs another request; if that one is
                            // also refused we are done for today.
                            Err(CoreError::RateLimited(msg)) => {
                                warnings
                                    .push(format!("刷新站点哈希被限流，提前结束本次查询：{msg}"));
                                return Ok((out, warnings));
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(CoreError::SidSha1Expired(msg)) => {
                        return Err(CoreError::Upstream {
                            what: "查询辅种".into(),
                            code: 403,
                            msg: format!("sid_sha1 在刷新后仍被拒绝：{msg}"),
                        });
                    }
                    // Quota gone mid-run: keep what we already found and stop.
                    // This is the case the local mirror exists to avoid, and it
                    // means IYUU counts something we do not (another client on
                    // the same token), so continuing is pointless.
                    Err(e @ CoreError::RateLimited(_)) => {
                        warnings.push(format!("查询配额耗尽，提前结束：{e}"));
                        return Ok((out, warnings));
                    }
                    // A single bad chunk shouldn't kill the whole run.
                    Err(e) => {
                        warnings.push(format!("批次查询失败：{e}"));
                        break;
                    }
                }
            }
            // Pacing between batches. `spend()` already spaces consecutive
            // requests by the tier interval; this only keeps a tight free-tier
            // loop from looking like a burst.
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        Ok((out, warnings))
    }

    // --------------------------------------------------------------- binding

    /// Bind a site account. `passkey` must be the **sha1 hex of the site
    /// passkey**, not the passkey itself — IYUU stores only the digest.
    ///
    /// # Errors
    /// Returns an error when the request fails or IYUU rejects the credentials.
    pub async fn bind(&self, req: &BindRequest<'_>) -> Result<()> {
        self.spend("绑定站点", 0).await?;
        let request = self.http.post(self.url("/reseed/users/bind")).form(req);
        let body = self.send(request, "绑定站点").await?;
        let _: serde_json::Value =
            self.unwrap_envelope(reqwest::StatusCode::OK, &body, "绑定站点")?;
        // Binding changes which sites we hold, so both caches are now stale.
        self.invalidate_sid_sha1();
        let mut guard = self.cache.write();
        guard.sites_fetched_at = None;
        Ok(())
    }

    /// Indexed torrent count for a site — useful to validate a site name.
    ///
    /// # Errors
    /// Returns an error when the request fails or the response cannot be
    /// decoded.
    pub async fn site_torrent_total(&self, site: &str) -> Result<i64> {
        #[derive(Deserialize)]
        struct TotalData {
            total: i64,
        }
        self.spend("查站点收录", 0).await?;
        let request = self
            .http
            .get(self.url("/spider/torrent/total"))
            .query(&[("site", site)]);
        let body = self.send(request, "查站点收录").await?;
        let data: TotalData = self.unwrap_envelope(reqwest::StatusCode::OK, &body, "查站点收录")?;
        Ok(data.total)
    }
}

/// Scheme to use when building a URL for a site.
///
/// IYUU's `is_https` field is 0 = http only, 1 = https only, 2 = both. Getting
/// 0 wrong matters: a plain-HTTP tracker has no TLS listener at all, so sending
/// https to it fails at the handshake, and the failure looks like "the site is
/// down" rather than "wrong scheme". Unknown values fall back to https, which is
/// the safe default for anything modern.
#[must_use]
pub fn site_scheme(site: &IyuuSite) -> &'static str {
    match site.is_https {
        0 => "http",
        _ => "https",
    }
}

/// Render a site's download template into a concrete URL.
///
/// Supported placeholders: `{}`, `{id}`, `{passkey}`, `{uid}`, `{torrent_key}`,
/// `{rsskey}`. Anything left unresolved means the template needs a mechanism we
/// don't implement (usually an API call), so we return `None` and let the caller
/// fall back to scraping the details page.
#[must_use]
pub fn render_download_template(
    template: &str,
    torrent_id: i64,
    creds: &SiteCredentials,
) -> Option<String> {
    let mut out = template.to_string();

    // Bare `{}` is the most common form in IYUU's templates.
    out = out.replacen("{}", &torrent_id.to_string(), 1);
    out = out.replace("{tid}", &torrent_id.to_string());
    out = out.replace("{id}", &torrent_id.to_string());
    out = out.replace("{torrent_id}", &torrent_id.to_string());
    out = out.replace("{passkey}", creds.passkey.as_deref().unwrap_or(""));
    out = out.replace("{uid}", creds.uid.as_deref().unwrap_or(""));
    out = out.replace("{torrent_key}", creds.torrent_key.as_deref().unwrap_or(""));
    out = out.replace("{rsskey}", creds.rsskey.as_deref().unwrap_or(""));

    // An unresolved brace means we cannot build this URL ourselves.
    if out.contains('{') || out.contains('}') {
        return None;
    }
    Some(out)
}

/// Per-site credentials, as configured by the user in this app.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SiteCredentials {
    #[serde(default)]
    pub passkey: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Some sites (TGG, `HDCity`) use an authkey instead of a passkey.
    #[serde(default)]
    pub torrent_key: Option<String>,
    /// Monika-style sites embed an rsskey in the RSS URL.
    #[serde(default)]
    pub rsskey: Option<String>,
    /// M-Team needs an API key to mint download tokens.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl SiteCredentials {
    /// Whether a patch carries no field at all.
    ///
    /// Used to refuse creating an entry out of nothing: an empty entry would be
    /// indistinguishable from a configured site in the UI and would make the
    /// engine try to fetch from a site it has no credentials for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.passkey.is_none()
            && self.uid.is_none()
            && self.cookie.is_none()
            && self.user_agent.is_none()
            && self.torrent_key.is_none()
            && self.rsskey.is_none()
            && self.api_key.is_none()
    }

    /// Whether at least one credential IYUU/the fetcher can actually use is set.
    ///
    /// Distinct from [`Self::is_empty`]: an entry holding only a `user_agent` is
    /// not empty, but it also cannot download anything.
    #[must_use]
    pub fn has_any_credential(&self) -> bool {
        [
            &self.passkey,
            &self.cookie,
            &self.torrent_key,
            &self.rsskey,
            &self.api_key,
        ]
        .iter()
        .any(|f| f.as_deref().is_some_and(|v| !v.trim().is_empty()))
    }

    /// Merge a patch into these credentials.
    ///
    /// The two field groups behave differently on purpose:
    ///
    /// * **Secrets** (`passkey`, `cookie`, `torrent_key`, `rsskey`, `api_key`)
    ///   are write-only — the API never returns them, so the browser cannot send
    ///   the current value back. An empty string therefore has to mean
    ///   "unchanged", otherwise editing one field in a per-site form would wipe
    ///   every other stored secret.
    /// * **Identity** (`uid`, `user_agent`) is echoed to the browser, so the form
    ///   always knows the truth and may send it verbatim. An empty value here
    ///   genuinely means "clear it".
    ///
    /// Clearing a whole site is a delete (`null` in the patch), not a blank
    /// merge.
    pub fn merge_from(&mut self, patch: Self) {
        keep_secret(&mut self.passkey, patch.passkey);
        keep_secret(&mut self.cookie, patch.cookie);
        keep_secret(&mut self.torrent_key, patch.torrent_key);
        keep_secret(&mut self.rsskey, patch.rsskey);
        keep_secret(&mut self.api_key, patch.api_key);
        replace_identity(&mut self.uid, patch.uid);
        replace_identity(&mut self.user_agent, patch.user_agent);
    }
}

fn keep_secret(dst: &mut Option<String>, src: Option<String>) {
    if let Some(v) = src.filter(|v| !v.trim().is_empty()) {
        *dst = Some(v);
    }
}

fn replace_identity(dst: &mut Option<String>, src: Option<String>) {
    if let Some(v) = src {
        *dst = if v.trim().is_empty() { None } else { Some(v) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bare_braces_template() {
        let creds = SiteCredentials {
            passkey: Some("deadbeef".into()),
            ..Default::default()
        };
        let url = render_download_template("download.php?id={}&passkey={passkey}", 42, &creds);
        assert_eq!(url.as_deref(), Some("download.php?id=42&passkey=deadbeef"));
    }

    #[test]
    fn renders_named_id_placeholder() {
        let creds = SiteCredentials::default();
        let url = render_download_template("/torrents/download/{id}.torrent", 7, &creds);
        assert_eq!(url.as_deref(), Some("/torrents/download/7.torrent"));
    }

    #[test]
    fn unresolved_placeholder_returns_none() {
        let creds = SiteCredentials::default();
        assert!(render_download_template("dl.php?id={}&sig={unknown}", 1, &creds).is_none());
    }

    #[test]
    fn scheme_respects_is_https_flag() {
        let mut s = IyuuSite {
            id: 1,
            site: "x".into(),
            nickname: String::new(),
            base_url: "x.org".into(),
            download_page: String::new(),
            details_page: String::new(),
            reseed_check: String::new(),
            is_https: 0,
            cookie_required: 0,
            bind_check: String::new(),
        };
        assert_eq!(site_scheme(&s), "http", "is_https=0 means http-only");
        s.is_https = 1;
        assert_eq!(site_scheme(&s), "https");
        s.is_https = 2;
        assert_eq!(site_scheme(&s), "https", "both -> prefer https");
    }

    // ------------------------------------------------------- credential merge

    #[test]
    fn merging_a_blank_secret_keeps_the_stored_one() {
        // The API never returns secrets, so a form re-submit always carries them
        // blank. If blank meant "clear", editing a uid would silently wipe the
        // passkey and every fetch for that site would start failing.
        let mut stored = SiteCredentials {
            passkey: Some("old".into()),
            cookie: Some("c=1".into()),
            api_key: Some("key".into()),
            ..Default::default()
        };
        stored.merge_from(SiteCredentials {
            passkey: Some(String::new()),
            cookie: None,
            api_key: Some("   ".into()),
            uid: Some("42".into()),
            ..Default::default()
        });

        assert_eq!(stored.passkey.as_deref(), Some("old"));
        assert_eq!(stored.cookie.as_deref(), Some("c=1"));
        assert_eq!(stored.api_key.as_deref(), Some("key"));
        assert_eq!(stored.uid.as_deref(), Some("42"));
    }

    #[test]
    fn merging_a_new_secret_replaces_the_old_one() {
        let mut stored = SiteCredentials {
            passkey: Some("old".into()),
            ..Default::default()
        };
        stored.merge_from(SiteCredentials {
            passkey: Some("new".into()),
            ..Default::default()
        });
        assert_eq!(stored.passkey.as_deref(), Some("new"));
    }

    #[test]
    fn identity_fields_can_be_cleared() {
        // `uid`/`user_agent` are echoed to the browser, so the form knows the
        // real value and its blank genuinely means "remove it".
        let mut stored = SiteCredentials {
            uid: Some("42".into()),
            user_agent: Some("UA".into()),
            ..Default::default()
        };
        stored.merge_from(SiteCredentials {
            uid: Some(String::new()),
            user_agent: Some(String::new()),
            ..Default::default()
        });
        assert!(stored.uid.is_none());
        assert!(stored.user_agent.is_none());
    }

    #[test]
    fn a_user_agent_alone_is_not_a_usable_credential() {
        let only_ua = SiteCredentials {
            user_agent: Some("Mozilla/5.0".into()),
            ..Default::default()
        };
        assert!(!only_ua.is_empty(), "it does hold a field");
        assert!(
            !only_ua.has_any_credential(),
            "but nothing that can download a torrent"
        );

        let with_passkey = SiteCredentials {
            passkey: Some("p".into()),
            ..Default::default()
        };
        assert!(with_passkey.has_any_credential());
    }

    #[test]
    fn blank_secrets_do_not_count_as_configured() {
        // A cleared field must not make the UI claim the site is configured.
        let blank = SiteCredentials {
            passkey: Some("  ".into()),
            cookie: Some(String::new()),
            ..Default::default()
        };
        assert!(!blank.has_any_credential());
    }

    // ------------------------------------------------------------- quota

    /// Usage record for "today" with the given counters.
    fn usage_today(requests: u32, hashes: u32) -> QuotaUsage {
        QuotaUsage {
            day: crate::ratelimit::quota_day(Utc::now()),
            used_requests: requests,
            used_hashes: hashes,
        }
    }

    fn client(tier: ApiTier, usage: &QuotaUsage) -> IyuuClient {
        IyuuClient::build("token", tier.base_url(), tier)
            .unwrap()
            .with_usage(usage)
    }

    #[test]
    fn tier_picks_matching_host() {
        assert_eq!(
            client(ApiTier::Free, &usage_today(0, 0)).base_url(),
            "https://2025.iyuu.cn"
        );
        assert_eq!(
            client(ApiTier::Vip, &usage_today(0, 0)).base_url(),
            "https://vip.iyuu.cn"
        );
    }

    #[test]
    fn persisted_usage_is_restored() {
        // A restart mid-day must not hand the next run a fresh budget.
        let c = client(ApiTier::Free, &usage_today(400, 80_000));
        let snap = c.quota();
        assert_eq!(snap.used_requests, 400);
        assert_eq!(snap.remaining_requests, 100);
    }

    #[test]
    fn stale_usage_from_yesterday_is_ignored() {
        let mut usage = usage_today(500, 100_000);
        usage.day = "2000-01-01".into();
        let snap = client(ApiTier::Free, &usage).quota();
        assert_eq!(
            snap.used_requests, 0,
            "yesterday's spend must not count today"
        );
        assert_eq!(snap.remaining_requests, 500);
    }

    #[tokio::test]
    async fn spend_refuses_when_daily_budget_is_gone() {
        // Already at the free limit: no request may go out, and the error must be
        // the typed quota one so the caller stops instead of retrying.
        let c = client(ApiTier::Free, &usage_today(500, 100_000));
        match c.spend("查询辅种", 200).await {
            Err(CoreError::RateLimited(msg)) => assert!(msg.contains("已用尽"), "{msg}"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // A refused request must not be counted as spent.
        assert_eq!(c.usage().used_requests, 500);
    }

    #[tokio::test]
    async fn spend_counts_request_and_hashes() {
        let c = client(ApiTier::Vip, &usage_today(0, 0));
        c.spend("查询辅种", 200).await.unwrap();
        let usage = c.usage();
        assert_eq!(usage.used_requests, 1);
        assert_eq!(usage.used_hashes, 200);
    }

    #[tokio::test]
    async fn budget_is_the_daily_remainder_not_a_per_run_cap() {
        // There is no per-run cap: one run is meant to spend whatever the day
        // still allows, because the account is metered per day, not per run.
        // Verified through the restored counter rather than by spending 500
        // requests — each `spend` paces itself, so looping would take minutes.
        let c = client(ApiTier::Free, &usage_today(1, 200));
        assert_eq!(
            c.budget().batches(),
            499,
            "one run may use all 499 remaining requests"
        );

        let c = client(ApiTier::Vip, &usage_today(1, 200));
        assert_eq!(c.budget().batches(), 2499);
    }

    #[tokio::test]
    async fn budget_reports_whole_batches_only() {
        // 150 requests used of 500 → 350 left; hashes used leave room for 350
        // batches too, so the request count is the binding constraint.
        let c = client(ApiTier::Free, &usage_today(150, 30_000));
        let budget = c.budget();
        assert_eq!(budget.batches(), 350);
        assert_eq!(budget.hashes_affordable(), 70_000);
    }

    #[test]
    fn breaker_error_reports_cooldown() {
        let c = client(ApiTier::Free, &usage_today(0, 0));
        let err = c.trip_breaker("请求过于频繁");
        assert!(matches!(err, CoreError::RateLimited(_)));
        // The cooldown must be visible to the engine, which reads `budget()`.
        assert_eq!(c.budget().batches(), 0);
        assert!(c.quota().blocked_until.is_some());
    }

    #[test]
    fn query_signature_is_over_compact_sorted_json() {
        // Two unsorted, mixed-case inputs must sign identical bytes as the
        // sorted lowercase form — this is what the server verifies.
        let mut a = vec![
            crate::bencode::normalize_hash("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"),
            crate::bencode::normalize_hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ];
        a.sort();
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(
            json,
            r#"["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]"#
        );
        assert!(!json.contains(' '), "compact JSON must have no spaces");
    }
}
