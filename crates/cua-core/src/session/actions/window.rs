//! `Inner` methods for window and app-lifecycle responsibilities.
//!
//! Every window here is an `AXWindow` element, addressed and changed through
//! public accessibility attributes and actions — `AXRaise`, `AXPosition`,
//! `AXSize`, `AXMinimized`, the zoom button's `AXPress`. Nothing here posts an
//! event, moves the pointer, or calls `NSRunningApplication.activate`: raising a
//! window is a request to that app's window list, not a change of which app
//! owns the keyboard. DESIGN.md §13 has the reasoning.

use objc2_core_foundation::{CGPoint, CGSize};

use super::*;

// ── public types ─────────────────────────────────────────────────────────────

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

// ── policy, kept pure so it is testable without a grant ─────────────────────

/// How much of a window, in points per side, must stay on some display.
///
/// Not "any overlap": a window with one point on screen exists in the strict
/// sense and is unrecoverable in every practical one, since its title bar is
/// where a human would grab it. Forty points matches the smallest window the
/// rest of cua-rs considers addressable at all.
const MIN_VISIBLE: f64 = 40.0;

/// Largest width or height a resize may ask for. Far beyond any real display
/// arrangement; it exists to refuse a unit mistake (pixels × scale, twice)
/// before the app has to.
const MAX_WINDOW_SIDE: f64 = 100_000.0;

/// How many pre-maximize frames are remembered. Oldest dropped first.
const MAX_REMEMBERED_FRAMES: usize = 64;

/// How long a minimize, maximize or restore is given to animate before the
/// window is read back. The write itself is synchronous; the state it reports
/// can lag the animation.
const ANIMATION_SETTLE_MS: u64 = 250;

/// How long `close_app` waits for the process to exit before reporting that
/// the app is still running.
const QUIT_WAIT_MS: u64 = 3_000;
const QUIT_POLL_MS: u64 = 100;

fn intersection(a: &CGRect, b: &CGRect) -> Option<CGRect> {
    let x0 = a.origin.x.max(b.origin.x);
    let y0 = a.origin.y.max(b.origin.y);
    let x1 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (x1 > x0 && y1 > y0).then_some(CGRect {
        origin: CGPoint { x: x0, y: y0 },
        size: CGSize {
            width: x1 - x0,
            height: y1 - y0,
        },
    })
}

/// Whether enough of `frame` lands on at least one display to be recoverable.
///
/// One display at a time, not the union: a window straddling two displays
/// with 30 points on each is recoverable by a human, but a window whose only
/// visible part is split across a gap between displays is not, and only the
/// per-display test tells those apart without computing a union of rectangles.
/// The requirement shrinks for a window smaller than [`MIN_VISIBLE`], which is
/// visible as long as it is wholly on screen.
fn visible_on_some_display(frame: &CGRect, displays: &[CGRect]) -> bool {
    let need_w = MIN_VISIBLE.min(frame.size.width);
    let need_h = MIN_VISIBLE.min(frame.size.height);
    displays.iter().any(|d| {
        intersection(frame, d).is_some_and(|i| i.size.width >= need_w && i.size.height >= need_h)
    })
}

/// The display a window is mostly on, falling back to the first (main) one.
fn display_for(frame: &CGRect, displays: &[CGRect]) -> Option<CGRect> {
    let area = |r: &CGRect| r.size.width * r.size.height;
    displays
        .iter()
        .filter_map(|d| intersection(frame, d).map(|i| (area(&i), *d)))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, d)| d)
        .or_else(|| displays.first().copied())
}

