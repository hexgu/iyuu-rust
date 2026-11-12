//! IYUU API quota, host selection, and pacing policy.
//!
//! The API is metered per account per day, and the accounting belongs to the
//! server — we can only keep a local mirror of it, stay under it, and stop the
//! moment IYUU says we are over. Three facts drive everything in this module:
//!
//!  * One reseed query carries at most 200 info-hashes, so the daily budget is
//!    really "N queries × 200 seeds".
//!  * The free account and the SVIP account are served by **different hosts**
//!    and have very different budgets. Which host to hit cannot be probed
//!    reliably (a wrong guess looks like an auth failure), so the account holder
//!    declares their tier and we honour it.
//!  * The reset boundary is IYUU's, not ours. The service is operated from
//!    China, so we mirror a UTC+8 day boundary; the server's own counter is
//!    always authoritative and a disagreement only ever costs us a wasted
//!    request, never an over-quota run.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Info-hashes per reseed query. Documented as 200; larger batches are rejected
/// outright, and a rejection loses the whole batch.
pub const HASHES_PER_REQUEST: usize = 200;

/// Offset of the daily quota boundary from UTC, in hours.
const QUOTA_TZ_OFFSET_HOURS: i64 = 8;

/// Which IYUU membership the account holds.
///
/// This is a **user declaration**, not something we can detect. Both hosts front
/// the same service and answer the same endpoints for the same token, so no probe
/// can tell a member from a non-member: a successful response proves nothing and
/// a failure proves nothing either. What the tier actually controls is (a) which
/// host we talk to and (b) how much daily budget we plan around.
///
/// Defaulting to `Free` is the safe direction — it under-uses a VIP budget
/// instead of planning 2500 requests a free account does not have. The UI
/// therefore forces an explicit choice instead of relying on the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiTier {
    #[default]
    Free,
    Vip,
}

impl ApiTier {
    /// Default API host for a free account.
    pub const FREE_BASE_URL: &'static str = "https://2025.iyuu.cn";
    /// Default API host advertised for SVIP accounts.
    ///
    /// The same service sits behind it — a different entry point, not a different
    /// product. Picking this without a membership does not break the connection;
    /// it only means the budget we plan around (2500/day) is bigger than the
    /// account's real one, and IYUU's own limiter becomes the thing that says no.
    pub const VIP_BASE_URL: &str = "https://vip.iyuu.cn";

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Vip => "vip",
        }
    }

    /// Anything unrecognised falls back to `Free`: guessing "VIP" would let a
    /// free account burn through ten times the budget it actually has.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "vip" | "svip" | "member" | "premium" => Self::Vip,
            _ => Self::Free,
        }
    }

    /// Host to send requests to, when the user has not overridden it.
    #[must_use]
    pub fn base_url(&self) -> &'static str {
        match self {
            Self::Free => Self::FREE_BASE_URL,
            Self::Vip => Self::VIP_BASE_URL,
        }
    }

    /// Reseed queries allowed per day, per IYUU's published limits.
    #[must_use]
    pub fn daily_requests(&self) -> u32 {
        match self {
            Self::Free => 500,
            Self::Vip => 2500,
        }
    }

    /// Seeds those queries can cover (requests × 200).
    #[must_use]
    pub fn daily_hashes(&self) -> u32 {
        self.daily_requests() * u32::try_from(HASHES_PER_REQUEST).unwrap_or(200)
    }

    /// Spacing between consecutive requests.
    ///
    /// Deliberately **the same for both tiers**. IYUU throttles bursts regardless
    /// of how much daily budget the account has — two requests a few hundred
    /// milliseconds apart earn "访问频率过快" either way — so a different interval
    /// per tier would only add a way to get it wrong. A too-tight gap costs a
    /// 30-minute cooldown, which is far worse than the second it saves.
    ///
    /// A single constant also means the engine's pacing cannot accidentally
    /// encode an assumption about which server is in use.
    #[must_use]
    pub fn request_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(1200)
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Free => "普通用户",
            Self::Vip => "SVIP 会员",
        }
    }
}

/// How many queries one run may still spend.
///
/// Both fields are hard caps: a run stops at whichever runs out first, because
/// a query that carries fewer than 200 hashes still costs one full request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaBudget {
    pub requests: u32,
    pub hashes: u32,
}

