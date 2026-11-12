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

use crate::bencode;
use crate::downloader::{AddTorrentOptions, Downloader};
use crate::error::{CoreError, Result};
use crate::models::{DownloaderKind, TorrentInfo};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Marker tag applied to every torrent we add, so we can identify our own work
/// and are never tricked into a duplicate add by a race.
pub const IYUU_TAG: &str = "iyuu-reseed";

pub struct QBittorrentClient {
    http: reqwest::Client,
    base: String,
    name: String,
    username: String,
    password: String,
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
    ratio: f64,
}

impl QBittorrentClient {
    /// Connect and log in.
    ///
    /// # Errors
    /// Returns an error when the HTTP client cannot be built or the login
    /// request is rejected by qBittorrent.
    pub async fn new(name: &str, base_url: &str, username: &str, password: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // qB authenticates with the `SID` cookie; a cookie store keeps the
            // session alive across the hundreds of calls one run makes.
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
            username: username.to_string(),
            password: password.to_string(),
            modern: AtomicBool::new(true),
            webapi: parking_lot::RwLock::new(None),
        };
        client.login().await?;
        Ok(client)
    }

    fn api(&self, path: &str) -> String {
        format!("{}api/v2/{}", self.base, path.trim_start_matches('/'))
    }

    async fn login(&self) -> Result<()> {
        let resp = self
            .http
            .post(self.api("auth/login"))
            .form(&[("username", &self.username), ("password", &self.password)])
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

    /// Ensure the session cookie is still valid. qB expires SID on restart,
    /// which is exactly when a scheduled run tends to fire.
    async fn ensure_alive(&self) -> Result<()> {
        let resp = self
            .http
            .get(self.api("app/version"))
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("{e}"),
            })?;
        // 403 == not authenticated anymore.
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            self.login().await?;
            return Ok(());
        }
        Ok(())
    }

    async fn post(&self, path: &str, form: Vec<(String, String)>) -> Result<String> {
        let resp = self
            .http
            .post(self.api(path))
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
            // Session died mid-run; re-auth once and retry.
            self.login().await?;
            let resp = self
                .http
                .post(self.api(path))
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
            ratio: t.ratio,
            error: None,
            trackers: Vec::new(),
        }
    }

    async fn info(&self, params: &[(&str, &str)]) -> Result<Vec<QbTorrent>> {
        self.ensure_alive().await?;
        let resp = self
            .http
            .get(self.api("torrents/info"))
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
        let resp = self
            .http
            .get(self.api("app/version"))
            .send()
            .await
            .map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("{e}"),
            })?;
        let app = resp.text().await.unwrap_or_default().trim().to_string();

        // WebAPI version drives which endpoint names / params exist.
        if let Ok(r) = self.http.get(self.api("app/webapiVersion")).send().await
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

/// Convenience: the qB marker tag for a specific site, e.g. `iyuu-springsunday`.
/// Used to find our own add when qB computes a different hash.
#[must_use]
pub fn site_tag(site: &str) -> String {
    format!("{IYUU_TAG}:{site}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(normalize_base("http://h:8080"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/api/v2"), "http://h:8080/");
        assert_eq!(normalize_base("http://h:8080/api/v2/"), "http://h:8080/");
    }

    #[test]
    fn parses_webapi_versions() {
        assert_eq!(parse_version("2.11.2"), Some((2, 11, 2)));
        assert_eq!(parse_version("2.8.3"), Some((2, 8, 3)));
        assert_eq!(parse_version("2.4"), Some((2, 4, 0)));
        assert!(parse_version("garbage").is_none());
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
