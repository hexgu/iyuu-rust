//! SQLite persistence.
//!
//! Two things here matter for correctness rather than convenience:
//!
//! 1. **Attempts are persisted, including in-flight verification state.** The
//!    reference plugin keeps its recheck queue in a Python dict, so a restart
//!    drops every cross-seed that has not finished verifying — they sit paused
//!    forever with nothing tracking them. Here `pending_attempts()` is a query,
//!    so a restart resumes exactly where it left off.
//!
//! 2. **`seeding` is a distinct state from `added`.** Counting an add as a
//!    success is what makes the "it says 200 succeeded but 190 are paused"
//!    situation possible. We record both and the UI shows both.

use crate::error::ServerError;
use chrono::{DateTime, Utc};
use iyuu_core::engine::AttemptStore;
use iyuu_core::models::{ReseedAttempt, ReseedState, Run, RunStats, RunStatus, RunTrigger};
use iyuu_core::ratelimit::QuotaUsage;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

pub type DbPool = Pool<SqliteConnectionManager>;

#[derive(Clone)]
pub struct Db {
    pub pool: DbPool,
}

const MIGRATIONS: &[&str] = &[
    r"
    CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    r"
    CREATE TABLE IF NOT EXISTS torrent_cache (
        info_hash   TEXT PRIMARY KEY,
        site_id     INTEGER NOT NULL,
        site_name   TEXT NOT NULL,
        torrent_id  INTEGER NOT NULL,
        seen_at     TEXT NOT NULL
    );
    ",
    r"
    CREATE TABLE IF NOT EXISTS attempts (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        source_hash       TEXT NOT NULL,
        source_name       TEXT NOT NULL,
        save_path         TEXT NOT NULL,
        target_info_hash  TEXT NOT NULL,
        target_sid        INTEGER NOT NULL,
        target_site       TEXT NOT NULL,
        target_torrent_id INTEGER NOT NULL,
        state             TEXT NOT NULL,
        check_count       INTEGER NOT NULL DEFAULT 0,
        last_checked_at   TEXT,
        verify_started_at TEXT,
        added_at          TEXT,
        finished_at       TEXT,
        message           TEXT,
        downloader        TEXT NOT NULL DEFAULT '',
        run_id            TEXT,
        created_at        TEXT NOT NULL
    );
    ",
    // One row per target hash: re-running discovery must not create duplicates.
    r"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_attempts_target
        ON attempts(target_info_hash);
    ",
    r"
    CREATE INDEX IF NOT EXISTS idx_attempts_state
        ON attempts(state);
    ",
    r"
    CREATE TABLE IF NOT EXISTS runs (
        id          TEXT PRIMARY KEY,
        trigger     TEXT NOT NULL,
        status      TEXT NOT NULL,
        started_at  TEXT NOT NULL,
        finished_at TEXT,
        stats       TEXT NOT NULL,
        message     TEXT
    );
    ",
    r"
    CREATE INDEX IF NOT EXISTS idx_runs_started
        ON runs(started_at DESC);
    ",
];

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self, ServerError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Migrate on a single dedicated connection *before* the pool exists.
        // Building the pool first and then migrating through it races: every
        // pooled connection applies its `with_init` PRAGMAs, and a
        // `journal_mode = WAL` transition needs an exclusive lock, so concurrent
        // connections fail with "database is locked" on a fresh file.
        {
            let conn = rusqlite::Connection::open(path)?;
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;",
            )?;
            for sql in MIGRATIONS {
                conn.execute_batch(sql)?;
            }
        }

        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;",
            )
        });
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .map_err(|e| ServerError::Db(format!("无法建立数据库连接池：{e}")))?;
        Ok(Self { pool })
    }

    // ------------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, ServerError> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), ServerError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- api usage

    /// Persisted daily usage of the IYUU API.
    ///
    /// This has to outlive the client object, which is rebuilt per run so a
    /// config change takes effect without a restart. Without a stored counter, a
    /// mid-day restart would reset the budget to full and the next run would
    /// spend requests the account no longer has.
    pub fn load_api_usage(&self) -> Result<QuotaUsage, ServerError> {
        const KEY: &str = "iyuu_api_usage";
        let raw = self.get_setting(KEY)?;
        match raw.as_deref().map(serde_json::from_str::<QuotaUsage>) {
            Some(Ok(usage)) => Ok(usage),
            // A corrupt counter must not block runs: starting from zero risks a
            // rejected request, which is recoverable; refusing to run is not.
            Some(Err(e)) => {
                tracing::warn!(error = %e, "已保存的 API 用量无法解析，按 0 计");
                Ok(QuotaUsage::default())
            }
            None => Ok(QuotaUsage::default()),
        }
    }

    pub fn save_api_usage(&self, usage: &QuotaUsage) -> Result<(), ServerError> {
        self.set_setting(
            "iyuu_api_usage",
            &serde_json::to_string(usage).unwrap_or_else(|_| "{}".into()),
        )
    }

    // ----------------------------------------------------------------- runs

    pub fn create_run(&self, run: &Run) -> Result<(), ServerError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO runs(id, trigger, status, started_at, stats, message)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id,
                run.trigger.as_str(),
                run.status.as_str(),
                run.started_at.to_rfc3339(),
                serde_json::to_string(&run.stats).unwrap_or_else(|_| "{}".into()),
                run.message,
            ],
        )?;
        Ok(())
    }

    // `status` (lifecycle) and `stats` (counters) are both SQL columns here;
    // renaming either would only make the INSERT bindings harder to match up.
    #[expect(
        clippy::similar_names,
        reason = "both names mirror the columns they bind to"
    )]
    pub fn finish_run(
        &self,
        run_id: &str,
        status: RunStatus,
        stats: &RunStats,
        message: Option<&str>,
    ) -> Result<(), ServerError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3, stats = ?4, message = ?5
             WHERE id = ?1",
            params![
                run_id,
                status.as_str(),
                Utc::now().to_rfc3339(),
                serde_json::to_string(stats).unwrap_or_else(|_| "{}".into()),
                message,
            ],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, limit: i64) -> Result<Vec<Run>, ServerError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, trigger, status, started_at, finished_at, stats, message
             FROM runs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            let stats: String = row.get(5)?;
            let started: String = row.get(3)?;
            let finished: Option<String> = row.get(4)?;
            Ok(Run {
                id: row.get(0)?,
                trigger: RunTrigger::parse(&row.get::<_, String>(1)?),
                status: RunStatus::parse(&row.get::<_, String>(2)?),
                started_at: parse_dt(&started),
                finished_at: finished.as_deref().map(parse_dt),
                stats: serde_json::from_str(&stats).unwrap_or_default(),
                message: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// A run still marked running that nobody owns any more (process restarted
    /// mid-run). Marking these failed keeps the UI honest.
    pub fn reconcile_orphan_runs(&self) -> Result<usize, ServerError> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE runs SET status = 'failed', finished_at = ?1,
                    message = COALESCE(message, '进程中断，本次任务未完成')
             WHERE status = 'running'",
            params![Utc::now().to_rfc3339()],
        )?;
        Ok(n)
    }

    // ------------------------------------------------------------- attempts

    pub fn list_attempts(
        &self,
        state: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ReseedAttempt>, ServerError> {
        let conn = self.pool.get()?;
        let map_row = |row: &rusqlite::Row<'_>| attempt_from_row(row);

        let mut out = Vec::new();
        if let Some(s) = state {
            let mut stmt = conn.prepare(
                "SELECT * FROM attempts WHERE state = ?1
                 ORDER BY id DESC LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![s, limit, offset], map_row)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt =
                conn.prepare("SELECT * FROM attempts ORDER BY id DESC LIMIT ?1 OFFSET ?2")?;
            let rows = stmt.query_map(params![limit, offset], map_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn count_attempts(&self, state: Option<&str>) -> Result<i64, ServerError> {
        let conn = self.pool.get()?;
        let n = match state {
            Some(s) => {
                conn.query_row("SELECT COUNT(*) FROM attempts WHERE state = ?1", [s], |r| {
                    r.get(0)
                })?
            }
            None => conn.query_row("SELECT COUNT(*) FROM attempts", [], |r| r.get(0))?,
        };
        Ok(n)
    }

    /// Per-state counts, for the dashboard.
    pub fn state_counts(&self) -> Result<Vec<(String, i64)>, ServerError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM attempts GROUP BY state")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_attempt(&self, id: i64) -> Result<Option<ReseedAttempt>, ServerError> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row("SELECT * FROM attempts WHERE id = ?1", [id], |r| {
                attempt_from_row(r)
            })
            .optional()?)
    }

    /// Force an attempt back into the verification queue. Used by the UI's
    /// "re-check" action after a user fixes a save path.
    pub fn requeue_attempt(&self, id: i64) -> Result<(), ServerError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE attempts SET state = 'added', check_count = 0,
                    verify_started_at = ?2, finished_at = NULL,
                    message = '手动重新校验'
             WHERE id = ?1",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_attempt(&self, id: i64) -> Result<(), ServerError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM attempts WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).map_or_else(|_| Utc::now(), |d| d.with_timezone(&Utc))
}

