//! The single thread that owns every native handle, and the snapshot store.
//!
//! # Why a dedicated thread
//!
//! [`cua_ax::Element`] wraps a `CFRetained<AXUIElement>` and is deliberately
//! not `Send`. That is not an oversight in the binding: AX calls are synchronous
//! IPC into another process's main run loop, and the retained references are
//! only meaningful to the thread that established them. Meanwhile the MCP layer
//! above is `async` and multi-threaded, and `rmcp` will happily poll a tool
//! future on a different worker between two `.await`s.
//!
//! Rather than sprinkle `unsafe impl Send` over FFI handles — which would make
//! the borrow checker stop complaining without making the code correct — all
//! native work is funneled onto one long-lived thread. The async side sends a
//! closure and blocks on a reply channel. Handles never cross a thread boundary,
//! so `Element` can stay honestly `!Send`.
//!
//! It also serializes everything by construction. Two concurrent tool calls
//! cannot interleave a tree walk with an action on the same app, which would
//! otherwise let an agent act on an element that a half-finished snapshot had
//! already invalidated.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use cua_ax::{AxNode, Element, Limits};
use cua_capture::WindowInfo;
use objc2_core_foundation::CGRect;

use cua_hid::{Modifiers, MouseButton};

use crate::apps::{self, AppInfo};
use crate::overlay::Overlay;

use super::types::*;
use crate::snapshot::match_nodes;

#[path = "delivery.rs"]
mod delivery;

use delivery::{
    KeyboardDelivery, KeyboardStrategy, PointerDelivery, PointerStrategy, ScrollDelivery, DELIVERY,
};

// ── errors ───────────────────────────────────────────────────────────────────

// ── snapshot ─────────────────────────────────────────────────────────────────

/// Monotonic snapshot ids, process-wide.
///
/// Global rather than per-app so an id is never ambiguous across apps: an agent
/// juggling two apps cannot accidentally pass app A's snapshot id with app B's
/// element index and have it validate.
static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

/// One tree walk of one window, with the handles it produced.
pub struct Snapshot {
    pub id: u64,
    pub nodes: Vec<AxNode>,
    pub window: Option<WindowInfo>,
    pub taken_at: Instant,
    /// Process incarnation that produced every native handle in `nodes`.
    /// PIDs are recycled, so pid equality alone cannot make those handles safe
    /// after an app exits and relaunches.
    process_key: ProcessKey,
    /// Whether this walk started from an element rather than from the window.
    ///
    /// A scoped snapshot describes a subtree, so diffing a later whole-window
    /// walk against it reports the entire window as new. Recorded so the
    /// post-action diff can decline instead of emitting that noise.
    scoped: bool,
    /// Caps this walk ran under.
    ///
    /// Same reason as `scoped`, one step subtler: a walk capped below the
    /// window's real size describes the same window *partially*, so the diff is
    /// against a tree that was never complete. Measured on KakaoTalk — a
    /// 40-element read followed by a click reported 278 appeared lines, all of
    /// them nodes the first walk had simply not reached.
    limits: Limits,
    /// Whether the walk that produced `nodes` finished, rather than stopping at a
    /// cap or the time budget.
    ///
    /// Same role as `limits`, for the case equal caps cannot catch: two walks can
    /// run under identical caps and still describe different amounts of the same
    /// window, because the time budget depends on how fast the app answers. A
    /// pre-action walk that timed out at 300 nodes against a post-action walk that
    /// reached 500 reports 200 nodes as having appeared, all of them nodes the
    /// first walk simply never asked about.
    complete: bool,
    /// An action has run since this walk, and nothing has re-read the window.
    ///
    /// Only set when the action declined `return_state`, since a re-read
    /// replaces the snapshot outright. `find` needs it: searching a tree from
    /// before the click answers a question about a window that no longer exists,
    /// and it answered "no AXMenu" about a menu that was open on screen. The
    /// snapshot is kept rather than dropped because a run of actions with
    /// `return_state: false` is exactly the flow that still needs its indices.
    acted_on: bool,
    /// Transient UI this app had up when the walk was taken.
    ///
    /// Kept so an action that does not enumerate windows of its own still has
    /// *something* to compare against, and can therefore say a menu appeared
    /// rather than only that one is present. See [`TransientWindow::appeared`]
    /// for what that claim is worth.
    popups: Vec<TransientWindow>,
}

