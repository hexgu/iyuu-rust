//! 「种子管理」视图：把下载器里的种子按「同一份资源」聚成组，主种子一行、辅种折叠。
//!
//! 这里只做聚合，不做持久化。分组是**推导**出来的：下载器里种子一删、记录一改，
//! 下次请求自然就是新的结果。把组写进表里等于给自己造一份会过期的副本。
//!
//! 页面管理的是**下载器**，不是辅种记录：每条种子都必须出现，记录只负责补充元数据
//! （站点、校验次数、失败原因）。识别顺序见 `docs/seeds-management.md`：
//! 位置身份（`content_path`）→ 数据身份（大小 + 名称）→ 记录 → 标签。标签只影响角色，
//! 不参与「这是不是同一份数据」的判断。

use chrono::{DateTime, Utc};
use iyuu_core::downloader::has_aux_tag;
use iyuu_core::identity::{content_key, same_release};
use iyuu_core::models::{IyuuSite, ReseedAttempt, ReseedState, TorrentInfo, TorrentPhase};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

pub const STATE_HEALTHY: &str = "healthy";
pub const STATE_VERIFYING: &str = "verifying";
pub const STATE_DEGRADED: &str = "degraded";
pub const STATE_MAIN_MISSING: &str = "main_missing";
pub const STATE_NO_AUX: &str = "no_aux";

/// 主种子是靠什么认出来的。UI 把 `inferred` 标出来，不假装推断是事实。
pub const MAIN_RECORD: &str = "record";
pub const MAIN_SINGLE: &str = "single";
pub const MAIN_TAG: &str = "tag";
pub const MAIN_INFERRED: &str = "inferred";
/// 靠下载流量认出来的：用户真的在这里下过数据，副本是复用文件加进来的。
pub const MAIN_TRAFFIC: &str = "traffic";
pub const MAIN_SNAPSHOT: &str = "snapshot";

/// 辅种站点信息的来源。
pub const SITE_RECORD: &str = "record";
pub const SITE_TRACKER: &str = "tracker";
pub const SITE_UNKNOWN: &str = "unknown";

/// 辅种是怎么被认出来的。
pub const ORIGIN_RECORD: &str = "record";
pub const ORIGIN_TAG: &str = "tag";
pub const ORIGIN_CONTENT: &str = "content";

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

