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
use cua_core::{Cua, Presence, ScrollDir, StateOptions, Target};
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
    /// Summarize large deep subtrees as `(+N elements)` instead of expanding
    /// them. Turn this on first for a dense app, then drill in.
    #[serde(default)]
    skeleton: bool,
    /// Walk from this element index (from the app's most recent snapshot)
    /// instead of from the window. This is how you expand a subtree that
    /// `skeleton` collapsed.
    #[serde(default)]
    scope_element_id: Option<String>,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeTextArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// Text to append after the element's current contents.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SelectTextArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// Literal substring to select, exactly as it appears in the tree.
    text: String,
    /// Text immediately before the target, to pick one of several occurrences.
    #[serde(default)]
    prefix: Option<String>,
    /// Text immediately after the target.
    #[serde(default)]
    suffix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressKeyArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// Key or chord: `return`, `escape`, `up`, `down`, or with --allow-hid any
    /// chord such as `cmd+shift+p`, `f5`, `ctrl+alt+delete`.
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SecondaryActionArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// AX action name, e.g. `AXShowMenu`, `AXRaise`, `AXIncrement`.
    action: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    /// Substring to look for in labels, values and roles.
    text: String,
    /// Maximum matches to return. Defaults to 20.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitForArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    /// Substring to wait for.
    text: String,
    /// Wait for the text to *disappear* instead of appear.
    #[serde(default)]
    gone: Option<bool>,
    /// Give up after this many milliseconds. Defaults to 5000, capped at 60000.
    #[serde(default)]
    timeout_ms: Option<u64>,
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
        description = "Read one app's front window: returns its accessibility tree and, by default, a screenshot taken from the same moment. For a dense app, pass skeleton=true first: large deep subtrees collapse to `(+N elements — pass scope_element_id=K to expand)`, which keeps the overall map cheap, then call again with scope_element_id=K to spend the whole element budget inside just that subtree. This MUST be called before acting on an app, because it assigns the `element_index` handles that click/set_value/scroll refer to. Lines starting with `[N]` are actionable targets; lines without a bracket are context only. Does not activate the app, move the cursor, or change focus, so it is safe to call while the user is working."
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
        opts.render.skeleton = a.skeleton;
        if let Some(raw) = &a.scope_element_id {
            match raw.trim().parse::<usize>() {
                Ok(i) => opts.scope = Some(i),
                Err(_) => {
                    return Ok(fail(format!(
                        "scope_element_id must be a whole number, got {raw:?}"
                    )))
                }
            }
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

    #[tool(
        description = "Append text to a text element. Unlike set_value this preserves what is already there: it collapses the caret at the end and writes through AXSelectedText, falling back to a whole-value rewrite when the element exposes no settable selection (the result says which happened). Delivered through the accessibility API, so it needs no focus and does not move the cursor — but for the same reason apps that only react to real key events (terminals, canvas editors) will ignore it."
    )]
    async fn type_text(
        &self,
        Parameters(a): Parameters<TypeTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let text = a.text.clone();
        match self.native(move |c| c.type_text(&app, target, &text)).await {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Select a literal substring inside a text element, so a following type_text overwrites exactly that span. Pass `prefix` and/or `suffix` to disambiguate a repeated string: the search matches prefix+text+suffix and selects only the `text` part. Without them the first occurrence wins. Returns the character range that was selected."
    )]
    async fn select_text(
        &self,
        Parameters(a): Parameters<SelectTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let (text, prefix, suffix) = (a.text.clone(), a.prefix.clone(), a.suffix.clone());
        match self
            .native(move |c| c.select_text(&app, target, &text, prefix, suffix))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Press a key on an element. `return`/`enter` and `escape` map to the accessibility verbs AXConfirm and AXCancel, and `up`/`down` to AXIncrement/AXDecrement on steppers and sliders — those work in the background without touching the cursor. Any other key or chord (cmd+shift+p, f5, ctrl+alt+delete) has NO accessibility equivalent and can only be sent as a real key event, which moves the cursor and steals focus; that is refused unless the server was started with --allow-hid. Every result reports `delivery: ax` or `delivery: hid` so you always know which happened."
    )]
    async fn press_key(
        &self,
        Parameters(a): Parameters<PressKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let key = a.key.clone();
        match self.native(move |c| c.press_key(&app, target, &key)).await {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Deliver an arbitrary accessibility action to an element by name, for the verbs the dedicated tools do not cover: AXShowMenu (open a context menu), AXRaise (bring a window forward without activating the app), AXIncrement / AXDecrement (steppers and sliders), AXScrollToVisible, AXCancel. If the element does not advertise the action, the error lists the ones it does — so a wrong guess is fixable in one step. get_app_state also shows each element's actions."
    )]
    async fn perform_secondary_action(
        &self,
        Parameters(a): Parameters<SecondaryActionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let action = a.action.clone();
        match self
            .native(move |c| c.perform_action(&app, target, &action))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Search the current snapshot for elements whose label, value or role contains a string, case-insensitively. Much cheaper than re-reading a whole tree when you already know what you are looking for. Actionable matches are listed first, and label matches rank above value and role matches. Searches the snapshot you already have so the returned indices stay valid; takes a fresh one only if none exists."
    )]
    async fn find(&self, Parameters(a): Parameters<FindArgs>) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let text = a.text.clone();
        let limit = a.limit.unwrap_or(20).clamp(1, 200);
        match self.native(move |c| c.find(&app, &text, limit)).await {
            Ok(r) => {
                if r.lines.is_empty() {
                    return Ok(ok(format!(
                        "no element matching {:?} among {} elements (snapshot {})",
                        a.text, r.searched, r.snapshot_id
                    )));
                }
                Ok(ok(format!(
                    "{} match(es) for {:?}  snapshot_id={}\n\n{}",
                    r.total,
                    a.text,
                    r.snapshot_id,
                    r.lines.join("\n")
                )))
            }
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Poll an app until a string appears in (or disappears from) its accessibility tree, or the timeout expires. Use this instead of guessing a sleep after an action that triggers loading, a dialog, or a navigation. Each poll re-walks the tree, so the interval is floored at 250ms. Returns whether the condition was met plus the snapshot_id of the final read."
    )]
    async fn wait_for(
        &self,
        Parameters(a): Parameters<WaitForArgs>,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let text = a.text.clone();
        let want = if a.gone.unwrap_or(false) {
            Presence::Disappears
        } else {
            Presence::Appears
        };
        // Capped: a tool call that can block for minutes will hit the MCP
        // client's own timeout and strand the poll loop.
        let timeout_ms = a.timeout_ms.unwrap_or(5_000).clamp(250, 60_000);
        match self
            .native(move |c| c.wait_for(&app, &text, want, timeout_ms))
            .await
        {
            Ok(r) => {
                let verb = if want == Presence::Appears {
                    "appeared"
                } else {
                    "disappeared"
                };
                let head = if r.satisfied {
                    format!("{:?} {verb} after {}ms", a.text, r.elapsed_ms)
                } else {
                    format!(
                        "timed out after {}ms: {:?} never {verb}",
                        r.elapsed_ms, a.text
                    )
                };
                let body = format!("{head}\npolls: {}\nsnapshot_id: {}", r.polls, r.snapshot_id);
                if r.satisfied {
                    Ok(ok(body))
                } else {
                    Ok(fail(body))
                }
            }
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
    let mut s = format!(
        "{} on {}\ndelivery: {}{}\nui_changed: {}",
        r.verb,
        r.target,
        r.delivery.as_str(),
        if r.delivery == cua_core::Delivery::Hid {
            "  (a real key event: the cursor and keyboard focus were used, exactly as if the user pressed the keys)"
        } else {
            "  (accessibility action: cursor, focus and frontmost app untouched)"
        },
        r.ui_changed,
    );
    if !r.ui_changed {
        s.push_str(
            "  (no observable change; call get_app_state to check, or the control may be a no-op)",
        );
    }
    s
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
