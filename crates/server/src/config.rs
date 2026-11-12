//! Application configuration, persisted as one JSON blob in SQLite.
//!
//! Kept in the database rather than a TOML file so the UI can change it at
//! runtime without a restart or a bind mount, and so a Docker user never has to
//! hand-edit a file. Secrets live here too — it is the app's own data directory,
//! same trust boundary as the .torrent cache.

use crate::db::Db;
use crate::error::{Result, ServerError};
use iyuu_core::downloader::DownloaderExtra;
use iyuu_core::engine::DiscoveryConfig;
use iyuu_core::iyuu::SiteCredentials;
use iyuu_core::models::DownloaderKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const SETTINGS_KEY: &str = "app_config";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub iyuu: IyuuConfig,
    #[serde(default)]
    pub downloader: DownloaderConfig,
    #[serde(default)]
    pub reseed: ReseedConfig,
    #[serde(default)]
    pub credentials: HashMap<String, SiteCredentials>,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IyuuConfig {
    #[serde(default)]
    pub token: String,
    /// Override the API base (mirrors exist; default is 2025.iyuu.cn).
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloaderConfig {
    #[serde(default = "default_downloader_kind")]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_qb_url")]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub extra: DownloaderExtra,
}

fn default_downloader_kind() -> String {
    "qbittorrent".into()
}
fn default_qb_url() -> String {
    "http://127.0.0.1:8080".into()
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            kind: default_downloader_kind(),
            name: "qBittorrent".into(),
            base_url: default_qb_url(),
            username: String::new(),
            password: String::new(),
            extra: DownloaderExtra::default(),
        }
    }
}

