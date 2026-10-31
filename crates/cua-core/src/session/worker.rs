//! Native worker thread, process-lifetime caches, and the async-facing handle.

use super::*;

// ── worker ───────────────────────────────────────────────────────────────────

type Job = Box<dyn FnOnce(&mut Inner) + Send>;

#[derive(Default)]
pub(super) struct Inner {
    /// Latest snapshot per pid. Exactly one: keeping history would let an agent
    /// act on an arbitrarily old view of the UI, and the whole point of the
    /// generation check is to prevent that.
    pub(super) snapshots: HashMap<libc::pid_t, Snapshot>,
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
    pub(super) enabled: HashSet<ProcessKey>,
    /// What the poke actually achieved, per process lifetime.
    ///
    /// Kept because the write is unreliable and fails silently, so "the tree is
    /// empty" and "the app refused to build one" have to be told apart in the
    /// response rather than guessed at by the caller.
    pub(super) enablement: HashMap<ProcessKey, cua_ax::Enablement>,
    /// The listen-only input tap behind `CUA_YIELD_TO_HUMAN`, shared with
    /// [`Cua`] so the tap is torn down when the last handle goes away rather
    /// than leaking for the life of the process. Inert unless the flag is set.
    pub(super) human: std::sync::Arc<crate::safety::HumanWatch>,
}

/// Identifies a process incarnation, not just a pid slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ProcessKey {
    pub(super) pid: libc::pid_t,
    /// Kernel start time in microseconds since the epoch, or `0` when it could
    /// not be read. A `0` makes the key degrade to pid-only, which is the
    /// pre-existing behavior and still correct in the common case.
    pub(super) start_time: u64,
}

impl ProcessKey {
    pub(super) fn for_pid(pid: libc::pid_t) -> Self {
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
    /// Held only to keep the yield-to-human tap alive for as long as any handle
    /// to this server is. Dropping the last one stops the tap's thread.
    _human: std::sync::Arc<crate::safety::HumanWatch>,
}

/// Where the drawn cursor should go once an operation has run.
///
/// Computed on the worker, right after the action, from fields that are
/// crate-private on [`ActionResult`]; applied on the calling thread, because
/// the overlay is a pipe write that has no reason to queue behind AX work.
#[derive(Debug, Clone, Copy)]
struct Mark {
    x: f64,
    y: f64,
    clicking: bool,
    target: Option<(u32, libc::pid_t)>,
}

/// One native operation, prepared on the calling thread and run on the
/// worker.
///
/// Every public [`Cua`] method is `self.run(Op::that_method(..))`: the
/// constructor builds the safety gate and the closure that runs through
/// `Inner::acting`, exactly as the method used to inline. Splitting
/// construction from execution is what lets [`Cua::run_batch`] ship several of
/// these to the worker as ONE job — so a solo call and a batched step are the
/// same code, through the same gates, against the same snapshot store, and a
/// batch costs one thread hop rather than one per step.
///
/// A worker job cannot itself call a `Cua` method — that would enqueue a job
/// behind the one currently running and deadlock — which is why the prepared
/// form has to exist at all.
pub struct Op<T> {
    run: OpFn<T>,
}

/// The worker-side body of an [`Op`]: its outcome, and where the drawn cursor
/// should go because of it.
type OpFn<T> = Box<dyn FnOnce(&mut Inner) -> (Result<T>, Option<Mark>) + Send>;

impl<T: Send + 'static> Op<T> {
    /// An operation that moves no drawn cursor.
    pub(super) fn query<F>(f: F) -> Self
    where
        F: FnOnce(&mut Inner) -> Result<T> + Send + 'static,
    {
        Self {
            run: Box::new(move |inner| (f(inner), None)),
        }
    }

    /// An operation with nothing left to do on the worker: its answer is
    /// already known. The MCP layer uses it for a batch step whose arguments
    /// were refused before anything native was attempted, so that step still
    /// takes its place in the sequence.
    pub fn ready(value: T) -> Self {
        Self {
            run: Box::new(move |_| (Ok(value), None)),
        }
    }