/// How a post-action re-read is rendered.
///
/// Omission notes are turned off because they are prose about the render, not
/// about the app: "12 structural elements omitted" is identical before and
/// after, so it would either be diffed away as noise or, worse, flip when the
/// count changes and read as a real UI change.
/// Offer an open menu as a *possible* cause of a capture failure.
///
/// Observed on KakaoTalk: while an NSMenu was up, `screencapture -l<id>` failed
/// for that app's windows and the same window id captured fine once the menu
/// closed. Worth mentioning, because the bare OS text — "could not create image
/// from window" — reads like a permission or window-identity problem.
///
/// Deliberately hedged, and deliberately narrow. The correlation is not
/// established: the same measurements were taken while ScreenCaptureKit on that
/// machine was in a degraded state, which makes capture fail broadly and would
/// produce the same pairing by coincidence (see DESIGN §2). And a capture can
/// fail for reasons a menu has nothing to do with — a worker timeout, an encode
/// error, a window mid-rebuild — so the note is attached only to the exact
/// window-server refusal it was observed with, never to every error that happens
/// to coincide with an `AXMenu` in the tree.
///
/// No fallback either way. Capturing the window's screen *region* instead does
/// succeed; measured, it returned an entirely unrelated app's window, because a
/// region capture photographs whatever is actually in front. That is a wrong
/// answer wearing a right answer's clothes, and it discloses a window the caller
/// never asked about. A named failure is worth more than either.
fn capture_failure_warning(err: &str, nodes: &[AxNode]) -> String {
    const WINDOW_SERVER_REFUSAL: &str = "could not create image from window";
    if err.contains(WINDOW_SERVER_REFUSAL) && nodes.iter().any(|n| n.role == "AXMenu") {
        return format!(
            "{err}. This app has a menu open, which may be why: the same window has been seen \
             to capture once its menu closed. The tree above is current either way; if you need \
             pixels, dismiss the menu (press_key `escape` on the AXMenu) and read again"
        );
    }
    err.to_string()
}

/// Whether a stored snapshot can be the `before` side of a post-action diff.
///
/// Every refusal fails the same way: the stored tree is not a complete
/// description of the window the re-read will walk, or it does not describe the
/// same moment, so subtracting them reports as "appeared" a pile of nodes that
/// were there all along and the few lines that are the actual answer fall past
/// the output cutoff. A stated reason is worth more than a wrong diff.
fn diff_basis(snap: &Snapshot) -> std::result::Result<(), &'static str> {
    if snap.scoped {
        return Err(
            "the previous snapshot was scoped to one subtree, which a whole-window \
                    re-read cannot be subtracted from",
        );
    }
    if snap.limits != post_action_limits() {
        return Err(
            "the previous snapshot was walked under different caps, so it never described \
                    the whole window and the difference would be mostly nodes it did not reach",
        );
    }
    if !snap.complete {
        return Err(
            "the previous snapshot's walk did not finish, so nodes it never reached would \
                    read as newly appeared",
        );
    }
    if snap.acted_on {
        return Err(
            "an action has already run against this snapshot without re-reading, so a diff \
                    would attribute that action's changes to this one",
        );
    }
    Ok(())
}

/// Caps a post-action re-read walks under.
///
/// Named rather than inlined so `rendered_current_tree` can compare a stored
/// snapshot against the exact budget the re-read will use, instead of assuming
/// the two happen to agree.
fn post_action_limits() -> Limits {
    StateOptions::default().limits
}

fn post_action_render() -> crate::snapshot::RenderOptions {
    crate::snapshot::RenderOptions {
        note_omissions: false,
        ..crate::snapshot::RenderOptions::default()
    }
}

/// Legacy switch for `click`: when set, restores the *original* tier order
/// (`AXPress` first, pid only when no AX verb exists at all, with a retry
/// through AX if the pid tier then fails). The default (`false`) is pid-only
/// with no AX attempt in either direction.
///
/// Accessibility is how cua-rs decides *where* to click; it was never how the
/// click is delivered, and it cannot express a click count at all — `AXPress` has
/// no notion of one, so a double-click was already pid-only. Making every click
/// take the same route removes the case analysis rather than adding a mechanism.
///
/// The retry through AX after a pid failure is what this default drops, and it
/// was never free: it reintroduces exactly the app-specific quirks that motivated
/// the pid tier — an element that advertises `AXPress` but silently ignores it, a
/// control whose action fires while its visual state lags, a stale handle
/// recycled onto other content that would still accept a press.
///
/// `CUA_AX_FIRST=1` is kept only as a bisecting tool if the pid tier proves
/// untrustworthy on a given machine or app — it is not a supported "best of
/// both" mode, since mixing the two tiers is the thing this default avoids.
/// Read one of this crate's opt-in switches.
///
/// `1` and `true` (in any case) turn a switch on; an unset variable, an empty
/// one, and anything else leave it off. Shared so the switches cannot drift
/// apart in what they accept, and so the parsing is testable without a
/// process-wide `set_var` — which `cargo test` would race across threads.
fn env_flag(name: &str) -> bool {
    flag_is_on(std::env::var(name).ok().as_deref())
}

