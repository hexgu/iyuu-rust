//! 自动清理：主种子没了，它的辅种副本也不该继续留着。
//!
//! 这是这个工具里**唯一会删东西**的路径，所以规则写得比功能更啰嗦：
//!
//! 1. 只处理 `main_missing` 的组——主种子不在下载器里。
//! 2. 必须有证据：组内有辅种记录，或者组里所有活种子都带着辅种标签。没有任何证据的
//!    组（比如识别不出来的一堆同数据副本）不删，只在页面上展示。
//! 3. 必须有宽限期：第一次观察到缺失开始计时，`grace_hours` 之后才动手。主种子刚消失
//!    可能只是用户在换盘、重新添加。
//! 4. 种子通过下载器删（`delete_files` 决定连不连文件）；记录跟着删。库里没有任何
//!    活种子时只清记录，不去碰磁盘——应用看不到宿主机的路径，猜路径删文件比不删更糟。
//! 5. 每一次动过手都写进 `cleanup_log`，包括 dry-run。

use crate::config::AppConfig;
use crate::error::{Result, ServerError};
use crate::runner::AppState;
use crate::seeds::{self, STATE_MAIN_MISSING, SeedsInput};
use chrono::{DateTime, Duration, Utc};
use iyuu_core::downloader::Downloader;
use iyuu_core::identity::content_key;
use iyuu_core::models::TorrentInfo;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct CleanupAction {
    pub key: String,
    pub name: String,
    pub main_hash: String,
    pub sites: Vec<String>,
    pub hashes: Vec<String>,
    /// 第一次观察到主种子缺失的时间。
    pub first_seen_at: DateTime<Utc>,
    pub waited_hours: i64,
    /// 还要等多久才会动手；已经过了宽限期是 0。
    pub eta_hours: i64,
    pub deleted: bool,
    pub delete_files: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub delete_files: bool,
    pub grace_hours: u32,
    /// 下载器里的资源组总数。
    pub groups: usize,
    /// 主种子缺失的组数。
    pub missing_groups: usize,
    /// 通过证据检查、进入清理流程的组数（含还在宽限期的）。
    pub candidates: usize,
    pub deleted_groups: usize,
    pub deleted_torrents: usize,
    pub deleted_records: usize,
    pub skipped_grace: usize,
    pub skipped_no_evidence: usize,
    pub errors: Vec<String>,
    pub actions: Vec<CleanupAction>,
}

/// 跑一次清理。`dry_run=true` 只观察、写预览，不删任何东西。
///
/// # Errors
/// 下载器列表、站点目录或数据库读不出来时直接失败——清理判断依赖这三份数据，
/// 少一份就可能删错。
pub async fn run(
    state: &AppState,
    dl: &Arc<dyn Downloader>,
    dry_run: bool,
) -> Result<CleanupReport> {
    let cfg = state.config_snapshot();
    let torrents: Vec<TorrentInfo> = dl.list_torrents().await.map_err(ServerError::from)?;
    let attempts = state.db.all_attempts()?;
    let sites = state.db.load_sites()?;

    let groups = seeds::build_groups(SeedsInput {
        torrents: &torrents,
        attempts: &attempts,
        aux_tags: &cfg.reseed.aux_tags,
        sites: &sites,
        downloader: downloader_label(&cfg),
    });

    Ok(clean_once(state, dl, &cfg, &groups, dry_run, Utc::now()).await)
}

