//! The cross-seed engine.
//!
//! Two separable phases, on purpose:
//!
//! **Discovery** (`discover`) — scan the downloader, filter locally, ask IYUU
//! which releases exist elsewhere, fetch the .torrent, add it paused-for-verify.
//! Every attempt is persisted *before* the network call that follows it, so a
//! crash mid-run leaves a resumable record rather than a stranded torrent.
//!
//! **Verification** (`verify_pending`) — poll the downloader for every attempt
//! still in `Added`/`Verifying` and drive it to a terminal state. This is the
//! piece the MoviePilot plugin is missing: it keeps its queue in memory, so a
//! restart strands every in-flight cross-seed in `paused` forever, and it has no
//! branch for `checkingDL` so it never learns that verification is still running.
//!
//! Splitting them means verification keeps working even when discovery is
//! disabled or IYUU is down, and that a restart resumes rather than loses.

use crate::bencode;
use crate::downloader::qbittorrent::{IYUU_TAG, site_tag};
use crate::downloader::{AddTorrentOptions, Downloader};
use crate::error::Result;
use crate::fetch::fetch_torrent;
use crate::iyuu::{HASH_CHUNK_SIZE, IyuuClient, SiteCredentials};
use crate::models::*;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Filtering rules applied to source torrents before any network work.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Minimum torrent size in GiB. 0 disables the filter.
    pub min_size_gib: f64,
    /// Source torrents carrying any of these tags/categories are skipped.
    pub exclude_tags: Vec<String>,
    /// Save-path prefixes to skip (e.g. a scratch download disk).
    pub exclude_paths: Vec<String>,
    /// Restrict seeding to these site ids. Empty = every bound site.
    pub site_ids: Vec<i64>,
    /// Reuse the source torrent's category on the cross-seed.
    pub reuse_category: bool,
    /// Category to apply when not reusing (and as fallback when the source has
    /// none). `None` = leave empty.
    pub default_category: Option<String>,
    /// Extra tags for every added cross-seed.
    pub tags: Vec<String>,
    /// Add a tag naming the target site, handy for per-site filtering in the UI.
    pub tag_with_site: bool,
    /// Also start the torrent after verification completes. When false the
    /// torrent is left complete-but-stopped for the operator to inspect.
    pub auto_start_after_verify: bool,
    /// Concurrency for the .torrent fetch step. Sites rate-limit, so this is
    /// deliberately small by default.
    pub fetch_concurrency: usize,
    /// Dry run: do everything except add to the downloader.
    pub dry_run: bool,
}

impl Default for DiscoveryConfig {
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
            dry_run: false,
        }
    }
}

/// Per-site credentials keyed by IYUU site id.
pub type CredentialMap = HashMap<i64, SiteCredentials>;

/// Everything the engine needs that isn't per-run.
pub struct EngineContext {
    pub iyuu: Arc<IyuuClient>,
    pub downloader: Arc<dyn Downloader>,
    pub http: reqwest::Client,
    pub config: DiscoveryConfig,
    pub credentials: CredentialMap,
    /// Hashes that previously failed to fetch — skip them so a dead release
    /// doesn't get retried on every scheduled run.
    pub blocked_hashes: HashSet<String>,
}

/// Persistence hooks. Implemented by the server over SQLite; the engine never
/// touches the database directly so it stays testable.
#[async_trait::async_trait]
pub trait AttemptStore: Send + Sync {
    async fn upsert_attempt(&self, attempt: &mut ReseedAttempt) -> Result<i64>;
    async fn update_state(&self, id: i64, state: ReseedState, message: Option<&str>) -> Result<()>;
    async fn update_check(
        &self,
        id: i64,
        state: ReseedState,
        check_count: i32,
        message: Option<&str>,
    ) -> Result<()>;
    /// Attempts still awaiting verification, oldest first.
    async fn pending_attempts(&self) -> Result<Vec<ReseedAttempt>>;
    /// Hashes we have already cross-seeded successfully, to avoid re-adding.
    async fn seeded_targets(&self) -> Result<HashSet<String>>;
    /// Hashes that terminally failed, so we don't retry them next run.
    async fn known_bad_targets(&self) -> Result<HashSet<String>>;
}

