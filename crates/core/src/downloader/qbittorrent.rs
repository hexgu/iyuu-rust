//! qBittorrent `WebAPI` v2 adapter.
//!
//! Version handling is the main hazard here. qB 5.x:
//!   * renamed `torrents/pause` → `torrents/stop` and `resume` → `start`
//!     (old names still respond in some builds, but not reliably),
//!   * renamed torrent states `paused*` → `stopped*`,
//!   * dropped `skip_checking` from `torrents/add` in later 5.x releases, so a
//!     cross-seed added that way lands in a stopped, unverified state instead of
//!     a verified-complete one.
//!
//! We therefore (a) probe `app/webapiVersion` once, (b) prefer the new endpoint
//! names and fall back to the old ones, and (c) never rely on `skip_checking` to
//! mean "done" — verification state is always read back from the torrent itself.
//!
//! Authentication has two modes. Password mode logs in and rides the `SID`
//! cookie, which qB invalidates on restart — hence the re-login paths below.
//! API-key mode (qB ≥ 5.2 / `WebAPI` ≥ 2.14.1) sends `Authorization: Bearer <key>`
//! on every request instead: stateless, survives restarts, and `auth/login`
//! must *not* be called because API keys are barred from the auth endpoints.

use crate::bencode;
use crate::downloader::{AddTorrentOptions, Downloader};
use crate::error::{CoreError, Result};
use crate::models::{DownloaderKind, TorrentInfo};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// Tag constants live in `crate::downloader` (`AUX_TAG` / `LEGACY_AUX_TAG`):
// tagging is a cross-client policy, not a qBittorrent detail.

/// How to authenticate against the `WebUI`.
#[derive(Debug, Clone)]
pub enum QbAuth {
    /// `POST auth/login` + the `SID` cookie.
    Password { username: String, password: String },
    /// `Authorization: Bearer <key>` (qB ≥ 5.2).
    ApiKey(String),
}

impl QbAuth {
    /// Pick the right mode from persisted settings.
    ///
    /// A key wins over a username even when both are stored, because the key is
    /// the more specific choice and a stale username must not silently take over.
    /// Whitespace-only values are treated as absent so a stray space in the form
    /// cannot select a mode that then fails to authenticate.
    #[must_use]
    pub fn from_settings(username: &str, password: &str, api_key: Option<&str>) -> Self {
        match api_key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(key) => Self::ApiKey(key.to_string()),
            None => Self::Password {
                username: username.to_string(),
                password: password.to_string(),
            },
        }
    }

    fn is_api_key(&self) -> bool {
        matches!(self, Self::ApiKey(_))
    }

    /// Headers this mode adds to every request. Empty for password mode: the
    /// cookie jar injects `SID` on its own.
    fn headers(&self, client: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        if let Self::ApiKey(key) = self {
            let value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                CoreError::DownloaderAuth {
                    client: client.to_string(),
                    msg: "API Key 含有非法字符（不能包含空格或换行）".into(),
                }
            })?;
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }
}

pub struct QBittorrentClient {
    http: reqwest::Client,
    base: String,
    name: String,
    auth: QbAuth,
    /// True once we know the API speaks the 5.x dialect.
    modern: AtomicBool,
    /// Cached `webapiVersion` as a comparable tuple, e.g. (2, 11, 0).
    webapi: parking_lot::RwLock<Option<(u32, u32, u32)>>,
}

#[derive(Debug, Deserialize)]
struct QbTorrent {
    hash: String,
    name: String,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    content_path: Option<String>,
    #[serde(default)]
    total_size: i64,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    added_on: i64,
    #[serde(default)]
    seeding_time: i64,
    #[serde(default)]
    uploaded: i64,
    #[serde(default)]
    downloaded: i64,
    #[serde(default)]
    ratio: f64,
    /// Carries every announce URL; qB does not expose a tracker list in
    /// `torrents/info`.
    #[serde(default)]
    magnet_uri: Option<String>,
}

impl QBittorrentClient {
    /// Connect and authenticate.
    ///
    /// In password mode this logs in; in API-key mode it verifies the key with a
    /// single authenticated call, so a wrong key fails here instead of on the
    /// first real query.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built or qBittorrent
    /// rejects the credentials.
    pub async fn new(name: &str, base_url: &str, auth: QbAuth) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // qB authenticates with the `SID` cookie in password mode; a cookie
            // store keeps the session alive across the hundreds of calls one run
            // makes. Harmless in API-key mode, where no cookie is ever set.
            .cookie_store(true)
            .user_agent(concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| CoreError::Downloader {
                client: name.into(),
                msg: format!("构造 HTTP 客户端失败：{e}"),
            })?;

