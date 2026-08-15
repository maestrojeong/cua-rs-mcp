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

use crate::apps::{self, AppInfo};
use crate::overlay::Overlay;

// ── errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Resolve(#[from] apps::ResolveError),

    #[error(transparent)]
    Ax(#[from] cua_ax::AxError),

    #[error(transparent)]
    Capture(#[from] cua_capture::CaptureError),

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

    #[error("`{app}` has no accessibility window that cua-rs can drive right now. Verify Accessibility is granted to the process that launched cua-rs, then retry after the app has finished opening. Some apps, including KakaoTalk, temporarily expose no AXWindows while backgrounded; opening a conversation once and retrying can make its tree available. Call check_permissions and list_apps to distinguish a permission or process-discovery problem")]
    NoWindow { app: String },

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

    #[error("{original}. This element advertises no AXPress/AXPick/AXConfirm, and the quiet SkyLight pid-routed click is unavailable: {reason}. cua-rs will not fall back to moving the real pointer. perform_secondary_action with AXShowMenu may reach the same control another way")]
    PidClickUnavailable {
        original: cua_ax::AxError,
        reason: String,
    },

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

    /// The native worker thread died. Unrecoverable for the process.
    #[error("the native worker thread is gone")]
    WorkerGone,

    /// The worker remains alive after this error. A bad native call must not
    /// look like a terminated MCP connection to the client.
    #[error("a native macOS operation panicked while handling this request; the cua-rs server is still running, so retry the call once. If it repeats, restart cua-rs and include the server stderr log")]
    NativePanic,
}

pub type Result<T> = std::result::Result<T, CoreError>;

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
}

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
    pub screenshot: Option<Screenshot>,
    /// Non-fatal problems worth telling the agent about, e.g. a missing screen
    /// recording grant when the tree itself came back fine.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Pixels per point. See [`cua_capture::WindowShot::scale`].
    pub scale: f64,
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

/// How a post-action re-read is rendered.
///
/// Omission notes are turned off because they are prose about the render, not
/// about the app: "12 structural elements omitted" is identical before and
/// after, so it would either be diffed away as noise or, worse, flip when the
/// count changes and read as a real UI change.
fn post_action_render() -> crate::snapshot::RenderOptions {
    crate::snapshot::RenderOptions {
        note_omissions: false,
        ..crate::snapshot::RenderOptions::default()
    }
}

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
    Point { x: f32, y: f32 },
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
    point: Option<(f64, f64)>,
    /// CGWindowID used only to pin the drawn cursor immediately above the
    /// target window instead of above unrelated foreground windows.
    overlay_window_id: Option<u32>,
    /// What the window looked like after the action, when the caller asked.
    pub state: Option<PostActionState>,
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
#[derive(Debug, Clone)]
pub struct PostActionState {
    /// Id of the snapshot taken after the action. Element indices in `diff` or
    /// `tree` belong to *this* snapshot, and it is the one to quote back.
    pub snapshot_id: u64,
    /// Lines that appeared or vanished versus the pre-action tree. `None` when
    /// there was no comparable snapshot to diff against, in which case `tree`
    /// carries the whole outline instead.
    pub diff: Option<crate::snapshot::TreeDiff>,
    /// The full outline, sent only when a diff was not possible.
    pub tree: Option<String>,
    pub node_count: usize,
}

impl ActionResult {
    /// An action that went through the accessibility API, plus the screen point
    /// the overlay should point at (`None` when nothing could be resolved).
    fn ax_at(
        verb: impl Into<String>,
        target: String,
        ui_changed: Observed,
        point: Option<(f64, f64)>,
    ) -> Self {
        Self {
            verb: verb.into(),
            target,
            ui_changed,
            delivery: Delivery::Ax,
            point,
            overlay_window_id: None,
            state: None,
        }
    }

    fn with_overlay_window(mut self, window_id: Option<u32>) -> Self {
        self.overlay_window_id = window_id;
        self
    }
}

/// Work out whether this app's front window can safely be clicked in order to
/// make it key, and where.
///
/// The reference implementation pairs its `ApplicationActivated` notice with a
/// click on the window's own `AXActivationPoint`, which is what actually makes
/// AppKit treat the window as key rather than merely telling the application it
/// is active. Reproducing that means synthesizing a real click at a point the app
/// chose, so it is gated on two checks:
///
/// 1. the window publishes an activation point at all — a guessed title-bar point
///    would be exactly the kind of coordinate that lands on a close button;
/// 2. a live system-wide hit test at that point resolves to *this* pid and to a
///    window rather than a control. Measured on KakaoTalk, the published point
///    sits about six pixels from the close button, so "the app said so" is not on
///    its own enough of a reason to click there.
///
/// `None` means skip the assist and send the bare notice, which is strictly what
/// cua-rs did before and therefore cannot regress anything.
fn window_focus_assist(
    pid: libc::pid_t,
    window_origin: &objc2_core_foundation::CGPoint,
) -> Option<cua_hid::ActivationAssist> {
    let app_el = Element::for_pid(pid);
    let window_el = app_el
        .element(cua_ax::attr::FOCUSED_WINDOW)
        .or_else(|| app_el.element(cua_ax::attr::MAIN_WINDOW))
        .or_else(|| app_el.elements(cua_ax::attr::WINDOWS).into_iter().next())?;

    let point = window_el.activation_point()?;

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
        window_origin: (window_origin.x, window_origin.y),
        activation_point: (point.x, point.y),
    })
}

/// An element's best on-screen point for a drawn cursor: its own
/// `AXActivationPoint` when it has one — not always the geometric centre, for
/// a wide list row or a control with a large transparent hit area — falling
/// back to the frame centre. `None` when the element publishes neither.
fn element_point(el: &Element) -> Option<(f64, f64)> {
    if let Some(p) = el.activation_point() {
        return Some((p.x, p.y));
    }
    el.frame().map(|f| {
        (
            f.origin.x + f.size.width / 2.0,
            f.origin.y + f.size.height / 2.0,
        )
    })
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
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Ax => "ax",
            Delivery::Pid => "pid",
        }
    }
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

// ── worker ───────────────────────────────────────────────────────────────────

type Job = Box<dyn FnOnce(&mut Inner) + Send>;

