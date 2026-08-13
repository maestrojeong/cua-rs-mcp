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

    #[error("element_index {index} is out of range (the snapshot has {count} elements)")]
    BadIndex { index: usize, count: usize },

    #[error("`{app}` has no window that can be captured or driven right now")]
    NoWindow { app: String },

    /// A key was requested that only a real key event can deliver, but the
    /// server was not started with `--allow-hid`.
    ///
    /// Deliberately not a silent fallback: synthesizing the event would move the
    /// user's cursor and steal their focus, which is the one thing this server
    /// promises not to do. The operator, not the agent, decides to allow it.
    #[error("`{key}` has no accessibility equivalent, so it can only be sent as a real key event. That moves the cursor and steals keyboard focus, so it requires starting the server with --allow-hid. Alternatives that stay in the background: return/enter and escape on an element that accepts AXConfirm/AXCancel, perform_secondary_action with AXShowMenu for a context menu, or clicking the menu item directly")]
    HidRefused { key: String },

    /// The key has an AX verb, but this particular element does not accept it.
    ///
    /// Kept distinct from [`CoreError::HidRefused`] so the message cannot
    /// contradict itself by naming the very key it just refused as one that
    /// works.
    #[error("`{key}` maps to the accessibility verb {verb}, but this element does not accept it (it supports {available}). {verb} usually lives on the window or the dialog's default button, not on an inner control — target that instead, or start the server with --allow-hid to send a real key event")]
    KeyVerbUnsupported {
        key: String,
        verb: &'static str,
        available: String,
    },

    #[error("{0}")]
    Hid(String),

    /// The native worker thread died. Unrecoverable for the process.
    #[error("the native worker thread is gone")]
    WorkerGone,
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
#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// By snapshot index, optionally pinned to the snapshot it came from.
    Index {
        index: usize,
        snapshot_id: Option<u64>,
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
    pub ui_changed: bool,
    /// Which mechanism carried the action.
    ///
    /// Present on every result, not just the interesting ones, so an agent never
    /// has to infer it from the absence of a field.
    pub delivery: Delivery,
}

impl ActionResult {
    /// An action that went through the accessibility API, which is all of them
    /// unless `--allow-hid` was passed.
    fn ax(verb: impl Into<String>, target: String, ui_changed: bool) -> Self {
        Self {
            verb: verb.into(),
            target,
            ui_changed,
            delivery: Delivery::Ax,
        }
    }
}

/// How an action reached the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Addressed to a specific UI element. Did not move the cursor, change
    /// focus, or activate the app.
    Ax,
    /// Written to the session's shared HID event stream. Behaves exactly as if
    /// the user pressed the keys: it goes to whatever has focus, and it competes
    /// with the human at the keyboard.
    Hid,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Ax => "ax",
            Delivery::Hid => "hid",
        }
    }
}

/// Whether this process may synthesize HID input.
///
/// A process-global switch set once from the command line, rather than a
/// per-call argument. If it were per-call, an agent could grant itself the
/// capability by passing a flag — the point is that a *human* decides, at launch,
/// whether this server is ever allowed to touch the shared cursor.
static ALLOW_HID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable the HID fallback. Call once, from argument parsing, before serving.
pub fn allow_hid(allow: bool) {
    ALLOW_HID.store(allow, Ordering::Relaxed);
}

