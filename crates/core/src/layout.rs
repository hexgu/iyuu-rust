//! 候选 `.torrent` 的文件布局。
//!
//! 预检要回答的问题只有一个：把这个 `.torrent` 交给下载器，它会在磁盘上找哪些文件？
//! 答案完全由 `info` 字典决定——顶层名字决定目录，`files` 列表决定目录里的东西。
//! 这里只解出布局，不算哈希：真正的判定永远是下载器的 piece 校验，我们只在它之前
//! 拦下**必然**失败的候选。
//!
//! 两个必须有意识处理的细节：
//!
//!  * **BEP-47 padding 文件**（`attr` 含 `p`）不是真实数据，qBittorrent 不会把它列进
//!    `torrents/files`。不过滤的话，每个带 padding 的候选都会被判成「候选多出文件」，
//!    预检反而成了误伤源。
//!  * **v2 / 混合种子**没有 v1 的 `files` 列表，只有 `file tree`。这里不假装能读它，
//!    直接返回错误，让调用方放行给下载器自己判断——「不知道」不能当成「不匹配」。

use crate::bencode::Value;
use crate::error::{CoreError, Result};

/// 布局里的一个文件。`path` 相对种子顶层目录，多级用 `/` 分隔。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutFile {
    pub path: String,
    pub size: i64,
}

/// 候选种子的文件布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentLayout {
    /// `info.name`：多文件种子的顶层目录名，单文件种子的文件名。
    pub root: String,
    /// `info.piece length`，仅用于诊断：piece 大小不同不影响校验能否通过，
    /// 因为 piece hash 是对同一份数据算的。
    pub piece_length: i64,
    pub single_file: bool,
    /// 不含顶层目录的文件列表。单文件种子里只有一项，`path` 为空串。
    pub files: Vec<LayoutFile>,
}

/// 解析种子文件的布局。
///
/// # Errors
/// 输入不是合法 bencode、缺少 `info`/`info.name`、既没有 `files` 也没有 `length`，
/// 或者这是 v2/混合种子（没有 v1 文件列表）。
pub fn parse(bytes: &[u8]) -> Result<TorrentLayout> {
    let doc = crate::bencode::decode(bytes)?;
    let info = doc
        .get("info")
        .ok_or_else(|| CoreError::Other("种子缺少 info 字典".into()))?;

    if info.get("meta version").and_then(Value::as_int) == Some(2)
        || info.get("file tree").is_some()
    {
        return Err(CoreError::Other(
            "v2/混合种子没有 v1 文件列表，无法预检".into(),
        ));
    }

    let root = info
        .get("name")
        .and_then(Value::as_str_lossy)
        .ok_or_else(|| CoreError::Other("种子缺少 info.name".into()))?;
    let piece_length = info
        .get("piece length")
        .and_then(Value::as_int)
        .unwrap_or_default();

    if let Some(entries) = info.get("files").and_then(Value::as_list) {
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            if is_padding(entry) {
                continue;
            }
            let size = entry
                .get("length")
                .and_then(Value::as_int)
                .unwrap_or_default();
            // 不用 `.map(..).unwrap_or_default()`：clippy 1.100 会要求换成
            // `map_or_default`。写成 match 是让「路径缺失」这个分支显式地留在
            // 代码里，而不是被一个默认值吞掉。
            let mut path = match entry.get("path").and_then(Value::as_list) {
                Some(segs) => segs
                    .iter()
                    .filter_map(Value::as_str_lossy)
                    .collect::<Vec<_>>()
                    .join("/"),
                None => String::new(),
            };
            if let Some(rest) = path.strip_prefix("./") {
                path = rest.to_string();
            }
            // 少数种子把顶层目录重复写进了 path 的第一段。
            if let Some(rest) = path
                .strip_prefix(root.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
            {
                path = rest.to_string();
            }
            files.push(LayoutFile { path, size });
        }
        return Ok(TorrentLayout {
            root,
            piece_length,
            single_file: false,
            files,
        });
    }

    if let Some(size) = info.get("length").and_then(Value::as_int) {
        return Ok(TorrentLayout {
            root,
            piece_length,
            single_file: true,
            files: vec![LayoutFile {
                path: String::new(),
                size,
            }],
        });
    }

    Err(CoreError::Other("种子既没有 files 也没有 length".into()))
}

