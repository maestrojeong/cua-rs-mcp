//! The public session types: what every [`crate::PlatformAdapter`] takes and
//! returns.
//!
//! Platform-neutral by construction — nothing here names a native handle — so
//! this module compiles on every target. The macOS backend that produces these
//! lives next door in `orchestration` and its children, behind
//! `cfg(target_os = "macos")`; it adds its own inherent methods to some of
//! these types there (`ScrollDir::verb`, `Presence::wants_present`, …), which
//! is why a few of them look method-less here. Moved verbatim from
//! `orchestration.rs` in the platform split; see DESIGN.md §15.

use crate::apps::{self, AppInfo};
use crate::platform::leaf::{CGRect, Limits, Modifiers, MouseButton};

// ── errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    /// NotificationCenter, or ControlCenter's clock that opens its history, is
    /// not reachable through accessibility right now.
    #[error("Notification Center is not reachable: {reason}. Call check_permissions to rule out a missing Accessibility grant")]
    NotificationCenterUnavailable { reason: String },

    /// No notification with that id is in Notification Center's tree.
    #[error("no notification with id `{id}` is in Notification Center{}. Ids come from list_notifications and stay valid until the notification itself goes away (expired from a banner into history, dismissed, or cleared); call list_notifications again", if *searched_history { " (banners and the history panel were both searched)" } else { " on screen (pass include_history=true to also search the history panel)" })]
    NotificationNotFound { id: String, searched_history: bool },

    /// The notification does not offer the action asked for.
    #[error("notification `{id}` has no `{action}` action cua-rs can press: {why}. It advertises: [{available}]")]
    NotificationActionUnavailable {
        id: String,
        action: String,
        available: String,
        why: String,
    },

    #[error(transparent)]
    Resolve(#[from] apps::ResolveError),

    /// A raw AX call failed. macOS only: the payload is `cua-ax`'s own error.
    #[cfg(target_os = "macos")]
    #[error(transparent)]
    Ax(#[from] cua_ax::AxError),

    /// ScreenCaptureKit failed. macOS only, like [`CoreError::Ax`].
    #[cfg(target_os = "macos")]
    #[error(transparent)]
    Capture(#[from] cua_capture::CaptureError),

    /// The capability exists in the tool surface but this platform's backend
    /// cannot provide it — not right now, not with a retry. Answered instead of
    /// a silent no-op, which is the one failure an agent cannot detect.
    /// `feature` names the capability as a tool would (`screenshot`,
    /// `list_notifications`); `why` says what is missing and, when there is
    /// one, what to do instead. Never constructed by the macOS backend.
    #[error("{feature} is not supported on {platform}: {why}")]
    PlatformUnsupported {
        feature: &'static str,
        platform: &'static str,
        why: String,
    },

    /// The element exists and is current, but does not implement what the
    /// call needs — a `set_value` on something with neither editable text nor
    /// a numeric value, a `perform_secondary_action` naming an action it does
    /// not advertise. Raised by backends whose accessibility layer reports
    /// capabilities up front (AT-SPI's interface list), so the call can be
    /// refused before anything is sent; the macOS backend reaches the same
    /// situation through `Ax(Unsupported)`. Never constructed on macOS.
    #[error("cannot {verb} {element}: {why}")]
    ElementUnsupported {
        verb: &'static str,
        element: String,
        why: String,
    },

    /// A platform accessibility call failed for a reason none of the variants
    /// above names. The non-macOS counterpart of [`CoreError::Ax`], which is
    /// typed by `cua-ax` and so cannot be shared: on Linux it carries the
    /// AT-SPI / D-Bus error text verbatim, so bug reports stay useful.
    #[error("accessibility error: {reason}")]
    Accessibility { reason: String },

    /// A safety gate declined to let this action happen. See [`crate::safety`]
    /// for what the gates are and how each one is cleared; the message always
    /// names the way out, because a refusal an agent cannot resolve just
    /// becomes a retry loop.
    #[error(transparent)]
    Refused(#[from] crate::safety::Refused),

    /// The agent acted on an index from a snapshot that has since been replaced.
    ///
    /// Reported loudly and specifically rather than being silently remapped:
    /// index 42 in the previous snapshot is a different element than index 42 in
    /// the current one, so honoring it would click the wrong thing. That is the
    /// one failure mode in this whole system that a retry cannot fix and the
    /// user cannot see.
    #[error("element_index {index} refers to snapshot {given}, but the current snapshot for this app is {current}. Call get_app_state again and use a fresh index")]
    StaleSnapshot {
        index: usize,
        given: u64,
        current: u64,
    },

    #[error("no snapshot for `{app}` yet. Call get_app_state first")]
    NoSnapshot { app: String },

    #[error("the process behind `{app}` changed since this snapshot was taken. Call get_app_state again; cached native element handles cannot cross an app relaunch")]
    ProcessReplaced { app: String },

    #[error("element_index {index} is out of range (the snapshot has {count} elements)")]
    BadIndex { index: usize, count: usize },

    #[error("`{app}` has no accessibility window that cua-rs can drive right now. Verify Accessibility is granted to the process that launched cua-rs, then retry after the app has finished opening. Call check_permissions and list_apps to distinguish a permission or process-discovery problem")]
    NoWindow { app: String },

    /// Distinct from [`CoreError::NoWindow`] on purpose. That variant means AX
    /// answered and said there is nothing; this one means AX did not answer at
    /// all — `kAXErrorCannotComplete` on `AXFocusedWindow`/`AXMainWindow`/
    /// `AXWindows`, twice in a row, `WINDOW_RETRY_DELAY` apart. Measured on
    /// KakaoTalk and Telegram (both Qt on macOS): the call can time out while
    /// `CGWindowListCopyWindowInfo` shows the same window on screen at that
    /// exact moment, so "open a window and retry" is the wrong advice — the
    /// window is already there, its accessibility bridge is just slow to
    /// answer this call. Worth a caller-side retry after a longer pause than
    /// `get_app_state` already gave it, not a different action on the app.
    #[error("`{app}`'s accessibility bridge did not answer in time (kAXErrorCannotComplete on its window attributes, twice). This does not mean the app has no window — apps built on non-native UI toolkits (Qt: KakaoTalk, Telegram) are measured to time out on this call while a window is genuinely on screen. Wait a moment and call get_app_state again rather than trying to open a new window first")]
    WindowQueryTimedOut { app: String },

    /// The requested key has no element-addressed accessibility equivalent.
    #[error("`{key}` has no accessibility equivalent. cua-rs does not synthesize shared HID keyboard input because it would steal keyboard focus. Background-safe alternatives: return/enter and escape on an element that accepts AXConfirm/AXCancel, perform_secondary_action with AXShowMenu for a context menu, or clicking the menu item directly")]
    KeyNoAccessibilityEquivalent { key: String },

    /// The key has an AX verb, but this particular element does not accept it.
    ///
    /// Kept distinct from [`CoreError::KeyNoAccessibilityEquivalent`] so the message cannot
    /// contradict itself by naming the very key it just refused as one that
    /// works.
    #[error("`{key}` maps to the accessibility verb {verb}, but this element does not accept it (it supports {available}). {verb} usually lives on the window or the dialog's default button, not on an inner control — target that instead")]
    KeyVerbUnsupported {
        key: String,
        verb: &'static str,
        available: String,
    },

    /// `key` did not parse as a chord `cua_hid::parse_chord` understands, in
    /// the default (pid-first) keyboard mode. There is no AX-verb fallback to
    /// try here — see [`CoreError::PidKeyUnavailable`] for why.
    #[error("`{key}` is not a chord cua-rs's keyboard parser understands ({reason}). Set CUA_KEY_AX_ONLY=1 to fall back to the old AX-verb-only path (return/escape/up/down only) instead")]
    KeyChordUnparseable { key: String, reason: String },

    /// A chord parsed but the pid-routed delivery itself failed. Distinct from
    /// [`CoreError::PidClickFailed`]'s wording because the escape hatch here is
    /// `CUA_KEY_AX_ONLY`, not `CUA_AX_FIRST` — the two tiers were promoted to
    /// pid-only on separate switches so one can be dialed back without the
    /// other.
    #[error("could not deliver key `{key}` via the pid-routed route: {reason}. cua-rs's default keyboard path does not fall back to accessibility (see CUA_KEY_AX_ONLY to opt into the old AX-verb-only path)")]
    PidKeyUnavailable { key: String, reason: String },

    /// Strict focus mode (`CUA_KEY_STRICT_FOCUS=1`) refused to deliver because
    /// the app names a *different* element as its focused one. Nothing was
    /// sent. Off by default, because a keystroke that has to be dropped
    /// whenever `AXFocused` is unsettable would make `press_key` useless on
    /// Terminal, whose text view is a measured case of exactly that.
    #[error("refused to send `{what}`: this app reports {focused} as its focused element, not {addressed}, and a pid-routed keystroke goes to whatever the process's own first responder is — so it would have landed there instead. Unset CUA_KEY_STRICT_FOCUS to send it anyway, or click the element first")]
    FocusMismatch {
        what: String,
        addressed: String,
        focused: String,
    },

    #[cfg(target_os = "macos")]
    #[error("{original}. This element advertises no AXPress/AXPick/AXConfirm, and the quiet SkyLight pid-routed click is unavailable: {reason}. cua-rs will not fall back to moving the real pointer. perform_secondary_action with AXShowMenu may reach the same control another way")]
    PidClickUnavailable {
        original: cua_ax::AxError,
        reason: String,
    },

    /// The pid-routed click could not be delivered, and — unlike
    /// [`CoreError::PidClickUnavailable`] — accessibility was never tried at
    /// all. This is the default click path's only failure mode: it does not
    /// attempt `AXPress` first and does not retry through it afterward, because
    /// one delivery mechanism per action beats a tier that sometimes silently
    /// no-ops. `CUA_AX_FIRST=1` restores the old AXPress-then-pid order for a
    /// caller that would rather have the fallback back.
    #[error("could not deliver this click via the pid-routed SkyLight route: {reason}. cua-rs's default click path does not fall back to accessibility (see CUA_AX_FIRST to opt into the old AXPress-then-pid order)")]
    PidClickFailed { reason: String },

    /// The live element no longer contains the identifying text captured in
    /// the snapshot. List views can recycle one AX handle for another row.
    #[error("element_index {index} no longer shows what it did when the snapshot was taken (it read {expected}, it now reads {found}). Call get_app_state again and re-pick")]
    TargetChanged {
        index: usize,
        expected: String,
        found: String,
    },

    /// An `element_token` named a role the element at that index no longer has.
    ///
    /// The token carries the role precisely so this is catchable: an index
    /// that used to be a table row and is now a button is drift the caller can
    /// see and fix, where a bare index would have acted on the button.
    #[error("element_token points at index {index}, which was a {expected} when the token was issued and is a {found} now. Call get_app_state again and take a fresh token")]
    TokenRoleMismatch {
        index: usize,
        expected: String,
        found: String,
    },

    /// A coordinate was resolved against a snapshot that an action has already
    /// invalidated.
    #[error("an action has run on `{app}` since its last read, so the coordinates in this call would be resolved against stale geometry. Call get_app_state again, or address the element by element_index, which survives an action because it names the element rather than the place")]
    StalePointGeometry { app: String },

    /// A raw coordinate cited a snapshot that is no longer the current one.
    ///
    /// The coordinate counterpart to [`CoreError::StaleSnapshot`], and needed
    /// for the same reason with a sharper edge. An index at least names an
    /// element, so a mismatch can sometimes be caught by the role or the text
    /// it used to show. A pixel names a *place*, and every place still exists
    /// after the window re-renders — it is simply covering something else now.
    /// There is nothing about a stale point that looks wrong, which is exactly
    /// why it has to be refused on the generation number rather than on
    /// inspection.
    #[error("({x:.0}, {y:.0}) was chosen from snapshot {given} of `{app}`, but its current snapshot is {current}. A coordinate is only meaningful against the window state it was read from, and nothing about a stale point looks wrong — call get_app_state again and re-pick the point from the fresh screenshot")]
    StaleCoordinate {
        app: String,
        given: u64,
        current: u64,
        x: f64,
        y: f64,
    },

    /// A coordinate landed on nothing in the app's current snapshot.
    #[error("no element of `{app}` covers ({x}, {y}) in its current snapshot. Coordinates are resolved against the snapshot's geometry, so the point has to be inside the window get_app_state read; call it again and click the element_index you want")]
    NoElementAtPoint { app: String, x: f32, y: f32 },

    /// An elementless click could not be delivered.
    ///
    /// Deliberately separate from [`CoreError::PidClickUnavailable`], which is
    /// phrased as "the accessibility route was tried and this is why the quiet
    /// fallback also failed". Nothing was tried here: the caller asked for the
    /// pid route by name, so there is no `original` accessibility error to
    /// report and suggesting `perform_secondary_action` instead would be noise.
    #[error("cannot click ({x:.0}, {y:.0}) inside window {wid} of `{app}`: {reason}")]
    WindowClickRefused {
        app: String,
        wid: u32,
        x: f64,
        y: f64,
        reason: String,
    },

    /// A drag could not be delivered.
    ///
    /// One variant for the whole gesture rather than one per gate, because a
    /// drag has two endpoints and the interesting part of the message is always
    /// which of them went wrong and why — which `reason` says — not which of a
    /// dozen enum arms it landed in.
    #[error("cannot drag from {from} to {to} in `{app}`: {reason}")]
    DragRefused {
        app: String,
        from: String,
        to: String,
        reason: String,
    },

    /// A hover or an event-tier scroll could not be delivered.
    #[error("cannot deliver a {what} to `{app}`: {reason}")]
    PointerEventRefused {
        app: String,
        what: &'static str,
        reason: String,
    },

    /// A menu bar path did not lead anywhere pressable.
    #[error("in `{app}`: {reason}")]
    MenuPath { app: String, reason: String },

    /// A pasteboard call failed, or a read raced a write and was discarded.
    #[error("{verb}: {reason}")]
    Clipboard { verb: &'static str, reason: String },
    /// A window tool could not select or change the window it was given.
    ///
    /// One variant with a reason rather than one per failure, for the same
    /// reason as [`CoreError::DragRefused`]: what the caller needs is which
    /// window and what to do instead, and that is all in `reason`.
    #[error("window of `{app}`: {reason}")]
    Window { app: String, reason: String },

    /// `launch_app` or `close_app` reached the system and the system declined.
    #[error("`{app}`: {reason}")]
    Lifecycle { app: String, reason: String },

    /// The native worker thread died. Unrecoverable for the process.
    #[error("the native worker thread is gone")]
    WorkerGone,

    /// The worker remains alive after this error. A bad native call must not
    /// look like a terminated MCP connection to the client.
    #[error("a native macOS operation panicked while handling this request; the cua-rs server is still running, so retry the call once. If it repeats, restart cua-rs and include the server stderr log")]
    NativePanic,
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// What the agent gets back from `get_app_state`.
#[derive(Debug, Clone)]
pub struct AppState {
    pub app: AppInfo,
    pub snapshot_id: u64,
    /// Rendered outline. See [`crate::snapshot::render_tree`].
    pub tree: String,
    pub node_count: usize,
    pub actionable_count: usize,
    pub window_title: Option<String>,
    pub window_frame: Option<CGRect>,
    /// CGWindowID of the window this walk describes, when one was verified.
    ///
    /// Reported because it is the only handle the elementless click tier can be
    /// anchored to: with no element to name, the window is what the caller has
    /// to identify, and guessing it from `list_apps` would defeat the point of
    /// requiring evidence that the caller looked at this window through cua-rs.
    /// `None` when no window could be verified, in which case there is nothing
    /// safe to click blindly either.
    pub window_id: Option<u32>,
    pub screenshot: Option<Screenshot>,
    /// Transient UI this app currently has up above its own content — an open
    /// menu, a context menu, a popover — topmost first.
    ///
    /// Reported because the tree cannot report it. A walk describes one window,
    /// and a pop-up is a *different* window that accessibility does not describe
    /// at all (§10), so without this line an open menu is invisible in a read
    /// that otherwise looks complete. Costs nothing: the window list this comes
    /// from is the same one already enumerated to identify the window to
    /// capture.
    pub popups: Vec<TransientWindow>,
    /// Non-fatal problems worth telling the agent about, e.g. a missing screen
    /// recording grant when the tree itself came back fine.
    pub warnings: Vec<String>,
}

/// A window of the target app that sits above ordinary content: a menu, a
/// context menu, a popover.
///
/// Deliberately carries geometry and nothing else. There is no label, no item
/// list and no text, because there is nothing to read them from — see
/// the macOS backend's `transient_popups`.
#[derive(Debug, Clone, PartialEq)]
pub struct TransientWindow {
    pub id: u32,
    /// `NSWindow` level. 101 is `kCGPopUpMenuWindowLevel`, the level a menu
    /// opened by a control lands on.
    pub layer: i64,
    /// Position and size in global points.
    pub frame: CGRect,
    /// Whether this window was absent the last time cua-rs looked at this app's
    /// windows, and so appeared while the action was running.
    ///
    /// `None` means nobody looked before, which is a different claim from "it
    /// was already there". The comparison point is the enumeration each action
    /// makes to revalidate its target window immediately before posting, or
    /// failing that the last `get_app_state` — so `Some(true)` means "not
    /// present at that moment", not "nothing else could have opened it".
    pub appeared: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Pixels per point. See [`cua_capture::WindowShot::scale`].
    pub scale: f64,
    /// The screen rect these pixels cover. Normally the window's own frame, and
    /// larger than it when a pop-up was open — see
    /// [`cua_capture::WindowShot::frame`].
    pub frame: CGRect,
    /// The requested window's own frame, for comparison with `frame`.
    pub window_frame: CGRect,
}

/// Options for one `get_app_state` call.
#[derive(Debug, Clone, Copy)]
pub struct StateOptions {
    pub limits: Limits,
    pub render: crate::snapshot::RenderOptions,
    /// Capture pixels as well as the tree.
    ///
    /// Worth turning off: an AX-driven agent targets by index, so on follow-up
    /// calls the screenshot is often the single most expensive part of the
    /// response and the least used.
    pub include_screenshot: bool,
    /// Longest screenshot edge in pixels, `0` for native.
    pub max_image_dim: u32,
    /// Walk from this element index in the app's *current* snapshot instead of
    /// from the window.
    ///
    /// The drill-in half of skeleton mode: the summary line names the index, the
    /// caller passes it back, and the next walk spends its whole budget inside
    /// that subtree. Requires an existing snapshot, since that is where the index
    /// comes from.
    pub scope: Option<usize>,
}

/// Whether `press_key` routes through the pid tier
/// (`cua_hid::press_chord_background_pid`) instead of the old, AX-verb-only
/// path. `set_value`/`type_text` are unaffected by this switch: one `AXValue`
/// write replaces a whole string atomically and is addressed at the element,
/// where the same text as keystrokes is a long stream landing on whatever holds
/// focus. This switch only covers discrete key and chord presses, which have no
/// accessibility verb to express them in the first place.
///
/// DESIGN.md §10 gated `press_chord_background_pid` out of the MCP surface
/// entirely: "a keystroke that lands in the wrong process is far worse than a
/// click that does not land". Making it the *only* tier for `press_key` (no
/// AX verb attempted, not even as a fallback) is the reversal of that gate —
/// so `CUA_KEY_AX_ONLY=1` is kept as the way back to the old, purely
/// AX-verb-limited path (`return`/`escape`/`up`/`down` only, nothing else, no
/// synthesized input at all) if the pid tier proves untrustworthy on a given
/// machine or app.
/// Whether a pid-routed keyboard action refuses to deliver when the app names
/// a *different* element as focused ([`FocusState::Mismatched`]).
///
/// Off by default, and the default is the considered one rather than the
/// timid one. `AXFocused` is not settable on every element — Terminal's text
/// view is a measured case — so an app can route keys perfectly well while
/// reporting a focused element that is not the one addressed, and refusing
/// there would break the exact targets §10 wants reached. The fix for the
/// silence was to *report* the mismatch, not to start dropping keystrokes.
///
/// `CUA_KEY_STRICT_FOCUS=1` is for the caller who would rather lose a
/// keystroke than land one in the wrong field of an app the human is also in —
/// the one situation where a pid-routed miss can be seen by a human at all,
/// since the event never leaves the target process. It refuses only on
/// `Mismatched`; `Unverified` still delivers, because "the app said nothing"
/// is not evidence of a miss and treating it as one would refuse almost
/// everything.
/// Whether to deliver a wheel event that has been measured not to scroll.
///
/// Off by default, which is unusual for this file: every other switch here
/// gates a capability that *works*. This one gates a capability that does not.
///
/// The measurement is in DESIGN §11 — a pid-routed `scrollWheel` is delivered
/// and moves nothing, on a native `AXScrollArea` and on Chromium web content, in
/// both units, while a pid-routed `pagedown` keystroke scrolls the same window in
/// the same run. Given that, delivering it is worse than refusing: the caller is
/// told `delivery: pid`, concludes the scroll happened, and reads a stale tree
/// as the new state. A refusal that names `press_key` costs one round trip and
/// gets the caller somewhere.
///
/// The mechanism stays in the tree rather than being deleted, because it is
/// *correctly built* — the failure is in what macOS does with a scroll event
/// that has no AppKit identity, not in the code — and an app that reads the
/// event record directly may yet accept it. `CUA_WHEEL_SCROLL=1` is how the next
/// person re-runs the experiment without reverting a commit.
impl Default for StateOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            render: crate::snapshot::RenderOptions::default(),
            include_screenshot: true,
            max_image_dim: 1400,
            scope: None,
        }
    }
}

/// How the agent addressed an element.
///
/// No longer `Copy`: pinning a token to a role means carrying a `String`. The
/// handful of call sites that passed it by value now clone, which costs one
/// small allocation per action and buys a check that catches an index whose
/// meaning has changed.
#[derive(Debug, Clone)]
pub enum Target {
    /// By snapshot index, optionally pinned to the snapshot it came from and
    /// to the role that index had when the snapshot was taken.
    Index {
        index: usize,
        snapshot_id: Option<u64>,
        /// Checked against the recorded node's role when present. An index
        /// that has come to mean a different *kind* of thing is the cheapest
        /// detectable form of drift, and the one a caller can act on.
        expected_role: Option<String>,
    },
    /// By screen point, in AX global points. Hit-tested to an element, then
    /// acted on through AX — never by moving the pointer.
    Point {
        x: f32,
        y: f32,
        /// The snapshot this point was chosen from, when the caller says.
        ///
        /// Honoured exactly as [`Target::Index`]'s is, and it was an omission
        /// that it once was not: a caller could pass `snapshot_id` alongside
        /// `x`/`y` and have it silently ignored, which is worse than not
        /// offering the field at all — the guard reads as present and is not.
        snapshot_id: Option<u64>,
    },
}

/// The button and modifier keys a pointer action carries.
///
/// One value rather than two parameters on five methods, because the two always
/// travel together and neither means anything without the delivery path the
/// other one uses. Both default to "an ordinary click": left button, nothing
/// held.
#[derive(Debug, Clone, Copy)]
pub struct MouseOptions {
    pub button: MouseButton,
    pub modifiers: Modifiers,
    /// Click count: 1, or 2 for a target that only opens on a double-click.
    /// Ignored by the gestures that have no notion of one — a drag, a hover.
    pub count: u8,
}

impl Default for MouseOptions {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            // `Modifiers` is a re-exported `CGEventFlags`, which has no
            // `Default` of its own, so this cannot be derived.
            modifiers: Modifiers::empty(),
            count: 1,
        }
    }
}

