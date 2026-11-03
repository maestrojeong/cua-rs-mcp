//! The seam between the MCP surface and one operating system's accessibility
//! stack.
//!
//! Everything above this trait — the tool schemas in `cua-mcp`, the result
//! types and their rendering, [`crate::outcome`]'s error classification, the
//! safety policy in [`crate::safety`] — is the same on every platform. Every
//! line below it is one platform's: how a tree is walked, how an element is
//! addressed, how input reaches a process, what a window is. DESIGN.md §15
//! describes the split and why it is shaped this way; the short version is
//! here.
//!
//! # Prepared operations, not just methods
//!
//! A backend is a handle to **one native thread** (DESIGN.md §4). Native
//! accessibility handles — an `AXUIElement` on macOS, an AT-SPI object proxy
//! on Linux — live on that thread and never leave it, and calls are serialized
//! through it so no two actions interleave.
//!
//! `batch` extends that serialization across several steps: the whole batch is
//! ONE job on the native thread, so nothing another caller sends can land
//! between two of its steps. That is only possible if a step can be *prepared*
//! on the calling thread and *run* later on the native one, which is what
//! [`PlatformAdapter::Op`] is. Each backend picks its own representation — on
//! macOS it is the closure over the worker's state that `session::Op` has
//! always been — and the trait only fixes how one is built
//! (`op_*`), transformed ([`PlatformAdapter::map`]) and run
//! ([`PlatformAdapter::run`], [`PlatformAdapter::run_batch`]).
//!
//! Operations that are never batched (clipboard, windows, lifecycle,
//! notifications, displays) are plain blocking methods.
//!
//! # Blocking on purpose
//!
//! Every method blocks until the native thread answers. The MCP layer moves
//! each call onto tokio's blocking pool; a backend never needs an async
//! runtime of its own in the caller's context, and one that uses async I/O
//! internally (AT-SPI is D-Bus) keeps that runtime on its own thread.
//!
//! # What a backend may not do
//!
//! - **Panic across this boundary.** A native panic is caught on the native
//!   thread and reported as [`CoreError::NativePanic`]; the server stays up.
//! - **Silently no-op.** A capability a platform cannot provide answers
//!   [`CoreError::PlatformUnsupported`], which [`crate::outcome`] classifies as
//!   `PLATFORM_NOT_SUPPORTED` / `not_delivered`. A tool that reports success
//!   while doing nothing is the one failure an agent cannot detect.
//! - **Skip a safety gate.** Gates run through [`crate::safety`]'s shared
//!   policy, with the platform supplying only the facts it alone can read
//!   (is the session locked, is a human typing) — see
//!   [`crate::safety::SessionProbe`].

use crate::apps::AppInfo;
use crate::menubar::MenuListing;
use crate::notifications::{DismissAllOutcome, NotificationActed, NotificationListing};
#[cfg(doc)]
use crate::session::CoreError;
use crate::session::{
    ActionResult, AppState, ClipboardRead, ClipboardWrite, CloseOutcome, FindResult, LaunchOutcome,
    Mechanism, MouseOptions, Permissions, PointerLocation, Presence, Result, ScrollAmount,
    ScrollDir, StateOptions, Target, WaitOutcome, WindowListing, WindowOp, WindowOutcome,
    WindowPixel, WindowSelector,
};

pub mod leaf;
#[cfg(any(not(target_os = "macos"), test))]
pub mod portable;

#[cfg(target_os = "macos")]
mod macos;

pub use leaf::{CGPoint, CGRect, CGSize, DisplayInfo, Limits, Modifiers, MouseButton};

/// The backend this build was compiled for, as an MCP client should read it.
#[cfg(target_os = "macos")]
pub const PLATFORM: &str = "macos";
#[cfg(target_os = "linux")]
pub const PLATFORM: &str = "linux";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub const PLATFORM: &str = std::env::consts::OS;

