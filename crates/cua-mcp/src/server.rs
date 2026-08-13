//! The MCP surface.
//!
//! # Tool naming
//!
//! The tool names here (`list_apps`, `get_app_state`, `click`, `type_text`,
//! `press_key`, `scroll`, `drag`, `set_value`, `select_text`,
//! `perform_secondary_action`) are deliberately identical to the ones OpenAI's
//! bundled Codex computer-use plugin exposes. That surface has become the
//! de-facto vocabulary for this capability on macOS, and models have seen it.
//! Inventing a prefixed dialect would buy nothing and cost recognition.
//!
//! # The one invariant
//!
//! `get_app_state` must be called before any action on an app, because it is
//! what produces the `element_index` handles actions refer to. Every action
//! therefore either takes an index from the latest snapshot, or explicit
//! coordinates. There is no "click the button labeled X" tool: resolving a label
//! to an element is the model's job, and doing it server-side would hide the
//! ambiguity that makes automation dangerous.

use std::sync::Arc;

use base64::Engine;
use cua_core::{Cua, ScrollDir, StateOptions, Target};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_router, ErrorData as McpError};
use schemars::JsonSchema;
use serde::Deserialize;

fn ok(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn fail(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

// ── arguments ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct AppArgs {
    /// App name, bundle identifier, or bundle path. `Safari`,
    /// `com.apple.Safari` and `/Applications/Safari.app` all work.
    app: String,
    /// Include a screenshot of the window. Leave on for the first call; turn it
    /// off on follow-up calls, where the tree alone is usually enough and the
    /// image is the expensive part.
    #[serde(default = "yes")]
    include_screenshot: bool,
    /// Longest screenshot edge in pixels. `0` means native resolution.
    #[serde(default)]
    max_image_dim: Option<u32>,
    /// Cap on elements in the tree. Raise it for dense apps, lower it to save
    /// context.
    #[serde(default)]
    max_elements: Option<usize>,
    /// Include layout containers and unlabeled elements that are normally
    /// filtered out. Only useful when an expected control is missing.
    #[serde(default)]
    verbose: bool,
}

fn yes() -> bool {
    true
}

/// Shared by every action tool.
///
/// `element_index` and `x`/`y` are alternatives. Index is strongly preferred:
/// it is what the snapshot advertises, it survives the window moving, and it
/// cannot land on the wrong control because of a stale coordinate.
#[derive(Debug, Deserialize, JsonSchema)]
struct ActionArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    /// Index from the most recent `get_app_state` for this app, e.g. `"12"`.
    #[serde(default)]
    element_index: Option<String>,
    /// Pass the `snapshot_id` from `get_app_state` to make the call fail loudly
    /// if the UI has been re-snapshotted since. Recommended for anything
    /// destructive.
    #[serde(default)]
    snapshot_id: Option<u64>,
    /// Screen x, in points. Only used when `element_index` is absent.
    #[serde(default)]
    x: Option<f32>,
    /// Screen y, in points.
    #[serde(default)]
    y: Option<f32>,
}

