//! Domain models shared by the engine, the downloader adapters, and the API.
//!
//! The state machine here is deliberately richer than what MoviePilot's plugin
//! does, because the failure modes seen in production were all state-machine
//! gaps:
//!
//!  * A torrent added with `paused` sits idle until something starts it.
//!  * A torrent added for verification moves through
//!    `checkingDL -> checkingResumeData -> pausedDL/queuedDL` (or `missingFiles`),
//!    and the *only* state that means "verification passed and data is intact"
//!    is a torrent whose `progress == 1.0` while it is not checking and not
//!    errored.
//!  * Treating "added" as "success" hides every verification failure, which is
//!    how you end up with hundreds of paused cross-seeds and no idea why.
//!
//! So we model an explicit lifecycle, persist it, and only ever report SUCCESS
//! after observing a healthy, verifying-complete torrent.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// IYUU API payloads
// ---------------------------------------------------------------------------

/// One entry of `GET /reseed/sites/index`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IyuuSite {
    pub id: i64,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub base_url: String,
    /// Download path template, e.g. `download.php?id={}&passkey={passkey}`.
    #[serde(default)]
    pub download_page: String,
    /// e.g. `details.php?id={}` — used for the details-page fallback.
    #[serde(default)]
    pub details_page: String,
    /// Which credential the site needs: `passkey`, `authkey`, `uid`, ...
    #[serde(default)]
    pub reseed_check: String,
    /// 0 = http only, 1 = https only, 2 = both.
    #[serde(default)]
    pub is_https: i64,
    #[serde(default)]
    pub cookie_required: i64,
    /// Sites that need a login cookie rather than / in addition to a passkey.
    #[serde(default)]
    pub bind_check: String,
}

impl IyuuSite {
    /// Host without scheme, e.g. `springsunday.net`.
    pub fn host(&self) -> &str {
        self.base_url.trim_start_matches("http://").trim_start_matches("https://")
    }

    /// Best display name available.
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.site
        } else {
            &self.nickname
        }
    }
}

/// A cross-seed candidate from `POST /reseed/index/index`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedCandidate {
    /// Site id, join key into the site list.
    pub sid: i64,
    pub torrent_id: i64,
    #[serde(default)]
    pub info_hash: String,
}

// ---------------------------------------------------------------------------
// Downloader snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloaderKind {
    Qbittorrent,
    Transmission,
}

impl DownloaderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
            Self::Transmission => "transmission",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "qbittorrent" | "qb" | "qbit" => Some(Self::Qbittorrent),
            "transmission" | "tr" => Some(Self::Transmission),
            _ => None,
        }
    }
}

/// A torrent as seen in a downloader, normalized across clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentInfo {
    /// lowercase hex
    pub hash: String,
    pub name: String,
    /// Content path (the file or folder the torrent seeds).
    pub save_path: String,
    /// Complete path incl. name, when the client exposes it.
    pub content_path: Option<String>,
    pub total_size: i64,
    /// 0.0..=1.0
    pub progress: f64,
    /// Raw client state string, kept verbatim for debugging.
    pub state: String,
    pub kind: DownloaderKind,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub added_on: Option<DateTime<Utc>>,
    /// Seconds seeding, when known.
    pub seeding_time: Option<i64>,
    pub uploaded: i64,
    pub ratio: f64,
    /// Error message when the client reports one.
    pub error: Option<String>,
    /// Tracker host list, when cheaply available.
    pub trackers: Vec<String>,
}