#[derive(Default)]
struct Inner {
    /// Latest snapshot per pid. Exactly one: keeping history would let an agent
    /// act on an arbitrarily old view of the UI, and the whole point of the
    /// generation check is to prevent that.
    snapshots: HashMap<libc::pid_t, Snapshot>,
    /// Apps already poked for a rich AX tree, keyed by *process lifetime*.
    ///
    /// Setting `AXManualAccessibility` is expensive: the app synchronously
    /// materializes its DOM into a native AX tree and then keeps it in lockstep
    /// with every subsequent mutation. Doing it on every snapshot is a known way
    /// to peg WindowServer, so it happens once.
    ///
    /// Keyed on `(pid, start_time)` rather than pid alone because pids are
    /// recycled. A relaunched Electron app can land on the pid its predecessor
    /// had, and inheriting that "already enabled" decision would skip the poke
    /// and hand back a permanently empty web-content tree.
    enabled: HashSet<ProcessKey>,
    /// What the poke actually achieved, per process lifetime.
    ///
    /// Kept because the write is unreliable and fails silently, so "the tree is
    /// empty" and "the app refused to build one" have to be told apart in the
    /// response rather than guessed at by the caller.
    enablement: HashMap<ProcessKey, cua_ax::Enablement>,
}

/// Identifies a process incarnation, not just a pid slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProcessKey {
    pid: libc::pid_t,
    /// Kernel start time in microseconds since the epoch, or `0` when it could
    /// not be read. A `0` makes the key degrade to pid-only, which is the
    /// pre-existing behavior and still correct in the common case.
    start_time: u64,
}

impl ProcessKey {
    fn for_pid(pid: libc::pid_t) -> Self {
        Self {
            pid,
            start_time: process_start_time(pid).unwrap_or(0),
        }
    }
}

/// Read a process's start time, in microseconds since the epoch.
///
/// Uses `proc_pidinfo(PROC_PIDTBSDINFO)`. The alternative,
/// `sysctl(KERN_PROC_PID)`, needs `struct kinfo_proc`, which embeds the
/// kernel's `extern_proc` and is not exposed by the `libc` crate on Apple
/// platforms; `proc_bsdinfo` is, and it carries the same timestamp.
///
/// Returns `None` on any failure — a process that exited between resolution and
/// this call is normal, not exceptional. Callers degrade to a pid-only key,
/// which is what the code did before this existed.
fn process_start_time(pid: libc::pid_t) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;

    // SAFETY: `info` is a live, correctly-sized `proc_bsdinfo` and `size`
    // describes exactly it, which is what PROC_PIDTBSDINFO expects.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut std::ffi::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// Handle to the native worker. Cheap to clone, safe to share across tasks.
#[derive(Clone)]
pub struct Cua {
    tx: mpsc::Sender<Job>,
    /// The drawn cursor. A separate object rather than something `Inner`
    /// owns: marking it happens from the async-facing wrapper methods below,
    /// on whichever thread is running the `spawn_blocking` call, not on the
    /// single-threaded AX worker — there is no reason to serialize a stdin
    /// write behind tree walks and clicks.
    overlay: std::sync::Arc<Overlay>,
}

impl Cua {
    /// Spawn the worker thread.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("cua-native".into())
            // AX tree walks recurse and some apps are pathologically deep.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let mut inner = Inner::default();
                while let Ok(job) = rx.recv() {
                    // One malformed tree must not take down the worker and with
                    // it every future tool call, so each job is isolated.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&mut inner);
                    }));
                    if let Err(e) = result {
                        tracing::error!("native job panicked: {e:?}");
                    }
                }
            })
            .expect("spawn cua-native thread");
        Self {
            tx,
            overlay: std::sync::Arc::new(Overlay::new()),
        }
    }

    /// Run `f` on the worker thread and wait for its result.
    fn exec<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Inner) -> T + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Box::new(move |inner| {
                // AX objects originate in foreign code. Turn an unexpected
                // panic into this request's error response, keeping the worker
                // available for later MCP calls.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(inner)))
                    .map_err(|_| CoreError::NativePanic);
                let _ = tx.send(outcome);
            }))
            .map_err(|_| CoreError::WorkerGone)?;
        rx.recv().map_err(|_| CoreError::WorkerGone)?
    }

    /// Run an action on the worker thread, then point the drawn cursor at
    /// wherever it landed, if anywhere. Every public action wrapper below is
    /// this plus its own arguments; centralizing it here means the overlay
    /// stays a one-line concern instead of three repeated lines per action.
    /// A no-op on the overlay side for actions that resolved no point (a HID
    /// key event, a select with an unreadable range) — the drawn cursor
    /// simply stays where it was.
    fn exec_action<F>(&self, clicking: bool, f: F) -> Result<ActionResult>
    where
        F: FnOnce(&mut Inner) -> Result<ActionResult> + Send + 'static,
    {
        let result = self.exec(f)?;
        if let Ok(r) = &result {
            if let Some((x, y)) = r.point {
                self.overlay.mark(x, y, clicking, r.overlay_window_id);
            }
        }
        result
    }

    // ── operations ───────────────────────────────────────────────────────

    /// Running applications, frontmost first.
    pub fn list_apps(&self) -> Result<Vec<AppInfo>> {
        self.exec(|_| apps::list_apps())
    }

    /// Whether both grants this server needs are in place.
    pub fn permissions(&self) -> Result<Permissions> {
        self.exec(|_| Permissions {
            accessibility: cua_ax::is_trusted(),
            screen_recording: cua_capture::has_screen_recording_permission(),
        })
    }

    /// Walk one app's front window and, unless told otherwise, capture it.
    ///
    /// This is the one call that must happen before any action: it produces the
    /// indices every action refers to.
    pub fn get_app_state(&self, app: &str, opts: StateOptions) -> Result<AppState> {
        let app = app.to_string();
        self.exec(move |inner| inner.get_app_state(&app, opts))?
    }

    /// Activate an element the way a click would.
    ///
    /// `count` is the click count. `2` means the caller knows this target only
    /// responds to a real double-click, which no accessibility verb can
    /// express — see [`Inner::click`].
    ///
    /// `return_state` re-reads the window afterwards and attaches the result;
    /// see [`PostActionState`] for why that is worth a round trip.
    pub fn click(
        &self,
        app: &str,
        target: Target,
        count: u8,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        self.exec_action(true, move |inner| {
            inner.acting(&app, return_state, |i| i.click(&app, target, count))
        })
    }

    /// Replace a text element's contents.
    pub fn set_value(
        &self,
        app: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let value = value.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| i.set_value(&app, target, &value))
        })
    }

    /// Scroll a scrollable element by whole pages.
    pub fn scroll(
        &self,
        app: &str,
        target: Target,
        dir: ScrollDir,
        pages: u32,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| i.scroll(&app, target, dir, pages))
        })
    }

    /// Append text to an element, preferring insertion over replacement.
    pub fn type_text(
        &self,
        app: &str,
        target: Target,
        text: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let text = text.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| i.type_text(&app, target, &text))
        })
    }

    /// Select a literal substring inside an element's text.
    pub fn select_text(
        &self,
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let text = text.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| {
                i.select_text(&app, target, &text, prefix.as_deref(), suffix.as_deref())
            })
        })
    }

    /// Press a key, through AX when the key has a verb and HID otherwise.
    pub fn press_key(
        &self,
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let key = key.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| i.press_key(&app, target, &key))
        })
    }

    /// Deliver an arbitrary AX action by name.
    pub fn perform_action(
        &self,
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let action = action.to_string();
        self.exec_action(false, move |inner| {
            inner.acting(&app, return_state, |i| {
                i.perform_action(&app, target, &action)
            })
        })
    }

    /// Elements whose label, value or role contains `needle`.
    ///
    /// Reads the existing snapshot when there is one, and takes a fresh one
    /// otherwise, so the caller does not have to sequence a `get_app_state`
    /// first just to search.
    pub fn find(&self, app: &str, needle: &str, limit: usize) -> Result<FindResult> {
        let app = app.to_string();
        let needle = needle.to_string();
        self.exec(move |inner| inner.find(&app, &needle, limit))?
    }

    /// Poll until `needle` appears in (or disappears from) an app's tree.
    pub fn wait_for(
        &self,
        app: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        let app = app.to_string();
        let needle = needle.to_string();
        self.exec(move |inner| inner.wait_for(&app, &needle, want, timeout_ms))?
    }
}