    /// Transform the outcome, on the worker, immediately after the operation.
    ///
    /// Errors are handed to `f` too, so a caller can turn every outcome into
    /// one response type and let a batch decide whether to stop by looking at
    /// that.
    pub fn map<U, F>(self, f: F) -> Op<U>
    where
        U: Send + 'static,
        F: FnOnce(Result<T>) -> U + Send + 'static,
    {
        let run = self.run;
        Op {
            run: Box::new(move |inner| {
                let (res, mark) = run(inner);
                (Ok(f(res)), mark)
            }),
        }
    }
}

impl Op<ActionResult> {
    /// An action whose result may point the drawn cursor somewhere. This is
    /// what `exec_action` used to do around every action wrapper.
    fn action<F>(clicking: bool, f: F) -> Self
    where
        F: FnOnce(&mut Inner) -> Result<ActionResult> + Send + 'static,
    {
        Self {
            run: Box::new(move |inner| {
                let res = f(inner);
                let mark = res.as_ref().ok().and_then(|r| {
                    r.point.map(|(x, y)| Mark {
                        x,
                        y,
                        clicking,
                        target: r.overlay_target,
                    })
                });
                (res, mark)
            }),
        }
    }
}

impl Cua {
    /// Spawn the worker thread.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        // Started before the worker so the first action already knows whether
        // the tap is up, rather than racing it. A no-op unless
        // `CUA_YIELD_TO_HUMAN=1`.
        let human = std::sync::Arc::new(crate::safety::HumanWatch::start());
        let worker_human = human.clone();
        std::thread::Builder::new()
            .name("cua-native".into())
            // AX tree walks recurse and some apps are pathologically deep.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let mut inner = Inner {
                    human: worker_human,
                    ..Inner::default()
                };
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
            _human: human,
        }
    }

    /// Run `f` on the worker thread and wait for its result.
    pub(super) fn exec<T, F>(&self, f: F) -> Result<T>
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

    /// Point the drawn cursor wherever an action landed, if anywhere. A no-op
    /// for operations that resolved no point (a key event, a read) — the drawn
    /// cursor simply stays where it was.
    fn apply_mark(&self, mark: Option<Mark>) {
        if let Some(m) = mark {
            self.overlay.mark(m.x, m.y, m.clicking, m.target);
        }
    }

    /// Run one prepared operation on the worker: one job, one reply.
    pub fn run<T: Send + 'static>(&self, op: Op<T>) -> Result<T> {
        let run = op.run;
        let (res, mark) = self.exec(move |inner| run(inner))?;
        self.apply_mark(mark);
        res
    }

    /// Run several prepared operations, in order, as ONE worker job.
    ///
    /// The worker runs them back to back, so nothing another caller sends can
    /// interleave between two steps — the same serialization §4 of DESIGN.md
    /// gives a single call, extended across the batch — and the whole sequence
    /// costs one channel round trip instead of one per step.
    ///
    /// Each step sees the snapshot store exactly as the previous step left it:
    /// a step whose `snapshot_id` an earlier step superseded fails with the
    /// ordinary staleness error, because it goes through the ordinary check.
    ///
    /// `stop_after` is asked about each outcome as it lands; when it answers
    /// `true` the remaining operations are dropped unrun. The returned vector
    /// therefore has one entry per operation that actually ran, in order.
    ///
    /// Each step is isolated with its own `catch_unwind`, so a panic in step
    /// `k` becomes step `k`'s [`CoreError::NativePanic`] rather than losing
    /// the results of the steps before it.
    pub fn run_batch<T, S>(&self, ops: Vec<Op<T>>, stop_after: S) -> Result<Vec<Result<T>>>
    where
        T: Send + 'static,
        S: Fn(&Result<T>) -> bool + Send + 'static,
    {
        let ran = self.exec(move |inner| {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                let run = op.run;
                let (res, mark) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut *inner)))
                        .unwrap_or((Err(CoreError::NativePanic), None));
                let halt = stop_after(&res);
                out.push((res, mark));
                if halt {
                    break;
                }
            }
            out
        })?;
        Ok(ran
            .into_iter()
            .map(|(res, mark)| {
                self.apply_mark(mark);
                res
            })
            .collect())
    }

    // ── operations ───────────────────────────────────────────────────────
    //
    // Each is `run` over the matching `Op` constructor below; the constructor
    // carries the body, so a batched step is literally the same code.

    /// Running applications, frontmost first.
    pub fn list_apps(&self) -> Result<Vec<AppInfo>> {
        self.run(Op::list_apps())
    }

    /// Whether both grants this server needs are in place.
    pub fn permissions(&self) -> Result<Permissions> {
        self.run(Op::permissions())
    }

    /// Walk one app's front window and, unless told otherwise, capture it.
    ///
    /// This is the one call that must happen before any action: it produces the
    /// indices every action refers to.
    pub fn get_app_state(&self, app: &str, opts: StateOptions) -> Result<AppState> {
        self.run(Op::get_app_state(app, opts))
    }

    /// Activate an element the way a click would.
    ///
    /// `count` is the click count. `2` means the caller knows this target only
    /// responds to a real double-click, which no accessibility verb can
    /// express — see [`Inner::click`].
    ///
    /// `return_state` re-reads the window afterwards and attaches the result;
    /// see [`PostActionState`] for why that is worth a round trip.
    ///
    /// `confirm_destructive` clears the label gate in [`crate::safety`] for
    /// this one call. It is a parameter rather than a prompt because an MCP
    /// server has no channel to a human, and it is per-call rather than a mode
    /// because the point is to put the decision in the transcript.
    pub fn click(
        &self,
        app: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        self.run(Op::click(
            app,
            target,
            mouse,
            return_state,
            confirm_destructive,
        ))
    }

    /// Press at one point, move through interpolated intermediate points, and
    /// release at another — all inside one window.
    ///
    /// Either end may be an element or a bare window-local pixel, and they may
    /// be different elements of the same app. See [`Inner::drag`] for the gates
    /// and [`cua_hid::drag_path`] for why the moves are interpolated rather
    /// than jumped.
    pub fn drag(
        &self,
        app: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::drag(app, from, to, mouse, snapshot_id, return_state))
    }

    /// Tell one window the pointer moved to a point, so hover-only UI appears.
    ///
    /// The real pointer does not move; this is a synthesized `mouseMoved`
    /// event. See [`Inner::hover`] for what that cannot reach.
    pub fn hover(
        &self,
        app: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::hover(app, at, modifiers, snapshot_id, return_state))
    }

    /// Click a window-local point directly, with no element behind it.
    ///
    /// The deliberate opt-in for canvas-style targets. `x`/`y` are points from
    /// the window's top-left corner, and the result is labelled
    /// `pid (no element)` because nothing verified that anything is there. See
    /// [`Inner::click_in_window`] for the gates and for why this is not a
    /// fallback from [`Cua::click`].
    pub fn click_in_window(
        &self,
        app: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::click_in_window(app, at, mouse, return_state))
    }

    /// Replace a text element's contents.
    pub fn set_value(
        &self,
        app: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::set_value(app, target, value, return_state))
    }

    /// Scroll a scrollable element, by whole pages through accessibility or by
    /// a wheel event when the element advertises no scroll action.
    ///
    /// The tier is chosen by [`scroll_tier`] and named in the result.
    pub fn scroll(
        &self,
        app: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::scroll(app, target, dir, amount, return_state))
    }

    /// Append text to an element, preferring insertion over replacement.
    ///
    /// `mechanism` picks how: [`Mechanism::Ax`] (the default) writes the
    /// string through accessibility in one call, [`Mechanism::Keystrokes`]
    /// sends real per-character key events to the target pid for the targets
    /// that ignore `AXValue`. See [`Mechanism`] for why the choice is the
    /// caller's rather than automatic.
    pub fn type_text(
        &self,
        app: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Result<ActionResult> {
        self.run(Op::type_text(app, target, text, mechanism, return_state))
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
        self.run(Op::select_text(
            app,
            target,
            text,
            prefix,
            suffix,
            return_state,
        ))
    }

    /// Press a key, through AX when the key has a verb and HID otherwise.
    ///
    /// The key is classified as well as the element: `cmd+delete` is Move to
    /// Trash regardless of what the row it lands on is called.
    pub fn press_key(
        &self,
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        self.run(Op::press_key(
            app,
            target,
            key,
            return_state,
            confirm_destructive,
        ))
    }

    /// Deliver an arbitrary AX action by name.
    pub fn perform_action(
        &self,
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        self.run(Op::perform_action(
            app,
            target,
            action,
            return_state,
            confirm_destructive,
        ))
    }

    /// Read one level of the app's menu bar.
    ///
    /// `path` is `>`-separated titles — `""` for the top level, `"Edit"` for
    /// one menu, `"Edit > Transformations"` for a submenu. This is the one menu
    /// accessibility describes; see [`crate::menubar`] for why that matters and
    /// what it does not extend to.
    pub fn menu_bar(&self, app: &str, path: &str) -> Result<crate::menubar::MenuListing> {
        self.run(Op::menu_bar(app, path))
    }

    /// Press a menu bar row named by its path.
    ///
    /// The route to a row with no keyboard shortcut, when the row exists here.
    /// `AXPress` on an `AXMenuItem` runs the item's action without opening the
    /// menu, so nothing is drawn, the pointer does not move and the app is not
    /// activated — measured on TextEdit while another app was frontmost.
    pub fn press_menu_bar(
        &self,
        app: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        self.run(Op::press_menu_bar(
            app,
            path,
            return_state,
            confirm_destructive,
        ))
    }

    /// Elements whose label, value or role contains `needle`.
    ///
    /// Reads the existing snapshot when there is one, and takes a fresh one
    /// otherwise, so the caller does not have to sequence a `get_app_state`
    /// first just to search.
    pub fn find(&self, app: &str, needle: &str, limit: usize) -> Result<FindResult> {
        self.run(Op::find(app, needle, limit))
    }

    /// Poll until `needle` appears in (or disappears from) an app's tree.
    pub fn wait_for(
        &self,
        app: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        self.run(Op::wait_for(app, needle, want, timeout_ms))
    }
}

