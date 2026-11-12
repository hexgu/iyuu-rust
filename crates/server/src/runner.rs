//! Run orchestration: one long-lived service that owns the discovery run and a
//! verification loop, plus the shared state the API reads.
//!
//! Design notes that exist because of real failure modes:
//!
//! * **Only one discovery run at a time.** Two concurrent runs would both add
//!   the same candidates (the dedupe set is per-run), producing duplicate
//!   torrents. A mutex-guarded "is running" flag is the cheapest correct answer.
//! * **Verification runs independently of discovery.** It must keep working when
//!   IYUU is down or discovery is not scheduled, and it must survive a restart —
//!   which it does, because it reads pending attempts from the database rather
//!   than an in-memory queue.
//! * **Progress is pushed, not polled.** The API exposes a snapshot; the worker
//!   updates it as it goes so the UI can show a live log.

use crate::config::AppConfig;
use crate::db::Db;
use crate::error::{Result, ServerError};
use chrono::Utc;
use iyuu_core::downloader::{self, Downloader};
use iyuu_core::engine::{self, EngineContext};
use iyuu_core::iyuu::IyuuClient;
use iyuu_core::models::{Run, RunStats, RunStatus, RunTrigger};
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Live progress of the current (or last) discovery run.
#[derive(Debug, Clone, Serialize)]
pub struct RunProgress {
    pub run_id: Option<String>,
    pub running: bool,
    pub trigger: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub stats: RunStats,
    /// Rolling log of notable events, most recent first. Bounded so a long run
    /// cannot grow it without limit.
    pub log: Vec<String>,
    pub phase: String,
}

impl Default for RunProgress {
    fn default() -> Self {
        Self {
            run_id: None,
            running: false,
            trigger: "manual".into(),
            started_at: None,
            finished_at: None,
            stats: RunStats::default(),
            log: Vec::new(),
            phase: "idle".into(),
        }
    }
}

/// Shared application state. Cheap to clone (all Arc inside).
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<RwLock<AppConfig>>,
    pub progress: Arc<RwLock<RunProgress>>,
    /// Cancellation token for the in-flight run, if any.
    pub cancel: Arc<RwLock<Option<CancellationToken>>>,
    /// Guards against two discovery runs at once.
    pub run_gate: Arc<tokio::sync::Mutex<()>>,
    /// Set while a discovery run holds the gate, for a fast API-side check.
    pub is_running: Arc<AtomicBool>,
    pub started_at: chrono::DateTime<Utc>,
}

impl AppState {
    pub fn new(db: Db, config: AppConfig) -> Self {
        Self {
            db,
            config: Arc::new(RwLock::new(config)),
            progress: Arc::new(RwLock::new(RunProgress::default())),
            cancel: Arc::new(RwLock::new(None)),
            run_gate: Arc::new(tokio::sync::Mutex::new(())),
            is_running: Arc::new(AtomicBool::new(false)),
            started_at: Utc::now(),
        }
    }

    pub fn config_snapshot(&self) -> AppConfig {
        self.config.read().clone()
    }

    pub fn push_log(&self, line: impl Into<String>) {
        let mut p = self.progress.write();
        p.log.insert(0, format!("{} {}", Utc::now().format("%H:%M:%S"), line.into()));
        p.log.truncate(400);
    }
}

/// Build an engine context from the current config.
///
/// Rebuilt per run on purpose: a config change (new token, different downloader)
/// should take effect on the next run without restarting the process. The
/// IyuuClient is recreated too, which also drops its cached `sid_sha1` — correct,
/// because that cache is scoped to the token that produced it.
pub async fn build_engine(state: &AppState) -> Result<EngineContext> {
    let cfg = state.config_snapshot();

    if cfg.iyuu.token.trim().is_empty() {
        return Err(ServerError::Config("尚未配置 IYUU Token".into()));
    }

    let iyuu = match cfg.iyuu.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(base) => IyuuClient::with_base_url(cfg.iyuu.token.clone(), base)?,
        None => IyuuClient::new(cfg.iyuu.token.clone())?,
    };

    let kind = cfg.downloader.kind()?;
    let downloader: Arc<dyn Downloader> = downloader::build(
        kind,
        if cfg.downloader.name.is_empty() {
            kind.as_str()
        } else {
            &cfg.downloader.name
        },
        &cfg.downloader.base_url,
        &cfg.downloader.username,
        &cfg.downloader.password,
        &cfg.downloader.extra,
    )
    .await?;

    let blocked = {
        // Anything the downloader already holds is a duplicate; the engine
        // re-checks during discovery, but seeding this set up front keeps the
        // candidate list honest in the log.
        let mut set = std::collections::HashSet::new();
        for a in state.db.list_attempts(Some("verify_failed"), 500, 0)? {
            set.insert(a.target_info_hash);
        }
        set
    };

    Ok(EngineContext {
        iyuu: Arc::new(iyuu),
        downloader,
        http: iyuu_core::fetch::site_http_client()?,
        config: (&cfg.reseed).into(),
        credentials: cfg.credential_map(),
        blocked_hashes: blocked,
    })
}

