//! `iyuu-core` — the cross-seed engine, downloader adapters, and IYUU API client.
//!
//! Layering:
//!   `bencode`    — byte-exact torrent parsing and info-hash computation
//!   `models`     — domain types and the cross-seed state machine
//!   `identity`   — 「这份数据是不是同一个资源」的跨站识别规则
//!   `group`      — 按资源去重（IYUU 查询前每个资源只问一次）
//!   `iyuu`       — IYUU `OpenAPI` client
//!   `fetch`      — turn (site, `torrent_id`) into a validated .torrent
//!   `downloader` — qBittorrent / Transmission adapters behind one trait
//!   `engine`     — discovery + verification orchestration
//!
//! The server crate owns persistence, HTTP routes, and scheduling; nothing here
//! knows about SQLite or axum.

pub mod bencode;
pub mod downloader;
pub mod engine;
pub mod error;
pub mod fetch;
pub mod group;
pub mod identity;
pub mod iyuu;
pub mod models;
pub mod ratelimit;

pub use error::{CoreError, Result};