/// Outcome of a discovery run.
#[derive(Debug, Default)]
pub struct DiscoveryOutcome {
    pub stats: RunStats,
    pub warnings: Vec<String>,
    /// Attempt ids created, so the caller can kick verification immediately.
    pub new_attempt_ids: Vec<i64>,
    pub cancelled: bool,
}

/// Run discovery against the configured downloader.
pub async fn discover(
    ctx: &EngineContext,
    store: &dyn AttemptStore,
    run_id: &str,
    cancel: &tokio_util::sync::CancellationToken,
    progress: &(dyn Fn(&RunStats) + Send + Sync),
) -> Result<DiscoveryOutcome> {
    let mut out = DiscoveryOutcome::default();
    let mut stats = RunStats::default();

    // ---------------------------------------------------------------- scan
    let torrents = ctx.downloader.list_torrents().await?;
    stats.scanned = torrents.len() as i64;

    // The set of hashes already present, so we never add a duplicate. One
    // snapshot is enough: an add during this run cannot collide with a later
    // add of the same target because we dedupe candidates by hash below.
    let present: HashSet<String> = torrents.iter().map(|t| t.hash.clone()).collect();

    let known_bad = store.known_bad_targets().await?;
    let already_seeded = store.seeded_targets().await?;

    // ------------------------------------------------------- local filters
    let mut sources: Vec<&TorrentInfo> = Vec::new();
    for t in &torrents {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        match filter_source(t, &ctx.config) {
            Ok(()) => sources.push(t),
            Err(reason) => {
                tracing::debug!(hash = %t.hash, name = %t.name, reason, "跳过源种子");
                stats.skipped += 1;
            }
        }
    }

    tracing::info!(
        total = stats.scanned,
        candidates = sources.len(),
        "扫描完成"
    );

    // ------------------------------------------------------- IYUU query
    let site_map = ctx.iyuu.sites_by_id().await?;
    let sid_list: Vec<i64> = if ctx.config.site_ids.is_empty() {
        // Report everything we have credentials for, plus every site IYUU knows:
        // a narrower list would filter out results we then have to re-query for.
        site_map.keys().copied().collect()
    } else {
        ctx.config.site_ids.clone()
    };

    let source_hashes: Vec<String> = sources.iter().map(|t| t.hash.clone()).collect();
    let (found, warnings) = ctx.iyuu.reseed_query_all(&source_hashes, &sid_list).await?;
    out.warnings = warnings;
    stats.queried = source_hashes.len() as i64;
    stats.candidates = found.values().map(|v| v.len() as i64).sum();
    progress(&stats);

    // ------------------------------------------------- flatten + dedupe
    // One entry per (source, candidate). A candidate hash can appear under
    // several sources only if two source torrents share a hash, which cannot
    // happen, so keying by target hash alone is safe and prevents double-adds.
    let mut jobs: Vec<Job> = Vec::new();
    let mut seen_targets: HashSet<String> = HashSet::new();
    let source_by_hash: HashMap<&str, &TorrentInfo> =
        sources.iter().map(|t| (t.hash.as_str(), *t)).collect();

    for (source_hash, candidates) in &found {
        let Some(source) = source_by_hash.get(source_hash.as_str()) else {
            continue;
        };
        for cand in candidates {
            let target = bencode::normalize_hash(&cand.info_hash);
            if target.is_empty() {
                continue;
            }
            if present.contains(&target) || already_seeded.contains(&target) {
                stats.existed += 1;
                continue;
            }
            if known_bad.contains(&target) || ctx.blocked_hashes.contains(&target) {
                stats.skipped += 1;
                continue;
            }
            if let Some(restrict) = site_filter(&ctx.config, cand.sid)
                && !restrict
            {
                stats.skipped += 1;
                continue;
            }
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            let Some(site) = site_map.get(&cand.sid) else {
                // IYUU returned a sid we have no metadata for; cannot build a
                // download URL without it.
                out.warnings
                    .push(format!("未知站点 sid={}，跳过 {} 个候选", cand.sid, 1));
                stats.skipped += 1;
                continue;
            };
            jobs.push(Job {
                source_name: source.name.clone(),
                source_hash: source.hash.clone(),
                save_path: source.save_path.clone(),
                category: category_for(source, &ctx.config),
                candidate: cand.clone(),
                site: site.clone(),
            });
        }
    }

    tracing::info!(jobs = jobs.len(), "待处理辅种任务");

    // ------------------------------------------------- fetch + add (bounded)
    //
    // Processed in chunks of `fetch_concurrency` with `join_all`. An earlier
    // version kept a Vec of JoinHandles and `pop()`ed one per iteration, which
    // awaited the *newest* task each time: the oldest jobs then piled up and only
    // resolved at the end, so progress reporting lagged and cancellation took
    // effect late. Chunking makes the in-flight set exact.
    //
    // The context is shared via one Arc rather than cloned per job — the
    // credential map can hold dozens of sites and cloning it per candidate was
    // pure waste.
    let ctx = Arc::new(clone_ctx(ctx));
    let window = ctx.config.fetch_concurrency.clamp(1, 16);
    let total_jobs = jobs.len();

    for chunk in jobs.chunks(window) {
        if cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }

        let futures: Vec<_> = chunk
            .iter()
            .map(|job| {
                let ctx = ctx.clone();
                let run_id = run_id.to_string();
                let site_map = &site_map;
                async move {
                    let result = process_job(&ctx, job, site_map, &run_id).await;
                    (job, result)
                }
            })
            .collect();

        for (job, result) in futures::future::join_all(futures).await {
            apply_job_result(&mut stats, &mut out, store, job, result).await;
        }

        progress(&stats);
        tracing::debug!(
            done = stats.added + stats.fetch_failed + stats.add_failed + stats.existed,
            total = total_jobs,
            "辅种进度"
        );
    }

    out.stats = stats;
    Ok(out)
}