impl Default for Cua {
    fn default() -> Self {
        Self::new()
    }
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

impl ScrollDir {
    fn verb(self) -> &'static str {
        match self {
            ScrollDir::Up => cua_ax::action::SCROLL_UP_BY_PAGE,
            ScrollDir::Down => cua_ax::action::SCROLL_DOWN_BY_PAGE,
            ScrollDir::Left => cua_ax::action::SCROLL_LEFT_BY_PAGE,
            ScrollDir::Right => cua_ax::action::SCROLL_RIGHT_BY_PAGE,
        }
    }
}

// ── worker-side implementation ───────────────────────────────────────────────

impl Inner {
    fn get_app_state(&mut self, query: &str, opts: StateOptions) -> Result<AppState> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let app_el = Element::for_pid(info.pid);

        // Once per app, ask Chromium/Electron to build its tree, then let it
        // settle: the build is asynchronous, so reading immediately would return
        // the same empty window we are trying to fix.
        // `insert` returns false when the key was already present, which makes
        // "poke once per process lifetime" a single atomic step.
        let key = ProcessKey::for_pid(info.pid);
        let first_read = self.enabled.insert(key);
        if first_read {
            let enablement = app_el.enable_rich_accessibility();
            tracing::debug!(?enablement, pid = info.pid, "requested rich accessibility");
            self.enablement.insert(key, enablement);
            // A short settle, not a wait for completion. Some apps do publish
            // within a few hundred milliseconds; the ones that take seconds are
            // handled by telling the caller to ask again rather than by blocking
            // every first call for that long.
            std::thread::sleep(std::time::Duration::from_millis(400));
        }

        let mut warnings = Vec::new();

        // Prefer the focused window, fall back to main, then to the first one.
        // A minimized-only app has none of these, which is a real state and not
        // an error we can paper over.
        let window_el = app_el
            .element(cua_ax::attr::FOCUSED_WINDOW)
            .or_else(|| app_el.element(cua_ax::attr::MAIN_WINDOW))
            .or_else(|| app_el.elements(cua_ax::attr::WINDOWS).into_iter().next())
            .ok_or_else(|| CoreError::NoWindow {
                app: info.name.clone(),
            })?;

