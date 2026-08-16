//! The MCP surface.
//!
//! # Tool naming
//!
//! The tool names here (`list_apps`, `get_app_state`, `click`, `type_text`,
//! `press_key`, `scroll`, `drag`, `hover`, `set_value`, `select_text`,
//! `perform_secondary_action`) deliberately match the vocabulary that has become
//! de-facto standard for this capability on macOS, which models have already
//! seen. Inventing a prefixed dialect would buy nothing and cost recognition.
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
use cua_core::{
    Cua, MouseOptions, PointerLocation, Presence, ScrollAmount, ScrollDir, StateOptions, Target,
};
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
    /// One opaque handle from `get_app_state`, e.g. `"7-12-AXButton"`. It
    /// carries the snapshot, the index and the role together, so pinning an
    /// action to the exact element it was chosen from costs nothing and cannot
    /// be forgotten. Preferred over `element_index`.
    #[serde(default)]
    element_token: Option<String>,
    /// Index from the most recent `get_app_state` for this app, e.g. `"12"`.
    /// Still accepted; `element_token` is stricter for free.
    #[serde(default)]
    element_index: Option<String>,
    /// Pass the `snapshot_id` from `get_app_state` to make the call fail loudly
    /// if the UI has been re-snapshotted since. Recommended for anything
    /// destructive.
    #[serde(default)]
    snapshot_id: Option<u64>,
    /// Screen x, in points. Only used when `element_index` is absent.
    ///
    /// Resolved to whichever element of the latest `get_app_state` snapshot
    /// covers the point, so a snapshot has to exist and the point has to be
    /// inside it. A point that covers nothing is an error, never a guess.
    #[serde(default)]
    x: Option<f32>,
    /// Screen y, in points.
    #[serde(default)]
    y: Option<f32>,
    /// Re-read the window after the action and report what changed, as a diff
    /// against the tree from before it. **On by default.**
    ///
    /// This is strictly cheaper than the `get_app_state` that would otherwise
    /// follow: same single tree walk, but one round trip instead of two, and a
    /// few lines of diff instead of the whole outline. It also replaces
    /// `ui_changed`, which is only a heuristic — it compares the focused element
    /// and the window title, and reports `no` for real changes it cannot see, a
    /// menu opening in its own window being the measured case.
    ///
    /// Set it to `false` for a run of actions whose intermediate states nobody
    /// will look at — filling three fields before submitting, say. That is the
    /// one case where the re-read is pure cost, and on an app with a slow
    /// accessibility tree the walk is seconds, not milliseconds.
    ///
    /// Read the diff as a *textual* delta, not as proof. Lines are compared
    /// without their index or indentation, so two elements with identical text
    /// are interchangeable: if a selection moves between two rows that read the
    /// same, the diff is empty even though something changed. It is reliable for
    /// structure arriving or leaving — a menu opening, a dialog replacing a pane
    /// — and an empty diff means "no line-level difference", not "nothing
    /// happened". To know one specific element's state, read that element.
    #[serde(default)]
    return_state: Option<bool>,
}

/// Turn an `element_token` into a pinned [`Target`].
///
/// Free-standing rather than a method so the tools that address two elements at
/// once — `drag` — parse each end exactly the way every other tool parses its
/// single one.
fn parse_element_token(raw: &str) -> Result<Target, String> {
    let raw = raw.trim();
    let mut parts = raw.splitn(3, '-');
    let (Some(snap), Some(idx), Some(role)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!(
            "element_token should look like `7-12-AXButton` (snapshot-index-role), got {raw:?}"
        ));
    };
    let snapshot_id: u64 = snap
        .parse()
        .map_err(|_| format!("element_token has a non-numeric snapshot id: {raw:?}"))?;
    let index: usize = idx
        .parse()
        .map_err(|_| format!("element_token has a non-numeric index: {raw:?}"))?;
    Ok(Target::Index {
        index,
        snapshot_id: Some(snapshot_id),
        expected_role: Some(role.to_string()),
    })
}

impl ActionArgs {
    /// Defaults to `true`: the common loop is act-then-look, and an agent that
    /// forgets to look draws conclusions from `ui_changed` alone — which is
    /// exactly the failure this was built to remove. Paying for a re-read nobody
    /// reads is the cheaper mistake, and it is the one a caller can turn off
    /// knowingly.
    fn return_state(&self) -> bool {
        self.return_state.unwrap_or(true)
    }