impl QuotaBudget {
    /// No local cap (used by tests and by an explicitly unlimited tier).
    pub const UNLIMITED: Self = Self {
        requests: u32::MAX,
        hashes: u32::MAX,
    };

    #[must_use]
    pub fn new(requests: u32, hashes: u32) -> Self {
        Self { requests, hashes }
    }

    /// Full budget for a tier on an untouched day.
    #[must_use]
    pub fn full(tier: ApiTier) -> Self {
        Self::new(tier.daily_requests(), tier.daily_hashes())
    }

    #[must_use]
    pub fn is_unlimited(&self) -> bool {
        *self == Self::UNLIMITED
    }

    /// Whole batches this budget affords. A partial batch is not afforded: the
    /// request cost is the same whether it carries 1 hash or 200, so spending a
    /// request on 3 hashes wastes the budget.
    #[must_use]
    pub fn batches(&self) -> usize {
        if self.is_unlimited() {
            return usize::MAX;
        }
        let by_requests = usize::try_from(self.requests).unwrap_or(usize::MAX);
        let by_hashes = usize::try_from(self.hashes).unwrap_or(usize::MAX) / HASHES_PER_REQUEST;
        by_requests.min(by_hashes)
    }

    /// Hashes this budget can actually carry.
    #[must_use]
    pub fn hashes_affordable(&self) -> usize {
        self.batches().saturating_mul(HASHES_PER_REQUEST)
    }
}

/// Persisted form of the local quota mirror.
///
/// The client instance is short-lived (one per run, so a config change takes
/// effect without a restart), while the *day's usage* outlives it. This struct is
/// what the server stores between runs — without it a restart mid-day would hand
/// the next run a fresh budget it does not have.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaUsage {
    /// Quota day the counters below belong to (`YYYY-MM-DD`, UTC+8).
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub used_requests: u32,
    #[serde(default)]
    pub used_hashes: u32,
}

impl QuotaUsage {
    /// Counters are only meaningful for the day they were recorded on; a stale
    /// record from yesterday means "nothing used yet".
    #[must_use]
    pub fn for_day(&self, day: &str) -> Self {
        if self.day == day {
            self.clone()
        } else {
            Self {
                day: day.to_string(),
                ..Self::default()
            }
        }
    }
}

/// Local mirror of the account's daily usage, for the API and the UI.
///
/// `used_*` is what *we* have sent today. IYUU counts the same requests the same
/// way, so the numbers track; when they drift (another client on the same token)
/// the server still has the final say and will reject us.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaSnapshot {
    pub tier: ApiTier,
    pub tier_label: &'static str,
    /// Quota day, `YYYY-MM-DD` in UTC+8.
    pub day: String,
    pub daily_requests: u32,
    pub used_requests: u32,
    pub remaining_requests: u32,
    pub daily_hashes: u32,
    pub used_hashes: u32,
    pub remaining_hashes: u32,
    /// Set while IYUU has told us to back off.
    pub blocked_until: Option<DateTime<Utc>>,
    /// When today's counter resets (next UTC+8 midnight).
    pub reset_at: Option<DateTime<Utc>>,
    /// Percentage of the request budget consumed, 0–100.
    pub percent_used: f64,
    /// Requests refused locally because the budget was gone. Non-zero explains a
    /// run that added far less than the site list suggests it should have.
    pub denied_requests: u32,
}

impl QuotaSnapshot {
    #[must_use]
    pub fn new(
        tier: ApiTier,
        now: DateTime<Utc>,
        used_requests: u32,
        used_hashes: u32,
        blocked_until: Option<DateTime<Utc>>,
    ) -> Self {
        let daily_requests = tier.daily_requests();
        let daily_hashes = tier.daily_hashes();
        let remaining_requests = daily_requests.saturating_sub(used_requests);
        let remaining_hashes = daily_hashes.saturating_sub(used_hashes);
        // `u32 -> f64` is lossless: f64 holds 53 bits and these counters are
        // thousands at most, so no `cast_precision_loss` suppression is needed.
        let percent_used = if daily_requests == 0 {
            0.0
        } else {
            (f64::from(used_requests) / f64::from(daily_requests) * 100.0).min(100.0)
        };

        Self {
            tier,
            tier_label: tier.label(),
            day: quota_day(now),
            daily_requests,
            used_requests,
            remaining_requests,
            daily_hashes,
            used_hashes,
            remaining_hashes,
            blocked_until,
            reset_at: quota_reset_at(now),
            percent_used,
            denied_requests: 0,
        }
    }

