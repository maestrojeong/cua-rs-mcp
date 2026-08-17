//! `Inner` methods for pointer responsibilities.

use super::*;

impl Inner {
    pub(super) fn click(
        &mut self,
        query: &str,
        target: Target,
        mouse: MouseOptions,
    ) -> Result<ActionResult> {
        let count = mouse.count;
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.watch(info.pid);
        let expected = match &target {
            Target::Index { index, .. } => self
                .snapshots
                .get(&info.pid)
                .map(|snapshot| (*index, snapshot_tokens(&snapshot.nodes, *index))),
            Target::Point { .. } => None,
        };

        // `ax_first()` (default `false`) is now a pure legacy switch: it
        // restores the *original* tier order (AXPress, then pid only when no
        // AX verb exists at all). The default has moved past "try pid first,
        // then retry through AX" to no AX fallback whatsoever: accessibility
        // decides *where* to click and never delivered the click, and it cannot
        // express a click count at all, so a double-click was already pid-only.
        // One route for every click removes the case analysis instead of adding
        // a mechanism. Retrying through AX after a pid failure reintroduces
        // exactly the app-specific AX quirks (`AXPress` advertised but
        // ignored, a stale-handle press) that motivated moving to pid at all.
        // The legacy AX-first order can only serve a plain single left click:
        // `AXPress` has no notion of a click count, a button, or a held
        // modifier, so a right click or a ⌘-click has nothing to fall back
        // *to* and goes straight to the pid tier whatever this switch says.
        if DELIVERY.pointer_capability().strategy == PointerStrategy::AxFirst
            && count <= 1
            && mouse.button == MouseButton::Left
            && mouse.modifiers.is_empty()
        {
            match el.activate() {
                Ok(verb) => {
                    let changed = self.changed_since(info.pid, before);
                    return Ok(ActionResult::ax_at(verb, desc, changed, element_point(&el))
                        .with_overlay_target(self.overlay_target(info.pid)));
                }
                Err(ax_err) => {
                    if !matches!(ax_err, cua_ax::AxError::Unsupported { .. }) {
                        return Err(CoreError::Ax(ax_err));
                    }
                    return match self.pid_click_result(&info, &el, desc, mouse, expected, before) {
                        Ok(result) => Ok(result),
                        Err(PidFailure::Fatal(err)) => Err(err),
                        Err(PidFailure::Retryable(reason)) => Err(CoreError::PidClickUnavailable {
                            original: ax_err,
                            reason,
                        }),
                    };
                }
            }
        }

        match self.pid_click_result(&info, &el, desc, mouse, expected, before) {
            Ok(result) => Ok(result),
            Err(PidFailure::Fatal(err)) => Err(err),
            Err(PidFailure::Retryable(pid_reason)) => {
                Err(CoreError::PidClickFailed { reason: pid_reason })
            }
        }
    }

