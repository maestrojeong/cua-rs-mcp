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
    Cua, MouseOptions, Op, PointerLocation, Presence, ScrollAmount, ScrollDir, StateOptions, Target,
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
    /// `snapshot_id` applies here too, and is worth passing: a stale index can
    /// be caught by the role it used to have, while a stale coordinate looks
    /// exactly like a fresh one.
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
            (Some(x), Some(y)) => Ok(Target::Point {
                x,
                y,
                snapshot_id: self.snapshot_id,
            }),
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

/// Note on `confirm_destructive`, which three of the acting tools carry.
///
/// It is a parameter and not a prompt: an MCP server has no channel to a human,
/// so the only place the decision can be made visible is the transcript the
/// human can scroll back through. The refusal names the field, so clearing it
/// costs one round trip and no guessing.
#[derive(Debug, Deserialize, JsonSchema)]
struct ClickArgs {
    #[serde(flatten)]
    target: ActionArgs,
    #[serde(flatten)]
    mouse: MouseArgs,
    /// Set to true to proceed when the target's label reads as destructive
    /// (Delete, Remove, Erase, Reset, Move to Trash, Don't Save, 삭제, 제거,
    /// 초기화, 나가기, …), or when a terse button answers a sheet or dialog
    /// that reads that way ("OK" under "Delete 4 items?", 확인 under
    /// "4개 항목을 삭제할까요?"). Without it such a click is REFUSED and the
    /// error names this field and quotes the question. The classifier
    /// deliberately over-reports — confirming a false positive is the right
    /// answer and costs one round trip. Cancel, No and 취소 are never refused
    /// on the dialog's wording, so backing out never needs this.
    #[serde(default)]
    confirm_destructive: bool,
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
    /// if the UI has been re-snapshotted since. Applies to both ends,
    /// element-addressed and coordinate-addressed alike — and it is the only
    /// guard a raw `window_x`/`window_y` end has, because a pixel picked off an
    /// old screenshot still exists on the new one, just covering something
    /// else.
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
    /// if the UI has been re-snapshotted since. The only guard a raw
    /// `window_x`/`window_y` destination has.
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
    /// The `snapshot_id` this coordinate was read off. Strongly recommended
    /// here of all places: the window id proves you are aiming at a window you
    /// have seen, but not at the state you saw it in, and a window id survives
    /// any number of re-reads. Supply it and a point chosen from an older
    /// screenshot is refused instead of landing on whatever now occupies that
    /// pixel.
    #[serde(default)]
    snapshot_id: Option<u64>,
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
    /// How to deliver it. `"ax"` (default) writes the string through the
    /// accessibility API in one atomic, element-addressed call — no focus
    /// needed, cannot land anywhere else, and the right choice for ordinary
    /// text fields. `"keystrokes"` synthesizes real per-character key events
    /// and routes them to the target process: use it ONLY when you know the
    /// target ignores AXValue (terminals, canvas editors), because the events
    /// go to whatever that process's own first responder is rather than to the
    /// element you named. The result reports `focus:` so you can tell whether
    /// the addressed element was the one holding focus.
    #[serde(default)]
    mechanism: Option<String>,
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
struct MenuBarArgs {
    /// App name, bundle id or pid.
    app: String,
    /// `>`-separated menu titles, e.g. `Edit > Transformations > Make Upper
    /// Case`. Omit for the top level.
    #[serde(default)]
    path: Option<String>,
    /// Press the row `path` names, instead of listing what is there. Reading
    /// first is the intended flow: enablement, the checkmark and the exact
    /// title are all only knowable from a read.
    #[serde(default)]
    press: bool,
    /// Set to true to proceed when the row's title reads as destructive.
    #[serde(default)]
    confirm_destructive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListNotificationsArgs {
    /// Also read the Notification Center history panel. cua-rs opens it by
    /// pressing the menu-bar clock, expands collapsed per-app stacks so every
    /// notification gets its own row, reads, and closes the panel again if it
    /// opened it. Off by default, because it presses UI; without it this is a
    /// pure read of whatever banners are on screen right now (a banner lasts
    /// about 5 s).
    #[serde(default)]
    include_history: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NotificationIdArgs {
    /// The notification's `id` from list_notifications — Notification Center's
    /// own UUID for it, e.g. `805A1087-89B8-482B-B822-30F2726D4663`.
    id: String,
    /// Look in the history panel too when the notification is not on screen as
    /// a banner. On by default, since a banner expires into history in ~5 s.
    #[serde(default = "yes")]
    include_history: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DismissAllArgs {
    /// Include the history panel, not just live banners. On by default.
    #[serde(default = "yes")]
    include_history: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NotificationActionArgs {
    /// The notification's `id` from list_notifications.
    id: String,
    /// The action's label exactly as list_notifications reports it under
    /// `actions`, e.g. `Show Details` / `세부사항 보기`. Labels are localized.
    action: String,
    /// Look in the history panel too. On by default.
    #[serde(default = "yes")]
    include_history: bool,
    /// Set to true to proceed when the action's label reads as destructive
    /// (`Clear All`, `Delete`, `모두 지우기`…).
    #[serde(default)]
    confirm_destructive: bool,
}

/// Validate a notification id before it reaches the worker.
///
/// Notification Center's ids are UUIDs. Requiring that shape catches the two
/// mistakes worth catching early — passing a title, or passing a list position
/// — with a message that says what an id looks like, instead of a not-found
/// that reads as if the notification had gone away.
fn notification_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("`id` is empty; pass the id list_notifications reported".into());
    }
    let shaped = id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !shaped {
        return Err(format!(
            "`{id}` is not a notification id. Ids are Notification Center's UUIDs, like \
             805A1087-89B8-482B-B822-30F2726D4663 — take one from list_notifications rather than \
             a title or a position"
        ));
    }
    Ok(id.to_string())
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressKeyArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// Background-safe semantic key: `return`, `escape`, `up`, or `down`.
    key: String,
    /// Set to true to proceed when the key or its target reads as destructive.
    /// `cmd+delete` always counts; a bare `delete` counts everywhere except a
    /// text field, where it is one character.
    #[serde(default)]
    confirm_destructive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SecondaryActionArgs {
    #[serde(flatten)]
    target: ActionArgs,
    /// AX action name, e.g. `AXShowMenu`, `AXRaise`, `AXIncrement`.
    action: String,
    /// Set to true to proceed when the target's label reads as destructive.
    #[serde(default)]
    confirm_destructive: bool,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct ClipboardGetArgs {
    /// Return at most this many characters of text; the total is always
    /// reported. Defaults to 20000, capped at 200000.
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClipboardSetArgs {
    /// Plain text to put on the clipboard. Replaces everything on it.
    text: String,
}
/// Which window a window tool acts on. Shared so every window tool names a
/// window the same way.
#[derive(Debug, Deserialize, JsonSchema)]
struct WindowTargetArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
    /// The `window_id` list_windows reports. Preferred: an id names exactly one
    /// window, where a title can be shared. Needs Screen Recording; without it
    /// list_windows reports no ids and window_title is the way to name one.
    #[serde(default)]
    window_id: Option<u32>,
    /// The window's exact title, as list_windows shows it. Refused when two
    /// windows share it. Ignored when window_id is given. Omit both to act on
    /// the app's focused window (or its main window when none is focused).
    #[serde(default)]
    window_title: Option<String>,
}

impl WindowTargetArgs {
    fn selector(&self) -> cua_core::WindowSelector {
        cua_core::WindowSelector {
            window_id: self.window_id,
            title: self.window_title.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListWindowsArgs {
    /// App name, bundle identifier, or bundle path.
    app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MoveWindowArgs {
    #[serde(flatten)]
    window: WindowTargetArgs,
    /// New left edge, in global screen POINTS (the space list_displays reports;
    /// the main display's top-left corner is 0,0, and a display to its left has
    /// negative x).
    x: f64,
    /// New top edge, in global screen POINTS, measured downward.
    y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ResizeWindowArgs {
    #[serde(flatten)]
    window: WindowTargetArgs,
    /// New width in POINTS. The top-left corner stays where it is.
    width: f64,
    /// New height in POINTS.
    height: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LaunchAppArgs {
    /// Bundle identifier (`com.apple.TextEdit`, exact), app name looked up in
    /// the standard Applications folders (`TextEdit`), or a path to the .app.
    app: String,
    /// Bring the launched app to the front. OFF by default, and leave it off
    /// unless the user asked to see the app: activation takes keyboard focus
    /// away from whatever the human is typing into, and may switch Spaces. The
    /// window tools and every action work on a background app.
    #[serde(default)]
    activate: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CloseAppArgs {
    /// App name, bundle identifier, or bundle path of a running app.
    app: String,
    /// Must be true. Quitting can discard unsaved work, so close_app refuses
    /// every call without it — there is no label to judge here, the quit
    /// itself is the destructive part.
    #[serde(default)]
    confirm_destructive: bool,
}

// ── prepared calls ───────────────────────────────────────────────────────────
//
// Each tool's whole behaviour lives in one `prep_*` function: parse and
// validate the arguments, build the matching `cua_core::Op`, and say how its
// outcome is rendered. The `#[tool]` method only dispatches it, and `batch`
// dispatches the very same value — so a step in a batch and a solo call are one
// code path, not two that have to be kept in step.

/// A tool call ready to send to the worker, or the refusal its arguments
/// earned before anything native was attempted.
type Prepared = Result<Op<CallToolResult>, CallToolResult>;

fn rendered_action(op: Op<cua_core::ActionResult>) -> Op<CallToolResult> {
    op.map(|r| match r {
        Ok(r) => ok(render_action(&r)),
        Err(e) => fail(e.to_string()),
    })
}

fn prep_check_permissions() -> Op<CallToolResult> {
    Op::permissions().map(|r| match r {
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
            ok(lines.join("\n"))
        }
        Err(e) => fail(e.to_string()),
    })
}

fn prep_list_apps() -> Op<CallToolResult> {
    Op::list_apps().map(|r| match r {
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
            ok(out)
        }
        Err(e) => fail(e.to_string()),
    })
}

fn prep_get_app_state(a: AppArgs) -> Prepared {
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
                return Err(fail(format!(
                    "scope_element_id must be a whole number, got {raw:?}"
                )))
            }
        }
    }
    Ok(Op::get_app_state(&a.app, opts).map(|r| match r {
        Ok(state) => render_app_state(state),
        Err(e) => fail(e.to_string()),
    }))
}

fn render_app_state(state: cua_core::AppState) -> CallToolResult {
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

    let popups = render_popups(&state.popups);
    let mut blocks = vec![ContentBlock::text(format!(
        "{header}{popups}\n{}",
        state.tree
    ))];
    if let Some(shot) = state.screenshot {
        // The extent is stated, not assumed. `screencapture -l<id>`
        // photographs the window group, so with a menu open the image covers
        // the union of the window and the menu — bigger than the window, and
        // with the scale to match. A caller converting a pixel back to a
        // point has to divide by *this* scale and measure from *this*
        // origin, so both are printed rather than left to be inferred from
        // the window frame.
        let grew = shot.frame.size.width > shot.window_frame.size.width
            || shot.frame.size.height > shot.window_frame.size.height;
        blocks.push(ContentBlock::text(format!(
            "screenshot: {}x{} px, {:.2} px per point, covering {:.0},{:.0} {:.0}x{:.0} points{}",
            shot.width,
            shot.height,
            shot.scale,
            shot.frame.origin.x,
            shot.frame.origin.y,
            shot.frame.size.width,
            shot.frame.size.height,
            if grew {
                "  (LARGER than the window itself: macOS captures a window together with the pop-up UI attached to it, so this one image already contains the menu — there is no separate screenshot to ask for. Subtract the origin above, not the window's, when turning a pixel into a point)"
            } else {
                ""
            }
        )));
        blocks.push(ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(&shot.png),
            "image/png",
        ));
    }
    CallToolResult::success(blocks)
}

fn prep_click(a: ClickArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let mouse = a.mouse.options().map_err(fail)?;
    let want_state = a.target.return_state();
    let mouse = mouse.with_count(a.count.unwrap_or(1).clamp(1, 3));
    Ok(rendered_action(Op::click(
        &a.target.app,
        target,
        mouse,
        want_state,
        a.confirm_destructive,
    )))
}

fn prep_drag(a: DragArgs) -> Prepared {
    let from = a.drag_origin().map_err(fail)?;
    let to = a.drag_destination().map_err(fail)?;
    let mouse = a.mouse.options().map_err(fail)?;
    let want_state = a.return_state.unwrap_or(true);
    Ok(rendered_action(Op::drag(
        &a.app,
        from,
        to,
        mouse,
        a.snapshot_id,
        want_state,
    )))
}

fn prep_hover(a: HoverArgs) -> Prepared {
    let at = a.at.location(a.snapshot_id, "").map_err(fail)?;
    let modifiers = MouseOptions::parse("", a.modifiers.as_deref().unwrap_or(""))
        .map_err(fail)?
        .modifiers;
    let want_state = a.return_state.unwrap_or(true);
    Ok(rendered_action(Op::hover(
        &a.app,
        at,
        modifiers,
        a.snapshot_id,
        want_state,
    )))
}

fn prep_click_in_window(a: ClickInWindowArgs) -> Prepared {
    let at = cua_core::WindowPixel {
        window_id: a.window_id,
        x: a.x,
        y: a.y,
        snapshot_id: a.snapshot_id,
    };
    let want_state = a.return_state.unwrap_or(true);
    let mouse = a
        .mouse
        .options()
        .map_err(fail)?
        .with_count(a.count.unwrap_or(1).clamp(1, 3));
    Ok(rendered_action(Op::click_in_window(
        &a.app, at, mouse, want_state,
    )))
}

fn prep_set_value(a: SetValueArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let want_state = a.target.return_state();
    Ok(rendered_action(Op::set_value(
        &a.target.app,
        target,
        &a.value,
        want_state,
    )))
}

fn prep_scroll(a: ScrollArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let dir = match a.direction.to_lowercase().as_str() {
        "up" => ScrollDir::Up,
        "down" => ScrollDir::Down,
        "left" => ScrollDir::Left,
        "right" => ScrollDir::Right,
        other => {
            return Err(fail(format!(
                "direction must be up/down/left/right, got {other:?}"
            )))
        }
    };
    let want_state = a.target.return_state();
    // `pixels` wins when both are given: it is the more specific request,
    // and it is the only one of the two that can express a distance at all.
    let amount = match a.pixels {
        Some(px) => ScrollAmount::Points(px.clamp(1, 20_000)),
        None => ScrollAmount::Pages(a.pages.unwrap_or(1).clamp(1, 50)),
    };
    Ok(rendered_action(Op::scroll(
        &a.target.app,
        target,
        dir,
        amount,
        want_state,
    )))
}

fn prep_type_text(a: TypeTextArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let mechanism = match a.mechanism.as_deref().map(cua_core::Mechanism::parse) {
        Some(Ok(m)) => m,
        Some(Err(e)) => return Err(fail(e)),
        None => cua_core::Mechanism::default(),
    };
    let want_state = a.target.return_state();
    Ok(rendered_action(Op::type_text(
        &a.target.app,
        target,
        &a.text,
        mechanism,
        want_state,
    )))
}

fn prep_select_text(a: SelectTextArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let want_state = a.target.return_state();
    Ok(rendered_action(Op::select_text(
        &a.target.app,
        target,
        &a.text,
        a.prefix,
        a.suffix,
        want_state,
    )))
}

fn prep_press_key(a: PressKeyArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let want_state = a.target.return_state();
    Ok(rendered_action(Op::press_key(
        &a.target.app,
        target,
        &a.key,
        want_state,
        a.confirm_destructive,
    )))
}

fn prep_menu_bar(a: MenuBarArgs) -> Prepared {
    let path = a.path.unwrap_or_default();
    if a.press {
        return Ok(rendered_action(Op::press_menu_bar(
            &a.app,
            &path,
            false,
            a.confirm_destructive,
        )));
    }
    Ok(Op::menu_bar(&a.app, &path).map(|r| match r {
        Ok(listing) => ok(render_menu(&listing)),
        Err(e) => fail(e.to_string()),
    }))
}

fn prep_perform_secondary_action(a: SecondaryActionArgs) -> Prepared {
    let target = a.target.target().map_err(fail)?;
    let want_state = a.target.return_state();
    Ok(rendered_action(Op::perform_action(
        &a.target.app,
        target,
        &a.action,
        want_state,
        a.confirm_destructive,
    )))
}

fn prep_find(a: FindArgs) -> Prepared {
    let limit = a.limit.unwrap_or(20).clamp(1, 200);
    let text = a.text;
    Ok(Op::find(&a.app, &text, limit).map(move |r| match r {
        Ok(r) => {
            if r.lines.is_empty() {
                return ok(format!(
                    "no element matching {:?} among {} elements (snapshot {})",
                    text, r.searched, r.snapshot_id
                ));
            }
            ok(format!(
                "{} match(es) for {:?}  snapshot_id={}\n\n{}",
                r.total,
                text,
                r.snapshot_id,
                r.lines.join("\n")
            ))
        }
        Err(e) => fail(e.to_string()),
    }))
}

fn prep_wait_for(a: WaitForArgs) -> Prepared {
    let want = if a.gone.unwrap_or(false) {
        Presence::Disappears
    } else {
        Presence::Appears
    };
    // Capped: a tool call that can block for minutes will hit the MCP
    // client's own timeout and strand the poll loop.
    let timeout_ms = a.timeout_ms.unwrap_or(5_000).clamp(250, 60_000);
    let text = a.text;
    Ok(
        Op::wait_for(&a.app, &text, want, timeout_ms).map(move |r| match r {
            Ok(r) => {
                let verb = if want == Presence::Appears {
                    "appeared"
                } else {
                    "disappeared"
                };
                let head = if r.satisfied {
                    format!("{:?} {verb} after {}ms", text, r.elapsed_ms)
                } else {
                    format!(
                        "timed out after {}ms: {:?} never {verb}",
                        r.elapsed_ms, text
                    )
                };
                let body = format!("{head}\npolls: {}\nsnapshot_id: {}", r.polls, r.snapshot_id);
                if r.satisfied {
                    ok(body)
                } else {
                    fail(body)
                }
            }
            Err(e) => fail(e.to_string()),
        }),
    )
}

// ── batch ────────────────────────────────────────────────────────────────────

/// One step of a `batch`: the name of another tool and exactly the arguments
/// that tool takes on its own.
///
/// Every variant wraps the tool's existing argument struct, so a field added to
/// a tool is a field its batch step accepts, with no second definition to
/// forget. `batch` itself is deliberately absent: a nested batch would add
/// nothing a flat one cannot say, and only obscure which step failed.
/// The `params` of a step whose tool takes no arguments. Strict, so a field
/// meant for some other tool is an error rather than silently dropped.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "tool", content = "params", rename_all = "snake_case")]
enum BatchStep {
    /// No params: omit `params`, or pass `null` or `{}`.
    CheckPermissions(
        #[expect(
            dead_code,
            reason = "exists only so `params: {}` deserializes; there is nothing to read"
        )]
        Option<NoArgs>,
    ),
    /// No params: omit `params`, or pass `null` or `{}`.
    ListApps(
        #[expect(
            dead_code,
            reason = "exists only so `params: {}` deserializes; there is nothing to read"
        )]
        Option<NoArgs>,
    ),
    GetAppState(AppArgs),
    Find(FindArgs),
    WaitFor(WaitForArgs),
    Click(ClickArgs),
    ClickInWindow(ClickInWindowArgs),
    Drag(DragArgs),
    Hover(HoverArgs),
    SetValue(SetValueArgs),
    TypeText(TypeTextArgs),
    SelectText(SelectTextArgs),
    PressKey(PressKeyArgs),
    MenuBar(MenuBarArgs),
    Scroll(ScrollArgs),
    PerformSecondaryAction(SecondaryActionArgs),
}

impl BatchStep {
    fn name(&self) -> &'static str {
        match self {
            Self::CheckPermissions(_) => "check_permissions",
            Self::ListApps(_) => "list_apps",
            Self::GetAppState(_) => "get_app_state",
            Self::Find(_) => "find",
            Self::WaitFor(_) => "wait_for",
            Self::Click(_) => "click",
            Self::ClickInWindow(_) => "click_in_window",
            Self::Drag(_) => "drag",
            Self::Hover(_) => "hover",
            Self::SetValue(_) => "set_value",
            Self::TypeText(_) => "type_text",
            Self::SelectText(_) => "select_text",
            Self::PressKey(_) => "press_key",
            Self::MenuBar(_) => "menu_bar",
            Self::Scroll(_) => "scroll",
            Self::PerformSecondaryAction(_) => "perform_secondary_action",
        }
    }

