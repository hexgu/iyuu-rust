//! 「种子管理」视图：把下载器快照与辅种记录聚成「主种子 + 辅种子」。
//!
//! 这里只做聚合，不做持久化。分组是**推导**出来的：下载器里种子一删、辅种记录一改，
//! 下次请求自然就是新的结果。把组写进表里等于给自己造一份会过期的副本。
//!
//! 识别规则在 `iyuu_core::identity`，本模块只负责把这些规则用在两条数据源上：
//! 辅种记录给的 `source_hash ↔ target_info_hash` 是最强的事实，下载器里同数据身份的
//! 两个种子是补充证据。主种子被删掉之后，组仍然存在——成员是记录，不是下载器条目。

use chrono::{DateTime, Utc};
use iyuu_core::identity::same_release;
use iyuu_core::models::{ReseedAttempt, ReseedState, TorrentInfo, TorrentPhase};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// 辅种种子身上的标签前缀。带这个前缀的种子不是「用户自己下的那一份」。
pub const AUX_TAG: &str = "iyuu-reseed";

pub const STATE_HEALTHY: &str = "healthy";
pub const STATE_VERIFYING: &str = "verifying";
pub const STATE_DEGRADED: &str = "degraded";
pub const STATE_MAIN_MISSING: &str = "main_missing";
pub const STATE_NO_AUX: &str = "no_aux";