pub(crate) fn flag_is_on(value: Option<&str>) -> bool {
    value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// A pointer location resolved against a live window.
struct PointerAim {
    /// Screen point, in points.
    point: (f64, f64),
    /// The same point relative to the live window's origin.
    window_local: (f64, f64),
    /// What to name in the result.
    desc: String,
    /// Whether accessibility agreed something is there. False for a raw pixel,
    /// which is what makes the whole action report `pid (no element)`.
    from_element: bool,
}

/// What cua-rs knew about an app immediately *before* an action, so the settle
/// afterwards has something to subtract.
///
/// The pop-up half is an `Option` on purpose. Enumerating windows costs p50
/// ~28 ms with a couple of hundred of them live, and the budget for this feature
/// is one new enumeration per action — the one after. So the before-set is only
/// ever taken from a list the action was going to fetch anyway, and when there
/// is no such list the honest answer is that nobody looked.
#[derive(Debug, Clone, Default)]
struct Watch {
    fingerprint: Option<String>,
    popups: Option<Vec<u32>>,
}

impl Watch {
    /// Record the pop-ups visible in a window list the caller already had.
    fn with_windows(mut self, windows: &[WindowInfo], pid: libc::pid_t) -> Self {
        self.popups = Some(popup_ids(windows, pid));
        self
    }
}

/// What the settle after an action observed.
struct Settled {
    changed: Observed,
    popups: Vec<TransientWindow>,
}

impl ActionResult {
    /// An action that went through the accessibility API, plus the screen point
    /// the overlay should point at (`None` when nothing could be resolved).
    fn ax_at(
        verb: impl Into<String>,
        target: String,
        settled: Settled,
        point: Option<(f64, f64)>,
    ) -> Self {
        Self {
            verb: verb.into(),
            target,
            ui_changed: settled.changed,
            popups: settled.popups,
            delivery: Delivery::Ax,
            point,
            overlay_target: None,
            state: None,
            focus: None,
        }
    }

    fn with_overlay_target(mut self, target: Option<(u32, libc::pid_t)>) -> Self {
        self.overlay_target = target;
        self
    }
}

/// Work out whether this app's front window can safely be clicked in order to
/// make it key, and where.
///
/// An `ApplicationActivated` notice has to be paired with a click on the window's
/// own `AXActivationPoint` for AppKit to treat the window as key, rather than
/// merely telling the application it is active. That means synthesizing a real
/// click at a point the app chose, so it is gated on two checks:
///
/// 1. the activation point comes from the AX window that corresponds to `live` —
///    the very window whose number the click will carry — and not from whichever
///    window accessibility happens to consider focused;
/// 2. the point lies inside `live`'s own frame, so the window-local coordinate
///    the event carries cannot be negative or off the end of the window;
/// 3. the window publishes an activation point at all — a guessed title-bar point
///    would be exactly the kind of coordinate that lands on a close button;
/// 4. a live system-wide hit test at that point resolves to *this* pid and to a
///    window rather than a control. Measured on KakaoTalk, the published point
///    sits about six pixels from the close button, so "the app said so" is not on
///    its own enough of a reason to click there.
///
/// Check 1 exists because it was missing: the window was chosen by
/// `AXFocusedWindow` independently of the window being clicked, so in a
/// multi-window app — a chat app with a list window and several conversation
/// windows, which is the measured case — the assist could take window A's
/// activation point, localize it against window B's origin, and stamp B's window
/// number onto a real click aimed at a point inside A.
///
/// `None` means skip the assist and send the bare notice. That is what cua-rs did
/// before the assist existed, so it is the safe direction to fail in — but it is
/// not free: a control that only arms itself when its window is key will keep
/// refusing the click that follows.
fn window_focus_assist(pid: libc::pid_t, live: &WindowInfo) -> Option<cua_hid::ActivationAssist> {
    // Correspondence by frame, the same public-API route `best_window_match` uses
    // in the other direction. `_AXUIElementGetWindow` would answer directly and is
    // private; see §6.
    let app_el = Element::for_pid(pid);
    let window_el = app_el
        .elements(cua_ax::attr::WINDOWS)
        .into_iter()
        .filter_map(|w| {
            let frame = w.frame()?;
            let distance = frame_distance(&frame, &live.frame);
            (distance <= AX_WINDOW_MATCH_TOLERANCE).then_some((distance, w))
        })
        .min_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, w)| w)?;

    let point = window_el.activation_point()?;
    if !frame_contains(&live.frame, point.x, point.y) {
        return None;
    }

    // Ask the window server who owns that pixel right now, not who owned it when
    // the snapshot was taken.
    let owner = Element::system_wide()
        .element_at(point.x as f32, point.y as f32)
        .ok()?;
    if owner.pid().ok()? != pid {
        return None;
    }
    if owner.role().as_deref() != Some("AXWindow") {
        return None;
    }

    Some(cua_hid::ActivationAssist {
        window_origin: (live.frame.origin.x, live.frame.origin.y),
        activation_point: (point.x, point.y),
    })
}

/// How far an AX window frame may sit from a window-server frame and still be
/// considered the same window.
///
/// Not zero: the two are reported by different subsystems and a point of
/// disagreement is normal. Small enough that two genuinely different windows
/// cannot both match, unless an app stacks windows within two points of each
/// other — in which case the assist declines rather than guessing.
const AX_WINDOW_MATCH_TOLERANCE: f64 = 2.0;

/// Whether a screen point falls inside a frame, half-open on the far edges so
/// adjacent frames cannot both claim it.
fn frame_contains(frame: &CGRect, x: f64, y: f64) -> bool {
    x >= frame.origin.x
        && y >= frame.origin.y
        && x < frame.origin.x + frame.size.width
        && y < frame.origin.y + frame.size.height
}

