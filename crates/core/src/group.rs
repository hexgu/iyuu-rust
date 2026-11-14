//! 以「同一份数据」为单位的去重。
//!
//! IYUU 的 `/reseed/index/index` 是按 info hash 查的：同一份数据在主站和各个辅种站
//! 上是不同的 hash，一次查询就能把全部站点找出来。所以把辅种副本也拿去查，是在拿同一
//! 份数据的重复提问消耗额度，答案还完全一样。
//!
//! 这里只做一件事：把一批种子按资源分组，每组留一个代表。分组依据与「种子管理」页
//! 一致（位置身份 → 大小 + 名称），但不掺下载器记录——查询前还没必要知道谁是主种子，
//! 只要不重复提问就行。代表的选择优先「没被标成辅种、校验通过、加入更早」，这样被
//! 拿去查的通常是主种子。

use crate::downloader::has_aux_tag;
use crate::identity::{content_key, same_release};
use crate::models::TorrentInfo;
use std::collections::{HashMap, HashSet};

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

/// 每个种子所属资源组的编号（同一组返回同一个值；编号本身不稳定，只用于分组）。
#[must_use]
pub fn release_roots(torrents: &[&TorrentInfo]) -> Vec<usize> {
    let mut dsu = DisjointSet::new(torrents.len());

    // 位置身份：路径 + 大小。路径相同、大小不同说明文件集不一样，宁可分开。
    let mut by_path: HashMap<(String, i64), usize> = HashMap::new();
    for (i, t) in torrents.iter().enumerate() {
        let Some(path) = content_key(&t.save_path, t.content_path.as_deref(), &t.name) else {
            continue;
        };
        let key = (path, t.total_size);
        if let Some(&first) = by_path.get(&key) {
            dsu.union(first, i);
        } else {
            by_path.insert(key, i);
        }
    }

    // 数据身份：大小相同 + 名称够像。分桶把 O(n²) 砍成几个小桶内的比较。
    let mut buckets: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, t) in torrents.iter().enumerate() {
        if t.total_size > 0 {
            buckets.entry(t.total_size).or_default().push(i);
        }
    }
    for bucket in buckets.values() {
        for (pos, &a) in bucket.iter().enumerate() {
            for &b in &bucket[..pos] {
                if same_release(
                    torrents[a].total_size,
                    &torrents[a].name,
                    torrents[b].total_size,
                    &torrents[b].name,
                ) {
                    dsu.union(a, b);
                }
            }
        }
    }

    (0..torrents.len()).map(|i| dsu.find(i)).collect()
}

/// 每组保留一个代表，按输入顺序返回。
///
/// `known_sources` 是辅种记录里出现过的 `source_hash`：那是「哪一份是原件」最硬的
/// 历史证据。不传的话只能按加入时间猜，重装/重新添加过的种子会被副本抢走代表位。
#[must_use]
pub fn primary_sources<'a, S>(
    torrents: &[&'a TorrentInfo],
    aux_tags: &[String],
    known_sources: &HashSet<String, S>,
) -> Vec<&'a TorrentInfo>
where
    S: std::hash::BuildHasher,
{
    let roots = release_roots(torrents);
    let mut best: HashMap<usize, usize> = HashMap::new();
    for (i, &root) in roots.iter().enumerate() {
        best.entry(root)
            .and_modify(|j| {
                if more_primary(torrents[i], torrents[*j], aux_tags, known_sources) {
                    *j = i;
                }
            })
            .or_insert(i);
    }

    let chosen: HashSet<usize> = best.values().copied().collect();
    torrents
        .iter()
        .enumerate()
        .filter(|(i, _)| chosen.contains(i))
        .map(|(_, t)| *t)
        .collect()
}

