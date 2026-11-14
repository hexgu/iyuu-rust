//! 自动清理：主种子没了，它的辅种副本也不该继续留着。
//!
//! 这是这个工具里**唯一会删东西**的路径，所以规则写得比功能更啰嗦：
//!
//! 1. 只处理 `main_missing` 的组——主种子不在下载器里。
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
use iyuu_core::models::TorrentInfo;
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
    /// 主种子缺失的组数。
    pub missing_groups: usize,
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

    for g in groups.iter().filter(|g| g.state == STATE_MAIN_MISSING) {
        report.missing_groups += 1;

        // 没有证据就没有资格删：识别不出来的一堆副本留在页面上，由用户判断。
        if g.record_count == 0 && !g.all_tagged {
            report.skipped_no_evidence += 1;
            continue;
        }
        report.candidates += 1;

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
        let delete_files = cfg.cleanup.delete_files
            && !live.is_empty()
            && !files_shared_with_other_group(g, groups);

        let mut action = CleanupAction {
            key: g.key.clone(),
            name: g.name.clone(),
            main_hash: g.main.hash.clone(),
            sites,
            hashes: all_hashes.clone(),
            deleted: false,
            delete_files,
        };

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