    /// The same `prep_*` the solo tool dispatches.
    fn prepare(self) -> Prepared {
        match self {
            Self::CheckPermissions(_) => Ok(prep_check_permissions()),
            Self::ListApps(_) => Ok(prep_list_apps()),
            Self::GetAppState(a) => prep_get_app_state(a),
            Self::Find(a) => prep_find(a),
            Self::WaitFor(a) => prep_wait_for(a),
            Self::Click(a) => prep_click(a),
            Self::ClickInWindow(a) => prep_click_in_window(a),
            Self::Drag(a) => prep_drag(a),
            Self::Hover(a) => prep_hover(a),
            Self::SetValue(a) => prep_set_value(a),
            Self::TypeText(a) => prep_type_text(a),
            Self::SelectText(a) => prep_select_text(a),
            Self::PressKey(a) => prep_press_key(a),
            Self::MenuBar(a) => prep_menu_bar(a),
            Self::Scroll(a) => prep_scroll(a),
            Self::PerformSecondaryAction(a) => prep_perform_secondary_action(a),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BatchArgs {
    /// The calls to make, in order. Each is `{"tool": "<name>", "params":
    /// {...}}` with exactly the arguments that tool takes on its own;
    /// `list_apps` and `check_permissions` take no `params`.
    steps: Vec<BatchStep>,
    /// Stop at the first step that fails (the default), reporting which steps
    /// were not run. Pass false to run every step regardless and get every
    /// result back.
    #[serde(default = "yes")]
    bail_on_error: bool,
}

/// Steps one batch may carry. The batch holds the native thread for its whole
/// duration, so it is bounded like every other budget in this server.
const MAX_BATCH_STEPS: usize = 50;

fn step_failed(r: &cua_core::session::Result<CallToolResult>) -> bool {
    match r {
        Ok(result) => result.is_error == Some(true),
        Err(_) => true,
    }
}

/// Run a whole batch. Called once, inside one `spawn_blocking`, and itself
/// issues one worker job for all the steps (`Cua::run_batch`).
fn run_batch_blocking(cua: &Cua, a: BatchArgs) -> CallToolResult {
    let total = a.steps.len();
    if total == 0 {
        return fail("steps is empty: pass at least one {\"tool\": \"<name>\", \"params\": {...}}");
    }
    if total > MAX_BATCH_STEPS {
        return fail(format!(
            "a batch holds at most {MAX_BATCH_STEPS} steps, got {total}: split it"
        ));
    }
    let names: Vec<&'static str> = a.steps.iter().map(BatchStep::name).collect();
    // A step whose arguments were refused still occupies its place: it fails
    // at its index, after the steps before it have run, exactly where a solo
    // call made at that point would have failed.
    let ops: Vec<Op<CallToolResult>> = a
        .steps
        .into_iter()
        .map(|step| step.prepare().unwrap_or_else(Op::ready))
        .collect();
    let bail = a.bail_on_error;
    let ran = match cua.run_batch(ops, move |r| bail && step_failed(r)) {
        Ok(ran) => ran,
        Err(e) => return fail(e.to_string()),
    };
    let results: Vec<CallToolResult> = ran
        .into_iter()
        .map(|r| r.unwrap_or_else(|e| fail(e.to_string())))
        .collect();
    render_batch(&names, results)
}

fn render_batch(names: &[&'static str], results: Vec<CallToolResult>) -> CallToolResult {
    let total = names.len();
    let ran = results.len();
    let failed = results.iter().filter(|r| r.is_error == Some(true)).count();
    let mut summary = format!("batch: ran {ran} of {total} step(s), {failed} failed");
    if ran < total {
        summary.push_str(&format!(
            "; stopped at step {} because it failed (bail_on_error=true) — steps {}..={} were NOT run",
            ran.saturating_sub(1),
            ran,
            total - 1
        ));
    }
    let mut content = vec![ContentBlock::text(summary)];
    for (i, result) in results.into_iter().enumerate() {
        let status = if result.is_error == Some(true) {
            "ERROR"
        } else {
            "ok"
        };
        content.push(ContentBlock::text(format!(
            "── step {i}: {} — {status}",
            names[i]
        )));
        content.extend(result.content);
    }
    for (i, name) in names.iter().enumerate().skip(ran) {
        content.push(ContentBlock::text(format!("── step {i}: {name} — not run")));
    }
    if failed > 0 {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
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
    ///
    /// A panic inside `f` is already turned into an error by the worker's own
    /// `catch_unwind`; a failed join here is reported as a tool error rather
    /// than unwinding through the MCP service.
    async fn native<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Cua) -> T + Send + 'static,
    {
        let cua = self.cua.clone();
        tokio::task::spawn_blocking(move || f(&cua))
            .await
            .map_err(|e| format!("native task failed: {e}"))
    }

    /// Run one prepared tool call: the single path every solo tool takes.
    ///
    /// A refusal decided while parsing arguments is answered without touching
    /// the worker at all; everything else is exactly one `spawn_blocking` and
    /// one worker job.
    async fn dispatch(&self, prepared: Prepared) -> CallToolResult {
        let op = match prepared {
            Ok(op) => op,
            Err(refusal) => return refusal,
        };
        match self.native(move |c| c.run(op)).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => fail(e.to_string()),
            Err(e) => fail(e),
        }
    }
}

#[tool_router(router = tool_router)]
impl CuaServer {
    #[tool(
        description = "Report whether this server holds the two macOS grants it needs: Accessibility (to read UI structure and deliver actions) and Screen Recording (to capture window images). Never prompts. Call this first when anything fails with a permission error."
    )]
    async fn check_permissions(&self) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(Ok(prep_check_permissions())).await)
    }