/// Snapshot the context for one discovery run.
///
/// Called once per run, not once per job: the credential map and the blocked-hash
/// set can both be large, and every job only needs to read them.
fn clone_ctx(ctx: &EngineContext) -> EngineContext {
    EngineContext {
        iyuu: ctx.iyuu.clone(),
        downloader: ctx.downloader.clone(),
        http: ctx.http.clone(),
        config: ctx.config.clone(),
        credentials: ctx.credentials.clone(),
        blocked_hashes: ctx.blocked_hashes.clone(),
    }
}

/// A single (source torrent → target site release) work item.
#[derive(Debug, Clone)]
pub struct Job {
    /// Name of the torrent already in the downloader; used for display and as
    /// the human-readable anchor when diagnosing a failure.
    pub source_name: String,
    pub source_hash: String,
    /// Verbatim save path of the source. Reused so verification finds the bytes
    /// already on disk instead of re-downloading them.
    pub save_path: String,
    pub category: Option<String>,
    pub candidate: SeedCandidate,
    pub site: IyuuSite,
}

/// Why a source torrent was rejected locally. Kept as a string for logging.
fn filter_source(t: &TorrentInfo, cfg: &DiscoveryConfig) -> std::result::Result<(), &'static str> {
    // Must be verified complete: an incomplete source has no data to reuse.
    if t.progress < 0.999 {
        return Err("未完成下载");
    }
    // A torrent already being verified is not a valid source yet.
    if matches!(t.phase(), TorrentPhase::Checking) {
        return Err("正在校验");
    }
    if matches!(t.phase(), TorrentPhase::Error) {
        return Err("下载器报告错误");
    }
    if matches!(t.phase(), TorrentPhase::MissingFiles) {
        return Err("文件缺失");
    }

    if cfg.min_size_gib > 0.0 {
        let gib = t.total_size as f64 / 1024.0 / 1024.0 / 1024.0;
        if gib < cfg.min_size_gib {
            return Err("体积小于阈值");
        }
    }

    // Skip our own cross-seeds: seeding them again from themselves would be a
    // no-op at best and a duplicate-add loop at worst.
    if t.tags
        .iter()
        .any(|tag| tag == IYUU_TAG || tag.starts_with(&format!("{IYUU_TAG}:")))
    {
        return Err("本身即为辅种");
    }
    for excluded in &cfg.exclude_tags {
        if t.tags.iter().any(|tag| tag == excluded) {
            return Err("命中排除标签");
        }
        if t.category.as_deref() == Some(excluded.as_str()) {
            return Err("命中排除分类");
        }
    }
    for prefix in &cfg.exclude_paths {
        if !prefix.is_empty() && t.save_path.starts_with(prefix) {
            return Err("命中排除路径");
        }
    }
    Ok(())
}