impl MouseOptions {
    /// Parse the two strings an MCP caller supplies, together, so a caller who
    /// misspells either gets one message naming which.
    ///
    /// Both may be empty: that is the default click. The modifier vocabulary is
    /// `cua_hid::parse_modifiers`, which is the same table `press_key` uses, so
    /// `cmd+shift` means the same thing whichever tool it is written on.
    pub fn parse(button: &str, modifiers: &str) -> std::result::Result<Self, String> {
        Ok(Self {
            button: MouseButton::parse(button).map_err(|e| e.to_string())?,
            modifiers: crate::platform::leaf::parse_modifiers(modifiers)
                .map_err(|e| e.to_string())?,
            ..Self::default()
        })
    }

    /// Same options, with a click count. Separate from [`MouseOptions::parse`]
    /// because a drag and a hover have no click count to set and should not
    /// have to name one.
    pub fn with_count(self, count: u8) -> Self {
        Self { count, ..self }
    }

    /// How this reads in a result line: `left`, or `cmd+shift right`.
    /// How the button and modifiers read in a result line: `left`,
    /// `cmd+shift left`.
    pub fn describe(&self) -> String {
        let mods = describe_modifiers(self.modifiers);
        if mods.is_empty() {
            self.button.as_str().to_string()
        } else {
            format!("{mods} {}", self.button.as_str())
        }
    }
}

