//! `iyuu-reseed` — IYUU cross-seed service.
//!
//! Serves the JSON API plus the built front end, runs the verification loop, and
//! fires scheduled discovery runs.

mod api;
mod config;
mod db;
mod env_config;
mod error;
mod runner;
mod scheduler;

use crate::config::AppConfig;
use crate::db::Db;
use crate::runner::AppState;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "iyuu-reseed", version, about = "IYUU 自动辅种服务")]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "IYUU_LISTEN", default_value = "0.0.0.0:8098")]
    listen: String,

    /// Data directory (SQLite db + torrent cache).
    #[arg(long, env = "IYUU_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Directory holding the built front end (`index.html`).
    #[arg(long, env = "IYUU_WEB_DIR", default_value = "./web/dist")]
    web_dir: PathBuf,

    /// Log filter, e.g. `info` or `iyuu_core=debug,info`.
    #[arg(long, env = "IYUU_LOG", default_value = "info")]
    log: String,

    /// Validate config and exit — useful in CI and as a container healthcheck.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new(&args.log))
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    std::fs::create_dir_all(&args.data_dir)?;

    let db = Db::open(&args.data_dir.join("iyuu.db"))
        .map_err(|e| anyhow::anyhow!("初始化数据库失败：{e}"))?;
    let cfg = AppConfig::load(&db).map_err(|e| anyhow::anyhow!("加载配置失败：{e}"))?;

    if args.check {
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("配置校验失败：{e}"))?;
        println!("配置有效");
        return Ok(());
    }

    let state = AppState::new(db, cfg);

    // Verification keeps running even with no schedule configured: an attempt
    // added yesterday and interrupted by a restart still needs to be resolved.
    tokio::spawn(runner::verification_loop(state.clone()));

    // Scheduler: restarts cleanly whenever the cron setting changes.
    let sched = scheduler::spawn(state.clone());

    let app = build_router(state.clone(), &args.web_dir);

    let addr: SocketAddr = args
        .listen
        .parse()
        .map_err(|e| anyhow::anyhow!("监听地址无效 {}：{e}", args.listen))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        %addr,
        data_dir = %args.data_dir.display(),
        web_dir = %args.web_dir.display(),
        api_base_override = env_config::api_base().unwrap_or("-"),
        "iyuu-reseed 启动"
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    tokio::select! {
        res = serve => {
            if let Err(e) = res {
                tracing::error!(error = %e, "HTTP 服务异常退出");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到信号，正在关闭");
        }
    }

    sched.shutdown();
    Ok(())
}

fn build_router(state: AppState, web_dir: &std::path::Path) -> axum::Router {
    let api = api::router(state);

    let mut app = axum::Router::new()
        .merge(api)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        // Permissive CORS: the SPA is served from the same origin in production,
        // but `bun dev` runs on another port during development.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Serve the built SPA with an index.html fallback so client-side routes work.
    //
    // `ServeDir::not_found_service` only kicks in for a *missing file*, and its
    // default response for a missing path under a router that already handled
    // the request is a 404 — a deep link like /attempts therefore 404'd and the
    // SPA showed nothing on refresh. Routing everything that is not an API call
    // or an existing asset through index.html is the reliable shape.
    if web_dir.exists() {
        let dir = web_dir.to_path_buf();
        let index_html = std::fs::read(web_dir.join("index.html")).unwrap_or_default();

        app = app.fallback(move |req: axum::extract::Request| {
            let dir = dir.clone();
            let index_html = index_html.clone();
            async move {
                use axum::body::Body;
                use axum::response::IntoResponse;

                let path = req.uri().path().trim_start_matches('/').to_string();

                // Serve a real file when one exists and the request looks like an
                // asset (has an extension). Everything else is a client-side
                // route and gets index.html, so a refresh on /attempts works.
                if !path.is_empty() && path.contains('.') {
                    let candidate = dir.join(&path);
                    // Reject traversal: a `..` segment must never escape web_dir.
                    if !path.split('/').any(|seg| seg == "..")
                        && let Ok(bytes) = tokio::fs::read(&candidate).await
                    {
                        let ctype = guess_content_type(&candidate);
                        return (
                            [(axum::http::header::CONTENT_TYPE, ctype)],
                            Body::from(bytes),
                        )
                            .into_response();
                    }
                }

                (
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    Body::from(index_html),
                )
                    .into_response()
            }
        });
    } else {
        tracing::warn!(
            dir = %web_dir.display(),
            "未找到前端构建产物，仅提供 API（开发时请用 bun dev）"
        );
        app = app.fallback(|| async {
            (
                axum::http::StatusCode::OK,
                "iyuu-reseed API is running. Front end not built.",
            )
        });
    }

    app
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // Either signal means the same thing: shut down.
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Minimal extension→MIME map, covering what a Vite build actually emits.
fn guess_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}
