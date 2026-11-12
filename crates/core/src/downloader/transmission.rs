//! Transmission RPC adapter.
//!
//! Transmission is friendlier than qB in one important way: torrents added
//! paused still hash-check on add (it verifies in place), so a cross-seed is
//! either immediately complete or immediately wrong. It is *less* friendly in
//! protocol terms:
//!
//!  * the `X-Transmission-Session-Id` handshake must be repeated whenever the
//!    daemon restarts (it answers 409 with a new id),
//!  * `torrent-get` is a field-projection API — you only get the fields you ask
//!    for, and unknown fields are a hard error, so the field list must be exact,
//!  * `status` is a bitfield enum, not a boolean; `isFinished` is not the same as
//!    "verified complete".

use crate::bencode;
use crate::downloader::{AddTorrentOptions, Downloader};
use crate::error::{CoreError, Result};
use crate::models::{DownloaderKind, TorrentInfo};
use async_trait::async_trait;
use base64::Engine;
use chrono::{TimeZone, Utc};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// The exact field set we request. Transmission errors on unknown names, so this
/// list is deliberately conservative and version-stable (4.x supports all of it).
const TORRENT_FIELDS: &[&str] = &[
    "id",
    "hashString",
    "name",
    "downloadDir",
    "totalSize",
    "percentDone",
    "status",
    "labels",
    "addedDate",
    "secondsSeeding",
    "uploadedEver",
    "uploadRatio",
    "error",
    "errorString",
    "trackers",
    "trackerStats",
];

pub struct TransmissionClient {
    http: reqwest::Client,
    url: String,
    name: String,
    auth_header: Option<String>,
    session_id: RwLock<Option<String>>,
    rpc_id: RwLock<i64>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    result: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct TrTorrent {
    #[serde(rename = "hashString", default)]
    hash_string: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "downloadDir", default)]
    download_dir: String,
    #[serde(rename = "totalSize", default)]
    total_size: i64,
    #[serde(rename = "percentDone", default)]
    percent_done: f64,
    #[serde(default)]
    status: i64,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(rename = "addedDate", default)]
    added_date: i64,
    #[serde(rename = "secondsSeeding", default)]
    seconds_seeding: i64,
    #[serde(rename = "uploadedEver", default)]
    uploaded_ever: i64,
    #[serde(rename = "uploadRatio", default)]
    upload_ratio: f64,
    #[serde(default)]
    error: i64,
    #[serde(rename = "errorString", default)]
    error_string: String,
    #[serde(default)]
    trackers: Vec<TrTracker>,
}

#[derive(Debug, Deserialize)]
struct TrTracker {
    #[serde(default)]
    announce: String,
}

/// Transmission `tr_torrent_status` values we care about.
mod status {
    pub const STOPPED: i64 = 0;
    pub const CHECK_WAIT: i64 = 1;
    pub const CHECK: i64 = 2;
    pub const DOWNLOAD_WAIT: i64 = 3;
    pub const DOWNLOAD: i64 = 4;
    pub const SEED_WAIT: i64 = 5;
    pub const SEED: i64 = 6;
}

impl TransmissionClient {
    pub async fn new(
        name: &str,
        base_url: &str,
        username: &str,
        password: &str,
        rpc_path: Option<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("iyuu-reseed/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| CoreError::Downloader {
                client: name.into(),
                msg: format!("构造 HTTP 客户端失败：{e}"),
            })?;

        let path = rpc_path.unwrap_or_else(|| "/transmission/rpc".to_string());
        let base = base_url.trim_end_matches('/');
        // Accept either a bare host:port or a full RPC URL.
        let url = if base.ends_with("/transmission/rpc") {
            base.to_string()
        } else {
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            format!("{base}{path}")
        };

