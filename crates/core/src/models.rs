//! Domain models shared by the engine, the downloader adapters, and the API.
//!
//! The state machine here is deliberately richer than what `MoviePilot`'s plugin
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
    /// e.g. `details.php?id={}` — used for the details-page fallback. Present in
    /// the API's example response but not in its parameter table; optional.
    #[serde(default)]
    pub details_page: String,
    /// Which credential the site needs: `passkey`, `authkey`, `uid`, ...
    #[serde(default)]
    pub reseed_check: String,
    /// 0 = http only, 1 = https only, 2 = both.
    #[serde(default)]
    pub is_https: i64,
    /// `1` = the site gates downloads behind a login cookie.
    #[serde(default)]
    pub cookie_required: i64,
}

impl IyuuSite {
    /// Host without scheme, e.g. `springsunday.net`.
    #[must_use]
    pub fn host(&self) -> &str {
        self.base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
    }

    /// Best display name available.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.site
        } else {
            &self.nickname
        }
    }
}

/// One entry of `GET /reseed/sites/recommend` — a **cooperative** site.
///
/// This is a different list from [`IyuuSite`], and confusing the two is the
/// mistake this type exists to prevent:
///
/// * [`IyuuSite`] (`/reseed/sites/index`) is the catalogue of sites IYUU can
///   cross-seed into, with the download-URL template needed to fetch a .torrent.
/// * `PartnerSite` is the shorter list of sites IYUU has a **commercial
///   partnership** with. Binding one of *your* accounts on such a site
///   (`POST /reseed/users/bind`) is what **activates the token** — without it
///   IYUU answers `403 用户未绑定推荐站点` and every other endpoint refuses.
///
/// So this list is an *activation menu*, not a reseed target list. It carries no
/// `base_url` and no `download_page`: the response simply does not include them,
/// so parsing it as [`IyuuSite`] yields dozens of entries with empty URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartnerSite {
    /// Same `sid` space as [`IyuuSite::id`].
    pub id: i64,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub nickname: String,
    /// Which fields the bind call needs, e.g. `passkey,uid`. Documented as
    /// deprecated upstream; kept because it is what tells the UI whether to ask
    /// for a uid at all.
    #[serde(default)]
    pub bind_check: String,
    /// Reseed credential hint. Documented as deprecated upstream.
    #[serde(default)]
    pub reseed_check: String,
}

impl PartnerSite {
    /// Display name, falling back to the slug.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.nickname.is_empty() {
            &self.site
        } else {
            &self.nickname
        }
    }

    /// Whether the bind call needs the site's numeric user id.
    ///
    /// `bind_check` is a comma-separated list (`passkey,uid`). Absent or
    /// unparsable → assume it is needed, because the bind endpoint documents
    /// `id` as required and a missing uid is a rejected call, not a worse one.
    #[must_use]
    pub fn needs_uid(&self) -> bool {
        if self.bind_check.trim().is_empty() {
            return true;
        }
        self.bind_check
            .split(',')
            .any(|f| f.trim().eq_ignore_ascii_case("uid"))
    }
}

/// A cross-seed candidate from `POST /reseed/index/index`.
///
/// `sid` and `torrent_id` are documented as integers, and the example response
/// shows integers — but in practice the server passes through whatever the
/// per-site spider stored, and some sites yield **numeric strings**
/// (`"torrent_id": "631188"`). Deserializing those as plain `i64` fails the whole
/// batch with `invalid type: string "631188", expected i64`, throwing away up to
/// 200 hashes of good answers because one site's row was stringly typed.
///
/// So both fields accept either form. The lenience lives in the type, not at the
/// call site, because every consumer only ever wants the number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedCandidate {
    /// Site id, join key into the site list.
    #[serde(deserialize_with = "de_i64_lenient")]
    pub sid: i64,
    #[serde(deserialize_with = "de_i64_lenient")]
    pub torrent_id: i64,
    #[serde(default)]
    pub info_hash: String,
}

/// Deserialize an integer that may arrive as a JSON number or a numeric string.
///
/// Also accepts a float that is exactly integral (`74215.0`), which some PHP
/// `json_encode` paths produce for values that overflowed an int column. Rejects
/// anything else loudly rather than silently defaulting to 0 — a zero id would
/// build a nonsense download URL and fail at the site instead of here.
fn de_i64_lenient<'de, D>(de: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(i64),
        /// `u64` covers ids above `i64::MAX`, which would otherwise be reported as
        /// a type error rather than a range one.
        UInt(u64),
        Float(f64),
        Str(String),
    }

    match Raw::deserialize(de)? {
        Raw::Int(v) => Ok(v),
        Raw::UInt(v) => {
            i64::try_from(v).map_err(|_| D::Error::custom(format!("站点 ID 超出范围：{v}")))
        }
        Raw::Float(v) => {
            // `f64 -> i64` is only meaningful when the value is integral and in
            // range; Rust's `as` cast saturates silently, which would turn a
            // malformed id into a plausible-looking wrong one.
            // Compare against the f64 representation of the *exact* bounds rather
            // than casting `i64::MIN`/`MAX` to f64: `i64::MAX as f64` rounds up to
            // 2^63, so the naive comparison would accept an out-of-range value.
            // `-2^63` is exactly representable, `2^63` is the first float above it.
            if v.fract() != 0.0
                || v < -9_223_372_036_854_775_808.0
                || v >= 9_223_372_036_854_775_808.0
            {
                return Err(D::Error::custom(format!("站点 ID 不是整数：{v}")));
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "range and integrality were checked immediately above"
            )]
            Ok(v as i64)
        }
        Raw::Str(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| D::Error::custom(format!("站点 ID 无法解析为整数：「{s}」"))),
    }
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
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qbittorrent => "qbittorrent",
            Self::Transmission => "transmission",
        }
    }

    #[must_use]
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
    /// Total bytes downloaded for this torrent, all-time.
    ///
    /// The main seed is the one the user actually pulled here; a cross-seed is
    /// added on top of files that already exist and downloads nothing. So
    /// `downloaded > 0` is direct evidence of "this is the original", and it is
    /// the client's own accounting rather than a tag someone typed.
    pub downloaded: i64,
    pub ratio: f64,
    /// Error message when the client reports one.
    pub error: Option<String>,
    /// Tracker host list, when cheaply available.
    pub trackers: Vec<String>,
}