/// Pull an aim point back into the part of an element the window actually shows.
///
/// Returns the point unchanged when it is already inside the window. Otherwise
/// the centre of the element's visible region — the intersection of the
/// element's frame with the window's — and, when those do not overlap at all,
/// the point unchanged so the caller gets the honest "outside the window"
/// refusal instead of a silently invented coordinate.
///
/// This exists because a scrollable element's frame is not its viewport: an
/// `AXWebArea`'s frame is the whole document, and a long list's frame covers
/// every row, so the centre of either can be far outside the window showing it.
fn clamp_into_window(
    element_frame: Option<CGRect>,
    window: &cua_capture::WindowInfo,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let w = &window.frame;
    if frame_contains(w, x, y) {
        return (x, y);
    }
    let Some(e) = element_frame else {
        return (x, y);
    };
    let left = e.origin.x.max(w.origin.x);
    let top = e.origin.y.max(w.origin.y);
    let right = (e.origin.x + e.size.width).min(w.origin.x + w.size.width);
    let bottom = (e.origin.y + e.size.height).min(w.origin.y + w.size.height);
    if right <= left || bottom <= top {
        return (x, y);
    }
    let clamped = ((left + right) / 2.0, (top + bottom) / 2.0);
    tracing::debug!(
        "aim ({x:.0}, {y:.0}) is outside window {}; using the centre of the element's visible \
         region ({:.0}, {:.0}) instead",
        window.id,
        clamped.0,
        clamped.1
    );
    clamped
}

/// An element's best on-screen point to aim an event at.
///
/// `AXActivationPoint` first, because it is the app's own answer and is not
/// always the geometric centre — a wide list row, or a control with a large
/// transparent hit area, puts it somewhere better than the middle. The frame
/// centre otherwise. `None` when the element publishes neither.
///
/// # An activation point outside its own frame is not an activation point
///
/// Chromium publishes `AXActivationPoint = (0, 982)` for *every* element in the
/// window — measured on Chrome, where a button whose frame is `15,239 194x34`
/// reports that same corner point as its activation point, as does the web area
/// containing it. Taken at face value it aims every pid-routed click, hover,
/// drag and wheel event at one corner of the display, which is nowhere near the
/// control and, on this machine, not even inside the browser's content. Nothing
/// errors: the event is delivered, to the wrong place, and the caller is told
/// `delivery: pid`.
///
/// The rule that catches it needs no app-specific knowledge. A point that
/// activates an element has to be *on* that element, so an activation point
/// outside the element's own frame is self-contradictory and gets discarded in
/// favour of the frame centre. An element with no frame to check against keeps
/// the benefit of the doubt, because then there is nothing better to use.
fn element_point(el: &Element) -> Option<(f64, f64)> {
    let frame = el.frame();
    if let Some(p) = el.activation_point() {
        match &frame {
            Some(f) if !frame_contains(f, p.x, p.y) => {
                tracing::debug!(
                    "ignoring AXActivationPoint ({:.0}, {:.0}): outside the element's own frame \
                     ({:.0},{:.0} {:.0}x{:.0})",
                    p.x,
                    p.y,
                    f.origin.x,
                    f.origin.y,
                    f.size.width,
                    f.size.height
                );
            }
            _ => return Some((p.x, p.y)),
        }
    }
    frame.map(|f| {
        (
            f.origin.x + f.size.width / 2.0,
            f.origin.y + f.size.height / 2.0,
        )
    })
}

/// One reading of an app's focus state: the focused element itself, plus the
/// cheap string [`Inner::window_fingerprint`] compares. See
/// [`Inner::focus_probe`].
struct FocusProbe {
    focused: Option<Element>,
    fingerprint: Option<String>,
}

/// A short human-readable name for a live element, for error text and for
/// naming the element that holds focus instead of the addressed one.
/// The default button of the nearest window-like ancestor that publishes one.
///
/// Walks up from the element itself, asking each window-like ancestor for
/// `AXDefaultButton` and returning the first answer. Both steps matter: a sheet
/// publishes its own default button, and asking the element's parent window
/// first would return the window's instead of the sheet's — the wrong control,
/// in exactly the case a sheet is up.
///
/// `None` when nothing up the chain publishes one, which is the ordinary case
/// outside a dialog and is why the caller only asks inside a decision context.
fn default_button_of_ancestor(el: &Element) -> cua_ax::Result<Option<Element>> {
    /// Roles that can own a default button. `AXSheet` before `AXWindow` is not
    /// an ordering here — the walk is bottom-up, which gets the same result.
    const WINDOW_LIKE: &[&str] = &["AXSheet", "AXDialog", "AXWindow", "AXPopover"];
    /// A window is a handful of levels above a button even in a deep toolkit;
    /// the cap only exists so a malformed parent chain cannot spin.
    const MAX_STEPS: usize = 32;

    let mut current = Some(el.clone());
    for _ in 0..MAX_STEPS {
        let Some(node) = current else {
            return Ok(None);
        };
        let role = node.role().unwrap_or_default();
        if WINDOW_LIKE.iter().any(|w| *w == role) {
            if let Some(button) = node.element_checked("AXDefaultButton")? {
                return Ok(Some(button));
            }
        }
        current = node.element_checked(cua_ax::attr::PARENT)?;
    }
    Ok(None)
}