// ── prepared operations ──────────────────────────────────────────────────────
//
// The bodies the `Cua` methods above used to inline, unchanged. Each builds its
// safety gate here, on the calling thread, and defers everything that touches
// a native handle or the snapshot store to the worker.

impl Op<Vec<AppInfo>> {
    /// Prepared form of [`Cua::list_apps`].
    pub fn list_apps() -> Self {
        Op::query(|_| Ok(apps::list_apps()))
    }
}

impl Op<Permissions> {
    /// Prepared form of [`Cua::permissions`].
    pub fn permissions() -> Self {
        Op::query(|_| {
            Ok(Permissions {
                accessibility: cua_ax::is_trusted(),
                screen_recording: cua_capture::has_screen_recording_permission(),
            })
        })
    }
}

impl Op<AppState> {
    /// Prepared form of [`Cua::get_app_state`].
    pub fn get_app_state(app: &str, opts: StateOptions) -> Self {
        let app = app.to_string();
        Op::query(move |inner| inner.get_app_state(&app, opts))
    }
}

impl Op<crate::menubar::MenuListing> {
    /// Prepared form of [`Cua::menu_bar`].
    pub fn menu_bar(app: &str, path: &str) -> Self {
        let app = app.to_string();
        let path = path.to_string();
        Op::query(move |inner| inner.menu_bar(&app, &path))
    }
}