impl TorrentInfo {
    /// Client-agnostic interpretation of the raw state.
    pub fn phase(&self) -> TorrentPhase {
        let s = self.state.to_ascii_lowercase();
        match self.kind {
            DownloaderKind::Qbittorrent => match s.as_str() {
                // These all mean "verifying"; do NOT touch, do NOT report done.
                "checkingdl" | "checkingup" | "checkingresumedata" | "checking" => {
                    TorrentPhase::Checking
                }
                "metadl" => TorrentPhase::DownloadingMetadata,
                "missingfiles" => TorrentPhase::MissingFiles,
                "error" => TorrentPhase::Error,
                "uploading" | "stalledup" | "queuedup" | "forcedup" | "pausedup"
                | "stoppedup" => {
                    if self.progress >= 0.999 {
                        TorrentPhase::Complete
                    } else {
                        TorrentPhase::Incomplete
                    }
                }
                "downloading" | "stalleddl" | "queueddl" | "forceddl" | "pauseddl"
                | "stoppeddl" | "allocating" | "moving" => {
                    if self.progress >= 0.999 {
                        TorrentPhase::Complete
                    } else {
                        TorrentPhase::Incomplete
                    }
                }
                _ => TorrentPhase::Unknown,
            },
            DownloaderKind::Transmission => {
                // Transmission status codes are numeric but the field is a
                // bitfield; the adapter pre-decodes into words.
                if s.contains("check") {
                    TorrentPhase::Checking
                } else if s.contains("error") || s.contains("isolated") {
                    // Transmission reports a failed hash check as `isolated`,
                    // which is an error by any useful definition.
                    TorrentPhase::Error
                } else if self.progress >= 0.999 {
                    TorrentPhase::Complete
                } else {
                    TorrentPhase::Incomplete
                }
            }
        }
    }

    /// True when the data is verified intact and the torrent is seeding (or
    /// held complete). This is the ONLY condition under which a cross-seed
    /// counts as successful.
    pub fn is_verified_complete(&self) -> bool {
        matches!(self.phase(), TorrentPhase::Complete) && self.progress >= 0.999
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TorrentPhase {
    /// Being hash-checked right now. Hands off.
    Checking,
    /// Fully downloaded and verified.
    Complete,
    /// Added but data not complete.
    Incomplete,
    /// Client cannot find the payload files.
    MissingFiles,
    DownloadingMetadata,
    Error,
    Unknown,
}

// ---------------------------------------------------------------------------
// Cross-seed lifecycle
// ---------------------------------------------------------------------------

/// Where a cross-seed attempt stands.
///
/// `Added` is explicitly NOT success. A torrent can be added, then fail
/// verification, and end up in `VerifyFailed` — that distinction is the whole
/// point of this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReseedState {
    /// Candidate discovered, not yet acted on.
    Pending,
    /// Skipped before any network work (already present, cached, filtered out).
    Skipped,
    /// .torrent being fetched from the source site.
    Fetching,
    /// Handed to the downloader, awaiting verification.
    Added,
    /// Downloader is hash-checking. Waiting; retries must not fire.
    Verifying,
    /// Verification passed, data intact, seeding. THIS is success.
    Seeding,
    /// Verification finished but data is incomplete → wrong save path or wrong
    /// release. Terminal until a human intervenes; never auto-retried forever.
    VerifyFailed,
    /// Client lost the files after a successful seed.
    MissingFiles,
    /// Could not fetch the .torrent / the site refused.
    FetchFailed,
    /// Downloader rejected the add.
    AddFailed,
    /// Gave up after exhausting retries (verification never resolved).
    TimedOut,
}

impl ReseedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::Fetching => "fetching",
            Self::Added => "added",
            Self::Verifying => "verifying",
            Self::Seeding => "seeding",
            Self::VerifyFailed => "verify_failed",
            Self::MissingFiles => "missing_files",
            Self::FetchFailed => "fetch_failed",
            Self::AddFailed => "add_failed",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "skipped" => Self::Skipped,
            "fetching" => Self::Fetching,
            "added" => Self::Added,
            "verifying" => Self::Verifying,
            "seeding" => Self::Seeding,
            "verify_failed" => Self::VerifyFailed,
            "missing_files" => Self::MissingFiles,
            "fetch_failed" => Self::FetchFailed,
            "add_failed" => Self::AddFailed,
            "timed_out" => Self::TimedOut,
            _ => Self::Pending,
        }
    }

    /// Terminal states never get retried automatically.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Seeding
                | Self::VerifyFailed
                | Self::MissingFiles
                | Self::FetchFailed
                | Self::AddFailed
                | Self::TimedOut
                | Self::Skipped
        )
    }

    /// States the verification worker should keep polling.
    pub fn awaiting_verification(&self) -> bool {
        matches!(self, Self::Added | Self::Verifying)
    }
}

