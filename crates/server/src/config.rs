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
use iyuu_core::ratelimit::ApiTier;
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
    pub cleanup: CleanupConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IyuuConfig {
    #[serde(default)]
    pub token: String,
    /// Which membership the account holds. Decides both the host (free and SVIP
    /// are served by different domains) and the daily quota the engine budgets
    /// against.
    ///
    /// `None` means the user has not answered yet — the UI forces the choice when
    /// a token is first saved, because defaulting silently is how a paying user
    /// ends up on the free host wondering why their quota is 500.
    #[serde(default)]
    pub tier: Option<String>,
    /// Obsolete: the base URL now comes from `IYUU_API_BASE` (see
    /// [`crate::env_config`]). Kept only so an existing config file with this
    /// field still deserializes instead of erroring out; it is dropped on the next
    /// save. Never read — if it were, a stale value could override the
    /// environment, which is precisely the ambiguity this change removes.
    #[serde(default, skip_serializing, alias = "base_url")]
    #[expect(
        dead_code,
        reason = "deserialization-only tombstone for the removed base_url field"
    )]
    pub legacy_base_url: Option<String>,
}

impl IyuuConfig {
    /// Parsed membership tier. Missing → `Free`: an unanswered account must not
    /// be assumed to have a paying one's budget.
    #[must_use]
    pub fn tier(&self) -> ApiTier {
        self.tier.as_deref().map_or(ApiTier::Free, ApiTier::parse)
    }

    /// Whether the user has actually answered the tier question.
    #[must_use]
    pub fn tier_chosen(&self) -> bool {
        self.tier.is_some()
    }

    /// Host to actually call: the environment override if set, otherwise the host
    /// belonging to the tier.
    ///
    /// The override lives in the environment rather than this config on purpose —
    /// it is a deployment concern (mirror, reverse proxy), it must not be a
    /// user-facing field, and it must never be echoed back to the browser.
    #[must_use]
    pub fn effective_base_url(&self) -> &str {
        Self::base_url_for(self.tier())
    }

    /// Host for an explicitly chosen tier.
    ///
    /// An associated function, not a method, because it reads no config: callers
    /// sometimes need the host for a tier that is not saved yet (the setup dialog
    /// tests a token against the tier the user just picked). The environment
    /// override always wins — an operator running a mirror must have *every*
    /// request go through it, including one-off probes.
    #[must_use]
    pub fn base_url_for(tier: ApiTier) -> &'static str {
        crate::env_config::api_base().map_or_else(|| tier.base_url(), |base| base)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloaderConfig {
    #[serde(default = "default_downloader_kind")]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_qb_url")]
    pub base_url: String,
    /// Which credential the client should present.
    ///
    /// qBittorrent 5.2 added stateless API keys, which survive a `WebUI` restart
    /// that invalidates a `SID` cookie session. Stored as an explicit mode rather
    /// than inferred from "is `api_key` non-empty", because a leftover key from a
    /// previous attempt must not silently override a password that currently
    /// works — and vice versa when the key is rotated.
    #[serde(default = "default_downloader_auth")]
    pub auth: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// qBittorrent API key (`qbt_…`, 32 chars). Only used when `auth` is
    /// `api_key`; Transmission has no equivalent and never reads it.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub extra: DownloaderExtra,
}

fn default_downloader_kind() -> String {
    "qbittorrent".into()
}
fn default_qb_url() -> String {
    "http://127.0.0.1:8080".into()
}
fn default_downloader_auth() -> String {
    DownloaderAuthMode::Password.as_str().to_string()
}

/// The two ways to authenticate against a downloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloaderAuthMode {
    Password,
    ApiKey,
}

impl DownloaderAuthMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::ApiKey => "api_key",
        }
    }

    /// Parse a stored/patched value. Unknown strings are `None` so the caller can
    /// reject them instead of silently falling back to password auth — a typo
    /// would otherwise look like "the key is ignored" with no explanation.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "password" | "basic" => Some(Self::Password),
            "api_key" | "apikey" | "key" => Some(Self::ApiKey),
            _ => None,
        }
    }
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            kind: default_downloader_kind(),
            name: "qBittorrent".into(),
            base_url: default_qb_url(),
            auth: default_downloader_auth(),
            username: String::new(),
            password: String::new(),
            api_key: String::new(),
            extra: DownloaderExtra::default(),
        }
    }
}

impl DownloaderConfig {
    pub fn kind(&self) -> std::result::Result<DownloaderKind, ServerError> {
        DownloaderKind::parse(&self.kind)
            .ok_or_else(|| ServerError::Config(format!("未知的下载器类型：{}", self.kind)))
    }

