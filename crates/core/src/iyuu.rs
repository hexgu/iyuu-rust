//! IYUU 辅种 API client.
//!
//! Endpoints implemented (all against `https://2025.iyuu.cn`):
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

use crate::error::{CoreError, Result};
use crate::models::{IyuuSite, SeedCandidate};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Version string advertised to the API. The server uses it for compatibility
/// gating, so keep it in sync with what upstream clients send.
pub const IYUU_API_VERSION: &str = "8.2.0";
pub const DEFAULT_BASE_URL: &str = "https://2025.iyuu.cn";

/// Chunk size for `hash` batches. The documented/reference limit is 200; the
/// server rejects larger batches, and a rejection loses the whole chunk, so we
/// stay at the documented value.
pub const HASH_CHUNK_SIZE: usize = 200;

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

/// `sid_sha1` is documented as valid for 7 days; we refresh at 6 to leave slack.
const SID_SHA1_TTL: Duration = Duration::from_hours(144);
/// Site list changes rarely; a day is plenty and avoids a request per run.
const SITES_TTL: Duration = Duration::from_hours(24);

pub struct IyuuClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    cache: RwLock<Cache>,
}

impl IyuuClient {
    /// Build a client against the public IYUU API.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// Build a client against an explicit base URL (used by tests and mirrors).
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built.
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
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
        })
    }

    /// Share one client (and therefore one connection pool + cache) across the
    /// whole process. Every reseed run reuses it.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn token_header(&self) -> Result<reqwest::header::HeaderValue> {
        reqwest::header::HeaderValue::from_str(&self.token)
            .map_err(|_| CoreError::Config("IYUU token contains invalid header characters".into()))
    }

    /// Unwrap the `{code, data, msg}` envelope, mapping the API's own error
    /// codes into a typed error that callers can react to.
    fn unwrap_envelope<T>(status: reqwest::StatusCode, body: &str, what: &str) -> Result<T>
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
                return Err(CoreError::Upstream {
                    what: what.to_string(),
                    code,
                    msg: if msg.is_empty() { body } else { msg },
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

        let req = self
            .http
            .get(self.url("/reseed/sites/index"))
            .header("token", self.token_header()?);
        let body = self.send(req, "获取站点列表").await?;
        let data: SitesData =
            Self::unwrap_envelope(reqwest::StatusCode::OK, &body, "获取站点列表")?;

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

        let req = self
            .http
            .get(self.url("/reseed/sites/recommend"))
            .header("token", self.token_header()?);
        let body = self.send(req, "获取推荐站点").await?;
        let data: Wrapper = Self::unwrap_envelope(reqwest::StatusCode::OK, &body, "获取推荐站点")?;
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
        let req = self
            .http
            .post(self.url("/reseed/sites/reportExisting"))
            .header("token", self.token_header()?)
            .json(&serde_json::json!({ "sid_list": sid_list }));
        let body = self.send(req, "汇报站点").await?;
        let data: SidSha1Data = Self::unwrap_envelope(reqwest::StatusCode::OK, &body, "汇报站点")?;
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

        let req = self
            .http
            .post(self.url("/reseed/index/index"))
            .header("token", self.token_header()?)
            .form(&form);
        let body = self.send(req, "查询辅种").await?;

        match Self::unwrap_envelope::<ReseedData>(reqwest::StatusCode::OK, &body, "查询辅种") {
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

    /// Convenience wrapper: query with automatic chunking and one `sid_sha1`
    /// refresh-and-retry. Returns per-hash candidates plus non-fatal warnings.
    ///
    /// # Errors
    /// Returns an error when a chunk cannot be queried even after refreshing
    /// `sid_sha1`.
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

        let mut sid_sha1 = self.ensure_sid_sha1(sid_list).await?;
        let mut refreshed = false;

        for chunk in hashes.chunks(HASH_CHUNK_SIZE) {
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
                        sid_sha1 = self.report_existing(sid_list).await?;
                    }
                    Err(CoreError::SidSha1Expired(msg)) => {
                        return Err(CoreError::Upstream {
                            what: "查询辅种".into(),
                            code: 403,
                            msg: format!("sid_sha1 在刷新后仍被拒绝：{msg}"),
                        });
                    }
                    // A single bad chunk shouldn't kill the whole run.
                    Err(e) => {
                        warnings.push(format!("批次查询失败：{e}"));
                        break;
                    }
                }
            }
            // Be a good API citizen: no documented rate limit, but the site does
            // throttle, and burning that budget gets the token temp-banned.
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
        let request = self.http.post(self.url("/reseed/users/bind")).form(req);
        let body = self.send(request, "绑定站点").await?;
        let _: serde_json::Value =
            Self::unwrap_envelope(reqwest::StatusCode::OK, &body, "绑定站点")?;
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
        let request = self
            .http
            .get(self.url("/spider/torrent/total"))
            .query(&[("site", site)]);
        let body = self.send(request, "查站点收录").await?;
        let data: TotalData = Self::unwrap_envelope(reqwest::StatusCode::OK, &body, "查站点收录")?;
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