    fn target(&self) -> Result<Target, String> {
        // Token first: it is the form that pins everything, so a caller that
        // supplies both should get the stricter reading.
        if let Some(raw) = &self.element_token {
            return parse_element_token(raw);
        }
        if let Some(raw) = &self.element_index {
            let index: usize = raw
                .trim()
                .parse()
                .map_err(|_| format!("element_index must be a whole number, got {raw:?}"))?;
            return Ok(Target::Index {
                index,
                snapshot_id: self.snapshot_id,
                expected_role: None,
            });
        }
        match (self.x, self.y) {
            (Some(x), Some(y)) => Ok(Target::Point { x, y }),
            _ => Err("pass element_token (preferred), element_index, or both x and y".to_string()),
        }
    }
}

/// The button and the held modifier keys, shared by every tool that presses
/// something.
///
/// Flattened rather than repeated so the two fields cannot drift apart between
/// `click`, `click_in_window` and `drag` — and so a caller who learns the
/// `modifiers` spelling once has learned it everywhere.
#[derive(Debug, Deserialize, JsonSchema)]
struct MouseArgs {
    /// Which mouse button: `left` (default), `right` or `middle`. `right`
    /// sends a real rightMouseDown/rightMouseUp pair rather than the AXShowMenu
    /// accessibility action, which is the only thing that opens a context menu
    /// on a custom-drawn control that advertises no actions at all. Use
    /// perform_secondary_action with AXShowMenu instead when the element does
    /// advertise it — that path needs no coordinate.
    #[serde(default)]
    button: Option<String>,
    /// Modifier keys to hold for the duration of the press, as a `+`-separated
    /// list: `cmd`, `shift`, `alt` (or `option`), `ctrl`, `fn`, and any
    /// combination such as `cmd+shift`. Same spelling as press_key. This is how
    /// you get ⌘-click to open a link in a new tab, ⇧-click to extend a
    /// selection to a row, or ⌥-click to reveal an alternate action. Omit it
    /// for a plain click.
    #[serde(default)]
    modifiers: Option<String>,
}