    /// The pid-routed tier of a click, decoupled from which tier is tried
    /// first. `Ok` on success; `Err` distinguishes a failure the caller may
    /// still recover from by trying accessibility ([`PidFailure::Retryable`])
    /// from one that must not be retried at all ([`PidFailure::Fatal`]).
    ///
    /// The only fatal case is a detected [`CoreError::TargetChanged`]: falling
    /// back to `AXPress` after finding that the live element no longer matches
    /// what the snapshot described would press whatever now occupies a
    /// recycled handle, which is exactly the staleness bug §3 of DESIGN.md
    /// calls "the single worst bug this system can have". Every other failure
    /// here is a capability or transient-state gap (no SkyLight, no window,
    /// element publishes no point) and is safe to retry through AX.
    pub(super) fn pid_click_result(
        &mut self,
        info: &AppInfo,
        el: &Element,
        desc: String,
        mouse: MouseOptions,
        expected: Option<(usize, HashSet<String>)>,
        before: Watch,
    ) -> std::result::Result<ActionResult, PidFailure> {
        let count = mouse.count;
        // `AXActivationPoint` is the app's answer to "where is this element
        // clicked", and it is not always the middle of its frame, so prefer it
        // when available.
        let (x, y) = match el.activation_point() {
            Some(p) => (p.x, p.y),
            None => {
                let frame = el.frame().ok_or_else(|| {
                    PidFailure::Retryable(
                        "the element publishes neither AXActivationPoint nor AXFrame".into(),
                    )
                })?;
                (
                    frame.origin.x + frame.size.width / 2.0,
                    frame.origin.y + frame.size.height / 2.0,
                )
            }
        };

        if !cua_hid::skylight_available() {
            return Err(PidFailure::Retryable(
                "SLEventPostToPid is not available on this macOS version".into(),
            ));
        }

        // Detect staleness before anything else fires: an index whose element
        // has changed identity since the snapshot must never be pressed by
        // either tier, so this check has to happen regardless of which tier
        // ends up doing the pressing.
        if let Some((index, expected)) = &expected {
            if !expected.is_empty() {
                let found = live_tokens(el);
                if !tokens_still_present(expected, &found) {
                    let mut gone: Vec<&String> = expected.difference(&found).collect();
                    gone.sort();
                    return Err(PidFailure::Fatal(CoreError::TargetChanged {
                        index: *index,
                        expected: format!("{gone:?}"),
                        found: format!("{:?}", sorted(&found)),
                    }));
                }
            }
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
            .ok_or_else(|| {
                PidFailure::Retryable(
                    "the snapshot has no verified ScreenCaptureKit window id; enable Screen \
                     Recording and take a fresh snapshot"
                        .into(),
                )
            })?;
        let live_windows = cua_capture::list_windows().map_err(|e| {
            PidFailure::Retryable(format!(
                "could not revalidate the target window immediately before input: {e}"
            ))
        })?;
        // Free: this list was fetched to check the window id, and it is also the
        // sharpest possible answer to "what was already open before the click",
        // taken microseconds before the event rather than at the last read.
        let before = before.with_windows(&live_windows, info.pid);
        let live_window =
            current_window_for_pid_click(&live_windows, &snapshot_window, info.pid, x, y)
                .map_err(PidFailure::Retryable)?;
        let wid = live_window.id;
        let window_local = (
            x - live_window.frame.origin.x,
            y - live_window.frame.origin.y,
        );

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
        let assist = window_focus_assist(info.pid, &live_window);
        cua_hid::click_background_pid(
            cua_hid::PidClick {
                pid: info.pid,
                point: (x, y),
                window_local,
                wid,
                count,
                button: mouse.button,
                modifiers: mouse.modifiers,
            },
            assist,
            &believes_frontmost,
        )
        .map_err(|e| PidFailure::Retryable(e.to_string()))?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!(
                "SkyLight pid-routed {count}-click ({}) at ({x:.0}, {y:.0})",
                mouse.describe()
            ),
            target: desc,
            ui_changed: changed.changed,
            popups: changed.popups,
            delivery: Delivery::Pid,
            point: Some((x, y)),
            overlay_target: Some((wid, info.pid)),
            state: None,
            focus: None,
        })
    }

    /// Click a point in a window that accessibility does not describe.
    ///
    /// This is the one action in cua-rs that does not resolve an element first,
    /// and it exists because a canvas is not a bug in accessibility — a
    /// custom-drawn map, chart, or game view genuinely publishes no children, so
    /// there is no element for `click` to find and nothing a better tree walk
    /// would reveal. An agent looking at the screenshot has a pixel, and until
    /// now that was a dead end by policy rather than by capability: [`PidClick`]
    /// is `{pid, point, window_local, wid, count}` and never contained an
    /// `Element`. Accessibility is how cua-rs normally decides *where* to click;
    /// it was never how the click is delivered.
    ///
    /// It is a distinct entry point and never a fallback from [`Inner::click`].
    /// "The point covers nothing" is exactly the shape of a typo, and clicking a
    /// typo blindly is the worst outcome available here, so the caller has to
    /// ask for this by name.
    ///
    /// # Transient UI, and the limit found by trying it
    ///
    /// Everything above describes a canvas. A pop-up is a second case, and the
    /// reason this accepts a pop-up's window id at all: measured on KakaoTalk's
    /// chat-room hamburger, the menu is a window of the app's own process at
    /// level 101 that accessibility does not describe *at all* — the application
    /// element has only its two `AXMenuBar` children, and
    /// `AXUIElementCopyElementAtPosition` inside the menu's frame returns the
    /// menu bar as a fallback. No element to find, no better walk to make, no AX
    /// verb to send. A window-local coordinate is the only addressing that
    /// exists, so refusing the window id — which is what the old level-3 cap
    /// did — left the one kind of UI accessibility cannot see as the one kind
    /// cua-rs could not aim at either.
    ///
    /// What aiming at it turned out to buy is less than hoped, and saying so is
    /// part of shipping it. A pid-routed click at a menu row was measured twice,
    /// on two different rows of that menu: the event arrives — the menu closes —
    /// and **nothing is selected**. The menu's own state was unchanged
    /// afterwards, including on a run where the human's real pointer was
    /// hovering a different row, so it does not pick the wrong item either; it
    /// picks none. That is consistent with the other thing the menu does: it
    /// draws itself at the *real* cursor rather than at the point the event
    /// carried. A menu tracks the pointer, and cua-rs does not move the pointer,
    /// which puts menu-row selection in the same bucket as §9's
    /// pointer-position spoofing — out, permanently, by the same rule.
    ///
    /// So the honest ordering for a menu is `press_key` with the item's own key
    /// equivalent first, which was measured to work (⌘T on that menu toggled the
    /// chat window's always-on-top state). This call remains right for a pop-up
    /// that is not a menu — a popover or panel whose views handle events
    /// normally — and for dismissing a menu. cua-rs reads no shortcuts out of
    /// the image and does no OCR; the caller reads the screenshot and decides.
    ///
    /// # Coordinates are window-local
    ///
    /// `x`/`y` are measured from the window's top-left corner, in points — the
    /// same space as the screenshot `get_app_state` returns, divided by its
    /// `scale`. Screen coordinates were the obvious alternative and are worse:
    /// the caller would have to add the window origin itself, and the sum would
    /// silently address the wrong pixel the moment the user moved the window
    /// between the read and the click. Window-local coordinates are re-anchored
    /// to the live origin here, immediately before posting, so a window move
    /// between the two calls is harmless rather than invisible.
    ///
    /// For the same reason this does not consult the snapshot's geometry and so
    /// has no reason to reject an `acted_on` snapshot: there is no element whose
    /// position could have gone stale. What it does require is that a snapshot
    /// exists and describes this very window, which is the only available
    /// evidence that the caller is aiming at something it actually looked at.
    ///
    /// # What this cannot do
    ///
    /// Verify. There is no element to re-read, so the post-action delta is the
    /// only feedback available, and on a canvas even that is empty. A successful
    /// return means the events were accepted for delivery to that pixel of that
    /// window — not that anything was there.
    pub(super) fn click_in_window(
        &mut self,
        query: &str,
        at: WindowPixel,
        mouse: MouseOptions,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let WindowPixel {
            window_id: wid,
            x,
            y,
            snapshot_id,
        } = at;
        let count = mouse.count;
        let info = apps::resolve_app(query)?;
        self.check_coordinate_generation(&info, snapshot_id, (x, y))?;
        let refuse = |reason: String| CoreError::WindowClickRefused {
            app: info.name.clone(),
            wid,
            x,
            y,
            reason,
        };

        if !cua_hid::skylight_available() {
            return Err(refuse(
                "SLEventPostToPid is not available on this macOS version, and cua-rs will not fall back to moving the real pointer".into(),
            ));
        }
        if x < 0.0 || y < 0.0 {
            return Err(refuse(
                "coordinates are window-local and measured from the window's top-left corner, so neither can be negative".into(),
            ));
        }

        // Gate 1: the caller must have read this window through cua-rs. Without
        // an element, the window id is the whole of the addressing, and an id
        // taken from anywhere else is an id whose contents the caller has never
        // seen.
        //
        // A pop-up satisfies it differently, and has to. Transient UI is not in
        // any snapshot's tree — accessibility does not describe it — so
        // "the window this app's last read was of" can never be the menu. What
        // stands in for it is that the window is a live pop-up *of the same
        // process the caller has read*: cua-rs reported that pop-up's id in the
        // state and in the result of the action that opened it, which is the same
        // evidence, arriving by the only route available.
        //
        // Re-enumerated once here rather than trusted from the snapshot, because
        // a pid-addressed event carrying a stale window id is exactly the thing
        // that must not be sent. p50 ~28 ms with a couple of hundred windows
        // live; it is the only enumeration this path makes before the event.
        let live_windows = cua_capture::list_windows()
            .map_err(|e| refuse(format!("could not revalidate the window before input: {e}")))?;
        let snapshot_wid = self
            .snapshots
            .get(&info.pid)
            .and_then(|snap| snap.window.as_ref())
            .map(|w| w.id);
        let is_live_popup = live_windows
            .iter()
            .any(|w| w.id == wid && w.pid == info.pid && w.is_transient_popup());
        match snapshot_wid {
            Some(seen) if seen == wid => {}
            _ if is_live_popup && snapshot_wid.is_some() => {}
            Some(seen) => {
                return Err(refuse(format!(
                    "the last get_app_state of this app read window {seen}, not {wid}, and {wid} is not a pop-up this app currently has open. Read the window you mean to click first"
                )));
            }
            None => {
                return Err(refuse(
                    "no verified window has been read for this app. Call get_app_state first and pass the window_id it reports".into(),
                ));
            }
        }

        // Gate 2: the window still exists, still belongs to this pid, and is
        // either ordinary content or transient UI this app put up — never the
        // desktop, the menu bar, cua-rs's own overlay or a 1x1 tracking window.
        let live_window =
            live_window_for_pid_click(&live_windows, wid, info.pid).map_err(refuse)?;

        // Only now does a screen point exist: the live origin is what the
        // window-local coordinates are measured against.
        let (sx, sy) = (
            live_window.frame.origin.x + x,
            live_window.frame.origin.y + y,
        );
        screen_point_inside(&live_window, sx, sy).map_err(|frame| {
            refuse(format!(
                "the window is currently {:.0}x{:.0} points, so ({x:.0}, {y:.0}) falls outside it (frame {frame})",
                live_window.frame.size.width, live_window.frame.size.height
            ))
        })?;

        let before = self.watch(info.pid).with_windows(&live_windows, info.pid);
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        let assist = window_focus_assist(info.pid, &live_window);
        cua_hid::click_background_pid(
            cua_hid::PidClick {
                pid: info.pid,
                point: (sx, sy),
                window_local: (x, y),
                wid,
                count,
                button: mouse.button,
                modifiers: mouse.modifiers,
            },
            assist,
            &believes_frontmost,
        )
        .map_err(|e| refuse(e.to_string()))?;

        let settled = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!(
                "SkyLight pid-routed {count}-click ({}) at window-local ({x:.0}, {y:.0})",
                mouse.describe()
            ),
            target: format!(
                "window {wid} of {}{} at no element — the caller aimed this",
                info.name,
                if is_live_popup {
                    ", a pop-up accessibility cannot see into"
                } else {
                    ""
                }
            ),
            ui_changed: settled.changed,
            popups: settled.popups,
            delivery: Delivery::PidNoElement,
            point: Some((sx, sy)),
            overlay_target: Some((wid, info.pid)),
            state: None,
            focus: None,
        })
    }

    /// Press at one point, move through interpolated intermediate points, and
    /// release at another.
    ///
    /// # Why both endpoints are checked against one window
    ///
    /// A pid-routed event carries a window number, and every event of one
    /// gesture has to carry the same one — the window server routes on it, and
    /// a drag whose up lands in a different window than its down is not a drag
    /// anywhere. So a drag is confined to the window this app's most recent
    /// `get_app_state` read, both endpoints must fall inside its *live* frame,
    /// and a request spanning two windows is refused rather than interpolated
    /// across the boundary.
    ///
    /// # What it can and cannot promise
    ///
    /// The events were delivered, in order, and the release was sent even if a
    /// move failed partway — see [`cua_hid::drag_background_pid`]. Whether the
    /// target implemented a drag at all, and whether it accepted this one, is
    /// what `return_state` is for. Where either endpoint is a raw pixel the
    /// result is labelled `pid (no element)`, because then nothing verified
    /// there was anything at that end.
    pub(super) fn drag(
        &mut self,
        query: &str,
        from: PointerLocation,
        to: PointerLocation,
        mouse: MouseOptions,
        snapshot_id: Option<u64>,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let (from_name, to_name) = (describe_location(&from), describe_location(&to));
        let refuse = |reason: String| CoreError::DragRefused {
            app: info.name.clone(),
            from: from_name.clone(),
            to: to_name.clone(),
            reason,
        };

        if !cua_hid::skylight_available() {
            return Err(refuse(
                "SLEventPostToPid is not available on this macOS version, and cua-rs will not fall back to moving the real pointer".into(),
            ));
        }
        let live = self.live_snapshot_window(&info).map_err(&refuse)?;
        let origin = self
            .aim(query, &info, &live, &from, snapshot_id)
            .map_err(&refuse)?;
        let destination = self
            .aim(query, &info, &live, &to, snapshot_id)
            .map_err(&refuse)?;

        // A drag onto its own origin is a click with extra steps, and almost
        // always a caller that resolved both ends to the same element by
        // mistake. Refusing names the mistake; delivering would leave a
        // press-and-release the caller will read as a successful drag.
        if origin.point == destination.point {
            return Err(refuse(format!(
                "both ends resolve to the same point ({:.0}, {:.0}). A drag needs two different points; use click if a press and release in one place is what you meant",
                origin.point.0, origin.point.1
            )));
        }

        let before = self.watch(info.pid);
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        let assist = window_focus_assist(info.pid, &live);
        let steps = cua_hid::drag_step_count(origin.point, destination.point);
        cua_hid::drag_background_pid(
            cua_hid::PidDrag {
                pid: info.pid,
                wid: live.id,
                window_origin: (live.frame.origin.x, live.frame.origin.y),
                origin: origin.point,
                destination: destination.point,
                button: mouse.button,
                modifiers: mouse.modifiers,
            },
            assist,
            &believes_frontmost,
        )
        .map_err(|e| refuse(e.to_string()))?;

        let changed = self.changed_since(info.pid, before);
        let verified = origin.from_element && destination.from_element;
        Ok(ActionResult {
            verb: format!(
                "SkyLight pid-routed {} drag ({}) from ({:.0}, {:.0}) to ({:.0}, {:.0}) through {steps} interpolated moves",
                mouse.button.as_str(),
                mouse.describe(),
                origin.point.0,
                origin.point.1,
                destination.point.0,
                destination.point.1,
            ),
            target: format!("{} → {}", origin.desc, destination.desc),
            ui_changed: changed.changed,
            popups: changed.popups,
            delivery: if verified {
                Delivery::Pid
            } else {
                Delivery::PidNoElement
            },
            point: Some(destination.point),
            overlay_target: Some((live.id, info.pid)),
            // A drag is addressed at two points; it moves no keyboard focus, so
            // there is no focus verdict to report.
            focus: None,
            state: None,
        })
    }

    /// Tell one window the pointer arrived at a point, so hover-revealed UI
    /// appears and is readable in the next snapshot.
    ///
    /// # The real pointer does not move
    ///
    /// This is a synthesized `mouseMoved` addressed to the target process. The
    /// human's cursor stays exactly where they left it, which is the point of
    /// the whole project — and also the honest limit of this tool. An app that
    /// reads where the pointer *is* (`NSEvent.mouseLocation`, a poll of the
    /// cursor position) rather than where the event says it went will not
    /// respond, and no version of this call can make it.
    ///
    /// # What it is measured to drive
    ///
    /// Web content, in both Chromium and WebKit: a `:hover` rule fires and the
    /// page reads back the exact coordinate the event carried. A Finder list row
    /// did not respond at all — not in the tree and not in its pixels — while a
    /// click at the same point in the same run selected it. DESIGN §11 has both
    /// readings; the population between those two poles is unknown.
    ///
    /// Nothing is pressed, so unlike a click this never synthesizes the
    /// activation-assist click on the window's own activation point; only the
    /// two activation *notices* are sent.
    pub(super) fn hover(
        &mut self,
        query: &str,
        at: PointerLocation,
        modifiers: Modifiers,
        snapshot_id: Option<u64>,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let refuse = |reason: String| CoreError::PointerEventRefused {
            app: info.name.clone(),
            what: "hover (mouseMoved) event",
            reason,
        };
        if !cua_hid::skylight_available() {
            return Err(refuse(
                "SLEventPostToPid is not available on this macOS version, and cua-rs will not fall back to moving the real pointer".into(),
            ));
        }
        let live = self.live_snapshot_window(&info).map_err(&refuse)?;
        let aim = self
            .aim(query, &info, &live, &at, snapshot_id)
            .map_err(&refuse)?;

        let before = self.watch(info.pid);
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        cua_hid::move_mouse_background_pid(
            cua_hid::PidMouseMove {
                pid: info.pid,
                point: aim.point,
                window_local: aim.window_local,
                wid: live.id,
                modifiers,
            },
            &believes_frontmost,
        )
        .map_err(|e| refuse(e.to_string()))?;

        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!(
                "SkyLight pid-routed mouseMoved to ({:.0}, {:.0}) — the real pointer did not move",
                aim.point.0, aim.point.1
            ),
            target: aim.desc,
            ui_changed: changed.changed,
            popups: changed.popups,
            delivery: if aim.from_element {
                Delivery::Pid
            } else {
                Delivery::PidNoElement
            },
            point: Some(aim.point),
            overlay_target: Some((live.id, info.pid)),
            // Hover presses nothing and focuses nothing, so there is no focus
            // verdict to report.
            focus: None,
            state: None,
        })
    }
}