/// Render a modifier set back into the vocabulary it was parsed from, so a
/// result line quotes something the caller could paste into the next call.
/// `cmd+shift` for a modifier set, in the fixed order cmd, shift, alt, ctrl, fn.
pub fn describe_modifiers(flags: Modifiers) -> String {
    [
        (Modifiers::MaskCommand, "cmd"),
        (Modifiers::MaskShift, "shift"),
        (Modifiers::MaskAlternate, "alt"),
        (Modifiers::MaskControl, "ctrl"),
        (Modifiers::MaskSecondaryFn, "fn"),
    ]
    .into_iter()
    .filter(|(bit, _)| flags.contains(*bit))
    .map(|(_, name)| name)
    .collect::<Vec<_>>()
    .join("+")
}

/// Where a pointer event should land, for the actions that accept either an
/// element or a bare pixel.
///
/// `click` does not offer this and deliberately keeps its elementless form in a
/// separate tool: "the point covers nothing" is the shape of a typo, and a
/// click on a typo is the worst outcome in this project. A drag endpoint and a
/// hover are different. A drag frequently has one end on a real row and the
/// other on empty canvas — a reorder into a gap, a selection rectangle drawn
/// across background — and a hover presses nothing at all, so the same
/// blind-click argument does not apply to either.
#[derive(Debug, Clone)]
pub enum PointerLocation {
    /// Resolved through the snapshot exactly as `click` does.
    Element(Target),
    /// A point in POINTS from the top-left corner of the window the app's most
    /// recent `get_app_state` read — the same coordinate space
    /// `click_in_window` takes, and re-anchored to the window's live origin at
    /// delivery time for the same reason.
    WindowPoint { x: f64, y: f64 },
}