fn describe_element(el: &Element) -> String {
    let role = el.role().unwrap_or_else(|| "element".into());
    match el
        .string(cua_ax::attr::TITLE)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            el.string(cua_ax::attr::DESCRIPTION)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            el.string(cua_ax::attr::PLACEHOLDER)
                .filter(|s| !s.is_empty())
        }) {
        Some(label) => format!("{role} {label:?}"),
        None => role,
    }
}

/// Classify a focus check from its two independent inputs.
///
/// `addressed_is_focused` is `None` when the app published no
/// `AXFocusedUIElement` at all — a different answer from "it published a
/// different one", and deliberately not collapsed into it.
///
/// The `AXFocused` write outcome deliberately does *not* enter the verdict.
/// A refused write on an element that already holds focus is still verified
/// delivery, and an accepted write is not evidence the AppKit first responder
/// moved — only the read-back is. The write is reported alongside, as
/// context, not as the answer.
fn classify_focus(addressed_is_focused: Option<bool>) -> FocusState {
    match addressed_is_focused {
        Some(true) => FocusState::Verified,
        Some(false) => FocusState::Mismatched,
        None => FocusState::Unverified,
    }
}

/// The outcome of attempting the pid tier of a click, split so the caller can
/// tell "try accessibility instead" apart from "stop — do not press anything
/// else". See [`Inner::pid_click_result`].
enum PidFailure {
    /// Safe to retry through accessibility: the pid tier could not run, but
    /// nothing was pressed and the identified element has not been shown to
    /// have changed.
    Retryable(String),
    /// Must not be retried through any tier: pressing anything now would act
    /// on an element the caller no longer means.
    Fatal(CoreError),
}

fn live_tokens(el: &Element) -> HashSet<String> {
    fn walk(el: &Element, depth: usize, out: &mut HashSet<String>) {
        if depth == 0 || out.len() >= 16 {
            return;
        }
        for child in el.elements(cua_ax::attr::CHILDREN) {
            push_token(child.string(cua_ax::attr::TITLE), out);
            push_token(child.string(cua_ax::attr::VALUE), out);
            push_token(child.string(cua_ax::attr::DESCRIPTION), out);
            walk(&child, depth - 1, out);
        }
    }

    let mut out = HashSet::new();
    push_token(el.string(cua_ax::attr::TITLE), &mut out);
    push_token(el.string(cua_ax::attr::VALUE), &mut out);
    // Whatever the snapshot would have called this element, resolved the same
    // way the walk resolves it. Without this the guard compares two different
    // questions and refuses a target that has not changed at all: measured on
    // TextEdit, whose document view publishes an empty `AXTitle` and is
    // labelled "First Text View" further down the chain, so the snapshot held
    // a token the live read could never produce and every click on it was
    // rejected as stale. The child walk below already read `AXDescription`;
    // the element itself read only title and value.
    push_token(el.label(), &mut out);
    walk(el, 3, &mut out);
    out
}

