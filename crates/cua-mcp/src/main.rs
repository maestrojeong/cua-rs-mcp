//! `cua-rs` — an MCP server that drives native macOS apps without taking over
//! the machine.
//!
//! Transports mirror the sibling projects: no argument means stdio (what an MCP
//! client spawns), an address means Streamable HTTP.

mod auth;
mod server;

use std::sync::Arc;

use auth::{Token, TokenSource};
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

/// Reject anything reaching `/mcp` without the bearer token.
///
/// A flat 401 with `WWW-Authenticate: Bearer` and a body that says which header
/// is missing — enough for a human debugging their client config, and nothing
/// that distinguishes "wrong token" from "no token", which would turn the
/// endpoint into a guessing aid.
async fn require_bearer(
    axum::extract::State(token): axum::extract::State<Arc<Token>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if token.authorizes(presented) {
        return next.run(request).await;
    }

    tracing::warn!("rejected an unauthenticated request to /mcp");
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
        "cua-rs requires `Authorization: Bearer <token>` on /mcp. The token is printed on \
         stderr at startup, or set by CUA_HTTP_TOKEN.\n",
    )
        .into_response()
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

    // This server can read any window and press any button on the machine.
    // Loopback keeps the network out; it does not keep out another process on
    // this machine, or a web page whose JavaScript can reach 127.0.0.1. Both
    // boundaries are needed, and neither substitutes for the other.
    if !bind_address_is_loopback(&bind) {
        anyhow::bail!(
            "refusing to bind {bind}: cua-rs exposes full desktop control, \
             so it may only listen on loopback"
        );
    }

    let token = Token::resolve()?;
    match token.source {
        TokenSource::Environment => {
            tracing::info!("using the bearer token from {}", auth::TOKEN_ENV);
        }
        // stderr, at startup, once. There is nowhere else to put it: the token
        // is generated after the process starts and is gone when it stops, and
        // a human copying it out of the log is the intended flow. Set
        // CUA_HTTP_TOKEN to keep a stable one instead.
        TokenSource::Generated => {
            tracing::info!(
                "generated a bearer token for this run. Clients must send \
                 `Authorization: Bearer {}` to /mcp. Set {} to pin your own.",
                token.value(),
                auth::TOKEN_ENV
            );
        }
    }

    let config = StreamableHttpServerConfig::default().with_stateful_mode(false);
    let factory_cua = cua.clone();
    let service: StreamableHttpService<CuaServer, NeverSessionManager> = StreamableHttpService::new(
        move || Ok(CuaServer::new(factory_cua.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    // Two routers merged rather than one with a selective middleware, so the
    // set of authenticated paths is a structural fact rather than a condition
    // inside a filter that a later edit could get wrong. `/health` is outside
    // the guarded router; everything under `/mcp` is inside it.
    let guarded = axum::Router::new().nest_service("/mcp", service).layer(
        axum::middleware::from_fn_with_state(Arc::new(token), require_bearer),
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
        .merge(guarded);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("cua-rs MCP server on http://{bind}/mcp");
    axum::serve(listener, router).await?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mode = args
        .iter()
        .find(|a| {
            matches!(
                a.as_str(),
                "--version" | "-V" | "--help" | "-h" | "permissions"
            )
        })
        .cloned();
    let bind = args.iter().find(|a| !a.starts_with('-')).cloned();

    if let Some(a) = mode.as_deref() {
        match a {
            "--version" | "-V" => {
                println!("cua-rs {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "cua-rs {}\n\n\
                     Drive native macOS apps over MCP, through the Accessibility API.\n\
                     Does not move your cursor, change focus, or switch Space.\n\n\
                     USAGE:\n  \
                       cua-rs                 serve MCP over stdio (default)\n  \
                       cua-rs <port|addr>     serve Streamable HTTP on loopback\n  \
                       cua-rs permissions     print grant status and exit\n\n\
                     CLICK DELIVERY:\n  \
                       AXPress/AXPick/AXConfirm first; custom-drawn controls use\n                         \
                       process-routed SkyLight events. Neither path moves the cursor.\n\n\
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

    if let Some(flag) = args.iter().find(|a| a.starts_with('-')) {
        anyhow::bail!("unknown option `{flag}`; run cua-rs --help");
    }

    tracing_subscriber::fmt()
        // Default to `info` rather than `off`: the startup permission warnings
        // are aimed at the human, and a silent server that cannot act because a
        // grant is missing is the single most confusing way this can fail.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // `cua_hid` belongs here too. It is the crate that reports when a
                // synthesized click had to give up waiting for the target to
                // agree it is frontmost — the one signal that distinguishes "the
                // activation notices work and cost nothing" from "they do
                // nothing and every click pays the full budget". Leaving it out
                // of the default filter made that warning unreachable in a
                // server nobody sets RUST_LOG on, which is every server.
                tracing_subscriber::EnvFilter::new("cua_rs=info,cua_mcp=info,cua_hid=info")
            }),
        )
        // stdout is the MCP transport on the stdio path; anything written there
        // that is not a JSON-RPC frame corrupts the session.
        .with_writer(std::io::stderr)
        .init();

    let cua = Arc::new(Cua::new());
    warn_about_permissions(&cua);

    let bind = bind.filter(|a| a != "stdio");
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
