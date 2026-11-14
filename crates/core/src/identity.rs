//! 「这两个种子是不是同一份数据」的唯一定义处。
//!
//! 跨站辅种的前提是识别：同一个资源在两个站点上是两个不同的 `.torrent`，info hash
//! 必然不同，但**数据**是同一份。工具此前只有 hash 概念，于是「主种子」和它的辅种在
//! 数据结构上毫无关系，页面只能按尝试流水展示。
//!
//! 这里给出三层依据，从强到弱：
//!
//! 1. **大小**：不同大小一定不是同一份数据。这是硬否定，先把候选砍掉一个数量级。
//! 2. **名称归一化**后相等：同一份数据在不同站点的标题通常一字不差。
//! 3. **词元相似度**：站点会各自动标题——加 `[中文名].` 前缀、把 `WEB-DL` 写成
//!    `WEB.DL`、尾部补组名。归一化之后逐词比较，容得下这些差异。
//!
//! 没有「哈希前缀相同」「名称包含」这类模糊规则：误判的代价是把两个不同资源并成一组，
//! 用户照着这组去删种子就会删错文件。

use std::collections::HashSet;

/// 词元 Jaccard 相似度的判定阈值，整数形式：交集 × 100 >= 并集 × 85。
///
/// 用整数比较而不是浮点：`intersection / union` 在长标题上会出现 `0.8499999…`
/// 这种边界抖动，而这里是二值判定，不需要浮点的表达力。
const SIMILARITY_PERCENT: usize = 85;

/// 归一化后的种子名：小写、去掉站点自加的前缀、标点压成单空格。
///
/// 去掉开头连续的 `[...]` / `【...】` 是刻意的：国内站点普遍在标题前面补一段中文名
/// （`[律界战争].WAR.S01...`），而同一资源在别的站点可能没有这段。它不携带片名信息
/// 之外的识别价值，去掉之后两侧才能对齐。
#[must_use]
pub fn name_key(name: &str) -> String {
    let stripped = strip_leading_groups(name);
    let mut out = String::with_capacity(stripped.len());
    let mut pending_space = false;

    for ch in stripped.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            out.push(ch);
            pending_space = false;
        } else {
            // 标点、分隔符、空白统一成一个空格；连续多个只留一个。
            pending_space = true;
        }
    }
    out
}

/// 去掉名称开头连续的 `[...]` 与 `【...】` 段，以及它们之间的空白。
fn strip_leading_groups(name: &str) -> &str {
    let mut rest = name.trim_start();
    loop {
        let mut chars = rest.chars();
        let Some(first) = chars.next() else {
            return rest;
        };
        let close = match first {
            '[' => ']',
            '【' => '】',
            _ => return rest,
        };
        let Some(offset) = rest.find(close) else {
            // 括号没闭合：整串当作普通标题，硬切会吃掉真正的内容。
            return rest;
        };
        rest = rest[offset + close.len_utf8()..].trim_start();
    }
}

/// 归一化名称的词元集合。
#[must_use]
pub fn tokens(name: &str) -> HashSet<String> {
    name_key(name)
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 两个种子是否承载同一份数据。
///
/// `size` 为 0 表示「大小未知」，此时只承认名称完全一致之外的否定结论——也就是说
/// 只要有一侧大小未知就直接返回 `false`。少合并一次只是页面多一组，合错了会让用户
/// 以为两个不相干的资源是一回事。
#[must_use]
pub fn same_release(size_a: i64, name_a: &str, size_b: i64, name_b: &str) -> bool {
    if size_a <= 0 || size_b <= 0 || size_a != size_b {
        return false;
    }

    let key_a = name_key(name_a);
    let key_b = name_key(name_b);
    if key_a.is_empty() || key_b.is_empty() {
        return false;
    }
    if key_a == key_b {
        return true;
    }

    similar_tokens(&key_a, &key_b)
}

/// 词元 Jaccard 相似度是否达到阈值。名称已经在调用方归一化。
fn similar_tokens(key_a: &str, key_b: &str) -> bool {
    let a: HashSet<&str> = key_a.split(' ').filter(|s| !s.is_empty()).collect();
    let b: HashSet<&str> = key_b.split(' ').filter(|s| !s.is_empty()).collect();
    if a.is_empty() || b.is_empty() {
        return false;
    }

    let intersection = a.intersection(&b).count();
    let union = a.union(&b).count();
    // 短标题（两三个词）上一个词的差异会让比例掉得很快，但那往往正是关键差异
    // （`S01` vs `S02`），所以这里不做「短名称放宽」的特殊处理。
    intersection * 100 >= union * SIMILARITY_PERCENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_prefix_is_ignored() {
        let a = "[律界战争].WAR.S01.2026.2160p.MAX.WEB-DL.H265.DDP.5.1.Atmos-CMCTV";
        let b = "WAR.S01.2026.2160p.MAX.WEB-DL.H265.DDP.5.1.Atmos-CMCTV";
        assert!(same_release(6_661_586_546, a, 6_661_586_546, b));
    }

    #[test]
    fn multiple_leading_groups_are_ignored() {
        let a = "【玫瑰战争】[HDR].War.of.Thorns.S01.2026.2160p.WEB-DL.H265.AAC.2Audios-CMCTV";
        let b = "War.of.Thorns.S01.2026.2160p.WEB-DL.H265.AAC.2Audios-CMCTV";
        assert!(same_release(1_184_478_662, a, 1_184_478_662, b));
    }

    #[test]
    fn separators_do_not_matter() {
        assert!(same_release(
            8 * 1024 * 1024,
            "Some.Movie.2024.1080p.WEB-DL.x264-GROUP",
            8 * 1024 * 1024,
            "Some Movie 2024 1080p WEB.DL x264-GROUP"
        ));
    }

    #[test]
    fn different_size_is_a_hard_no() {
        // 名称一模一样也不合并：不同大小的同名文件是不同资源（加长版、不同压制）。
        assert!(!same_release(
            8 * 1024 * 1024,
            "Some.Movie.2024.1080p.WEB-DL.x264-GROUP",
            9 * 1024 * 1024,
            "Some.Movie.2024.1080p.WEB-DL.x264-GROUP"
        ));
    }

    #[test]
    fn unknown_size_is_never_merged() {
        assert!(!same_release(0, "Some.Movie.2024", 0, "Some.Movie.2024"));
    }

    #[test]
    fn different_titles_are_not_merged() {
        assert!(!same_release(
            8 * 1024 * 1024,
            "Some.Movie.2024.1080p.WEB-DL.x264-GROUP",
            8 * 1024 * 1024,
            "Other.Movie.2023.1080p.BluRay.x264-OTHER"
        ));
    }

    #[test]
    fn single_word_difference_is_significant() {
        // S01/S02 只差一个词元，但那是两季不同的剧集，不能靠相似度抹平。
        assert!(!same_release(
            4 * 1024 * 1024 * 1024,
            "Show.S01.2160p.WEB-DL",
            4 * 1024 * 1024 * 1024,
            "Show.S02.2160p.WEB-DL"
        ));
    }

    #[test]
    fn unclosed_bracket_is_kept() {
        assert_eq!(name_key("[未闭合 Show 2024"), "未闭合 show 2024");
    }

    #[test]
    fn name_key_collapses_punctuation() {
        assert_eq!(name_key("A...B -- C__D"), "a b c d");
    }
}