/// 一次请求需要的全部输入。
#[derive(Clone, Copy)]
pub struct SeedsInput<'a> {
    pub torrents: &'a [TorrentInfo],
    pub attempts: &'a [ReseedAttempt],
    /// 其它辅种工具留下的标签（默认 `辅种`）；我们自己的 `iyuu-rust` 与旧
    /// `iyuu-reseed*` 永远认得，不需要配置。
    pub aux_tags: &'a [String],
    /// IYUU 站点目录，用于把 tracker host / 站点 slug 换成人看得懂的名字。
    pub sites: &'a [IyuuSite],
    /// 下载器显示名。每条记录都带上它，为「多个下载器」留好位置。
    pub downloader: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeedGroup {
    /// 组标识：主种子的 hash；主种子缺失时取组内最小的 hash，保证刷新之间稳定。
    pub key: String,
    pub name: String,
    pub size: i64,
    pub save_path: String,
    pub content_path: Option<String>,
    pub category: Option<String>,
    pub state: &'static str,
    /// `record` / `single` / `tag` / `inferred` / `snapshot`
    pub main_source: &'static str,
    pub main: MainSeed,
    pub aux: Vec<AuxSeed>,
    pub counts: GroupCounts,
    pub updated_at: Option<DateTime<Utc>>,
    /// 组内种子分布在哪几个下载器上。
    pub downloaders: Vec<String>,
    /// 组内有多少条辅种记录。自动清理要求这个不为 0（或者全部活种子都带辅种标签）。
    pub record_count: usize,
    /// 组内所有活种子都带辅种标签：没有一个像是用户自己下的那份。
    pub all_tagged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MainSeed {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub save_path: String,
    pub content_path: Option<String>,
    pub state: String,
    pub phase: TorrentPhase,
    pub progress: f64,
    pub seeding_time: Option<i64>,
    pub uploaded: i64,
    /// 累计下载量。主种子是用户真的拉过数据的那一份，副本是 0。
    pub downloaded: i64,
    pub ratio: f64,
    pub added_on: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub trackers: Vec<String>,
    pub downloader: String,
    /// false = 下载器里已经找不到它了，字段来自最后的记录快照。
    pub present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxSeed {
    /// 库里没有这条记录时是 `None`：别的工具加的副本也照样显示。
    pub attempt_id: Option<i64>,
    pub info_hash: String,
    /// 站点 slug 或 tracker host，原样保留，便于排查。
    pub site: String,
    /// 人看的站点名；站点目录里查不到时退回 `site`。
    pub site_name: String,
    pub site_source: &'static str,
    pub origin: &'static str,
    /// 同一站点上的重复副本（同数据的第二、第三份）。
    pub duplicate: bool,
    /// 有效状态：`seeding` / `verifying` / `failed` / `pending` / `missing`。
    pub state: &'static str,
    pub phase: Option<TorrentPhase>,
    pub progress: f64,
    pub check_count: i32,
    pub message: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub in_downloader: bool,
    pub tags: Vec<String>,
    pub uploaded: i64,
    pub ratio: f64,
    pub seeding_time: Option<i64>,
    pub added_on: Option<DateTime<Utc>>,
    pub save_path: Option<String>,
    /// 这个副本实际落盘的路径。清理前用它检查有没有别的资源共用同一份文件。
    pub content_path: Option<String>,
    pub downloader: String,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct GroupCounts {
    pub aux: usize,
    pub seeding: usize,
    pub verifying: usize,
    /// 记录说失败，或下载器报错 / 文件缺失。
    pub failed: usize,
    /// 记录说成功，但下载器里已经没有这个种子。
    pub missing: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct SeedCounts {
    pub groups: usize,
    /// 下载器里的种子总数（含主种子与辅种）。
    pub torrents: usize,
    pub with_aux: usize,
    pub no_aux: usize,
    pub aux_total: usize,
    pub aux_seeding: usize,
    pub aux_verifying: usize,
    pub aux_failed: usize,
    pub aux_missing: usize,
    pub main_missing: usize,
    pub with_failure: usize,
    /// 主种子靠推断（而不是记录 / 标签 / 唯一候选）认出来的组数。
    pub inferred: usize,
}

// ------------------------------------------------------------------ site index

/// tracker host / 站点 slug → 人看的站点名。
struct SiteIndex {
    entries: Vec<(String, String)>,
}

impl SiteIndex {
    fn new(sites: &[IyuuSite]) -> Self {
        let mut entries = Vec::new();
        for s in sites {
            let name = s.display_name().to_string();
            if name.trim().is_empty() {
                continue;
            }
            let slug = s.site.trim().to_ascii_lowercase();
            if !slug.is_empty() {
                entries.push((slug, name.clone()));
            }
            if let Some(host) = normalize_host(s.host()) {
                entries.push((host, name));
            }
        }
        Self { entries }
    }

    /// 先按 slug / host 精确匹配，再退一步按域名后缀匹配
    /// （`tracker.example.org` ↔ `example.org`）。
    fn label(&self, raw: &str) -> Option<String> {
        let host = normalize_host(raw)?;
        if let Some((_, name)) = self.entries.iter().find(|(k, _)| *k == host) {
            return Some(name.clone());
        }
        self.entries
            .iter()
            .find(|(k, _)| host.ends_with(&format!(".{k}")) || k.ends_with(&format!(".{host}")))
            .map(|(_, name)| name.clone())
    }
}

/// 去掉 scheme / 端口 / 路径，返回小写 host。
fn normalize_host(raw: &str) -> Option<String> {
    let rest = raw.split("://").nth(1).unwrap_or(raw);
    let host = rest.split('/').next()?.split(':').next()?.trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
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

/// 把下载器快照与辅种记录聚成资源组。**每一条种子都会出现在结果里**——包括从未
/// 参与过辅种的普通种子，它们就是 `no_aux` 的组。
#[must_use]
pub fn build_groups(input: SeedsInput<'_>) -> Vec<SeedGroup> {
    let SeedsInput {
        torrents,
        attempts,
        aux_tags,
        sites,
        downloader,
    } = input;
    let site_index = SiteIndex::new(sites);

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

    // 证据二：位置身份。同一份数据在磁盘上只有一个落脚点，跨站副本的 content_path
    // 完全一样——这是最强的一条正证据，且与标签无关。
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for (i, slot) in nodes.torrents.iter().enumerate() {
        let Some(t) = slot else { continue };
        let Some(path) = content_key(&t.save_path, t.content_path.as_deref(), &t.name) else {
            continue;
        };
        // 路径相同、大小不同：两个 .torrent 描述的文件集不一样，宁可分开。
        let key = format!("{path}|{}", t.total_size);
        if let Some(&first) = by_path.get(&key) {
            dsu.union(first, i);
        } else {
            by_path.insert(key, i);
        }
    }

    // 证据三：数据身份（大小 + 名称）。按大小分桶，桶内再比名称——大小不同直接
    // 不可能同源，先分桶就把 O(n²) 砍成几个小桶内的比较。
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
        .map(|(root, (hashes, live))| {
            let group_attempts = attempts_by_root.remove(&root).unwrap_or_default();
            build_group(GroupInput {
                hashes,
                live,
                attempts: group_attempts,
                aux_tags,
                sites: &site_index,
                downloader,
            })
        })
        .collect();

    sort_groups(&mut groups);
    groups
}

struct GroupInput<'a> {
    hashes: Vec<String>,
    live: Vec<&'a TorrentInfo>,
    attempts: Vec<&'a ReseedAttempt>,
    aux_tags: &'a [String],
    sites: &'a SiteIndex,
    downloader: &'a str,
}

#[expect(
    clippy::too_many_lines,
    reason = "组的名称、快照、角色、辅种、状态共享同一批派生信息，拆开只会传递一长串参数"
)]
fn build_group(input: GroupInput<'_>) -> SeedGroup {
    let GroupInput {
        mut hashes,
        live,
        attempts,
        aux_tags,
        sites,
        downloader,
    } = input;

    let source_hashes: HashSet<String> = attempts
        .iter()
        .map(|a| a.source_hash.trim().to_ascii_lowercase())
        .collect();
    let target_hashes: HashSet<String> = attempts
        .iter()
        .map(|a| a.target_info_hash.trim().to_ascii_lowercase())
        .collect();

    let (main, main_source) = pick_main(&live, &source_hashes, &target_hashes, aux_tags)
        .map_or((None, MAIN_SNAPSHOT), |(t, source)| (Some(t), source));
    let main_hash = main.map(|t| t.hash.trim().to_ascii_lowercase());
    let snapshot = attempts.iter().min_by_key(|a| a.id);

    let name = main.map_or_else(
        || {
            snapshot
                .map(|a| a.source_name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| live.first().map(|t| t.name.clone()))
                .unwrap_or_default()
        },
        |t| t.name.clone(),
    );
    let save_path = main.map_or_else(
        || {
            snapshot
                .map(|a| a.save_path.clone())
                .filter(|p| !p.is_empty())
                .or_else(|| live.first().map(|t| t.save_path.clone()))
                .unwrap_or_default()
        },
        |t| t.save_path.clone(),
    );
    // 主种子被删了也要知道这个资源落在哪：清理要检查「删文件会不会带走别的资源」，
    // 辅种副本的路径就是同一个位置。
    let content_path = main
        .and_then(|t| t.content_path.clone())
        .or_else(|| live.iter().find_map(|t| t.content_path.clone()));
    let size = main.map_or_else(
        || live.iter().map(|t| t.total_size).max().unwrap_or(0),
        |t| t.total_size,
    );
    let category = main.and_then(|t| t.category.clone());

    let main_seed = main.map_or_else(
        || MainSeed {
            hash: snapshot.map_or_else(String::new, |a| a.source_hash.clone()),
            name: name.clone(),
            size,
            save_path: save_path.clone(),
            content_path: None,
            state: String::new(),
            phase: TorrentPhase::Unknown,
            progress: 0.0,
            seeding_time: None,
            uploaded: 0,
            downloaded: 0,
            ratio: 0.0,
            added_on: None,
            tags: Vec::new(),
            trackers: Vec::new(),
            downloader: snapshot
                .map(|a| a.downloader.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| downloader.to_string()),
            present: false,
        },
        |t| MainSeed {
            hash: t.hash.clone(),
            name: t.name.clone(),
            size: t.total_size,
            save_path: t.save_path.clone(),
            content_path: t.content_path.clone(),
            state: t.state.clone(),
            phase: t.phase(),
            progress: t.progress,
            seeding_time: t.seeding_time,
            uploaded: t.uploaded,
            downloaded: t.downloaded,
            ratio: t.ratio,
            added_on: t.added_on,
            tags: t.tags.clone(),
            trackers: t.trackers.clone(),
            downloader: downloader.to_string(),
            present: true,
        },
    );

    let aux = build_aux(
        &live,
        &attempts,
        main_hash.as_deref(),
        aux_tags,
        sites,
        downloader,
    );
    let counts = count_aux(&aux);

    let main_broken = main_seed.present
        && matches!(
            main_seed.phase,
            TorrentPhase::Error | TorrentPhase::MissingFiles
        );
    let state = if !main_seed.present {
        STATE_MAIN_MISSING
    } else if main_broken || counts.failed > 0 || counts.missing > 0 {
        STATE_DEGRADED
    } else if counts.verifying > 0 || matches!(main_seed.phase, TorrentPhase::Checking) {
        STATE_VERIFYING
    } else if aux.is_empty() {
        STATE_NO_AUX
    } else {
        STATE_HEALTHY
    };

    let updated_at = attempts
        .iter()
        .flat_map(|a| [a.last_checked_at, a.finished_at, a.added_at])
        .flatten()
        .max()
        .or_else(|| live.iter().filter_map(|t| t.added_on).max())
        .or(main_seed.added_on);

    let key = if main_seed.present {
        main_seed.hash.clone()
    } else {
        // 主种子没了：用组内最小的 hash 当标识，保证同样的记录每次算出同一个 key。
        hashes.sort();
        hashes.first().cloned().unwrap_or_default()
    };

    let mut downloaders: Vec<String> = Vec::new();
    for d in std::iter::once(main_seed.downloader.clone())
        .chain(aux.iter().map(|a| a.downloader.clone()))
    {
        if !d.is_empty() && !downloaders.iter().any(|x| x.eq_ignore_ascii_case(&d)) {
            downloaders.push(d);
        }
    }

    SeedGroup {
        key,
        name,
        size,
        save_path,
        content_path,
        category,
        state,
        main_source,
        main: main_seed,
        aux,
        counts,
        updated_at,
        downloaders,
        record_count: attempts.len(),
        all_tagged: !live.is_empty() && live.iter().all(|t| has_aux_tag(&t.tags, aux_tags)),
    }
}

/// 组内选主种子。返回 `(种子, 依据)`；`None` 表示主种子已经不在下载器里。
///
/// 有记录时，绝不拿一条辅种子冒充主种子：下载器里只剩辅种，说明真实的主种子已经被
/// 删掉了，页面必须说 `main_missing`，而不是把某个站点的副本标成「主种子」。
fn pick_main<'a>(
    live: &[&'a TorrentInfo],
    source_hashes: &HashSet<String>,
    target_hashes: &HashSet<String>,
    aux_tags: &[String],
) -> Option<(&'a TorrentInfo, &'static str)> {
    if let Some(t) = pick(
        live.iter()
            .copied()
            .filter(|t| source_hashes.contains(&t.hash.trim().to_ascii_lowercase())),
    ) {
        return Some((t, MAIN_RECORD));
    }

    let candidates: Vec<&TorrentInfo> = live
        .iter()
        .copied()
        .filter(|t| {
            !has_aux_tag(&t.tags, aux_tags)
                && !target_hashes.contains(&t.hash.trim().to_ascii_lowercase())
        })
        .collect();
    let t = pick(candidates)?;

    let source = if !source_hashes.is_empty() || !target_hashes.is_empty() {
        // 记录说明其余副本是辅种，这个既不是源也不是目标 —— 主种子。
        MAIN_RECORD
    } else if live.len() == 1 {
        MAIN_SINGLE
    } else if live.iter().any(|x| has_aux_tag(&x.tags, aux_tags)) {
        MAIN_TAG
    } else if t.downloaded > 0 {
        // 没有记录也没有标签，但这一份有下载流量：用户真的在这里拉过数据。
        // 副本是复用已有文件加进来的，客户端记的下载量是 0。
        MAIN_TRAFFIC
    } else {
        // 什么都没有：只能按加入时间推断。
        MAIN_INFERRED
    };
    Some((t, source))
}

/// 一组候选里挑「最像原件」的：校验通过优先，其次加入时间早，再其次上传量。
fn pick<'a, I>(pool: I) -> Option<&'a TorrentInfo>
where
    I: IntoIterator<Item = &'a TorrentInfo>,
{
    pool.into_iter().min_by(|a, b| {
        // 下载流量优先于「加入更早」：重新添加过的主种子会丢掉时间优势，但它的
        // 下载量是客户端记的账，不会因为重装而变成 0。
        let ka = (
            u8::from(a.downloaded <= 0),
            u8::from(!a.is_verified_complete()),
            a.added_on.map_or(i64::MAX, |d| d.timestamp()),
            -a.uploaded,
        );
        let kb = (
            u8::from(b.downloaded <= 0),
            u8::from(!b.is_verified_complete()),
            b.added_on.map_or(i64::MAX, |d| d.timestamp()),
            -b.uploaded,
        );
        ka.cmp(&kb)
    })
}

fn build_aux(
    live: &[&TorrentInfo],
    attempts: &[&ReseedAttempt],
    main_hash: Option<&str>,
    aux_tags: &[String],
    sites: &SiteIndex,
    downloader: &str,
) -> Vec<AuxSeed> {
    // 同一目标 hash 可能有多条记录（重试、重跑），只保留最新的一条。
    let mut by_target: HashMap<String, &ReseedAttempt> = HashMap::new();
    for a in attempts {
        let key = a.target_info_hash.trim().to_ascii_lowercase();
        by_target
            .entry(key)
            .and_modify(|cur| {
                if a.id > cur.id {
                    *cur = a;
                }
            })
            .or_insert(a);
    }

    let mut aux: Vec<AuxSeed> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for t in live {
        let hash = t.hash.trim().to_ascii_lowercase();
        if main_hash == Some(hash.as_str()) {
            continue;
        }
        let record = by_target.get(&hash).copied();
        aux.push(aux_from_live(t, record, aux_tags, sites, downloader));
        seen.insert(hash);
    }

    for (hash, record) in &by_target {
        if seen.contains(hash) || main_hash == Some(hash.as_str()) {
            continue;
        }
        aux.push(aux_from_record(record, sites));
    }

    // 同一个站点上的多份副本：只有活种子才算，记录里的失败目标是「这一站没辅上」，
    // 不是重复。
    let mut per_site: HashMap<String, usize> = HashMap::new();
    for a in &mut aux {
        if !a.in_downloader {
            continue;
        }
        // 认不出「这是哪一次辅种」的活副本，本身就是同数据的额外一份。
        if a.origin == ORIGIN_CONTENT {
            a.duplicate = true;
        }
        let site = a.site.to_ascii_lowercase();
        if site.is_empty() {
            continue;
        }
        let count = per_site.entry(site).or_insert(0);
        *count += 1;
        if *count > 1 {
            a.duplicate = true;
        }
    }

    aux.sort_by(|a, b| {
        a.site_name
            .to_ascii_lowercase()
            .cmp(&b.site_name.to_ascii_lowercase())
            .then_with(|| a.info_hash.cmp(&b.info_hash))
    });
    aux
}

fn aux_from_live(
    t: &TorrentInfo,
    record: Option<&ReseedAttempt>,
    aux_tags: &[String],
    sites: &SiteIndex,
    downloader: &str,
) -> AuxSeed {
    let origin = if record.is_some() {
        ORIGIN_RECORD
    } else if has_aux_tag(&t.tags, aux_tags) {
        ORIGIN_TAG
    } else {
        ORIGIN_CONTENT
    };

    let (site, site_name, site_source) = match record.map(|a| a.target_site.trim()) {
        Some(slug) if !slug.is_empty() => (
            slug.to_string(),
            sites.label(slug).unwrap_or_else(|| slug.to_string()),
            SITE_RECORD,
        ),
        _ => match t.trackers.first() {
            Some(host) => (
                host.clone(),
                sites.label(host).unwrap_or_else(|| host.clone()),
                SITE_TRACKER,
            ),
            None => (String::new(), String::new(), SITE_UNKNOWN),
        },
    };

    AuxSeed {
        attempt_id: record.map(|a| a.id),
        info_hash: t.hash.trim().to_ascii_lowercase(),
        site,
        site_name,
        site_source,
        origin,
        duplicate: false,
        state: live_state(t, record),
        phase: Some(t.phase()),
        progress: t.progress,
        check_count: record.map_or(0, |a| a.check_count),
        message: record.and_then(|a| a.message.clone()),
        added_at: record.and_then(|a| a.added_at),
        last_checked_at: record.and_then(|a| a.last_checked_at),
        finished_at: record.and_then(|a| a.finished_at),
        in_downloader: true,
        tags: t.tags.clone(),
        uploaded: t.uploaded,
        ratio: t.ratio,
        seeding_time: t.seeding_time,
        added_on: t.added_on,
        save_path: Some(t.save_path.clone()),
        content_path: t.content_path.clone(),
        downloader: downloader.to_string(),
    }
}

fn aux_from_record(record: &ReseedAttempt, sites: &SiteIndex) -> AuxSeed {
    let site = record.target_site.trim().to_string();
    let site_name = if site.is_empty() {
        String::new()
    } else {
        sites.label(&site).unwrap_or_else(|| site.clone())
    };
    let state = record_only_state(record);
    AuxSeed {
        attempt_id: Some(record.id),
        info_hash: record.target_info_hash.trim().to_ascii_lowercase(),
        site: site.clone(),
        site_name,
        site_source: if site.is_empty() {
            SITE_UNKNOWN
        } else {
            SITE_RECORD
        },
        origin: ORIGIN_RECORD,
        duplicate: false,
        state,
        phase: None,
        progress: 0.0,
        check_count: record.check_count,
        message: record.message.clone(),
        added_at: record.added_at,
        last_checked_at: record.last_checked_at,
        finished_at: record.finished_at,
        in_downloader: false,
        tags: Vec::new(),
        uploaded: 0,
        ratio: 0.0,
        seeding_time: None,
        added_on: None,
        save_path: None,
        content_path: None,
        downloader: record.downloader.clone(),
    }
}

/// 辅种的有效状态：下载器的事实优先于记录。
///
/// 记录说 `seeding`，下载器却报文件缺失——这时候还显示「做种中」就是撒谎，而那正是
/// 这个工具存在的意义：只承认真的在做种。
fn live_state(t: &TorrentInfo, record: Option<&ReseedAttempt>) -> &'static str {
    match t.phase() {
        TorrentPhase::Error | TorrentPhase::MissingFiles => "failed",
        TorrentPhase::Checking => "verifying",
        TorrentPhase::Complete if t.progress >= 0.999 => "seeding",
        TorrentPhase::Complete | TorrentPhase::Incomplete | TorrentPhase::DownloadingMetadata => {
            // 数据不全、或者停在奇怪的中间态：不算成功。此时再看一眼记录，如果记录
            // 已经判过失败，就把失败原因留住。
            if record.is_some_and(record_is_failed) {
                "failed"
            } else {
                "pending"
            }
        }
        TorrentPhase::Unknown => {
            if record.is_some_and(record_is_failed) {
                "failed"
            } else {
                "pending"
            }
        }
    }
}

fn record_only_state(record: &ReseedAttempt) -> &'static str {
    match record.state {
        // 记录说在种 / 在校验，下载器里却没有这个种子：它已经不在了。
        ReseedState::Seeding | ReseedState::Added | ReseedState::Verifying => "missing",
        ReseedState::VerifyFailed
        | ReseedState::MissingFiles
        | ReseedState::FetchFailed
        | ReseedState::AddFailed
        | ReseedState::TimedOut => "failed",
        ReseedState::Pending | ReseedState::Skipped | ReseedState::Fetching => "pending",
    }
}

