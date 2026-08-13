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
}

impl Default for StateOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            render: crate::snapshot::RenderOptions::default(),
            include_screenshot: true,
            max_image_dim: 1400,
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
        if self.enabled.insert(ProcessKey::for_pid(info.pid)) {
            app_el.enable_rich_accessibility();
            // The tree is built asynchronously, so reading immediately would
            // return the same empty window we are trying to fix.
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

        let nodes = window_el.snapshot_tree(opts.limits);
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
        Ok(ActionResult {
            verb: verb.to_string(),
            target: desc,
            ui_changed: self.changed_since(info.pid, before),
        })
    }

    fn set_value(&mut self, query: &str, target: Target, value: &str) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, target)?;
        let before = self.window_fingerprint(info.pid);
        el.set_string(cua_ax::attr::VALUE, value)?;
        Ok(ActionResult {
            verb: "AXValue=".to_string(),
            target: desc,
            ui_changed: self.changed_since(info.pid, before),
        })
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
        Ok(ActionResult {
            verb: verb.to_string(),
            target: desc,
            ui_changed: self.changed_since(info.pid, before),
        })
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