/// Pair accessibility windows with window-server windows by frame.
///
/// The public-API substitute for `_AXUIElementGetWindow` (DESIGN.md §6): the
/// same frame-distance evidence `best_window_match` uses, applied to every
/// window at once. Greedy on distance, so the closest pair is fixed first and
/// neither side can be claimed twice — two AX windows can never report the same
/// id, which is what would let a `focus_window` by id raise the wrong one.
/// Anything farther apart than [`AX_WINDOW_MATCH_TOLERANCE`] stays unpaired:
/// a missing id is reported honestly, a guessed one is not.
fn pair_windows(ax: &[Option<CGRect>], server: &[CGRect]) -> Vec<Option<usize>> {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (i, a) in ax.iter().enumerate() {
        let Some(a) = a else { continue };
        for (j, s) in server.iter().enumerate() {
            let d = frame_distance(a, s);
            if d <= AX_WINDOW_MATCH_TOLERANCE {
                pairs.push((d, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = vec![None; ax.len()];
    let mut used = vec![false; server.len()];
    for (_, i, j) in pairs {
        if out[i].is_none() && !used[j] {
            out[i] = Some(j);
            used[j] = true;
        }
    }
    out
}

/// Remember a window's frame from before `maximize_window`, replacing any
/// older entry for the same window and evicting the oldest past the cap.
fn remember_frame<T: PartialEq>(store: &mut Vec<(T, CGRect)>, key: T, frame: CGRect) {
    store.retain(|(k, _)| *k != key);
    store.push((key, frame));
    if store.len() > MAX_REMEMBERED_FRAMES {
        store.remove(0);
    }
}

fn describe_frame(f: &CGRect) -> String {
    format!(
        "{:.0},{:.0} {:.0}x{:.0}",
        f.origin.x, f.origin.y, f.size.width, f.size.height
    )
}

fn describe_window(w: &WindowSummary) -> String {
    let title = w.title.as_deref().unwrap_or("(untitled)");
    match w.window_id {
        Some(id) => format!("window {title:?} (window_id={id})"),
        None => format!("window {title:?}"),
    }
}

/// Whether a window tool changed anything it can see.
fn observed_change(before: &WindowSummary, after: &WindowSummary) -> Observed {
    let frame_moved = match (before.frame, after.frame) {
        (Some(a), Some(b)) => frame_distance(&a, &b) > 0.5,
        (None, None) => false,
        _ => true,
    };
    if frame_moved
        || before.minimized != after.minimized
        || before.full_screen != after.full_screen
        || before.main != after.main
        || before.focused != after.focused
        || before.ax_index != after.ax_index
    {
        Observed::Changed
    } else {
        Observed::Unchanged
    }
}

/// Whether the process a [`ProcessKey`] names has exited.
///
/// `kill(pid, 0)` answers for the pid slot, and the start-time comparison
/// answers for the incarnation: a pid recycled into a new process in the
/// interval reads as gone, which it is. Read directly rather than through
/// `NSRunningApplication.isTerminated`, which is only updated by the main run
/// loop — and cua-rs's worker thread never runs one.
fn process_gone(key: ProcessKey) -> bool {
    if !apps::pid_alive(key.pid) {
        return true;
    }
    let now = ProcessKey::for_pid(key.pid);
    key.start_time != 0 && now.start_time != 0 && now.start_time != key.start_time
}

// ── worker-side implementation ───────────────────────────────────────────────

impl Inner {
    fn ax_windows(&self, info: &AppInfo) -> Result<Vec<Element>> {
        match Element::for_pid(info.pid).elements_checked(cua_ax::attr::WINDOWS) {
            Ok(windows) => Ok(windows),
            Err(cua_ax::AxError::CannotComplete) => Err(CoreError::WindowQueryTimedOut {
                app: info.name.clone(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// The window server's plausible windows for this pid, or why there are none.
    fn server_windows(pid: libc::pid_t) -> std::result::Result<Vec<WindowInfo>, String> {
        cua_capture::list_windows()
            .map(|list| {
                list.into_iter()
                    .filter(|w| w.pid == pid && w.is_plausible_target())
                    .collect()
            })
            .map_err(|e| e.to_string())
    }

    fn summarize(
        el: &Element,
        ax_index: usize,
        focused: Option<&Element>,
        server: Option<&WindowInfo>,
    ) -> WindowSummary {
        WindowSummary {
            ax_index: Some(ax_index),
            window_id: server.map(|w| w.id),
            title: el
                .string(cua_ax::attr::TITLE)
                .filter(|t| !t.is_empty())
                .or_else(|| server.and_then(|w| w.title.clone())),
            subrole: el.string(cua_ax::attr::SUBROLE),
            frame: el.frame().or_else(|| server.map(|w| w.frame)),
            level: server.map(|w| w.layer),
            minimized: el.bool(cua_ax::attr::MINIMIZED).unwrap_or(false),
            full_screen: el.bool(cua_ax::attr::FULL_SCREEN).unwrap_or(false),
            main: el.bool(cua_ax::attr::MAIN).unwrap_or(false),
            focused: focused.is_some_and(|f| f == el),
            on_screen: server.map(|w| w.on_screen),
            manageable: true,
        }
    }

    pub(super) fn list_windows(&mut self, query: &str) -> Result<WindowListing> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let windows = self.ax_windows(&info)?;
        let focused = Element::for_pid(info.pid).element(cua_ax::attr::FOCUSED_WINDOW);
        let mut warnings = Vec::new();

        let server = match Self::server_windows(info.pid) {
            Ok(list) => Some(list),
            Err(e) => {
                warnings.push(format!(
                    "no window_id or level for any window: {e}. The window tools still work — \
                     address a window by window_title, or omit both to act on the focused one"
                ));
                None
            }
        };

        let ax_frames: Vec<Option<CGRect>> = windows.iter().map(Element::frame).collect();
        let server_frames: Vec<CGRect> = server
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|w| w.frame)
            .collect();
        let pairing = pair_windows(&ax_frames, &server_frames);

        let mut out: Vec<WindowSummary> = windows
            .iter()
            .enumerate()
            .map(|(i, el)| {
                let paired = pairing[i].and_then(|j| server.as_ref().and_then(|s| s.get(j)));
                Self::summarize(el, i, focused.as_ref(), paired)
            })
            .collect();

        // Windows the window server has and accessibility does not list. Most
        // often they are on another Space, which DESIGN.md §9 keeps
        // observation-only; reporting them is what stops an agent concluding
        // the app has fewer windows than it does.
        if let Some(server) = &server {
            for (j, w) in server.iter().enumerate() {
                if pairing.contains(&Some(j)) {
                    continue;
                }
                out.push(WindowSummary {
                    ax_index: None,
                    window_id: Some(w.id),
                    title: w.title.clone(),
                    subrole: None,
                    frame: Some(w.frame),
                    level: Some(w.layer),
                    minimized: false,
                    full_screen: false,
                    main: false,
                    focused: false,
                    on_screen: Some(w.on_screen),
                    manageable: false,
                });
            }
        }

        Ok(WindowListing {
            app: info,
            windows: out,
            warnings,
        })
    }

    /// Resolve a [`WindowSelector`] to one AX window and its summary.
    fn select_window(
        &self,
        info: &AppInfo,
        sel: &WindowSelector,
    ) -> Result<(Element, WindowSummary)> {
        let windows = self.ax_windows(info)?;
        if windows.is_empty() {
            return Err(CoreError::NoWindow {
                app: info.name.clone(),
            });
        }
        let app_el = Element::for_pid(info.pid);
        let focused = app_el.element(cua_ax::attr::FOCUSED_WINDOW);
        let window_err = |reason: String| CoreError::Window {
            app: info.name.clone(),
            reason,
        };

        if let Some(wid) = sel.window_id {
            let server = Self::server_windows(info.pid).map_err(|e| {
                window_err(format!(
                    "window_id needs the window server's window list, which is unavailable: {e}. \
                     Pass window_title instead, or omit both to act on the focused window"
                ))
            })?;
            let Some(j) = server.iter().position(|w| w.id == wid) else {
                return Err(window_err(format!(
                    "window_id {wid} is not an open window of this app (it closed, or the id \
                     came from another app). Call list_windows again"
                )));
            };
            let ax_frames: Vec<Option<CGRect>> = windows.iter().map(Element::frame).collect();
            let frames: Vec<CGRect> = server.iter().map(|w| w.frame).collect();
            let pairing = pair_windows(&ax_frames, &frames);
            let Some(i) = pairing.iter().position(|p| *p == Some(j)) else {
                return Err(window_err(format!(
                    "window_id {wid} exists but accessibility lists no window with its frame, so \
                     it cannot be managed — it is most likely on another Space. Switch to that \
                     Space, or address the window by window_title if accessibility lists it"
                )));
            };
            let summary = Self::summarize(&windows[i], i, focused.as_ref(), server.get(j));
            return Ok((windows[i].clone(), summary));
        }

        let index = if let Some(title) = &sel.title {
            let hits: Vec<usize> = windows
                .iter()
                .enumerate()
                .filter(|(_, w)| w.string(cua_ax::attr::TITLE).as_deref() == Some(title.as_str()))
                .map(|(i, _)| i)
                .collect();
            match hits.as_slice() {
                [i] => *i,
                [] => {
                    let titles: Vec<String> = windows
                        .iter()
                        .map(|w| format!("{:?}", w.string(cua_ax::attr::TITLE).unwrap_or_default()))
                        .collect();
                    return Err(window_err(format!(
                        "no window titled {title:?}; this app's windows are {}. Titles match \
                         exactly",
                        titles.join(", ")
                    )));
                }
                _ => {
                    return Err(window_err(format!(
                        "{} windows are titled {title:?}. Pass window_id from list_windows to \
                         name one of them",
                        hits.len()
                    )))
                }
            }
        } else {
            let preferred = focused
                .clone()
                .or_else(|| app_el.element(cua_ax::attr::MAIN_WINDOW));
            preferred
                .and_then(|p| windows.iter().position(|w| *w == p))
                .unwrap_or(0)
        };

        // The id is filled in when the window server can vouch for it, so the
        // result can name the window the same way list_windows does.
        let server = Self::server_windows(info.pid).ok();
        let paired = server.as_ref().and_then(|server| {
            let ax_frames: Vec<Option<CGRect>> = windows.iter().map(Element::frame).collect();
            let frames: Vec<CGRect> = server.iter().map(|w| w.frame).collect();
            pair_windows(&ax_frames, &frames)[index].and_then(|j| server.get(j))
        });
        let summary = Self::summarize(&windows[index], index, focused.as_ref(), paired);
        Ok((windows[index].clone(), summary))
    }

    /// Re-read a window after an action, keeping the identity fields the
    /// selection established.
    fn reread(&self, info: &AppInfo, el: &Element, before: &WindowSummary) -> WindowSummary {
        let focused = Element::for_pid(info.pid).element(cua_ax::attr::FOCUSED_WINDOW);
        let index = self
            .ax_windows(info)
            .ok()
            .and_then(|ws| ws.iter().position(|w| w == el));
        let mut after = Self::summarize(el, index.unwrap_or(0), focused.as_ref(), None);
        after.ax_index = index;
        after.window_id = before.window_id;
        after.level = before.level;
        after.on_screen = before.on_screen;
        after
    }

    fn displays() -> Result<Vec<CGRect>> {
        let displays = cua_capture::list_displays()?;
        Ok(displays.into_iter().map(|d| d.frame).collect())
    }

    fn require_visible(info: &AppInfo, target: &CGRect) -> Result<()> {
        let displays = Self::displays()?;
        if visible_on_some_display(target, &displays) {
            return Ok(());
        }
        let listed: Vec<String> = displays.iter().map(describe_frame).collect();
        Err(CoreError::Window {
            app: info.name.clone(),
            reason: format!(
                "refusing to put the window at {}: less than {MIN_VISIBLE:.0} points of it would \
                 be on any display, and a window nobody can see or grab is not recoverable by the \
                 human. Active displays (global points): {}. Call list_displays and choose a frame \
                 inside one of them",
                describe_frame(target),
                if listed.is_empty() {
                    "none".to_string()
                } else {
                    listed.join("; ")
                }
            ),
        })
    }

    fn require_settable(info: &AppInfo, el: &Element, attr: &str, what: &str) -> Result<()> {
        if el.is_settable(attr) {
            Ok(())
        } else {
            Err(CoreError::Window {
                app: info.name.clone(),
                reason: format!(
                    "this window does not let {what} be changed ({attr} is not settable). Sheets, \
                     panels and some fixed-size windows refuse it"
                ),
            })
        }
    }

    fn set_frame(el: &Element, frame: &CGRect) -> Result<()> {
        // Position first, then size, then position again: a window moved to a
        // smaller display can be clamped by its old size, and one write of each
        // in the other order is clamped by the old position instead.
        el.set_point(cua_ax::attr::POSITION, frame.origin)?;
        el.set_size(cua_ax::attr::SIZE, frame.size)?;
        el.set_point(cua_ax::attr::POSITION, frame.origin)?;
        Ok(())
    }

    /// Run one window tool, behind the same gates every action passes.
    pub(super) fn window_action(
        &mut self,
        query: &str,
        sel: WindowSelector,
        op: WindowOp,
    ) -> Result<WindowOutcome> {
        cua_ax::require_trusted()?;
        let gate = crate::safety::Gate::elementless(op.verb());
        let mut states: Option<(WindowSummary, WindowSummary, Option<String>)> = None;
        let action = self.acting(query, gate, false, |i| {
            let (action, before, after, note) = i.apply_window_op(query, &sel, op)?;
            states = Some((before, after, note));
            Ok(action)
        })?;
        let (before, after, note) = states.ok_or(CoreError::NativePanic)?;
        Ok(WindowOutcome {
            action,
            before,
            after,
            note,
        })
    }

    fn apply_window_op(
        &mut self,
        query: &str,
        sel: &WindowSelector,
        op: WindowOp,
    ) -> Result<(ActionResult, WindowSummary, WindowSummary, Option<String>)> {
        let info = apps::resolve_app(query)?;
        let (el, before) = self.select_window(&info, sel)?;
        let window_err = |reason: String| CoreError::Window {
            app: info.name.clone(),
            reason,
        };
        let minimized_err = || {
            window_err(format!(
                "{} is minimized. Call restore_window first",
                describe_window(&before)
            ))
        };
        let current_frame = || {
            before.frame.ok_or_else(|| {
                window_err("this window publishes no position or size to start from".to_string())
            })
        };

        let mut note = None;
        let mut settle = false;
        let verb: String = match op {
            WindowOp::Focus => {
                if before.minimized {
                    return Err(minimized_err());
                }
                if !el.actions().iter().any(|a| a == cua_ax::action::RAISE) {
                    return Err(CoreError::Ax(cua_ax::AxError::Unsupported {
                        what: "action",
                        name: cua_ax::action::RAISE.to_string(),
                    }));
                }
                el.perform(cua_ax::action::RAISE)?;
                // Main-window status is the app's own bookkeeping, so it is
                // written as well when the app allows it: a raised window that
                // is not main still receives the app's menu commands second.
                // Best-effort, and said so when refused, because the raise is
                // the part that was asked for.
                if el.is_settable(cua_ax::attr::MAIN) {
                    if let Err(e) = el.set_bool(cua_ax::attr::MAIN, true) {
                        note = Some(format!("raised, but AXMain=true was refused: {e}"));
                    }
                }
                "AXRaise".into()
            }
            WindowOp::Move { x, y } => {
                if before.minimized {
                    return Err(minimized_err());
                }
                if !x.is_finite() || !y.is_finite() {
                    return Err(window_err("x and y must be finite numbers".into()));
                }
                let frame = current_frame()?;
                let target = CGRect {
                    origin: CGPoint { x, y },
                    size: frame.size,
                };
                Self::require_visible(&info, &target)?;
                Self::require_settable(&info, &el, cua_ax::attr::POSITION, "its position")?;
                el.set_point(cua_ax::attr::POSITION, target.origin)?;
                "AXPosition".into()
            }
            WindowOp::Resize { width, height } => {
                if before.minimized {
                    return Err(minimized_err());
                }
                let valid = |v: f64| v.is_finite() && (1.0..=MAX_WINDOW_SIDE).contains(&v);
                if !valid(width) || !valid(height) {
                    return Err(window_err(format!(
                        "width and height must be between 1 and {MAX_WINDOW_SIDE:.0} points, got \
                         {width}x{height}"
                    )));
                }
                let frame = current_frame()?;
                let target = CGRect {
                    origin: frame.origin,
                    size: CGSize { width, height },
                };
                Self::require_visible(&info, &target)?;
                Self::require_settable(&info, &el, cua_ax::attr::SIZE, "its size")?;
                el.set_size(cua_ax::attr::SIZE, target.size)?;
                "AXSize".into()
            }
            WindowOp::Minimize => {
                if before.minimized {
                    note = Some("already minimized; nothing was changed".into());
                    "none".into()
                } else {
                    Self::require_settable(&info, &el, cua_ax::attr::MINIMIZED, "minimization")?;
                    el.set_bool(cua_ax::attr::MINIMIZED, true)?;
                    settle = true;
                    "AXMinimized=true".into()
                }
            }
            WindowOp::Maximize => {
                if before.minimized {
                    return Err(minimized_err());
                }
                if before.full_screen {
                    return Err(window_err(format!(
                        "{} is in full screen, which is already as large as it gets. Call \
                         restore_window to leave full screen first",
                        describe_window(&before)
                    )));
                }
                let frame = current_frame()?;
                let remembered = self
                    .pre_maximize
                    .iter()
                    .find(|(k, _)| *k == el)
                    .map(|(_, f)| *f);
                if remembered.is_some_and(|f| frame_distance(&f, &frame) > 0.5) {
                    // Pressing zoom again would *un*-zoom: the button toggles.
                    note = Some(
                        "already maximized by maximize_window; nothing was changed (call \
                         restore_window to go back)"
                            .into(),
                    );
                    "none".into()
                } else {
                    remember_frame(&mut self.pre_maximize, el.clone(), frame);
                    settle = true;
                    let zoom = el
                        .element(cua_ax::attr::ZOOM_BUTTON)
                        .filter(|z| z.actions().iter().any(|a| a == cua_ax::action::PRESS));
                    match zoom {
                        Some(zoom) => {
                            zoom.perform(cua_ax::action::PRESS)?;
                            "AXPress on AXZoomButton".into()
                        }
                        None => {
                            // No zoom button — a panel, a utility window, an
                            // app that draws its own title bar. The fallback is
                            // the display's bounds, not its visible frame:
                            // `NSScreen.visibleFrame` is main-thread-only, and
                            // the AppKit apps that have a menu bar and Dock to
                            // avoid constrain their own windows below the menu
                            // bar. The readback reports what the app kept.
                            Self::require_settable(
                                &info,
                                &el,
                                cua_ax::attr::POSITION,
                                "its position",
                            )?;
                            Self::require_settable(&info, &el, cua_ax::attr::SIZE, "its size")?;
                            let target =
                                display_for(&frame, &Self::displays()?).ok_or_else(|| {
                                    window_err("there is no active display to fill".into())
                                })?;
                            Self::set_frame(&el, &target)?;
                            note = Some(
                                "this window has no zoom button, so it was sized to its display's \
                                 bounds instead"
                                    .into(),
                            );
                            "AXPosition+AXSize (no zoom button)".into()
                        }
                    }
                }
            }
            WindowOp::Restore => {
                if before.minimized {
                    Self::require_settable(&info, &el, cua_ax::attr::MINIMIZED, "minimization")?;
                    el.set_bool(cua_ax::attr::MINIMIZED, false)?;
                    settle = true;
                    "AXMinimized=false".into()
                } else if before.full_screen {
                    Self::require_settable(&info, &el, cua_ax::attr::FULL_SCREEN, "full screen")?;
                    el.set_bool(cua_ax::attr::FULL_SCREEN, false)?;
                    settle = true;
                    "AXFullScreen=false".into()
                } else if let Some(pos) = self.pre_maximize.iter().position(|(k, _)| *k == el) {
                    let (_, frame) = self.pre_maximize.remove(pos);
                    Self::set_frame(&el, &frame)?;
                    settle = true;
                    "AXPosition+AXSize (frame from before maximize_window)".into()
                } else {
                    note = Some(
                        "nothing to restore: the window is not minimized, not in full screen, and \
                         was not maximized by maximize_window in this server's lifetime, so there \
                         is no earlier frame to return to. Use move_window and resize_window to \
                         set one explicitly"
                            .into(),
                    );
                    "none".into()
                }
            }
        };

        if settle {
            std::thread::sleep(std::time::Duration::from_millis(ANIMATION_SETTLE_MS));
        }
        let after = self.reread(&info, &el, &before);
        let changed = observed_change(&before, &after);
        let action = ActionResult::ax_at(
            verb,
            describe_window(&before),
            Settled {
                changed,
                popups: Vec::new(),
            },
            None,
        );
        Ok((action, before, after, note))
    }

    pub(super) fn launch_app(&mut self, spec: &str, activate: bool) -> Result<LaunchOutcome> {
        let target = apps::locate_app(spec).map_err(|reason| CoreError::Lifecycle {
            app: spec.to_string(),
            reason,
        })?;
        // The app is not running, so there is no AppInfo to gate on yet. The
        // gates only read the bundle id and name — scope and forbidden list —
        // plus session-wide state, so a synthetic one is exactly as strict as
        // a real one would be. pid -1 never matches the frontmost app, which
        // is correct: nobody can be using an app that has not started.
        let pending = AppInfo {
            name: target.name.clone(),
            bundle_id: target.bundle_id.clone(),
            pid: -1,
            active: false,
            regular: true,
        };
        let gate = crate::safety::Gate::elementless("launch_app");
        crate::safety::guard(&pending, &self.human, &gate, None)?;

        if let Some(running) = target
            .bundle_id
            .as_deref()
            .and_then(apps::running_with_bundle_id)
        {
            return Ok(LaunchOutcome {
                frontmost: apps::frontmost_pid() == Some(running.pid),
                app: running,
                path: target.path,
                already_running: true,
                activate_requested: activate,
            });
        }

        let pid = apps::launch(&target, activate).map_err(|reason| CoreError::Lifecycle {
            app: target.name.clone(),
            reason,
        })?;
        let app = apps::app_for_pid(pid).unwrap_or(AppInfo { pid, ..pending });
        Ok(LaunchOutcome {
            frontmost: apps::frontmost_pid() == Some(pid),
            app,
            path: target.path,
            already_running: false,
            activate_requested: activate,
        })
    }

    pub(super) fn close_app(&mut self, query: &str, confirm: bool) -> Result<CloseOutcome> {
        let info = apps::resolve_app(query)?;
        let gate = crate::safety::Gate::elementless("close_app");
        let mut waited = None;
        self.acting(query, gate, false, |_| {
            // SAFETY: both calls only read this process's own ids.
            let (me, parent) = unsafe { (libc::getpid(), libc::getppid()) };
            crate::safety::close_app_policy(&info, confirm, me, parent)?;
            let key = ProcessKey::for_pid(info.pid);
            if !apps::request_quit(info.pid) {
                return Err(CoreError::Lifecycle {
                    app: info.name.clone(),
                    reason: "the quit request could not be delivered; the app may already have \
                             exited. Call list_apps"
                        .into(),
                });
            }
            let started = Instant::now();
            let mut quit = process_gone(key);
            while !quit && started.elapsed().as_millis() < u128::from(QUIT_WAIT_MS) {
                std::thread::sleep(std::time::Duration::from_millis(QUIT_POLL_MS));
                quit = process_gone(key);
            }
            waited = Some((quit, started.elapsed().as_millis() as u64));
            Ok(ActionResult::ax_at(
                "terminate (quit Apple event)",
                format!("app `{}` (pid {})", info.name, info.pid),
                Settled {
                    changed: if quit {
                        Observed::Changed
                    } else {
                        Observed::Unknown
                    },
                    popups: Vec::new(),
                },
                None,
            ))
        })?;
        let (quit, waited_ms) = waited.ok_or(CoreError::NativePanic)?;
        if quit {
            // Every native handle for that process is now meaningless.
            self.snapshots.remove(&info.pid);
            let pid = info.pid;
            self.pre_maximize
                .retain(|(el, _)| el.pid().is_ok_and(|p| p != pid));
        }
        Ok(CloseOutcome {
            app: info,
            quit,
            waited_ms,
        })
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect {
            origin: CGPoint { x, y },
            size: CGSize {
                width: w,
                height: h,
            },
        }
    }

    fn summary(frame: CGRect) -> WindowSummary {
        WindowSummary {
            ax_index: Some(0),
            window_id: Some(7),
            title: Some("Doc".into()),
            subrole: None,
            frame: Some(frame),
            level: Some(0),
            minimized: false,
            full_screen: false,
            main: true,
            focused: true,
            on_screen: Some(true),
            manageable: true,
        }
    }

    const MAIN: CGRect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: 1512.0,
            height: 982.0,
        },
    };
    const RIGHT: CGRect = CGRect {
        origin: CGPoint { x: 1512.0, y: 0.0 },
        size: CGSize {
            width: 1920.0,
            height: 1080.0,
        },
    };

    #[test]
    fn a_window_on_a_display_is_visible() {
        assert!(visible_on_some_display(
            &rect(100.0, 100.0, 800.0, 600.0),
            &[MAIN]
        ));
    }

    #[test]
    fn a_window_entirely_off_every_display_is_refused() {
        assert!(!visible_on_some_display(
            &rect(5000.0, 5000.0, 800.0, 600.0),
            &[MAIN, RIGHT]
        ));
        assert!(!visible_on_some_display(
            &rect(-900.0, 0.0, 800.0, 600.0),
            &[MAIN]
        ));
    }

    #[test]
    fn a_sliver_on_screen_is_not_enough() {
        // Ten points peeking in from the left edge: technically on screen,
        // practically lost.
        assert!(!visible_on_some_display(
            &rect(-790.0, 100.0, 800.0, 600.0),
            &[MAIN]
        ));
        assert!(visible_on_some_display(
            &rect(-700.0, 100.0, 800.0, 600.0),
            &[MAIN]
        ));
    }

    #[test]
    fn a_window_on_a_secondary_display_is_visible() {
        assert!(visible_on_some_display(
            &rect(2000.0, 200.0, 800.0, 600.0),
            &[MAIN, RIGHT]
        ));
        assert!(!visible_on_some_display(
            &rect(2000.0, 200.0, 800.0, 600.0),
            &[MAIN]
        ));
    }

    #[test]
    fn a_tiny_window_needs_only_to_be_wholly_on_screen() {
        assert!(visible_on_some_display(
            &rect(10.0, 10.0, 20.0, 20.0),
            &[MAIN]
        ));
    }

    #[test]
    fn no_display_means_nothing_is_visible() {
        assert!(!visible_on_some_display(&rect(0.0, 0.0, 800.0, 600.0), &[]));
    }

    #[test]
    fn the_display_a_window_is_mostly_on_wins() {
        let straddling = rect(1400.0, 100.0, 800.0, 600.0);
        assert_eq!(display_for(&straddling, &[MAIN, RIGHT]), Some(RIGHT));
        assert_eq!(
            display_for(&rect(9000.0, 0.0, 10.0, 10.0), &[MAIN, RIGHT]),
            Some(MAIN)
        );
        assert_eq!(display_for(&straddling, &[]), None);
    }

    #[test]
    fn windows_pair_by_frame_and_never_share_an_id() {
        let a = rect(0.0, 0.0, 800.0, 600.0);
        let b = rect(100.0, 100.0, 800.0, 600.0);
        let pairs = pair_windows(&[Some(a), Some(b)], &[b, a]);
        assert_eq!(pairs, vec![Some(1), Some(0)]);

        // Two AX windows with the same frame cannot both claim one server window.
        let pairs = pair_windows(&[Some(a), Some(a)], &[a]);
        assert_eq!(pairs.iter().filter(|p| p.is_some()).count(), 1);
    }

    #[test]
    fn a_distant_frame_stays_unpaired_rather_than_guessed() {
        let pairs = pair_windows(
            &[Some(rect(0.0, 0.0, 800.0, 600.0)), None],
            &[rect(50.0, 0.0, 800.0, 600.0)],
        );
        assert_eq!(pairs, vec![None, None]);
    }

    #[test]
    fn a_pixel_of_disagreement_still_pairs() {
        let pairs = pair_windows(
            &[Some(rect(0.0, 0.0, 800.0, 600.0))],
            &[rect(0.5, 0.0, 800.0, 601.0)],
        );
        assert_eq!(pairs, vec![Some(0)]);
    }

    #[test]
    fn remembered_frames_replace_per_window_and_are_capped() {
        let mut store: Vec<(u32, CGRect)> = Vec::new();
        remember_frame(&mut store, 1, rect(0.0, 0.0, 1.0, 1.0));
        remember_frame(&mut store, 1, rect(5.0, 5.0, 1.0, 1.0));
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].1.origin.x, 5.0);
        for k in 0..(MAX_REMEMBERED_FRAMES as u32 + 10) {
            remember_frame(&mut store, k + 100, rect(0.0, 0.0, 1.0, 1.0));
        }
        assert_eq!(store.len(), MAX_REMEMBERED_FRAMES);
        assert!(
            store.iter().all(|(k, _)| *k != 1),
            "oldest entries go first"
        );
    }

    #[test]
    fn a_move_reads_as_a_change_and_a_no_op_does_not() {
        let before = summary(rect(0.0, 0.0, 800.0, 600.0));
        let mut after = before.clone();
        assert_eq!(observed_change(&before, &after), Observed::Unchanged);
        after.frame = Some(rect(10.0, 0.0, 800.0, 600.0));
        assert_eq!(observed_change(&before, &after), Observed::Changed);
        let mut minimized = before.clone();
        minimized.minimized = true;
        assert_eq!(observed_change(&before, &minimized), Observed::Changed);
    }

    #[test]
    fn verbs_are_the_tool_names() {
        assert_eq!(WindowOp::Focus.verb(), "focus_window");
        assert_eq!(WindowOp::Move { x: 0.0, y: 0.0 }.verb(), "move_window");
        assert_eq!(WindowOp::Restore.verb(), "restore_window");
    }

    #[test]
    fn a_window_is_described_by_title_and_id() {
        let s = summary(rect(0.0, 0.0, 1.0, 1.0));
        assert_eq!(describe_window(&s), "window \"Doc\" (window_id=7)");
        let mut untitled = s;
        untitled.title = None;
        untitled.window_id = None;
        assert_eq!(describe_window(&untitled), "window \"(untitled)\"");
    }

    #[test]
    fn this_process_is_not_gone() {
        let me = ProcessKey::for_pid(unsafe { libc::getpid() });
        assert!(!process_gone(me));
    }

    #[test]
    fn a_pid_that_does_not_exist_is_gone() {
        // Beyond the default pid_max on macOS (99998).
        let key = ProcessKey {
            pid: 999_999,
            start_time: 0,
        };
        assert!(process_gone(key));
    }
}