/// Clears the `is_running` flag when dropped.
///
/// Three separate places used to set and clear this flag by hand, and any early
/// return that missed the clear would leave the verification loop permanently
/// skipped (it refuses to poll while a discovery run is in flight). A guard makes
/// "the flag is cleared" an invariant rather than something each path must
/// remember.
struct RunningGuard {
    state: AppState,
}

impl RunningGuard {
    fn new(state: AppState) -> Self {
        state.is_running.store(true, Ordering::SeqCst);
        Self { state }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.state.is_running.store(false, Ordering::SeqCst);
    }
}

/// Execute one discovery run to completion.
///
/// Takes the run gate itself rather than trusting the caller: the `is_running`
/// flag is only a UI hint, and two requests arriving at the same instant would
/// both see it false. `try_lock` makes "one run at a time" an invariant rather
/// than a race.
pub async fn execute_discovery(state: &AppState, trigger: RunTrigger) -> Result<String> {
    let guard = state.run_gate.clone().try_lock_owned().map_err(|_| {
        ServerError::Config("已有任务正在运行".into())
    })?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let mut run = Run {
        id: run_id.clone(),
        trigger,
        status: RunStatus::Running,
        started_at: Utc::now(),
        finished_at: None,
        stats: RunStats::default(),
        message: None,
    };

    {
        let mut p = state.progress.write();
        *p = RunProgress {
            run_id: Some(run_id.clone()),
            running: true,
            trigger: trigger.as_str().to_string(),
            started_at: Some(run.started_at.to_rfc3339()),
            ..Default::default()
        };
        p.phase = "starting".into();
    }
    state.db.create_run(&run)?;
    state.push_log("开始扫描下载器…");

    let cancel = CancellationToken::new();
    *state.cancel.write() = Some(cancel.clone());
    let _running = RunningGuard::new(state.clone());
    state.is_running.store(true, Ordering::SeqCst);

    // Ensure an interrupted previous run's rows don't linger as "running".
    let _ = state.db.reconcile_orphan_runs();

    // Deliver the result to the caller even on the error path.
    let result = run_inner(state, &run_id, &cancel).await;

    *state.cancel.write() = None;

    match result {
        Ok(outcome) => {
            run.stats = outcome.stats.clone();
            run.status = if outcome.cancelled {
                RunStatus::Cancelled
            } else {
                RunStatus::Completed
            };
            run.finished_at = Some(Utc::now());
            run.message = if outcome.warnings.is_empty() {
                None
            } else {
                Some(outcome.warnings.join("；"))
            };

            state.db.finish_run(
                &run_id,
                run.status,
                &run.stats,
                run.message.as_deref(),
            )?;

            {
                let mut p = state.progress.write();
                p.running = false;
                p.finished_at = run.finished_at.map(|d| d.to_rfc3339());
                p.stats = run.stats.clone();
                p.phase = if outcome.cancelled {
                    "cancelled".into()
                } else {
                    "done".into()
                };
            }

                    state.push_log(format!(
                "完成：新增 {}，已存在 {}，失败 {}（校验不通过 {}）",
                run.stats.added,
                run.stats.existed,
                run.stats.fetch_failed + run.stats.add_failed,
                run.stats.verify_failed
            ));
            drop(guard);
            Ok(run_id)
        }
        Err(e) => {
            run.status = RunStatus::Failed;
            run.finished_at = Some(Utc::now());
            run.message = Some(e.to_string());
            let _ = state.db.finish_run(
                &run_id,
                RunStatus::Failed,
                &run.stats,
                Some(&e.to_string()),
            );
            {
                let mut p = state.progress.write();
                p.running = false;
                p.finished_at = run.finished_at.map(|d| d.to_rfc3339());
                p.phase = "failed".into();
            }
            state.push_log(format!("任务失败：{e}"));
            drop(guard);
            Err(e)
        }
    }
}