    #[tool(
        description = "List running applications, frontmost first. Use this to discover the exact name or bundle identifier to pass as `app`. Apps marked `background` have no windows and usually cannot be driven."
    )]
    async fn list_apps(&self) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(Ok(prep_list_apps())).await)
    }

    #[tool(
        description = "Read one app's front window: returns its accessibility tree and, by default, a screenshot taken from the same moment. For a dense app, pass skeleton=true first: large deep subtrees collapse to `(+N elements — pass scope_element_id=K to expand)`, which keeps the overall map cheap, then call again with scope_element_id=K to spend the whole element budget inside just that subtree. This MUST be called before acting on an app, because it assigns the `element_index` handles that click/set_value/scroll refer to. Lines starting with `[N]` are actionable targets; lines without a bracket are context only. If the app has a menu or popover open, it is listed separately under `transient UI` — that is a different window with NO accessibility representation, so it never appears in the tree and has to be read from the screenshot and driven by shortcut or window coordinate. Does not activate the app, move the cursor, or change focus, so it is safe to call while the user is working."
    )]
    async fn get_app_state(
        &self,
        Parameters(a): Parameters<AppArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_get_app_state(a)).await)
    }

    #[tool(
        description = "Activate an element without moving the system cursor. Uses AXPress, AXPick, or AXConfirm when the element supports one; otherwise builds real AppKit mouse events (NSEvent, carrying a fresh event number, the true click count and the target's window number) and routes them to the target process through SkyLight, reporting `delivery: pid`. A synthesized ApplicationActivated notice wraps the click so custom-drawn views that only act when their app is active still accept it — the real frontmost app, keyboard focus and Space never change. Frame-bearing controls can receive an `element_index` even when they advertise no AX actions. Prefer `element_index` from the latest get_app_state; x/y is resolved to an element first. Pass count=2 for a target that only opens on a double-click; accessibility has no click count, so double-clicks use pid delivery. Pass button=right for a real rightMouseDown/rightMouseUp pair (a context menu on a control that advertises no AXShowMenu), button=middle for a middle click, and modifiers=\"cmd\" / \"shift\" / \"alt\" / \"ctrl\" (or a combination like \"cmd+shift\") to hold keys down for the press — that is how you open a link in a new tab, extend a selection to a row, or reach an app's alternate ⌥ action. There is no real-pointer fallback. SAFETY: a target whose label reads as destructive (Delete, Remove, Erase, Reset, Move to Trash, Don't Save, 삭제, 제거, 초기화, 나가기, …) is refused unless you also pass confirm_destructive=true, and so is a terse button answering a sheet or dialog that reads that way — \"OK\" under \"Delete 4 items?\" — while Cancel/No/취소 on that same dialog is never refused; credential managers, Keychain Access, System Settings and login surfaces are refused outright; nothing is delivered while the screen is locked."
    )]
    async fn click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_click(a)).await)
    }

    #[tool(
        description = "Press the mouse down at one point, move through interpolated intermediate points, and release at another — a real drag, delivered to the target process by pid with the cursor, keyboard focus, frontmost app and Space all untouched. Use it to reorder a list row, drag a file onto a folder, draw a selection rectangle on a canvas, move a slider knob, or resize a split. Each end is named independently and the two may be different elements of the same app: pass from_element_token / to_element_token (preferred, from get_app_state), or from_element_index / to_element_index, or from_window_x+from_window_y / to_window_x+to_window_y for an end that is bare canvas with no element behind it. Window coordinates are POINTS from the window's top-left corner (screenshot pixel / the `px per point` scale get_app_state reports), re-anchored to the window's live position just before the events go out. Both ends must be inside the SAME window that the app's most recent get_app_state read — a gesture cannot cross a window boundary and cua-rs refuses rather than guessing. The intermediate moves are interpolated rather than jumped, because a down at A followed by an up at B is not a drag to anything that implements one. `button` and `modifiers` work as they do on click. A success means the events were delivered in order and the mouse-up was sent even if a move failed partway; whether the target implemented a drag at all is what the returned state diff is for."
    )]
    async fn drag(&self, Parameters(a): Parameters<DragArgs>) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_drag(a)).await)
    }

    #[tool(
        description = "Move the pointer over a point WITHOUT clicking, so hover-only UI appears: a tooltip, a submenu, a row's delete button that is only drawn under the cursor, a chart's value readout. Follow it with get_app_state (or leave return_state on, which is the default) to read whatever appeared. Address it like a drag end: element_token (preferred), element_index, or window_x + window_y in POINTS from the window's top-left corner for a spot with no element. WHERE THIS IS MEASURED TO WORK: web content. In Chrome and in Safari, a button a page reveals only on CSS :hover appeared in the accessibility tree while the point was hovered and was gone once the pointer left, and the page read back the exact coordinate the event carried. WHERE IT IS MEASURED TO DO NOTHING: a Finder list row, where the same event changed neither the tree nor one byte of the window image, while a click at the same pixel in the same run selected the row. A native hover affordance is often the window server's own tracking of the real cursor rather than an event the app is handed, so treat a silent hover on a native control as expected rather than as a bug, and reach the control another way. IMPORTANT — your real cursor does not move. That is what makes this safe to use while the user is working, and it is also the permanent limit: an app that asks where the pointer IS (NSEvent.mouseLocation, a poll of the cursor position) rather than reading the event it was handed cannot react, and no setting changes that. The hover lasts until the app decides otherwise; there is no matching un-hover, and a later click or hover elsewhere replaces it."
    )]
    async fn hover(
        &self,
        Parameters(a): Parameters<HoverArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_hover(a)).await)
    }

    #[tool(
        description = "Click a bare point inside a window, with no element behind it. TWO USES. (1) TRANSIENT UI — a pop-up this app has open, reported as `transient UI open above this app's content`. That is a separate window at layer 101 which accessibility does not describe at all: no elements, no indices, `find` matches nothing in it, `click` has nothing to address, so a window-local coordinate is the only addressing that exists and this tool accepts the pop-up's own window_id. READ THIS BEFORE USING IT ON A MENU: on a standard macOS menu the event is delivered and the menu DISMISSES WITHOUT SELECTING ANYTHING — measured on KakaoTalk, twice, on two different rows. A menu tracks the real pointer to decide what is highlighted, and cua-rs never moves the real pointer. Use press_key with the item's own keyboard shortcut instead (⌘I, ⌘Y, ⌘B, ⌘T, ⌥⌘, …), which is measured to activate the item. FOR A ROW WITH NO SHORTCUT, NEITHER A CLICK NOR A KEYSTROKE WORKS: the arrow keys move the menu's highlight but nothing activates the highlighted row. Use the `menu_bar` tool, which reaches the same row where the app also draws it in its menu bar, and which reports every row's key equivalent as text so you never have to read one off an image. This tool remains the right one for a pop-up that is not a menu — a popover or panel with ordinary event-driven views — and for dismissing a menu when `escape` does not. x/y are measured from the POP-UP's own top-left corner, and the screenshot already contains the menu (macOS captures a window together with the pop-up attached to it, so there is no separate image to fetch). (2) LAST RESORT for ordinary windows: use it ONLY for custom-drawn surfaces that genuinely publish no children — maps, charts, canvases, game views — after `click` and `find` have shown there is no element to address. There, it is not a retry for a `click` that failed and it is never chosen automatically, because a point that covers nothing is indistinguishable from a typo. x and y are POINTS from the window's top-left corner (screenshot pixel / the `px per point` scale get_app_state reports), NOT screen coordinates: they are re-anchored to the window's live position just before the event is sent, so moving the window between the read and the click is harmless. Delivery is the same pid-routed SkyLight path as `click` — the cursor, keyboard focus, frontmost app and Space are untouched — but the result is labelled `pid (no element)` because NOTHING WAS VERIFIED. There is no element to inspect afterwards, so a success means only that the events were delivered to that pixel of that window; whether anything was there, and whether it was the right thing, is entirely the caller's aim. Requires that the most recent get_app_state of this app read this same window_id, OR that window_id is a pop-up this same app currently has open (which cua-rs reported to you when it opened)."
    )]
    async fn click_in_window(
        &self,
        Parameters(a): Parameters<ClickInWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_click_in_window(a)).await)
    }

    #[tool(
        description = "Replace a text element's contents by writing its accessibility value directly. This does not synthesize keystrokes, so it works on a background window and does not require focus, but it REPLACES the existing value rather than appending, and apps that only react to real key events (terminals, canvas editors, some games) will ignore it. Only works where get_app_state marked the element `editable`."
    )]
    async fn set_value(
        &self,
        Parameters(a): Parameters<SetValueArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_set_value(a)).await)
    }

    #[tool(
        description = "Scroll a scrollable element. Two tiers, chosen automatically and named in the result. If the element advertises an accessibility scroll action, whole pages go through it (`delivery: ax`) — that is the better answer where it exists, because the app decides what a page of its own content is and no coordinate is involved. If it advertises none — an Electron list, a canvas, a web area inside a native shell, which is the common case and used to be a dead end — a real scrollWheel event is delivered at the element's point, routed to the target process by pid (`delivery: pid`), with the cursor, keyboard focus and frontmost app untouched. Pass `pixels` to scroll by an exact number of POINTS instead of by pages; that always uses the wheel tier, because accessibility cannot express a distance at all. Target the scroll area, list or table itself, not the element you want to reveal. IMPORTANT: the wheel tier is REFUSED by default, because it was measured not to scroll — delivered and ignored, checked against the window's own pixels on a native scroll area and on Chromium web content, in both units, while a pid-routed pagedown keystroke scrolled the same window in the same run. So a request that would use it errors instead of reporting a success that did not happen, and the error names the alternative. To scroll something that advertises no accessibility scroll action, use press_key with `pagedown`, `pageup`, `down` or `up`: a pid-routed keystroke reaches the scroller that ignores the wheel."
    )]
    async fn scroll(
        &self,
        Parameters(a): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_scroll(a)).await)
    }

    #[tool(
        description = "Append text to a text element. Unlike set_value this preserves what is already there: it collapses the caret at the end and writes through AXSelectedText, falling back to a whole-value rewrite when the element exposes no settable selection (the result says which happened). By default (mechanism=\"ax\") it is delivered through the accessibility API, so it needs no focus and does not move the cursor — but for the same reason apps that only react to real key events (terminals, canvas editors) will ignore it. Pass mechanism=\"keystrokes\" for those targets and only those: it sends real key events routed to the target process, which reaches a terminal but lands on whatever that process's first responder is rather than on the element you named. Whichever mechanism runs, your pointer, keyboard focus and Space are untouched."
    )]
    async fn type_text(
        &self,
        Parameters(a): Parameters<TypeTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_type_text(a)).await)
    }

    #[tool(
        description = "Select a literal substring inside a text element, so a following type_text overwrites exactly that span. Pass `prefix` and/or `suffix` to disambiguate a repeated string: the search matches prefix+text+suffix and selects only the `text` part. Without them the first occurrence wins. Returns the character range that was selected."
    )]
    async fn select_text(
        &self,
        Parameters(a): Parameters<SelectTextArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_select_text(a)).await)
    }

    #[tool(
        description = "Press any key or chord on an element: `return`, `escape`, `tab`, a letter or digit, `f5`, and arbitrary combinations such as `cmd+shift+p` or `ctrl+alt+delete`. `+` or `-` separates, case does not matter, and the modifier names are the same ones the click tools take (`cmd`, `shift`, `alt`/`option`, `ctrl`, `fn`). The event is synthesized and routed to the target process by pid, never through the shared keyboard tap, so your pointer, your keyboard focus, the frontmost app and your Space are all untouched; successful results report `delivery: pid (keyboard)`. It is addressed to the PROCESS, not to the element: it lands on whatever that process's own first responder is, which cua-rs best-effort-focuses to the element you named first (AXFocused, where the app makes it settable). The result therefore reports `focus: verified | unverified | mismatched` so you can tell what actually happened — `mismatched` means the key most likely went to the named element's sibling instead, and the delivery is still made unless CUA_KEY_STRICT_FOCUS=1 is set. THIS IS HOW YOU DRIVE AN OPEN MENU, and the only way measured to work. When a pop-up is up — cua-rs reports it as `transient UI open above this app's content` — accessibility cannot see inside it, and a coordinate click was measured to dismiss the menu without selecting anything, because the menu tracks the real pointer and cua-rs never moves it. Menu items nearly always draw their own key equivalent beside them (⌘I, ⌘Y, ⌘B, ⌘T, ⌥⌘,, ⌥⌘⌫); read one off the screenshot and press it here. It goes through the app's own key handling, so it activates the item the app associates with that chord rather than a pixel you aimed at — verified on KakaoTalk, where ⌘T on the open chat-room menu toggled the window's always-on-top state. AN ITEM WITH NO SHORTCUT IS NOT REACHABLE HERE, AND THE ANSWER IS THE `menu_bar` TOOL. The arrow keys do reach an open menu and do move its highlight, but `return`, `enter` and `space` are consumed without activating the highlighted row — measured, on every event recipe tried — so do not walk a menu with `down` and assume the item ran. Most apps draw the same rows in their menu bar, which accessibility publishes in full: press `escape` here, then use `menu_bar` to read that level and press the row. `menu_bar` also reports each row's key equivalent as text already spelled for this tool, which is how you learn a pop-up row's shortcut without recognising a ⌘ glyph in a screenshot — cua-rs does no OCR. `escape` dismisses an open menu. SAFETY: `cmd+delete`, and a bare `delete`/`backspace` anywhere that is not a text field, are refused unless you also pass confirm_destructive=true — and note that a menu shortcut can be destructive too (⌥⌘⌫ leaves a KakaoTalk chat room), so read what the item says before pressing it."
    )]
    async fn press_key(
        &self,
        Parameters(a): Parameters<PressKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_press_key(a)).await)
    }

    #[tool(
        description = "Read the app's MENU BAR, and press a row in it. This is the ONLY menu accessibility describes, and it is the ONLY measured way to activate a menu row that has NO KEYBOARD SHORTCUT. Call it with `app` alone for the top-level menus, then with `path` (`>`-separated titles, e.g. `Edit`, then `Edit > Transformations`) to read one level at a time, then with `press: true` on a leaf. Each row comes back with its title, whether it is ENABLED right now, whether it opens a submenu, its CHECKMARK if it has one (which is how you read a toggle's state without looking at pixels), and its KEY EQUIVALENT already spelled the way press_key takes it (`cmd+i`, `cmd+alt+,`) — so you never have to recognise ⌘ glyphs in a screenshot. USE THIS WHEN A POP-UP MENU BLOCKS YOU: a menu a click opened is a separate window with no accessibility inside it, a coordinate click dismisses it without selecting, and only an item's own shortcut activates it — so a shortcut-less row in a pop-up is unreachable there. Very often the same row exists in the menu bar (a text view's context menu and its Edit menu are the same NSMenuItems), and pressing it here does exactly what choosing it there would. Press `escape` with press_key to dismiss the pop-up first. ENABLED IS LIVE: a menu bar validates against the app's current focus and selection, so `Make Upper Case` reads disabled with nothing selected and enabled once something is — select first, then read, then press. A disabled row is refused rather than pressed, because pressing one does nothing and reporting success would be a lie. NOTHING IS DRAWN AND NOTHING IS ACTIVATED: the press goes straight to the item through accessibility, so no menu opens on screen, your pointer does not move, the app is not raised and your frontmost app does not change — verified on TextEdit while another app was frontmost. A row that opens a submenu is refused with the names of its rows, because opening a submenu is not an action. SAFETY: a menu bar reaches Quit, Log Out, Move to Trash and Leave Chat in two steps and the app will not confirm any of them for you, so a destructive-looking title needs confirm_destructive=true."
    )]
    async fn menu_bar(
        &self,
        Parameters(a): Parameters<MenuBarArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_menu_bar(a)).await)
    }

    #[tool(
        description = "Deliver an arbitrary accessibility action to an element by name, for the verbs the dedicated tools do not cover: AXShowMenu (open a context menu), AXRaise (bring a window forward without activating the app), AXIncrement / AXDecrement (steppers and sliders), AXScrollToVisible, AXCancel. If the element does not advertise the action, the error lists the ones it does — so a wrong guess is fixable in one step. get_app_state also shows each element's actions. SAFETY: as with `click`, a destructive-looking target — or one answering a destructive-looking dialog — needs confirm_destructive=true."
    )]
    async fn perform_secondary_action(
        &self,
        Parameters(a): Parameters<SecondaryActionArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_perform_secondary_action(a)).await)
    }

    #[tool(
        description = "Search the current snapshot for elements whose label, value or role contains a string, case-insensitively. Much cheaper than re-reading a whole tree when you already know what you are looking for. Actionable matches are listed first, and label matches rank above value and role matches. Searches the snapshot you already have so the returned indices stay valid; walks afresh when there is none, or when an action has run since the last read."
    )]
    async fn find(&self, Parameters(a): Parameters<FindArgs>) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_find(a)).await)
    }

    #[tool(
        description = "Poll an app until a string appears in (or disappears from) its accessibility tree, or the timeout expires. Use this instead of guessing a sleep after an action that triggers loading, a dialog, or a navigation. Each poll re-walks the tree, so the interval is floored at 250ms. Returns whether the condition was met plus the snapshot_id of the final read."
    )]
    async fn wait_for(
        &self,
        Parameters(a): Parameters<WaitForArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.dispatch(prep_wait_for(a)).await)
    }

    #[tool(
        description = "Run several of this server's other tools in ONE call, in order, and get every step's result back tagged by its index. Each step is {\"tool\": \"<name>\", \"params\": {<exactly the arguments that tool takes on its own>}} — e.g. [{\"tool\":\"set_value\",\"params\":{\"app\":\"TextEdit\",\"element_token\":\"7-3-AXTextArea\",\"value\":\"hi\",\"return_state\":false}},{\"tool\":\"press_key\",\"params\":{\"app\":\"TextEdit\",\"element_token\":\"7-3-AXTextArea\",\"key\":\"cmd+s\"}}]. Every tool except batch itself can be a step; list_apps and check_permissions take no params. A step is the SAME code as the solo call: the same safety gates (app scope, refused apps, confirm_destructive, screen lock, yield-to-human) run for each step, and snapshot_id / element_token staleness is checked against the state the PREVIOUS steps left behind — an action with return_state on, or a get_app_state, replaces the snapshot, so a later step citing the old snapshot_id fails exactly as it would solo. Plan for that: turn return_state off on intermediate steps, or cite only the snapshot you expect to still be current. bail_on_error (default true) stops at the first failing step and reports which steps were not run; pass false to run every step and collect every result. The whole batch runs as one uninterrupted job on the native thread, so no other call can interleave with it — which also means wait_for timeouts add up, so keep long waits out of large batches. At most 50 steps."
    )]
    async fn batch(
        &self,
        Parameters(a): Parameters<BatchArgs>,
    ) -> Result<CallToolResult, McpError> {
        // One spawn_blocking for the whole batch, and inside it one worker job
        // (`Cua::run_batch`) — not one hop per step.
        Ok(match self.native(move |c| run_batch_blocking(c, a)).await {
            Ok(result) => result,
            Err(e) => fail(e),
        })
    }

    #[tool(
        description = "Read the clipboard's plain text. Not tied to any app and changes nothing. When the clipboard holds no text (an image, a file list, nothing at all) this succeeds with `text: none` and lists `available_types` instead of failing. SAFETY: text that may be a secret is withheld and only the type list is returned — when the writer marked it concealed (org.nspasteboard.ConcealedType, which password managers set), when it was written by a credential or security app, or when such an app is frontmost. To move withheld text into a field, paste it with press_key cmd+v instead of reading it. On macOS 26 the system may ask the user before letting this process read the clipboard; `access` reports that setting."
    )]
    async fn clipboard_get(
        &self,
        Parameters(a): Parameters<ClipboardGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let max = a.max_chars.unwrap_or(20_000).clamp(1, 200_000);
        match self.native(|c| c.clipboard_get()).await {
            Ok(Ok(r)) => Ok(ok(render_clipboard_read(&r, max))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Replace the clipboard with plain text. Typical use: clipboard_set, then press_key cmd+v on the target field to paste it — the paste is the action, and it is gated like any other. Not tied to any app, so the per-app gates do not apply; nothing is written while the session is locked, and with CUA_YIELD_TO_HUMAN=1 nothing is written while the human is actively using the machine. Overwrites whatever the user had copied."
    )]
    async fn clipboard_set(
        &self,
        Parameters(a): Parameters<ClipboardSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.native(move |c| c.clipboard_set(&a.text)).await {
            Ok(Ok(w)) => Ok(ok(render_clipboard_write(&w))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Empty the clipboard. Gated like clipboard_set: refused while the session is locked, and with CUA_YIELD_TO_HUMAN=1 while the human is active."
    )]
    async fn clipboard_clear(&self) -> Result<CallToolResult, McpError> {
        match self.native(|c| c.clipboard_clear()).await {
            Ok(Ok(w)) => Ok(ok(render_clipboard_write(&w))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "List every window of one app: window_id, title, frame (x,y,w,h in global screen points), window level, and whether it is minimized, full screen, the app's main window and its focused window. Windows are listed front to back as accessibility orders them; windows the window server knows about but accessibility does not list (usually on another Space) come last, marked `not manageable`. window_id is the window server's CGWindowID, matched to the accessibility window by frame through public API only — it needs Screen Recording, and a window that cannot be paired unambiguously gets no id rather than a guessed one. Read-only: nothing is raised or activated."
    )]
    async fn list_windows(
        &self,
        Parameters(a): Parameters<ListWindowsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        match self.native(move |c| c.list_windows(&app)).await {
            Ok(Ok(listing)) => Ok(ok(render_window_listing(&listing))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Raise one window to the front of its app's windows with the accessibility AXRaise action, and make it the app's main window where the app allows. It does NOT activate the app: the frontmost app, keyboard focus and Space stay exactly as the human left them, so the window comes forward visually without taking their typing. Use it before reading a window that another window of the same app covers. A minimized window is refused — call restore_window. Gated like every action (CUA_ALLOWED_APPS scope, forbidden apps, screen lock, yield to human)."
    )]
    async fn focus_window(
        &self,
        Parameters(a): Parameters<WindowTargetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(a, cua_core::WindowOp::Focus).await
    }

    #[tool(
        description = "Move a window so its top-left corner is at x,y in global screen POINTS (see list_displays for each display's bounds), by writing its AXPosition attribute — no drag, no cursor movement, no activation. Refused when less than 40 points of the window would land on any active display, because a window placed off screen cannot be recovered by the human. The response reads the frame back from the app, which may clamp it."
    )]
    async fn move_window(
        &self,
        Parameters(a): Parameters<MoveWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(a.window, cua_core::WindowOp::Move { x: a.x, y: a.y })
            .await
    }

    #[tool(
        description = "Resize a window to width x height POINTS, keeping its top-left corner fixed, by writing its AXSize attribute. Refused when the result would leave less than 40 points on any active display. Apps enforce their own minimum and maximum sizes; the response reads back the size the app actually kept."
    )]
    async fn resize_window(
        &self,
        Parameters(a): Parameters<ResizeWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(
            a.window,
            cua_core::WindowOp::Resize {
                width: a.width,
                height: a.height,
            },
        )
        .await
    }

    #[tool(
        description = "Minimize a window to the Dock by setting its AXMinimized attribute. Already-minimized is reported, not an error. Undo with restore_window."
    )]
    async fn minimize_window(
        &self,
        Parameters(a): Parameters<WindowTargetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(a, cua_core::WindowOp::Minimize).await
    }

    #[tool(
        description = "Maximize a window. Presses the window's own zoom button through accessibility (AXZoomButton, AXPress) when it has one — the app's native zoom, which fits the window to its content or the screen as the app decides; it is NOT full screen and does not create a Space. A window with no zoom button (panels, custom title bars) is instead sized to the bounds of the display it is mostly on, and the response says so. The frame from before is remembered so restore_window can return to it; calling maximize_window again on a window it already maximized is a no-op, because the zoom button toggles."
    )]
    async fn maximize_window(
        &self,
        Parameters(a): Parameters<WindowTargetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(a, cua_core::WindowOp::Maximize).await
    }

    #[tool(
        description = "Undo the last thing that took a window out of its normal state, in this order: un-minimize it (AXMinimized=false) if minimized; otherwise leave full screen (AXFullScreen=false) if in full screen; otherwise put back the frame it had before maximize_window. Anything else is reported as nothing to restore."
    )]
    async fn restore_window(
        &self,
        Parameters(a): Parameters<WindowTargetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.window_op(a, cua_core::WindowOp::Restore).await
    }

    #[tool(
        description = "List the active displays: display id, bounds in global screen POINTS (the coordinate space move_window and list_windows use; the main display's top-left is 0,0), pixels per point, and which one is main. Needs no permission."
    )]
    async fn list_displays(&self) -> Result<CallToolResult, McpError> {
        match self.native(|c| c.list_displays()).await {
            Ok(Ok(displays)) => Ok(ok(render_displays(&displays))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Launch an installed app through NSWorkspace, by bundle identifier (exact), app name (looked up in /Applications, /System/Applications and ~/Applications), or path to the .app. By DEFAULT IT IS NOT ACTIVATED: it starts behind whatever the human is using, which is enough for get_app_state and every action. Pass activate=true only when the user wants to see the app, since activation takes their keyboard focus. Launching an app that is already running does nothing and says so. Gated like every action: CUA_ALLOWED_APPS scope (an app outside it is refused before it starts), the forbidden-app list, and the screen lock. System prompts for quarantined apps are suppressed; such a launch fails instead of putting a dialog in front of the human."
    )]
    async fn launch_app(
        &self,
        Parameters(a): Parameters<LaunchAppArgs>,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let activate = a.activate;
        match self.native(move |c| c.launch_app(&app, activate)).await {
            Ok(Ok(r)) => Ok(ok(render_launch(&r))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Quit a running app gracefully — the same quit request its own Quit menu item sends, never a force quit. The app may ask about unsaved changes and stay open; the response says whether the process actually exited within 3 seconds, and get_app_state then shows any save prompt. REQUIRES confirm_destructive=true on every call, because quitting can discard work. Session-critical processes are always refused whatever the flags: Finder, Dock, SystemUIServer, Control Center, Notification Center, Window Manager, loginwindow, WindowServer, plus this server's own process and the app that launched it. Gated like every action (scope, forbidden apps, screen lock, yield to human)."
    )]
    async fn close_app(
        &self,
        Parameters(a): Parameters<CloseAppArgs>,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let confirm = a.confirm_destructive;
        match self.native(move |c| c.close_app(&app, confirm)).await {
            Ok(Ok(r)) => Ok(ok(render_close(&r))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "List macOS notifications: app, title, subtitle, body, the custom actions each one offers, and an `id` to pass to dismiss_notification / notification_action. Notifications are drawn by Notification Center, not by the app that posted them, so `app` is the posting app's display name as Notification Center shows it. The id is Notification Center's own UUID for the notification — stable across reads and from banner to history, so it does not go stale when a neighbour is removed. By default this reads only what is on screen right now: a banner lasts about 5 s and then moves to history. Pass include_history=true to read the history panel too; cua-rs opens it from the menu-bar clock, expands per-app stacks, and closes it again — that presses UI, so it is gated like an action. Verified on macOS 26.2; see DESIGN.md §13 for what is and is not."
    )]
    async fn list_notifications(
        &self,
        Parameters(a): Parameters<ListNotificationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let history = a.include_history;
        match self.native(move |c| c.list_notifications(history)).await {
            Ok(Ok(listing)) => Ok(ok(render_notifications(&listing, history))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Dismiss one notification by the id list_notifications gave, by pressing the Close action Notification Center advertises on it (there is no separate close button in its accessibility tree). Measured on macOS 26.2: Close removes the notification from history too, not just the banner. Looks at live banners first, then the history panel (opened and closed again around the call) unless include_history=false. The result says whether the notification was verified gone afterwards. SAFETY: gated like any action on Notification Center, and additionally refused when the notification came from an app cua-rs may not act on (password managers, authenticators, Keychain, System Settings) or, under CUA_ALLOWED_APPS, from an app outside the scope or one that cannot be identified."
    )]
    async fn dismiss_notification(
        &self,
        Parameters(a): Parameters<NotificationIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match notification_id(&a.id) {
            Ok(id) => id,
            Err(e) => return Ok(fail(e)),
        };
        let history = a.include_history;
        match self
            .native(move |c| c.dismiss_notification(&id, history))
            .await
        {
            Ok(Ok(acted)) => Ok(ok(render_acted(&acted))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Dismiss every notification, one Close at a time, re-reading Notification Center after each one. Best effort: it STOPS at the first notification it cannot close — including one refused by a safety gate — and reports what it dismissed, what it stopped on and why, and how many remain. Covers the history panel too unless include_history=false. Same gates as dismiss_notification, applied per notification."
    )]
    async fn dismiss_all_notifications(
        &self,
        Parameters(a): Parameters<DismissAllArgs>,
    ) -> Result<CallToolResult, McpError> {
        let history = a.include_history;
        match self
            .native(move |c| c.dismiss_all_notifications(history))
            .await
        {
            Ok(Ok(outcome)) => {
                let text = render_dismiss_all(&outcome);
                if outcome.stopped_on.is_some() {
                    Ok(fail(text))
                } else {
                    Ok(ok(text))
                }
            }
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }

    #[tool(
        description = "Press one of the named actions a notification advertises, by its label as list_notifications reports it under `actions` (localized: on a Korean system Show Details is `세부사항 보기`). Only the custom actions Notification Center itself publishes are reachable. Verified on macOS 26.2 for the system ones (Show Details, Close); APP-DEFINED BUTTONS (Reply, Snooze, Mark as Read…) WERE NOT AVAILABLE TO TEST and may not appear here at all — if the action is not advertised the error lists what is. The bare default click (AXPress, which hands off to the posting app) is not offered. SAFETY: a destructive-looking label (Clear All, Delete) needs confirm_destructive=true, and the same source-app gate as dismiss_notification applies — an action on an authenticator's notification is acting on the authenticator."
    )]
    async fn notification_action(
        &self,
        Parameters(a): Parameters<NotificationActionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match notification_id(&a.id) {
            Ok(id) => id,
            Err(e) => return Ok(fail(e)),
        };
        if a.action.trim().is_empty() {
            return Ok(fail(
                "`action` is empty; pass one of the labels list_notifications reported",
            ));
        }
        let action = a.action.clone();
        let history = a.include_history;
        let confirm = a.confirm_destructive;
        match self
            .native(move |c| c.notification_action(&id, &action, history, confirm))
            .await
        {
            Ok(Ok(acted)) => Ok(ok(render_acted(&acted))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }
}

impl CuaServer {
    /// Every window-changing tool is this with a different [`cua_core::WindowOp`].
    async fn window_op(
        &self,
        a: WindowTargetArgs,
        op: cua_core::WindowOp,
    ) -> Result<CallToolResult, McpError> {
        let app = a.app.clone();
        let sel = a.selector();
        match self.native(move |c| c.window_action(&app, sel, op)).await {
            Ok(Ok(r)) => Ok(ok(render_window_outcome(&r))),
            Ok(Err(e)) => Ok(fail(e.to_string())),
            Err(e) => Ok(fail(e)),
        }
    }
}

fn render_frame(f: &cua_core::CGRect) -> String {
    format!(
        "{:.0},{:.0} {:.0}x{:.0}",
        f.origin.x, f.origin.y, f.size.width, f.size.height
    )
}

/// One window per line, flags inline, so the exact id or title to pass back is
/// at the start of the line a model reads.
fn render_window(w: &cua_core::WindowSummary) -> String {
    let mut line = match w.window_id {
        Some(id) => format!("window_id={id}"),
        None => "window_id=-".to_string(),
    };
    line.push_str(&format!(
        "  {:?}",
        w.title.as_deref().unwrap_or("(untitled)")
    ));
    match &w.frame {
        Some(f) => line.push_str(&format!("  at {}", render_frame(f))),
        None => line.push_str("  (no frame)"),
    }
    if let Some(level) = w.level {
        line.push_str(&format!("  level={level}"));
    }
    if let Some(sub) = &w.subrole {
        line.push_str(&format!("  {sub}"));
    }
    let flags: Vec<&str> = [
        (w.focused, "focused"),
        (w.main, "main"),
        (w.minimized, "minimized"),
        (w.full_screen, "full screen"),
        (w.on_screen == Some(false) && !w.minimized, "off screen"),
        (
            !w.manageable,
            "not manageable: accessibility does not list it (another Space?)",
        ),
    ]
    .into_iter()
    .filter_map(|(on, label)| on.then_some(label))
    .collect();
    if !flags.is_empty() {
        line.push_str(&format!("  ({})", flags.join(", ")));
    }
    line
}

fn render_window_listing(listing: &cua_core::WindowListing) -> String {
    let mut out = format!(
        "{} (pid {}), {} windows, front to back\n",
        listing.app.name,
        listing.app.pid,
        listing.windows.len()
    );
    for w in &listing.windows {
        out.push_str("  ");
        out.push_str(&render_window(w));
        out.push('\n');
    }
    for warning in &listing.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}

/// What a window tool did, then the window as the app reports it afterwards.
fn render_window_outcome(r: &cua_core::WindowOutcome) -> String {
    let mut out = format!(
        "{} on {}\ndelivery: {}  (accessibility attribute/action on the window: cursor, keyboard focus and frontmost app untouched)\nchanged: {}\nbefore: {}\nafter:  {}",
        r.action.verb,
        r.action.target,
        r.action.delivery.as_str(),
        r.action.ui_changed.as_str(),
        render_window(&r.before),
        render_window(&r.after),
    );
    if let Some(note) = &r.note {
        out.push_str(&format!("\nnote: {note}"));
    }
    out.push_str(
        "\nThe app's element tree was not re-read; call get_app_state before acting on \
         coordinates in this window, since its geometry may have changed.",
    );
    out
}

fn render_displays(displays: &[cua_core::DisplayInfo]) -> String {
    let mut out = format!("{} active displays (global points)\n", displays.len());
    for d in displays {
        out.push_str(&format!(
            "  display_id={}  at {}  {:.1} px per point{}\n",
            d.id,
            render_frame(&d.frame),
            d.scale,
            if d.main { "  (main)" } else { "" }
        ));
    }
    out
}

fn render_launch(r: &cua_core::LaunchOutcome) -> String {
    let head = if r.already_running {
        format!(
            "{} was already running (pid {}); nothing was launched or activated",
            r.app.name, r.app.pid
        )
    } else {
        format!(
            "launched {} (pid {}) from {}",
            r.app.name, r.app.pid, r.path
        )
    };
    let front = match (r.frontmost, r.activate_requested) {
        (true, true) => "frontmost: yes (activation was requested)",
        (true, false) => {
            "frontmost: yes — it came forward on its own although activation was not requested"
        }
        (false, true) => {
            "frontmost: not yet (activation was requested; it may still be coming forward)"
        }
        (false, false) => "frontmost: no (not activated; the human's focus is unchanged)",
    };
    format!(
        "{head}\nbundle_id: {}\n{front}\nIts windows may take a moment to appear; call list_windows or get_app_state next.",
        r.app.bundle_id.as_deref().unwrap_or("-")
    )
}

fn render_close(r: &cua_core::CloseOutcome) -> String {
    if r.quit {
        format!(
            "{} (pid {}) quit after {}ms",
            r.app.name, r.app.pid, r.waited_ms
        )
    } else {
        format!(
            "asked {} (pid {}) to quit, and it is still running after {}ms. It is most likely \
             asking about unsaved changes: call get_app_state to see the prompt, and answer it \
             (Save, or Don't Save with confirm_destructive) to finish quitting. cua-rs never \
             force-quits.",
            r.app.name, r.app.pid, r.waited_ms
        )
    }
}

/// Render a clipboard read.
///
/// The type list is printed in every branch: for `none` it is the answer to
/// "what is there instead", and for a withheld read it is what makes the
/// refusal checkable.
fn render_clipboard_read(r: &cua_core::ClipboardRead, max_chars: usize) -> String {
    let mut s = String::new();
    match &r.contents {
        cua_core::ClipboardContents::Text(text) => {
            let total = text.chars().count();
            if total > max_chars {
                let cut: String = text.chars().take(max_chars).collect();
                s.push_str(&format!(
                    "text: {total} chars, showing the first {max_chars} (raise max_chars for more)\n{cut}\n"
                ));
            } else {
                s.push_str(&format!("text: {total} chars\n{text}\n"));
            }
        }
        cua_core::ClipboardContents::NoText => {
            s.push_str("text: none (the clipboard holds no plain text)\n");
            if r.access == cua_core::ClipboardAccess::AlwaysDeny {
                s.push_str(
                    "  macOS is set to DENY this process clipboard access, so this does not mean the clipboard is empty. The user can change it in System Settings > Privacy & Security > Paste from Other Apps\n",
                );
            }
        }
        cua_core::ClipboardContents::Withheld(reason) => {
            s.push_str(&format!("text: withheld\n  {reason}\n"));
        }
    }
    s.push_str(&format!("available_types: {}\n", render_types(&r.types)));
    s.push_str(&format!("change_count: {}\n", r.change_count));
    s.push_str(&format!("access: {}", r.access.as_str()));
    s
}

fn render_types(types: &[String]) -> String {
    if types.is_empty() {
        "(none — the clipboard is empty)".to_string()
    } else {
        types.join(", ")
    }
}

fn render_clipboard_write(w: &cua_core::ClipboardWrite) -> String {
    let what = if w.verb == "clipboard_clear" {
        "cleared the clipboard".to_string()
    } else {
        format!("wrote {} chars of plain text to the clipboard", w.chars)
    };
    format!(
        "{}: {what}\nchange_count: {}\nverified: {}",
        w.verb,
        w.change_count,
        if w.verified {
            "yes (metadata re-read matches: no other writer in between)"
        } else {
            "NO (the clipboard's metadata does not match the write — another app may have written in between; call clipboard_get to see what is there)"
        }
    )
}

fn render_notifications(
    listing: &cua_core::notifications::NotificationListing,
    history: bool,
) -> String {
    use cua_core::notifications::NotificationKind;
    let mut out = if listing.notifications.is_empty() {
        if history {
            "no notifications on screen or in the history panel\n".to_string()
        } else {
            "no notification banners on screen right now (banners last about 5 s; pass include_history=true to read the history panel)\n".to_string()
        }
    } else {
        format!("{} notification(s)\n", listing.notifications.len())
    };
    for n in &listing.notifications {
        out.push_str(&format!(
            "\n- id={}  app={}{}{}\n",
            n.id,
            n.app.as_deref().unwrap_or("(unknown)"),
            if n.in_history {
                "  [history]"
            } else {
                "  [banner]"
            },
            if n.kind == NotificationKind::CollapsedStack {
                "  [collapsed stack: top item only]"
            } else {
                ""
            },
        ));
        for (k, v) in [
            ("title", &n.title),
            ("subtitle", &n.subtitle),
            ("body", &n.body),
            ("time", &n.time),
        ] {
            if let Some(v) = v {
                out.push_str(&format!("  {k}: {v}\n"));
            }
        }
        let actions: Vec<&str> = n
            .actions
            .iter()
            .map(String::as_str)
            .filter(|a| *a != "AXPress")
            .collect();
        if !actions.is_empty() {
            out.push_str(&format!("  actions: {}\n", actions.join(", ")));
        }
    }
    if listing.opened_panel {
        out.push_str("\n(opened the history panel to read it, and closed it again)\n");
    }
    for w in &listing.warnings {
        out.push_str(&format!("\nwarning: {w}\n"));
    }
    out
}

/// One pressed notification action.
///
/// Not `render_action`: Notification Center draws everything in one
/// full-screen window, which the generic renderer reports as "transient UI open
/// above this app's content" with advice about menus that does not apply here,
/// and its window-fingerprint diff has nothing to compare for this process. The
/// verification that matters — is the notification still listed — is reported
/// instead.
fn render_acted(acted: &cua_core::notifications::NotificationActed) -> String {
    let n = &acted.notification;
    let mut out = format!(
        "pressed `{}` on notification {} from `{}`",
        acted.label,
        n.id,
        n.app.as_deref().unwrap_or("unknown app")
    );
    let what: Vec<&str> = [&n.title, &n.subtitle, &n.body]
        .iter()
        .filter_map(|p| p.as_deref())
        .collect();
    if !what.is_empty() {
        out.push_str(&format!(" ({})", what.join(" / ")));
    }
    out.push_str(&format!(
        "\ndelivery: {}  (accessibility custom action: cursor, focus and frontmost app untouched)\n",
        match acted.result.delivery {
            cua_core::Delivery::Ax => "ax",
            _ => "other",
        }
    ));
    out.push_str(if acted.gone {
        "verified: the notification is no longer in Notification Center\n"
    } else {
        "the notification is still listed afterwards (expected for actions other than Close)\n"
    });
    for w in &acted.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

fn render_dismiss_all(o: &cua_core::notifications::DismissAllOutcome) -> String {
    let mut out = format!("dismissed {} notification(s)\n", o.dismissed.len());
    for n in &o.dismissed {
        out.push_str(&format!(
            "  - {} ({}: {})\n",
            n.id,
            n.app.as_deref().unwrap_or("unknown app"),
            n.title.as_deref().unwrap_or("")
        ));
    }
    if let Some((n, why)) = &o.stopped_on {
        out.push_str(&format!(
            "STOPPED on {} ({}): {why}\n",
            n.id,
            n.app.as_deref().unwrap_or("unknown app")
        ));
    }
    out.push_str(&format!("{} notification(s) remain\n", o.remaining));
    for w in &o.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

/// Render the transient UI an app has up, or nothing at all when it has none.
///
/// Printed as its own block rather than folded into the tree, because it is not
/// part of the tree and saying otherwise would be the lie this feature exists to
/// stop. A pop-up is a separate window with no accessibility representation:
/// there are no element indices inside it, `find` will not match its items, and
/// `click` has nothing to address. What it has is a window id and a frame, and
/// those are exactly what `click_in_window` takes.
fn render_popups(popups: &[cua_core::TransientWindow]) -> String {
    if popups.is_empty() {
        return String::new();
    }
    let mut s = String::from("\ntransient UI open above this app's content (topmost first):\n");
    for p in popups {
        s.push_str(&format!(
            "  window_id={} layer={} at {:.0},{:.0} size {:.0}x{:.0} points{}\n",
            p.id,
            p.layer,
            p.frame.origin.x,
            p.frame.origin.y,
            p.frame.size.width,
            p.frame.size.height,
            match p.appeared {
                Some(true) => "  <- OPENED just now",
                Some(false) => "  (was already open)",
                None => "",
            }
        ));
    }
    s.push_str(
        "  Accessibility cannot see inside these: no elements, no indices, and `find` will not \
         match their contents. Read them from the screenshot, which already includes them. \
         TO ACTIVATE AN ITEM, USE press_key WITH THE ITEM'S OWN KEYBOARD SHORTCUT (⌘I, ⌘Y, ⌘B, \
         ⌘T, ⌥⌘, …) — measured to work on a real menu. click_in_window against the pop-up's \
         window_id is accepted and the event is delivered, but on a standard macOS menu it was \
         measured to DISMISS the menu without selecting anything: the menu tracks the real \
         pointer, which cua-rs will not move. So a coordinate is the fallback for pop-ups that \
         are not menus, not the way to pick a menu row. An item with no shortcut may not be \
         reachable at all. press_key `escape` dismisses the menu.\n",
    );
    s
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
    // Before the tree diff, deliberately. When an action opens a menu the diff
    // is the least informative part of the answer — the window it describes did
    // not change, because everything that happened happened somewhere else.
    s.push_str(&render_popups(&r.popups));
    if let Some(focus) = &r.focus {
        s.push_str(&render_focus(focus));
    }
    if let Some(state) = &r.state {
        s.push_str(&render_post_action_state(state));
    }
    s
}

/// Say where the input actually went, for the paths where that is a real
/// question.
///
/// A pid-routed key event is addressed to a process and arrives at that
/// process's first responder, so "it succeeded" and "it landed on the element
/// you named" are two different claims. Printing only the first one is what
/// let a keystroke into the wrong field of the same app read as an unqualified
/// success — `ui_changed` cannot catch it either, since two text fields of one
/// window usually share a role and have no title, leaving the fingerprint
/// identical.
fn render_focus(focus: &cua_core::FocusCheck) -> String {
    let mut s = format!("\nfocus: {}", focus.state.as_str());
    s.push_str(match focus.state {
        cua_core::FocusState::Verified => {
            "  (the app names the element you addressed as its focused one, so this is where the input went)"
        }
        cua_core::FocusState::Unverified => {
            "  (this app published no focused element, so nothing here says where the input went — NOT evidence that it missed. Re-read the element to be sure)"
        }
        cua_core::FocusState::Mismatched => {
            "  (the app names a DIFFERENT element of the same process as focused — the input most likely went there instead. It cannot have reached another app: the event was routed to this pid only. Click the element first, or set CUA_KEY_STRICT_FOCUS=1 to refuse rather than deliver)"
        }
    });
    if let Some(other) = &focus.focused_instead {
        s.push_str(&format!("\n  focused instead: {other}"));
    }
    if !focus.focus_write_accepted {
        s.push_str(&format!(
            "\n  AXFocused write refused: {} (not itself a failure — AXFocused is unsettable on many elements, including Terminal's text view)",
            focus
                .focus_write_error
                .as_deref()
                .unwrap_or("no reason given")
        ));
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
             instead of a mis-click. Actions pass through four safety gates: apps that hold \
             credentials or system security state are never driven (reading them is still \
             allowed, minus screenshots), a target whose label reads as destructive — or that answers a dialog which \
             does — needs confirm_destructive=true, nothing is delivered while the session is locked, and — \
             if the operator enabled it — cua-rs stands down on an app the human is using. \
             Every refusal names what to pass or change. To make several calls in one round \
             trip, use batch: each step is the same call, through the same gates."
                .to_string(),
        );
        info
    }
}

/// One level of a menu bar, as lines a model reads top to bottom.
///
/// Deliberately one row per line with the flags inline rather than a table:
/// what a caller needs is the exact title to pass back, whether it can be
/// pressed now, and whether pressing it is even the right move (a submenu is
/// not). Separators are drawn, because a menu the caller can also see should
/// have the same shape here.
fn render_menu(listing: &cua_core::MenuListing) -> String {
    let where_ = if listing.path.is_empty() {
        "menu bar".to_string()
    } else {
        format!("`{}`", listing.path)
    };
    let mut out = format!("{where_}, {} rows\n", listing.items.len());
    for item in &listing.items {
        if item.title.is_empty() {
            out.push_str("  ───\n");
            continue;
        }
        out.push_str("  ");
        out.push_str(match &item.mark {
            Some(mark) => mark,
            None => " ",
        });
        out.push(' ');
        out.push_str(&item.title);
        if item.has_submenu {
            out.push_str(" ›");
        }
        if let Some(shortcut) = &item.shortcut {
            out.push_str(&format!("  [{shortcut}]"));
        }
        if !item.enabled {
            out.push_str("  (disabled)");
        }
        out.push('\n');
    }
    if listing.items.iter().any(|i| i.has_submenu) {
        out.push_str(
            "rows marked › open a submenu: read that level before pressing anything in it\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> BatchArgs {
        serde_json::from_value(json).unwrap()
    }

    fn texts(r: &CallToolResult) -> Vec<String> {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[test]
    fn a_step_is_the_tool_name_plus_that_tools_own_arguments() {
        let a = args(serde_json::json!({
            "steps": [
                {"tool": "list_apps"},
                {"tool": "check_permissions", "params": null},
                {"tool": "list_apps", "params": {}},
                {"tool": "click", "params": {"app": "Safari", "element_token": "7-12-AXButton", "confirm_destructive": true}},
                {"tool": "press_key", "params": {"app": "Safari", "element_index": "3", "key": "return"}},
                {"tool": "menu_bar", "params": {"app": "TextEdit", "path": "Edit"}}
            ]
        }));
        assert!(a.bail_on_error, "bail_on_error defaults to true");
        let names: Vec<_> = a.steps.iter().map(BatchStep::name).collect();
        assert_eq!(
            names,
            [
                "list_apps",
                "check_permissions",
                "list_apps",
                "click",
                "press_key",
                "menu_bar"
            ]
        );
        match &a.steps[3] {
            BatchStep::Click(c) => {
                assert!(c.confirm_destructive);
                assert_eq!(c.target.element_token.as_deref(), Some("7-12-AXButton"));
            }
            other => panic!("expected click, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_tool_or_a_nested_batch_rejects_the_whole_batch() {
        for bad in ["batch", "not_a_tool"] {
            let r: Result<BatchArgs, _> = serde_json::from_value(serde_json::json!({
                "steps": [{"tool": "list_apps"}, {"tool": bad, "params": {}}]
            }));
            assert!(r.is_err(), "{bad} must not deserialize as a step");
        }
        // A step missing a field its tool requires is refused the same way.
        let r: Result<BatchArgs, _> = serde_json::from_value(serde_json::json!({
            "steps": [{"tool": "set_value", "params": {"app": "Safari"}}]
        }));
        assert!(r.is_err());
    }

    /// Three grant-free steps; the middle one is refused by `click`'s own
    /// argument check (no target at all), which is exactly the refusal a solo
    /// `click` gives.
    fn three_steps(bail: bool) -> BatchArgs {
        args(serde_json::json!({
            "bail_on_error": bail,
            "steps": [
                {"tool": "check_permissions"},
                {"tool": "click", "params": {"app": "Finder"}},
                {"tool": "list_apps"}
            ]
        }))
    }

    #[test]
    fn bail_on_error_stops_at_the_failing_step() {
        let cua = Cua::new();
        let r = run_batch_blocking(&cua, three_steps(true));
        assert_eq!(r.is_error, Some(true));
        let t = texts(&r);
        assert!(
            t[0].starts_with("batch: ran 2 of 3 step(s), 1 failed"),
            "{t:?}"
        );
        assert!(t[0].contains("stopped at step 1"), "{t:?}");
        assert!(t.iter().any(|l| l == "── step 0: check_permissions — ok"));
        assert!(t.iter().any(|l| l == "── step 1: click — ERROR"));
        assert!(t
            .iter()
            .any(|l| l.contains("pass element_token (preferred)")));
        assert!(t.iter().any(|l| l == "── step 2: list_apps — not run"));
        assert!(
            !t.iter().any(|l| l.contains("* = frontmost")),
            "list_apps must not run"
        );
    }

    #[test]
    fn without_bail_every_step_runs_and_reports() {
        let cua = Cua::new();
        let r = run_batch_blocking(&cua, three_steps(false));
        assert_eq!(
            r.is_error,
            Some(true),
            "one step failed, so the batch is an error"
        );
        let t = texts(&r);
        assert!(
            t[0].starts_with("batch: ran 3 of 3 step(s), 1 failed"),
            "{t:?}"
        );
        assert!(t.iter().any(|l| l == "── step 1: click — ERROR"));
        assert!(t.iter().any(|l| l == "── step 2: list_apps — ok"));
        assert!(t.iter().any(|l| l.contains("* = frontmost")));
    }

    #[test]
    fn a_solo_call_and_a_batched_step_give_the_same_refusal() {
        let solo =
            prep_click(serde_json::from_value(serde_json::json!({"app": "Finder"})).unwrap())
                .err()
                .unwrap();
        let cua = Cua::new();
        let batched = run_batch_blocking(
            &cua,
            args(serde_json::json!({"steps": [{"tool": "click", "params": {"app": "Finder"}}]})),
        );
        assert_eq!(texts(&batched)[2], texts(&solo)[0]);
    }

    #[test]
    fn an_empty_or_oversized_batch_is_refused_before_anything_runs() {
        let cua = Cua::new();
        let r = run_batch_blocking(&cua, args(serde_json::json!({"steps": []})));
        assert_eq!(r.is_error, Some(true));
        let many: Vec<_> = (0..=MAX_BATCH_STEPS)
            .map(|_| serde_json::json!({"tool": "list_apps"}))
            .collect();
        let r = run_batch_blocking(&cua, args(serde_json::json!({"steps": many})));
        assert_eq!(r.is_error, Some(true));
        assert!(texts(&r)[0].contains("at most"));
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use cua_core::notifications::{
        DismissAllOutcome, Notification, NotificationKind, NotificationListing,
    };

    fn note(id: &str, history: bool, kind: NotificationKind) -> Notification {
        Notification {
            id: id.into(),
            app: Some("스크립트 편집기".into()),
            title: Some("hist 3".into()),
            subtitle: None,
            body: Some("hist body 3".into()),
            time: None,
            kind,
            actions: vec![
                "AXPress".into(),
                "세부사항 보기".into(),
                "보기".into(),
                "닫기".into(),
            ],
            in_history: history,
        }
    }

    #[test]
    fn notification_ids_must_look_like_uuids() {
        assert_eq!(
            notification_id("  805A1087-89B8-482B-B822-30F2726D4663 ").unwrap(),
            "805A1087-89B8-482B-B822-30F2726D4663"
        );
        assert!(notification_id("").unwrap_err().contains("empty"));
        assert!(notification_id("   ").is_err());
        // A title or a position is the likely mistake, and gets told what an id is.
        let e = notification_id("probe title").unwrap_err();
        assert!(e.contains("UUID"), "{e}");
        assert!(
            notification_id("3").is_ok(),
            "a bare hex digit is shaped like an id; the worker reports not-found"
        );
        assert!(notification_id("0.0.1").is_err());
        assert!(notification_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn args_default_history_on_for_actions_and_off_for_listing() {
        let l: ListNotificationsArgs = serde_json::from_str("{}").unwrap();
        assert!(!l.include_history);
        let d: NotificationIdArgs = serde_json::from_str(r#"{"id":"AB-12"}"#).unwrap();
        assert!(d.include_history);
        let all: DismissAllArgs = serde_json::from_str("{}").unwrap();
        assert!(all.include_history);
        let a: NotificationActionArgs =
            serde_json::from_str(r#"{"id":"AB","action":"Show"}"#).unwrap();
        assert!(a.include_history);
        assert!(!a.confirm_destructive);
        assert!(serde_json::from_str::<NotificationActionArgs>(r#"{"id":"AB"}"#).is_err());
    }

    #[test]
    fn listing_renders_ids_sources_and_hides_the_default_click() {
        let listing = NotificationListing {
            notifications: vec![
                note(
                    "6B5207BF-3B2F-4025-B7FF-46665C40260F",
                    true,
                    NotificationKind::Single,
                ),
                note("9F7D44F4", false, NotificationKind::CollapsedStack),
            ],
            opened_panel: true,
            warnings: vec!["w1".into()],
        };
        let out = render_notifications(&listing, true);
        assert!(out.starts_with("2 notification(s)"));
        assert!(
            out.contains("id=6B5207BF-3B2F-4025-B7FF-46665C40260F  app=스크립트 편집기  [history]")
        );
        assert!(out.contains("[banner]  [collapsed stack: top item only]"));
        assert!(out.contains("actions: 세부사항 보기, 보기, 닫기"));
        assert!(!out.contains("AXPress"));
        assert!(out.contains("closed it again"));
        assert!(out.contains("warning: w1"));
    }

    #[test]
    fn empty_listing_says_where_it_looked() {
        let empty = NotificationListing {
            notifications: vec![],
            opened_panel: false,
            warnings: vec![],
        };
        assert!(render_notifications(&empty, false).contains("include_history=true"));
        assert!(render_notifications(&empty, true).contains("history panel"));
    }

    #[test]
    fn dismiss_all_reports_partial_progress() {
        let o = DismissAllOutcome {
            dismissed: vec![note("AA", true, NotificationKind::Single)],
            stopped_on: Some((note("BB", true, NotificationKind::Single), "refused".into())),
            remaining: 3,
            warnings: vec![],
        };
        let out = render_dismiss_all(&o);
        assert!(out.contains("dismissed 1 notification(s)"));
        assert!(out.contains("STOPPED on BB"));
        assert!(out.contains("refused"));
        assert!(out.contains("3 notification(s) remain"));
    }
}

#[cfg(test)]
mod clipboard_render_tests {
    use super::*;
    use cua_core::{ClipboardAccess, ClipboardContents, ClipboardRead, ClipboardWrite};

    fn read(contents: ClipboardContents, types: &[&str], access: ClipboardAccess) -> ClipboardRead {
        ClipboardRead {
            contents,
            types: types.iter().map(|t| t.to_string()).collect(),
            change_count: 7,
            access,
        }
    }

    #[test]
    fn no_text_is_a_success_that_lists_the_types() {
        let r = read(
            ClipboardContents::NoText,
            &["public.png", "public.tiff"],
            ClipboardAccess::AlwaysAllow,
        );
        let s = render_clipboard_read(&r, 100);
        assert!(s.contains("text: none"));
        assert!(s.contains("available_types: public.png, public.tiff"));
        assert!(!s.contains("DENY"));
    }

    #[test]
    fn no_text_under_always_deny_says_it_is_not_evidence_of_empty() {
        let r = read(ClipboardContents::NoText, &[], ClipboardAccess::AlwaysDeny);
        let s = render_clipboard_read(&r, 100);
        assert!(s.contains("does not mean the clipboard is empty"));
        assert!(s.contains("(none — the clipboard is empty)"));
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary_and_the_total_reported() {
        let r = read(
            ClipboardContents::Text("안녕하세요 world".to_string()),
            &["public.utf8-plain-text"],
            ClipboardAccess::NotApplicable,
        );
        let s = render_clipboard_read(&r, 3);
        assert!(s.contains("text: 11 chars, showing the first 3"));
        assert!(s.contains("\n안녕하\n"));
    }

    #[test]
    fn a_withheld_read_prints_the_reason_and_never_the_text() {
        let r = read(
            ClipboardContents::Withheld("marked as a secret".to_string()),
            &["org.nspasteboard.ConcealedType", "public.utf8-plain-text"],
            ClipboardAccess::Default,
        );
        let s = render_clipboard_read(&r, 100);
        assert!(s.contains("text: withheld"));
        assert!(s.contains("marked as a secret"));
        assert!(s.contains("org.nspasteboard.ConcealedType"));
    }

    #[test]
    fn an_unverified_write_says_so() {
        let w = ClipboardWrite {
            verb: "clipboard_set",
            change_count: 9,
            chars: 5,
            verified: false,
        };
        let s = render_clipboard_write(&w);
        assert!(s.contains("wrote 5 chars"));
        assert!(s.contains("verified: NO"));
    }
}
