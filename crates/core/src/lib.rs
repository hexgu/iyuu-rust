//! `iyuu-core` — the cross-seed engine, downloader adapters, and IYUU API client.
//!
//! Layering:
//!   `bencode`    — byte-exact torrent parsing and info-hash computation
//!   `models`     — domain types and the cross-seed state machine
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
pub mod iyuu;
pub mod models;
pub mod ratelimit;

pub use error::{CoreError, Result};
