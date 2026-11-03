//! Every tool, as the Linux native thread runs it.
//!
//! Each action takes the same path as on macOS, in the same order: resolve
//! the app, resolve the target against the current snapshot (with the same
//! staleness errors), run the shared safety gate, re-read the element for
//! drift, deliver through exactly one mechanism, then optionally re-read the
//! window for a diff. The mechanisms differ:
//!
//! - **AT-SPI first, and by default only.** `Action.DoAction`,
//!   `EditableText`, `Text` selection and `Value` writes are addressed to an
//!   element and touch nothing shared — the property the macOS `ax` delivery
//!   has, and reported with the same label.
//! - **XTEST only when the operator opts in** (`CUA_LINUX_XTEST=1`, X11
//!   only). It is real input on the *shared* devices: the human's pointer
//!   moves and keys go to the focused window. macOS has a pid-routed tier with
//!   no shared side effects; Linux has no equivalent, so rather than quietly
//!   downgrading the guarantee, the shared tier is off unless asked for and
//!   labelled `xtest (shared input)` when it runs. Without it, anything that
//!   needs a pointer or keystrokes answers `PlatformUnsupported`.

use std::time::{Duration, Instant};

use atspi::{Interface, State};
use cua_core::menubar::{menu_path_steps, MenuItem, MenuListing, MenuWalkError};
use cua_core::safety::{Candidate, Gate};
use cua_core::session::Result;
use cua_core::snapshot::{match_nodes, render_tree, TreeNode};
use cua_core::{
    ActionResult, AppInfo, AppState, ClipboardAccess, ClipboardContents, ClipboardRead,
    ClipboardWrite, CloseOutcome, CoreError, Delivery, DisplayInfo, FindResult, FocusCheck,
    FocusState, LaunchOutcome, Mechanism, Modifiers, MouseButton, MouseOptions, Observed,
    Permissions, PointerLocation, Presence, ScrollAmount, ScrollDir, StateOptions, Target,
    WaitOutcome, WindowListing, WindowOp, WindowOutcome, WindowPixel, WindowSelector,
    WindowSummary,
};

use crate::bus::{self, timed, Handle};
use crate::keys;
use crate::tree::{self, BusApp, LinuxNode};
use crate::worker::{next_snapshot_id, unsupported, Inner, Snapshot};

/// AT-SPI action names that mean "the primary activation", most specific
/// first. GTK uses `click`, Qt `Press`, toolkits disagree on the rest.
const CLICK_ACTIONS: &[&str] = &[
    "click", "press", "activate", "jump", "toggle", "open", "select",
];

const WAIT_POLL_MS: u64 = 250;

/// The rows at the end of a menu path, and the row the last step named.
type MenuLevel = (Vec<LinuxNode>, Option<LinuxNode>);

fn preferred_action(actions: &[String], wanted: &[&str]) -> Option<usize> {
    wanted
        .iter()
        .find_map(|w| actions.iter().position(|a| a.eq_ignore_ascii_case(w)))
}

fn xtest_off(feature: &'static str, what: &str) -> CoreError {
    unsupported(
        feature,
        format!(
            "{what} needs synthesized input, and Linux has no per-process input route like macOS's: \
             the only one is XTEST, which drives the SHARED pointer and keyboard (the human's real \
             pointer moves, keys go to the focused window). It is off by default; an operator who \
             accepts that can set CUA_LINUX_XTEST=1 on an X11 session"
        ),
    )
}

impl Inner {
    // ── observation ────────────────────────────────────────────────────────

    pub(crate) fn permissions(&mut self) -> Result<Permissions> {
        let accessibility = match self.a11y() {
            Ok(c) => self.block(tree::list_apps(&c)).is_ok(),
            Err(_) => false,
        };
        Ok(Permissions {
            accessibility,
            screen_recording: self.x11_available(),
        })
    }

    pub(crate) fn list_apps(&mut self) -> Result<Vec<AppInfo>> {
        Ok(self.bus_apps()?.into_iter().map(|a| a.info).collect())
    }

    pub(crate) fn get_app_state(&mut self, query: &str, opts: StateOptions) -> Result<AppState> {
        let app = self.resolve(query)?;
        let c = self.a11y()?;
        let mut warnings = Vec::new();

        // A scoped read walks from a node of the current snapshot instead of
        // the window, exactly as `scope_element_id` does on macOS.
        let (root, window, scoped) = match opts.scope {
            Some(index) => {
                let snap = self.current(&app, query)?;
                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                (node.handle.clone(), snap.window.clone(), true)
            }
            None => {
                let front = self.block(tree::front_frame(&c, &app))?;
                let Some(front) = front else {
                    return Err(CoreError::NoWindow {
                        app: query.to_string(),
                    });
                };
                (front.clone(), Some(front), false)
            }
        };

        let walk = self.block(tree::walk(&c, root, opts.limits, self.xtest))?;
        if !walk.complete {
            warnings.push(format!(
                "the walk stopped at a cap or its {}s budget; pass skeleton=true or scope_element_id to read the rest",
                opts.limits.budget.as_secs()
            ));
        }
        let tree_text = render_tree(&walk.nodes, opts.render);
        let window_node = walk.nodes.first();
        let window_title = window_node.and_then(|n| n.label.clone());
        let window_frame = window_node.and_then(|n| n.frame);

        let (x_window, screenshot) = match self.x11("screenshot") {
            Ok(x) => {
                let x_window = x.window_for(app.info.pid, window_frame);
                let shot = if !opts.include_screenshot {
                    None
                } else if let Some(why) = cua_core::safety::screenshot_refusal(&app.info) {
                    warnings.push(why);
                    None
                } else {
                    match window_frame {
                        Some(f) => match x.capture(f, opts.max_image_dim) {
                            Ok(s) => {
                                warnings.push(
                                    "the screenshot is a read of the screen rectangle the window occupies \
                                     (X11 cannot capture an occluded window on its own), so anything on top \
                                     of the window is in it too"
                                        .to_string(),
                                );
                                Some(s)
                            }
                            Err(e) => {
                                warnings.push(format!("no screenshot: {e}"));
                                None
                            }
                        },
                        None => {
                            warnings.push("no screenshot: the window reports no geometry".into());
                            None
                        }
                    }
                };
                (x_window, shot)
            }
            Err(e) => {
                if opts.include_screenshot {
                    warnings.push(format!("no screenshot: {e}"));
                }
                (None, None)
            }
        };

        let actionable_count = walk.nodes.iter().filter(|n| n.is_actionable()).count();
        let node_count = walk.nodes.len();
        let id = next_snapshot_id();
        let pid = app.info.pid;
        self.snapshots.insert(
            pid,
            Snapshot {
                id,
                app: app.clone(),
                start: tree::process_start(pid),
                nodes: walk.nodes,
                tree: tree_text.clone(),
                window,
                x_window,
                frame: window_frame,
                limits: opts.limits,
                complete: walk.complete,
                scoped,
                acted_on: false,
            },
        );
        Ok(AppState {
            app: app.info,
            snapshot_id: id,
            tree: tree_text,
            node_count,
            actionable_count,
            window_title,
            window_frame,
            window_id: x_window,
            screenshot,
            // AT-SPI describes pop-up menus as part of the app's tree, so there
            // is no separate transient-window list to report here.
            popups: Vec::new(),
            warnings,
        })
    }