struct DiscoveryOutcome {
    stats: RunStats,
    warnings: Vec<String>,
    cancelled: bool,
}

async fn run_inner(
    state: &AppState,
    run_id: &str,
    cancel: &CancellationToken,
) -> Result<DiscoveryOutcome> {
    let ctx = build_engine(state).await?;

    {
        let mut p = state.progress.write();
        p.phase = "scanning".into();
    }

    // Progress callback: the engine calls this after each batch, and we mirror
    // the counters into shared state so the UI updates live.
    let progress_state = state.clone();
    let on_progress = move |stats: &RunStats| {
        let mut p = progress_state.progress.write();
        p.stats = stats.clone();
        p.phase = "processing".into();
    };

    let outcome = engine::discover(
        &ctx,
        &state.db,
        run_id,
        cancel,
        &on_progress,
    )
    .await?;

    for w in &outcome.warnings {
        state.push_log(format!("警告：{w}"));
    }

    // Kick verification right away so a small torrent's success shows up within
    // seconds instead of waiting for the next tick.
    if !outcome.new_attempt_ids.is_empty() {
        state.push_log(format!(
            "新增 {} 个辅种，进入校验队列",
            outcome.new_attempt_ids.len()
        ));
        let ctx = Arc::new(ctx);
        let state2 = state.clone();
        tokio::spawn(async move {
            match engine::verify_pending(&ctx, &state2.db, &CancellationToken::new()).await {
                Ok(v) => {
                    state2.push_log(format!(
                        "首次校验：通过 {}，失败 {}，等待 {}",
                        v.promoted, v.failed, v.still_waiting
                    ));
                }
                Err(e) => state2.push_log(format!("首次校验失败：{e}")),
            }
        });
    }

    Ok(DiscoveryOutcome {
        stats: outcome.stats,
        warnings: outcome.warnings,
        cancelled: outcome.cancelled,
    })
}

/// The verification loop. Runs forever; reads pending attempts from the
/// database each tick so it resumes after a restart with no special handling.
pub async fn verification_loop(state: AppState) {
    // A short initial delay: at startup the downloader may still be coming up,
    // and a failure there would log scary errors for no reason.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    loop {
        let interval = {
            let cfg = state.config.read();
            cfg.reseed.verify_interval_secs.clamp(5, 600)
        };

        // Skip while discovery is adding torrents: those adds are tracked by the
        // run itself, and polling mid-add produces half-initialized reads.
        if !state.is_running.load(Ordering::SeqCst) {
            if let Err(e) = verify_once(&state).await {
                // Connection failures are expected (downloader restart) — log at
                // debug level so the UI log doesn't fill with noise.
                tracing::debug!(error = %e, "校验巡检失败");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

async fn verify_once(state: &AppState) -> Result<()> {
    // Cheap pre-check: nothing pending means no need to build a downloader.
    let pending_ids = state.db.list_attempts(Some("added"), 1, 0)?;
    let pending_verifying = state.db.list_attempts(Some("verifying"), 1, 0)?;
    if pending_ids.is_empty() && pending_verifying.is_empty() {
        return Ok(());
    }

    let ctx = build_engine(state).await?;
    let cancel = CancellationToken::new();
    let outcome = engine::verify_pending(&ctx, &state.db, &cancel).await?;

    if outcome.promoted > 0 || outcome.failed > 0 || outcome.timed_out > 0 {
        state.push_log(format!(
            "校验巡检：通过 {}，失败 {}，超时 {}，等待 {}",
            outcome.promoted, outcome.failed, outcome.timed_out, outcome.still_waiting
        ));
    }
    Ok(())
}

/// Pending-count summary for the dashboard.
pub fn pending_counts(state: &AppState) -> Result<(i64, i64)> {
    let added = state.db.count_attempts(Some("added"))?;
    let verifying = state.db.count_attempts(Some("verifying"))?;
    Ok((added, verifying))
}
