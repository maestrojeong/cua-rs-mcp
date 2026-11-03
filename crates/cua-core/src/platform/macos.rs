//! The macOS backend's side of [`PlatformAdapter`]: a one-for-one forwarding
//! of every trait method to the [`Cua`] handle and [`Op`] constructors that
//! were the server's direct API before the platform split.
//!
//! Nothing here decides anything. Each method is a single call into the code
//! that already ran for that tool, so the trait adds a seam without adding a
//! second implementation that could drift from the first — the gates, the
//! worker hop, the drawn-cursor mark and the batch-as-one-job behavior are all
//! still exactly where `session` put them.

use super::PlatformAdapter;
use crate::apps::AppInfo;
use crate::menubar::MenuListing;
use crate::notifications::{DismissAllOutcome, NotificationActed, NotificationListing};
use crate::session::{
    ActionResult, AppState, ClipboardRead, ClipboardWrite, CloseOutcome, Cua, FindResult,
    LaunchOutcome, Mechanism, MouseOptions, Op, Permissions, PointerLocation, Presence, Result,
    ScrollAmount, ScrollDir, StateOptions, Target, WaitOutcome, WindowListing, WindowOp,
    WindowOutcome, WindowPixel, WindowSelector,
};
use crate::{DisplayInfo, Modifiers};

impl PlatformAdapter for Cua {
    type Op<T: Send + 'static> = Op<T>;

    fn new() -> Self {
        Cua::new()
    }

    fn run<T: Send + 'static>(&self, op: Op<T>) -> Result<T> {
        Cua::run(self, op)
    }

    fn run_batch<T, S>(&self, ops: Vec<Op<T>>, stop_after: S) -> Result<Vec<Result<T>>>
    where
        T: Send + 'static,
        S: Fn(&Result<T>) -> bool + Send + 'static,
    {
        Cua::run_batch(self, ops, stop_after)
    }

    fn ready<T: Send + 'static>(value: T) -> Op<T> {
        Op::ready(value)
    }

    fn map<T, U, F>(op: Op<T>, f: F) -> Op<U>
    where
        T: Send + 'static,
        U: Send + 'static,
        F: FnOnce(Result<T>) -> U + Send + 'static,
    {
        op.map(f)
    }

    fn op_permissions() -> Op<Permissions> {
        Op::permissions()
    }

    fn op_list_apps() -> Op<Vec<AppInfo>> {
        Op::list_apps()
    }

    fn op_get_app_state(app: &str, opts: StateOptions) -> Op<AppState> {
        Op::get_app_state(app, opts)
    }

    fn op_find(app: &str, needle: &str, limit: usize) -> Op<FindResult> {
        Op::find(app, needle, limit)
    }

    fn op_wait_for(app: &str, needle: &str, want: Presence, timeout_ms: u64) -> Op<WaitOutcome> {
        Op::wait_for(app, needle, want, timeout_ms)
    }

    fn op_menu_bar(app: &str, path: &str) -> Op<MenuListing> {
        Op::menu_bar(app, path)
    }

    fn op_click(
        app: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        Op::click(app, target, mouse, return_state, confirm_destructive)
    }

    fn op_drag(
        app: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::drag(app, from, to, mouse, snapshot_id, return_state)
    }

    fn op_hover(
        app: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::hover(app, at, modifiers, snapshot_id, return_state)
    }

    fn op_click_in_window(
        app: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::click_in_window(app, at, mouse, return_state)
    }

    fn op_set_value(
        app: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::set_value(app, target, value, return_state)
    }

    fn op_scroll(
        app: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::scroll(app, target, dir, amount, return_state)
    }

    fn op_type_text(
        app: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::type_text(app, target, text, mechanism, return_state)
    }

    fn op_select_text(
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
        return_state: bool,
    ) -> Op<ActionResult> {
        Op::select_text(app, target, text, prefix, suffix, return_state)
    }

    fn op_press_key(
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        Op::press_key(app, target, key, return_state, confirm_destructive)
    }

    fn op_perform_action(
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        Op::perform_action(app, target, action, return_state, confirm_destructive)
    }

    fn op_press_menu_bar(
        app: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        Op::press_menu_bar(app, path, return_state, confirm_destructive)
    }

    fn permissions(&self) -> Result<Permissions> {
        Cua::permissions(self)
    }

    fn clipboard_get(&self) -> Result<ClipboardRead> {
        Cua::clipboard_get(self)
    }

    fn clipboard_set(&self, text: &str) -> Result<ClipboardWrite> {
        Cua::clipboard_set(self, text)
    }

    fn clipboard_clear(&self) -> Result<ClipboardWrite> {
        Cua::clipboard_clear(self)
    }

    fn list_windows(&self, app: &str) -> Result<WindowListing> {
        Cua::list_windows(self, app)
    }

    fn window_action(
        &self,
        app: &str,
        window: WindowSelector,
        op: WindowOp,
    ) -> Result<WindowOutcome> {
        Cua::window_action(self, app, window, op)
    }

    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        Cua::list_displays(self)
    }

    fn launch_app(&self, spec: &str, activate: bool) -> Result<LaunchOutcome> {
        Cua::launch_app(self, spec, activate)
    }

    fn close_app(&self, app: &str, confirm_destructive: bool) -> Result<CloseOutcome> {
        Cua::close_app(self, app, confirm_destructive)
    }

    fn list_notifications(&self, include_history: bool) -> Result<NotificationListing> {
        Cua::list_notifications(self, include_history)
    }

    fn dismiss_notification(&self, id: &str, include_history: bool) -> Result<NotificationActed> {
        Cua::dismiss_notification(self, id, include_history)
    }

    fn dismiss_all_notifications(&self, include_history: bool) -> Result<DismissAllOutcome> {
        Cua::dismiss_all_notifications(self, include_history)
    }

    fn notification_action(
        &self,
        id: &str,
        action: &str,
        include_history: bool,
        confirm_destructive: bool,
    ) -> Result<NotificationActed> {
        Cua::notification_action(self, id, action, include_history, confirm_destructive)
    }
}
