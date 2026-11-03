//! `PlatformAdapter` for the Linux backend: each prepared operation is a
//! closure over the native thread's state, exactly the shape of macOS's `Op`,
//! so `batch` is one native-thread job here too.

use cua_core::menubar::MenuListing;
use cua_core::notifications::{DismissAllOutcome, NotificationActed, NotificationListing};
use cua_core::session::Result;
use cua_core::{
    ActionResult, AppInfo, AppState, ClipboardRead, ClipboardWrite, CloseOutcome, CoreError,
    DisplayInfo, FindResult, LaunchOutcome, Mechanism, Modifiers, MouseOptions, Permissions,
    PlatformAdapter, PointerLocation, Presence, ScrollAmount, ScrollDir, StateOptions, Target,
    WaitOutcome, WindowListing, WindowOp, WindowOutcome, WindowPixel, WindowSelector,
};

use crate::worker::{unsupported, LinuxCua, Op};

/// Why every notification tool answers `PlatformUnsupported` on Linux.
const NOTIFICATIONS_WHY: &str = "freedesktop notifications are delivered to the notification \
     daemon over org.freedesktop.Notifications and nothing else may read them: there is no \
     interface for a third party to list, dismiss or act on another app's notifications, and the \
     daemons (GNOME Shell, Plasma, dunst, mako) do not expose their history through AT-SPI \
     consistently enough to drive";

impl PlatformAdapter for LinuxCua {
    type Op<T: Send + 'static> = Op<T>;

    fn new() -> Self {
        LinuxCua::new()
    }

