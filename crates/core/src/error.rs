//! Error types shared across the core crate.
//!
//! The shape matters: the API layer maps these to distinct HTTP statuses so the
//! UI can react correctly (re-bind sites vs. retry later vs. fix config), and
//! the run log shows something a human can act on.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("配置错误：{0}")]
    Config(String),

    #[error("HTTP 请求失败（{what}）：{source}")]
    Http {
        what: String,
        #[source]
        source: reqwest::Error,
    },

    /// Any non-success answer from the IYUU API, including its own error codes.
    #[error("IYUU API 错误（{what}），code={code}：{msg}")]
    Upstream {
        what: String,
        code: i64,
        msg: String,
    },

    /// The token is missing, wrong, or has no sites bound to it. This is the
    /// single most common first-run failure, so it gets its own variant.
    #[error("IYUU Token 无效或未绑定站点：{0}")]
    IyuuToken(String),

    /// `sid_sha1` expired mid-run — recoverable by re-reporting.
    #[error("sid_sha1 失效：{0}")]
    SidSha1Expired(String),

    /// IYUU rejected a request for going over its rate/quota limit. A distinct
    /// variant because the correct reaction is "stop calling for a while", not
    /// "retry" and not "the token is broken".
    #[error("IYUU 接口限流或超出配额：{0}")]
    RateLimited(String),

    /// The downloader rejected us (auth, version, endpoint missing).
    #[error("下载器错误（{client}）：{msg}")]
    Downloader { client: String, msg: String },

    /// The downloader rejected our credentials — either a wrong username/password
    /// pair or, for qBittorrent ≥ 5.2, a rejected API key. The message says which.
    #[error("下载器认证失败（{client}）：{msg}")]
    DownloaderAuth { client: String, msg: String },

    /// Could not obtain a usable .torrent file from the target site.
    #[error("获取种子失败（{site}）：{msg}")]
    TorrentFetch { site: String, msg: String },

    #[error("bencode 解析失败：{0}")]
    Bencode(#[from] crate::bencode::BencodeError),

    #[error("JSON 解析失败：{0}")]
    Json(#[from] serde_json::Error),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("其他错误：{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl From<reqwest::Error> for CoreError {
    fn from(e: reqwest::Error) -> Self {
        CoreError::Http {
            what: "未分类请求".into(),
            source: e,
        }
    }
}