fn snapshot_tokens(nodes: &[AxNode], index: usize) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut frontier: HashSet<usize> = HashSet::from([index]);
    if let Some(node) = nodes.get(index) {
        push_token(node.label.clone(), &mut out);
        push_token(node.value.clone(), &mut out);
    }
    for _ in 0..3 {
        let mut next = HashSet::new();
        for (i, node) in nodes.iter().enumerate() {
            if node.parent.is_some_and(|p| frontier.contains(&p)) {
                push_token(node.label.clone(), &mut out);
                push_token(node.value.clone(), &mut out);
                next.insert(i);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

fn push_token(text: Option<String>, out: &mut HashSet<String>) {
    if let Some(t) = text {
        let t = t.trim();
        if !t.is_empty() && !t.starts_with("_NS:") && out.len() < 16 {
            out.insert(t.to_string());
        }
    }
}

fn tokens_still_present(expected: &HashSet<String>, found: &HashSet<String>) -> bool {
    expected.iter().all(|t| found.contains(t))
}

fn sorted(set: &HashSet<String>) -> Vec<&String> {
    let mut values: Vec<&String> = set.iter().collect();
    values.sort();
    values
}

#[path = "worker.rs"]
mod worker;

pub use worker::{Cua, Op};
use worker::{Inner, ProcessKey};

impl ScrollDir {
    fn verb(self) -> &'static str {
        match self {
            ScrollDir::Up => cua_ax::action::SCROLL_UP_BY_PAGE,
            ScrollDir::Down => cua_ax::action::SCROLL_DOWN_BY_PAGE,
            ScrollDir::Left => cua_ax::action::SCROLL_LEFT_BY_PAGE,
            ScrollDir::Right => cua_ax::action::SCROLL_RIGHT_BY_PAGE,
        }
    }

    /// The `(vertical, horizontal)` wheel delta that moves the view this way by
    /// `amount` units.
    ///
    /// `CGEventCreateScrollWheelEvent2` counts a positive vertical delta as
    /// scrolling *up* — the view moves toward the start of the document, which
    /// is what `AXScrollUpByPage` also means — and a positive horizontal delta
    /// as scrolling left. Down and right are therefore negations, and getting
    /// that backwards is the single easiest mistake here, hence the unit test.
    fn wheel_delta(self, amount: i32) -> (i32, i32) {
        match self {
            ScrollDir::Up => (amount, 0),
            ScrollDir::Down => (-amount, 0),
            ScrollDir::Left => (0, amount),
            ScrollDir::Right => (0, -amount),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ScrollDir::Up => "up",
            ScrollDir::Down => "down",
            ScrollDir::Left => "left",
            ScrollDir::Right => "right",
        }
    }
}

/// Everything the wheel tier needs beyond the element it is aimed at.
///
/// One struct rather than four parameters because they are one request, and
/// because `advertises` and `asked` only mean anything together with the
/// direction and amount they qualify.
struct WheelRequest {
    dir: ScrollDir,
    amount: ScrollAmount,
    /// Whether the element advertised an accessibility scroll action. Only used
    /// to word the refusal when there is no point to aim at.
    advertises: bool,
    /// A coordinate the caller named, in screen points. Honoured over the
    /// element's own point: `resolve` used it only to find *which* element
    /// covers it, and a scroll container often covers its whole extent.
    asked: Option<(f64, f64)>,
}

/// Which mechanism a scroll used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollTier {
    /// `AXScroll*ByPage` on the element.
    Ax,
    /// A pid-routed `scrollWheel` event at the element's point.
    Wheel,
}

/// Choose the scroll tier. Pure, so the policy is testable without a grant.
///
/// The AX verb wins whenever it is available and the caller asked in pages,
/// because it is the better answer where it exists: the app decides what a page
/// of *its* content is, it needs no coordinate, and it cannot be swallowed by a
/// subview that happens to sit under the point. The wheel tier exists for the
/// case that is otherwise a dead end — an Electron list, a canvas, a web area —
/// where the element advertises no scroll action at all and there is nothing
/// for the AX path to call.
fn scroll_tier(amount: ScrollAmount, advertises_ax_scroll: bool) -> ScrollTier {
    match amount {
        ScrollAmount::Points(_) => ScrollTier::Wheel,
        ScrollAmount::Pages(_) if advertises_ax_scroll => ScrollTier::Ax,
        ScrollAmount::Pages(_) => ScrollTier::Wheel,
    }
}

/// How many points of content one "page" is worth on the wheel tier.
///
/// Derived from the element's own height rather than from a constant, because a
/// page is a property of the thing being scrolled: a page of a full-height
/// message list and a page of a 120-point sidebar are not the same distance,
/// and a constant would badly overshoot one and undershoot the other. The 0.9
/// keeps roughly a line of overlap across the boundary, which is what every
/// real page-down does so the reader does not lose their place.
///
/// The clamp bounds both ends of the guess. An element that reports no frame,
/// or a degenerate one, still has to scroll by *something* usable, and no
/// single wheel event should be able to fling a list by a screen height it
/// never had.
fn page_points(element_height: Option<f64>) -> i32 {
    const FALLBACK: f64 = 400.0;
    const MIN: f64 = 60.0;
    const MAX: f64 = 4000.0;
    let height = element_height.filter(|h| h.is_finite() && *h > 0.0);
    (height.unwrap_or(FALLBACK) * 0.9).clamp(MIN, MAX) as i32
}

// ── worker-side implementation ───────────────────────────────────────────────

#[path = "actions/clipboard.rs"]
mod clipboard_actions;
#[path = "actions/keyboard.rs"]
mod keyboard_actions;
#[path = "actions/menu.rs"]
mod menu_actions;
#[path = "actions/notifications.rs"]
mod notification_actions;
#[path = "observe.rs"]
mod observe;
#[path = "actions/pointer.rs"]
mod pointer_actions;
#[path = "actions/scroll.rs"]
mod scroll_actions;
#[path = "targeting.rs"]
mod targeting;
#[path = "actions/text.rs"]
mod text_actions;
#[path = "actions/window.rs"]
mod window_actions;

/// Window ids of the transient UI one process currently has up.
fn popup_ids(windows: &[WindowInfo], pid: libc::pid_t) -> Vec<u32> {
    windows
        .iter()
        .filter(|w| w.pid == pid && w.is_transient_popup())
        .map(|w| w.id)
        .collect()
}

/// The transient UI one process currently has up, topmost first.
///
/// Ordering is the whole reason this is a function and not a filter written
/// inline. A caller looking at a stack of menus — a submenu over its parent —
/// has to know which one is in front, because that is the one a coordinate will
/// reach; so they are sorted by level and then by window number, newest first,
/// which is the order the window server stacks them in. The first entry is the
/// one on top.
///
/// `before` is the set of ids seen the last time anyone looked, used only to
/// fill in [`TransientWindow::appeared`].
fn transient_popups(
    windows: &[WindowInfo],
    pid: libc::pid_t,
    before: Option<&[u32]>,
) -> Vec<TransientWindow> {
    let mut popups: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| w.pid == pid && w.is_transient_popup())
        .collect();
    popups.sort_by(|a, b| b.layer.cmp(&a.layer).then_with(|| b.id.cmp(&a.id)));
    popups
        .into_iter()
        .map(|w| TransientWindow {
            id: w.id,
            layer: w.layer,
            frame: w.frame,
            appeared: before.map(|ids| !ids.contains(&w.id)),
        })
        .collect()
}