impl ActionArgs {
    fn target(&self) -> Result<Target, String> {
        if let Some(raw) = &self.element_index {
            let index: usize = raw
                .trim()
                .parse()
                .map_err(|_| format!("element_index must be a whole number, got {raw:?}"))?;
            return Ok(Target::Index {
                index,
                snapshot_id: self.snapshot_id,
            });
        }
        match (self.x, self.y) {
            (Some(x), Some(y)) => Ok(Target::Point { x, y }),
            _ => Err("pass element_index (preferred) or both x and y".to_string()),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetValueArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// Replacement contents. This writes the element's value directly rather
    /// than typing, so it replaces rather than appends.
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// `up`, `down`, `left` or `right`.
    direction: String,
    /// Number of pages. Defaults to 1.
    #[serde(default)]
    pages: Option<u32>,
}

// ── server ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CuaServer {
    cua: Arc<Cua>,
    tool_router: ToolRouter<Self>,
}

impl CuaServer {
    pub fn new(cua: Arc<Cua>) -> Self {
        Self {
            cua,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a blocking native call without stalling the async runtime.
    ///
    /// Every `Cua` method blocks on the native worker thread's reply. Calling
    /// one directly from an async context would occupy a tokio worker for the
    /// duration of an AX round-trip, so it is moved to the blocking pool.
    async fn native<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&Cua) -> T + Send + 'static,
    {
        let cua = self.cua.clone();
        tokio::task::spawn_blocking(move || f(&cua))
            .await
            .expect("native task panicked")
    }
}

#[tool_router(router = tool_router)]
impl CuaServer {
    #[tool(
        description = "Report whether this server holds the two macOS grants it needs: Accessibility (to read UI structure and deliver actions) and Screen Recording (to capture window images). Never prompts. Call this first when anything fails with a permission error."
    )]
    async fn check_permissions(&self) -> Result<CallToolResult, McpError> {
        match self.native(|c| c.permissions()).await {
            Ok(p) => {
                let mut lines = vec![
                    format!("accessibility:    {}", yes_no(p.accessibility)),
                    format!("screen_recording: {}", yes_no(p.screen_recording)),
                ];
                if !p.accessibility {
                    lines.push(
                        "\nGrant Accessibility in System Settings > Privacy & Security > Accessibility. Add the app that LAUNCHED this server (your terminal or agent app), not the cua-rs binary: macOS attributes the request to the host process."
                            .into(),
                    );
                }
                if !p.screen_recording {
                    lines.push(
                        "\nGrant Screen Recording in System Settings > Privacy & Security > Screen Recording, same host process. The accessibility tree still works without it; only screenshots are unavailable."
                            .into(),
                    );
                }
                Ok(ok(lines.join("\n")))
            }
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "List running applications, frontmost first. Use this to discover the exact name or bundle identifier to pass as `app`. Apps marked `background` have no windows and usually cannot be driven."
    )]
    async fn list_apps(&self) -> Result<CallToolResult, McpError> {
        match self.native(|c| c.list_apps()).await {
            Ok(apps) => {
                let mut out = String::new();
                for a in apps {
                    out.push_str(&format!(
                        "{}{}  {}  pid={}{}\n",
                        if a.active { "* " } else { "  " },
                        a.name,
                        a.bundle_id.as_deref().unwrap_or("-"),
                        a.pid,
                        if a.regular { "" } else { "  (background)" }
                    ));
                }
                out.push_str("\n* = frontmost");
                Ok(ok(out))
            }
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Read one app's front window: returns its accessibility tree and, by default, a screenshot taken from the same moment. This MUST be called before acting on an app, because it assigns the `element_index` handles that click/set_value/scroll refer to. Lines starting with `[N]` are actionable targets; lines without a bracket are context only. Does not activate the app, move the cursor, or change focus, so it is safe to call while the user is working."
    )]
    async fn get_app_state(
        &self,
        Parameters(a): Parameters<AppArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut opts = StateOptions {
            include_screenshot: a.include_screenshot,
            ..Default::default()
        };
        if let Some(d) = a.max_image_dim {
            opts.max_image_dim = d;
        }
        if let Some(n) = a.max_elements {
            opts.limits.max_nodes = n.clamp(1, 20_000);
        }
        if a.verbose {
            opts.render.include_noise = true;
            opts.render.include_frames = true;
        }

        let app = a.app.clone();
        let state = match self.native(move |c| c.get_app_state(&app, opts)).await {
            Ok(s) => s,
            Err(e) => return Ok(fail(e.to_string())),
        };

        let mut header = format!(
            "{} (pid {})  snapshot_id={}\nwindow: {}\nelements: {} total, {} actionable\n",
            state.app.name,
            state.app.pid,
            state.snapshot_id,
            state.window_title.as_deref().unwrap_or("(untitled)"),
            state.node_count,
            state.actionable_count,
        );
        for w in &state.warnings {
            header.push_str(&format!("warning: {w}\n"));
        }

        let mut blocks = vec![ContentBlock::text(format!("{header}\n{}", state.tree))];
        if let Some(shot) = state.screenshot {
            blocks.push(ContentBlock::text(format!(
                "screenshot: {}x{} px, {:.2} px per point",
                shot.width, shot.height, shot.scale
            )));
            blocks.push(ContentBlock::image(
                base64::engine::general_purpose::STANDARD.encode(&shot.png),
                "image/png",
            ));
        }
        Ok(CallToolResult::success(blocks))
    }