    pub(crate) fn find(&mut self, query: &str, needle: &str, limit: usize) -> Result<FindResult> {
        let app = self.resolve(query)?;
        let snap = self.current(&app, query)?;
        let all = match_nodes(&snap.nodes, needle, usize::MAX);
        Ok(FindResult {
            snapshot_id: snap.id,
            total: all.len(),
            lines: all.into_iter().take(limit.max(1)).collect(),
            searched: snap.nodes.len(),
        })
    }

    pub(crate) fn wait_for(
        &mut self,
        query: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(timeout_ms);
        let mut polls = 0u32;
        loop {
            let state = self.get_app_state(
                query,
                StateOptions {
                    include_screenshot: false,
                    ..StateOptions::default()
                },
            )?;
            polls += 1;
            let found = self.find(query, needle, 1)?.total > 0;
            let satisfied = found == (want == Presence::Appears);
            if satisfied || Instant::now() >= deadline {
                return Ok(WaitOutcome {
                    satisfied,
                    polls,
                    snapshot_id: state.snapshot_id,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
            std::thread::sleep(Duration::from_millis(WAIT_POLL_MS));
        }
    }

    // ── the action pipeline ────────────────────────────────────────────────

    /// Resolve, gate, revalidate, deliver, settle — see the module docs.
    #[allow(clippy::too_many_arguments)]
    fn act<F>(
        &mut self,
        query: &str,
        target: &Target,
        gate: Gate,
        verb: &str,
        return_state: bool,
        deliver: F,
    ) -> Result<ActionResult>
    where
        F: FnOnce(&mut Self, &BusApp, &LinuxNode) -> Result<(Delivery, Option<FocusCheck>)>,
    {
        let app = self.resolve(query)?;
        let index = self.locate(&app, query, target)?;
        let (candidate, node) = {
            let snap = self.current(&app, query)?;
            let node = snap.nodes.get(index).cloned().ok_or(CoreError::BadIndex {
                index,
                count: snap.nodes.len(),
            })?;
            (Inner::candidate(snap, index), node)
        };
        self.guard(&app.info, &gate, candidate.as_ref())?;
        self.revalidate(app.info.pid, index)?;
        let (delivery, focus) = deliver(self, &app, &node)?;
        let (ui_changed, state) = self.settle(app.info.pid, return_state);
        let mut r = ActionResult::new(verb, node.describe(), ui_changed, delivery);
        r.state = state;
        r.focus = focus;
        Ok(r)
    }

    fn do_action(
        &mut self,
        app: &BusApp,
        node: &LinuxNode,
        index: usize,
        what: &'static str,
    ) -> Result<()> {
        let c = self.a11y()?;
        let name = node.actions.get(index).cloned().unwrap_or_default();
        let done = self.block(async {
            let a = bus::action(&c, &node.handle).await?;
            timed(a.do_action(index as i32)).await
        });
        match done {
            Ok(true) => Ok(()),
            Ok(false) => Err(CoreError::PointerEventRefused {
                app: app.info.name.clone(),
                what,
                reason: format!("the app declined DoAction({name:?}) on {}", node.describe()),
            }),
            Err(e) => Err(tree::gone_or(e, node.index, &node.describe())),
        }
    }

    // ── click family ───────────────────────────────────────────────────────

    pub(crate) fn click(
        &mut self,
        query: &str,
        target: Target,
        mouse: MouseOptions,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("click", &target).confirmed(confirm_destructive);
        self.act(
            query,
            &target,
            gate,
            "click",
            return_state,
            |me, app, node| {
                let plain = mouse.button == MouseButton::Left
                    && mouse.modifiers.is_empty()
                    && mouse.count == 1;
                if plain {
                    if let Some(i) = preferred_action(&node.actions, CLICK_ACTIONS).or(
                        // An element with exactly one action means that action.
                        (node.actions.len() == 1).then_some(0),
                    ) {
                        me.do_action(app, node, i, "click")?;
                        return Ok((Delivery::Ax, None));
                    }
                }
                if !me.xtest {
                    return Err(xtest_off(
                        "click",
                        &if plain {
                            format!(
                                "{} advertises no AT-SPI activation action, so clicking it",
                                node.describe()
                            )
                        } else {
                            format!(
                                "a {} click (AT-SPI has no button, modifier or click count)",
                                mouse.describe()
                            )
                        },
                    ));
                }
                let (x, y) = node
                    .centre()
                    .ok_or_else(|| CoreError::PointerEventRefused {
                        app: app.info.name.clone(),
                        what: "click",
                        reason: format!("{} reports no geometry to aim at", node.describe()),
                    })?;
                me.xtest_click(x, y, mouse)?;
                Ok((Delivery::SharedInput, None))
            },
        )
    }

    pub(crate) fn click_in_window(
        &mut self,
        query: &str,
        at: WindowPixel,
        mouse: MouseOptions,
        return_state: bool,
    ) -> Result<ActionResult> {
        if !self.xtest {
            return Err(xtest_off(
                "click_in_window",
                "a click at a bare window coordinate",
            ));
        }
        let app = self.resolve(query)?;
        let (frame, x_window, id) = {
            let snap = self.current(&app, query)?;
            (snap.frame, snap.x_window, snap.id)
        };
        if let Some(given) = at.snapshot_id {
            if given != id {
                return Err(CoreError::StaleCoordinate {
                    app: query.to_string(),
                    given,
                    current: id,
                    x: at.x,
                    y: at.y,
                });
            }
        }
        if x_window != Some(at.window_id) {
            return Err(CoreError::WindowClickRefused {
                app: query.to_string(),
                wid: at.window_id,
                x: at.x,
                y: at.y,
                reason: "that is not the window the latest get_app_state of this app read".into(),
            });
        }
        let frame = frame.ok_or_else(|| CoreError::WindowClickRefused {
            app: query.to_string(),
            wid: at.window_id,
            x: at.x,
            y: at.y,
            reason: "the window reports no geometry".into(),
        })?;
        if at.x < 0.0 || at.y < 0.0 || at.x >= frame.size.width || at.y >= frame.size.height {
            return Err(CoreError::WindowClickRefused {
                app: query.to_string(),
                wid: at.window_id,
                x: at.x,
                y: at.y,
                reason: format!(
                    "the point is outside the window's {}x{} frame",
                    frame.size.width, frame.size.height
                ),
            });
        }
        self.guard(&app.info, &Gate::elementless("click_in_window"), None)?;
        self.xtest_click(frame.origin.x + at.x, frame.origin.y + at.y, mouse)?;
        let (ui_changed, state) = self.settle(app.info.pid, return_state);
        let mut r = ActionResult::new(
            "click_in_window",
            format!("({:.0}, {:.0}) in window {}", at.x, at.y, at.window_id),
            ui_changed,
            Delivery::SharedInput,
        );
        r.state = state;
        Ok(r)
    }

    fn point_of(&self, app: &BusApp, query: &str, loc: &PointerLocation) -> Result<(f64, f64)> {
        match loc {
            PointerLocation::Element(t) => {
                let i = self.locate(app, query, t)?;
                let snap = self.current(app, query)?;
                snap.nodes
                    .get(i)
                    .and_then(LinuxNode::centre)
                    .ok_or_else(|| CoreError::PointerEventRefused {
                        app: query.to_string(),
                        what: "pointer gesture",
                        reason: "the element reports no geometry".into(),
                    })
            }
            PointerLocation::WindowPoint { x, y } => {
                let snap = self.current(app, query)?;
                let f = snap.frame.ok_or_else(|| CoreError::PointerEventRefused {
                    app: query.to_string(),
                    what: "pointer gesture",
                    reason: "the window reports no geometry".into(),
                })?;
                Ok((f.origin.x + x, f.origin.y + y))
            }
        }
    }

    pub(crate) fn drag(
        &mut self,
        query: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        _snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Result<ActionResult> {
        if !self.xtest {
            return Err(xtest_off("drag", "a drag"));
        }
        let app = self.resolve(query)?;
        let a = self.point_of(&app, query, &from)?;
        let b = self.point_of(&app, query, &to)?;
        self.guard(&app.info, &Gate::elementless("drag"), None)?;
        let button = x_button(mouse.button);
        let mods = mod_keysyms(mouse.modifiers)?;
        self.with_x(|x, me| {
            me.hold(x, &mods, true)?;
            x.pointer_move(a.0, a.1)?;
            x.button(button, true)?;
            const STEPS: u32 = 12;
            for i in 1..=STEPS {
                let t = f64::from(i) / f64::from(STEPS);
                x.pointer_move(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)?;
                std::thread::sleep(Duration::from_millis(8));
            }
            x.button(button, false)?;
            me.hold(x, &mods, false)?;
            x.sync()
        })?;
        let (ui_changed, state) = self.settle(app.info.pid, return_state);
        let mut r = ActionResult::new(
            "drag",
            format!("({:.0}, {:.0}) → ({:.0}, {:.0})", a.0, a.1, b.0, b.1),
            ui_changed,
            Delivery::SharedInput,
        );
        r.state = state;
        Ok(r)
    }

    pub(crate) fn hover(
        &mut self,
        query: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        _snapshot_id: Option<u64>,
        return_state: bool,
    ) -> Result<ActionResult> {
        if !self.xtest {
            return Err(xtest_off("hover", "a hover"));
        }
        let app = self.resolve(query)?;
        let p = self.point_of(&app, query, &at)?;
        self.guard(&app.info, &Gate::elementless("hover"), None)?;
        let mods = mod_keysyms(modifiers)?;
        self.with_x(|x, me| {
            me.hold(x, &mods, true)?;
            x.pointer_move(p.0, p.1)?;
            me.hold(x, &mods, false)?;
            x.sync()
        })?;
        let (ui_changed, state) = self.settle(app.info.pid, return_state);
        let mut r = ActionResult::new(
            "hover",
            format!("({:.0}, {:.0})", p.0, p.1),
            ui_changed,
            Delivery::SharedInput,
        );
        r.state = state;
        Ok(r)
    }

    pub(crate) fn scroll(
        &mut self,
        query: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
        return_state: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("scroll", &target);
        self.act(
            query,
            &target,
            gate,
            "scroll",
            return_state,
            |me, app, node| {
                // AT-SPI has no "scroll by a page" verb — `Component.ScrollTo`
                // only brings an element into view — so every scroll is a wheel.
                if !me.xtest {
                    return Err(xtest_off("scroll", "scrolling"));
                }
                let (x, y) = node
                    .centre()
                    .ok_or_else(|| CoreError::PointerEventRefused {
                        app: app.info.name.clone(),
                        what: "scroll",
                        reason: format!("{} reports no geometry to aim at", node.describe()),
                    })?;
                // One wheel click is a toolkit-defined step (GTK: ~50 px); a page
                // is approximated as five, points as one click per 50.
                let clicks = match amount {
                    ScrollAmount::Pages(n) => n.saturating_mul(5),
                    ScrollAmount::Points(p) => (p / 50).max(1),
                }
                .min(200);
                let button = match dir {
                    ScrollDir::Up => 4,
                    ScrollDir::Down => 5,
                    ScrollDir::Left => 6,
                    ScrollDir::Right => 7,
                };
                me.with_x(|xc, _| {
                    xc.pointer_move(x, y)?;
                    for _ in 0..clicks {
                        xc.button(button, true)?;
                        xc.button(button, false)?;
                    }
                    xc.sync()
                })?;
                Ok((Delivery::SharedInput, None))
            },
        )
    }

    // ── text ───────────────────────────────────────────────────────────────

    pub(crate) fn set_value(
        &mut self,
        query: &str,
        target: Target,
        value: &str,
        return_state: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("set_value", &target);
        let value = value.to_string();
        self.act(
            query,
            &target,
            gate,
            "set_value",
            return_state,
            |me, app, node| {
                let c = me.a11y()?;
                if node.has(Interface::EditableText) {
                    let ok = me
                        .block(async {
                            let e = bus::editable_text(&c, &node.handle).await?;
                            timed(e.set_text_contents(&value)).await
                        })
                        .map_err(|e| tree::gone_or(e, node.index, &node.describe()))?;
                    if !ok {
                        return Err(CoreError::PointerEventRefused {
                            app: app.info.name.clone(),
                            what: "set_value",
                            reason: format!(
                                "the app declined SetTextContents on {}",
                                node.describe()
                            ),
                        });
                    }
                    return Ok((Delivery::Ax, None));
                }
                if node.has(Interface::Value) {
                    let n: f64 =
                        value
                            .trim()
                            .parse()
                            .map_err(|_| CoreError::ElementUnsupported {
                                verb: "set_value",
                                element: node.describe(),
                                why: format!("it holds a number and {value:?} is not one"),
                            })?;
                    me.block(async {
                        let v = bus::value(&c, &node.handle).await?;
                        timed(v.set_current_value(n)).await
                    })
                    .map_err(|e| tree::gone_or(e, node.index, &node.describe()))?;
                    return Ok((Delivery::Ax, None));
                }
                Err(CoreError::ElementUnsupported {
                    verb: "set_value",
                    element: node.describe(),
                    why: "it implements neither AT-SPI EditableText nor Value".into(),
                })
            },
        )
    }

    pub(crate) fn type_text(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
        return_state: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("type_text", &target);
        let text = text.to_string();
        self.act(query, &target, gate, "type_text", return_state, |me, app, node| {
            match mechanism {
                Mechanism::Ax => {
                    if !node.has(Interface::EditableText) {
                        return Err(CoreError::ElementUnsupported {
                            verb: "type_text",
                            element: node.describe(),
                            why: "it implements no AT-SPI EditableText; pass mechanism=\"keystrokes\" \
                                  (needs CUA_LINUX_XTEST=1) for terminals and canvases"
                                .into(),
                        });
                    }
                    let c = me.a11y()?;
                    let chars = text.chars().count() as i32;
                    let ok = me
                        .block(async {
                            let pos = match bus::text(&c, &node.handle).await {
                                Ok(t) => {
                                    let caret = timed(t.caret_offset()).await.unwrap_or(-1);
                                    if caret >= 0 {
                                        caret
                                    } else {
                                        timed(t.character_count()).await.unwrap_or(0)
                                    }
                                }
                                Err(_) => 0,
                            };
                            let e = bus::editable_text(&c, &node.handle).await?;
                            timed(e.insert_text(pos, &text, chars)).await
                        })
                        .map_err(|e| tree::gone_or(e, node.index, &node.describe()))?;
                    if !ok {
                        return Err(CoreError::PointerEventRefused {
                            app: app.info.name.clone(),
                            what: "type_text",
                            reason: format!("the app declined InsertText on {}", node.describe()),
                        });
                    }
                    Ok((Delivery::Ax, None))
                }
                Mechanism::Keystrokes => {
                    if !me.xtest {
                        return Err(xtest_off("type_text", "typing with real keystrokes"));
                    }
                    let focus = me.focus_for_keys(node)?;
                    me.xtest_type(&text)?;
                    Ok((Delivery::SharedInput, Some(focus)))
                }
            }
        })
    }

    pub(crate) fn select_text(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
        return_state: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("select_text", &target);
        let (text, prefix, suffix) = (
            text.to_string(),
            prefix.unwrap_or("").to_string(),
            suffix.unwrap_or("").to_string(),
        );
        self.act(
            query,
            &target,
            gate,
            "select_text",
            return_state,
            |me, app, node| {
                if !node.has(Interface::Text) {
                    return Err(CoreError::ElementUnsupported {
                        verb: "select_text",
                        element: node.describe(),
                        why: "it implements no AT-SPI Text interface".into(),
                    });
                }
                let c = me.a11y()?;
                let ok = me
                    .block(async {
                        let t = bus::text(&c, &node.handle).await?;
                        let count = timed(t.character_count()).await?;
                        let all = timed(t.get_text(0, count)).await?;
                        let Some((start, len)) = find_range(&all, &text, &prefix, &suffix) else {
                            return Ok(None);
                        };
                        let (s, e) = (start as i32, (start + len) as i32);
                        let n = timed(t.get_n_selections()).await.unwrap_or(0);
                        let done = if n > 0 {
                            timed(t.set_selection(0, s, e)).await?
                        } else {
                            timed(t.add_selection(s, e)).await?
                        };
                        Ok(Some(done))
                    })
                    .map_err(|e| tree::gone_or(e, node.index, &node.describe()))?;
                match ok {
                    // Same classification as macOS's `AxError::NoValue` here.
                    None => Err(CoreError::Accessibility {
                        reason: format!("text {text:?} was not found in this element"),
                    }),
                    Some(false) => Err(CoreError::PointerEventRefused {
                        app: app.info.name.clone(),
                        what: "select_text",
                        reason: format!("the app declined the selection on {}", node.describe()),
                    }),
                    Some(true) => Ok((Delivery::Ax, None)),
                }
            },
        )
    }

    // ── keys and named actions ─────────────────────────────────────────────

    pub(crate) fn press_key(
        &mut self,
        query: &str,
        target: Target,
        key: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("press_key", &target)
            .with_key(key)
            .confirmed(confirm_destructive);
        let key = key.to_string();
        self.act(
            query,
            &target,
            gate,
            "press_key",
            return_state,
            |me, app, node| {
                let chord =
                    keys::parse_chord(&key).map_err(|reason| CoreError::KeyChordUnparseable {
                        key: key.clone(),
                        reason,
                    })?;
                if me.xtest {
                    let focus = me.focus_for_keys(node)?;
                    me.xtest_chord(&chord)?;
                    return Ok((Delivery::SharedInput, Some(focus)));
                }
                // Without XTEST the only keys with an element-addressed meaning
                // are the ones that activate: return/enter/space on a control
                // with an activation action.
                let activating = chord.modifiers.is_empty()
                    && matches!(
                        key.trim().to_lowercase().as_str(),
                        "return" | "enter" | "space"
                    );
                if activating {
                    if let Some(i) = preferred_action(&node.actions, CLICK_ACTIONS) {
                        me.do_action(app, node, i, "key press")?;
                        return Ok((Delivery::Ax, None));
                    }
                }
                Err(CoreError::KeyNoAccessibilityEquivalent { key: key.clone() })
            },
        )
    }

    pub(crate) fn perform_action(
        &mut self,
        query: &str,
        target: Target,
        action: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        let gate = Gate::at("perform_secondary_action", &target).confirmed(confirm_destructive);
        let action = action.to_string();
        self.act(
            query,
            &target,
            gate,
            "perform_secondary_action",
            return_state,
            |me, app, node| {
                // Exact AT-SPI name first, then the AX verb an agent learned on
                // macOS, mapped to the names toolkits use for it.
                let aliases: &[&str] = match action.as_str() {
                    "AXPress" | "AXConfirm" | "AXPick" => CLICK_ACTIONS,
                    "AXShowMenu" => &["showmenu", "show menu", "menu", "popup", "context menu"],
                    "AXIncrement" => &["increment", "increase"],
                    "AXDecrement" => &["decrement", "decrease"],
                    "AXExpand" | "AXCollapse" => {
                        &["expand or contract", "expand or collapse", "toggle"]
                    }
                    _ => &[],
                };
                let index = node
                    .actions
                    .iter()
                    .position(|a| a.eq_ignore_ascii_case(&action))
                    .or_else(|| preferred_action(&node.actions, aliases));
                match index {
                    Some(i) => {
                        me.do_action(app, node, i, "action")?;
                        Ok((Delivery::Ax, None))
                    }
                    None => Err(CoreError::ElementUnsupported {
                        verb: "perform_secondary_action",
                        element: node.describe(),
                        why: format!(
                            "it has no {action:?} action; it advertises [{}]",
                            node.actions.join(", ")
                        ),
                    }),
                }
            },
        )
    }

    // ── menu bar ───────────────────────────────────────────────────────────

    /// The app's menu bar, found by role under its front window.
    fn menu_bar_root(&mut self, app: &BusApp) -> Result<Option<LinuxNode>> {
        let c = self.a11y()?;
        let Some(front) = self.block(tree::front_frame(&c, app))? else {
            return Ok(None);
        };
        // Menus are often not Showing until opened, so the walk must not skip
        // hidden subtrees here, and it stops at the first menu bar.
        let limits = cua_core::Limits {
            max_nodes: 400,
            max_depth: 8,
            skip_offscreen: false,
            ..cua_core::Limits::default()
        };
        let walk = self.block(tree::walk(&c, front, limits, false))?;
        Ok(walk.nodes.into_iter().find(|n| n.role == "AXMenuBar"))
    }

    async fn children_of(c: &zbus::Connection, h: &Handle) -> Vec<LinuxNode> {
        let Ok(acc) = bus::accessible(c, h).await else {
            return Vec::new();
        };
        let Ok(refs) = timed(acc.get_children()).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, r) in refs.iter().enumerate() {
            if let Some(h) = Handle::from_ref(r) {
                if let Ok((n, _)) = tree::read_node(c, h, i, None, false).await {
                    out.push(n);
                }
            }
        }
        out
    }

    /// Walk `path` from the menu bar; return the rows at the end of it, or the
    /// row it names when `want_row`.
    fn walk_menu(
        &mut self,
        app: &BusApp,
        path: &str,
    ) -> Result<std::result::Result<MenuLevel, MenuWalkError>> {
        let Some(bar) = self.menu_bar_root(app)? else {
            return Ok(Err(MenuWalkError::NoMenuBar));
        };
        let c = self.a11y()?;
        let mut level = self.block(Self::children_of(&c, &bar.handle));
        let mut walked = String::new();
        let mut last: Option<LinuxNode> = None;
        for step in menu_path_steps(path) {
            let Some(row) = level
                .iter()
                .find(|n| {
                    n.label
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(step))
                })
                .cloned()
            else {
                return Ok(Err(MenuWalkError::NoSuchItem {
                    step: step.to_string(),
                    at: walked,
                    available: level
                        .iter()
                        .map(|n| n.label.clone().unwrap_or_default())
                        .collect(),
                }));
            };
            walked = if walked.is_empty() {
                step.to_string()
            } else {
                format!("{walked}>{step}")
            };
            // A GTK menu-bar item holds its menu as its only child; a menu item
            // with a submenu is itself a Menu.
            let mut next = self.block(Self::children_of(&c, &row.handle));
            if next.len() == 1 && next[0].role == "AXMenu" {
                let inner = next[0].handle.clone();
                next = self.block(Self::children_of(&c, &inner));
            }
            level = next;
            last = Some(row);
        }
        Ok(Ok((level, last)))
    }

    fn menu_item(&mut self, node: &LinuxNode, path: &str) -> MenuItem {
        let title = node.label.clone().unwrap_or_default();
        let full = if path.is_empty() {
            title.clone()
        } else {
            format!("{path}>{title}")
        };
        let shortcut = match self.a11y() {
            Ok(c) if node.has(Interface::Action) => self
                .block(async {
                    let a = bus::action(&c, &node.handle).await.ok()?;
                    timed(a.get_key_binding(0)).await.ok()
                })
                .and_then(|k| keys::accelerator_to_chord(&k)),
            _ => None,
        };
        MenuItem {
            title,
            path: full,
            enabled: node.enabled,
            has_submenu: node.atspi_role == atspi::Role::Menu,
            shortcut,
            mark: node
                .states
                .contains(State::Checked)
                .then(|| "✓".to_string()),
        }
    }

    pub(crate) fn menu_bar(&mut self, query: &str, path: &str) -> Result<MenuListing> {
        let app = self.resolve(query)?;
        match self.walk_menu(&app, path)? {
            Ok((rows, _)) => {
                let items = rows.iter().map(|n| self.menu_item(n, path)).collect();
                Ok(MenuListing {
                    path: path.to_string(),
                    items,
                })
            }
            Err(e) => Err(CoreError::MenuPath {
                app: query.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    pub(crate) fn press_menu_bar(
        &mut self,
        query: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        let app = self.resolve(query)?;
        let (rows, row) = match self.walk_menu(&app, path)? {
            Ok(r) => r,
            Err(e) => {
                return Err(CoreError::MenuPath {
                    app: query.to_string(),
                    reason: e.to_string(),
                })
            }
        };
        let Some(row) = row else {
            return Err(CoreError::MenuPath {
                app: query.to_string(),
                reason: "name a menu row to press, e.g. `File>Quit`".into(),
            });
        };
        if row.atspi_role == atspi::Role::Menu || !rows.is_empty() {
            return Err(CoreError::MenuPath {
                app: query.to_string(),
                reason: MenuWalkError::IsSubmenu {
                    path: path.to_string(),
                    children: rows
                        .iter()
                        .map(|n| n.label.clone().unwrap_or_default())
                        .collect(),
                }
                .to_string(),
            });
        }
        let candidate = Candidate {
            role: row.role.clone(),
            label: row.label.clone(),
            description: format!("menu bar row `{path}`"),
            ..Candidate::default()
        };
        let gate =
            Gate::labelled("press_menu_bar", candidate.clone()).confirmed(confirm_destructive);
        self.guard(&app.info, &gate, Some(&candidate))?;
        let i = preferred_action(&row.actions, CLICK_ACTIONS)
            .or((!row.actions.is_empty()).then_some(0))
            .ok_or_else(|| CoreError::MenuPath {
                app: query.to_string(),
                reason: format!("`{path}` advertises no AT-SPI action to press"),
            })?;
        self.do_action(&app, &row, i, "menu press")?;
        let (ui_changed, state) = self.settle(app.info.pid, return_state);
        let mut r = ActionResult::new(
            "press_menu_bar",
            format!("`{path}`"),
            ui_changed,
            Delivery::Ax,
        );
        r.state = state;
        Ok(r)
    }

    // ── windows ────────────────────────────────────────────────────────────

    fn window_summaries(&mut self, app: &BusApp) -> Result<Vec<(WindowSummary, Handle)>> {
        let c = self.a11y()?;
        let frames = self.block(tree::frames(&c, app))?;
        let mut out = Vec::new();
        for (i, h) in frames.into_iter().enumerate() {
            let Ok((n, _)) = self.block(tree::read_node(&c, h.clone(), i, None, false)) else {
                continue;
            };
            let window_id = self
                .x11("list_windows")
                .ok()
                .and_then(|x| x.window_for(app.info.pid, n.frame));
            let minimized = n.states.contains(State::Iconified)
                || window_id
                    .is_some_and(|w| self.x11("list_windows").is_ok_and(|x| x.is_hidden(w)));
            let active = n.states.contains(State::Active);
            out.push((
                WindowSummary {
                    ax_index: Some(i),
                    window_id,
                    title: n.label.clone(),
                    subrole: n.subrole.clone(),
                    frame: n.frame,
                    level: None,
                    minimized,
                    full_screen: false,
                    main: active,
                    focused: active,
                    on_screen: Some(n.states.contains(State::Showing) && !minimized),
                    manageable: window_id.is_some(),
                },
                h,
            ));
        }
        Ok(out)
    }

    pub(crate) fn list_windows(&mut self, query: &str) -> Result<WindowListing> {
        let app = self.resolve(query)?;
        let windows: Vec<WindowSummary> = self
            .window_summaries(&app)?
            .into_iter()
            .map(|(w, _)| w)
            .collect();
        let mut warnings = Vec::new();
        if windows.iter().any(|w| w.window_id.is_none()) {
            warnings.push(
                "a window with no window_id could not be paired with an X11 window (no X display, \
                 a Wayland-native window, or no _NET_WM_PID), so the window tools cannot act on it"
                    .to_string(),
            );
        }
        Ok(WindowListing {
            app: app.info,
            windows,
            warnings,
        })
    }

    pub(crate) fn window_action(
        &mut self,
        query: &str,
        sel: WindowSelector,
        op: WindowOp,
    ) -> Result<WindowOutcome> {
        let app = self.resolve(query)?;
        let all = self.window_summaries(&app)?;
        let chosen = match (&sel.window_id, &sel.title) {
            (Some(id), _) => all.iter().find(|(w, _)| w.window_id == Some(*id)),
            (None, Some(t)) => {
                let hits: Vec<_> = all
                    .iter()
                    .filter(|(w, _)| {
                        w.title
                            .as_deref()
                            .is_some_and(|x| x.eq_ignore_ascii_case(t))
                    })
                    .collect();
                if hits.len() > 1 {
                    return Err(CoreError::Window {
                        app: query.to_string(),
                        reason: format!(
                            "{} windows are titled {t:?}; pass window_id from list_windows",
                            hits.len()
                        ),
                    });
                }
                hits.into_iter().next()
            }
            (None, None) => all.iter().find(|(w, _)| w.focused).or(all.first()),
        };
        let Some((before, _)) = chosen.cloned() else {
            return Err(CoreError::Window {
                app: query.to_string(),
                reason: "no window matches; call list_windows".into(),
            });
        };
        let Some(win) = before.window_id else {
            return Err(unsupported(
                "window management",
                "this window could not be paired with an X11 window, and AT-SPI itself offers no \
                 portable move, resize, minimize or maximize; under Wayland only the compositor can do these",
            ));
        };
        self.guard(&app.info, &Gate::elementless(op.verb()), None)?;
        let note = {
            let x = self.x11("window management")?;
            let sent = match op {
                WindowOp::Focus => x.raise(win),
                WindowOp::Move { x: px, y: py } => x.move_resize(win, Some((px, py)), None),
                WindowOp::Resize { width, height } => {
                    x.move_resize(win, None, Some((width, height)))
                }
                WindowOp::Minimize => x.minimize(win),
                WindowOp::Maximize => x.set_maximized(win, true),
                WindowOp::Restore => {
                    if before.minimized {
                        x.activate(win)
                    } else {
                        x.set_maximized(win, false)
                    }
                }
            };
            sent.map_err(|reason| CoreError::Window {
                app: query.to_string(),
                reason,
            })?;
            match op {
                WindowOp::Focus => Some(
                    "raised with _NET_RESTACK_WINDOW, which does not activate the window; the window \
                     manager decides whether to honor it".to_string(),
                ),
                WindowOp::Restore if before.minimized => Some(
                    "un-minimizing on X11 means _NET_ACTIVE_WINDOW, so this restore also activated the window"
                        .to_string(),
                ),
                _ => None,
            }
        };
        std::thread::sleep(Duration::from_millis(250));
        let after = self
            .window_summaries(&app)?
            .into_iter()
            .map(|(w, _)| w)
            .find(|w| w.window_id == Some(win))
            .unwrap_or_else(|| before.clone());
        let changed = if after == before {
            Observed::Unchanged
        } else {
            Observed::Changed
        };
        let target = format!(
            "window {:?} (window_id={win})",
            before.title.clone().unwrap_or_default()
        );
        Ok(WindowOutcome {
            action: ActionResult::new(op.verb(), target, changed, Delivery::WindowManager),
            before,
            after,
            note,
        })
    }

    pub(crate) fn list_displays(&mut self) -> Result<Vec<DisplayInfo>> {
        let x = self.x11("list_displays")?;
        x.monitors().map_err(|reason| CoreError::Window {
            app: "displays".into(),
            reason,
        })
    }

    // ── lifecycle ──────────────────────────────────────────────────────────

    pub(crate) fn launch_app(&mut self, spec: &str, activate: bool) -> Result<LaunchOutcome> {
        let spec = spec.trim();
        if let Ok(running) = self.resolve(spec) {
            return Ok(LaunchOutcome {
                path: tree::exe_name(running.info.pid).unwrap_or_default(),
                frontmost: running.info.active,
                app: running.info,
                already_running: true,
                activate_requested: activate,
            });
        }
        let path = locate_executable(spec).ok_or_else(|| CoreError::Lifecycle {
            app: spec.to_string(),
            reason: "no executable by that name on PATH (pass an absolute path, or the command \
                     name as `which` would find it)"
                .into(),
        })?;
        let exe = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !cua_core::safety::forbidden_targets_allowed() {
            if let Some(why) = cua_core::safety::forbidden_bundle(&exe) {
                return Err(CoreError::Refused(cua_core::Refused::ForbiddenTarget {
                    verb: "launch_app",
                    app: exe.clone(),
                    bundle_id: exe,
                    why,
                }));
            }
        }
        let mut child = std::process::Command::new(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| CoreError::Lifecycle {
                app: spec.to_string(),
                reason: e.to_string(),
            })?;
        let pid = child.id() as libc::pid_t;
        // Reap it whenever it exits so it never lingers as a zombie.
        let _ = std::thread::Builder::new()
            .name("cua-launched-child".into())
            .spawn(move || {
                let _ = child.wait();
            });
        // Wait for it to register on the accessibility bus.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(app) = self.bus_apps()?.into_iter().find(|a| a.info.pid == pid) {
                return Ok(LaunchOutcome {
                    path: path.to_string_lossy().into_owned(),
                    frontmost: app.info.active,
                    app: app.info,
                    already_running: false,
                    activate_requested: activate,
                });
            }
            if Instant::now() > deadline {
                return Err(CoreError::Lifecycle {
                    app: spec.to_string(),
                    reason: format!(
                        "started (pid {pid}) but it did not appear on the accessibility bus within 15s; \
                         it may not enable toolkit accessibility"
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub(crate) fn close_app(&mut self, _query: &str, _confirm: bool) -> Result<CloseOutcome> {
        Err(unsupported(
            "close_app",
            "Linux has no polite, app-answerable quit request: macOS sends a quit Apple Event the app \
             can answer with \"save changes?\", while SIGTERM simply ends most GTK and Qt apps and \
             loses unsaved work. Close the window from the app's own menu with press_menu_bar instead",
        ))
    }

    // ── clipboard ──────────────────────────────────────────────────────────

    pub(crate) fn clipboard_get(&mut self) -> Result<ClipboardRead> {
        self.guard_session("clipboard_get")?;
        self.x11("clipboard_get")?;
        let types = crate::clipboard::types().map_err(|reason| CoreError::Clipboard {
            verb: "clipboard_get",
            reason,
        })?;
        let frontmost = self
            .x11("clipboard_get")
            .ok()
            .and_then(|x| x.active_pid())
            .and_then(|pid| {
                self.bus_apps()
                    .ok()?
                    .into_iter()
                    .find(|a| a.info.pid == pid)
                    .map(|a| a.info)
            });
        let contents = match cua_core::safety::clipboard_withheld(
            &types,
            &[crate::clipboard::CONCEALED],
            "ctrl+v",
            None,
            frontmost.as_ref(),
            cua_core::safety::forbidden_targets_allowed(),
        ) {
            Some(reason) => ClipboardContents::Withheld(reason),
            None => match crate::clipboard::text().map_err(|reason| CoreError::Clipboard {
                verb: "clipboard_get",
                reason,
            })? {
                Some(t) => ClipboardContents::Text(t),
                None => ClipboardContents::NoText,
            },
        };
        Ok(ClipboardRead {
            contents,
            types,
            change_count: -1,
            access: ClipboardAccess::NoPrivacyLayer,
        })
    }

    pub(crate) fn clipboard_set(&mut self, text: &str) -> Result<ClipboardWrite> {
        self.guard_session("clipboard_set")?;
        self.x11("clipboard_set")?;
        let verified =
            crate::clipboard::set(text.to_string()).map_err(|reason| CoreError::Clipboard {
                verb: "clipboard_set",
                reason,
            })?;
        Ok(ClipboardWrite {
            verb: "clipboard_set",
            change_count: -1,
            chars: text.chars().count(),
            verified,
        })
    }

    pub(crate) fn clipboard_clear(&mut self) -> Result<ClipboardWrite> {
        self.guard_session("clipboard_clear")?;
        self.x11("clipboard_clear")?;
        let verified = crate::clipboard::clear().map_err(|reason| CoreError::Clipboard {
            verb: "clipboard_clear",
            reason,
        })?;
        Ok(ClipboardWrite {
            verb: "clipboard_clear",
            change_count: -1,
            chars: 0,
            verified,
        })
    }

    // ── XTEST helpers ──────────────────────────────────────────────────────

    /// Run `f` with the X connection. Split out so a helper can borrow both
    /// the connection and `self` without fighting the borrow checker.
    fn with_x<T>(
        &mut self,
        f: impl FnOnce(&crate::x11::X11, &Self) -> std::result::Result<T, String>,
    ) -> Result<T> {
        self.x11("xtest")?;
        let x = self.x11_ref().ok_or(CoreError::WorkerGone)?;
        f(x, self).map_err(|reason| CoreError::PointerEventRefused {
            app: "X server".into(),
            what: "synthesized input",
            reason,
        })
    }

    fn hold(
        &self,
        x: &crate::x11::X11,
        keysyms: &[u32],
        press: bool,
    ) -> std::result::Result<(), String> {
        let codes: Vec<u8> = keysyms
            .iter()
            .map(|k| {
                x.keycode_for(*k)
                    .map(|(c, _)| c)
                    .ok_or_else(|| format!("no keycode for modifier keysym {k:#x}"))
            })
            .collect::<std::result::Result<_, _>>()?;
        if press {
            for c in &codes {
                x.key(*c, true)?;
            }
        } else {
            for c in codes.iter().rev() {
                x.key(*c, false)?;
            }
        }
        Ok(())
    }

    fn xtest_click(&mut self, px: f64, py: f64, mouse: MouseOptions) -> Result<()> {
        let mods = mod_keysyms(mouse.modifiers)?;
        let button = x_button(mouse.button);
        self.with_x(|x, me| {
            me.hold(x, &mods, true)?;
            x.pointer_move(px, py)?;
            for _ in 0..mouse.count.max(1) {
                x.button(button, true)?;
                x.button(button, false)?;
            }
            me.hold(x, &mods, false)?;
            x.sync()
        })
    }

    fn xtest_chord(&mut self, chord: &keys::XChord) -> Result<()> {
        let chord = chord.clone();
        self.with_x(|x, me| {
            let (code, shift) = x.keycode_for(chord.key).ok_or_else(|| {
                format!(
                    "no key on this keyboard layout produces keysym {:#x}",
                    chord.key
                )
            })?;
            let mut mods = chord.modifiers.clone();
            if shift && !mods.contains(&keys::SHIFT_L) {
                mods.push(keys::SHIFT_L);
            }
            me.hold(x, &mods, true)?;
            x.key(code, true)?;
            x.key(code, false)?;
            me.hold(x, &mods, false)?;
            x.sync()
        })
    }

    fn xtest_type(&mut self, text: &str) -> Result<()> {
        let text = text.to_string();
        self.with_x(|x, me| {
            for ch in text.chars() {
                let sym = keys::char_keysym(ch);
                let (code, shift) = x
                    .keycode_for(sym)
                    .ok_or_else(|| format!("no key on this keyboard layout types {ch:?}"))?;
                let mods: Vec<u32> = if shift {
                    vec![keys::SHIFT_L]
                } else {
                    Vec::new()
                };
                me.hold(x, &mods, true)?;
                x.key(code, true)?;
                x.key(code, false)?;
                me.hold(x, &mods, false)?;
            }
            x.sync()
        })
    }

    /// Move keyboard focus to `node` before shared keystrokes, and report
    /// whether it took — the Linux reading of macOS's `FocusCheck`.
    fn focus_for_keys(&mut self, node: &LinuxNode) -> Result<FocusCheck> {
        let c = self.a11y()?;
        let grabbed = self.block(async {
            let comp = bus::component(&c, &node.handle).await?;
            timed(comp.grab_focus()).await
        });
        let focused_now = self
            .block(async {
                let acc = bus::accessible(&c, &node.handle).await?;
                timed(acc.get_state()).await
            })
            .map(|s| s.contains(State::Focused))
            .ok();
        let (accepted, err) = match grabbed {
            Ok(b) => (b, (!b).then(|| "the app declined GrabFocus".to_string())),
            Err(e) => (false, Some(e.to_string())),
        };
        let state = match focused_now {
            Some(true) => FocusState::Verified,
            Some(false) if accepted => FocusState::Mismatched,
            _ => FocusState::Unverified,
        };
        if state == FocusState::Mismatched
            && cua_core::safety::truthy(
                std::env::var("CUA_KEY_STRICT_FOCUS")
                    .as_deref()
                    .unwrap_or(""),
            )
        {
            return Err(CoreError::FocusMismatch {
                what: "keys".into(),
                addressed: node.describe(),
                focused: "another element".into(),
            });
        }
        Ok(FocusCheck {
            state,
            focus_write_accepted: accepted,
            focus_write_error: err,
            focused_instead: None,
        })
    }
}

/// X button number for a mouse button.
fn x_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

/// The modifier keysyms to hold for `m`.
fn mod_keysyms(m: Modifiers) -> Result<Vec<u32>> {
    if m.contains(Modifiers::MaskSecondaryFn) {
        return Err(unsupported("fn modifier", "`fn` has no X11 keysym"));
    }
    let mut out = Vec::new();
    for (bit, sym) in [
        (Modifiers::MaskControl, keys::CONTROL_L),
        (Modifiers::MaskAlternate, keys::ALT_L),
        (Modifiers::MaskShift, keys::SHIFT_L),
        (Modifiers::MaskCommand, keys::SUPER_L),
    ] {
        if m.contains(bit) {
            out.push(sym);
        }
    }
    Ok(out)
}

/// First occurrence of `prefix + needle + suffix`, as a char range over the
/// needle alone — the rule `cua_ax::find_text_range` applies on macOS. AT-SPI
/// offsets count characters, so no UTF-16 conversion is needed here.
pub(crate) fn find_range(
    hay: &str,
    needle: &str,
    prefix: &str,
    suffix: &str,
) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let pattern = format!("{prefix}{needle}{suffix}");
    let byte = hay.find(&pattern)? + prefix.len();
    Some((hay[..byte].chars().count(), needle.chars().count()))
}

/// An executable on `PATH`, or an absolute path to one. Strict on purpose,
/// like macOS's `LaunchSpec`: no fuzzy matching for something that starts a
/// process.
fn locate_executable(spec: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &std::path::Path| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if spec.contains('/') {
        let p = std::path::PathBuf::from(spec);
        return (p.is_absolute() && is_exec(&p)).then_some(p);
    }
    if spec.is_empty() || spec.contains(char::is_whitespace) {
        return None;
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(spec))
            .find(|p| is_exec(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_range_counts_characters_and_honours_anchors() {
        assert_eq!(find_range("hello world", "world", "", ""), Some((6, 5)));
        assert_eq!(find_range("a cat, a cat!", "cat", "", "!"), Some((9, 3)));
        assert_eq!(find_range("한글 text", "text", "", ""), Some((3, 4)));
        assert_eq!(find_range("abc", "", "", ""), None);
        assert_eq!(find_range("abc", "z", "", ""), None);
    }

    #[test]
    fn modifiers_map_to_x_keysyms_and_fn_is_refused() {
        let m = Modifiers::MaskControl | Modifiers::MaskShift;
        assert_eq!(
            mod_keysyms(m).unwrap(),
            vec![keys::CONTROL_L, keys::SHIFT_L]
        );
        assert!(mod_keysyms(Modifiers::MaskSecondaryFn).is_err());
    }

    #[test]
    fn the_primary_action_is_found_by_name_not_position() {
        let acts = vec!["expand or contract".to_string(), "activate".to_string()];
        assert_eq!(preferred_action(&acts, CLICK_ACTIONS), Some(1));
        assert_eq!(preferred_action(&[], CLICK_ACTIONS), None);
    }
}