/// A pixel in a window the caller has read, together with the snapshot
/// generation it was read from.
///
/// The four travel as one value because they are one claim: "I looked at this
/// window in this state and chose this pixel." Splitting them across four
/// parameters is how the snapshot half came to be omitted from `click_in_window`
/// in the first place — the window id looked like enough addressing, and it is
/// only enough to name the window, not the state.
#[derive(Debug, Clone, Copy)]
pub struct WindowPixel {
    /// The `window_id` this app's most recent `get_app_state` reported.
    pub window_id: u32,
    /// Horizontal offset from the window's top-left corner, in points.
    pub x: f64,
    /// Vertical offset from the window's top-left corner, in points.
    pub y: f64,
    /// The snapshot the pixel was chosen from, when the caller says. Optional
    /// for the same reason every other staleness guard here is: the common
    /// flow reads and acts in one turn.
    pub snapshot_id: Option<u64>,
}

/// The outcome of an action, in enough detail to be auditable.
#[derive(Debug, Clone)]
pub struct ActionResult {
    /// The AX verb that actually landed, e.g. `AXPress`.
    pub verb: String,
    /// Role and label of what was acted on, so the agent can confirm it hit the
    /// thing it meant to. Cheap insurance against an off-by-one index.
    pub target: String,
    /// True when the target's window changed in a way we could observe.
    pub ui_changed: Observed,
    /// Transient UI the app has up now that the action has run, topmost first.
    ///
    /// In the action's own response, and that is the point. A menu opened by a
    /// click lives in a window of its own that the click's target window knows
    /// nothing about, so a caller that only reads the diff is told the control
    /// did nothing — which is exactly what users reported about KakaoTalk's
    /// hamburger, and exactly what was untrue. Telling them one round trip later
    /// is too late: by then the caller has already concluded the click failed.
    pub popups: Vec<TransientWindow>,
    /// Which mechanism carried the action.
    ///
    /// Present on every result, not just the interesting ones, so an agent never
    /// has to infer it from the absence of a field.
    pub delivery: Delivery,
    /// Where the action landed, in screen points, best-effort — an
    /// element's `AXActivationPoint` or frame centre, or the exact point a
    /// caller passed for a `Target::Point`. `None` when nothing could be
    /// resolved. Crate-private and not part of the tool contract: it exists
    /// only to feed the drawn-cursor overlay (see `Cua::exec_action`), so a wrong or
    /// missing value never surfaces as an error, and it never has to be kept
    /// stable for callers outside this crate.
    // Read only by the macOS drawn-cursor overlay; no other backend has one.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) point: Option<(f64, f64)>,
    /// CGWindowID and owning pid, used only to pin the drawn cursor
    /// immediately above the target window and to let the overlay hide
    /// itself the moment a *different* app becomes frontmost, rather than
    /// trusting window ordering alone to keep it from showing above
    /// whatever the human just switched to.
    // Read only by the macOS drawn-cursor overlay; no other backend has one.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) overlay_target: Option<(u32, libc::pid_t)>,
    /// What the window looked like after the action, when the caller asked.
    pub state: Option<PostActionState>,
    /// Whether the addressed element actually held keyboard focus when the
    /// input was sent.
    ///
    /// `Some` only on the paths where the question means something — the
    /// pid-routed keyboard ones, where the event is addressed to a process and
    /// lands on that process's first responder. An `AXValue` write is
    /// addressed at the element itself and cannot miss, so it reports `None`
    /// rather than a reassuring `verified` it did not earn.
    pub focus: Option<FocusCheck>,
}