        // A scoped walk starts from an element the caller saw in the previous
        // snapshot, not from the window. Resolved before the new snapshot
        // replaces the old one, since that is where the index lives.
        let root = match opts.scope {
            None => window_el.clone(),
            Some(index) => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;
                if snap.process_key != key {
                    return Err(CoreError::ProcessReplaced {
                        app: info.name.clone(),
                    });
                }
                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                node.element.clone()
            }
        };

        let (nodes, complete) = root.snapshot_tree_reporting(opts.limits);

        // A small tree on the *first* read of an app is ambiguous, and the
        // ambiguity is worth stating rather than resolving badly.
        //
        // Chromium and Electron build their accessibility tree lazily once poked,
        // and it does not arrive promptly: Slack measured 13 elements for over
        // three seconds after the poke and 367 a minute later. Deciding "this
        // window is empty" from the first read is therefore wrong, and so is
        // deciding "this app refuses AX" — the read-back of
        // AXManualAccessibility is `false` on Slack even when it is plainly
        // working, so there is no signal there either.
        //
        // What is honest and actionable in both cases: say the tree may still be
        // building, say to ask again, and say what it means if it never grows.
        const LOOKS_EMPTY: usize = 20;
        if nodes.len() < LOOKS_EMPTY && first_read {
            warnings.push(format!(
                "only {} elements on the first read of this app. Chromium and Electron apps build \
                 their accessibility tree lazily after being asked, and it can take several \
                 seconds to appear — call get_app_state again. If it stays this small across a \
                 few tries, this app does not expose its web content over the accessibility API \
                 at all and has to be driven over CDP instead; its native chrome (window buttons, \
                 menu bar) is still reachable here.",
                nodes.len()
            ));
        }

        if nodes.len() >= opts.limits.max_nodes {
            warnings.push(format!(
                "tree truncated at {} elements; pass a larger max_nodes or narrow the target",
                opts.limits.max_nodes
            ));
        } else if !complete {
            // Truncation by *time*, which looks nothing like truncation by
            // count: the tree is short, so nothing suggests anything is
            // missing. Measured on KakaoTalk with ten windows open, a walk that
            // would have returned 2000 nodes took 171 s; the budget cuts that
            // to 10 s and 429 nodes, and the conversation the caller wanted was
            // in the part that never arrived. Without this line the caller
            // concludes the element does not exist.
            warnings.push(format!(
                "tree is INCOMPLETE: the walk hit its {:.0}s time budget after {} elements, so \
                 anything further down is missing rather than absent. This app is answering \
                 accessibility calls slowly. Narrow the walk with scope_element_id, or use find \
                 to search, before concluding an element is not there",
                opts.limits.budget.as_secs_f64(),
                nodes.len()
            ));
        }

        // Match the AX window to a ScreenCaptureKit window by pid + frame.
        // The direct route would be `_AXUIElementGetWindow`, which is a private
        // symbol; matching on public API keeps *window identity* off SPI and
        // thus off the "breaks on the next macOS release" risk. (Input
        // synthesis's quiet tier does use SkyLight SPI, but that lives in
        // cua-hid, not in this matching path.)
        let ax_frame = window_el.frame();
        let window = match cua_capture::list_windows() {
            Ok(list) => best_window_match(&list, info.pid, ax_frame),
            Err(e) => {
                // The tree is still useful without pixels, so this is a warning
                // and not a failure.
                warnings.push(e.to_string());
                None
            }
        };

        let screenshot = match (opts.include_screenshot, &window) {
            (true, Some(w)) => match cua_capture::capture_window(w.id, opts.max_image_dim) {
                Ok(shot) => Some(Screenshot {
                    png: shot.png,
                    width: shot.width,
                    height: shot.height,
                    scale: shot.scale,
                }),
                Err(e) => {
                    warnings.push(e.to_string());
                    None
                }
            },
            (true, None) => {
                warnings.push("could not identify a capturable window for this app".into());
                None
            }
            (false, _) => None,
        };

        let id = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
        let tree = crate::snapshot::render_tree(&nodes, opts.render);
        let node_count = nodes.len();
        let actionable_count = nodes.iter().filter(|n| n.is_actionable()).count();
        let window_title = window.as_ref().and_then(|w| w.title.clone());

        self.snapshots.insert(
            info.pid,
            Snapshot {
                id,
                nodes,
                window: window.clone(),
                taken_at: Instant::now(),
                process_key: key,
            },
        );

        Ok(AppState {
            app: info,
            snapshot_id: id,
            tree,
            node_count,
            actionable_count,
            window_title,
            window_frame: window.map(|w| w.frame).or(ax_frame),
            screenshot,
            warnings,
        })
    }

    /// Turn a [`Target`] into a concrete element, validating snapshot identity.
    fn resolve(&self, query: &str, target: &Target) -> Result<(AppInfo, Element, String)> {
        let info = apps::resolve_app(query)?;
        match *target {
            Target::Index {
                index,
                snapshot_id,
                ref expected_role,
            } => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;

                if snap.process_key != ProcessKey::for_pid(info.pid) {
                    return Err(CoreError::ProcessReplaced {
                        app: info.name.clone(),
                    });
                }

                // Only checked when the caller supplied an id. Requiring it
                // would break the simple "read then act in one turn" flow that
                // is the overwhelmingly common case; honoring it when present
                // lets a careful caller get a hard guarantee.
                if let Some(given) = snapshot_id {
                    if given != snap.id {
                        return Err(CoreError::StaleSnapshot {
                            index,
                            given,
                            current: snap.id,
                        });
                    }
                }

                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                if let Some(expected) = expected_role {
                    if &node.role != expected {
                        return Err(CoreError::TokenRoleMismatch {
                            index,
                            expected: expected.clone(),
                            found: node.role.clone(),
                        });
                    }
                }

                Ok((info, node.element.clone(), describe_node(node)))
            }
            Target::Point { x, y } => {
                let app_el = Element::for_pid(info.pid);
                let el = app_el.element_at(x, y)?;
                let desc = format!(
                    "{} at ({x}, {y})",
                    el.role().unwrap_or_else(|| "AXUnknown".into())
                );
                Ok((info, el, desc))
            }
        }
    }

    /// Run one action and, when asked, re-read the window and attach what
    /// changed.
    ///
    /// The re-read has to happen here rather than in a follow-up call for two
    /// reasons. It is the same hop on the AX worker thread, so it cannot
    /// interleave with another caller's action; and the *pre*-action tree has to
    /// be rendered before the action runs, because the action replaces the
    /// snapshot it would have been rendered from.
    ///
    /// Failing to re-read is not an error. The action already happened, and
    /// reporting it as a failure because the follow-up read did not work would
    /// invite a caller to retry something that already took effect.
    fn acting<F>(&mut self, query: &str, return_state: bool, act: F) -> Result<ActionResult>
    where
        F: FnOnce(&mut Self) -> Result<ActionResult>,
    {
        let before = if return_state {
            self.rendered_current_tree(query)
        } else {
            None
        };

        let mut result = act(self)?;

        if return_state {
            result.state = self.read_state_after(query, before);
        }
        Ok(result)
    }

    /// The outline of the snapshot this app already has, rendered the same way
    /// the post-action read will be so the two are comparable.
    fn rendered_current_tree(&self, query: &str) -> Option<String> {
        let info = apps::resolve_app(query).ok()?;
        let snap = self.snapshots.get(&info.pid)?;
        Some(crate::snapshot::render_tree(
            &snap.nodes,
            post_action_render(),
        ))
    }

    /// Re-walk the window after an action and diff it against `before`.
    fn read_state_after(&mut self, query: &str, before: Option<String>) -> Option<PostActionState> {
        let opts = StateOptions {
            include_screenshot: false,
            render: post_action_render(),
            ..StateOptions::default()
        };
        let state = self.get_app_state(query, opts).ok()?;
        Some(PostActionState {
            snapshot_id: state.snapshot_id,
            diff: before
                .as_deref()
                .map(|b| crate::snapshot::diff_trees(b, &state.tree)),
            tree: if before.is_some() {
                None
            } else {
                Some(state.tree)
            },
            node_count: state.node_count,
        })
    }

    fn click(&mut self, query: &str, target: Target, count: u8) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.window_fingerprint(info.pid);
        let expected = match &target {
            Target::Index { index, .. } => self
                .snapshots
                .get(&info.pid)
                .map(|snapshot| (*index, snapshot_tokens(&snapshot.nodes, *index))),
            Target::Point { .. } => None,
        };

        // AX first, always — unless the caller asked for a double-click, which
        // the accessibility API simply cannot say. `AXPress` is "activate this
        // element", with no notion of click count, and performing it twice is
        // not the same event: an app that opens on double-click and selects on
        // single-click (KakaoTalk's conversation list, measured) would see two
        // selections. So a `count` above 1 is a statement that only a real
        // mouse event will do, and it goes straight to pid-routed delivery.
        let ax_err = if count > 1 {
            // Checked *before* activating, not after: `el.activate()` is not a
            // query, and performing a press only to discard it would deliver
            // the single click the caller explicitly said was wrong.
            cua_ax::AxError::Unsupported {
                what: "action",
                name: format!("a {count}-click (accessibility has no click count)"),
            }
        } else {
            match el.activate() {
                Ok(verb) => {
                    let changed = self.changed_since(info.pid, before);
                    return Ok(ActionResult::ax_at(verb, desc, changed, element_point(&el))
                        .with_overlay_window(self.overlay_window_id(info.pid)));
                }
                Err(e) => e,
            }
        };

        if !matches!(ax_err, cua_ax::AxError::Unsupported { .. }) {
            return Err(CoreError::Ax(ax_err));
        }

        // No AX verb landed. Route a click to the target process without ever
        // touching the shared cursor. `AXActivationPoint` is the app's answer
        // to "where is this element clicked", and it is not always the middle
        // of its frame, so prefer it when available.
        let (x, y) = match el.activation_point() {
            Some(p) => (p.x, p.y),
            None => {
                let Some(frame) = el.frame() else {
                    return Err(CoreError::PidClickUnavailable {
                        original: ax_err,
                        reason: "the element publishes neither AXActivationPoint nor AXFrame"
                            .into(),
                    });
                };
                (
                    frame.origin.x + frame.size.width / 2.0,
                    frame.origin.y + frame.size.height / 2.0,
                )
            }
        };

        if !cua_hid::skylight_available() {
            return Err(CoreError::PidClickUnavailable {
                original: ax_err,
                reason: "SLEventPostToPid is not available on this macOS version".into(),
            });
        }

        // Re-enumerate the exact window immediately before posting. The
        // snapshot's frame can be stale if the user moved/resized the window,
        // and CGWindowIDs can be recycled after a close. A pid-addressed event
        // with a stale stamped window id is not safe enough to send.
        let snapshot_window = self
            .snapshots
            .get(&info.pid)
            .and_then(|snap| snap.window.as_ref())
            .cloned()
            .ok_or_else(|| CoreError::PidClickUnavailable {
                original: ax_err.clone(),
                reason: "the snapshot has no verified ScreenCaptureKit window id; enable Screen Recording and take a fresh snapshot".into(),
            })?;
        let live_windows =
            cua_capture::list_windows().map_err(|e| CoreError::PidClickUnavailable {
                original: ax_err.clone(),
                reason: format!(
                    "could not revalidate the target window immediately before input: {e}"
                ),
            })?;
        let live_window =
            current_window_for_pid_click(&live_windows, &snapshot_window, info.pid, x, y).map_err(
                |reason| CoreError::PidClickUnavailable {
                    original: ax_err.clone(),
                    reason,
                },
            )?;
        let wid = live_window.id;
        let window_local = (
            x - live_window.frame.origin.x,
            y - live_window.frame.origin.y,
        );

        if let Some((index, expected)) = expected {
            if !expected.is_empty() {
                let found = live_tokens(&el);
                if !tokens_still_present(&expected, &found) {
                    let mut gone: Vec<&String> = expected.difference(&found).collect();
                    gone.sort();
                    return Err(CoreError::TargetChanged {
                        index,
                        expected: format!("{gone:?}"),
                        found: format!("{:?}", sorted(&found)),
                    });
                }
            }
        }

        // The synthesized activation notice inside `click_background_pid` only
        // takes effect once the target's own run loop drains it, so the click has
        // to wait for the target to agree. `AXFrontmost` on the application
        // element is that agreement: it reflects what the *app* thinks, not what
        // `NSWorkspace` thinks, which is exactly the distinction the notice
        // exploits. Read fresh each poll — a cached answer would defeat the point.
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        let assist = window_focus_assist(info.pid, &live_window.frame.origin);
        cua_hid::click_background_pid(
            cua_hid::PidClick {
                pid: info.pid,
                point: (x, y),
                window_local,
                wid,
                count,
            },
            assist,
            &believes_frontmost,
        )
        .map_err(|e| CoreError::PidClickUnavailable {
            original: ax_err,
            reason: e.to_string(),
        })?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!("SkyLight pid-routed {count}-click at ({x:.0}, {y:.0})"),
            target: desc,
            ui_changed: changed,
            delivery: Delivery::Pid,
            point: Some((x, y)),
            overlay_window_id: Some(wid),
            state: None,
        })
    }

    fn set_value(&mut self, query: &str, target: Target, value: &str) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.window_fingerprint(info.pid);
        el.set_string(cua_ax::attr::VALUE, value)?;
        let changed = self.changed_since(info.pid, before);
        Ok(
            ActionResult::ax_at("AXValue=", desc, changed, element_point(&el))
                .with_overlay_window(self.overlay_window_id(info.pid)),
        )
    }

    fn type_text(&mut self, query: &str, target: Target, text: &str) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.window_fingerprint(info.pid);
        let write = el.append_text(text)?;
        let changed = self.changed_since(info.pid, before);
        // Name the mechanism, not just the intent. "typed" would imply
        // keystrokes were synthesized, which is exactly what did not happen.
        Ok(ActionResult::ax_at(
            format!("AXSelectedText+ ({})", write.as_str()),
            desc,
            changed,
            element_point(&el),
        )
        .with_overlay_window(self.overlay_window_id(info.pid)))
    }

    fn select_text(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let range = el.select_text(text, prefix, suffix)?;
        // Selecting text changes no window state the fingerprint can see, and
        // claiming otherwise would be noise. The returned range is the evidence
        // that it worked.
        Ok(ActionResult::ax_at(
            format!(
                "AXSelectedTextRange={{offset:{},length:{}}}",
                range.offset, range.length
            ),
            desc,
            // Selection changes no window state the fingerprint can see, so
            // there is nothing to observe here either way.
            Observed::Unknown,
            element_point(&el),
        )
        .with_overlay_window(self.overlay_window_id(info.pid)))
    }

    fn press_key(&mut self, query: &str, target: Target, key: &str) -> Result<ActionResult> {
        // Capability first so an unsupported chord reports the permanent API
        // constraint instead of a misleading missing-snapshot error.
        let Some(ax_verb) = ax_verb_for_key(key) else {
            return Err(CoreError::KeyNoAccessibilityEquivalent {
                key: key.to_string(),
            });
        };

        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.window_fingerprint(info.pid);

        let available = el.actions();
        if !available.iter().any(|a| a == ax_verb) {
            return Err(CoreError::KeyVerbUnsupported {
                key: key.to_string(),
                verb: ax_verb,
                available: format!("{available:?}"),
            });
        }

        el.perform(ax_verb)?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax_at(
            format!("{ax_verb} (for {key})"),
            desc,
            changed,
            element_point(&el),
        )
        .with_overlay_window(self.overlay_window_id(info.pid)))
    }

    fn perform_action(
        &mut self,
        query: &str,
        target: Target,
        action: &str,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let available = el.actions();
        if !available.iter().any(|a| a == action) {
            // List what the element *does* support: an agent that guessed a verb
            // can fix itself in one step instead of retrying blindly.
            return Err(CoreError::Ax(cua_ax::AxError::Unsupported {
                what: "action",
                name: format!("{action} (this element supports {available:?})"),
            }));
        }
        let before = self.window_fingerprint(info.pid);
        el.perform(action)?;
        let changed = self.changed_since(info.pid, before);
        Ok(
            ActionResult::ax_at(action, desc, changed, element_point(&el))
                .with_overlay_window(self.overlay_window_id(info.pid)),
        )
    }

    fn find(&mut self, query: &str, needle: &str, limit: usize) -> Result<FindResult> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;

        // Search the snapshot the agent is already holding, so the indices it
        // gets back stay valid against the state it has seen. Only walk afresh
        // when there is nothing to search.
        let snapshot_id = match self.snapshots.get(&info.pid) {
            Some(s) => s.id,
            None => {
                let opts = StateOptions {
                    include_screenshot: false,
                    ..Default::default()
                };
                self.get_app_state(query, opts)?.snapshot_id
            }
        };

        let snap = self
            .snapshots
            .get(&info.pid)
            .ok_or_else(|| CoreError::NoSnapshot {
                app: info.name.clone(),
            })?;

        let hits = match_nodes(&snap.nodes, needle, limit);
        Ok(FindResult {
            snapshot_id,
            total: hits.len(),
            lines: hits,
            searched: snap.nodes.len(),
        })
    }

    fn wait_for(
        &mut self,
        query: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        cua_ax::require_trusted()?;
        let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let opts = StateOptions {
            include_screenshot: false,
            ..Default::default()
        };

        let mut polls = 0u32;
        loop {
            polls += 1;
            let state = self.get_app_state(query, opts)?;
            let present = state.tree.contains(needle);
            if present == want.wants_present() {
                return Ok(WaitOutcome {
                    satisfied: true,
                    polls,
                    snapshot_id: state.snapshot_id,
                    elapsed_ms: elapsed_ms_until(deadline, timeout_ms),
                });
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome {
                    satisfied: false,
                    polls,
                    snapshot_id: state.snapshot_id,
                    elapsed_ms: timeout_ms,
                });
            }
            // A full tree walk per poll is not cheap, so the floor here is what
            // keeps `wait_for` from becoming a busy loop that starves the app it
            // is watching.
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    fn scroll(
        &mut self,
        query: &str,
        target: Target,
        dir: ScrollDir,
        pages: u32,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.window_fingerprint(info.pid);
        let verb = dir.verb();
        for _ in 0..pages.max(1) {
            el.perform(verb)?;
        }
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax_at(verb, desc, changed, element_point(&el))
            .with_overlay_window(self.overlay_window_id(info.pid)))
    }

    fn overlay_window_id(&self, pid: libc::pid_t) -> Option<u32> {
        self.snapshots
            .get(&pid)
            .and_then(|snapshot| snapshot.window.as_ref())
            .map(|window| window.id)
    }

    /// A cheap proxy for "did the UI move".
    ///
    /// Deliberately not a second full tree walk: that would double the cost of
    /// every action to answer a question the agent can answer better by taking a
    /// fresh snapshot when it actually needs one. The focused element's identity
    /// plus the window title catches the common cases — a dialog opened, focus
    /// moved, a tab switched — and honestly reports `false` otherwise rather
    /// than claiming success it cannot see.
    fn window_fingerprint(&self, pid: libc::pid_t) -> Option<String> {
        let app = Element::for_pid(pid);
        let focused = app.element(cua_ax::attr::FOCUSED_UI_ELEMENT);
        let title = app
            .element(cua_ax::attr::FOCUSED_WINDOW)
            .and_then(|w| w.string(cua_ax::attr::TITLE));
        let fingerprint = format!(
            "{}|{}|{}",
            title.unwrap_or_default(),
            focused.as_ref().and_then(|f| f.role()).unwrap_or_default(),
            focused
                .as_ref()
                .and_then(|f| f.string(cua_ax::attr::TITLE))
                .unwrap_or_default()
        );
        // Every field empty means the app told us nothing — not that it is in
        // a particular state. Returning `Some("||")` here would make two such
        // reads compare equal and manufacture an `Unchanged` out of silence.
        if fingerprint == "||" {
            return None;
        }
        Some(fingerprint)
    }

    fn changed_since(&self, pid: libc::pid_t, before: Option<String>) -> Observed {
        // A short settle window: AX reflects most changes within a frame or two,
        // and waiting longer would add latency to every single action.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let after = self.window_fingerprint(pid);
        match (before, after) {
            // Either end unreadable and the comparison is meaningless. Say so
            // instead of picking the answer that happens to be shorter.
            (None, _) | (_, None) => Observed::Unknown,
            (Some(a), Some(b)) if a == b => Observed::Unchanged,
            _ => Observed::Changed,
        }
    }
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

