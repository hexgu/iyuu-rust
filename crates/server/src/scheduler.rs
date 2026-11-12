//! Cron scheduling for discovery runs.
//!
//! A single task ticks every 30 seconds and compares a precomputed schedule. This
//! is deliberately simpler than a general cron engine: we need "run daily at
//! 04:00", we need config changes to take effect without a restart, and we need
//! to never overlap runs. 30-second granularity is plenty and makes the "is it
//! due?" logic trivial to reason about.
//!
//! The dedupe is per-minute: if a tick lands twice in the same minute (clock
//! adjustments, a slow tick), the run only fires once.

use crate::runner::{self, AppState};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use iyuu_core::models::RunTrigger;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::task::JoinHandle;

pub struct Scheduler {
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Scheduler {
    pub async fn shutdown(&self) {
        if let Some(h) = self.handle.lock().take() {
            h.abort();
        }
    }
}

pub fn spawn(state: AppState) -> Scheduler {
    let handle = tokio::spawn(loop_task(state));
    Scheduler {
        handle: Arc::new(Mutex::new(Some(handle))),
    }
}

async fn loop_task(state: AppState) {
    // Last minute we fired, as `YYYY-MM-DD HH:MM` in local time. Prevents
    // double-firing within the same minute.
    let mut last_fired: Option<String> = None;

    loop {
        let cfg = state.config_snapshot();
        if cfg.schedule.enabled
            && let Some(schedule) = parse_cron(&cfg.schedule.cron)
        {
            let now = chrono::Local::now();
            let minute_key = now.format("%Y-%m-%d %H:%M").to_string();

            if last_fired.as_deref() != Some(minute_key.as_str())
                && schedule.matches(now.with_timezone(&Utc))
            {
                last_fired = Some(minute_key);
                if state.is_running.load(Ordering::SeqCst) {
                    state.push_log("定时任务跳过：已有任务在运行");
                } else {
                    state.push_log(format!("定时任务触发（{}）", cfg.schedule.cron));
                    let state2 = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            runner::execute_discovery(&state2, RunTrigger::Schedule).await
                        {
                            state2.push_log(format!("定时任务失败：{e}"));
                        }
                    });
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Minimal 5-field cron: `min hour dom mon dow`.
///
/// Supports `*`, exact numbers, comma lists, ranges (`1-5`), and steps (`*/5`).
/// That covers every realistic schedule for this tool and avoids a dependency
/// whose semantics we would have to re-verify anyway.
#[derive(Debug, Clone)]
pub struct Schedule {
    minutes: Field,
    hours: Field,
    days_of_month: Field,
    months: Field,
    days_of_week: Field,
}

#[derive(Debug, Clone)]
struct Field {
    values: Vec<u32>,
    is_wildcard: bool,
}

impl Field {
    fn parse(spec: &str, min: u32, max: u32) -> Option<Self> {
        let is_wildcard = spec == "*";
        let mut values = Vec::new();

        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (range_part, step) = match part.split_once('/') {
                Some((r, s)) => (r, s.parse::<u32>().ok().filter(|v| *v > 0)?),
                None => (part, 1),
            };

            let (start, end) = if range_part == "*" {
                (min, max)
            } else if let Some((a, b)) = range_part.split_once('-') {
                (a.parse().ok()?, b.parse().ok()?)
            } else {
                let v: u32 = range_part.parse().ok()?;
                // A bare value with a step (`5/2`) means "5 to max".
                if step > 1 { (v, max) } else { (v, v) }
            };

            if start > end || end > max || start < min {
                return None;
            }
            let mut v = start;
            while v <= end {
                values.push(v);
                v += step;
            }
        }

        values.sort_unstable();
        values.dedup();
        if values.is_empty() {
            return None;
        }
        Some(Self {
            values,
            is_wildcard,
        })
    }

    fn matches(&self, v: u32) -> bool {
        self.values.contains(&v)
    }
}

impl Schedule {
    pub fn parse(expr: &str) -> Option<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        Some(Self {
            minutes: Field::parse(parts[0], 0, 59)?,
            hours: Field::parse(parts[1], 0, 23)?,
            days_of_month: Field::parse(parts[2], 1, 31)?,
            months: Field::parse(parts[3], 1, 12)?,
            days_of_week: Field::parse(parts[4], 0, 6)?,
        })
    }

    /// Whether the schedule fires during this minute (UTC).
    pub fn matches(&self, now: chrono::DateTime<Utc>) -> bool {
        if !self.minutes.matches(now.minute()) {
            return false;
        }
        if !self.hours.matches(now.hour()) {
            return false;
        }
        if !self.months.matches(now.month()) {
            return false;
        }

        // Standard cron quirk: if BOTH dom and dow are restricted, the schedule
        // fires when EITHER matches. Respecting this avoids surprising users who
        // copy a cron expression from elsewhere.
        let dom_match = self.days_of_month.matches(now.day());
        let dow_match = self
            .days_of_week
            .matches(now.weekday().num_days_from_sunday());
        match (
            self.days_of_month.is_wildcard,
            self.days_of_week.is_wildcard,
        ) {
            (true, true) => true,
            (true, false) => dow_match,
            (false, true) => dom_match,
            (false, false) => dom_match || dow_match,
        }
    }

    /// Next fire time, for showing "next run" in the UI. Brute-force to the
    /// minute; bounded to ~400 days which is more than a cron can care about.
    pub fn next_after(&self, from: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
        let mut t = from + chrono::Duration::minutes(1);
        // Align to the minute boundary.
        t = Utc
            .with_ymd_and_hms(t.year(), t.month(), t.day(), t.hour(), t.minute(), 0)
            .single()?;
        for _ in 0..(400 * 24 * 60) {
            if self.matches(t) {
                return Some(t);
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }
}

/// When the configured schedule will next fire, for the status endpoint.
/// `None` when the schedule is disabled or the expression is invalid.
pub fn next_run_at(cfg: &crate::config::ScheduleConfig) -> Option<chrono::DateTime<Utc>> {
    if !cfg.enabled {
        return None;
    }
    Schedule::parse(&cfg.cron)?.next_after(Utc::now())
}

fn parse_cron(expr: &str) -> Option<Schedule> {
    match Schedule::parse(expr) {
        Some(s) => Some(s),
        None => {
            tracing::warn!(expr, "cron 表达式无效，定时任务已跳过");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_daily_four_am() {
        let s = Schedule::parse("0 4 * * *").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 11, 12, 4, 0, 0).unwrap();
        assert!(s.matches(t));
        let t2 = Utc.with_ymd_and_hms(2026, 11, 12, 4, 1, 0).unwrap();
        assert!(!s.matches(t2));
    }

    #[test]
    fn parses_steps_and_ranges() {
        let s = Schedule::parse("*/15 9-11 * * *").unwrap();
        for m in [0, 15, 30, 45] {
            let t = Utc.with_ymd_and_hms(2026, 11, 12, 10, m, 0).unwrap();
            assert!(s.matches(t), "minute {m} should match");
        }
        let t = Utc.with_ymd_and_hms(2026, 11, 12, 10, 20, 0).unwrap();
        assert!(!s.matches(t));
        // Outside the hour range.
        let t = Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap();
        assert!(!s.matches(t));
    }

    #[test]
    fn dow_restriction_matches_that_day() {
        // 2026-11-12 is a Thursday (dow=4).
        let s = Schedule::parse("30 3 * * 4").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 11, 12, 3, 30, 0).unwrap();
        assert!(s.matches(t));
        let t2 = Utc.with_ymd_and_hms(2026, 11, 13, 3, 30, 0).unwrap();
        assert!(!s.matches(t2));
    }

    #[test]
    fn invalid_expressions_rejected() {
        assert!(Schedule::parse("0 4 * *").is_none());
        assert!(Schedule::parse("99 4 * * *").is_none());
        assert!(Schedule::parse("0 4 * * 9").is_none());
        assert!(Schedule::parse("").is_none());
    }

    #[test]
    fn next_after_finds_next_occurrence() {
        let s = Schedule::parse("0 4 * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2026, 11, 12, 5, 0, 0).unwrap();
        let next = s.next_after(from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 11, 13, 4, 0, 0).unwrap());
    }

    #[test]
    fn dom_and_dow_both_restricted_uses_or() {
        // 12th OR Thursday: the 1st of the month (not Thursday) still matches.
        let s = Schedule::parse("0 0 1 * 4").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        assert!(
            s.matches(t),
            "day-of-month match should fire even if not Thursday"
        );
    }
}