/// The target's state immediately after an action, when `return_state` was set.
///
/// This exists because `ui_changed` is a heuristic and a false negative is
/// expensive: it reads as "the control did nothing", which is exactly the wrong
/// conclusion to hand an agent. A menu that opens as its own window, for
/// instance, changes neither the focused element nor the window title, so the
/// heuristic reports `Unchanged` while the app has plainly done something. The
/// only way to be sure is to look — so this makes looking part of the action
/// rather than a second round trip the agent has to remember to make.
impl ActionResult {
    /// A result for a backend outside this crate.
    ///
    /// The two crate-private fields feed the macOS drawn-cursor overlay only,
    /// so they are left empty here; `state` and `focus` are public and can be
    /// filled in afterwards.
    pub fn new(
        verb: impl Into<String>,
        target: String,
        ui_changed: Observed,
        delivery: Delivery,
    ) -> Self {
        Self {
            verb: verb.into(),
            target,
            ui_changed,
            popups: Vec::new(),
            delivery,
            point: None,
            overlay_target: None,
            state: None,
            focus: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PostActionState {
    /// Id of the snapshot taken after the action. Element indices in `diff` or
    /// `tree` belong to *this* snapshot, and it is the one to quote back.
    ///
    /// `None` when the re-read itself failed, which is a different finding from
    /// "nothing changed" and from "nobody looked": the action already happened
    /// and its effect is simply unobserved. A click that closes the only window
    /// gets here — the action succeeded and there is no longer a window to read.
    pub snapshot_id: Option<u64>,
    /// Lines that appeared or vanished versus the pre-action tree. `None` when
    /// there was nothing fair to compare against; `note` then says why.
    pub diff: Option<crate::snapshot::TreeDiff>,
    /// Why no diff is present, when there is none.
    ///
    /// The whole outline is deliberately *not* sent instead. Falling back to it
    /// would spend exactly the tokens this feature exists to save, at the moment
    /// the caller least expects it; the fresh `snapshot_id` and this line are
    /// enough for the caller to decide whether reading the tree is worth it.
    pub note: Option<String>,
    pub node_count: usize,
}

/// What was observed after an action, in three values rather than two.
///
/// `Unknown` is the point of this type. The evidence for "something happened"
/// is a before/after fingerprint of the app's focused window, and some apps
/// publish nothing to fingerprint: KakaoTalk exposes zero `AXWindows` while it
/// is not frontmost, so both reads come back empty and comparing them proves
/// nothing at all. Collapsing that into `false` is a lie an agent acts on — it
/// retries a click that already worked, or reports failure for a command that
/// succeeded. cua-driver's `verify_state` draws the same distinction and is
/// explicit that unknown never implies success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// The fingerprint differed. Something in the app's UI moved.
    Changed,
    /// The fingerprint was readable and identical. Note this is still not
    /// proof of failure: plenty of real actions change nothing visible.
    Unchanged,
    /// There was nothing to compare. Says nothing either way.
    Unknown,
}

impl Observed {
    pub fn as_str(self) -> &'static str {
        match self {
            Observed::Changed => "yes",
            Observed::Unchanged => "no",
            Observed::Unknown => "unknown",
        }
    }
}

/// How an action reached the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Addressed to a specific UI element. Did not move the cursor, change
    /// focus, or activate the app.
    Ax,
    /// Routed to one process's window via the private SkyLight
    /// `SLEventPostToPid` SPI. The cursor never moves and nothing is raised or
    /// activated. This is the only synthesized-input delivery mode.
    Pid,
    /// The same pid-routed SkyLight delivery as [`Delivery::Pid`], addressed to a
    /// point in a window rather than to an element.
    ///
    /// Reported separately because the difference is not the mechanism, it is
    /// what the result can be trusted to mean. Every other delivery mode
    /// resolved an element first, so the result names a thing accessibility
    /// agreed was there. This one names a pixel the caller chose. cua-rs
    /// promises the event reached that pixel of that window and nothing more:
    /// there is no element to inspect afterwards, so a caller that needs to know
    /// whether anything was hit has to look for itself.
    PidNoElement,
    /// A key chord or literal text, routed to the target pid via
    /// `press_chord_background_pid`/`type_text_background_pid` — real
    /// `CGEventKeyboardEvent`s posted per-pid, not through the shared HID
    /// keyboard tap. The keystrokes land wherever the *target process's own*
    /// first responder currently is, which cua-rs cannot itself constrain
    /// beyond best-effort focusing the addressed element first (`AXFocused`
    /// where settable). A caller that must be certain which field received the
    /// text should re-read the element afterward rather than trust the
    /// delivery label alone.
    PidKey,
    /// Real input synthesized on the *shared* X11 input devices through the
    /// XTEST extension (Linux only, and only when the operator opted in with
    /// `CUA_LINUX_XTEST=1`). Unlike every variant above it is not addressed to
    /// an element or a process: pointer events move the human's real pointer,
    /// and key events go to whichever window holds the X keyboard focus, which
    /// the backend sets to the target first. Reported as its own delivery so
    /// that a caller can never mistake it for the macOS guarantee that nothing
    /// shared was touched. Never produced by the macOS backend.
    SharedInput,
    /// A request to the window manager (Linux EWMH client messages: raise,
    /// move/resize, iconify, maximize). No input was synthesized and nothing
    /// was addressed to the app; the window manager decided, and the result's
    /// before/after frames say what it decided. Never produced on macOS, where
    /// window tools go through accessibility and report `ax`.
    WindowManager,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Ax => "ax",
            Delivery::Pid => "pid",
            Delivery::PidNoElement => "pid (no element)",
            Delivery::PidKey => "pid (keyboard)",
            Delivery::SharedInput => "xtest (shared input)",
            Delivery::WindowManager => "wm (window-manager request)",
        }
    }
}