/// 组的整体状态，数字越小越需要用户先看。
fn state_rank(state: &str) -> u8 {
    match state {
        STATE_MAIN_MISSING => 0,
        STATE_DEGRADED => 1,
        STATE_VERIFYING => 2,
        STATE_NO_AUX => 3,
        _ => 4,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SeedGroup {
    /// 组标识：主种子的 hash；主种子缺失时取组内最小的 hash，保证刷新之间稳定。
    pub key: String,
    pub name: String,
    pub size: i64,
    pub save_path: String,
    pub category: Option<String>,
    pub state: &'static str,
    pub main: MainSeed,
    pub aux: Vec<AuxSeed>,
    /// 同一份数据的多余副本：既不是主种子，也不是某条辅种记录的目标。
    pub duplicates: Vec<DuplicateSeed>,
    pub counts: GroupCounts,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MainSeed {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub save_path: String,
    pub state: String,
    pub phase: TorrentPhase,
    pub progress: f64,
    pub seeding_time: Option<i64>,
    pub uploaded: i64,
    pub ratio: f64,
    pub added_on: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub trackers: Vec<String>,
    /// false = 下载器里已经找不到它了，字段来自最后的记录快照。
    pub present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxSeed {
    pub attempt_id: i64,
    pub site: String,
    pub site_name: String,
    pub sid: i64,
    pub torrent_id: i64,
    pub info_hash: String,
    /// 记录里的状态（`added` / `seeding` / `verify_failed` …）。
    pub state: String,
    pub check_count: i32,
    pub added_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
    pub in_downloader: bool,
    /// 下载器里的实时状态；种子不在了就是 null。
    pub live: Option<AuxLive>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxLive {
    pub state: String,
    pub phase: TorrentPhase,
    pub progress: f64,
    pub seeding_time: Option<i64>,
    pub uploaded: i64,
    pub ratio: f64,
    pub added_on: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateSeed {
    pub hash: String,
    pub name: String,
    pub uploaded: i64,
    pub added_on: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct GroupCounts {
    pub aux: usize,
    pub seeding: usize,
    pub verifying: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct SeedCounts {
    pub groups: usize,
    pub aux_total: usize,
    pub aux_seeding: usize,
    pub aux_verifying: usize,
    pub aux_failed: usize,
    pub with_failure: usize,
    pub no_aux: usize,
    pub main_missing: usize,
}

// ------------------------------------------------------------------ grouping

/// 并查集节点：一个 hash 一个节点。下载器里存在的是「活节点」，只在记录里出现的是
/// 幽灵节点（主种子被删掉的那种情况）。
struct Nodes<'a> {
    hashes: Vec<String>,
    torrents: Vec<Option<&'a TorrentInfo>>,
    index: HashMap<String, usize>,
}

impl Nodes<'_> {
    fn new() -> Self {
        Self {
            hashes: Vec::new(),
            torrents: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn intern(&mut self, hash: &str) -> usize {
        let key = hash.trim().to_ascii_lowercase();
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        let i = self.hashes.len();
        self.hashes.push(key.clone());
        self.torrents.push(None);
        self.index.insert(key, i);
        i
    }
}

struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            // 路径减半：深度压到常数级，顺手避免递归。
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// 把下载器快照与辅种记录聚成种子组。
///
/// 只返回**参与辅种**的组：组里至少有一条辅种记录，或至少有一个带辅种标签的种子。
/// 纯下载器种子归「下载器」页，不在这里重复列出。
#[must_use]
pub fn build_groups(torrents: &[TorrentInfo], attempts: &[ReseedAttempt]) -> Vec<SeedGroup> {
    let mut nodes = Nodes::new();
    for t in torrents {
        let i = nodes.intern(&t.hash);
        nodes.torrents[i] = Some(t);
    }
    for a in attempts {
        nodes.intern(&a.source_hash);
        nodes.intern(&a.target_info_hash);
    }

    let mut dsu = DisjointSet::new(nodes.hashes.len());

    // 证据一：辅种记录本身。它明确说了「这份数据是从哪个种子复用的」。
    for a in attempts {
        let s = nodes.intern(&a.source_hash);
        let t = nodes.intern(&a.target_info_hash);
        dsu.union(s, t);
    }

    // 证据二：下载器里同数据身份的两个种子。按大小分桶，桶内再比名称——大小不同
    // 直接不可能同源，先分桶就把 O(n²) 砍成几个小桶内的比较。
    let mut buckets: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, slot) in nodes.torrents.iter().enumerate() {
        if let Some(t) = slot
            && t.total_size > 0
        {
            buckets.entry(t.total_size).or_default().push(i);
        }
    }
    for bucket in buckets.values() {
        for (pos, &a) in bucket.iter().enumerate() {
            for &b in &bucket[..pos] {
                if let (Some(ta), Some(tb)) = (nodes.torrents[a], nodes.torrents[b])
                    && same_release(ta.total_size, &ta.name, tb.total_size, &tb.name)
                {
                    dsu.union(a, b);
                }
            }
        }
    }

    // 记录归属到「它自己的目标 hash」所在的组。
    let mut attempts_by_root: HashMap<usize, Vec<&ReseedAttempt>> = HashMap::new();
    for a in attempts {
        let node = nodes.intern(&a.target_info_hash);
        let root = dsu.find(node);
        attempts_by_root.entry(root).or_default().push(a);
    }

    let mut components: HashMap<usize, (Vec<String>, Vec<&TorrentInfo>)> = HashMap::new();
    for i in 0..nodes.hashes.len() {
        let root = dsu.find(i);
        let entry = components.entry(root).or_default();
        entry.0.push(nodes.hashes[i].clone());
        if let Some(t) = nodes.torrents[i] {
            entry.1.push(t);
        }
    }

    let mut groups: Vec<SeedGroup> = components
        .into_iter()
        .filter_map(|(root, (hashes, live))| {
            let group_attempts = attempts_by_root.remove(&root).unwrap_or_default();
            let tagged = live.iter().any(|t| is_aux_tag(&t.tags));
            if group_attempts.is_empty() && !tagged {
                return None;
            }
            Some(build_group(hashes, &live, &group_attempts))
        })
        .collect();

    sort_groups(&mut groups);
    groups
}

#[expect(
    clippy::too_many_lines,
    reason = "组内的主种子、辅种子、重复副本共享同一批派生信息，拆开只会传递一长串参数"
)]
fn build_group(
    mut hashes: Vec<String>,
    live: &[&TorrentInfo],
    attempts: &[&ReseedAttempt],
) -> SeedGroup {
    let source_hashes: HashSet<&str> = attempts.iter().map(|a| a.source_hash.as_str()).collect();
    let target_hashes: HashSet<&str> = attempts
        .iter()
        .map(|a| a.target_info_hash.as_str())
        .collect();

    let main = pick_main(live, &source_hashes, &target_hashes);
    let main_hash = main.map(|t| t.hash.clone());
    let snapshot = attempts.iter().min_by_key(|a| a.id);

    // 主种子缺失时，用记录快照 + 仍在下载器里的辅种拼出组的展示信息。
    let live_aux_size = live.iter().map(|t| t.total_size).max().unwrap_or(0);
    let name = main.map_or_else(
        || {
            snapshot
                .map(|a| a.source_name.clone())
                .or_else(|| live.first().map(|t| t.name.clone()))
                .unwrap_or_default()
        },
        |t| t.name.clone(),
    );
    let save_path = main.map_or_else(
        || {
            snapshot
                .map(|a| a.save_path.clone())
                .or_else(|| live.first().map(|t| t.save_path.clone()))
                .unwrap_or_default()
        },
        |t| t.save_path.clone(),
    );
    let size = main.map_or(live_aux_size, |t| t.total_size);
    let category = main.and_then(|t| t.category.clone());

    let main_seed = main.map_or_else(
        || MainSeed {
            hash: snapshot.map(|a| a.source_hash.clone()).unwrap_or_default(),
            name: name.clone(),
            size,
            save_path: save_path.clone(),
            state: String::new(),
            phase: TorrentPhase::Unknown,
            progress: 0.0,
            seeding_time: None,
            uploaded: 0,
            ratio: 0.0,
            added_on: None,
            tags: Vec::new(),
            trackers: Vec::new(),
            present: false,
        },
        |t| MainSeed {
            hash: t.hash.clone(),
            name: t.name.clone(),
            size: t.total_size,
            save_path: t.save_path.clone(),
            state: t.state.clone(),
            phase: t.phase(),
            progress: t.progress,
            seeding_time: t.seeding_time,
            uploaded: t.uploaded,
            ratio: t.ratio,
            added_on: t.added_on,
            tags: t.tags.clone(),
            trackers: t.trackers.clone(),
            present: true,
        },
    );

    let mut aux: Vec<AuxSeed> = attempts
        .iter()
        .map(|a| {
            let target = a.target_info_hash.trim().to_ascii_lowercase();
            let found = live
                .iter()
                .find(|t| t.hash.trim().eq_ignore_ascii_case(&target));
            AuxSeed {
                attempt_id: a.id,
                site: a.target_site.clone(),
                site_name: a.target_site.clone(),
                sid: a.target_sid,
                torrent_id: a.target_torrent_id,
                info_hash: target,
                state: a.state.as_str().to_string(),
                check_count: a.check_count,
                added_at: a.added_at,
                last_checked_at: a.last_checked_at,
                finished_at: a.finished_at,
                message: a.message.clone(),
                in_downloader: found.is_some(),
                live: found.map(|t| AuxLive {
                    state: t.state.clone(),
                    phase: t.phase(),
                    progress: t.progress,
                    seeding_time: t.seeding_time,
                    uploaded: t.uploaded,
                    ratio: t.ratio,
                    added_on: t.added_on,
                }),
            }
        })
        .collect();
    aux.sort_by(|a, b| a.site.cmp(&b.site).then(a.info_hash.cmp(&b.info_hash)));

    let counts = count_aux(&aux);
    let main_broken = main_seed.present
        && matches!(
            main_seed.phase,
            TorrentPhase::Error | TorrentPhase::MissingFiles
        );
    let state = if !main_seed.present && !attempts.is_empty() {
        STATE_MAIN_MISSING
    } else if main_broken || counts.failed > 0 {
        STATE_DEGRADED
    } else if counts.verifying > 0 {
        STATE_VERIFYING
    } else if aux.is_empty() {
        STATE_NO_AUX
    } else {
        STATE_HEALTHY
    };

    let duplicates = live
        .iter()
        .filter(|t| {
            main_hash
                .as_ref()
                .is_none_or(|h| !t.hash.eq_ignore_ascii_case(h))
                && !target_hashes.contains(t.hash.as_str())
        })
        .map(|t| DuplicateSeed {
            hash: t.hash.clone(),
            name: t.name.clone(),
            uploaded: t.uploaded,
            added_on: t.added_on,
        })
        .collect();

    let updated_at = attempts
        .iter()
        .flat_map(|a| [a.last_checked_at, a.finished_at, a.added_at])
        .flatten()
        .max()
        .or(main_seed.added_on);

    let key = if main_seed.present {
        main_seed.hash.clone()
    } else {
        // 主种子没了：用组内最小的 hash 当标识，保证同样的记录每次算出同一个 key。
        hashes.sort();
        hashes.first().cloned().unwrap_or_default()
    };

    SeedGroup {
        key,
        name,
        size,
        save_path,
        category,
        state,
        main: main_seed,
        aux,
        duplicates,
        counts,
        updated_at,
    }
}

/// 主种子的判定顺序（见 `docs/seeds-management.md`）。
///
/// 有辅种记录时，绝不拿一条辅种子冒充主种子：如果下载器里只剩辅种，真实的主种子
/// 已经被删掉了，页面必须说 `main_missing`，而不是把某个站点的副本标成「主种子」。
///
/// 但「还在下载器里、文件却缺了」不算缺失——它仍然是我们认得出的原件，只是坏了，
/// 组的 `degraded` 状态会把这件事说出来。把这两种情况混成一句「主种子缺失」会让人
/// 去找一个其实没被删掉的种子。
fn pick_main<'a>(
    live: &[&'a TorrentInfo],
    source_hashes: &HashSet<&str>,
    target_hashes: &HashSet<&str>,
) -> Option<&'a TorrentInfo> {
    if source_hashes.is_empty() {
        // 只有标签、没有记录：组里最像「原件」的那个当主种子。
        return pick(live.iter().copied());
    }

    let from_source: Vec<&TorrentInfo> = live
        .iter()
        .copied()
        .filter(|t| source_hashes.contains(t.hash.as_str()))
        .collect();
    if let Some(t) = pick(from_source) {
        return Some(t);
    }

    let untagged: Vec<&TorrentInfo> = live
        .iter()
        .copied()
        .filter(|t| !is_aux_tag(&t.tags) && !target_hashes.contains(t.hash.as_str()))
        .collect();
    pick(untagged)
}

/// 一组候选里挑「最像原件」的：校验通过优先，其次加入时间早，再其次上传量。
fn pick<'a, I>(pool: I) -> Option<&'a TorrentInfo>
where
    I: IntoIterator<Item = &'a TorrentInfo>,
{
    pool.into_iter().min_by(|a, b| {
        let ka = (
            u8::from(!a.is_verified_complete()),
            a.added_on.map_or(i64::MAX, |d| d.timestamp()),
            -a.uploaded,
        );
        let kb = (
            u8::from(!b.is_verified_complete()),
            b.added_on.map_or(i64::MAX, |d| d.timestamp()),
            -b.uploaded,
        );
        ka.cmp(&kb)
    })
}

fn is_aux_tag(tags: &[String]) -> bool {
    tags.iter()
        .any(|tag| tag == AUX_TAG || tag.starts_with(&format!("{AUX_TAG}:")))
}

/// 辅种的「有效状态」：下载器的实时状态优先于记录。
///
/// 记录说 `seeding`，但下载器已经报文件缺失——这时候还显示「做种中」就是撒谎，
/// 而那正是这个工具存在的意义：只承认真的在做种。
fn effective_state(aux: &AuxSeed) -> &'static str {
    if let Some(live) = &aux.live {
        match live.phase {
            TorrentPhase::Error | TorrentPhase::MissingFiles => return "failed",
            TorrentPhase::Checking => return "verifying",
            _ => {}
        }
    }
    match ReseedState::parse(&aux.state) {
        ReseedState::Seeding => "seeding",
        ReseedState::Added | ReseedState::Verifying => "verifying",
        ReseedState::VerifyFailed
        | ReseedState::MissingFiles
        | ReseedState::FetchFailed
        | ReseedState::AddFailed
        | ReseedState::TimedOut => "failed",
        ReseedState::Pending | ReseedState::Skipped | ReseedState::Fetching => "pending",
    }
}

fn count_aux(aux: &[AuxSeed]) -> GroupCounts {
    let mut counts = GroupCounts {
        aux: aux.len(),
        ..GroupCounts::default()
    };
    for a in aux {
        match effective_state(a) {
            "seeding" => counts.seeding += 1,
            "verifying" => counts.verifying += 1,
            "failed" => counts.failed += 1,
            _ => {}
        }
    }
    counts
}

/// 全量统计，用于页面顶部的概览数字。
#[must_use]
pub fn summarize(groups: &[SeedGroup]) -> SeedCounts {
    let mut counts = SeedCounts {
        groups: groups.len(),
        ..SeedCounts::default()
    };
    for g in groups {
        counts.aux_total += g.counts.aux;
        counts.aux_seeding += g.counts.seeding;
        counts.aux_verifying += g.counts.verifying;
        counts.aux_failed += g.counts.failed;
        match g.state {
            STATE_DEGRADED => counts.with_failure += 1,
            STATE_NO_AUX => counts.no_aux += 1,
            STATE_MAIN_MISSING => counts.main_missing += 1,
            _ => {}
        }
    }
    counts
}

pub fn sort_groups(groups: &mut [SeedGroup]) {
    groups.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// 过滤 + 搜索。`q` 命中名称、主种子 hash、任一辅种 hash、站点、保存路径。
#[must_use]
pub fn matches(group: &SeedGroup, filter: &str, q: &str) -> bool {
    let by_state = match filter {
        "has_failure" => group.state == STATE_DEGRADED,
        "verifying" => group.state == STATE_VERIFYING,
        "no_aux" => group.state == STATE_NO_AUX,
        "main_missing" => group.state == STATE_MAIN_MISSING,
        // "all" / "" / 未知 filter 一律不筛：宁可全显示，也不要让用户以为没有数据。
        _ => true,
    };
    if !by_state {
        return false;
    }

    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    group.name.to_lowercase().contains(&q)
        || group.save_path.to_lowercase().contains(&q)
        || group.main.hash.to_lowercase().contains(&q)
        || group.aux.iter().any(|a| {
            a.site.to_lowercase().contains(&q)
                || a.info_hash.to_lowercase().contains(&q)
                || a.site_name.to_lowercase().contains(&q)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iyuu_core::models::DownloaderKind;

    const MIB: i64 = 1024 * 1024;

    fn torrent(hash: &str, name: &str, size: i64) -> TorrentInfo {
        TorrentInfo {
            hash: hash.to_string(),
            name: name.to_string(),
            save_path: "/data/seed".to_string(),
            content_path: None,
            total_size: size,
            progress: 1.0,
            state: "stalledUP".to_string(),
            kind: DownloaderKind::Qbittorrent,
            category: Some("刷流".to_string()),
            tags: Vec::new(),
            added_on: Some(Utc::now()),
            seeding_time: Some(3600),
            uploaded: 1024,
            ratio: 1.0,
            error: None,
            trackers: vec!["tracker.example".to_string()],
        }
    }

    fn attempt(
        id: i64,
        source: &str,
        target: &str,
        site: &str,
        state: ReseedState,
    ) -> ReseedAttempt {
        ReseedAttempt {
            id,
            source_hash: source.to_string(),
            source_name: "Some.Movie.2024.1080p.WEB-DL.x264-GROUP".to_string(),
            save_path: "/data/seed".to_string(),
            target_info_hash: target.to_string(),
            target_sid: 23,
            target_site: site.to_string(),
            target_torrent_id: 12345,
            state,
            check_count: 2,
            last_checked_at: Some(Utc::now()),
            verify_started_at: Some(Utc::now()),
            added_at: Some(Utc::now()),
            finished_at: None,
            message: None,
            downloader: "qbittorrent".to_string(),
            run_id: None,
        }
    }

    fn name() -> &'static str {
        "Some.Movie.2024.1080p.WEB-DL.x264-GROUP"
    }

    #[test]
    fn main_and_two_aux_form_one_group() {
        let t = vec![
            torrent("aaaa", name(), 8 * MIB),
            torrent("bbbb", name(), 8 * MIB),
            torrent("cccc", name(), 8 * MIB),
        ];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "springsunday", ReseedState::Seeding),
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Seeding),
        ];

        let groups = build_groups(&t, &a);
        assert_eq!(groups.len(), 1, "同一资源应聚成一组");
        let g = &groups[0];
        assert_eq!(g.main.hash, "aaaa");
        assert!(g.main.present);
        assert_eq!(g.aux.len(), 2);
        assert_eq!(g.counts.seeding, 2);
        assert_eq!(g.state, STATE_HEALTHY);
    }

    #[test]
    fn main_deleted_keeps_group_alive() {
        // 只有辅种子还在下载器里；主种子 aaaa 已被删除。
        let t = vec![torrent("bbbb", name(), 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let groups = build_groups(&t, &a);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.state, STATE_MAIN_MISSING);
        assert!(!g.main.present, "主种子不在下载器里");
        assert_eq!(g.main.hash, "aaaa");
        assert!(g.main.name.contains("Some.Movie"), "名称来自记录快照");
        assert_eq!(g.counts.seeding, 1);
    }

    #[test]
    fn failed_aux_marks_group_degraded() {
        let t = vec![
            torrent("aaaa", name(), 8 * MIB),
            torrent("bbbb", name(), 8 * MIB),
        ];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "springsunday", ReseedState::VerifyFailed),
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Seeding),
        ];

        let groups = build_groups(&t, &a);
        let g = &groups[0];
        assert_eq!(g.state, STATE_DEGRADED);
        assert_eq!(g.counts.failed, 1);
        assert_eq!(g.counts.seeding, 1);
        // cccc 不在下载器里，它的 live 必须是 None 而不是伪造状态。
        assert!(
            !g.aux
                .iter()
                .any(|x| x.info_hash == "cccc" && x.in_downloader)
        );
    }

    #[test]
    fn different_sizes_stay_apart() {
        let t = vec![
            torrent("aaaa", name(), 8 * MIB),
            torrent("bbbb", name(), 9 * MIB),
        ];
        let a = vec![
            attempt(1, "aaaa", "dddd", "springsunday", ReseedState::Seeding),
            attempt(2, "bbbb", "eeee", "mteam", ReseedState::Seeding),
        ];

        let groups = build_groups(&t, &a);
        assert_eq!(groups.len(), 2, "大小不同必须分成两组");
    }

    #[test]
    fn duplicate_copy_is_reported_not_hidden() {
        let t = vec![
            torrent("aaaa", name(), 8 * MIB),
            torrent("bbbb", name(), 8 * MIB),
            torrent("cccc", name(), 8 * MIB),
        ];
        // cccc 既不是主种子也不是任何记录的目标 —— 同一份数据的第三个副本。
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let groups = build_groups(&t, &a);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].duplicates.len(), 1);
        assert_eq!(groups[0].duplicates[0].hash, "cccc");
    }

    #[test]
    fn tagged_torrent_without_attempt_is_no_aux() {
        let mut t = torrent("aaaa", name(), 8 * MIB);
        t.tags = vec![format!("{AUX_TAG}:mteam")];

        let groups = build_groups(&[t], &[]);
        assert_eq!(groups.len(), 1, "带辅种标签的种子也要进管理视图");
        assert_eq!(groups[0].state, STATE_NO_AUX);
        assert!(groups[0].aux.is_empty());
    }

    #[test]
    fn plain_downloader_torrent_is_not_listed() {
        let t = vec![torrent("aaaa", name(), 8 * MIB)];
        assert!(build_groups(&t, &[]).is_empty());
    }

    #[test]
    fn live_error_overrides_recorded_success() {
        let mut aux_torrent = torrent("bbbb", name(), 8 * MIB);
        aux_torrent.state = "missingFiles".to_string();
        let t = vec![torrent("aaaa", name(), 8 * MIB), aux_torrent];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let groups = build_groups(&t, &a);
        assert_eq!(
            groups[0].state, STATE_DEGRADED,
            "下载器说文件缺失就不算成功"
        );
        assert_eq!(groups[0].counts.failed, 1);
    }

    #[test]
    fn broken_main_is_degraded_not_missing() {
        let mut main = torrent("aaaa", name(), 8 * MIB);
        main.state = "missingFiles".to_string();
        let t = vec![main, torrent("bbbb", name(), 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let groups = build_groups(&t, &a);
        let g = &groups[0];
        assert!(g.main.present, "种子还在下载器里，只是文件缺了");
        assert_eq!(g.state, STATE_DEGRADED);
    }

    #[test]
    fn summarize_counts_groups_and_aux() {
        let t = vec![torrent("aaaa", name(), 8 * MIB)];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "springsunday", ReseedState::Seeding),
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Verifying),
        ];
        let groups = build_groups(&t, &a);
        let s = summarize(&groups);
        assert_eq!(s.groups, 1);
        assert_eq!(s.aux_total, 2);
        assert_eq!(s.aux_seeding, 1);
        assert_eq!(s.aux_verifying, 1);
    }

    #[test]
    fn filter_and_search() {
        let t = vec![torrent("aaaa", name(), 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::VerifyFailed,
        )];
        let groups = build_groups(&t, &a);
        let g = &groups[0];

        assert!(matches(g, "all", ""));
        assert!(matches(g, "has_failure", ""));
        assert!(!matches(g, "verifying", ""));
        assert!(matches(g, "all", "springsunday"));
        assert!(matches(g, "all", "SOME.MOVIE"));
        assert!(!matches(g, "all", "nonexistent"));
    }
}

