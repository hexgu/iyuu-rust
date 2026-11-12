//! Downloader abstraction.
//!
//! Both qBittorrent and Transmission adapters expose the same operations so the
//! reseed engine never branches on client type. The adapters are responsible for
//! all client-specific weirdness:
//!
//!  * qB 5.x renamed several `WebAPI` endpoints and changed `paused` to
//!    `stopped`, and silently dropped `skip_checking` from `torrents/add`.
//!  * Transmission's RPC needs a session-id handshake and returns every numeric
//!    field as an enum, not a bool.
//!
//! Getting these wrong produces the "added but never verified" behaviour we are
//! specifically designing against, so the adapters are careful about semantics
//! rather than just "does the call return 200".

use crate::error::Result;
use crate::models::{DownloaderKind, TorrentInfo};
use async_trait::async_trait;

pub mod qbittorrent;
pub mod transmission;

pub use qbittorrent::QBittorrentClient;
pub use transmission::TransmissionClient;

/// Options for adding a cross-seed torrent.
#[derive(Debug, Clone, Default)]
pub struct AddTorrentOptions {
    /// Where the client should look for existing data. Always the source
    /// torrent's save path, so verification reuses bytes already on disk.
    pub save_path: Option<String>,
    /// Category to set. `None` leaves it untouched.
    pub category: Option<String>,
    /// Tags to attach — we always tag with our marker so we can find our own
    /// work later and never re-add a duplicate.
    pub tags: Vec<String>,
    /// Add paused. Always true: verification must be driven explicitly so we can
    /// observe its outcome instead of racing it.
    pub paused: bool,
    /// qB-only: ask the client to skip the hash check. Deliberately *not*
    /// exposed as a user toggle. Skipping verification on a mismatched path
    /// makes qB re-download the whole payload from 0% and burn ratio, and hides
    /// exactly the failure the user needs to see.
    pub skip_checking: bool,
    /// Force the save path to win over the client's automatic torrent management.
    pub force_reuse_save_path: bool,
}

#[async_trait]
pub trait Downloader: Send + Sync {
    fn kind(&self) -> DownloaderKind;

    /// A short operator-facing label, e.g. the configured name.
    fn name(&self) -> &str;

    /// Cheap liveness + auth check. `Ok(version)` on success.
    async fn version(&self) -> Result<String>;

    /// Every torrent, normalized. Includes incomplete ones — the caller filters.
    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>>;

    /// Fetch specific torrents by hash. Missing hashes are simply absent from
    /// the result (clients disagree on whether that is an error).
    async fn torrents_by_hash(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>>;

    /// Fetch a single torrent; `None` when it does not exist.
    async fn torrent(&self, hash: &str) -> Result<Option<TorrentInfo>> {
        Ok(self
            .torrents_by_hash(&[hash.to_string()])
            .await?
            .into_iter()
            .next())
    }

    /// Add a .torrent, returning the resulting info-hash.
    ///
    /// Implementations must return the hash the *client* computed, not one we
    /// guessed: a malformed or v2-only torrent can hash differently than our
    /// parse, and using the wrong hash makes every later lookup fail.
    async fn add_torrent(&self, torrent_bytes: &[u8], opts: &AddTorrentOptions) -> Result<String>;

    /// Ask the client to hash-check. Returns the hashes actually accepted.
    async fn recheck(&self, hashes: &[String]) -> Result<()>;

    /// Start (resume) torrents.
    async fn start(&self, hashes: &[String]) -> Result<()>;

    /// Stop (pause) torrents.
    async fn stop(&self, hashes: &[String]) -> Result<()>;

    /// Set category.
    async fn set_category(&self, hashes: &[String], category: &str) -> Result<()>;

    /// Add tags. No-op on clients without tags.
    async fn add_tags(&self, hashes: &[String], tags: &[String]) -> Result<()>;

    /// Disable automatic torrent management for the given hashes so the save
    /// path we chose is not overridden by a category rule.
    async fn disable_auto_management(&self, hashes: &[String]) -> Result<()>;

    /// Delete torrents, optionally with data. Not used by the reseed flow itself
    /// (it is destructive) but needed for maintenance endpoints.
    async fn delete(&self, hashes: &[String], delete_files: bool) -> Result<()>;
}

/// Build a downloader from persisted settings.
///
/// # Errors
/// Returns an error when the configured client cannot be constructed or fails
/// to authenticate; the message names the client so the UI can point at the
/// right settings section.
pub async fn build(
    kind: DownloaderKind,
    name: &str,
    base_url: &str,
    username: &str,
    password: &str,
    extra: &DownloaderExtra,
) -> Result<std::sync::Arc<dyn Downloader>> {
    match kind {
        DownloaderKind::Qbittorrent => Ok(std::sync::Arc::new(
            QBittorrentClient::new(name, base_url, username, password).await?,
        )),
        DownloaderKind::Transmission => Ok(std::sync::Arc::new(
            TransmissionClient::new(
                name,
                base_url,
                username,
                password,
                extra.transmission_rpc_path.clone(),
            )
            .await?,
        )),
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DownloaderExtra {
    /// Transmission RPC endpoint path, default `/transmission/rpc`.
    #[serde(default)]
    pub transmission_rpc_path: Option<String>,
}