#[expect(
    clippy::too_many_lines,
    reason = "一次清理的判定、宽限期、删除与审计共享同一批派生信息，拆开只会传递十几个参数"
)]
async fn clean_once(
    state: &AppState,
    dl: &Arc<dyn Downloader>,
    cfg: &AppConfig,
    groups: &[seeds::SeedGroup],
    dry_run: bool,
    now: DateTime<Utc>,
) -> CleanupReport {
    let grace = Duration::hours(i64::from(cfg.cleanup.grace_hours));
    let mut report = CleanupReport {
        dry_run,
        delete_files: cfg.cleanup.delete_files,
        grace_hours: cfg.cleanup.grace_hours,
        groups: groups.len(),
        ..CleanupReport::default()
    };
    let mut keep: HashSet<String> = HashSet::new();

    for g in groups.iter().filter(|g| g.state == STATE_MAIN_MISSING) {
        report.missing_groups += 1;
        keep.insert(g.key.clone());

        // 没有证据就没有资格删：识别不出来的一堆副本留在页面上，由用户判断。
        if g.record_count == 0 && !g.all_tagged {
            report.skipped_no_evidence += 1;
            continue;
        }
        report.candidates += 1;

        let first_seen_at = match state.db.cleanup_touch(&g.key, &g.main.hash, &g.name, now) {
            Ok(t) => t,
            Err(e) => {
                report.errors.push(format!("{}: {e}", g.name));
                continue;
            }
        };

        let waited = now.signed_duration_since(first_seen_at);
        let eligible = waited >= grace;
        let live: Vec<&seeds::AuxSeed> = g.aux.iter().filter(|a| a.in_downloader).collect();
        let all_hashes: Vec<String> = g.aux.iter().map(|a| a.info_hash.clone()).collect();
        let live_hashes: Vec<String> = live.iter().map(|a| a.info_hash.clone()).collect();
        let sites: Vec<String> = {
            let mut v: Vec<String> = g
                .aux
                .iter()
                .map(|a| a.site_name.clone())
                .filter(|s| !s.is_empty())
                .collect();
            v.sort();
            v.dedup();
            v
        };

        // 删文件前先确认没有别的资源组共用这份数据：一个剧集种子可能就躺在
        // 季包目录里，把季包连文件删掉会顺手杀掉它。
        let shared = files_shared_with_other_group(g, groups);
        let delete_files = cfg.cleanup.delete_files && !live.is_empty() && !shared;

        let mut action = CleanupAction {
            key: g.key.clone(),
            name: g.name.clone(),
            main_hash: g.main.hash.clone(),
            sites,
            hashes: all_hashes.clone(),
            first_seen_at,
            waited_hours: waited.num_hours(),
            eta_hours: if eligible {
                0
            } else {
                (grace - waited).num_hours().max(1)
            },
            deleted: false,
            delete_files,
        };

        if !eligible {
            report.skipped_grace += 1;
            report.actions.push(action);
            continue;
        }

        if dry_run {
            action.deleted = true;
            report.deleted_groups += 1;
            report.deleted_torrents += live_hashes.len();
            report.actions.push(action);
            continue;
        }

        // 先删下载器里的种子（要连文件就一起），失败就保留记录下次再来。
        if !live_hashes.is_empty() {
            match dl.delete(&live_hashes, delete_files).await {
                Ok(()) => report.deleted_torrents += live_hashes.len(),
                Err(e) => {
                    report.errors.push(format!("{}: {e}", g.name));
                    report.actions.push(action);
                    continue;
                }
            }
        }

        match state.db.delete_attempts_for_targets(&all_hashes) {
            Ok(n) => report.deleted_records += n,
            Err(e) => report.errors.push(format!("{}: 删记录失败 {e}", g.name)),
        }

        if let Err(e) = state.db.cleanup_log_add(crate::db::CleanupLogEntry {
            group_key: &g.key,
            name: &g.name,
            main_hash: &g.main.hash,
            hashes: &all_hashes,
            sites: &action.sites,
            files_deleted: delete_files,
            dry_run: false,
        }) {
            report
                .errors
                .push(format!("{}: 写清理日志失败 {e}", g.name));
        }

        action.deleted = true;
        report.deleted_groups += 1;
        report.actions.push(action);
    }

    // dry-run 写一条汇总日志，方便对照「预览过什么」和「真删了什么」。
    if dry_run && report.deleted_groups > 0 {
        for action in report.actions.iter().filter(|a| a.deleted) {
            let _ = state.db.cleanup_log_add(crate::db::CleanupLogEntry {
                group_key: &action.key,
                name: &action.name,
                main_hash: &action.main_hash,
                hashes: &action.hashes,
                sites: &action.sites,
                files_deleted: action.delete_files,
                dry_run: true,
            });
        }
    }

    // 主种子回来了的组不再需要计时；失败/等待中的组保留原时间。
    if let Err(e) = state.db.cleanup_prune(&keep) {
        report.errors.push(format!("清理观察台账失败：{e}"));
    }
    report
}

/// 这个组要删的路径，是否也被别的组里的种子使用？
///
/// 只比较 `content_path` 的包含关系：同一个目录下的另一个种子（比如季包目录里的
/// 单集）在删文件时会被一起干掉，这种时候只删种子、不删文件。
///
/// 根目录取所有可能的候选：主种子的 `content_path`、记录快照的
/// `save_path + source_name`、以及每个副本自己的 `content_path`。任一候选与别的组
/// 有重叠就不删文件——站点改过名的副本会让这几个路径不一致，取最保守的那个。
fn files_shared_with_other_group(group: &seeds::SeedGroup, groups: &[seeds::SeedGroup]) -> bool {
    let mut roots: Vec<String> = Vec::new();
    let mut push = |path: Option<String>| {
        if let Some(path) = path.map(|p| p.trim().trim_end_matches('/').to_string())
            && !path.is_empty()
            && !roots.contains(&path)
        {
            roots.push(path);
        }
    };

    push(group.content_path.clone());
    push(content_key(&group.save_path, None, &group.name));
    for aux in &group.aux {
        push(aux.content_path.clone());
    }
    if roots.is_empty() {
        return false;
    }

    groups
        .iter()
        .filter(|g| g.key != group.key)
        .flat_map(|g| {
            std::iter::once(g.main.content_path.as_deref())
                .chain(g.aux.iter().map(|a| a.content_path.as_deref()))
        })
        .flatten()
        .any(|path| {
            let path = path.trim().trim_end_matches('/');
            roots
                .iter()
                .any(|root| path == root || path.starts_with(&format!("{root}/")))
        })
}

fn downloader_label(cfg: &AppConfig) -> &str {
    if cfg.downloader.name.trim().is_empty() {
        cfg.downloader.kind.as_str()
    } else {
        cfg.downloader.name.as_str()
    }
}