    /// Effective authentication mode. An unparsable stored value resolves to
    /// password auth so a corrupted config still boots; `validate` is what
    /// rejects a bad *patch*.
    #[must_use]
    pub fn auth_mode(&self) -> DownloaderAuthMode {
        DownloaderAuthMode::parse(&self.auth).unwrap_or(DownloaderAuthMode::Password)
    }

    /// The API key to hand the client, and only when that is the chosen mode.
    #[must_use]
    pub fn effective_api_key(&self) -> Option<&str> {
        (self.auth_mode() == DownloaderAuthMode::ApiKey)
            .then(|| self.api_key.trim())
            .filter(|k| !k.is_empty())
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
    #[serde(default = "default_true")]
    pub reuse_category: bool,
    #[serde(default)]
    pub default_category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 其它辅种工具留下的标签，用来识别「这已经是别人的副本」。
    /// 默认 `辅种`（`Hermes` / `AutoSeed` / 手工操作的常见写法）。
    #[serde(default = "default_aux_tags")]
    pub aux_tags: Vec<String>,
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
fn default_aux_tags() -> Vec<String> {
    vec!["辅种".to_string()]
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
            reuse_category: true,
            default_category: None,
            tags: Vec::new(),
            aux_tags: default_aux_tags(),
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
            reuse_category: c.reuse_category,
            default_category: c.default_category.clone(),
            tags: c.tags.clone(),
            aux_tags: c.aux_tags.clone(),
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

/// 自动清理：主种子被删掉之后，把它的辅种副本（和文件）一起收掉。
///
/// 默认关闭。这是一条不可逆的删除路径：删掉的是别的站点上的做种记录，有些站点的
/// H&R 还没跑完。所以它必须显式打开；打开之后不做延迟观察——确认主种子缺失且有
/// 证据的组，下一次任务就清。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 删种子时是否连文件一起删。用户手动删主种子时如果留下了文件，这里是唯一会
    /// 把那份数据收掉的地方。
    #[serde(default = "default_true")]
    pub delete_files: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            delete_files: true,
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
        if let Some(tier) = self.iyuu.tier.as_deref() {
            // Rejecting beats guessing: silently treating "vpi" as free would
            // hand a paying user the free host with no explanation.
            let normalized = tier.trim().to_ascii_lowercase();
            if !matches!(
                normalized.as_str(),
                "free" | "vip" | "svip" | "member" | "premium"
            ) {
                return Err(ServerError::Config(format!(
                    "未知的会员类型「{tier}」，可选 free / vip"
                )));
            }
        }
        // A token without an answered tier is a half-configured account: the
        // engine would silently assume `free` and a paying user would wonder why
        // their quota is 500. The UI collects both in one step for this reason.
        if !self.iyuu.token.trim().is_empty() && !self.iyuu.tier_chosen() {
            return Err(ServerError::Config(
                "请选择 IYUU 会员类型（普通用户 / SVIP）".into(),
            ));
        }
        let kind = self.downloader.kind()?;
        if self.downloader.base_url.trim().is_empty() {
            return Err(ServerError::Config("下载器地址不能为空".into()));
        }
        if self.downloader.auth_mode() == DownloaderAuthMode::ApiKey {
            // Say it now rather than letting the engine build a client with no
            // credential and fail on the first query with a 403.
            if self.downloader.api_key.trim().is_empty() {
                return Err(ServerError::Config(
                    "已选择 API Key 认证，但还没有填写密钥".into(),
                ));
            }
            if kind != DownloaderKind::Qbittorrent {
                return Err(ServerError::Config(
                    "API Key 认证仅 qBittorrent ≥ 5.2 支持，请改用用户名密码或切换下载器类型"
                        .into(),
                ));
            }
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
    pub cleanup: CleanupConfig,
    pub ui: UiConfig,
}

/// The IYUU part of the config as the browser sees it.
///
/// Note what is **not** here: the API host. It comes from the environment and is
/// never serialized to the client — a deployment detail like an internal mirror
/// hostname should not end up in a screenshot of the settings page. The UI shows
/// the tier, which is the thing a user can actually act on.
#[derive(Debug, Serialize)]
pub struct PublicIyuu {
    /// `true` when a token is stored; the value itself is never sent.
    pub has_token: bool,
    pub token_hint: String,
    /// `free` or `vip` — what the user declared.
    pub tier: String,
    /// `false` when the account predates the tier question or skipped it. The UI
    /// uses this to force the choice instead of guessing.
    pub tier_chosen: bool,
    /// Quota numbers for the declared tier, so the picker can show what the user
    /// is getting without hardcoding the plan table in the bundle.
    pub daily_requests: u32,
    pub daily_hashes: u32,
}

#[derive(Debug, Serialize)]
pub struct PublicDownloader {
    pub kind: String,
    pub name: String,
    pub base_url: String,
    /// `password` or `api_key` — the form needs it to render the right fields.
    pub auth: String,
    pub username: String,
    pub has_password: bool,
    /// Presence only. The key itself never leaves the server.
    pub has_api_key: bool,
    pub extra: DownloaderExtra,
}

// One `has_*` flag per stored secret so the UI can render "configured" badges
// without ever receiving the secret itself.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one presence flag per credential; collapsing them would leak nothing but lose per-field UI state"
)]
#[derive(Debug, Serialize)]
pub struct PublicCredentials {
    pub has_passkey: bool,
    /// Some sites (TGG, `HDCity`) take an authkey instead of a passkey.
    pub has_torrent_key: bool,
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
                tier: self.iyuu.tier().as_str().to_string(),
                tier_chosen: self.iyuu.tier_chosen(),
                daily_requests: self.iyuu.tier().daily_requests(),
                daily_hashes: self.iyuu.tier().daily_hashes(),
            },
            downloader: PublicDownloader {
                kind: self.downloader.kind.clone(),
                name: self.downloader.name.clone(),
                base_url: self.downloader.base_url.clone(),
                auth: self.downloader.auth_mode().as_str().to_string(),
                username: self.downloader.username.clone(),
                has_password: !self.downloader.password.is_empty(),
                has_api_key: !self.downloader.api_key.is_empty(),
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
                            has_passkey: v.passkey.as_deref().is_some_and(|s| !s.is_empty()),
                            has_torrent_key: v
                                .torrent_key
                                .as_deref()
                                .is_some_and(|s| !s.is_empty()),
                            uid: v.uid.clone(),
                            has_cookie: v.cookie.as_deref().is_some_and(|s| !s.is_empty()),
                            user_agent: v.user_agent.clone(),
                            has_api_key: v.api_key.as_deref().is_some_and(|s| !s.is_empty()),
                            has_rsskey: v.rsskey.as_deref().is_some_and(|s| !s.is_empty()),
                        },
                    )
                })
                .collect(),
            schedule: self.schedule.clone(),
            cleanup: self.cleanup.clone(),
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
    pub cleanup: Option<CleanupConfig>,
    pub ui: Option<UiConfig>,
    /// Per-site credentials. `null` value deletes the entry.
    pub credentials: Option<HashMap<String, Option<SiteCredentials>>>,
}