    /// Budget the engine may spend right now, from this snapshot alone.
    ///
    /// A block is a hard stop, not a hint: the next request during a cooldown is
    /// exactly what turns a temporary throttle into a longer ban.
    #[must_use]
    pub fn budget(&self) -> QuotaBudget {
        if self.blocked_until.is_some_and(|at| at > Utc::now()) {
            return QuotaBudget::new(0, 0);
        }
        QuotaBudget::new(self.remaining_requests, self.remaining_hashes)
    }
}

/// Quota day key (`YYYY-MM-DD`, UTC+8). Shifting UTC by the offset and taking the
/// date is equivalent to converting the timezone and cannot fail.
#[must_use]
pub fn quota_day(now: DateTime<Utc>) -> String {
    let shifted = now + Duration::hours(QUOTA_TZ_OFFSET_HOURS);
    shifted.format("%Y-%m-%d").to_string()
}

/// Next UTC+8 midnight, in UTC. `None` only for dates chrono cannot represent.
#[must_use]
pub fn quota_reset_at(now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let shifted = now + Duration::hours(QUOTA_TZ_OFFSET_HOURS);
    let midnight = shifted.date_naive().and_hms_opt(0, 0, 0)?;
    let next = DateTime::<Utc>::from_naive_utc_and_offset(midnight, Utc) + Duration::hours(24);
    Some(next - Duration::hours(QUOTA_TZ_OFFSET_HOURS))
}

/// Substrings IYUU uses to report "you are over the limit". Compared
/// case-insensitively against the envelope's `msg`.
const RATE_LIMIT_MARKERS: &[&str] = &[
    "超限",
    "限流",
    "超出",
    "频率",
    "频繁",
    "请求过多",
    "次数",
    "封禁",
    "rate limit",
    "too many requests",
];

/// Whether an upstream message is a rate-limit / quota rejection.
///
/// Getting this wrong in the permissive direction is harmless (we retry, the
/// server rejects again); getting it wrong in the strict direction is not, so
/// the marker list is deliberately broad. Unknown messages stay "generic error".
#[must_use]
pub fn looks_rate_limited(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    RATE_LIMIT_MARKERS
        .iter()
        .any(|marker| lower.contains(&marker.to_lowercase()))
}