/// How `type_text` should get a string into an element.
///
/// Two mechanisms, and the caller picks — because neither is right for every
/// target and picking silently would hide which one ran.
///
/// [`Mechanism::Ax`] is the default and stays the default: one `AXValue` /
/// `AXSelectedText` write replaces or inserts the whole string atomically and
/// is addressed at the element, so it cannot land anywhere else and needs no
/// focus at all. That is the one operation accessibility expresses *better*
/// than events do, so making events the default would be a downgrade for every
/// ordinary text field.
///
/// [`Mechanism::Keystrokes`] is for the targets that ignore `AXValue`
/// entirely — terminals, canvas editors — where a real key event is the only
/// thing that works. It is opt-in rather than a fallback because its
/// weaknesses are the mirror image: a long stream of per-character events
/// addressed at a *process*, landing on whatever that process's first
/// responder is, multiplying the focus risk by the length of the string. The
/// result carries a [`FocusCheck`] for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mechanism {
    /// A single accessibility write. Default.
    #[default]
    Ax,
    /// Real per-character key events, routed to the target pid.
    Keystrokes,
}

impl Mechanism {
    /// Parse the wire spelling. Unknown values are an error rather than a
    /// silent fall back to the default: a caller who typed `keystroke` meant
    /// keystrokes, and quietly writing `AXValue` into a terminal that ignores
    /// it would look like the tool succeeded and did nothing.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ax" => Ok(Mechanism::Ax),
            "keystrokes" => Ok(Mechanism::Keystrokes),
            other => Err(format!(
                "mechanism must be \"ax\" (one atomic AXValue write, the default) or \"keystrokes\" (real key events routed to the pid, for terminals and canvases that ignore AXValue), got {other:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mechanism::Ax => "ax",
            Mechanism::Keystrokes => "keystrokes",
        }
    }
}