/// Whether the caller is waiting for text to appear or to go away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Appears,
    Disappears,
}

impl Presence {
    fn wants_present(self) -> bool {
        matches!(self, Presence::Appears)
    }
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

fn elapsed_ms_until(deadline: Instant, timeout_ms: u64) -> u64 {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64;
    timeout_ms.saturating_sub(remaining)
}

/// Case-insensitive substring search over a snapshot's nodes.
///
/// Searches label, value and role, in that priority order — a query like
/// "Send" should find the button labeled Send before it finds an `AXSendButton`
/// role — and returns rendered lines rather than raw nodes so the output format
/// matches what `get_app_state` already showed the agent.
///
/// Actionable matches are listed first: a search is almost always the prelude to
/// an action, and a non-actionable static-text hit is context at best.
fn match_nodes(nodes: &[AxNode], needle: &str, limit: usize) -> Vec<String> {
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(u8, &AxNode)> = Vec::new();
    for node in nodes {
        let rank = if node
            .label
            .as_deref()
            .is_some_and(|l| l.to_lowercase().contains(&needle))
        {
            0
        } else if node
            .value
            .as_deref()
            .is_some_and(|v| v.to_lowercase().contains(&needle))
        {
            1
        } else if node.role.to_lowercase().contains(&needle) {
            2
        } else {
            continue;
        };
        // Actionable-first, then match quality, then document order.
        scored.push((rank + if node.is_actionable() { 0 } else { 3 }, node));
    }

    scored.sort_by_key(|(rank, node)| (*rank, node.index));
    scored
        .into_iter()
        .take(limit.max(1))
        .map(|(_, node)| render_match(node))
        .collect()
}

fn render_match(node: &AxNode) -> String {
    let mut s = String::new();
    if node.is_actionable() {
        s.push_str(&format!("[{}] ", node.index));
    } else {
        s.push_str("(not actionable) ");
    }
    s.push_str(&node.role);
    if let Some(l) = &node.label {
        s.push_str(&format!(" {l:?}"));
    }
    if let Some(v) = &node.value {
        if node.label.as_deref() != Some(v.as_str()) {
            let short: String = v.chars().take(80).collect();
            s.push_str(&format!(" = {short:?}"));
        }
    }
    if !node.enabled {
        s.push_str(" (disabled)");
    }
    s
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
    let live = windows
        .iter()
        .find(|w| w.id == snapshot.id && w.pid == pid && w.is_plausible_target())
        .ok_or_else(|| {
            format!(
                "window {} no longer belongs to pid {pid}; it was closed, replaced, or its id was recycled. Call get_app_state again",
                snapshot.id
            )
        })?;

    let f = live.frame;
    const EDGE_TOLERANCE: f64 = 1.0;
    let inside = x >= f.origin.x - EDGE_TOLERANCE
        && y >= f.origin.y - EDGE_TOLERANCE
        && x <= f.origin.x + f.size.width + EDGE_TOLERANCE
        && y <= f.origin.y + f.size.height + EDGE_TOLERANCE;
    if !inside {
        return Err(format!(
            "target point ({x:.0}, {y:.0}) is outside the current frame of window {} ({:.0},{:.0} {:.0}x{:.0}); the AX element and window snapshot drifted apart. Call get_app_state again",
            live.id,
            f.origin.x,
            f.origin.y,
            f.size.width,
            f.size.height
        ));
    }

    Ok(live.clone())
}

fn describe_node(node: &AxNode) -> String {
    match (&node.label, &node.value) {
        (Some(l), _) => format!("[{}] {} {l:?}", node.index, node.role),
        (None, Some(v)) => format!("[{}] {} = {v:?}", node.index, node.role),
        (None, None) => format!("[{}] {}", node.index, node.role),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_ax::Element;
    use objc2_core_foundation::{CGPoint, CGSize};

    fn win(id: u32, pid: libc::pid_t, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            title: None,
            pid,
            bundle_id: None,
            app_name: None,
            frame: CGRect {
                origin: CGPoint { x, y },
                size: CGSize {
                    width: w,
                    height: h,
                },
            },
            on_screen: true,
            layer: 0,
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect {
            origin: CGPoint { x, y },
            size: CGSize {
                width: w,
                height: h,
            },
        }
    }

    fn tnode(
        index: usize,
        role: &str,
        label: Option<&str>,
        value: Option<&str>,
        act: bool,
    ) -> AxNode {
        AxNode {
            index,
            depth: 0,
            parent: None,
            role: role.to_string(),
            subrole: None,
            label: label.map(str::to_string),
            value: value.map(str::to_string),
            help: None,
            frame: None,
            enabled: true,
            focused: false,
            selected: false,
            actions: if act {
                vec!["AXPress".to_string()]
            } else {
                vec![]
            },
            settable: false,
            element: Element::system_wide(),
        }
    }

    #[test]
    fn pid_click_revalidation_uses_the_live_moved_window_frame() {
        let snapshot = win(7, 42, 100.0, 100.0, 800.0, 600.0);
        let moved = win(7, 42, -400.0, 50.0, 800.0, 600.0);
        let live = current_window_for_pid_click(&[moved], &snapshot, 42, -200.0, 200.0)
            .expect("same id and pid should survive a move");
        assert_eq!(live.frame.origin.x, -400.0);
        assert_eq!(
            (-200.0 - live.frame.origin.x, 200.0 - live.frame.origin.y),
            (200.0, 150.0),
            "window-local input must use the live frame, not the snapshot frame"
        );
    }

    #[test]
    fn pid_click_revalidation_rejects_a_recycled_window_id() {
        let snapshot = win(7, 42, 0.0, 0.0, 800.0, 600.0);
        let recycled = win(7, 99, 0.0, 0.0, 800.0, 600.0);
        let err = current_window_for_pid_click(&[recycled], &snapshot, 42, 100.0, 100.0)
            .expect_err("same window id owned by another pid must fail closed");
        assert!(err.contains("no longer belongs to pid 42"), "got {err}");
    }

    #[test]
    fn pid_click_revalidation_rejects_ax_window_drift() {
        let snapshot = win(7, 42, 0.0, 0.0, 800.0, 600.0);
        let live = snapshot.clone();
        let err = current_window_for_pid_click(&[live], &snapshot, 42, 900.0, 100.0)
            .expect_err("a point outside the validated window must not be posted");
        assert!(err.contains("outside the current frame"), "got {err}");
    }

    /// Every key this maps to a verb must be one `press_key` can deliver
    /// without focusing anything.
    #[test]
    fn the_keys_offered_as_the_focus_free_alternative_really_are() {
        for key in ["return", "enter", "escape", "esc", "up", "down"] {
            assert!(
                ax_verb_for_key(key).is_some(),
                "{key} is advertised as needing no focus but has no AX verb"
            );
        }
    }

    /// The whole point of putting the role in the token is that a caller can
    /// be told *what changed*, not just that something did.
    #[test]
    fn a_token_role_mismatch_names_both_roles() {
        let err = CoreError::TokenRoleMismatch {
            index: 233,
            expected: "AXCell".into(),
            found: "AXButton".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("233"), "got {msg}");
        assert!(
            msg.contains("AXCell") && msg.contains("AXButton"),
            "got {msg}"
        );
        assert!(
            msg.contains("get_app_state"),
            "the remedy must be in the message: {msg}"
        );
    }

    #[test]
    fn observed_labels_are_stable_and_three_valued() {
        assert_eq!(Observed::Changed.as_str(), "yes");
        assert_eq!(Observed::Unchanged.as_str(), "no");
        assert_eq!(Observed::Unknown.as_str(), "unknown");
    }

    #[test]
    fn recycled_rows_fail_the_exact_target_check() {
        let expected = HashSet::from(["Alice".to_string(), "Profile".to_string()]);
        let same_row = HashSet::from([
            "Alice".to_string(),
            "Profile".to_string(),
            "New preview".to_string(),
        ]);
        let other_row = HashSet::from(["Bob".to_string(), "Profile".to_string()]);

        assert!(tokens_still_present(&expected, &same_row));
        assert!(!tokens_still_present(&expected, &other_row));
    }

    #[test]
    fn appkit_placeholder_identifiers_are_not_identity() {
        let mut out = HashSet::new();
        push_token(Some("_NS:87".to_string()), &mut out);
        push_token(Some("   ".to_string()), &mut out);
        push_token(None, &mut out);
        push_token(Some("Alice".to_string()), &mut out);
        assert_eq!(sorted(&out), vec!["Alice"]);
    }

    #[test]
    fn find_matches_case_insensitively_across_label_value_and_role() {
        let nodes = vec![
            tnode(0, "AXButton", Some("Send"), None, true),
            tnode(1, "AXTextArea", None, Some("please SEND it"), true),
            tnode(2, "AXSendButton", None, None, true),
            tnode(3, "AXButton", Some("Cancel"), None, true),
        ];
        let hits = match_nodes(&nodes, "send", 10);
        assert_eq!(hits.len(), 3, "got {hits:?}");
        // Label match ranks above value match, which ranks above role match.
        assert!(hits[0].contains("\"Send\""), "got {:?}", hits[0]);
        assert!(hits[1].contains("please SEND it"), "got {:?}", hits[1]);
        assert!(hits[2].contains("AXSendButton"), "got {:?}", hits[2]);
    }

    #[test]
    fn find_puts_actionable_matches_before_context() {
        let nodes = vec![
            tnode(0, "AXStaticText", Some("Save now"), None, false),
            tnode(1, "AXButton", Some("Save"), None, true),
        ];
        let hits = match_nodes(&nodes, "save", 10);
        assert!(
            hits[0].starts_with("[1] "),
            "actionable first, got {hits:?}"
        );
        assert!(hits[1].starts_with("(not actionable)"), "got {hits:?}");
    }

    #[test]
    fn find_respects_the_limit_and_rejects_an_empty_needle() {
        let nodes: Vec<AxNode> = (0..10)
            .map(|i| tnode(i, "AXButton", Some("item"), None, true))
            .collect();
        assert_eq!(match_nodes(&nodes, "item", 3).len(), 3);
        // A zero limit must still return something rather than silently nothing.
        assert_eq!(match_nodes(&nodes, "item", 0).len(), 1);
        assert!(match_nodes(&nodes, "", 5).is_empty());
    }

    #[test]
    fn find_does_not_print_a_value_that_repeats_the_label() {
        let nodes = vec![tnode(0, "AXStaticText", Some("dup"), Some("dup"), true)];
        let hits = match_nodes(&nodes, "dup", 5);
        assert_eq!(hits[0].matches("dup").count(), 1, "got {:?}", hits[0]);
    }

    #[test]
    fn only_intent_like_keys_have_an_ax_verb() {
        assert_eq!(ax_verb_for_key("return"), Some("AXConfirm"));
        assert_eq!(ax_verb_for_key("Enter"), Some("AXConfirm"));
        assert_eq!(ax_verb_for_key(" ESC "), Some("AXCancel"));
        assert_eq!(ax_verb_for_key("up"), Some("AXIncrement"));
        assert_eq!(ax_verb_for_key("down"), Some("AXDecrement"));
        // cua-rs deliberately refuses keys AX cannot address to an element.
        assert_eq!(ax_verb_for_key("cmd+shift+p"), None);
        assert_eq!(ax_verb_for_key("a"), None);
        assert_eq!(ax_verb_for_key("f5"), None);
    }

    #[test]
    fn delivery_labels_are_stable() {
        assert_eq!(Delivery::Ax.as_str(), "ax");
        assert_eq!(Delivery::Pid.as_str(), "pid");
    }

    #[test]
    fn a_panicking_native_job_returns_an_error_without_killing_the_worker() {
        let cua = Cua::new();
        let err = cua
            .exec::<(), _>(|_| panic!("synthetic native failure"))
            .expect_err("panic must be returned to the caller");
        assert!(matches!(err, CoreError::NativePanic));

        // A follow-up request must still be serviceable; otherwise MCP sees a
        // connection close instead of the original tool error.
        assert_eq!(cua.exec(|_| 7usize).unwrap(), 7);
    }

    #[test]
    fn pid_click_failure_promises_no_pointer_fallback() {
        let msg = CoreError::PidClickUnavailable {
            original: cua_ax::AxError::Unsupported {
                what: "action",
                name: "any of [\"AXPress\", \"AXPick\", \"AXConfirm\"]".into(),
            },
            reason: "SLEventPostToPid unavailable".into(),
        }
        .to_string();
        assert!(
            msg.contains("AXPress"),
            "must keep the original AX error visible: {msg}"
        );
        assert!(msg.contains("will not fall back to moving"), "got {msg}");
        assert!(
            msg.contains("AXShowMenu"),
            "must point at a background-safe alternative too: {msg}"
        );
    }

    #[test]
    fn an_unsupported_ax_verb_does_not_contradict_itself() {
        // The bug this guards: escape *does* have an AX verb, so refusing it
        // with the generic HID message produced text that named escape as
        // something that works without HID.
        let msg = CoreError::KeyVerbUnsupported {
            key: "escape".into(),
            verb: "AXCancel",
            available: r#"["AXPress"]"#.into(),
        }
        .to_string();
        assert!(msg.contains("AXCancel"), "must name the verb: {msg}");
        assert!(
            msg.contains("[\"AXPress\"]"),
            "must list what the element does accept: {msg}"
        );
        assert!(
            !msg.contains("escape work"),
            "must not claim escape works while refusing escape: {msg}"
        );
    }

    #[test]
    fn refusing_a_chord_explains_the_ax_alternatives() {
        let msg = CoreError::KeyNoAccessibilityEquivalent {
            key: "cmd+shift+p".into(),
        }
        .to_string();
        assert!(msg.contains("does not synthesize shared HID"), "got {msg}");
        assert!(
            msg.contains("AXShowMenu") && msg.contains("return/enter"),
            "must point at background-safe alternatives: {msg}"
        );
    }

    #[test]
    fn presence_maps_to_the_expected_polarity() {
        assert!(Presence::Appears.wants_present());
        assert!(!Presence::Disappears.wants_present());
    }

    #[test]
    fn window_match_prefers_the_frame_the_ax_tree_reported() {
        let windows = vec![
            win(1, 500, 0.0, 0.0, 400.0, 300.0),
            win(2, 500, 100.0, 100.0, 800.0, 600.0),
        ];
        let got = best_window_match(&windows, 500, Some(rect(102.0, 99.0, 800.0, 600.0)));
        assert_eq!(
            got.unwrap().id,
            2,
            "a few points of drift must not flip the match"
        );
    }

    #[test]
    fn window_match_ignores_other_processes() {
        let windows = vec![win(1, 999, 0.0, 0.0, 800.0, 600.0)];
        assert!(
            best_window_match(&windows, 500, Some(rect(0.0, 0.0, 800.0, 600.0))).is_none(),
            "an identical frame in another app is never the right window"
        );
    }

    #[test]
    fn identical_frames_prefer_the_window_that_is_on_screen() {
        let mut hidden_tab = win(1, 500, 0.0, 0.0, 800.0, 600.0);
        hidden_tab.on_screen = false;
        let visible_tab = win(2, 500, 0.0, 0.0, 800.0, 600.0);
        let windows = vec![hidden_tab, visible_tab];
        assert_eq!(
            best_window_match(&windows, 500, Some(rect(0.0, 0.0, 800.0, 600.0)))
                .unwrap()
                .id,
            2
        );
    }

    #[test]
    fn without_an_ax_frame_the_largest_window_wins() {
        let windows = vec![
            win(1, 7, 0.0, 0.0, 100.0, 100.0),
            win(2, 7, 0.0, 0.0, 1200.0, 800.0),
        ];
        assert_eq!(best_window_match(&windows, 7, None).unwrap().id, 2);
    }

    #[test]
    fn overlay_windows_are_never_matched() {
        let mut overlay = win(1, 7, 0.0, 0.0, 800.0, 600.0);
        overlay.layer = 25;
        assert!(best_window_match(&[overlay], 7, None).is_none());
    }
}