/// One operating system's implementation of every MCP tool.
///
/// See the module docs for the shape. Method names and arguments mirror the
/// tools they back one for one, so a reader can go from a tool in `cua-mcp`'s
/// `server.rs` to its platform implementation by name.
pub trait PlatformAdapter: Clone + Send + Sync + 'static {
    /// One prepared native operation. Built on the calling thread by an
    /// `op_*` constructor (which is where per-call safety context is
    /// assembled), run on the native thread by [`Self::run`] or
    /// [`Self::run_batch`].
    type Op<T: Send + 'static>: Send + 'static;

    /// Start the backend: spawn its native thread and whatever it owns.
    fn new() -> Self;

    // ── running prepared operations ────────────────────────────────────────

    /// Run one prepared operation: one native-thread job, one reply.
    fn run<T: Send + 'static>(&self, op: Self::Op<T>) -> Result<T>;

    /// Run several prepared operations, in order, as ONE native-thread job.
    ///
    /// `stop_after` sees each outcome as it lands; `true` drops the rest
    /// unrun. The result has one entry per operation that actually ran. A
    /// panic in step `k` is step `k`'s [`CoreError::NativePanic`], not the
    /// loss of the steps before it.
    fn run_batch<T, S>(&self, ops: Vec<Self::Op<T>>, stop_after: S) -> Result<Vec<Result<T>>>
    where
        T: Send + 'static,
        S: Fn(&Result<T>) -> bool + Send + 'static;

    /// An operation whose answer is already known (a batch step whose
    /// arguments were refused before anything native was attempted).
    fn ready<T: Send + 'static>(value: T) -> Self::Op<T>;

    /// Transform an operation's outcome on the native thread, immediately
    /// after it runs. Errors are handed to `f` too.
    fn map<T, U, F>(op: Self::Op<T>, f: F) -> Self::Op<U>
    where
        T: Send + 'static,
        U: Send + 'static,
        F: FnOnce(Result<T>) -> U + Send + 'static;

    // ── observation (batchable) ────────────────────────────────────────────

    /// Whether the grants this backend needs are in place. Never prompts.
    fn op_permissions() -> Self::Op<Permissions>;
    /// Running applications, frontmost first.
    fn op_list_apps() -> Self::Op<Vec<AppInfo>>;
    /// Walk one app's window into a numbered snapshot and, optionally,
    /// capture it.
    fn op_get_app_state(app: &str, opts: StateOptions) -> Self::Op<AppState>;
    /// Search the app's current snapshot.
    fn op_find(app: &str, needle: &str, limit: usize) -> Self::Op<FindResult>;
    /// Poll fresh snapshots until `needle` appears or disappears.
    fn op_wait_for(
        app: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Self::Op<WaitOutcome>;
    /// Read one level of the app's menu bar.
    fn op_menu_bar(app: &str, path: &str) -> Self::Op<MenuListing>;

    // ── actions (batchable, gated) ─────────────────────────────────────────

    fn op_click(
        app: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self::Op<ActionResult>;
    fn op_drag(
        app: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_hover(
        app: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_click_in_window(
        app: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_set_value(
        app: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_scroll(
        app: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_type_text(
        app: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_select_text(
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
        return_state: bool,
    ) -> Self::Op<ActionResult>;
    fn op_press_key(
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self::Op<ActionResult>;
    fn op_perform_action(
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self::Op<ActionResult>;
    fn op_press_menu_bar(
        app: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Self::Op<ActionResult>;

    // ── direct calls (never batched) ───────────────────────────────────────

    fn permissions(&self) -> Result<Permissions> {
        self.run(Self::op_permissions())
    }

    fn clipboard_get(&self) -> Result<ClipboardRead>;
    fn clipboard_set(&self, text: &str) -> Result<ClipboardWrite>;
    fn clipboard_clear(&self) -> Result<ClipboardWrite>;

    fn list_windows(&self, app: &str) -> Result<WindowListing>;
    fn window_action(
        &self,
        app: &str,
        window: WindowSelector,
        op: WindowOp,
    ) -> Result<WindowOutcome>;
    fn list_displays(&self) -> Result<Vec<DisplayInfo>>;

    fn launch_app(&self, spec: &str, activate: bool) -> Result<LaunchOutcome>;
    fn close_app(&self, app: &str, confirm_destructive: bool) -> Result<CloseOutcome>;

    fn list_notifications(&self, include_history: bool) -> Result<NotificationListing>;
    fn dismiss_notification(&self, id: &str, include_history: bool) -> Result<NotificationActed>;
    fn dismiss_all_notifications(&self, include_history: bool) -> Result<DismissAllOutcome>;
    fn notification_action(
        &self,
        id: &str,
        action: &str,
        include_history: bool,
        confirm_destructive: bool,
    ) -> Result<NotificationActed>;
}