/// BEP-47 padding 条目：`attr` 里带 `p`。
fn is_padding(entry: &Value) -> bool {
    entry
        .get("attr")
        .and_then(Value::as_bytes)
        .is_some_and(|attr| attr.contains(&b'p'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个最小可用的 `.torrent`。`extra` 是 `info` 字典里除 `name` 之外的
    /// 键值（已经编码好，末尾不带 `e`）；`announce` 只是让它长得像个真种子。
    ///
    /// 手写字面量很容易把 `6:pieces40:` 的字节数点错，而错了只会报
    /// `unexpected end of input`，看不出是测试写错了。所以长度在这里算。
    fn torrent(name: &str, extra: &str) -> Vec<u8> {
        let mut info = format!("d4:name{}:{name}", name.len()).into_bytes();
        info.extend_from_slice(extra.as_bytes());
        info.push(b'e');
        let mut out = b"d8:announce3:uri4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    #[test]
    fn parses_multi_file_and_strips_the_root_segment() {
        let doc = torrent(
            "Root",
            "5:filesl\
             d6:lengthi100e4:pathl4:Subs5:a.assee\
             d6:lengthi200e4:pathl4:Main5:b.mkvee\
             e12:piece lengthi262144e6:pieces0:",
        );
        let layout = parse(&doc).unwrap();
        assert_eq!(layout.root, "Root");
        assert!(!layout.single_file);
        assert_eq!(layout.piece_length, 262_144);
        assert_eq!(
            layout.files,
            vec![
                LayoutFile {
                    path: "Subs/a.ass".into(),
                    size: 100
                },
                LayoutFile {
                    path: "Main/b.mkv".into(),
                    size: 200
                },
            ]
        );
    }

    #[test]
    fn parses_single_file_as_one_empty_path() {
        let doc = torrent("one.bin", "6:lengthi42e12:piece lengthi16384e6:pieces0:");
        let layout = parse(&doc).unwrap();
        assert_eq!(layout.root, "one.bin");
        assert!(layout.single_file);
        assert_eq!(
            layout.files,
            vec![LayoutFile {
                path: String::new(),
                size: 42
            }]
        );
    }

    #[test]
    fn skips_bep47_padding_entries() {
        // qB 不会把这些算进 torrents/files；算进来的话，每个带 padding 的候选
        // 都会被误判成「候选多出文件」，预检就成了误伤源。
        let doc = torrent(
            "R",
            "5:filesl\
             d6:lengthi100e4:pathl1:aee\
             d4:attr1:p6:lengthi16e4:pathl12:padded_file_ee\
             d6:lengthi200e4:pathl1:beee12:piece lengthi16384e6:pieces0:",
        );
        let layout = parse(&doc).unwrap();
        let paths: Vec<&str> = layout.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "b"]);
    }

    #[test]
    fn strips_a_root_that_is_repeated_inside_the_path() {
        let doc = torrent(
            "Root",
            "5:filesl\
             d6:lengthi100e4:pathl4:Root1:aeee12:piece lengthi16384e6:pieces0:",
        );
        let layout = parse(&doc).unwrap();
        assert_eq!(layout.files[0].path, "a");
    }

    #[test]
    fn rejects_v2_and_hybrid_torrents() {
        let v2 = torrent("R", "12:meta versioni2e12:piece lengthi16384e6:pieces0:");
        let err = parse(&v2).unwrap_err().to_string();
        assert!(err.contains("v2"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_a_torrent_without_files_or_length() {
        let doc = torrent("R", "12:piece lengthi16384e6:pieces0:");
        assert!(parse(&doc).is_err());
    }
}