fn record_is_failed(record: &ReseedAttempt) -> bool {
    matches!(
        record.state,
        ReseedState::VerifyFailed
            | ReseedState::MissingFiles
            | ReseedState::FetchFailed
            | ReseedState::AddFailed
            | ReseedState::TimedOut
    )
}

fn count_aux(aux: &[AuxSeed]) -> GroupCounts {
    let mut counts = GroupCounts {
        aux: aux.len(),
        ..GroupCounts::default()
    };
    for a in aux {
        match a.state {
            "seeding" => counts.seeding += 1,
            "verifying" => counts.verifying += 1,
            "missing" => counts.missing += 1,
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
        counts.torrents +=
            usize::from(g.main.present) + g.aux.iter().filter(|a| a.in_downloader).count();
        if g.counts.aux > 0 {
            counts.with_aux += 1;
        }
        counts.aux_total += g.counts.aux;
        counts.aux_seeding += g.counts.seeding;
        counts.aux_verifying += g.counts.verifying;
        counts.aux_failed += g.counts.failed;
        counts.aux_missing += g.counts.missing;
        if g.main_source == MAIN_INFERRED {
            counts.inferred += 1;
        }
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

/// 过滤 + 搜索。`q` 命中名称、hash、站点、路径、标签。
#[must_use]
pub fn matches(group: &SeedGroup, filter: &str, q: &str) -> bool {
    let by_state = match filter {
        "has_aux" => group.counts.aux > 0,
        "no_aux" => group.state == STATE_NO_AUX,
        "has_failure" => group.state == STATE_DEGRADED,
        "verifying" => group.state == STATE_VERIFYING,
        "main_missing" => group.state == STATE_MAIN_MISSING,
        "inferred" => group.main_source == MAIN_INFERRED,
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
        || group
            .downloaders
            .iter()
            .any(|d| d.to_lowercase().contains(&q))
        || group
            .main
            .tags
            .iter()
            .any(|t| t.to_lowercase().contains(&q))
        || group.aux.iter().any(|a| {
            a.site.to_lowercase().contains(&q)
                || a.site_name.to_lowercase().contains(&q)
                || a.info_hash.to_lowercase().contains(&q)
                || a.tags.iter().any(|t| t.to_lowercase().contains(&q))
        })
}

/// 这组种子里的旧标签：用于把 `iyuu-reseed*` 重写成 `iyuu-rust`。
#[must_use]
pub fn legacy_tags(torrents: &[TorrentInfo]) -> Vec<(String, Vec<String>)> {
    torrents
        .iter()
        .filter_map(|t| {
            let legacy: Vec<String> = t
                .tags
                .iter()
                .filter(|tag| iyuu_core::downloader::is_legacy_aux_tag(tag))
                .cloned()
                .collect();
            (!legacy.is_empty()).then(|| (t.hash.clone(), legacy))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iyuu_core::models::DownloaderKind;

    const MIB: i64 = 1024 * 1024;
    const NAME: &str = "Some.Movie.2024.1080p.WEB-DL.x264-GROUP";

    fn torrent(hash: &str, name: &str, size: i64) -> TorrentInfo {
        torrent_at(hash, name, size, &format!("/data/seed/{name}"))
    }

    fn torrent_at(hash: &str, name: &str, size: i64, content_path: &str) -> TorrentInfo {
        TorrentInfo {
            hash: hash.to_string(),
            name: name.to_string(),
            save_path: "/data/seed".to_string(),
            content_path: Some(content_path.to_string()),
            total_size: size,
            progress: 1.0,
            state: "stalledUP".to_string(),
            kind: DownloaderKind::Qbittorrent,
            category: Some("刷流".to_string()),
            tags: Vec::new(),
            added_on: Some(Utc::now()),
            seeding_time: Some(3600),
            uploaded: 1024,
            // 默认当成「复用已有文件」的副本；需要下载流量的测试自己设置。
            downloaded: 0,
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
            source_name: NAME.to_string(),
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
            downloader: "qBittorrent".to_string(),
            run_id: None,
        }
    }

    fn groups(torrents: &[TorrentInfo], attempts: &[ReseedAttempt]) -> Vec<SeedGroup> {
        build_groups(SeedsInput {
            torrents,
            attempts,
            aux_tags: &["辅种".to_string()],
            sites: &[],
            downloader: "qBittorrent",
        })
    }

    #[test]
    fn plain_downloader_torrent_is_listed() {
        let t = vec![torrent("aaaa", NAME, 8 * MIB)];

        let g = groups(&t, &[]);
        assert_eq!(g.len(), 1, "页面管的是下载器，普通种子也要出现");
        assert_eq!(g[0].state, STATE_NO_AUX);
        assert_eq!(g[0].main.hash, "aaaa");
        assert_eq!(g[0].main_source, MAIN_SINGLE);
        assert!(g[0].aux.is_empty());
    }

    #[test]
    fn records_build_group_and_mark_aux() {
        let t = vec![
            torrent("aaaa", NAME, 8 * MIB),
            torrent("bbbb", NAME, 8 * MIB),
            torrent("cccc", NAME, 8 * MIB),
        ];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "springsunday", ReseedState::Seeding),
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Seeding),
        ];

        let g = groups(&t, &a);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].main.hash, "aaaa");
        assert_eq!(g[0].main_source, MAIN_RECORD);
        assert_eq!(g[0].aux.len(), 2);
        assert_eq!(g[0].counts.seeding, 2);
        assert_eq!(g[0].state, STATE_HEALTHY);
        assert_eq!(g[0].aux[0].site_name, "mteam");
        assert!(g[0].aux.iter().all(|a| a.origin == ORIGIN_RECORD));
    }

    #[test]
    fn content_path_identifies_copies_without_tags_or_records() {
        // 同一份数据、两个站点各自的标题，没有记录、没有标签：靠 content_path 归组。
        let t = vec![
            torrent_at("aaaa", NAME, 8 * MIB, "/data/seed/Some.Movie.2024"),
            torrent_at(
                "bbbb",
                "[中文名].Some.Movie.2024.1080p.WEB-DL",
                8 * MIB,
                "/data/seed/Some.Movie.2024/",
            ),
        ];

        let g = groups(&t, &[]);
        assert_eq!(g.len(), 1, "路径相同就是同一份数据");
        assert_eq!(g[0].aux.len(), 1);
        assert_eq!(g[0].aux[0].origin, ORIGIN_CONTENT);
        assert_eq!(g[0].main_source, MAIN_INFERRED);
        assert_eq!(g[0].counts.aux, 1);
    }

    #[test]
    fn download_traffic_identifies_the_main_seed() {
        // 两份都没有记录和标签；只有一份下载过数据，而且它加入得更晚。
        let mut main = torrent("aaaa", NAME, 8 * MIB);
        main.downloaded = 8 * MIB;
        main.added_on = Some(Utc::now());
        let mut earlier = torrent("bbbb", NAME, 8 * MIB);
        earlier.added_on = Some(Utc::now() - chrono::Duration::days(9));

        let g = groups(&[earlier, main], &[]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].main.hash, "aaaa", "下载过数据的那一份才是主种子");
        assert_eq!(g[0].main_source, MAIN_TRAFFIC);
        assert_eq!(g[0].main.downloaded, 8 * MIB);
    }

    #[test]
    fn foreign_tool_tag_is_recognized() {
        let mut main = torrent("aaaa", NAME, 8 * MIB);
        main.added_on = Some(Utc::now() - chrono::Duration::hours(2));
        let mut aux = torrent("bbbb", NAME, 8 * MIB);
        aux.tags = vec!["辅种".to_string()];

        let g = groups(&[main, aux], &[]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].main.hash, "aaaa", "带「辅种」的那个是副本");
        assert_eq!(g[0].main_source, MAIN_TAG);
        assert_eq!(g[0].aux.len(), 1);
        assert_eq!(g[0].aux[0].origin, ORIGIN_TAG);
    }

    #[test]
    fn legacy_tag_is_recognized() {
        let mut aux = torrent("bbbb", NAME, 8 * MIB);
        aux.tags = vec!["iyuu-reseed:mteam".to_string()];

        let g = groups(&[torrent("aaaa", NAME, 8 * MIB), aux], &[]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].aux.len(), 1);
        assert_eq!(g[0].aux[0].origin, ORIGIN_TAG);
    }

    #[test]
    fn main_deleted_keeps_group_alive() {
        let t = vec![torrent("bbbb", NAME, 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let g = groups(&t, &a);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].state, STATE_MAIN_MISSING);
        assert!(!g[0].main.present);
        assert_eq!(g[0].main.hash, "aaaa");
        assert!(g[0].main.name.contains("Some.Movie"), "名称来自记录快照");
        assert_eq!(g[0].counts.seeding, 1);
    }

    #[test]
    fn failed_aux_marks_group_degraded() {
        // bbbb 校验没通过：下载器报文件缺失，记录也判了失败。
        let mut broken = torrent("bbbb", NAME, 8 * MIB);
        broken.state = "missingFiles".to_string();
        let t = vec![torrent("aaaa", NAME, 8 * MIB), broken];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "springsunday", ReseedState::VerifyFailed),
            // cccc 连种子都不在了，记录却停在 seeding。
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Seeding),
        ];

        let g = groups(&t, &a);
        let g = &g[0];
        assert_eq!(g.state, STATE_DEGRADED);
        assert_eq!(g.counts.failed, 1);
        let missing = g.aux.iter().find(|x| x.info_hash == "cccc").unwrap();
        assert!(!missing.in_downloader);
        assert_eq!(missing.state, "missing", "记录说成功但种子没了，只能算丢失");
        assert_eq!(g.counts.missing, 1);
    }

    #[test]
    fn different_sizes_stay_apart() {
        // 两个不同目录、大小不同的同名资源：大小是硬否定，路径也不同。
        let t = vec![
            torrent_at("aaaa", NAME, 8 * MIB, "/data/a/Some.Movie"),
            torrent_at("bbbb", NAME, 9 * MIB, "/data/b/Some.Movie"),
        ];
        let a = vec![
            attempt(1, "aaaa", "cccc", "springsunday", ReseedState::Seeding),
            attempt(2, "bbbb", "dddd", "mteam", ReseedState::Seeding),
        ];

        let g = groups(&t, &a);
        assert_eq!(g.len(), 2, "大小不同必须分成两组");
    }

    #[test]
    fn same_site_duplicates_are_marked() {
        let t = vec![
            torrent("aaaa", NAME, 8 * MIB),
            torrent("bbbb", NAME, 8 * MIB),
            torrent("cccc", NAME, 8 * MIB),
        ];
        let a = vec![
            attempt(1, "aaaa", "bbbb", "mteam", ReseedState::Seeding),
            attempt(2, "aaaa", "cccc", "mteam", ReseedState::Seeding),
        ];

        let g = groups(&t, &a);
        assert_eq!(g[0].aux.len(), 2);
        assert!(g[0].aux.iter().any(|a| a.duplicate));
        assert_eq!(g[0].aux.iter().filter(|a| !a.duplicate).count(), 1);
    }

    #[test]
    fn live_error_overrides_recorded_success() {
        let mut broken = torrent("bbbb", NAME, 8 * MIB);
        broken.state = "missingFiles".to_string();
        let t = vec![torrent("aaaa", NAME, 8 * MIB), broken];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let g = groups(&t, &a);
        assert_eq!(g[0].state, STATE_DEGRADED, "下载器说文件缺失就不算成功");
        assert_eq!(g[0].counts.failed, 1);
    }

    #[test]
    fn broken_main_is_degraded_not_missing() {
        let mut main = torrent("aaaa", NAME, 8 * MIB);
        main.state = "missingFiles".to_string();
        let t = vec![main, torrent("bbbb", NAME, 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Seeding,
        )];

        let g = groups(&t, &a);
        assert!(g[0].main.present, "种子还在下载器里，只是文件缺了");
        assert_eq!(g[0].state, STATE_DEGRADED);
    }

    #[test]
    fn summarize_counts_torrents_and_flags() {
        let mut checking = torrent("bbbb", NAME, 8 * MIB);
        checking.state = "checkingUP".to_string();
        checking.progress = 0.5;
        let t = vec![
            torrent_at("aaaa", NAME, 8 * MIB, "/data/a/Some.Movie"),
            checking,
            torrent_at("dddd", "Other.Show.S01", 4 * MIB, "/data/c/Other.Show.S01"),
            torrent_at("eeee", "Third.Show.S01", 3 * MIB, "/data/d/Third.Show.S01"),
        ];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::Verifying,
        )];

        let g = groups(&t, &a);
        let s = summarize(&g);
        assert_eq!(s.groups, 3, "aaaa/bbbb 一组，另外两个各自一组");
        assert_eq!(s.torrents, 4);
        assert_eq!(s.with_aux, 1);
        assert_eq!(s.no_aux, 2);
        assert_eq!(s.aux_total, 1);
        assert_eq!(s.aux_verifying, 1);
        assert_eq!(s.inferred, 0);
    }

    #[test]
    fn filters_and_search() {
        let t = vec![torrent("aaaa", NAME, 8 * MIB)];
        let a = vec![attempt(
            1,
            "aaaa",
            "bbbb",
            "springsunday",
            ReseedState::VerifyFailed,
        )];
        let g = &groups(&t, &a)[0];

        assert!(matches(g, "all", ""));
        assert!(matches(g, "has_aux", ""));
        assert!(matches(g, "has_failure", ""));
        assert!(!matches(g, "verifying", ""));
        assert!(matches(g, "all", "springsunday"));
        assert!(matches(g, "all", "SOME.MOVIE"));
        assert!(!matches(g, "all", "nonexistent"));
        assert!(!matches(g, "no_aux", ""));
        assert!(!matches(g, "inferred", ""));
    }

    #[test]
    fn legacy_tags_only_lists_old_markers() {
        let mut a = torrent("aaaa", NAME, 8 * MIB);
        a.tags = vec!["iyuu-reseed".to_string(), "辅种".to_string()];
        let mut b = torrent("bbbb", NAME, 8 * MIB);
        b.tags = vec!["iyuu-rust".to_string()];
        let mut c = torrent("cccc", NAME, 8 * MIB);
        c.tags = vec!["iyuu-reseed:verified".to_string()];

        let found = legacy_tags(&[a, b, c]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "aaaa");
        assert_eq!(found[1].1, vec!["iyuu-reseed:verified".to_string()]);
    }
}