        let client = Self {
            http,
            base: normalize_base(base_url),
            name: name.to_string(),
            auth,
            modern: AtomicBool::new(true),
            webapi: parking_lot::RwLock::new(None),
        };
        client.authenticate().await?;
        Ok(client)
    }

    fn api(&self, path: &str) -> String {
        format!("{}api/v2/{}", self.base, path.trim_start_matches('/'))
    }

    /// Establish (or verify) authentication for the configured mode.
    async fn authenticate(&self) -> Result<()> {
        if self.auth.is_api_key() {
            // API keys cannot call the auth endpoints, so probe a cheap private
            // endpoint instead: 200 means the key is good.
            self.probe().await
        } else {
            self.login().await
        }
    }

    /// One authenticated GET used to validate an API key.
    async fn probe(&self) -> Result<()> {
        let resp = self
            .http
            .get(self.api("app/version"))
            .headers(self.auth.headers(&self.name)?)
            .send()
            .await
            .map_err(|e| CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: format!("无法连接：{e}"),
            })?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(CoreError::DownloaderAuth {
            client: self.name.clone(),
            msg: api_key_rejection(resp.status()),
        })
    }

    async fn login(&self) -> Result<()> {
        let (username, password) = match &self.auth {
            QbAuth::Password { username, password } => (username, password),
            // A caller asked for a password session but the stored mode is
            // key-based: nothing to do.
            QbAuth::ApiKey(_) => return Ok(()),
        };
        let resp = self
            .http
            .post(self.api("auth/login"))
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|e| CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: format!("无法连接：{e}"),
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: format!(
                    "HTTP {status}：{}",
                    body.chars().take(200).collect::<String>()
                ),
            });
        }
        // qB answers 200 with the literal "Fails." on bad credentials, or
        // "Your IP has been banned" after repeated failures.
        if body.trim().eq_ignore_ascii_case("Fails.") {
            return Err(CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: "用户名或密码错误".into(),
            });
        }
        if body.to_ascii_lowercase().contains("banned") {
            return Err(CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: "IP 已被 qBittorrent 临时封禁，请稍后再试".into(),
            });
        }
        Ok(())
    }

    /// Ensure we are still authenticated.
    ///
    /// qB expires the `SID` cookie on restart, which is exactly when a scheduled
    /// run tends to fire — so password mode re-logs in on a 403. An API key never
    /// expires, and a 403 there means the key was rotated or revoked: retrying
    /// the same key cannot help, so the error is surfaced as-is.
    async fn ensure_alive(&self) -> Result<()> {
        let resp = self
            .http
            .get(self.api("app/version"))
            .headers(self.auth.headers(&self.name)?)
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("{e}"),
            })?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            if self.auth.is_api_key() {
                return Err(CoreError::DownloaderAuth {
                    client: self.name.clone(),
                    msg: api_key_rejection(resp.status()),
                });
            }
            self.login().await?;
        }
        Ok(())
    }

    async fn post(&self, path: &str, form: Vec<(String, String)>) -> Result<String> {
        let headers = self.auth.headers(&self.name)?;
        let resp = self
            .http
            .post(self.api(path))
            .headers(headers.clone())
            .form(&form)
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("POST {path} 失败：{e}"),
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN {
            // Session died mid-run; re-auth once and retry. With an API key there
            // is no session to revive, so report the revocation instead of
            // hammering qB with a key it already rejected.
            if self.auth.is_api_key() {
                return Err(CoreError::DownloaderAuth {
                    client: self.name.clone(),
                    msg: api_key_rejection(status),
                });
            }
            self.login().await?;
            let resp = self
                .http
                .post(self.api(path))
                .headers(headers)
                .form(&form)
                .send()
                .await
                .map_err(|e| CoreError::Downloader {
                    client: self.name.clone(),
                    msg: format!("POST {path} 重试失败：{e}"),
                })?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(CoreError::Downloader {
                    client: self.name.clone(),
                    msg: format!("POST {path} HTTP {status}：{}", body.trim()),
                });
            }
            return Ok(body);
        }
        if !status.is_success() {
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("POST {path} HTTP {status}：{}", body.trim()),
            });
        }
        Ok(body)
    }

    /// Call a pair of endpoint names, preferring the modern one.
    async fn post_either(
        &self,
        modern_path: &str,
        legacy_path: &str,
        form: Vec<(String, String)>,
    ) -> Result<()> {
        if self.modern.load(Ordering::Relaxed) {
            if self.post(modern_path, form.clone()).await.is_ok() {
                return Ok(());
            }
            // Fall through to the legacy name and remember the result.
            self.post(legacy_path, form).await?;
            self.modern.store(false, Ordering::Relaxed);
            return Ok(());
        }
        self.post(legacy_path, form).await?;
        Ok(())
    }

    fn hashes_form(hashes: &[String]) -> String {
        // qB accepts a pipe-separated list for batch endpoints.
        hashes.join("|")
    }

    fn map_torrent(t: QbTorrent) -> TorrentInfo {
        let tags = t
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        TorrentInfo {
            hash: bencode::normalize_hash(&t.hash),
            name: t.name,
            save_path: t.save_path,
            content_path: t.content_path.filter(|p| !p.is_empty()),
            total_size: t.total_size,
            progress: t.progress,
            state: t.state,
            kind: DownloaderKind::Qbittorrent,
            category: if t.category.is_empty() {
                None
            } else {
                Some(t.category)
            },
            tags,
            added_on: if t.added_on > 0 {
                Utc.timestamp_opt(t.added_on, 0).single()
            } else {
                None
            },
            seeding_time: Some(t.seeding_time),
            uploaded: t.uploaded,
            downloaded: t.downloaded,
            ratio: t.ratio,
            error: None,
            // 不用 `.map(..).unwrap_or_default()`：新版 clippy 会要求换成
            // `map_or_default`，而那个方法在当前的 stable 上还没有。
            trackers: match t.magnet_uri.as_deref() {
                Some(magnet) => tracker_hosts(magnet),
                None => Vec::new(),
            },
        }
    }

    async fn info(&self, params: &[(&str, &str)]) -> Result<Vec<QbTorrent>> {
        self.ensure_alive().await?;
        let resp = self
            .http
            .get(self.api("torrents/info"))
            .headers(self.auth.headers(&self.name)?)
            .query(params)
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("查询种子列表失败：{e}"),
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN {
            if self.auth.is_api_key() {
                return Err(CoreError::DownloaderAuth {
                    client: self.name.clone(),
                    msg: api_key_rejection(status),
                });
            }
            self.login().await?;
            return Box::pin(self.info(params)).await;
        }
        if !status.is_success() {
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("查询种子列表 HTTP {status}：{}", body.trim()),
            });
        }
        serde_json::from_str(&body).map_err(|e| CoreError::Downloader {
            client: self.name.clone(),
            msg: format!("解析种子列表失败：{e}"),
        })
    }
}