impl DownloaderConfig {
    pub fn kind(&self) -> std::result::Result<DownloaderKind, ServerError> {
        DownloaderKind::parse(&self.kind)
            .ok_or_else(|| ServerError::Config(format!("未知的下载器类型：{}", self.kind)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReseedConfig {
    #[serde(default)]
    pub min_size_gib: f64,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    #[serde(default)]
    pub site_ids: Vec<i64>,
    #[serde(default = "default_true")]
    pub reuse_category: bool,
    #[serde(default)]
    pub default_category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_true")]
    pub tag_with_site: bool,
    #[serde(default = "default_true")]
    pub auto_start_after_verify: bool,
    #[serde(default = "default_fetch_concurrency")]
    pub fetch_concurrency: usize,
    /// Verification poll interval in seconds.
    #[serde(default = "default_verify_interval")]
    pub verify_interval_secs: u64,
}

fn default_true() -> bool {
    true
}
fn default_fetch_concurrency() -> usize {
    2
}
fn default_verify_interval() -> u64 {
    15
}

impl Default for ReseedConfig {
    fn default() -> Self {
        Self {
            min_size_gib: 0.0,
            exclude_tags: Vec::new(),
            exclude_paths: Vec::new(),
            site_ids: Vec::new(),
            reuse_category: true,
            default_category: None,
            tags: Vec::new(),
            tag_with_site: true,
            auto_start_after_verify: true,
            fetch_concurrency: 2,
            verify_interval_secs: 15,
        }
    }
}

impl From<&ReseedConfig> for DiscoveryConfig {
    fn from(c: &ReseedConfig) -> Self {
        Self {
            min_size_gib: c.min_size_gib,
            exclude_tags: c.exclude_tags.clone(),
            exclude_paths: c.exclude_paths.clone(),
            site_ids: c.site_ids.clone(),
            reuse_category: c.reuse_category,
            default_category: c.default_category.clone(),
            tags: c.tags.clone(),
            tag_with_site: c.tag_with_site,
            auto_start_after_verify: c.auto_start_after_verify,
            fetch_concurrency: c.fetch_concurrency.clamp(1, 16),
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Cron expression, 5 fields (`min hour dom mon dow`).
    #[serde(default = "default_cron")]
    pub cron: String,
}

fn default_cron() -> String {
    "0 4 * * *".into()
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: default_cron(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub language: Option<String>,
}

impl AppConfig {
    pub fn load(db: &Db) -> Result<Self> {
        match db.get_setting(SETTINGS_KEY)? {
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| ServerError::Config(format!("已保存的配置无法解析：{e}"))),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, db: &Db) -> Result<()> {
        let raw = serde_json::to_string(self)?;
        db.set_setting(SETTINGS_KEY, &raw)?;
        Ok(())
    }

    /// Convert the configured credentials into the map the engine wants.
    /// Keys are IYUU site ids stored as strings (JSON object keys).
    pub fn credential_map(&self) -> iyuu_core::engine::CredentialMap {
        self.credentials
            .iter()
            .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, v.clone())))
            .collect()
    }

    /// Validate before saving so the UI can show a real message instead of
    /// discovering the problem mid-run.
    pub fn validate(&self) -> Result<()> {
        if self.iyuu.token.trim().is_empty() {
            return Err(ServerError::Config("IYUU Token 不能为空".into()));
        }
        self.downloader.kind()?;
        if self.downloader.base_url.trim().is_empty() {
            return Err(ServerError::Config("下载器地址不能为空".into()));
        }
        if self.schedule.enabled {
            // Validate the cron by attempting a parse in the scheduler module;
            // here we only check the field count to fail fast with a clear error.
            if self.schedule.cron.split_whitespace().count() != 5 {
                return Err(ServerError::Config(
                    "Cron 表达式需要 5 个字段（分 时 日 月 周）".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Redacted view of the config for the API: secrets replaced with a placeholder
/// so the browser never receives them, and so a screenshot of the settings page
/// cannot leak a token.
#[derive(Debug, Serialize)]
pub struct PublicConfig {
    pub iyuu: PublicIyuu,
    pub downloader: PublicDownloader,
    pub reseed: ReseedConfig,
    pub credentials: HashMap<String, PublicCredentials>,
    pub schedule: ScheduleConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Serialize)]
pub struct PublicIyuu {
    /// `true` when a token is stored; the value itself is never sent.
    pub has_token: bool,
    pub token_hint: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PublicDownloader {
    pub kind: String,
    pub name: String,
    pub base_url: String,
    pub username: String,
    pub has_password: bool,
    pub extra: DownloaderExtra,
}

#[derive(Debug, Serialize)]
pub struct PublicCredentials {
    pub has_passkey: bool,
    pub uid: Option<String>,
    pub has_cookie: bool,
    pub user_agent: Option<String>,
    pub has_api_key: bool,
    pub has_rsskey: bool,
}

impl AppConfig {
    pub fn public_view(&self) -> PublicConfig {
        // Show the last 4 characters only: enough to tell two tokens apart,
        // not enough to use.
        let hint = if self.iyuu.token.len() > 8 {
            format!("…{}", &self.iyuu.token[self.iyuu.token.len() - 4..])
        } else if self.iyuu.token.is_empty() {
            String::new()
        } else {
            "…".into()
        };

        PublicConfig {
            iyuu: PublicIyuu {
                has_token: !self.iyuu.token.is_empty(),
                token_hint: hint,
                base_url: self.iyuu.base_url.clone(),
            },
            downloader: PublicDownloader {
                kind: self.downloader.kind.clone(),
                name: self.downloader.name.clone(),
                base_url: self.downloader.base_url.clone(),
                username: self.downloader.username.clone(),
                has_password: !self.downloader.password.is_empty(),
                extra: self.downloader.extra.clone(),
            },
            reseed: self.reseed.clone(),
            credentials: self
                .credentials
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        PublicCredentials {
                            has_passkey: v
                                .passkey
                                .as_deref()
                                .map(|s| !s.is_empty())
                                .unwrap_or(false),
                            uid: v.uid.clone(),
                            has_cookie: v.cookie.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
                            user_agent: v.user_agent.clone(),
                            has_api_key: v
                                .api_key
                                .as_deref()
                                .map(|s| !s.is_empty())
                                .unwrap_or(false),
                            has_rsskey: v.rsskey.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
                        },
                    )
                })
                .collect(),
            schedule: self.schedule.clone(),
            ui: self.ui.clone(),
        }
    }
}

/// Patch semantics for the settings endpoint: absent fields keep their current
/// value, which is what a settings form needs (send only what changed).
#[derive(Debug, Deserialize)]
pub struct ConfigPatch {
    pub iyuu: Option<IyuuPatch>,
    pub downloader: Option<DownloaderPatch>,
    pub reseed: Option<ReseedConfig>,
    pub schedule: Option<ScheduleConfig>,
    pub ui: Option<UiConfig>,
    /// Per-site credentials. `null` value deletes the entry.
    pub credentials: Option<HashMap<String, Option<SiteCredentials>>>,
}

#[derive(Debug, Deserialize)]
pub struct IyuuPatch {
    /// Send an empty string to clear; omit to keep.
    pub token: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DownloaderPatch {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub extra: Option<DownloaderExtra>,
}

impl AppConfig {
    pub fn apply_patch(&mut self, patch: ConfigPatch) {
        if let Some(p) = patch.iyuu {
            if let Some(t) = p.token {
                // A blank token means "clear it", which is a legitimate action.
                self.iyuu.token = t;
            }
            if p.base_url.is_some() {
                self.iyuu.base_url = p.base_url.filter(|s| !s.trim().is_empty());
            }
        }
        if let Some(p) = patch.downloader {
            if let Some(v) = p.kind {
                self.downloader.kind = v;
            }
            if let Some(v) = p.name {
                self.downloader.name = v;
            }
            if let Some(v) = p.base_url {
                self.downloader.base_url = v;
            }
            if let Some(v) = p.username {
                self.downloader.username = v;
            }
            // Empty password on a patch means "unchanged": a form re-submit with
            // a blank password field must not wipe a working credential.
            if let Some(v) = p.password
                && !v.is_empty()
            {
                self.downloader.password = v;
            }
            if let Some(v) = p.extra {
                self.downloader.extra = v;
            }
        }
        if let Some(v) = patch.reseed {
            self.reseed = v;
        }
        if let Some(v) = patch.schedule {
            self.schedule = v;
        }
        if let Some(v) = patch.ui {
            self.ui = v;
        }
        if let Some(creds) = patch.credentials {
            for (sid, value) in creds {
                match value {
                    None => {
                        self.credentials.remove(&sid);
                    }
                    Some(c) => {
                        self.credentials.insert(sid, c);
                    }
                }
            }
        }
    }
}