#[derive(Debug, Deserialize)]
pub struct IyuuPatch {
    /// Send an empty string to clear; omit to keep.
    pub token: Option<String>,
    /// `free` / `vip`. Anything else is rejected rather than coerced, so a typo
    /// cannot silently downgrade a paying account to the free host.
    pub tier: Option<String>,
}

impl IyuuPatch {
    /// Reject unknown values *before* they are normalized.
    ///
    /// This has to happen here rather than in `AppConfig::validate`: patching
    /// canonicalizes `tier` (`"SVIP"` → `"vip"`), and a typo like `"vpi"` would
    /// canonicalize to `"free"` — valid by the time `validate` runs, so the user
    /// would silently land on the free host with no error at all.
    fn validate(&self) -> Result<()> {
        if let Some(tier) = self.tier.as_deref() {
            let normalized = tier.trim().to_ascii_lowercase();
            if !matches!(
                normalized.as_str(),
                "free" | "vip" | "svip" | "member" | "premium"
            ) {
                return Err(ServerError::Config(format!(
                    "未知的会员类型「{tier}」，可选 free / vip"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct DownloaderPatch {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub auth: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
    pub extra: Option<DownloaderExtra>,
}

impl AppConfig {
    /// Validate the parts of a patch that normalization would otherwise hide.
    ///
    /// Called by the settings endpoint before `apply_patch`, because canonicalizing
    /// a typo turns it into a *different valid* value.
    pub fn validate_patch(patch: &ConfigPatch) -> Result<()> {
        if let Some(p) = patch.iyuu.as_ref() {
            p.validate()?;
        }
        if let Some(p) = patch.downloader.as_ref()
            && let Some(auth) = p.auth.as_deref()
            && DownloaderAuthMode::parse(auth).is_none()
        {
            return Err(ServerError::Config(format!(
                "未知的下载器认证方式「{auth}」，可选 password / api_key"
            )));
        }
        Ok(())
    }

    pub fn apply_patch(&mut self, patch: ConfigPatch) {
        if let Some(p) = patch.iyuu {
            if let Some(t) = p.token {
                // A blank token means "clear it", which is a legitimate action.
                self.iyuu.token = t;
            }
            if let Some(tier) = p.tier {
                // Store the canonical spelling so the config file never carries
                // "SVIP"/"vip "/"member" variants that later need normalizing.
                self.iyuu.tier = Some(ApiTier::parse(&tier).as_str().to_string());
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
            if let Some(v) = p.auth {
                // `validate_patch` already rejected anything unparsable.
                self.downloader.auth = DownloaderAuthMode::parse(&v)
                    .map_or_else(|| self.downloader.auth.clone(), |m| m.as_str().to_string());
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
            // The API key follows the same blank-means-unchanged rule: the form
            // renders the key as a masked write-only field, so an untouched field
            // posts blank and must not wipe a working key. Switching `auth` back
            // to `password` is what stops the key from being used.
            if let Some(v) = p.api_key.map(|k| k.trim().to_string())
                && !v.is_empty()
            {
                self.downloader.api_key = v;
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
        if let Some(v) = patch.cleanup {
            self.cleanup = v;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(token: &str, tier: Option<&str>) -> AppConfig {
        AppConfig {
            iyuu: IyuuConfig {
                token: token.into(),
                tier: tier.map(str::to_string),
                ..IyuuConfig::default()
            },
            ..AppConfig::default()
        }
    }

    #[test]
    fn token_without_tier_is_rejected() {
        // The whole point of asking: an unanswered tier would silently mean
        // "free", which is a wrong answer for a paying user and an unsafe one for
        // the quota budget.
        let cfg = cfg_with("token", None);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("会员类型"), "{err}");
    }

    #[test]
    fn token_with_tier_passes() {
        assert!(cfg_with("token", Some("free")).validate().is_ok());
        assert!(cfg_with("token", Some("vip")).validate().is_ok());
    }

    #[test]
    fn unknown_tier_is_rejected_by_patch_validation() {
        // Rejecting at the patch layer is what stops "vpi" from being normalized
        // into "free" — a valid value that means the opposite of the intent.
        let patch: ConfigPatch = serde_json::from_value(serde_json::json!({
            "iyuu": { "tier": "vpi" }
        }))
        .unwrap();
        let err = AppConfig::validate_patch(&patch).unwrap_err().to_string();
        assert!(err.contains("vpi"), "{err}");
    }

    #[test]
    fn tier_aliases_are_canonicalized() {
        for alias in ["SVIP", "vip", "member", "premium"] {
            let patch: ConfigPatch = serde_json::from_value(serde_json::json!({
                "iyuu": { "tier": alias }
            }))
            .unwrap();
            let mut cfg = cfg_with("token", None);
            cfg.apply_patch(patch);
            assert_eq!(cfg.iyuu.tier.as_deref(), Some("vip"), "alias {alias}");
        }
    }

    #[test]
    fn public_view_never_exposes_the_api_host() {
        // The host is a deployment detail (env var); leaking it into the config
        // response would put it in screenshots and in every browser's memory.
        // Scoped to the `iyuu` subtree on purpose — the *downloader* legitimately
        // has a `base_url`, and asserting globally would fail for the wrong reason.
        let cfg = cfg_with("token", Some("vip"));
        let view = serde_json::to_value(cfg.public_view()).unwrap();
        let iyuu = view.get("iyuu").expect("iyuu section");

        let serialized = iyuu.to_string();
        assert!(
            !serialized.contains("iyuu.cn"),
            "API host leaked into the iyuu section: {serialized}"
        );
        assert!(
            iyuu.get("base_url").is_none() && iyuu.get("effective_base_url").is_none(),
            "API host field leaked: {serialized}"
        );
    }

    #[test]
    fn public_view_reports_tier_choice() {
        let unchosen = cfg_with("token", None).public_view();
        assert!(!unchosen.iyuu.tier_chosen, "UI must be told to ask");
        // Still reports the effective tier, so quota math has a value to use.
        assert_eq!(unchosen.iyuu.tier, "free");

        let chosen = cfg_with("token", Some("vip")).public_view();
        assert!(chosen.iyuu.tier_chosen);
        assert_eq!(chosen.iyuu.daily_requests, 2500);
    }

    #[test]
    fn legacy_base_url_field_still_deserializes() {
        // An older config file carries `base_url`; it must load without error even
        // though the field no longer has any effect.
        let cfg: AppConfig = serde_json::from_value(serde_json::json!({
            "iyuu": { "token": "t", "tier": "free", "base_url": "https://old.example" }
        }))
        .unwrap();
        assert_eq!(cfg.iyuu.token, "t");
        // And it is dropped on the next save rather than resurrecting the field.
        let round_tripped = serde_json::to_value(&cfg).unwrap();
        let iyuu = round_tripped.get("iyuu").expect("iyuu section");
        assert!(
            iyuu.get("base_url").is_none() && iyuu.get("legacy_base_url").is_none(),
            "removed field came back: {iyuu}"
        );
    }

    #[test]
    fn api_key_mode_requires_a_key() {
        let mut cfg = cfg_with("token", Some("free"));
        cfg.downloader.auth = "api_key".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("API Key"), "{err}");

        cfg.downloader.api_key = "qbt_abc".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn api_key_mode_is_qbittorrent_only() {
        let mut cfg = cfg_with("token", Some("free"));
        cfg.downloader.kind = "transmission".into();
        cfg.downloader.auth = "api_key".into();
        cfg.downloader.api_key = "qbt_abc".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("qBittorrent"), "{err}");
    }

    #[test]
    fn unknown_auth_mode_is_rejected_by_patch_validation() {
        // Rejecting at the patch layer keeps a typo from being stored and then
        // silently ignored in favour of password auth.
        let patch: ConfigPatch = serde_json::from_value(serde_json::json!({
            "downloader": { "auth": "apikey-auth" }
        }))
        .unwrap();
        let err = AppConfig::validate_patch(&patch).unwrap_err().to_string();
        assert!(err.contains("apikey-auth"), "{err}");
    }

    #[test]
    fn switching_back_to_password_ignores_a_saved_key() {
        // A stale key must not keep being used after the user picks password auth,
        // which is the whole reason `auth` is stored instead of inferred.
        let mut cfg = cfg_with("token", Some("free"));
        cfg.downloader.api_key = "qbt_abc".into();
        cfg.downloader.auth = "password".into();
        assert_eq!(cfg.downloader.effective_api_key(), None);
        assert!(
            cfg.validate().is_ok(),
            "a saved key is not an error in password mode"
        );

        cfg.downloader.auth = "api_key".into();
        assert_eq!(cfg.downloader.effective_api_key(), Some("qbt_abc"));
    }

    #[test]
    fn blank_api_key_on_a_patch_keeps_the_saved_one() {
        let mut cfg = cfg_with("token", Some("free"));
        cfg.downloader.auth = "api_key".into();
        cfg.downloader.api_key = "qbt_saved".into();

        let patch: ConfigPatch = serde_json::from_value(serde_json::json!({
            "downloader": { "auth": "api_key", "api_key": "  " }
        }))
        .unwrap();
        cfg.apply_patch(patch);
        assert_eq!(cfg.downloader.api_key, "qbt_saved");
    }

    #[test]
    fn public_view_reports_auth_mode_without_the_key() {
        let mut cfg = cfg_with("token", Some("free"));
        cfg.downloader.auth = "api_key".into();
        cfg.downloader.api_key = "qbt_secret_value".into();

        let view = serde_json::to_value(cfg.public_view()).unwrap();
        let dl = view.get("downloader").expect("downloader section");
        assert_eq!(dl.get("auth").and_then(|v| v.as_str()), Some("api_key"));
        assert_eq!(
            dl.get("has_api_key").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert!(
            !dl.to_string().contains("qbt_secret_value"),
            "API key leaked into the public config: {dl}"
        );
    }

    #[test]
    fn legacy_config_without_auth_defaults_to_password() {
        // An existing install has no `auth` field; it must load as password mode
        // rather than failing to deserialize or silently switching to API keys.
        let cfg: AppConfig = serde_json::from_value(serde_json::json!({
            "iyuu": { "token": "t", "tier": "free" },
            "downloader": { "kind": "qbittorrent", "username": "u", "password": "p" }
        }))
        .unwrap();
        assert_eq!(cfg.downloader.auth_mode(), DownloaderAuthMode::Password);
        assert_eq!(cfg.downloader.effective_api_key(), None);
    }

    #[test]
    fn base_url_follows_the_tier_without_override() {
        // No environment override in the test process, so this is the tier's own
        // host. The override path is covered in `env_config`.
        let free = IyuuConfig::base_url_for(ApiTier::Free);
        let vip = IyuuConfig::base_url_for(ApiTier::Vip);
        assert_eq!(free, "https://2025.iyuu.cn");
        assert_eq!(vip, "https://vip.iyuu.cn");
        assert_ne!(free, vip, "the two tiers must not share a server");
    }
}