#[cfg(test)]
mod multi_group_tests {
    use super::*;
    use iyuu_core::models::DownloaderKind;

    const MIB: i64 = 1024 * 1024;

    fn t(hash: &str, name: &str, size: i64) -> TorrentInfo {
        TorrentInfo {
            hash: hash.to_string(),
            name: name.to_string(),
            save_path: "/data/media".to_string(),
            content_path: None,
            total_size: size,
            progress: 1.0,
            state: "stalledUP".to_string(),
            kind: DownloaderKind::Qbittorrent,
            category: Some("movies".to_string()),
            tags: Vec::new(),
            added_on: Some(Utc::now()),
            seeding_time: Some(1),
            uploaded: 0,
            ratio: 0.0,
            error: None,
            trackers: Vec::new(),
        }
    }

    fn a(id: i64, src: &str, dst: &str, site: &str) -> ReseedAttempt {
        ReseedAttempt {
            id,
            source_hash: src.to_string(),
            source_name: "x".to_string(),
            save_path: "/data/media".to_string(),
            target_info_hash: dst.to_string(),
            target_sid: 1,
            target_site: site.to_string(),
            target_torrent_id: 1,
            state: ReseedState::Seeding,
            check_count: 1,
            last_checked_at: Some(Utc::now()),
            verify_started_at: Some(Utc::now()),
            added_at: Some(Utc::now()),
            finished_at: None,
            message: None,
            downloader: "qbittorrent".to_string(),
            run_id: None,
        }
    }