/// A single cross-seed attempt, persisted.
///
/// One row per (source_torrent_hash, target_site, target_info_hash). Persisting
/// this is what makes the queue survive a restart — the MoviePilot plugin keeps
/// it in a Python dict and loses every in-flight verification on restart, which
/// is why torrents get stranded in `paused` forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReseedAttempt {
    pub id: i64,
    /// Hash of the torrent ALREADY in the downloader (the data source).
    pub source_hash: String,
    pub source_name: String,
    /// Verbatim save path of the source torrent — reused so no bytes move.
    pub save_path: String,
    pub target_info_hash: String,
    pub target_sid: i64,
    pub target_site: String,
    pub target_torrent_id: i64,
    pub state: ReseedState,
    /// How many verification polls have happened.
    pub check_count: i32,
    pub last_checked_at: Option<DateTime<Utc>>,
    /// When the downloader was first asked to verify.
    pub verify_started_at: Option<DateTime<Utc>>,
    pub added_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
    pub downloader: String,
    pub run_id: Option<String>,
}

impl ReseedAttempt {
    /// Backoff for verification polling: fast at first (a small torrent verifies
    /// in seconds), then progressively slower so a huge one doesn't hammer the
    /// API. Capped so we still notice completion promptly.
    pub fn next_check_delay(&self) -> std::time::Duration {
        let secs = match self.check_count {
            0 => 5,
            1 => 10,
            2 => 20,
            3 => 30,
            4..=9 => 60,
            10..=19 => 120,
            _ => 300,
        };
        std::time::Duration::from_secs(secs)
    }

    /// Total verification budget. A 60 GB torrent on spinning rust can legitimately
    /// take an hour to check, so this is generous — but not infinite, because a
    /// stuck `checking` state must eventually surface as a failure instead of
    /// silently sitting in the queue forever.
    pub const VERIFY_DEADLINE_SECS: i64 = 3 * 3600;
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    Manual,
    Schedule,
    Api,
}

impl RunTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
            Self::Api => "api",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "schedule" => Self::Schedule,
            "api" => Self::Api,
            _ => Self::Manual,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Running,
        }
    }
}

/// Aggregated counters for a run. Note `seeding` vs `added`: we track both,
/// because reporting only "added" is exactly the misleading metric that made the
/// MoviePilot plugin's numbers useless.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStats {
    /// Source torrents scanned in the downloader.
    pub scanned: i64,
    /// Source torrents that passed local filters and were sent to IYUU.
    pub queried: i64,
    /// Candidates returned by IYUU.
    pub candidates: i64,
    /// Candidates that already existed in the downloader.
    pub existed: i64,
    /// Attempts where a .torrent was successfully fetched and added.
    pub added: i64,
    /// Attempts confirmed seeding after verification. The real success metric.
    pub seeding: i64,
    /// Verification completed but data was incomplete.
    pub verify_failed: i64,
    /// .torrent fetch failures.
    pub fetch_failed: i64,
    /// Downloader add failures.
    pub add_failed: i64,
    /// Anything else that went wrong.
    pub other_failed: i64,
    /// Skipped before network work.
    pub skipped: i64,
}

impl RunStats {
    pub fn merge(&mut self, other: &RunStats) {
        self.scanned += other.scanned;
        self.queried += other.queried;
        self.candidates += other.candidates;
        self.existed += other.existed;
        self.added += other.added;
        self.seeding += other.seeding;
        self.verify_failed += other.verify_failed;
        self.fetch_failed += other.fetch_failed;
        self.add_failed += other.add_failed;
        self.other_failed += other.other_failed;
        self.skipped += other.skipped;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub trigger: RunTrigger,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stats: RunStats,
    pub message: Option<String>,
}