    #[tool(
        description = "Activate an element: the equivalent of clicking it. Delivered as an accessibility action (AXPress, falling back to AXPick or AXConfirm depending on what the element supports), so the pointer never moves and the app is never brought to the front. Prefer `element_index` from the latest get_app_state; x/y is a fallback that is still hit-tested to an element rather than posting a mouse event."
    )]
    async fn click(
        &self,
        Parameters(a): Parameters<ActionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.app.clone();
        match self.native(move |c| c.click(&app, target)).await {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Replace a text element's contents by writing its accessibility value directly. This does not synthesize keystrokes, so it works on a background window and does not require focus, but it REPLACES the existing value rather than appending, and apps that only react to real key events (terminals, canvas editors, some games) will ignore it. Only works where get_app_state marked the element `editable`."
    )]
    async fn set_value(
        &self,
        Parameters(a): Parameters<SetValueArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let value = a.value.clone();
        match self
            .native(move |c| c.set_value(&app, target, &value))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Scroll a scrollable element by whole pages, using the accessibility scroll actions rather than wheel events. Target the scroll area or table itself, not the element you want to reveal."
    )]
    async fn scroll(
        &self,
        Parameters(a): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let dir = match a.direction.to_lowercase().as_str() {
            "up" => ScrollDir::Up,
            "down" => ScrollDir::Down,
            "left" => ScrollDir::Left,
            "right" => ScrollDir::Right,
            other => {
                return Ok(fail(format!(
                    "direction must be up/down/left/right, got {other:?}"
                )))
            }
        };
        let app = a.target.app.clone();
        let pages = a.pages.unwrap_or(1).clamp(1, 50);
        match self
            .native(move |c| c.scroll(&app, target, dir, pages))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "granted"
    } else {
        "DENIED"
    }
}

/// Render an action outcome.
///
/// `ui_changed` is reported honestly, including when it is `false`. A `false`
/// here does not always mean the action failed — some controls change nothing
/// observable — but hiding it would let an agent believe every dispatched action
/// took effect, which is the failure mode that makes UI automation untrustworthy.
fn render_action(r: &cua_core::ActionResult) -> String {
    format!(
        "{} on {}\nui_changed: {}{}",
        r.verb,
        r.target,
        r.ui_changed,
        if r.ui_changed {
            ""
        } else {
            "  (no observable change; call get_app_state to check, or the control may be a no-op)"
        }
    )
}

impl rmcp::ServerHandler for CuaServer {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.tool_router
            .call(rmcp::handler::server::tool::ToolCallContext::new(
                self, request, context,
            ))
            .await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            ..Default::default()
        })
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info.name = "cua-rs".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
            "Drives native macOS apps through the Accessibility API. Call get_app_state(app) \
             first: it returns the window's element tree plus a screenshot and assigns the \
             [N] indices that click / set_value / scroll take as element_index. Actions are \
             delivered as accessibility actions, so the cursor never moves, focus never \
             changes, and the app is never brought to the front — you can drive a background \
             window while the user keeps working in another one. Indices are only valid until \
             the next get_app_state for that app; pass snapshot_id to make staleness an error \
             instead of a mis-click."
                .to_string(),
        );
        info
    }
}