fn attempt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReseedAttempt> {
    Ok(ReseedAttempt {
        id: row.get("id")?,
        source_hash: row.get("source_hash")?,
        source_name: row.get("source_name")?,
        save_path: row.get("save_path")?,
        target_info_hash: row.get("target_info_hash")?,
        target_sid: row.get("target_sid")?,
        target_site: row.get("target_site")?,
        target_torrent_id: row.get("target_torrent_id")?,
        state: ReseedState::parse(&row.get::<_, String>("state")?),
        check_count: row.get("check_count")?,
        last_checked_at: row
            .get::<_, Option<String>>("last_checked_at")?
            .as_deref()
            .map(parse_dt),
        verify_started_at: row
            .get::<_, Option<String>>("verify_started_at")?
            .as_deref()
            .map(parse_dt),
        added_at: row
            .get::<_, Option<String>>("added_at")?
            .as_deref()
            .map(parse_dt),
        finished_at: row
            .get::<_, Option<String>>("finished_at")?
            .as_deref()
            .map(parse_dt),
        message: row.get("message")?,
        downloader: row.get("downloader")?,
        run_id: row.get("run_id")?,
    })
}

#[async_trait::async_trait]
impl AttemptStore for Db {
    async fn upsert_attempt(&self, attempt: &mut ReseedAttempt) -> iyuu_core::Result<i64> {
        // rusqlite is synchronous; a tiny blocking hop keeps the async trait
        // honest without pulling in a blocking pool for a handful of writes.
        let this = self.clone();
        let mut attempt = attempt.clone();
        let id = tokio::task::spawn_blocking(move || -> Result<i64, ServerError> {
            let conn = this.pool.get()?;
            conn.execute(
                "INSERT INTO attempts(
                    source_hash, source_name, save_path, target_info_hash, target_sid,
                    target_site, target_torrent_id, state, check_count, last_checked_at,
                    verify_started_at, added_at, finished_at, message, downloader, run_id, created_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(target_info_hash) DO UPDATE SET
                    state = excluded.state,
                    check_count = excluded.check_count,
                    last_checked_at = excluded.last_checked_at,
                    verify_started_at = excluded.verify_started_at,
                    added_at = excluded.added_at,
                    message = excluded.message,
                    run_id = excluded.run_id",
                params![
                    attempt.source_hash,
                    attempt.source_name,
                    attempt.save_path,
                    attempt.target_info_hash,
                    attempt.target_sid,
                    attempt.target_site,
                    attempt.target_torrent_id,
                    attempt.state.as_str(),
                    attempt.check_count,
                    attempt.last_checked_at.map(|d| d.to_rfc3339()),
                    attempt.added_at.map(|d| d.to_rfc3339()),
                    attempt.verify_started_at.map(|d| d.to_rfc3339()),
                    attempt.finished_at.map(|d| d.to_rfc3339()),
                    attempt.message,
                    attempt.downloader,
                    attempt.run_id,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            let id: i64 = conn.query_row(
                "SELECT id FROM attempts WHERE target_info_hash = ?1",
                [&attempt.target_info_hash],
                |r| r.get(0),
            )?;
            Ok(id)
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("写入任务记录失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))?;

        attempt.id = id;
        Ok(id)
    }

    async fn update_state(
        &self,
        id: i64,
        state: ReseedState,
        message: Option<&str>,
    ) -> iyuu_core::Result<()> {
        let this = self.clone();
        let message = message.map(std::string::ToString::to_string);
        tokio::task::spawn_blocking(move || -> Result<(), ServerError> {
            let conn = this.pool.get()?;
            let finished = state.is_terminal().then(|| Utc::now().to_rfc3339());
            conn.execute(
                "UPDATE attempts SET state = ?2, message = ?3,
                        finished_at = COALESCE(?4, finished_at)
                 WHERE id = ?1",
                params![id, state.as_str(), message, finished],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("更新任务状态失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))
    }

    async fn update_check(
        &self,
        id: i64,
        state: ReseedState,
        check_count: i32,
        message: Option<&str>,
    ) -> iyuu_core::Result<()> {
        let this = self.clone();
        let message = message.map(std::string::ToString::to_string);
        tokio::task::spawn_blocking(move || -> Result<(), ServerError> {
            let conn = this.pool.get()?;
            let now = Utc::now().to_rfc3339();
            let finished = state.is_terminal().then(|| now.clone());
            conn.execute(
                "UPDATE attempts SET state = ?2, check_count = ?3, message = ?4,
                        last_checked_at = ?5,
                        verify_started_at = COALESCE(verify_started_at, ?5),
                        finished_at = COALESCE(?6, finished_at)
                 WHERE id = ?1",
                params![id, state.as_str(), check_count, message, now, finished],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("更新校验状态失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))
    }

    async fn pending_attempts(&self) -> iyuu_core::Result<Vec<ReseedAttempt>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<ReseedAttempt>, ServerError> {
            let conn = this.pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT * FROM attempts
                 WHERE state IN ('added', 'verifying')
                 ORDER BY COALESCE(verify_started_at, added_at, created_at) ASC",
            )?;
            let rows = stmt.query_map([], attempt_from_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("读取待校验任务失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))
    }

    async fn seeded_targets(&self) -> iyuu_core::Result<HashSet<String>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<HashSet<String>, ServerError> {
            let conn = this.pool.get()?;
            let mut stmt =
                conn.prepare("SELECT target_info_hash FROM attempts WHERE state = 'seeding'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<HashSet<_>, _>>()?)
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("读取已辅种记录失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))
    }

    async fn known_bad_targets(&self) -> iyuu_core::Result<HashSet<String>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || -> Result<HashSet<String>, ServerError> {
            let conn = this.pool.get()?;
            // `verify_failed` / `missing_files` / `timed_out` are terminal and
            // must not be retried on every scheduled run: they need a human to
            // fix the cause (usually a save-path mismatch).
            let mut stmt = conn.prepare(
                "SELECT target_info_hash FROM attempts
                 WHERE state IN ('verify_failed', 'missing_files', 'timed_out',
                                 'fetch_failed', 'add_failed')",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<HashSet<_>, _>>()?)
        })
        .await
        .map_err(|e| iyuu_core::CoreError::Other(format!("读取失败记录失败：{e}")))?
        .map_err(|e| iyuu_core::CoreError::Other(e.to_string()))
    }
}