/// Where a pid-routed keystroke was going to land, as far as accessibility can
/// say.
///
/// A pid-routed key event is addressed to a *process*, not to an element: it
/// arrives at whatever that process's own first responder is. cua-rs
/// best-effort-moves accessibility focus onto the addressed element first, but
/// the write is not always accepted and even an accepted one is not proof the
/// AppKit first responder followed. This is the honest answer to "did it?".
///
/// The blast radius, stated precisely: because the post is per-pid, a
/// misdelivered keystroke can only reach another element **of the same
/// process**. It cannot reach the human's foreground app when that is a
/// different process, which the shared HID tap this crate refuses to use
/// would. The risk is real and bounded to "the human and the agent are in the
/// same app at the same time".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusState {
    /// The app names the addressed element as its `AXFocusedUIElement`. The
    /// keystrokes went where they were aimed, as far as accessibility can see.
    Verified,
    /// The app published no focused element to compare against, so there is no
    /// evidence either way. Not a failure: plenty of apps answer nothing here
    /// and still route the keys correctly.
    Unverified,
    /// The app names a *different* element of the same process as focused. The
    /// keystrokes most likely landed there instead. Delivered anyway by
    /// default; `CUA_KEY_STRICT_FOCUS=1` refuses instead.
    Mismatched,
}

impl FocusState {
    pub fn as_str(self) -> &'static str {
        match self {
            FocusState::Verified => "verified",
            FocusState::Unverified => "unverified",
            FocusState::Mismatched => "mismatched",
        }
    }
}

/// [`FocusState`] plus the two inputs it was derived from, so a caller can see
/// *why* it says what it says rather than having to trust the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusCheck {
    pub state: FocusState,
    /// Whether the `AXFocused = true` write on the addressed element was
    /// accepted. `false` is not on its own a reason to withhold the keys:
    /// `AXFocused` is not settable on every element (Terminal's text view is a
    /// measured case) and the element may already hold focus. Until this
    /// change the result was discarded entirely, which is the defect: a failed
    /// focus move was invisible to the caller.
    pub focus_write_accepted: bool,
    /// Why the write was refused, when it was.
    pub focus_write_error: Option<String>,
    /// Role and title of whatever the app *does* report as focused, when that
    /// is not the addressed element. This is the element the keys probably
    /// reached.
    pub focused_instead: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Permissions {
    pub accessibility: bool,
    pub screen_recording: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

/// How much to scroll, and in which of the two vocabularies.
///
/// The distinction is not cosmetic: it selects the tier. Pages are what
/// accessibility can express, so a page request goes through `AXScroll*ByPage`
/// whenever the element advertises it. Points are what accessibility cannot
/// express at all, so a point request is an event or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAmount {
    /// Whole pages. Prefers the AX verb; falls back to a wheel event sized
    /// against the element's own height for an element that advertises none.
    Pages(u32),
    /// Exactly this many points of content. Always a wheel event.
    Points(u32),
}

/// Whether the caller is waiting for text to appear or to go away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Appears,
    Disappears,
}

#[derive(Debug, Clone)]
pub struct FindResult {
    pub snapshot_id: u64,
    /// Matching lines, already rendered.
    pub lines: Vec<String>,
    pub total: usize,
    /// How many nodes were scanned, so "0 matches" can be told apart from "the
    /// tree was empty because the app exposes nothing".
    pub searched: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct WaitOutcome {
    pub satisfied: bool,
    pub polls: u32,
    pub snapshot_id: u64,
    pub elapsed_ms: u64,
}

// ── clipboard ────────────────────────────────────────────────────────────────

/// What `clipboard_get` found in the text slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContents {
    /// Plain text was on the clipboard.
    Text(String),
    /// No plain-text representation: an image, a file list, or nothing at all.
    /// Not an error — the type list says what is there instead.
    NoText,
    /// There may be text, and policy kept it back. The reason quotes the
    /// evidence. The text was never read into this process.
    Withheld(String),
}