        let auth_header = if username.is_empty() && password.is_empty() {
            None
        } else {
            Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            ))
        };

        let client = Self {
            http,
            url,
            name: name.to_string(),
            auth_header,
            session_id: RwLock::new(None),
            rpc_id: RwLock::new(1),
        };
        // Probe once so misconfiguration surfaces at save time, not mid-run.
        client.rpc("session-get", json!({})).await?;
        Ok(client)
    }

    /// Perform an RPC call, handling the session-id handshake transparently.
    async fn rpc(&self, method: &str, arguments: Value) -> Result<Value> {
        let body = {
            let mut id = self.rpc_id.write();
            *id += 1;
            json!({ "method": method, "arguments": arguments, "tag": *id })
        };

        // Two attempts: the first may 409 with a new session id.
        for attempt in 0..2 {
            let mut req = self.http.post(&self.url).json(&body);
            if let Some(auth) = &self.auth_header {
                req = req.header("Authorization", auth);
            }
            if let Some(sid) = self.session_id.read().clone() {
                req = req.header("X-Transmission-Session-Id", sid);
            }

            let resp = req.send().await.map_err(|e| CoreError::DownloaderAuth {
                client: self.name.clone(),
                msg: format!("无法连接 Transmission：{e}"),
            })?;

            let status = resp.status();
            if status == reqwest::StatusCode::CONFLICT {
                // Handshake: the daemon hands us the id it wants. This happens on
                // first contact and after every daemon restart.
                let sid = resp
                    .headers()
                    .get("X-Transmission-Session-Id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                match sid {
                    Some(sid) if attempt == 0 => {
                        *self.session_id.write() = Some(sid);
                        continue;
                    }
                    _ => {
                        return Err(CoreError::Downloader {
                            client: self.name.clone(),
                            msg: "Transmission 会话握手失败（未返回 session-id）".into(),
                        });
                    }
                }
            }

            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(CoreError::DownloaderAuth {
                    client: self.name.clone(),
                    msg: "用户名或密码错误（HTTP 401）".into(),
                });
            }

            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(CoreError::Downloader {
                    client: self.name.clone(),
                    msg: format!("RPC {method} HTTP {status}：{}", text.trim()),
                });
            }

            let parsed: RpcResponse = resp.json().await.map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("RPC {method} 响应解析失败：{e}"),
            })?;

            if parsed.result != "success" {
                // A wrong session id also surfaces here in some builds.
                if parsed.result.to_ascii_lowercase().contains("session") && attempt == 0 {
                    *self.session_id.write() = None;
                    continue;
                }
                return Err(CoreError::Downloader {
                    client: self.name.clone(),
                    msg: format!("RPC {method} 失败：{}", parsed.result),
                });
            }
            return Ok(parsed.arguments);
        }

        Err(CoreError::Downloader {
            client: self.name.clone(),
            msg: "Transmission RPC 会话协商失败".into(),
        })
    }

    fn map_torrent(t: TrTorrent) -> TorrentInfo {
        // Translate the status bitfield into words our phase() understands,
        // keeping the numeric code for the UI.
        let state = match t.status {
            status::CHECK => "checking",
            status::CHECK_WAIT => "check_wait",
            status::DOWNLOAD => "downloading",
            status::DOWNLOAD_WAIT => "download_wait",
            status::SEED => "seeding",
            status::SEED_WAIT => "seed_wait",
            status::STOPPED => "stopped",
            _ => "unknown",
        };
        let error = if t.error != 0 || !t.error_string.is_empty() {
            Some(if t.error_string.is_empty() {
                format!("error code {}", t.error)
            } else {
                t.error_string
            })
        } else {
            None
        };
        TorrentInfo {
            hash: bencode::normalize_hash(&t.hash_string),
            name: t.name,
            save_path: t.download_dir,
            content_path: None,
            total_size: t.total_size,
            progress: t.percent_done,
            state: state.to_string(),
            kind: DownloaderKind::Transmission,
            category: t.labels.first().cloned(),
            tags: t.labels,
            added_on: if t.added_date > 0 {
                Utc.timestamp_opt(t.added_date, 0).single()
            } else {
                None
            },
            seeding_time: Some(t.seconds_seeding),
            uploaded: t.uploaded_ever,
            ratio: t.upload_ratio,
            error,
            trackers: t
                .trackers
                .into_iter()
                .filter_map(|t| host_of(&t.announce))
                .collect(),
        }
    }

    async fn get_torrents(&self, ids: Option<Value>) -> Result<Vec<TorrentInfo>> {
        let mut args = json!({ "fields": TORRENT_FIELDS });
        if let Some(ids) = ids {
            args["ids"] = ids;
        }
        let out = self.rpc("torrent-get", args).await?;

        // Transmission only includes the key when non-empty.
        let raw = out.get("torrents").cloned().unwrap_or_else(|| json!([]));
        let list: Vec<TrTorrent> =
            serde_json::from_value(raw).map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("解析种子列表失败：{e}"),
            })?;
        Ok(list.into_iter().map(Self::map_torrent).collect())
    }
}