/// `Some(true)` = allowed, `Some(false)` = excluded, `None` = no restriction.
fn site_filter(cfg: &DiscoveryConfig, sid: i64) -> Option<bool> {
    if cfg.site_ids.is_empty() {
        None
    } else {
        Some(cfg.site_ids.contains(&sid))
    }
}

fn category_for(source: &TorrentInfo, cfg: &DiscoveryConfig) -> Option<String> {
    if cfg.reuse_category {
        source
            .category
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| cfg.default_category.clone())
    } else {
        cfg.default_category.clone()
    }
}

struct JobResult {
    attempt: Option<ReseedAttempt>,
    outcome: JobOutcome,
    message: Option<String>,
}

enum JobOutcome {
    Added,
    FetchFailed,
    AddFailed,
    Skipped,
    DryRun,
}

async fn process_job(
    ctx: &EngineContext,
    job: &Job,
    site_map: &HashMap<i64, IyuuSite>,
    run_id: &str,
) -> JobResult {
    let cand = &job.candidate;
    let site = site_map
        .get(&cand.sid)
        .cloned()
        .unwrap_or_else(|| job.site.clone());
    let creds = ctx.credentials.get(&cand.sid).cloned().unwrap_or_default();
    let target = bencode::normalize_hash(&cand.info_hash);

    // Cheap pre-check: another worker may already have added this hash. qB
    // returns "Fails." for duplicates, which we would otherwise log as an error.
    match ctx.downloader.torrent(&target).await {
        Ok(Some(_)) => {
            return JobResult {
                attempt: None,
                outcome: JobOutcome::Skipped,
                message: Some("已在下载器中".into()),
            };
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(error = %e, "预检查失败，继续尝试添加");
        }
    }

    let fetched =
        match fetch_torrent(&ctx.http, &site, cand.torrent_id, &creds, Some(&target)).await {
            Ok(f) => f,
            Err(e) => {
                return JobResult {
                    attempt: None,
                    outcome: JobOutcome::FetchFailed,
                    message: Some(format!("{}: {e}", site.display_name())),
                };
            }
        };

    if ctx.config.dry_run {
        let mut attempt = new_attempt(job, &site, &target, run_id);
        attempt.state = ReseedState::Skipped;
        attempt.message = Some(format!("dry-run：可添加 {}", fetched.info_hash));
        return JobResult {
            attempt: Some(attempt),
            outcome: JobOutcome::DryRun,
            message: Some(format!("dry-run 命中 {}", site.display_name())),
        };
    }

    let mut tags = ctx.config.tags.clone();
    tags.push(IYUU_TAG.to_string());
    if ctx.config.tag_with_site {
        tags.push(site_tag(&site.site));
    }

    let opts = AddTorrentOptions {
        save_path: Some(job.save_path.clone()).filter(|s| !s.is_empty()),
        category: job.category.clone(),
        tags,
        paused: true,
        // Never skip verification: with a mismatched save path qB would
        // re-download the entire payload from 0% and destroy the ratio this
        // whole tool exists to protect.
        skip_checking: false,
        force_reuse_save_path: true,
    };

    match ctx.downloader.add_torrent(&fetched.bytes, &opts).await {
        Ok(hash) => {
            let mut attempt = new_attempt(job, &site, &hash, run_id);
            attempt.state = ReseedState::Added;
            attempt.added_at = Some(Utc::now());
            JobResult {
                attempt: Some(attempt),
                outcome: JobOutcome::Added,
                message: Some(fetched.url),
            }
        }
        Err(e) => {
            let mut attempt = new_attempt(job, &site, &target, run_id);
            attempt.state = ReseedState::AddFailed;
            attempt.message = Some(e.to_string());
            JobResult {
                attempt: Some(attempt),
                outcome: JobOutcome::AddFailed,
                message: Some(e.to_string()),
            }
        }
    }
}

