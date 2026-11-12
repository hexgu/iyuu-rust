//! Server-level error type and its HTTP mapping.
//!
//! The mapping matters for the UI: a bad token must not look like a transient
//! network blip, and a downloader auth failure must not look like "no results".

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum ServerError {
    /// Bad or missing IYUU token / no sites bound → action required by the user.
    IyuuAuth(String),
    /// Misconfiguration (missing downloader, empty field, ...).
    Config(String),
    /// Downloader reachable but rejected credentials.
    DownloaderAuth(String),
    /// Downloader unreachable / errored.
    Downloader(String),
    Db(String),
    NotFound(String),
    /// Anything else.
    Internal(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IyuuAuth(m) => write!(f, "IYUU 认证失败：{m}"),
            Self::Config(m) => write!(f, "配置错误：{m}"),
            Self::DownloaderAuth(m) => write!(f, "下载器认证失败：{m}"),
            Self::Downloader(m) => write!(f, "下载器错误：{m}"),
            Self::Db(m) => write!(f, "数据库错误：{m}"),
            Self::NotFound(m) => write!(f, "未找到：{m}"),
            Self::Internal(m) => write!(f, "内部错误：{m}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl ServerError {
    /// Machine-readable kind, so the front end can branch without parsing prose.
    fn kind(&self) -> &'static str {
        match self {
            Self::IyuuAuth(_) => "iyuu_auth",
            Self::Config(_) => "config",
            Self::DownloaderAuth(_) => "downloader_auth",
            Self::Downloader(_) => "downloader",
            Self::Db(_) => "db",
            Self::NotFound(_) => "not_found",
            Self::Internal(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::IyuuAuth(_) | Self::DownloaderAuth(_) => StatusCode::BAD_GATEWAY,
            Self::Config(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "kind": self.kind(),
                "message": self.to_string(),
            }
        });
        (self.status(), Json(body)).into_response()
    }
}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::Internal(format!("IO：{e}"))
    }
}

impl From<rusqlite::Error> for ServerError {
    fn from(e: rusqlite::Error) -> Self {
        ServerError::Db(e.to_string())
    }
}

impl From<r2d2::Error> for ServerError {
    fn from(e: r2d2::Error) -> Self {
        ServerError::Db(format!("连接池：{e}"))
    }
}

impl From<serde_json::Error> for ServerError {
    fn from(e: serde_json::Error) -> Self {
        ServerError::Internal(format!("JSON：{e}"))
    }
}

impl From<iyuu_core::CoreError> for ServerError {
    fn from(e: iyuu_core::CoreError) -> Self {
        use iyuu_core::CoreError as C;
        match e {
            C::IyuuToken(m) => ServerError::IyuuAuth(m),
            C::Config(m) => ServerError::Config(m),
            C::DownloaderAuth { msg, .. } => ServerError::DownloaderAuth(msg),
            C::Downloader { msg, .. } => ServerError::Downloader(msg),
            C::Upstream { what, code, msg } => {
                ServerError::Internal(format!("IYUU {what} 失败（code={code}）：{msg}"))
            }
            C::SidSha1Expired(m) => ServerError::IyuuAuth(m),
            C::TorrentFetch { site, msg } => {
                ServerError::Downloader(format!("获取种子失败（{site}）：{msg}"))
            }
            other => ServerError::Internal(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, ServerError>;