impl TorrentInfo {
    /// Client-agnostic interpretation of the raw state.
    #[must_use]
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
                // Uploading, queued, paused and downloading states all mean the
                // same thing to us: done or not done, judged by progress alone.
                "uploading" | "stalledup" | "queuedup" | "forcedup" | "pausedup" | "stoppedup"
                | "downloading" | "stalleddl" | "queueddl" | "forceddl" | "pauseddl"
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
    #[must_use]
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
    #[must_use]
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

    /// Parse the persisted string form. Anything unrecognised falls back to
    /// `Pending`, which re-examines the torrent on the next sweep rather than
    /// dropping the attempt on the floor.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            // Spelled out rather than folded into the wildcard: listing it keeps
            // the mapping symmetric with `as_str`.
            #[expect(
                clippy::match_same_arms,
                reason = "explicit round-trip with as_str beats a shorter wildcard"
            )]
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
    #[must_use]
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
    #[must_use]
    pub fn awaiting_verification(&self) -> bool {
        matches!(self, Self::Added | Self::Verifying)
    }
}

/// A single cross-seed attempt, persisted.
///
/// One row per (`source_torrent_hash`, `target_site`, `target_info_hash`). Persisting
/// this is what makes the queue survive a restart — the `MoviePilot` plugin keeps
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
    #[must_use]
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
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
            Self::Api => "api",
        }
    }

    #[must_use]
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
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
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
/// `MoviePilot` plugin's numbers useless.
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

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------- candidate field leniency

    #[test]
    fn torrent_id_as_a_numeric_string_is_accepted() {
        // The exact shape that broke a real run:
        //   响应数据结构不符：invalid type: string "631188", expected i64
        // Some sites' spider rows carry the id as a string, and the server passes
        // it through verbatim. Deserializing strictly cost the whole 200-hash
        // batch, so this must parse.
        let json = r#"{"sid":7,"torrent_id":"631188","info_hash":"abc"}"#;
        let c: SeedCandidate = serde_json::from_str(json).unwrap();
        assert_eq!(c.sid, 7);
        assert_eq!(c.torrent_id, 631_188);
        assert_eq!(c.info_hash, "abc");
    }

    #[test]
    fn sid_as_a_numeric_string_is_accepted() {
        // Same class of problem on the other field, and the one that would
        // otherwise fail *after* a fix to `torrent_id` alone.
        let json = r#"{"sid":"7","torrent_id":74215,"info_hash":"abc"}"#;
        let c: SeedCandidate = serde_json::from_str(json).unwrap();
        assert_eq!(c.sid, 7);
        assert_eq!(c.torrent_id, 74_215);
    }

    #[test]
    fn both_fields_as_strings_and_surrounding_space_are_accepted() {
        let json = r#"{"sid":" 7 ","torrent_id":" 74215 ","info_hash":""}"#;
        let c: SeedCandidate = serde_json::from_str(json).unwrap();
        assert_eq!((c.sid, c.torrent_id), (7, 74_215));
    }

    #[test]
    fn plain_integers_still_parse() {
        // The documented shape must keep working — leniency is additive.
        let json = r#"{"sid":32,"torrent_id":19537,"info_hash":"x"}"#;
        let c: SeedCandidate = serde_json::from_str(json).unwrap();
        assert_eq!((c.sid, c.torrent_id), (32, 19_537));
    }

    #[test]
    fn an_integral_float_is_accepted() {
        // A PHP json_encode of an oversized int column can surface as 74215.0.
        let json = r#"{"sid":7.0,"torrent_id":74215.0,"info_hash":"x"}"#;
        let c: SeedCandidate = serde_json::from_str(json).unwrap();
        assert_eq!((c.sid, c.torrent_id), (7, 74_215));
    }

    #[test]
    fn a_non_integral_float_is_rejected() {
        // Silently truncating 7.5 to 7 would build a download URL for the wrong
        // torrent — worse than failing, because it would look like it worked.
        let json = r#"{"sid":7.5,"torrent_id":74215,"info_hash":"x"}"#;
        let err = serde_json::from_str::<SeedCandidate>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不是整数"), "{err}");
    }

    #[test]
    fn a_garbage_id_is_rejected_rather_than_defaulted_to_zero() {
        // A 0 id would render a nonsense template and fail at the site; failing
        // here names the actual problem.
        let json = r#"{"sid":7,"torrent_id":"not-a-number","info_hash":"x"}"#;
        let err = serde_json::from_str::<SeedCandidate>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("无法解析"), "{err}");
    }

    #[test]
    fn an_out_of_range_id_is_rejected() {
        let json = r#"{"sid":7,"torrent_id":99999999999999999999999999,"info_hash":"x"}"#;
        assert!(serde_json::from_str::<SeedCandidate>(json).is_err());
    }

    #[test]
    fn one_bad_row_reports_which_field_is_at_fault() {
        // The original message named neither the field nor the site. Callers log
        // this string, so it has to be diagnosable on its own.
        let json = r#"{"sid":7,"torrent_id":{},"info_hash":"x"}"#;
        let err = serde_json::from_str::<SeedCandidate>(json)
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty(), "an error with no message is undebuggable");
    }
}