impl MouseArgs {
    fn options(&self) -> Result<MouseOptions, String> {
        MouseOptions::parse(
            self.button.as_deref().unwrap_or(""),
            self.modifiers.as_deref().unwrap_or(""),
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClickArgs {
    #[serde(flatten)]
    target: ActionArgs,
    #[serde(flatten)]
    mouse: MouseArgs,
    /// Click count. `2` for a double-click, for targets that open on
    /// double-click and merely select on single-click — chat and file lists,
    /// typically. Anything above 1 skips the accessibility path entirely
    /// (`AXPress` has no notion of click count) and therefore uses pid-routed
    /// SkyLight delivery.
    #[serde(default)]
    count: Option<u8>,
}

/// One end of a `drag`, or the destination of a `hover`.
///
/// Two ways to say where, and only two: an element handle from `get_app_state`,
/// or a window-local pixel. There is deliberately no screen coordinate — a
/// gesture is pinned to one window, and window-local coordinates are re-anchored
/// to that window's live position just before the events go out, so the user
/// moving the window between the read and the drag is harmless.
#[derive(Debug, Deserialize, JsonSchema)]
struct EndpointArgs {
    /// An `element_token` from get_app_state, e.g. `"7-12-AXButton"`.
    /// Preferred: it pins the snapshot, the index and the role together.
    #[serde(default)]
    element_token: Option<String>,
    /// An element index from the app's most recent get_app_state, e.g. `"12"`.
    /// Accepted; `element_token` is stricter for free.
    #[serde(default)]
    element_index: Option<String>,
    /// Horizontal offset in POINTS from the window's top-left corner, for an
    /// end that is empty canvas rather than an element — not screen
    /// coordinates, and not screenshot pixels (divide a screenshot pixel by the
    /// `px per point` scale get_app_state reports). Both this and the vertical
    /// offset are required together, and only when no element handle is given.
    #[serde(default)]
    window_x: Option<f64>,
    /// Vertical offset in POINTS from the window's top-left corner, measured
    /// downward.
    #[serde(default)]
    window_y: Option<f64>,
}

impl EndpointArgs {
    /// `prefix` is what this endpoint's fields are actually called on the tool
    /// — `"from_"` and `"to_"` on `drag`, empty on `hover` — so an error names
    /// arguments the caller can find in the schema rather than a canonical
    /// spelling that does not appear there.
    fn location(&self, snapshot_id: Option<u64>, prefix: &str) -> Result<PointerLocation, String> {
        if let Some(raw) = &self.element_token {
            return parse_element_token(raw).map(PointerLocation::Element);
        }
        if let Some(raw) = &self.element_index {
            let index: usize = raw.trim().parse().map_err(|_| {
                format!("{prefix}element_index must be a whole number, got {raw:?}")
            })?;
            return Ok(PointerLocation::Element(Target::Index {
                index,
                snapshot_id,
                expected_role: None,
            }));
        }
        match (self.window_x, self.window_y) {
            (Some(x), Some(y)) => Ok(PointerLocation::WindowPoint { x, y }),
            _ => Err(format!(
                "pass {prefix}element_token (preferred), {prefix}element_index, or both {prefix}window_x and {prefix}window_y"
            )),
        }
    }
}

/// Both ends of a `drag`, spelled out rather than nested.
///
/// serde has no field-prefix support, so the two endpoints cannot be one
/// flattened struct used twice; and a nested object would be worse anyway,
/// because MCP clients render a flat argument list far more legibly than a
/// schema with two sub-objects in it.
#[derive(Debug, Deserialize, JsonSchema)]
struct DragArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    /// Where the button goes down: an `element_token` from get_app_state.
    #[serde(default)]
    from_element_token: Option<String>,
    /// Where the button goes down, as an element index from the app's most
    /// recent get_app_state.
    #[serde(default)]
    from_element_index: Option<String>,
    /// Where the button goes down, as a horizontal offset in POINTS from the
    /// window's top-left corner. Use this end-form for empty canvas, where
    /// there is no element to name. Needs `from_window_y` too.
    #[serde(default)]
    from_window_x: Option<f64>,
    /// Vertical offset in POINTS from the window's top-left corner, measured
    /// downward.
    #[serde(default)]
    from_window_y: Option<f64>,
    /// Where the button comes back up: an `element_token`. May be a different
    /// element of the same app — a row and the folder it is dropped into, say.
    #[serde(default)]
    to_element_token: Option<String>,
    /// Where the button comes back up, as an element index.
    #[serde(default)]
    to_element_index: Option<String>,
    /// Where the button comes back up, as a horizontal offset in POINTS from
    /// the window's top-left corner. Needs `to_window_y` too.
    #[serde(default)]
    to_window_x: Option<f64>,
    /// Vertical offset in POINTS from the window's top-left corner, measured
    /// downward.
    #[serde(default)]
    to_window_y: Option<f64>,
    #[serde(flatten)]
    mouse: MouseArgs,
    /// Pass the `snapshot_id` from get_app_state to make the call fail loudly
    /// if the UI has been re-snapshotted since. Applies to both ends.
    #[serde(default)]
    snapshot_id: Option<u64>,
    /// Re-read the window afterwards and report what changed. On by default.
    #[serde(default)]
    return_state: Option<bool>,
}

impl DragArgs {
    fn drag_origin(&self) -> Result<PointerLocation, String> {
        EndpointArgs {
            element_token: self.from_element_token.clone(),
            element_index: self.from_element_index.clone(),
            window_x: self.from_window_x,
            window_y: self.from_window_y,
        }
        .location(self.snapshot_id, "from_")
    }

    fn drag_destination(&self) -> Result<PointerLocation, String> {
        EndpointArgs {
            element_token: self.to_element_token.clone(),
            element_index: self.to_element_index.clone(),
            window_x: self.to_window_x,
            window_y: self.to_window_y,
        }
        .location(self.snapshot_id, "to_")
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HoverArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    #[serde(flatten)]
    at: EndpointArgs,
    /// Modifier keys to appear to be holding while the pointer arrives, e.g.
    /// `alt` for an app that reveals a different tooltip under ⌥. Usually
    /// omitted.
    #[serde(default)]
    modifiers: Option<String>,
    /// Pass the `snapshot_id` from get_app_state to make the call fail loudly
    /// if the UI has been re-snapshotted since.
    #[serde(default)]
    snapshot_id: Option<u64>,
    /// Re-read the window afterwards and report what changed. Leave this on:
    /// the diff is how the hover-revealed UI becomes visible to you.
    #[serde(default)]
    return_state: Option<bool>,
}

/// Arguments for the elementless click.
///
/// Deliberately does not reuse `ActionArgs`: every field it carries is about
/// naming an element, and offering `element_index` here would blur the one
/// distinction this tool exists to make. There is no target to pin, only a
/// window and a pixel.
#[derive(Debug, Deserialize, JsonSchema)]
struct ClickInWindowArgs {
    /// App name, bundle id or pid, as for `get_app_state`.
    app: String,
    /// The `window_id` reported by the most recent `get_app_state` of this app.
    /// Any other id is refused: without an element, this is the only thing the
    /// click is anchored to.
    window_id: u32,
    /// Horizontal offset in POINTS from the window's top-left corner — not
    /// screen coordinates, and not screenshot pixels. Divide a screenshot pixel
    /// by the `px per point` scale that `get_app_state` reports.
    x: f64,
    /// Vertical offset in POINTS from the window's top-left corner, measured
    /// downward. Same scale conversion as `x`.
    y: f64,
    /// Click count. 2 for a double-click.
    #[serde(default)]
    count: Option<u8>,
    #[serde(flatten)]
    mouse: MouseArgs,
    /// Re-read the window afterwards and attach what changed. On a canvas the
    /// delta is usually empty, which is not evidence either way.
    #[serde(default)]
    return_state: Option<bool>,
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
    /// Number of pages. Defaults to 1. A page is whatever the element's own
    /// accessibility scroller calls a page; on the wheel tier below it is 90%
    /// of the element's height, which keeps a line of overlap.
    #[serde(default)]
    pages: Option<u32>,
    /// Scroll by exactly this many POINTS of content instead of by pages. This
    /// forces the wheel-event tier even on an element that does advertise an
    /// accessibility scroll action, because accessibility cannot express a
    /// distance — it only has whole pages. Use it when a page is too coarse:
    /// nudging a long list a few rows, or scrolling a canvas that has no notion
    /// of a page at all.
    #[serde(default)]
    pixels: Option<u32>,
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
    /// Background-safe semantic key: `return`, `escape`, `up`, or `down`.
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
                        "\nGrant Screen Recording in System Settings > Privacy & Security > Screen Recording, same host process. The accessibility tree and AX actions still work without it; screenshots and the custom-control pid-routed click fallback are unavailable because their target window cannot be revalidated."
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
            "{} (pid {})  snapshot_id={}\nelement_token: join this snapshot_id, the [N] in a tree line, and that line's role with dashes — e.g. `{}-12-AXButton` — and pass it as element_token to pin the action to exactly this element\nwindow: {}\nelements: {} total, {} actionable\n",
            state.app.name,
            state.app.pid,
            state.snapshot_id,
            state.snapshot_id,
            match state.window_id {
                // Printed rather than kept internal because it is the only
                // handle `click_in_window` accepts, and that tool refuses an id
                // this app's latest read did not produce.
                Some(id) => format!(
                    "{}  (window_id={id})",
                    state.window_title.as_deref().unwrap_or("(untitled)")
                ),
                None => format!(
                    "{}  (no verified window id)",
                    state.window_title.as_deref().unwrap_or("(untitled)")
                ),
            },
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
        description = "Activate an element without moving the system cursor. Uses AXPress, AXPick, or AXConfirm when the element supports one; otherwise builds real AppKit mouse events (NSEvent, carrying a fresh event number, the true click count and the target's window number) and routes them to the target process through SkyLight, reporting `delivery: pid`. A synthesized ApplicationActivated notice wraps the click so custom-drawn views that only act when their app is active still accept it — the real frontmost app, keyboard focus and Space never change. Frame-bearing controls can receive an `element_index` even when they advertise no AX actions. Prefer `element_index` from the latest get_app_state; x/y is resolved to an element first. Pass count=2 for a target that only opens on a double-click; accessibility has no click count, so double-clicks use pid delivery. Pass button=right for a real rightMouseDown/rightMouseUp pair (a context menu on a control that advertises no AXShowMenu), button=middle for a middle click, and modifiers=\"cmd\" / \"shift\" / \"alt\" / \"ctrl\" (or a combination like \"cmd+shift\") to hold keys down for the press — that is how you open a link in a new tab, extend a selection to a row, or reach an app's alternate ⌥ action. There is no real-pointer fallback."
    )]
    async fn click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = match a.target.target() {
            Ok(t) => t,
            Err(e) => return Ok(fail(e)),
        };
        let mouse = match a.mouse.options() {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.target.app.clone();
        let want_state = a.target.return_state();
        let mouse = mouse.with_count(a.count.unwrap_or(1).clamp(1, 3));
        match self
            .native(move |c| c.click(&app, target, mouse, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Press the mouse down at one point, move through interpolated intermediate points, and release at another — a real drag, delivered to the target process by pid with the cursor, keyboard focus, frontmost app and Space all untouched. Use it to reorder a list row, drag a file onto a folder, draw a selection rectangle on a canvas, move a slider knob, or resize a split. Each end is named independently and the two may be different elements of the same app: pass from_element_token / to_element_token (preferred, from get_app_state), or from_element_index / to_element_index, or from_window_x+from_window_y / to_window_x+to_window_y for an end that is bare canvas with no element behind it. Window coordinates are POINTS from the window's top-left corner (screenshot pixel / the `px per point` scale get_app_state reports), re-anchored to the window's live position just before the events go out. Both ends must be inside the SAME window that the app's most recent get_app_state read — a gesture cannot cross a window boundary and cua-rs refuses rather than guessing. The intermediate moves are interpolated rather than jumped, because a down at A followed by an up at B is not a drag to anything that implements one. `button` and `modifiers` work as they do on click. A success means the events were delivered in order and the mouse-up was sent even if a move failed partway; whether the target implemented a drag at all is what the returned state diff is for."
    )]
    async fn drag(&self, Parameters(a): Parameters<DragArgs>) -> Result<CallToolResult, McpError> {
        let from = match a.drag_origin() {
            Ok(l) => l,
            Err(e) => return Ok(fail(e)),
        };
        let to = match a.drag_destination() {
            Ok(l) => l,
            Err(e) => return Ok(fail(e)),
        };
        let mouse = match a.mouse.options() {
            Ok(m) => m,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.app.clone();
        let want_state = a.return_state.unwrap_or(true);
        match self
            .native(move |c| c.drag(&app, from, to, mouse, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Move the pointer over a point WITHOUT clicking, so hover-only UI appears: a tooltip, a submenu, a row's delete button that is only drawn under the cursor, a chart's value readout. Follow it with get_app_state (or leave return_state on, which is the default) to read whatever appeared. Address it like a drag end: element_token (preferred), element_index, or window_x + window_y in POINTS from the window's top-left corner for a spot with no element. IMPORTANT — your real cursor does not move. This sends a synthesized mouseMoved event to the target process, which is what makes it safe to use while the user is working, and also its one limitation: an app that asks where the pointer IS (NSEvent.mouseLocation, a poll of the cursor position) rather than reading the event it was handed will not react, and no setting changes that. Apps that use NSTrackingArea, and anything web-based, do react. The hover lasts until the app decides otherwise; there is no matching un-hover, and a later click or hover elsewhere replaces it."
    )]
    async fn hover(
        &self,
        Parameters(a): Parameters<HoverArgs>,
    ) -> Result<CallToolResult, McpError> {
        let at = match a.at.location(a.snapshot_id, "") {
            Ok(l) => l,
            Err(e) => return Ok(fail(e)),
        };
        let modifiers = match MouseOptions::parse("", a.modifiers.as_deref().unwrap_or("")) {
            Ok(m) => m.modifiers,
            Err(e) => return Ok(fail(e)),
        };
        let app = a.app.clone();
        let want_state = a.return_state.unwrap_or(true);
        match self
            .native(move |c| c.hover(&app, at, modifiers, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "LAST RESORT: click a bare point inside a window, with no element behind it. Use this ONLY for custom-drawn surfaces that genuinely publish no children — maps, charts, canvases, game views — after `click` and `find` have shown there is no element to address. It is not a retry for a `click` that failed and it is never chosen automatically, because a point that covers nothing is indistinguishable from a typo. x and y are POINTS from the window's top-left corner (screenshot pixel / the `px per point` scale get_app_state reports), NOT screen coordinates: they are re-anchored to the window's live position just before the event is sent, so moving the window between the read and the click is harmless. Delivery is the same pid-routed SkyLight path as `click` — the cursor, keyboard focus, frontmost app and Space are untouched — but the result is labelled `pid (no element)` because NOTHING WAS VERIFIED. There is no element to inspect afterwards, so a success means only that the events were delivered to that pixel of that window; whether anything was there, and whether it was the right thing, is entirely the caller's aim. Requires that the most recent get_app_state of this app read this same window_id."
    )]
    async fn click_in_window(
        &self,
        Parameters(a): Parameters<ClickInWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let (wid, x, y) = (a.window_id, a.x, a.y);
        let want_state = a.return_state.unwrap_or(true);
        let mouse = match a.mouse.options() {
            Ok(m) => m.with_count(a.count.unwrap_or(1).clamp(1, 3)),
            Err(e) => return Ok(fail(e)),
        };
        match self
            .native(move |c| c.click_in_window(&app, wid, x, y, mouse, want_state))
            .await
        {
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
        let want_state = a.target.return_state();
        let value = a.value.clone();
        match self
            .native(move |c| c.set_value(&app, target, &value, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Scroll a scrollable element. Two tiers, chosen automatically and named in the result. If the element advertises an accessibility scroll action, whole pages go through it (`delivery: ax`) — that is the better answer where it exists, because the app decides what a page of its own content is and no coordinate is involved. If it advertises none — an Electron list, a canvas, a web area inside a native shell, which is the common case and used to be a dead end — a real scrollWheel event is delivered at the element's point, routed to the target process by pid (`delivery: pid`), with the cursor, keyboard focus and frontmost app untouched. Pass `pixels` to scroll by an exact number of POINTS instead of by pages; that always uses the wheel tier, because accessibility cannot express a distance at all. Target the scroll area, list or table itself, not the element you want to reveal."
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
        let want_state = a.target.return_state();
        // `pixels` wins when both are given: it is the more specific request,
        // and it is the only one of the two that can express a distance at all.
        let amount = match a.pixels {
            Some(px) => ScrollAmount::Points(px.clamp(1, 20_000)),
            None => ScrollAmount::Pages(a.pages.unwrap_or(1).clamp(1, 50)),
        };
        match self
            .native(move |c| c.scroll(&app, target, dir, amount, want_state))
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
        let want_state = a.target.return_state();
        let text = a.text.clone();
        match self
            .native(move |c| c.type_text(&app, target, &text, want_state))
            .await
        {
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
        let want_state = a.target.return_state();
        let (text, prefix, suffix) = (a.text.clone(), a.prefix.clone(), a.suffix.clone());
        match self
            .native(move |c| c.select_text(&app, target, &text, prefix, suffix, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Press any key or chord on an element: `return`, `escape`, `tab`, a letter or digit, `f5`, and arbitrary combinations such as `cmd+shift+p` or `ctrl+alt+delete`. `+` or `-` separates, case does not matter, and the modifier names are the same ones the click tools take (`cmd`, `shift`, `alt`/`option`, `ctrl`, `fn`). The keys are real key events routed to the target process by pid and reported as `delivery: pid (keyboard)` — the shared keyboard tap is never used, so the user's own typing keeps going where it was going. They land wherever that process's own first responder currently is; cua-rs best-effort-focuses the element you name first (AXFocused, where the app makes it settable) but cannot guarantee it, so re-read the element afterwards when it matters which field received the keys."
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
        let want_state = a.target.return_state();
        let key = a.key.clone();
        match self
            .native(move |c| c.press_key(&app, target, &key, want_state))
            .await
        {
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
        let want_state = a.target.return_state();
        let action = a.action.clone();
        match self
            .native(move |c| c.perform_action(&app, target, &action, want_state))
            .await
        {
            Ok(r) => Ok(ok(render_action(&r))),
            Err(e) => Ok(fail(e.to_string())),
        }
    }

    #[tool(
        description = "Search the current snapshot for elements whose label, value or role contains a string, case-insensitively. Much cheaper than re-reading a whole tree when you already know what you are looking for. Actionable matches are listed first, and label matches rank above value and role matches. Searches the snapshot you already have so the returned indices stay valid; walks afresh when there is none, or when an action has run since the last read."
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
/// `ui_changed` is reported honestly, including when the answer is "no" or
/// "unknown". `no` does not mean the action failed — plenty of controls change
/// nothing observable — and `unknown` means the app published nothing to
/// compare, which is not the same claim at all. Collapsing either into a
/// confident `false` is the failure mode that makes UI automation
/// untrustworthy.
fn render_action(r: &cua_core::ActionResult) -> String {
    let mut s = format!(
        "{} on {}\ndelivery: {}{}\nui_changed: {}",
        r.verb,
        r.target,
        r.delivery.as_str(),
        match r.delivery {
            cua_core::Delivery::Pid => {
                "  (input synthesized and routed to the target process via the private SkyLight SPI: cursor, keyboard focus and frontmost app untouched)"
            }
            cua_core::Delivery::PidNoElement => {
                "  (same pid-routed SkyLight delivery, but aimed at a pixel rather than an element: NOTHING was verified to be there, and there is no element to re-read. This confirms delivery only)"
            }
            cua_core::Delivery::Ax => {
                "  (accessibility action: cursor, focus and frontmost app untouched)"
            }
            cua_core::Delivery::PidKey => {
                "  (real key events routed to the target process via the private SkyLight SPI, not through the shared keyboard tap: they land wherever that process's own first responder currently is, which cua-rs best-effort-focuses first but cannot guarantee)"
            }
        },
        r.ui_changed.as_str(),
    );
    // The advice attached to `ui_changed` all ends in "go and look" — which is
    // wrong to print when the section below *is* the result of having looked.
    // Two answers to the same question, one hedged and one measured, read as a
    // contradiction and invite a redundant `get_app_state`.
    let looked = r.state.as_ref().is_some_and(|s| s.diff.is_some());
    if !looked {
        s.push_str(match r.ui_changed {
            cua_core::Observed::Changed => "",
            cua_core::Observed::Unchanged => {
                "  (no observable change; call get_app_state to check, or the control may be a no-op)"
            }
            cua_core::Observed::Unknown => {
                "  (nothing to compare: this app published no window state before or after, so this is NOT evidence the action failed. Verify with get_app_state)"
            }
        });
    } else if r.ui_changed != cua_core::Observed::Changed {
        s.push_str("  (heuristic only — the tree diff below is the real answer)");
    }
    if let Some(state) = &r.state {
        s.push_str(&render_post_action_state(state));
    }
    s
}

/// Append what the window looked like after the action.
///
/// A diff of zero lines is reported explicitly rather than omitted. "The tree is
/// identical" and "nobody looked" are different findings, and only the first one
/// is evidence that a control did nothing — leaving the section out would let a
/// caller read a silence as either.
fn render_post_action_state(state: &cua_core::PostActionState) -> String {
    // Enough to see what happened, small enough that turning this on is never
    // the reason a response blows the context window. A change this large is
    // better handled by re-reading the tree deliberately.
    const MAX_LINES: usize = 40;

    let Some(snapshot_id) = state.snapshot_id else {
        // No snapshot means the re-read failed rather than finding nothing. Say
        // so, because the action itself did happen.
        return format!(
            "\n\nstate after: unavailable\n  {}",
            state
                .note
                .as_deref()
                .unwrap_or("the window could not be re-read")
        );
    };

    let mut s = format!(
        "\n\nstate after (snapshot_id={}, {} elements):",
        snapshot_id, state.node_count
    );

    let Some(diff) = &state.diff else {
        s.push_str("\n  ");
        s.push_str(state.note.as_deref().unwrap_or("no diff available"));
        return s;
    };

    if diff.is_empty() {
        s.push_str("\n  no change in the accessibility tree");
        return s;
    }

    let mut push = |label: &str, lines: &[String], mark: char| {
        if lines.is_empty() {
            return;
        }
        s.push_str(&format!("\n  {label} ({}):", lines.len()));
        for line in lines.iter().take(MAX_LINES) {
            s.push_str(&format!("\n  {mark} {}", line.trim_end()));
        }
        if lines.len() > MAX_LINES {
            s.push_str(&format!(
                "\n  … {} more (call get_app_state for the full tree)",
                lines.len() - MAX_LINES
            ));
        }
    };
    push("appeared", &diff.added, '+');
    push("vanished", &diff.removed, '-');
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
             [N] indices that click / set_value / scroll take as element_index. Actions use \
             element-addressed accessibility verbs when available; custom controls can use a \
             window-pinned event routed directly to the target process (delivery: pid). Beyond \
             click there is drag (press, interpolated moves, release, either end an element or \
             a bare pixel), hover (a synthesized mouseMoved that reveals hover-only UI), and \
             scroll (the accessibility page verb where the element has one, a real wheel event \
             where it does not). Neither \
             path moves the cursor, changes keyboard focus, raises the app, or switches Space — \
             you can drive a background window while the user keeps working in another one. \
             Indices are only valid until \
             the next get_app_state for that app; pass snapshot_id to make staleness an error \
             instead of a mis-click."
                .to_string(),
        );
        info
    }
}