#[async_trait]
impl Downloader for TransmissionClient {
    fn kind(&self) -> DownloaderKind {
        DownloaderKind::Transmission
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn version(&self) -> Result<String> {
        let args = self.rpc("session-get", json!({})).await?;
        Ok(args
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string())
    }

    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>> {
        self.get_torrents(None).await
    }

    async fn torrents_by_hash(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        // Transmission wants uppercase hex for `ids`; it is case-sensitive here
        // even though it echoes `hashString` in the case the torrent was added.
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        self.get_torrents(Some(json!(ids))).await
    }

    async fn add_torrent(&self, torrent_bytes: &[u8], opts: &AddTorrentOptions) -> Result<String> {
        let local_hash =
            bencode::info_hash_from_torrent(torrent_bytes).map_err(|e| CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("种子文件解析失败：{e}"),
            })?;
        let hex_hash = bencode::hash_to_hex(&local_hash);

        let mut args = json!({
            "metainfo": base64::engine::general_purpose::STANDARD.encode(torrent_bytes),
            // Always paused: we drive verification explicitly so the outcome is
            // observable. Transmission still hash-checks on add.
            "paused": opts.paused,
        });
        if let Some(sp) = opts.save_path.as_deref().filter(|s| !s.is_empty()) {
            args["download-dir"] = json!(sp);
        }
        if !opts.tags.is_empty() {
            args["labels"] = json!(opts.tags);
        }

        let out = self.rpc("torrent-add", args).await?;

        // Transmission answers with either torrent-added or torrent-duplicate.
        if let Some(dup) = out.get("torrent-duplicate") {
            let hash = dup
                .get("hashString")
                .and_then(|v| v.as_str())
                .map(bencode::normalize_hash)
                .unwrap_or(hex_hash);
            return Err(CoreError::Downloader {
                client: self.name.clone(),
                msg: format!("种子已存在：{hash}"),
            });
        }

        let added = out
            .get("torrent-added")
            .ok_or_else(|| CoreError::Downloader {
                client: self.name.clone(),
                msg: "响应中缺少 torrent-added".into(),
            })?;

        // Prefer the client's own hash: a hybrid torrent's v1 hash can differ
        // from what our local parse produced if the file is v2-primary.
        let hash = added
            .get("hashString")
            .and_then(|v| v.as_str())
            .map(bencode::normalize_hash)
            .unwrap_or(hex_hash);
        Ok(hash)
    }

    async fn recheck(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        self.rpc("torrent-verify", json!({ "ids": ids })).await?;
        Ok(())
    }

    async fn start(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        self.rpc("torrent-start", json!({ "ids": ids })).await?;
        Ok(())
    }

    async fn stop(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        self.rpc("torrent-stop", json!({ "ids": ids })).await?;
        Ok(())
    }

    async fn set_category(&self, hashes: &[String], category: &str) -> Result<()> {
        // Transmission has no categories; labels are the closest analogue and we
        // add rather than replace so we never drop the user's own labels.
        self.add_tags(hashes, &[category.to_string()]).await
    }

    async fn add_tags(&self, hashes: &[String], tags: &[String]) -> Result<()> {
        if hashes.is_empty() || tags.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        for tag in tags {
            self.rpc("torrent-set", json!({ "ids": ids, "labels": [tag] }))
                .await?;
        }
        Ok(())
    }

    async fn disable_auto_management(&self, _hashes: &[String]) -> Result<()> {
        // Transmission has no automatic torrent management; the download-dir we
        // pass on add is always honoured.
        Ok(())
    }

    async fn delete(&self, hashes: &[String], delete_files: bool) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
        self.rpc(
            "torrent-remove",
            json!({ "ids": ids, "delete-local-data": delete_files }),
        )
        .await?;
        Ok(())
    }
}

fn host_of(announce: &str) -> Option<String> {
    let after_scheme = announce.split("://").nth(1)?;
    let host = after_scheme.split('/').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tracker_host() {
        assert_eq!(
            host_of("https://tracker.springsunday.net/announce?passkey=x").as_deref(),
            Some("tracker.springsunday.net")
        );
        assert_eq!(
            host_of("udp://open.tracker.org:1337/announce").as_deref(),
            Some("open.tracker.org")
        );
        assert!(host_of("").is_none());
    }

    #[test]
    fn maps_status_to_phase() {
        let mk = |status: i64, pct: f64| TrTorrent {
            hash_string: "A".repeat(40),
            name: "n".into(),
            download_dir: "/d".into(),
            total_size: 10,
            percent_done: pct,
            status,
            labels: vec![],
            added_date: 0,
            seconds_seeding: 0,
            uploaded_ever: 0,
            upload_ratio: 0.0,
            error: 0,
            error_string: String::new(),
            trackers: vec![],
        };

        // CHECK must map to Checking regardless of percentDone.
        let checking = TransmissionClient::map_torrent(mk(status::CHECK, 1.0));
        assert_eq!(
            checking.phase(),
            crate::models::TorrentPhase::Checking,
            "a torrent being hash-checked must never be treated as done"
        );

        let seeding = TransmissionClient::map_torrent(mk(status::SEED, 1.0));
        assert!(seeding.is_verified_complete());

        let stopped_partial = TransmissionClient::map_torrent(mk(status::STOPPED, 0.0));
        assert!(!stopped_partial.is_verified_complete());
    }

    #[test]
    fn lowercase_hash_is_normalized() {
        let t = TransmissionClient::map_torrent(TrTorrent {
            hash_string: "ABCDEF0123456789ABCDEF0123456789ABCDEF01".into(),
            name: "n".into(),
            download_dir: "/d".into(),
            total_size: 1,
            percent_done: 1.0,
            status: status::SEED,
            labels: vec![],
            added_date: 0,
            seconds_seeding: 0,
            uploaded_ever: 0,
            upload_ratio: 0.0,
            error: 0,
            error_string: String::new(),
            trackers: vec![],
        });
        assert_eq!(t.hash, "abcdef0123456789abcdef0123456789abcdef01");
    }
}
