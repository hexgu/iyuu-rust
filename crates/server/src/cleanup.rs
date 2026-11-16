//! 自动清理：主种子没了，它的辅种副本也不该继续留着。
//!
//! 这是这个工具里**唯一会删东西**的路径，所以规则写得比功能更啰嗦：
//!
//! 1. 只处理两种组：主种子不在下载器里（`main_missing`），或者主种子还在但已经被
//!    证实**一份数据都没有**（`progress == 0` 且有失败的辅种记录）。后者是「下载器
//!    的 100% 只是缓存」造成的僵尸：文件早被删了，qB 不监听文件系统，会一直宣称
//!    100%，靠「主种子在不在」永远筛不出来。
//! 2. 必须有证据：组内有辅种记录，或者组里所有活种子都带着辅种标签。没有任何证据的
//!    组（比如识别不出来的一堆同数据副本）不删，只在页面上展示。
//! 3. 不做延迟观察：一旦确认主种子缺失且有证据，下一次任务就删。宽限期会让人以为
//!    「已经清过了」，实际上还在等时间。
//! 4. 种子通过下载器删（`delete_files` 决定连不连文件）；记录跟着删。库里没有任何
//!    活种子时只清记录，不去碰磁盘——应用看不到宿主机的路径，猜路径删文件比不删更糟。
//! 5. 每一次动过手都写进 `cleanup_log`，包括 dry-run。

use crate::config::AppConfig;
use crate::error::{Result, ServerError};
use crate::runner::AppState;
use crate::seeds::{self, STATE_MAIN_MISSING, SeedsInput};
use iyuu_core::downloader::Downloader;
use iyuu_core::identity::content_key;
use iyuu_core::models::{TorrentInfo, TorrentPhase};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct CleanupAction {
    pub key: String,
    pub name: String,
    pub main_hash: String,
    pub sites: Vec<String>,
    pub hashes: Vec<String>,
    pub deleted: bool,
    pub delete_files: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub delete_files: bool,
    /// 下载器里的资源组总数。
    pub groups: usize,
    /// 主种子不在下载器里的组数。
    pub missing_groups: usize,
    /// 主种子还在下载器里、但数据已经确认不在盘上的组数。
    pub empty_groups: usize,
    /// 通过证据检查、进入清理流程的组数。
    pub candidates: usize,
    pub deleted_groups: usize,
    pub deleted_torrents: usize,
    pub deleted_records: usize,
    pub skipped_no_evidence: usize,
    pub errors: Vec<String>,
    pub actions: Vec<CleanupAction>,
}

/// 跑一次清理。`dry_run=true` 只列出会删什么，不删任何东西。
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

    Ok(clean_once(state, dl, &cfg, &groups, dry_run).await)
}