/// The AX verb that expresses a key, when one exists.
///
/// This list is short because AX genuinely has almost nothing here: it models
/// *intents* (confirm, cancel, increment) rather than keys. Return and Escape map
/// cleanly because "accept" and "dismiss" are intents; the arrows map only on
/// elements that expose stepper semantics, and everything else — every modifier
/// chord, every letter — has no representation at all.
fn ax_verb_for_key(key: &str) -> Option<&'static str> {
    match key.trim().to_lowercase().as_str() {
        "return" | "enter" => Some(cua_ax::action::CONFIRM),
        "escape" | "esc" => Some(cua_ax::action::CANCEL),
        "up" => Some(cua_ax::action::INCREMENT),
        "down" => Some(cua_ax::action::DECREMENT),
        _ => None,
    }
}

// ── find / wait ──────────────────────────────────────────────────────────────

/// Minimum gap between `wait_for` polls.
///
/// Each poll is a full tree walk plus AX IPC, so polling tighter than this would
/// spend more of the target app's main-thread time answering us than letting it
/// make the progress we are waiting for.
const POLL_INTERVAL_MS: u64 = 250;

impl Presence {
    fn wants_present(self) -> bool {
        matches!(self, Presence::Appears)
    }
}

fn elapsed_ms_until(deadline: Instant, timeout_ms: u64) -> u64 {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64;
    timeout_ms.saturating_sub(remaining)
}