    fn run<T: Send + 'static>(&self, op: Op<T>) -> Result<T> {
        let run = op.run;
        self.exec(move |inner| run(inner))?
    }

    fn run_batch<T, S>(&self, ops: Vec<Op<T>>, stop_after: S) -> Result<Vec<Result<T>>>
    where
        T: Send + 'static,
        S: Fn(&Result<T>) -> bool + Send + 'static,
    {
        self.exec(move |inner| {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                let run = op.run;
                let res =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut *inner)))
                        .unwrap_or(Err(CoreError::NativePanic));
                let halt = stop_after(&res);
                out.push(res);
                if halt {
                    break;
                }
            }
            out
        })
    }

    fn ready<T: Send + 'static>(value: T) -> Op<T> {
        Op::new(move |_| Ok(value))
    }

    fn map<T, U, F>(op: Op<T>, f: F) -> Op<U>
    where
        T: Send + 'static,
        U: Send + 'static,
        F: FnOnce(Result<T>) -> U + Send + 'static,
    {
        let run = op.run;
        Op::new(move |inner| Ok(f(run(inner))))
    }

    fn op_permissions() -> Op<Permissions> {
        Op::new(|i| i.permissions())
    }

    fn op_list_apps() -> Op<Vec<AppInfo>> {
        Op::new(|i| i.list_apps())
    }

    fn op_get_app_state(app: &str, opts: StateOptions) -> Op<AppState> {
        let app = app.to_string();
        Op::new(move |i| i.get_app_state(&app, opts))
    }

    fn op_find(app: &str, needle: &str, limit: usize) -> Op<FindResult> {
        let (app, needle) = (app.to_string(), needle.to_string());
        Op::new(move |i| i.find(&app, &needle, limit))
    }

    fn op_wait_for(app: &str, needle: &str, want: Presence, timeout_ms: u64) -> Op<WaitOutcome> {
        let (app, needle) = (app.to_string(), needle.to_string());
        Op::new(move |i| i.wait_for(&app, &needle, want, timeout_ms))
    }

    fn op_menu_bar(app: &str, path: &str) -> Op<MenuListing> {
        let (app, path) = (app.to_string(), path.to_string());
        Op::new(move |i| i.menu_bar(&app, &path))
    }

    fn op_click(
        app: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        let app = app.to_string();
        Op::new(move |i| i.click(&app, target, mouse, return_state, confirm_destructive))
    }

    fn op_drag(
        app: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Op<ActionResult> {
        let app = app.to_string();
        Op::new(move |i| i.drag(&app, from, to, mouse, snapshot_id, return_state))
    }

    fn op_hover(
        app: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Op<ActionResult> {
        let app = app.to_string();
        Op::new(move |i| i.hover(&app, at, modifiers, snapshot_id, return_state))
    }

    fn op_click_in_window(
        app: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Op<ActionResult> {
        let app = app.to_string();
        Op::new(move |i| i.click_in_window(&app, at, mouse, return_state))
    }

    fn op_set_value(
        app: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Op<ActionResult> {
        let (app, value) = (app.to_string(), value.to_string());
        Op::new(move |i| i.set_value(&app, target, &value, return_state))
    }

    fn op_scroll(
        app: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Op<ActionResult> {
        let app = app.to_string();
        Op::new(move |i| i.scroll(&app, target, dir, amount, return_state))
    }

    fn op_type_text(
        app: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Op<ActionResult> {
        let (app, text) = (app.to_string(), text.to_string());
        Op::new(move |i| i.type_text(&app, target, &text, mechanism, return_state))
    }

    fn op_select_text(
        app: &str,
        target: Target,
        text: &str,
        prefix: Option<String>,
        suffix: Option<String>,
        return_state: bool,
    ) -> Op<ActionResult> {
        let (app, text) = (app.to_string(), text.to_string());
        Op::new(move |i| {
            i.select_text(
                &app,
                target,
                &text,
                prefix.as_deref(),
                suffix.as_deref(),
                return_state,
            )
        })
    }

    fn op_press_key(
        app: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        let (app, key) = (app.to_string(), key.to_string());
        Op::new(move |i| i.press_key(&app, target, &key, return_state, confirm_destructive))
    }

    fn op_perform_action(
        app: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        let (app, action) = (app.to_string(), action.to_string());
        Op::new(move |i| i.perform_action(&app, target, &action, return_state, confirm_destructive))
    }

    fn op_press_menu_bar(
        app: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Op<ActionResult> {
        let (app, path) = (app.to_string(), path.to_string());
        Op::new(move |i| i.press_menu_bar(&app, &path, return_state, confirm_destructive))
    }

    fn clipboard_get(&self) -> Result<ClipboardRead> {
        self.exec(|i| i.clipboard_get())?
    }

    fn clipboard_set(&self, text: &str) -> Result<ClipboardWrite> {
        let text = text.to_string();
        self.exec(move |i| i.clipboard_set(&text))?
    }

    fn clipboard_clear(&self) -> Result<ClipboardWrite> {
        self.exec(|i| i.clipboard_clear())?
    }

    fn list_windows(&self, app: &str) -> Result<WindowListing> {
        let app = app.to_string();
        self.exec(move |i| i.list_windows(&app))?
    }

    fn window_action(
        &self,
        app: &str,
        window: WindowSelector,
        op: WindowOp,
    ) -> Result<WindowOutcome> {
        let app = app.to_string();
        self.exec(move |i| i.window_action(&app, window, op))?
    }

    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        self.exec(|i| i.list_displays())?
    }

    fn launch_app(&self, spec: &str, activate: bool) -> Result<LaunchOutcome> {
        let spec = spec.to_string();
        self.exec(move |i| i.launch_app(&spec, activate))?
    }

    fn close_app(&self, app: &str, confirm_destructive: bool) -> Result<CloseOutcome> {
        let app = app.to_string();
        self.exec(move |i| i.close_app(&app, confirm_destructive))?
    }

    fn list_notifications(&self, _include_history: bool) -> Result<NotificationListing> {
        Err(unsupported("list_notifications", NOTIFICATIONS_WHY))
    }

    fn dismiss_notification(&self, _id: &str, _include_history: bool) -> Result<NotificationActed> {
        Err(unsupported("dismiss_notification", NOTIFICATIONS_WHY))
    }

    fn dismiss_all_notifications(&self, _include_history: bool) -> Result<DismissAllOutcome> {
        Err(unsupported("dismiss_all_notifications", NOTIFICATIONS_WHY))
    }

    fn notification_action(
        &self,
        _id: &str,
        _action: &str,
        _include_history: bool,
        _confirm_destructive: bool,
    ) -> Result<NotificationActed> {
        Err(unsupported("notification_action", NOTIFICATIONS_WHY))
    }
}