/// How long to stop calling the API after a rate-limit rejection.
///
/// There is no `Retry-After` on this API, so we pick a value that is long enough
/// to let a burst window close and short enough that a scheduled daily run still
/// happens. Deliberately not exponential: a second rejection within the same day
/// usually means the daily budget is gone, and no amount of waiting fixes that.
#[must_use]
pub fn rate_limit_cooldown() -> Duration {
    Duration::minutes(30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn tier_limits_match_published_numbers() {
        assert_eq!(ApiTier::Free.daily_requests(), 500);
        assert_eq!(ApiTier::Free.daily_hashes(), 100_000);
        assert_eq!(ApiTier::Vip.daily_requests(), 2500);
        assert_eq!(ApiTier::Vip.daily_hashes(), 500_000);
    }

    #[test]
    fn tiers_use_different_entry_points() {
        // Two entry points into the *same* service. This is a routing detail, not
        // proof of membership — which is why the tier can never be verified by
        // probing and has to be declared by the user.
        assert_eq!(ApiTier::Free.base_url(), "https://2025.iyuu.cn");
        assert_eq!(ApiTier::Vip.base_url(), "https://vip.iyuu.cn");
    }

    #[test]
    fn tier_only_changes_budget_and_host_not_pacing() {
        // Pacing must not encode an assumption about which server is in use: IYUU
        // throttles bursts the same way for both, and a tighter interval costs a
        // 30-minute cooldown rather than a little budget.
        assert_eq!(
            ApiTier::Free.request_interval(),
            ApiTier::Vip.request_interval()
        );
        // What the tier *does* change is the planned budget.
        assert_ne!(
            ApiTier::Free.daily_requests(),
            ApiTier::Vip.daily_requests()
        );
    }

    #[test]
    fn tier_parsing_defaults_to_free() {
        assert_eq!(ApiTier::parse("vip"), ApiTier::Vip);
        assert_eq!(ApiTier::parse("SVIP"), ApiTier::Vip);
        // A typo must not grant a ten-times budget.
        assert_eq!(ApiTier::parse("vpi"), ApiTier::Free);
        assert_eq!(ApiTier::parse(""), ApiTier::Free);
    }

    #[test]
    fn budget_counts_only_whole_batches() {
        // 3 requests but only 250 hashes: one full batch fits, the 50 leftover
        // hashes would cost a whole request, so they are not counted.
        let b = QuotaBudget::new(3, 250);
        assert_eq!(b.batches(), 1);
        assert_eq!(b.hashes_affordable(), 200);

        let b = QuotaBudget::new(3, 600);
        assert_eq!(b.batches(), 3);
        assert_eq!(b.hashes_affordable(), 600);

        // Hashes left but no requests: nothing can be sent.
        assert_eq!(QuotaBudget::new(0, 10_000).batches(), 0);
    }

    #[test]
    fn unlimited_budget_never_truncates() {
        let b = QuotaBudget::UNLIMITED;
        assert!(b.is_unlimited());
        assert_eq!(b.batches(), usize::MAX);
        assert_eq!(b.hashes_affordable(), usize::MAX);
    }

    #[test]
    fn full_budget_matches_tier() {
        assert_eq!(QuotaBudget::full(ApiTier::Free).batches(), 500);
        assert_eq!(QuotaBudget::full(ApiTier::Vip).batches(), 2500);
    }

    #[test]
    fn snapshot_reports_remaining_and_percent() {
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 8, 0, 0).unwrap();
        let s = QuotaSnapshot::new(ApiTier::Free, now, 125, 25_000, None);
        assert_eq!(s.remaining_requests, 375);
        assert_eq!(s.remaining_hashes, 75_000);
        assert!((s.percent_used - 25.0).abs() < f64::EPSILON);
        assert_eq!(s.day, "2026-11-12");
    }

    #[test]
    fn snapshot_percent_is_capped_when_over_budget() {
        // Uses we recorded from an earlier tier, or another client sharing the
        // token, must not render as 300%.
        let now = Utc.with_ymd_and_hms(2026, 11, 12, 8, 0, 0).unwrap();
        let s = QuotaSnapshot::new(ApiTier::Free, now, 900, 180_000, None);
        assert_eq!(s.remaining_requests, 0);
        assert!((s.percent_used - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn active_block_yields_zero_budget() {
        let now = Utc::now();
        let s = QuotaSnapshot::new(
            ApiTier::Free,
            now,
            10,
            2000,
            Some(now + Duration::minutes(5)),
        );
        assert_eq!(s.budget().batches(), 0);
    }

    #[test]
    fn expired_block_restores_budget() {
        let now = Utc::now();
        let s = QuotaSnapshot::new(
            ApiTier::Free,
            now,
            10,
            2000,
            Some(now - Duration::minutes(5)),
        );
        assert_eq!(s.budget().batches(), 490);
    }

    #[test]
    fn quota_day_uses_china_boundary() {
        // 2026-11-12 16:30 UTC is already the 13th in UTC+8.
        let t = Utc.with_ymd_and_hms(2026, 11, 12, 16, 30, 0).unwrap();
        assert_eq!(quota_day(t), "2026-11-13");

        let reset = quota_reset_at(t).unwrap();
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 11, 13, 16, 0, 0).unwrap());
    }

    #[test]
    fn quota_day_is_stable_within_a_day() {
        let a = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap();
        let b = Utc.with_ymd_and_hms(2026, 11, 12, 15, 59, 0).unwrap();
        assert_eq!(quota_day(a), quota_day(b));
        assert_eq!(quota_day(a), "2026-11-12");
    }

    #[test]
    fn detects_rate_limit_messages() {
        assert!(looks_rate_limited("今日辅种查询次数已超限"));
        assert!(looks_rate_limited("请求过于频繁，请稍后再试"));
        assert!(looks_rate_limited("Too Many Requests"));
        assert!(looks_rate_limited("超出每日请求次数"));
        assert!(!looks_rate_limited("做种列表sha1校验失败"));
        assert!(!looks_rate_limited("用户未绑定推荐站点"));
    }
}