/// Pick the SCK window that corresponds to an AX window.
///
/// Frames are compared with a tolerance because AX reports points while SCK's
/// numbers can be a hair off after a resize, and because a window that is
/// mid-animation will disagree by a pixel or two. Among same-pid candidates the
/// closest origin wins; with no AX frame to compare against, the largest
/// plausible on-screen window is the best available guess.
fn best_window_match(
    windows: &[WindowInfo],
    pid: libc::pid_t,
    ax_frame: Option<CGRect>,
) -> Option<WindowInfo> {
    // `is_plausible_target`, deliberately, and never the wider
    // `is_addressable_target`: this chooses the window a snapshot is *of*, and a
    // pop-up must not be choosable here. Matching a menu to the AX window's
    // frame would stamp the menu's number onto clicks meant for content, which
    // is the failure §6 records. A caller reaches a pop-up by naming it.
    let mut candidates: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| w.pid == pid && w.is_plausible_target())
        .collect();
    if candidates.is_empty() {
        return None;
    }

    match ax_frame {
        Some(f) => {
            candidates.sort_by(|a, b| {
                let da = frame_distance(&a.frame, &f);
                let db = frame_distance(&b.frame, &f);
                da.partial_cmp(&db)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // Terminal tabs and similar app-managed windows can share
                    // an identical frame. Prefer the one the window server says
                    // is actually on screen instead of an inactive sibling
                    // that a window-id capture cannot render.
                    .then_with(|| b.on_screen.cmp(&a.on_screen))
            });
            candidates.first().map(|w| (*w).clone())
        }
        None => {
            // No AX frame means no evidence tying any window to what
            // accessibility is showing, and "largest wins" is a guess. Restrict
            // the guess to level 0: level 3 is shared by ordinary floating
            // windows and torn-off menus, and a menu picked here would have its
            // window number stamped onto clicks meant for content.
            candidates.retain(|w| w.layer == 0);
            candidates.sort_by(|a, b| {
                let area = |w: &WindowInfo| w.frame.size.width * w.frame.size.height;
                (area(b), b.on_screen)
                    .partial_cmp(&(area(a), a.on_screen))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            candidates.first().map(|w| (*w).clone())
        }
    }
}

fn frame_distance(a: &CGRect, b: &CGRect) -> f64 {
    (a.origin.x - b.origin.x).abs()
        + (a.origin.y - b.origin.y).abs()
        + (a.size.width - b.size.width).abs()
        + (a.size.height - b.size.height).abs()
}

/// Revalidate the snapshot window at the last possible point before private
/// input delivery.
///
/// Matching by id *and* pid rejects both a closed window and a recycled
/// CGWindowID. Returning the live frame fixes window-local coordinates after a
/// move or resize. Finally, the target point must still be inside that window;
/// otherwise an AX handle and a captured window have drifted apart and the only
/// safe recovery is a fresh snapshot.
fn current_window_for_pid_click(
    windows: &[WindowInfo],
    snapshot: &WindowInfo,
    pid: libc::pid_t,
    x: f64,
    y: f64,
) -> std::result::Result<WindowInfo, String> {
    let live = live_window_for_pid_click(windows, snapshot.id, pid)?;
    screen_point_inside(&live, x, y).map_err(|frame| {
        format!(
            "target point ({x:.0}, {y:.0}) is outside the current frame of window {} ({frame}); the AX element and window snapshot drifted apart. Call get_app_state again",
            live.id
        )
    })?;
    Ok(live)
}

/// Find the one live window a pid-routed event may be stamped with.
///
/// Split out of [`current_window_for_pid_click`] because the elementless path
/// needs the live *frame* before it has a screen point at all: its coordinates
/// arrive window-local, so the origin this returns is what turns them into a
/// point to check. The identity rules are the same either way — matching by id
/// *and* pid rejects both a closed window and a recycled CGWindowID, and
/// `is_addressable_target` keeps the event off the desktop, the menu bar,
/// cua-rs's own overlay and anything too small to hold a control.
///
/// That predicate is the wide one on purpose. It used to be
/// `is_plausible_target`, which caps at level 3, and the consequence was that a
/// pop-up menu's window number could never be stamped on an event — so the one
/// kind of UI accessibility cannot describe was also the one kind cua-rs could
/// not click. Widening it here and nowhere else keeps a menu addressable
/// without making it selectable as a snapshot's window.
fn live_window_for_pid_click(
    windows: &[WindowInfo],
    wid: u32,
    pid: libc::pid_t,
) -> std::result::Result<WindowInfo, String> {
    windows
        .iter()
        .find(|w| w.id == wid && w.pid == pid && w.is_addressable_target())
        .cloned()
        .ok_or_else(|| {
            format!(
                "window {wid} does not currently belong to pid {pid}; it was closed, replaced, its id was recycled, or it is not an ordinary application window. Call get_app_state again and use the window_id it reports"
            )
        })
}

/// Whether a screen point lies within a window, to within a point of its edge.
///
/// The tolerance exists because AX frames and CGWindow frames disagree by
/// fractions of a point, so an activation point on a window's own border would
/// otherwise fail a strict comparison. On failure returns the frame, rendered,
/// so each caller can phrase its own diagnosis around it.
fn screen_point_inside(w: &WindowInfo, x: f64, y: f64) -> std::result::Result<(), String> {
    const EDGE_TOLERANCE: f64 = 1.0;
    let f = w.frame;
    let inside = x >= f.origin.x - EDGE_TOLERANCE
        && y >= f.origin.y - EDGE_TOLERANCE
        && x <= f.origin.x + f.size.width + EDGE_TOLERANCE
        && y <= f.origin.y + f.size.height + EDGE_TOLERANCE;
    if inside {
        Ok(())
    } else {
        Err(format!(
            "{:.0},{:.0} {:.0}x{:.0}",
            f.origin.x, f.origin.y, f.size.width, f.size.height
        ))
    }
}

/// Pick the element a coordinate names, from a snapshot's frames.
///
/// Actionable candidates win over context-only ones, because a coordinate in a
/// `click` means "press whatever is here" and the static label drawn on top of a
/// button is not a thing that can be pressed. Among equals the smallest frame
/// wins: overlapping frames in an AX tree are almost always ancestor and
/// descendant, and the descendant is the specific answer.
fn hit_test(nodes: &[AxNode], x: f32, y: f32) -> Option<&AxNode> {
    let (x, y) = (f64::from(x), f64::from(y));
    nodes
        .iter()
        .filter(|n| {
            n.frame.is_some_and(|f| {
                x >= f.origin.x
                    && y >= f.origin.y
                    && x < f.origin.x + f.size.width
                    && y < f.origin.y + f.size.height
            })
        })
        .min_by(|a, b| {
            let area = |n: &AxNode| n.frame.map_or(f64::MAX, |f| f.size.width * f.size.height);
            b.is_actionable()
                .cmp(&a.is_actionable())
                .then(area(a).total_cmp(&area(b)))
                // Equal frames are the common case, not an edge one: a table row
                // and its single cell usually occupy exactly the same rectangle.
                // Area cannot separate them, and without this the winner is
                // whichever the breadth-first walk reached first — always the
                // ancestor, so a point inside a cell selected its row. The deeper
                // element is the more specific answer.
                .then(b.depth.cmp(&a.depth))
        })
}

/// A pointer location as it should read in an error, before anything has been
/// resolved. Errors about a drag have to name both ends, and by the time one of
/// them fails to resolve there is nothing better to call it.
fn describe_location(loc: &PointerLocation) -> String {
    match loc {
        PointerLocation::Element(Target::Index { index, .. }) => format!("element {index}"),
        PointerLocation::Element(Target::Point { x, y, .. }) => {
            format!("the element at ({x}, {y})")
        }
        PointerLocation::WindowPoint { x, y } => format!("window-local ({x:.0}, {y:.0})"),
    }
}

fn describe_node(node: &AxNode) -> String {
    match (&node.label, &node.value) {
        (Some(l), _) => format!("[{}] {} {l:?}", node.index, node.role),
        (None, Some(v)) => format!("[{}] {} = {v:?}", node.index, node.role),
        (None, None) => format!("[{}] {}", node.index, node.role),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