fn new_attempt(job: &Job, site: &IyuuSite, target_hash: &str, run_id: &str) -> ReseedAttempt {
    ReseedAttempt {
        id: 0,
        source_hash: job.source_hash.clone(),
        source_name: job.source_name.clone(),
        save_path: job.save_path.clone(),
        target_info_hash: target_hash.to_string(),
        target_sid: site.id,
        target_site: site.display_name().to_string(),
        target_torrent_id: job.candidate.torrent_id,
        state: ReseedState::Pending,
        check_count: 0,
        last_checked_at: None,
        verify_started_at: None,
        added_at: None,
        finished_at: None,
        message: None,
        downloader: String::new(),
        run_id: Some(run_id.to_string()),
    }
}

async fn apply_job_result(
    stats: &mut RunStats,
    out: &mut DiscoveryOutcome,
    store: &dyn AttemptStore,
    job: &Job,
    result: JobResult,
) {
    match result.outcome {
        JobOutcome::Added => {
            stats.added += 1;
        }
        JobOutcome::FetchFailed => {
            stats.fetch_failed += 1;
            if let Some(msg) = &result.message {
                tracing::warn!(source = %job.source_name, %msg, "获取种子失败");
            }
        }
        JobOutcome::AddFailed => {
            stats.add_failed += 1;
        }
        JobOutcome::Skipped => {
            stats.existed += 1;
        }
        JobOutcome::DryRun => {
            stats.added += 1;
        }
    }

    if let Some(mut attempt) = result.attempt {
        match store.upsert_attempt(&mut attempt).await {
            Ok(id) => {
                if matches!(attempt.state, ReseedState::Added | ReseedState::Verifying) {
                    out.new_attempt_ids.push(id);
                }
            }
            Err(e) => out.warnings.push(format!("保存辅种记录失败：{e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Verification phase
// ---------------------------------------------------------------------------

/// One verification sweep.
#[derive(Debug, Default)]
pub struct VerifyOutcome {
    pub checked: usize,
    pub promoted: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub still_waiting: usize,
}

/// Poll every pending attempt and advance its state machine.
///
/// Timing rules that make this correct rather than merely plausible:
///  * While the client reports `checking*`, the attempt stays `Verifying` and is
///    never counted as success or failure. (The plugin's missing branch is the
///    reason torrents sit in `paused` forever: it only checks "can seed yet?",
///    sees no, and gives up.)
///  * `progress == 1.0` outside checking == verified. Promote to `Seeding` and
///    start it.
///  * Verification finished but `progress < 1.0` == wrong save path or wrong
///    release. Terminal `VerifyFailed`, with an actionable message.
///  * `missingFiles` == the client lost the data. Terminal.
///  * A torrent that has not resolved within `VERIFY_DEADLINE_SECS` is marked
///    `TimedOut` so it stops consuming attention — but it is NOT deleted, since
///    an operator may still want to see it.
pub async fn verify_pending(
    ctx: &EngineContext,
    store: &dyn AttemptStore,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<VerifyOutcome> {
    let mut out = VerifyOutcome::default();
    let pending = store.pending_attempts().await?;
    if pending.is_empty() {
        return Ok(out);
    }

    // One batched lookup instead of one request per torrent.
    let hashes: Vec<String> = pending.iter().map(|a| a.target_info_hash.clone()).collect();

    // Clients cap batch sizes; stay well under any of them.
    for chunk in hashes.chunks(200) {
        if cancel.is_cancelled() {
            return Ok(out);
        }
        let found: HashMap<String, TorrentInfo> = ctx
            .downloader
            .torrents_by_hash(chunk)
            .await?
            .into_iter()
            .map(|t| (t.hash.clone(), t))
            .collect();

        for attempt in pending
            .iter()
            .filter(|a| chunk.contains(&a.target_info_hash))
        {
            out.checked += 1;
            let Some(info) = found.get(&attempt.target_info_hash) else {
                // The torrent vanished: someone deleted it, or the add silently
                // failed. Treat as timed out rather than retrying forever.
                out.timed_out += 1;
                store
                    .update_state(
                        attempt.id,
                        ReseedState::TimedOut,
                        Some("下载器中找不到该种子，可能已被删除"),
                    )
                    .await?;
                continue;
            };

            let deadline_exceeded = attempt
                .verify_started_at
                .or(attempt.added_at)
                .map(|start| {
                    (Utc::now() - start).num_seconds() > ReseedAttempt::VERIFY_DEADLINE_SECS
                })
                .unwrap_or(false);

            match info.phase() {
                TorrentPhase::Checking => {
                    if deadline_exceeded {
                        out.timed_out += 1;
                        store
                            .update_check(
                                attempt.id,
                                ReseedState::TimedOut,
                                attempt.check_count + 1,
                                Some("校验超时，请检查磁盘或手动重新校验"),
                            )
                            .await?;
                    } else {
                        out.still_waiting += 1;
                        store
                            .update_check(
                                attempt.id,
                                ReseedState::Verifying,
                                attempt.check_count + 1,
                                Some(&format!("校验中：{}", info.state)),
                            )
                            .await?;
                    }
                }
                TorrentPhase::Complete if info.progress >= 0.999 => {
                    // Success. Only now can we say the cross-seed works.
                    out.promoted += 1;
                    if ctx.config.auto_start_after_verify
                        && let Err(e) = ctx
                            .downloader
                            .start(std::slice::from_ref(&attempt.target_info_hash))
                            .await
                    {
                        tracing::warn!(hash = %attempt.target_info_hash, error = %e, "启动辅种失败");
                    }
                    if let Err(e) = ctx
                        .downloader
                        .add_tags(
                            std::slice::from_ref(&attempt.target_info_hash),
                            &[format!("{IYUU_TAG}:verified")],
                        )
                        .await
                    {
                        tracing::debug!(error = %e, "打标签失败");
                    }
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::Seeding,
                            attempt.check_count + 1,
                            Some("校验通过，开始辅种"),
                        )
                        .await?;
                }
                TorrentPhase::MissingFiles => {
                    out.failed += 1;
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::MissingFiles,
                            attempt.check_count + 1,
                            Some("下载器报告文件缺失：保存路径与原种不一致"),
                        )
                        .await?;
                }
                TorrentPhase::Error => {
                    out.failed += 1;
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::VerifyFailed,
                            attempt.check_count + 1,
                            Some(&format!(
                                "下载器错误：{}",
                                info.error.as_deref().unwrap_or(&info.state)
                            )),
                        )
                        .await?;
                }
                TorrentPhase::Incomplete if deadline_exceeded => {
                    out.timed_out += 1;
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::TimedOut,
                            attempt.check_count + 1,
                            Some("超时仍未完成校验"),
                        )
                        .await?;
                }
                TorrentPhase::Incomplete => {
                    // Two very different situations share this state:
                    //  (a) verification just finished and found data missing, or
                    //  (b) a verify was requested and has not started yet.
                    // The client tells us which via the state string. A stopped
                    // torrent at 0% that we already asked to verify is (b).
                    let lower = info.state.to_ascii_lowercase();
                    let verify_pending_state = lower.contains("paused")
                        || lower.contains("stopped")
                        || lower.contains("queued")
                        || lower.contains("stoppeddl");

                    if verify_pending_state && attempt.check_count <= 3 {
                        // Give it a nudge: without this, a qB that ignored the
                        // recheck request sits at 0% forever.
                        if attempt.check_count == 0
                            && let Err(e) = ctx
                                .downloader
                                .recheck(std::slice::from_ref(&attempt.target_info_hash))
                                .await
                        {
                            tracing::debug!(error = %e, "补发校验失败");
                        }
                        out.still_waiting += 1;
                        store
                            .update_check(
                                attempt.id,
                                ReseedState::Verifying,
                                attempt.check_count + 1,
                                Some("等待校验开始"),
                            )
                            .await?;
                    } else {
                        // Verification had its chance and found the data
                        // incomplete. This is the failure the user must see.
                        out.failed += 1;
                        store
                            .update_check(
                                attempt.id,
                                ReseedState::VerifyFailed,
                                attempt.check_count + 1,
                                Some(&format!(
                                    "校验后仍不完整（进度 {:.1}%）：保存路径与原种不一致，或该站资源与本地不同",
                                    info.progress * 100.0
                                )),
                            )
                            .await?;
                    }
                }
                TorrentPhase::DownloadingMetadata => {
                    out.still_waiting += 1;
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::Verifying,
                            attempt.check_count + 1,
                            Some("等待元数据"),
                        )
                        .await?;
                }
                TorrentPhase::Complete => {
                    // Complete but progress < 0.999 — inconsistent report.
                    out.still_waiting += 1;
                }
                TorrentPhase::Unknown => {
                    out.still_waiting += 1;
                    store
                        .update_check(
                            attempt.id,
                            ReseedState::Verifying,
                            attempt.check_count + 1,
                            Some(&format!("未知状态：{}", info.state)),
                        )
                        .await?;
                }
            }
        }
    }

    Ok(out)
}

/// Sleep helper matching the engine's polling cadence.
pub fn verify_tick() -> Duration {
    Duration::from_secs(15)
}

/// Number of candidates in a batch that the API accepts.
pub const MAX_HASHES_PER_QUERY: usize = HASH_CHUNK_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(hash: &str, progress: f64, state: &str) -> TorrentInfo {
        TorrentInfo {
            hash: hash.to_string(),
            name: "t".into(),
            save_path: "/data".into(),
            content_path: None,
            total_size: 10 * 1024 * 1024 * 1024,
            progress,
            state: state.into(),
            kind: DownloaderKind::Qbittorrent,
            category: None,
            tags: vec![],
            added_on: None,
            seeding_time: Some(0),
            uploaded: 0,
            ratio: 0.0,
            error: None,
            trackers: vec![],
        }
    }

    #[test]
    fn rejects_incomplete_sources() {
        let t = torrent("a", 0.5, "downloading");
        assert_eq!(
            filter_source(&t, &DiscoveryConfig::default()),
            Err("未完成下载")
        );
    }

    #[test]
    fn rejects_checking_sources() {
        let t = torrent("a", 1.0, "checkingDL");
        assert_eq!(
            filter_source(&t, &DiscoveryConfig::default()),
            Err("正在校验")
        );
    }

    #[test]
    fn rejects_our_own_cross_seeds() {
        let mut t = torrent("a", 1.0, "uploading");
        t.tags = vec![IYUU_TAG.to_string()];
        assert_eq!(
            filter_source(&t, &DiscoveryConfig::default()),
            Err("本身即为辅种")
        );

        // Site-suffixed variant must also be recognized.
        let mut t2 = torrent("b", 1.0, "uploading");
        t2.tags = vec![site_tag("springsunday")];
        assert_eq!(
            filter_source(&t2, &DiscoveryConfig::default()),
            Err("本身即为辅种")
        );
    }

    #[test]
    fn accepts_healthy_seed() {
        let t = torrent("a", 1.0, "uploading");
        assert!(filter_source(&t, &DiscoveryConfig::default()).is_ok());
    }

    #[test]
    fn size_filter_applies() {
        let t = torrent("a", 1.0, "uploading");
        let cfg = DiscoveryConfig {
            min_size_gib: 20.0,
            ..Default::default()
        };
        assert_eq!(filter_source(&t, &cfg), Err("体积小于阈值"));
    }

    #[test]
    fn excluded_paths_and_tags() {
        let mut t = torrent("a", 1.0, "uploading");
        t.save_path = "/tmp/scratch/x".into();
        let cfg = DiscoveryConfig {
            exclude_paths: vec!["/tmp/scratch".into()],
            ..Default::default()
        };
        assert_eq!(filter_source(&t, &cfg), Err("命中排除路径"));

        let mut t2 = torrent("b", 1.0, "uploading");
        t2.tags = vec!["noadd".into()];
        let cfg2 = DiscoveryConfig {
            exclude_tags: vec!["noadd".into()],
            ..Default::default()
        };
        assert_eq!(filter_source(&t2, &cfg2), Err("命中排除标签"));
    }

    #[test]
    fn reuse_category_falls_back_to_default() {
        let t = torrent("a", 1.0, "uploading");
        let cfg = DiscoveryConfig {
            reuse_category: true,
            default_category: Some("pt".into()),
            ..Default::default()
        };
        assert_eq!(category_for(&t, &cfg).as_deref(), Some("pt"));

        let mut with_cat = t.clone();
        with_cat.category = Some("movies".into());
        assert_eq!(category_for(&with_cat, &cfg).as_deref(), Some("movies"));

        let cfg2 = DiscoveryConfig {
            reuse_category: false,
            default_category: Some("pt".into()),
            ..Default::default()
        };
        assert_eq!(category_for(&with_cat, &cfg2).as_deref(), Some("pt"));
    }
}