#[async_trait]
impl Downloader for QBittorrentClient {
    fn kind(&self) -> DownloaderKind {
        DownloaderKind::Qbittorrent
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn version(&self) -> Result<String> {
        self.ensure_alive().await?;
        let headers = self.auth.headers(&self.name)?;
        let resp = self
            .http
            .get(self.api("app/version"))
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("{e}"),
            })?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN && self.auth.is_api_key() {
            return Err(CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: api_key_rejection(resp.status()),
            });
        }
        let app = resp.text().await.unwrap_or_default().trim().to_string();

        // WebAPI version drives which endpoint names / params exist.
        if let Ok(r) = self
            .http
            .get(self.api("app/webapiVersion"))
            .headers(headers)
            .send()
            .await
            && let Ok(v) = r.text().await
        {
            let parsed = parse_version(v.trim());
            *self.webapi.write() = parsed;
            if let Some((maj, min, _)) = parsed {
                // torrents/stop|start landed in WebAPI 2.11 (qB 5.0).
                self.modern
                    .store(maj > 2 || (maj == 2 && min >= 11), Ordering::Relaxed);
            }
        }
        Ok(app)
    }

    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>> {
        Ok(self
            .info(&[("filter", "all")])
            .await?
            .into_iter()
            .map(Self::map_torrent)
            .collect())
    }

    async fn torrents_by_hash(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let list = Self::hashes_form(hashes);
        Ok(self
            .info(&[("hashes", list.as_str())])
            .await?
            .into_iter()
            .map(Self::map_torrent)
            .collect())
    }

    async fn add_torrent(&self, torrent_bytes: &[u8], opts: &AddTorrentOptions) -> Result<String> {
        // Hash locally first: qB's add response carries no hash, and we must
        // verify against something the client actually accepted.
        let local_hash =
            bencode::info_hash_from_torrent(torrent_bytes).map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("种子文件解析失败：{e}"),
            })?;
        let hex_hash = bencode::hash_to_hex(&local_hash);

        let part = reqwest::multipart::Part::bytes(torrent_bytes.to_vec())
            .file_name("cross-seed.torrent")
            .mime_str("application/x-bittorrent")
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("{e}"),
            })?;

        let mut form = reqwest::multipart::Form::new().part("torrents", part);

        if let Some(sp) = opts.save_path.as_deref().filter(|s| !s.is_empty()) {
            form = form.text("savepath", sp.to_string());
        }
        // `autoTMM=false` is what makes the save path stick. With it left on,
        // qB relocates the torrent into the category's directory, the data is
        // "missing", and the cross-seed verifies to 0% — the classic failure.
        if opts.force_reuse_save_path {
            form = form.text("autoTMM", "false".to_string());
        }
        if let Some(cat) = opts.category.as_deref().filter(|c| !c.is_empty()) {
            form = form.text("category", cat.to_string());
        }
        if !opts.tags.is_empty() {
            form = form.text("tags", opts.tags.join(","));
        }
        // qB 5.x uses `stopped`, older uses `paused`. Send both: unknown form
        // fields are ignored, so this is safe across versions.
        if opts.paused {
            form = form.text("paused", "true".to_string());
            form = form.text("stopped", "true".to_string());
        }
        if opts.skip_checking {
            form = form.text("skip_checking", "true".to_string());
        }

        self.ensure_alive().await?;
        let resp = self
            .http
            .post(self.api("torrents/add"))
            .headers(self.auth.headers(&self.name)?)
            .header(
                USER_AGENT,
                concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")),
            )
            .multipart(form)
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("添加种子失败：{e}"),
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN {
            if self.auth.is_api_key() {
                return Err(CoreError::DownloaderAuth {
                    client: self.name.clone(),
                    msg: api_key_rejection(status),
                });
            }
            self.login().await?;
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: "会话失效，已重新登录，请重试".into(),
            });
        }
        if !status.is_success() {
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("添加种子 HTTP {status}：{}", body.trim()),
            });
        }
        // qB says "Ok." on success, "Fails." on rejection.
        let trimmed = body.trim();
        if trimmed.eq_ignore_ascii_case("Fails.") {
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: "qBittorrent 拒绝了该种子（Fails.）".into(),
            });
        }

        // Wait for the torrent to appear so callers can immediately act on it.
        // qB is asynchronous: the add returns before the torrent is queryable.
        for attempt in 0..10u64 {
            if self
                .torrents_by_hash(std::slice::from_ref(&hex_hash))
                .await
                .is_ok_and(|v| !v.is_empty())
            {
                return Ok(hex_hash);
            }
            // Fall back to tag lookup, in case qB computed a different v1 hash
            // (hybrid v1/v2 torrents).
            if let Some(tag) = opts.tags.iter().find(|t| t.starts_with("iyuu-"))
                && let Ok(found) = self.info(&[("tag", tag.as_str())]).await
                && let Some(t) = found.first()
            {
                return Ok(bencode::normalize_hash(&t.hash));
            }
            tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
        }

        // Accept it anyway: the torrent is very likely there, just slow.
        Ok(hex_hash)
    }

    async fn recheck(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.post(
            "torrents/recheck",
            vec![("hashes".into(), Self::hashes_form(hashes))],
        )
        .await?;
        Ok(())
    }

    async fn start(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.post_either(
            "torrents/start",
            "torrents/resume",
            vec![("hashes".into(), Self::hashes_form(hashes))],
        )
        .await
    }

    async fn stop(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.post_either(
            "torrents/stop",
            "torrents/pause",
            vec![("hashes".into(), Self::hashes_form(hashes))],
        )
        .await
    }

    async fn set_category(&self, hashes: &[String], category: &str) -> Result<()> {
        if hashes.is_empty() || category.is_empty() {
            return Ok(());
        }
        self.post(
            "torrents/setCategory",
            vec![
                ("hashes".into(), Self::hashes_form(hashes)),
                ("category".into(), category.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn add_tags(&self, hashes: &[String], tags: &[String]) -> Result<()> {
        if hashes.is_empty() || tags.is_empty() {
            return Ok(());
        }
        self.post(
            "torrents/addTags",
            vec![
                ("hashes".into(), Self::hashes_form(hashes)),
                ("tags".into(), tags.join(",")),
            ],
        )
        .await?;
        Ok(())
    }

    async fn remove_tags(&self, hashes: &[String], tags: &[String]) -> Result<bool> {
        if hashes.is_empty() || tags.is_empty() {
            return Ok(true);
        }
        self.post(
            "torrents/removeTags",
            vec![
                ("hashes".into(), Self::hashes_form(hashes)),
                ("tags".into(), tags.join(",")),
            ],
        )
        .await?;
        Ok(true)
    }

    async fn disable_auto_management(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.post(
            "torrents/setAutoManagement",
            vec![
                ("hashes".into(), Self::hashes_form(hashes)),
                ("enable".into(), "false".into()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, hashes: &[String], delete_files: bool) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        self.post(
            "torrents/delete",
            vec![
                ("hashes".into(), Self::hashes_form(hashes)),
                ("deleteFiles".into(), delete_files.to_string()),
            ],
        )
        .await?;
        Ok(())
    }
}

/// Explain an authentication failure in API-key mode.
///
/// An API key is stateless — there is no session to renew — so a 401/403 can
/// only mean the key is wrong, was rotated (rotating invalidates the old key
/// immediately), or the server predates 5.2 and does not understand the header
/// at all. Say that instead of "re-login and retry", which cannot help.
fn api_key_rejection(status: reqwest::StatusCode) -> String {
    format!(
        "API Key 被拒绝（HTTP {}）。请确认 qBittorrent ≥ 5.2 且密钥未被轮换，\
         可在「设置 → WebUI → API Key」重新生成",
        status.as_u16()
    )
}

/// Strip trailing slashes and re-add exactly one before `api/v2`.
fn normalize_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    // Allow users to paste a URL with or without the WebUI path.
    if trimmed.ends_with("/api/v2") {
        format!("{}/", trimmed.trim_end_matches("/api/v2"))
    } else {
        format!("{trimmed}/")
    }
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut parts = v.split('.');
    let maj = parts.next()?.trim().parse().ok()?;
    let min = parts.next().unwrap_or("0").trim().parse().ok()?;
    let patch = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
    Some((maj, min, patch))
}

/// Tracker hosts out of a magnet URI (`magnet:?xt=…&tr=https%3A%2F%2Ftracker…`).
///
/// `torrents/info` does not carry a tracker list, but it does carry `magnet_uri`,
/// which is where qB keeps every announce URL. Parsing it here avoids one
/// `torrents/trackers` request per torrent just to label a cross-seed with its
/// site.
#[must_use]
pub fn tracker_hosts(magnet: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for pair in magnet.split('&') {
        let Some(value) = pair.strip_prefix("tr=") else {
            continue;
        };
        let decoded = urlencoding::decode(value).unwrap_or_else(|_| value.into());
        if let Some(host) = super::host_of(&decoded)
            && !hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
        {
            hosts.push(host);
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_trackers_from_the_magnet_uri() {
        let magnet = "magnet:?xt=urn:btih:abc&dn=Some.Show&tr=https%3A%2F%2Ftracker.example.org%2Fannounce&tr=udp%3A%2F%2Ftracker.other.net%3A6969%2Fannounce";
        assert_eq!(
            tracker_hosts(magnet),
            vec!["tracker.example.org", "tracker.other.net"]
        );
        assert_eq!(tracker_hosts(""), Vec::<String>::new());
    }

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(normalize_base("http://h:8080"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/api/v2"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/api/v2/"), "http://h:8080/");
    }

    #[test]
    fn picks_api_key_when_one_is_configured() {
        // A non-empty key wins over a stored username: the key is the more
        // specific choice, and password-mode fields are often left over.
        let auth = QbAuth::from_settings("admin", "adminadmin", Some("qbt_abc"));
        assert!(
            matches!(&auth, QbAuth::ApiKey(k) if k == "qbt_abc"),
            "expected API-key mode"
        );
        assert!(auth.is_api_key());

        // Blank / whitespace keys fall back to password mode instead of sending
        // an empty Bearer token that could only ever be rejected.
        for blank in [None, Some(""), Some("   ")] {
            let auth = QbAuth::from_settings("admin", "pw", blank);
            assert!(
                !auth.is_api_key(),
                "blank key {blank:?} must not select API-key mode"
            );
        }
    }

    #[test]
    fn api_key_mode_sends_a_bearer_header() {
        let headers = QbAuth::ApiKey("qbt_test".into())
            .headers("qBittorrent")
            .expect("valid header");
        assert_eq!(
            headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
            Some("Bearer qbt_test")
        );

        // Password mode adds nothing: the cookie jar supplies `SID`.
        let headers = QbAuth::Password {
            username: "u".into(),
            password: "p".into(),
        }
        .headers("qBittorrent")
        .expect("valid header");
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn newline_in_api_key_is_rejected() {
        // Header values cannot carry CR/LF; failing here beats sending a request
        // that reqwest would refuse with a much less obvious error.
        let err = QbAuth::ApiKey("qbt_bad\nkey".into())
            .headers("qBittorrent")
            .unwrap_err();
        assert!(err.to_string().contains("API Key"), "{err}");
    }

    #[test]
    fn parses_webapi_versions() {
        assert_eq!(parse_version("2.11.2"), Some((2, 11, 2)));
        assert_eq!(parse_version("2.8.3"), Some((2, 8, 3)));
        assert_eq!(parse_version("2.4"), Some((2, 4, 0)));
        assert!(parse_version("garbage").is_none());
    }

    /// Minimal fake `WebUI`: records every request, answers `app/version` only to a
    /// valid bearer token, and rejects anything that touches `auth/login`.
    async fn fake_server(expected_key: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Bind on an ephemeral port and hand the address back to the caller.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    continue;
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // The bare minimum protocol handling: this only needs to prove
                // what the client sends, not to serve a real WebUI.
                let ok = req.starts_with("GET /api/v2/app/version ")
                    && req
                        .to_ascii_lowercase()
                        .contains(&format!("authorization: bearer {expected_key}"));
                let (status, body) = if ok {
                    ("200 OK", "v5.2.0")
                } else {
                    ("401 Unauthorized", "")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn api_key_mode_authenticates_without_logging_in() {
        // The fake server only accepts the exact Bearer header, so a client that
        // tried `auth/login` (which API keys are barred from) could not pass.
        let (base, server) = fake_server("qbt_test_key").await;
        let client = QBittorrentClient::new("t", &base, QbAuth::ApiKey("qbt_test_key".into()))
            .await
            .expect("API key should authenticate");
        assert!(client.auth.is_api_key());
        server.abort();
    }

    #[tokio::test]
    async fn wrong_api_key_fails_at_construction() {
        let (base, server) = fake_server("qbt_right_key").await;
        let err = QBittorrentClient::new("t", &base, QbAuth::ApiKey("qbt_wrong".into()))
            .await
            .err()
            .expect("a wrong key must not authenticate");
        let msg = err.to_string();
        assert!(msg.contains("API Key 被拒绝"), "{msg}");
        // The advice must point at the one action that can fix it.
        assert!(msg.contains("轮换") || msg.contains("重新生成"), "{msg}");
        server.abort();
    }

    #[test]
    fn detects_modern_states() {
        let t = TorrentInfo {
            hash: "a".repeat(40),
            name: "x".into(),
            save_path: "/d".into(),
            content_path: None,
            total_size: 100,
            progress: 1.0,
            state: "stoppedUP".into(),
            kind: DownloaderKind::Qbittorrent,
            category: None,
            tags: vec![],
            added_on: None,
            seeding_time: Some(0),
            uploaded: 0,
            downloaded: 0,
            ratio: 0.0,
            error: None,
            trackers: vec![],
        };
        // stoppedUP at 100% is a verified-complete torrent (qB 5.x spelling).
        assert!(t.is_verified_complete());

        let mut checking = t.clone();
        checking.state = "checkingDL".into();
        assert!(!checking.is_verified_complete());

        let mut paused = t.clone();
        paused.state = "pausedDL".into();
        paused.progress = 0.0;
        assert!(!paused.is_verified_complete());
    }
}