impl Op<FindResult> {
    /// Prepared form of [`Cua::find`].
    pub fn find(app: &str, needle: &str, limit: usize) -> Self {
        let app = app.to_string();
        let needle = needle.to_string();
        Op::query(move |inner| inner.find(&app, &needle, limit))
    }
}

impl Op<WaitOutcome> {
    /// Prepared form of [`Cua::wait_for`].
    pub fn wait_for(app: &str, needle: &str, want: Presence, timeout_ms: u64) -> Self {
        let app = app.to_string();
        let needle = needle.to_string();
        Op::query(move |inner| inner.wait_for(&app, &needle, want, timeout_ms))
    }
}

impl Op<ActionResult> {
    /// Prepared form of [`Cua::click`].
    pub fn click(
        app: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self {
        let app = app.to_string();
        let gate = crate::safety::Gate::at("click", &target).confirmed(confirm_destructive);
        Op::action(true, move |inner| {
            inner.acting(&app, gate, return_state, |i| i.click(&app, target, mouse))
        })
    }

    /// Prepared form of [`Cua::drag`].
    pub fn drag(
        app: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        // `elementless`, even when both ends resolve to elements: the
        // destructive-label heuristic classifies the thing being *pressed*, and
        // a drag presses nothing — its consequence lives in where it is
        // dropped, which no label describes. The app blocklist, the screen-lock
        // guard and the yield check all still apply, and they are the gates that
        // matter for a gesture.
        let gate = crate::safety::Gate::elementless("drag");
        Op::action(true, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.drag(&app, from.clone(), to.clone(), mouse, snapshot_id)
            })
        })
    }

    /// Prepared form of [`Cua::hover`].
    pub fn hover(
        app: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        // Hover presses nothing at all, so the label heuristic has nothing to
        // classify; the app blocklist, lock guard and yield check still run.
        let gate = crate::safety::Gate::elementless("hover");
        Op::action(true, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.hover(&app, at.clone(), modifiers, snapshot_id)
            })
        })
    }

    /// Prepared form of [`Cua::click_in_window`].
    pub fn click_in_window(
        app: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        // No element, so no label, so no destructive classification is possible
        // here at all — see `safety::Gate::elementless`. The other three gates
        // still apply.
        let gate = crate::safety::Gate::elementless("click_in_window");
        Op::action(true, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.click_in_window(&app, at, mouse)
            })
        })
    }

    /// Prepared form of [`Cua::set_value`].
    pub fn set_value(app: &str, target: Target, value: &str, return_state: bool) -> Self {
        let app = app.to_string();
        let value = value.to_string();
        let gate = crate::safety::Gate::at("set_value", &target);
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.set_value(&app, target, &value)
            })
        })
    }

    /// Prepared form of [`Cua::scroll`].
    pub fn scroll(
        app: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        let gate = crate::safety::Gate::at("scroll", &target);
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.scroll(&app, target, dir, amount)
            })
        })
    }

    /// Prepared form of [`Cua::type_text`].
    pub fn type_text(
        app: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        let text = text.to_string();
        let gate = crate::safety::Gate::at("type_text", &target);
        // `false` is the drawn cursor's "no click ring", the same as
        // `press_key`: keystrokes are not a click whichever mechanism carries
        // them.
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.type_text(&app, target, &text, mechanism)
            })
        })
    }

    /// Prepared form of [`Cua::select_text`].
    pub fn select_text(
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
        return_state: bool,
    ) -> Self {
        let app = app.to_string();
        let text = text.to_string();
        let gate = crate::safety::Gate::at("select_text", &target);
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.select_text(&app, target, &text, prefix.as_deref(), suffix.as_deref())
            })
        })
    }

    /// Prepared form of [`Cua::press_key`].
    pub fn press_key(
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self {
        let app = app.to_string();
        let key = key.to_string();
        let gate = crate::safety::Gate::at("press_key", &target)
            .with_key(&key)
            .confirmed(confirm_destructive);
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.press_key(&app, target, &key)
            })
        })
    }

    /// Prepared form of [`Cua::perform_action`].
    pub fn perform_action(
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self {
        let app = app.to_string();
        let action = action.to_string();
        let gate = crate::safety::Gate::at("perform_secondary_action", &target)
            .confirmed(confirm_destructive);
        Op::action(false, move |inner| {
            inner.acting(&app, gate, return_state, |i| {
                i.perform_action(&app, target, &action)
            })
        })
    }

    /// Prepared form of [`Cua::press_menu_bar`].
    pub fn press_menu_bar(
        app: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self {
        let app = app.to_string();
        let path = path.to_string();
        Op::action(false, move |inner| {
            inner.press_menu_bar(&app, &path, return_state, confirm_destructive)
        })
    }
}

impl Default for Cua {
    fn default() -> Self {
        Self::new()
    }
}