/// One clipboard read.
#[derive(Debug, Clone)]
pub struct ClipboardRead {
    pub contents: ClipboardContents,
    /// Every type on the clipboard. Format names only, never contents, so they
    /// are returned in every branch, including a withheld one.
    pub types: Vec<String>,
    pub change_count: isize,
    /// What macOS 26 pasteboard privacy will let this process read. Under
    /// [`ClipboardAccess::AlwaysDeny`] a `NoText` is not evidence of anything.
    pub access: ClipboardAccess,
}

/// One clipboard write or clear.
#[derive(Debug, Clone)]
pub struct ClipboardWrite {
    pub verb: &'static str,
    /// The change count the write produced.
    pub change_count: isize,
    /// Characters written; `0` for a clear.
    pub chars: usize,
    /// Whether a metadata re-read agrees with what was written: same change
    /// count (nobody wrote in between), and the type list is plain text for a
    /// set or empty for a clear. Checked without reading the contents back,
    /// because that read is the one macOS 26 may raise an alert for.
    pub verified: bool,
}

/// What the system will let this process read, as of macOS 26.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardAccess {
    /// Never prompted yet: the first content read may raise a system alert.
    Default,
    /// The user is asked on each programmatic content read.
    Ask,
    AlwaysAllow,
    /// Content reads return nothing, silently. A "no text" answer under this
    /// is not evidence that the clipboard is empty.
    AlwaysDeny,
    /// A value this build does not know.
    Other(isize),
    /// The OS predates the setting (before macOS 26): reads are unrestricted.
    NotApplicable,
    /// A platform with no clipboard privacy layer at all (X11): any client
    /// may read the selection, so reads are unrestricted. Never reported by
    /// the macOS backend.
    NoPrivacyLayer,
}

impl ClipboardAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipboardAccess::Default => "default (macOS may ask the user on the first read)",
            ClipboardAccess::Ask => "ask (macOS asks the user on each read)",
            ClipboardAccess::AlwaysAllow => "always allow",
            ClipboardAccess::AlwaysDeny => {
                "ALWAYS DENY (macOS hides the contents from this process; the clipboard may not be empty)"
            }
            ClipboardAccess::Other(_) => "unrecognized",
            ClipboardAccess::NotApplicable => "unrestricted (this macOS has no pasteboard privacy setting)",
            ClipboardAccess::NoPrivacyLayer => {
                "unrestricted (this platform has no clipboard privacy layer: any client may read it)"
            }
        }
    }
}

// ── windows and app lifecycle ────────────────────────────────────────────────

/// Which window of an app a window tool acts on.
///
/// `window_id` wins when both are given, because it is the stricter of the two:
/// a title can be shared by two windows, an id cannot. Neither means the app's
/// focused window, falling back to its main window and then its frontmost.
#[derive(Debug, Clone, Default)]
pub struct WindowSelector {
    pub window_id: Option<u32>,
    pub title: Option<String>,
}

/// One window, as `list_windows` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSummary {
    /// Position in the app's `AXWindows`, front to back. `None` for a window
    /// the window server reports but accessibility does not list.
    pub ax_index: Option<usize>,
    /// `CGWindowID`, when Screen Recording is granted and the window could be
    /// paired with exactly one window-server window by frame.
    pub window_id: Option<u32>,
    pub title: Option<String>,
    pub subrole: Option<String>,
    pub frame: Option<CGRect>,
    /// Window-server level: `0` normal, `3` floating. `None` without a pairing.
    pub level: Option<i64>,
    pub minimized: bool,
    pub full_screen: bool,
    pub main: bool,
    pub focused: bool,
    pub on_screen: Option<bool>,
    /// Whether the window tools can act on it. `false` means accessibility does
    /// not list it — most often because it is on another Space.
    pub manageable: bool,
}

/// Every window of one app.
#[derive(Debug, Clone)]
pub struct WindowListing {
    pub app: AppInfo,
    pub windows: Vec<WindowSummary>,
    pub warnings: Vec<String>,
}

/// What a window tool should do to the window it selected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowOp {
    Focus,
    Move { x: f64, y: f64 },
    Resize { width: f64, height: f64 },
    Minimize,
    Maximize,
    Restore,
}

impl WindowOp {
    /// The tool name, which is also the verb a refusal quotes.
    pub fn verb(self) -> &'static str {
        match self {
            WindowOp::Focus => "focus_window",
            WindowOp::Move { .. } => "move_window",
            WindowOp::Resize { .. } => "resize_window",
            WindowOp::Minimize => "minimize_window",
            WindowOp::Maximize => "maximize_window",
            WindowOp::Restore => "restore_window",
        }
    }
}

/// The result of a window tool: what was done, and the window before and after.
///
/// Both states are read back from accessibility rather than echoed from the
/// request, because an app is free to clamp a frame, refuse a minimize, or
/// round a size to its own grid — and an agent told "moved to 5000,0" when the
/// app kept it at 1200,0 will place the next window wrongly.
#[derive(Debug, Clone)]
pub struct WindowOutcome {
    pub action: ActionResult,
    pub before: WindowSummary,
    pub after: WindowSummary,
    pub note: Option<String>,
}

/// The result of `launch_app`.
#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub app: AppInfo,
    pub path: String,
    /// The app was already running; nothing was launched or activated.
    pub already_running: bool,
    pub activate_requested: bool,
    /// Whether it is frontmost now, read after the launch reported back.
    pub frontmost: bool,
}

/// The result of `close_app`.
#[derive(Debug, Clone)]
pub struct CloseOutcome {
    pub app: AppInfo,
    /// The process is gone. `false` means the quit was delivered and the app is
    /// still running — usually because it is asking about unsaved changes.
    pub quit: bool,
    pub waited_ms: u64,
}
