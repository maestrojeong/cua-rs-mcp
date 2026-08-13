//! `cua-rs` — an MCP server that drives native macOS apps without taking over
//! the machine.
//!
//! Transports mirror the sibling projects: no argument means stdio (what an MCP
//! client spawns), an address means Streamable HTTP.

mod server;

use std::sync::Arc;

use cua_core::Cua;
use server::CuaServer;

fn bind_address_is_loopback(bind: &str) -> bool {
    let Some((host, _port)) = bind.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback())
}

/// Report missing grants once, at startup, on stderr.
///
/// Worth doing even though every tool reports its own permission errors: an MCP
/// client shows tool errors to the model, not to the human, and the human is the
/// only one who can actually open System Settings.
fn warn_about_permissions(cua: &Cua) {
    match cua.permissions() {
        Ok(p) => {
            if !p.accessibility {
                tracing::warn!(
                    "Accessibility permission is NOT granted. Grant it to the app that launched \
                     this server (terminal or agent app) in System Settings > Privacy & Security \
                     > Accessibility, then restart."
                );
            }
            if !p.screen_recording {
                tracing::warn!(
                    "Screen Recording permission is NOT granted. The accessibility tree will \
                     work; screenshots will not."
                );
            }
        }
        Err(e) => tracing::warn!("could not check permissions: {e}"),
    }
}

async fn serve_stdio(cua: Arc<Cua>) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let service = CuaServer::new(cua).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn serve_http(cua: Arc<Cua>, addr: &str) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let bind = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("127.0.0.1:{addr}")
    };

    // This server can read any window and press any button on the machine. It
    // has no authentication of its own, so binding it anywhere reachable would
    // hand the desktop to the network.
    if !bind_address_is_loopback(&bind) {
        anyhow::bail!(
            "refusing to bind {bind}: cua-rs exposes full desktop control and has no auth, \
             so it may only listen on loopback"
        );
    }

    let config = StreamableHttpServerConfig::default().with_stateful_mode(false);
    let factory_cua = cua.clone();
    let service: StreamableHttpService<CuaServer, NeverSessionManager> = StreamableHttpService::new(
        move || Ok(CuaServer::new(factory_cua.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    let router = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "ok": true,
                    "name": "cua-rs",
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            }),
        )
        .nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("cua-rs MCP server on http://{bind}/mcp");
    axum::serve(listener, router).await?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1);
    if let Some(a) = arg.as_deref() {
        match a {
            "--version" | "-V" => {
                println!("cua-rs {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "cua-rs {}\n\n\
                     Drive native macOS apps over MCP, through the Accessibility API.\n\n\
                     USAGE:\n  \
                       cua-rs                 serve MCP over stdio (default)\n  \
                       cua-rs <port|addr>     serve Streamable HTTP on loopback\n  \
                       cua-rs permissions     print grant status and exit\n\n\
                     Requires Accessibility, and Screen Recording for screenshots.",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "permissions" => {
                let cua = Cua::new();
                let p = cua.permissions()?;
                println!("accessibility:    {}", p.accessibility);
                println!("screen_recording: {}", p.screen_recording);
                return Ok(());
            }
            _ => {}
        }
    }

    tracing_subscriber::fmt()
        // Default to `info` rather than `off`: the startup permission warnings
        // are aimed at the human, and a silent server that cannot act because a
        // grant is missing is the single most confusing way this can fail.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("cua_rs=info,cua_mcp=info")),
        )
        // stdout is the MCP transport on the stdio path; anything written there
        // that is not a JSON-RPC frame corrupts the session.
        .with_writer(std::io::stderr)
        .init();

    let cua = Arc::new(Cua::new());
    warn_about_permissions(&cua);

    let bind = arg.filter(|a| a != "stdio");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match bind {
            Some(addr) => serve_http(cua, &addr).await,
            None => serve_stdio(cua).await,
        }
    })
}