/// `a` 是否比 `b` 更像「用户自己下的那一份」。
///
/// 顺序：不是辅种 → 校验通过 → 加入更早 → 上传更多。"Not tagged as a copy" first
/// because querying a copy answers exactly the same thing while burning the same quota,
/// but the record it produces should point at the original.
fn more_primary<S>(
    a: &TorrentInfo,
    b: &TorrentInfo,
    aux_tags: &[String],
    known_sources: &HashSet<String, S>,
) -> bool
where
    S: std::hash::BuildHasher,
{
    let key = |t: &TorrentInfo| {
        (
            u8::from(!known_sources.contains(&t.hash.trim().to_ascii_lowercase())),
            // 标签先于流量：一个带辅种标签却下载过数据的种子，多半是校验失败后
            // 重新下载的坏副本，不能拿它当来源写进记录。
            u8::from(has_aux_tag(&t.tags, aux_tags)),
            u8::from(t.downloaded <= 0),
            u8::from(!t.is_verified_complete()),
            t.added_on.map_or(i64::MAX, |d| d.timestamp()),
            -t.uploaded,
        )
    };
    key(a) < key(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DownloaderKind;
    use chrono::{Duration, Utc};

    const MIB: i64 = 1024 * 1024;

    fn torrent(hash: &str, name: &str, size: i64, path: &str) -> TorrentInfo {
        TorrentInfo {
            hash: hash.to_string(),
            name: name.to_string(),
            save_path: "/data".to_string(),
            content_path: Some(path.to_string()),
            total_size: size,
            progress: 1.0,
            state: "stalledUP".to_string(),
            kind: DownloaderKind::Qbittorrent,
            category: None,
            tags: Vec::new(),
            added_on: Some(Utc::now()),
            seeding_time: Some(1),
            uploaded: 0,
            downloaded: 0,
            ratio: 0.0,
            error: None,
            trackers: Vec::new(),
        }
    }

    #[test]
    fn same_content_yields_one_representative() {
        let a = torrent(
            "aaaa",
            "Some.Movie.2024.1080p.WEB-DL",
            8 * MIB,
            "/data/Some.Movie.2024",
        );
        let mut b = torrent(
            "bbbb",
            "[中文名].Some.Movie.2024.1080p.WEB-DL",
            8 * MIB,
            "/data/Some.Movie.2024",
        );
        b.tags = vec!["辅种".to_string()];

        let picked = primary_sources(&[&a, &b], &["辅种".to_string()], &HashSet::new());
        assert_eq!(picked.len(), 1, "同一份数据只该查一次");
        assert_eq!(picked[0].hash, "aaaa", "没带辅种标签的当代表");
    }

    #[test]
    fn different_sizes_are_queried_separately() {
        let a = torrent("aaaa", "Some.Movie.2024", 8 * MIB, "/data/a");
        let b = torrent("bbbb", "Some.Movie.2024", 16 * MIB, "/data/b");

        assert_eq!(primary_sources(&[&a, &b], &[], &HashSet::new()).len(), 2);
    }

    #[test]
    fn untagged_copy_loses_to_the_earlier_addition() {
        let mut main = torrent("aaaa", "Some.Movie.2024", 8 * MIB, "/data/x");
        main.added_on = Some(Utc::now() - Duration::days(3));
        let aux = torrent("bbbb", "Some.Movie.2024", 8 * MIB, "/data/x");

        let picked = primary_sources(&[&aux, &main], &[], &HashSet::new());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].hash, "aaaa");
    }

    #[test]
    fn known_source_beats_the_earlier_copy() {
        let mut main = torrent("aaaa", "Some.Movie.2024", 8 * MIB, "/data/x");
        main.added_on = Some(Utc::now());
        let mut aux = torrent("bbbb", "Some.Movie.2024", 8 * MIB, "/data/x");
        aux.added_on = Some(Utc::now() - Duration::days(10));

        let known: HashSet<String> = ["aaaa".to_string()].into_iter().collect();
        let picked = primary_sources(&[&aux, &main], &[], &known);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].hash, "aaaa", "记录指名的主种子优先");
    }

    #[test]
    fn download_traffic_marks_the_original() {
        // 主种子是后来重新添加的（added_on 更晚），但只有它下载过数据。
        let mut main = torrent("aaaa", "Some.Movie.2024", 8 * MIB, "/data/x");
        main.added_on = Some(Utc::now());
        main.downloaded = 8 * MIB;
        let aux = torrent("bbbb", "Some.Movie.2024", 8 * MIB, "/data/x");

        let picked = primary_sources(&[&aux, &main], &[], &HashSet::new());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].hash, "aaaa", "有下载流量的那一份才是原件");
    }

    #[test]
    fn lone_torrent_kept() {
        let a = torrent("aaaa", "Some.Movie.2024", 8 * MIB, "/data/x");
        assert_eq!(primary_sources(&[&a], &[], &HashSet::new()).len(), 1);
    }
}