pub fn hid_allowed() -> bool {
    ALLOW_HID.load(Ordering::Relaxed)
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
        Self { tx }
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
                let _ = tx.send(f(inner));
            }))
            .map_err(|_| CoreError::WorkerGone)?;
        rx.recv().map_err(|_| CoreError::WorkerGone)
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
    pub fn click(&self, app: &str, target: Target) -> Result<ActionResult> {
        let app = app.to_string();
        self.exec(move |inner| inner.click(&app, target))?
    }

    /// Replace a text element's contents.
    pub fn set_value(&self, app: &str, target: Target, value: &str) -> Result<ActionResult> {
        let app = app.to_string();
        let value = value.to_string();
        self.exec(move |inner| inner.set_value(&app, target, &value))?
    }

    /// Scroll a scrollable element by whole pages.
    pub fn scroll(
        &self,
        app: &str,
        target: Target,
        dir: ScrollDir,
        pages: u32,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        self.exec(move |inner| inner.scroll(&app, target, dir, pages))?
    }

    /// Append text to an element, preferring insertion over replacement.
    pub fn type_text(&self, app: &str, target: Target, text: &str) -> Result<ActionResult> {
        let app = app.to_string();
        let text = text.to_string();
        self.exec(move |inner| inner.type_text(&app, target, &text))?
    }

    /// Select a literal substring inside an element's text.
    pub fn select_text(
        &self,
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
    ) -> Result<ActionResult> {
        let app = app.to_string();
        let text = text.to_string();
        self.exec(move |inner| {
            inner.select_text(&app, target, &text, prefix.as_deref(), suffix.as_deref())
        })?
    }

    /// Press a key, through AX when the key has a verb and HID otherwise.
    pub fn press_key(&self, app: &str, target: Target, key: &str) -> Result<ActionResult> {
        let app = app.to_string();
        let key = key.to_string();
        self.exec(move |inner| inner.press_key(&app, target, &key))?
    }

    /// Deliver an arbitrary AX action by name.
    pub fn perform_action(&self, app: &str, target: Target, action: &str) -> Result<ActionResult> {
        let app = app.to_string();
        let action = action.to_string();
        self.exec(move |inner| inner.perform_action(&app, target, &action))?
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
                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                node.element.clone()
            }
        };

        let nodes = root.snapshot_tree(opts.limits);

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
        }

        // Match the AX window to a ScreenCaptureKit window by pid + frame.
        // The direct route would be `_AXUIElementGetWindow`, which is a private
        // symbol; matching on public API keeps this crate free of SPI and thus
        // free of the "breaks on the next macOS release" risk.
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
    fn resolve(&self, query: &str, target: Target) -> Result<(AppInfo, Element, String)> {
        let info = apps::resolve_app(query)?;
        match target {
            Target::Index { index, snapshot_id } => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;

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

    fn click(&mut self, query: &str, target: Target) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);
        let verb = el.activate()?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax(verb, desc, changed))
    }

    fn set_value(&mut self, query: &str, target: Target, value: &str) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);
        el.set_string(cua_ax::attr::VALUE, value)?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax("AXValue=", desc, changed))
    }

    fn type_text(&mut self, query: &str, target: Target, text: &str) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);
        let write = el.append_text(text)?;
        let changed = self.changed_since(info.pid, before);
        // Name the mechanism, not just the intent. "typed" would imply
        // keystrokes were synthesized, which is exactly what did not happen.
        Ok(ActionResult::ax(
            format!("AXSelectedText+ ({})", write.as_str()),
            desc,
            changed,
        ))
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
        let (_, el, desc) = self.resolve(query, target)?;
        let range = el.select_text(text, prefix, suffix)?;
        // Selecting text changes no window state the fingerprint can see, and
        // claiming otherwise would be noise. The returned range is the evidence
        // that it worked.
        Ok(ActionResult::ax(
            format!(
                "AXSelectedTextRange={{offset:{},length:{}}}",
                range.offset, range.length
            ),
            desc,
            false,
        ))
    }

    fn press_key(&mut self, query: &str, target: Target, key: &str) -> Result<ActionResult> {
        // Capability first, before anything that touches AX or needs a snapshot.
        //
        // Ordering matters for the message the caller sees. Resolving the target
        // first meant a chord asked for without `--allow-hid` reported "no
        // snapshot for this app" — a state problem the caller would then try to
        // fix, when the real answer is that this server is not permitted to send
        // that key at all and no amount of snapshotting will change it.
        let ax_verb = ax_verb_for_key(key);
        if ax_verb.is_none() && !hid_allowed() {
            return Err(CoreError::HidRefused {
                key: key.to_string(),
            });
        }

        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);

        // AX first, for the handful of keys that have a semantic verb. This path
        // keeps the guarantee: no cursor, no focus change, and it works on a
        // background window.
        if let Some(verb) = ax_verb {
            let available = el.actions();
            if available.iter().any(|a| a == verb) {
                el.perform(verb)?;
                let changed = self.changed_since(info.pid, before);
                return Ok(ActionResult::ax(
                    format!("{verb} (for {key})"),
                    desc,
                    changed,
                ));
            }
            // The key *does* have an AX verb; this element just does not accept
            // it. Reporting the generic "no accessibility verb" message here
            // would contradict itself, since that message names `escape` as
            // something that works without HID. Say what actually went wrong,
            // and where the verb usually lives.
            if !hid_allowed() {
                return Err(CoreError::KeyVerbUnsupported {
                    key: key.to_string(),
                    verb,
                    available: format!("{available:?}"),
                });
            }
        }

        // Reaching here means either the key has no AX verb and HID is permitted
        // (checked at the top), or it had a verb the element would not accept and
        // HID is permitted (checked in the branch above).
        let chord = cua_hid::parse_chord(key).map_err(|e| CoreError::Hid(e.to_string()))?;
        cua_hid::post_chord(chord).map_err(|e| CoreError::Hid(e.to_string()))?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!("HID key {key}"),
            // The target is informational only here: a HID event goes to whatever
            // has focus, not to the element the caller named. Say so rather than
            // implying the element received it.
            target: format!("{desc} — NOTE: delivered to the focused app, not this element"),
            ui_changed: changed,
            delivery: Delivery::Hid,
        })
    }

    fn perform_action(
        &mut self,
        query: &str,
        target: Target,
        action: &str,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
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
        Ok(ActionResult::ax(action, desc, changed))
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
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);
        let verb = dir.verb();
        for _ in 0..pages.max(1) {
            el.perform(verb)?;
        }
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax(verb, desc, changed))
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
        Some(format!(
            "{}|{}|{}",
            title.unwrap_or_default(),
            focused.as_ref().and_then(|f| f.role()).unwrap_or_default(),
            focused
                .as_ref()
                .and_then(|f| f.string(cua_ax::attr::TITLE))
                .unwrap_or_default()
        ))
    }

    fn changed_since(&self, pid: libc::pid_t, before: Option<String>) -> bool {
        // A short settle window: AX reflects most changes within a frame or two,
        // and waiting longer would add latency to every single action.
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.window_fingerprint(pid) != before
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
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
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
        // The whole reason cua-hid exists: AX cannot express these.
        assert_eq!(ax_verb_for_key("cmd+shift+p"), None);
        assert_eq!(ax_verb_for_key("a"), None);
        assert_eq!(ax_verb_for_key("f5"), None);
    }

    #[test]
    fn hid_is_off_until_a_human_turns_it_on() {
        // The default matters: a server started without the flag must refuse.
        assert!(!hid_allowed(), "HID must be opt-in, never the default");
        allow_hid(true);
        assert!(hid_allowed());
        allow_hid(false);
        assert!(!hid_allowed());
    }

    #[test]
    fn delivery_labels_are_stable() {
        assert_eq!(Delivery::Ax.as_str(), "ax");
        assert_eq!(Delivery::Hid.as_str(), "hid");
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
    fn refusing_hid_explains_the_ax_alternatives() {
        let msg = CoreError::HidRefused {
            key: "cmd+shift+p".into(),
        }
        .to_string();
        assert!(msg.contains("--allow-hid"), "must name the flag: {msg}");
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