async fn clean_once(
    state: &AppState,
    dl: &Arc<dyn Downloader>,
    cfg: &AppConfig,
    groups: &[seeds::SeedGroup],
    dry_run: bool,
) -> CleanupReport {
    let mut report = CleanupReport {
        dry_run,
        delete_files: cfg.cleanup.delete_files,
        groups: groups.len(),
        ..CleanupReport::default()
    };

    for g in groups {
        // 两种「主种子已经死了」的情况处理方式一样：主种子都不该再留着。
        let main_gone = g.state == STATE_MAIN_MISSING;
        let main_empty = !main_gone && main_is_empty(g);
        if !main_gone && !main_empty {
            continue;
        }
        if main_gone {
            report.missing_groups += 1;
        } else {
            report.empty_groups += 1;
        }

        // 没有证据就没有资格删：识别不出来的一堆副本留在页面上，由用户判断。
        if g.record_count == 0 && !g.all_tagged {
            report.skipped_no_evidence += 1;
            continue;
        }
        report.candidates += 1;

        let Plan {
            mut action,
            all_hashes,
            live_hashes,
        } = plan_action(cfg, g, groups, main_empty);

        if dry_run {
            // 预览里把「会删的」算成会删，好让页面直接拿来确认。
            action.deleted = true;
            report.deleted_groups += 1;
            report.deleted_torrents += live_hashes.len();
            report.actions.push(action);
            continue;
        }

        // 先删下载器里的种子（要连文件就一起），失败就保留记录下次再来。
        if !live_hashes.is_empty() {
            match dl.delete(&live_hashes, action.delete_files).await {
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
            files_deleted: action.delete_files,
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

    report
}

/// 一次清理要删的东西：给页面/日志的动作，加上两组 hash。
struct Plan {
    action: CleanupAction,
    /// 辅种记录里的 target hash，用来删库。
    all_hashes: Vec<String>,
    /// 下载器里还活着、需要删掉的 hash。
    live_hashes: Vec<String>,
}

fn plan_action(
    cfg: &AppConfig,
    g: &seeds::SeedGroup,
    groups: &[seeds::SeedGroup],
    main_empty: bool,
) -> Plan {
    let live: Vec<&seeds::AuxSeed> = g.aux.iter().filter(|a| a.in_downloader).collect();
    let all_hashes: Vec<String> = g.aux.iter().map(|a| a.info_hash.clone()).collect();
    let mut live_hashes: Vec<String> = live.iter().map(|a| a.info_hash.clone()).collect();
    if main_empty {
        // 主种子还在列表里，数据没了。它自己也得删——否则下一轮扫描还会把它当成
        // 一个「100% 已完成」的主种子去查辅种，继续生产注定失败的候选。
        live_hashes.push(g.main.hash.clone());
    }

    let mut sites: Vec<String> = g
        .aux
        .iter()
        .map(|a| a.site_name.clone())
        .filter(|s| !s.is_empty())
        .collect();
    sites.sort();
    sites.dedup();

    // `main_empty` 时不删文件：数据已经一块都不在了，删文件没有意义，而那个路径
    // 很可能已经被别的资源组（甚至另一份拷贝）复用。
    let delete_files = cfg.cleanup.delete_files
        && !main_empty
        && !live.is_empty()
        && !files_shared_with_other_group(g, groups);

    Plan {
        action: CleanupAction {
            key: g.key.clone(),
            name: g.name.clone(),
            main_hash: g.main.hash.clone(),
            sites,
            hashes: all_hashes.clone(),
            deleted: false,
            delete_files,
        },
        all_hashes,
        live_hashes,
    }
}

/// 主种子还在下载器里，但已经被证实一份数据都没有。
///
/// 三条同时成立才算，少一条都可能误删：
///
///  1. `progress` 恰好是 0——数据一块都没对上。下了一半的种子进不来，用户正在重新
///     下载的也进不来。
///  2. 组里有失败的辅种（`counts.failed`）。这是唯一的证据来源：它说明我们真的把副本
///     加进去过，而下载器连一块都没匹配上。没有这条的组，哪怕主种子是 0% 也只说明
///     「用户在下东西」，不能碰。
///  3. 主种子不在校验 / 取元数据 / 正在传输——这些状态意味着还没有结论，等下一轮。
fn main_is_empty(g: &seeds::SeedGroup) -> bool {
    if !g.main.present || g.main.progress > 0.0 || g.counts.failed == 0 {
        return false;
    }
    if matches!(
        g.main.phase,
        TorrentPhase::Checking | TorrentPhase::DownloadingMetadata | TorrentPhase::Complete
    ) {
        return false;
    }
    let state = g.main.state.to_ascii_lowercase();
    !matches!(
        state.as_str(),
        "downloading" | "allocating" | "moving" | "forceddl" | "queueddl"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn main_seed(progress: f64, state: &str, phase: TorrentPhase) -> seeds::MainSeed {
        seeds::MainSeed {
            hash: "main".into(),
            name: "release".into(),
            size: 100,
            save_path: "/data".into(),
            content_path: Some("/data/release".into()),
            state: state.into(),
            phase,
            progress,
            seeding_time: Some(0),
            uploaded: 0,
            downloaded: 0,
            ratio: 0.0,
            added_on: None,
            tags: vec![],
            trackers: vec![],
            downloader: "qb".into(),
            present: true,
        }
    }

    fn group(progress: f64, state: &str, phase: TorrentPhase, failed: usize) -> seeds::SeedGroup {
        seeds::SeedGroup {
            key: "main".into(),
            name: "release".into(),
            size: 100,
            save_path: "/data".into(),
            content_path: Some("/data/release".into()),
            category: None,
            state: seeds::STATE_DEGRADED,
            main_source: seeds::MAIN_RECORD,
            main: main_seed(progress, state, phase),
            aux: vec![],
            counts: seeds::GroupCounts {
                failed,
                ..seeds::GroupCounts::default()
            },
            updated_at: None,
            downloaders: vec!["qb".into()],
            record_count: failed,
            all_tagged: false,
        }
    }

    #[test]
    fn an_empty_main_with_failed_aux_is_a_zombie() {
        // 文件早被删了，qB 只是还留着 .fastresume 里的位图。
        let g = group(0.0, "stoppedDL", TorrentPhase::Incomplete, 1);
        assert!(main_is_empty(&g));
    }

    #[test]
    fn a_complete_main_is_never_a_zombie() {
        let g = group(1.0, "stalledUP", TorrentPhase::Complete, 1);
        assert!(!main_is_empty(&g));
    }

    #[test]
    fn a_zero_main_without_failed_aux_is_left_alone() {
        // 用户在正常下载：0% 但没有辅种失败的证据，碰都不能碰。
        let g = group(0.0, "stalledDL", TorrentPhase::Incomplete, 0);
        assert!(!main_is_empty(&g));
    }

    #[test]
    fn a_half_downloaded_main_is_left_alone() {
        let g = group(0.42, "stalledDL", TorrentPhase::Incomplete, 1);
        assert!(!main_is_empty(&g));
    }

    #[test]
    fn a_main_still_checking_is_left_alone() {
        let g = group(0.0, "checkingDL", TorrentPhase::Checking, 1);
        assert!(!main_is_empty(&g));
    }

    #[test]
    fn a_main_actively_downloading_is_left_alone() {
        let g = group(0.0, "downloading", TorrentPhase::Incomplete, 1);
        assert!(!main_is_empty(&g));
    }

    #[test]
    fn an_absent_main_is_handled_by_the_missing_branch() {
        let mut g = group(0.0, "stoppedDL", TorrentPhase::Incomplete, 1);
        g.main.present = false;
        assert!(!main_is_empty(&g));
    }
}