    #[test]
    fn four_releases_stay_in_four_groups() {
        let sources = [
            (
                "aaaa",
                "Interstellar.2014.2160p.UHD.BluRay.HDR.x265-GROUP",
                6000 * MIB,
            ),
            (
                "bbbb",
                "Three.Body.S01.2024.1080p.WEB-DL.H264-HDCTV",
                4600 * MIB,
            ),
            (
                "cccc",
                "Kind.of.Blue.2023.1080p.BluRay.x264-AMIABLE",
                2200 * MIB,
            ),
            ("dddd", "Some.Show.S02.2160p.WEB-DL.H265-DRAMA", 9000 * MIB),
        ];
        let mut torrents: Vec<TorrentInfo> = sources.iter().map(|(h, n, s)| t(h, n, *s)).collect();
        let mut attempts = Vec::new();
        for (i, (h, n, size)) in sources.iter().enumerate() {
            for (j, site) in ["alpha", "beta"].into_iter().enumerate() {
                let id = i64::try_from(i * 2 + j).unwrap() + 1;
                let target = format!("{:04x}", i * 10 + j);
                torrents.push(t(&target, &format!("{n}.{}", site.to_uppercase()), *size));
                attempts.push(a(id, h, &target, site));
            }
        }
        let groups = build_groups(&torrents, &attempts);
        println!("groups = {}", groups.len());
        for g in &groups {
            println!("- {} | {} | aux {}", g.name, g.state, g.aux.len());
        }
        assert_eq!(groups.len(), 4);
    }
}
