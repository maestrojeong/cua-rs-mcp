//! `Inner` methods for scroll responsibilities.

use super::*;

impl Inner {
    /// Scroll an element, through accessibility where it advertises a scroll
    /// action and through a pid-routed wheel event where it does not.
    ///
    /// Both tiers are kept because neither dominates. `AXScroll*ByPage` needs no
    /// coordinate, lets the app decide what a page of its own content is, and
    /// cannot be swallowed by whatever subview happens to sit under a point — so
    /// it stays the default for a page-shaped request on an element that offers
    /// it. But the elements an agent most often needs to scroll offer nothing:
    /// an Electron list, a canvas, a web area inside a native shell all publish
    /// a frame and no actions, and until now that was a refusal rather than a
    /// mechanism. A `scrollWheel` event delivered on the same pid route as a
    /// click reaches all of them.
    ///
    /// The tier that ran is named in `verb` and reflected in `delivery`, because
    /// the two do not fail the same way: an AX page scroll that returns success
    /// really did call the app's own scroller, while a wheel event is only
    /// known to have been delivered to a point.
    pub(super) fn scroll(
        &mut self,
        query: &str,
        target: Target,
        dir: ScrollDir,
        amount: ScrollAmount,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let verb = dir.verb();
        let advertises = el.actions().iter().any(|a| a == verb);

        match scroll_tier(amount, advertises) {
            ScrollTier::Ax => {
                let before = self.watch(info.pid);
                let ScrollAmount::Pages(pages) = amount else {
                    unreachable!("scroll_tier only chooses Ax for a page request")
                };
                for _ in 0..pages.max(1) {
                    el.perform(verb)?;
                }
                let changed = self.changed_since(info.pid, before);
                Ok(ActionResult::ax_at(verb, desc, changed, element_point(&el))
                    .with_overlay_target(self.overlay_target(info.pid)))
            }
            ScrollTier::Wheel => {
                // A caller who named a coordinate has said where they want the
                // wheel, and that is better information than the element's own
                // point: `resolve` only used the coordinate to find out *which*
                // element covers it, and a scroll container is often covered by
                // one element for its whole scrollable extent. Discarding it, as
                // this used to, meant `Target::Point` scrolled somewhere the
                // caller never asked about.
                let asked = match target {
                    Target::Point { x, y, .. } => Some((x as f64, y as f64)),
                    Target::Index { .. } => None,
                };
                self.wheel_scroll(
                    &info,
                    &el,
                    desc,
                    WheelRequest {
                        dir,
                        amount,
                        advertises,
                        asked,
                    },
                )
            }
        }
    }

    /// The event tier of a scroll: a pid-routed `scrollWheel` at the element's
    /// own point.
    pub(super) fn wheel_scroll(
        &mut self,
        info: &AppInfo,
        el: &Element,
        desc: String,
        req: WheelRequest,
    ) -> Result<ActionResult> {
        let WheelRequest {
            dir,
            amount,
            advertises,
            asked,
        } = req;
        let refuse = |reason: String| CoreError::PointerEventRefused {
            app: info.name.clone(),
            what: "scroll wheel event",
            reason,
        };
        if !DELIVERY.scroll_capability().wheel_enabled {
            return Err(refuse(format!(
                "cua-rs refuses this rather than pretending: a pid-routed scrollWheel event is \
                 delivered and scrolls nothing. Measured against the window's own pixels on a \
                 native AXScrollArea and on Chromium web content, in both pixel and line units, \
                 while a pid-routed `pagedown` keystroke scrolled the same window in the same run \
                 — so the scroll event is what fails, not the routing. Use press_key with \
                 `pagedown`, `pageup`, `down` or `up` on this element instead{}. Set \
                 CUA_WHEEL_SCROLL=1 to deliver it anyway and re-run the experiment",
                if advertises {
                    ", or ask in pages rather than points so the accessibility scroll action this \
                     element does advertise is used"
                } else {
                    ""
                }
            )));
        }
        if !cua_hid::skylight_available() {
            return Err(refuse(
                "SLEventPostToPid is not available on this macOS version, and cua-rs will not fall back to moving the real pointer".into(),
            ));
        }
        let own = element_point(el).ok_or_else(|| {
            refuse(format!(
                "this element advertises {}, and it publishes neither AXActivationPoint nor AXFrame, so there is no point to aim a wheel event at",
                if advertises { "a scroll action but the caller asked in points" } else { "no scroll action" }
            ))
        })?;
        let live = self.live_snapshot_window(info).map_err(refuse)?;

        // A scrollable element's frame is not its viewport. A web area's frame
        // is the whole document and a long list's frame is all of its rows, so
        // the centre of either can sit far below the window that shows it —
        // measured on Chrome, where the aim came out at the bottom edge of the
        // display. A wheel event delivered there scrolls nothing, and reports
        // success. So the point is pulled back into the part of the element the
        // window actually shows.
        let (ax, ay) = asked.unwrap_or(own);
        let (x, y) = clamp_into_window(el.frame(), &live, ax, ay);

        screen_point_inside(&live, x, y).map_err(|frame| {
            refuse(format!(
                "the wheel point ({x:.0}, {y:.0}) is outside the current frame of window {} ({frame}); the AX element and window snapshot drifted apart. Call get_app_state again",
                live.id
            ))
        })?;

        let points = match amount {
            ScrollAmount::Points(p) => p as i32,
            ScrollAmount::Pages(pages) => {
                page_points(el.frame().map(|f| f.size.height)) * pages.max(1) as i32
            }
        };
        let (delta_y, delta_x) = dir.wheel_delta(points);
        let before = self.watch(info.pid);
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        cua_hid::scroll_background_pid(
            cua_hid::PidScroll {
                pid: info.pid,
                point: (x, y),
                window_local: (x - live.frame.origin.x, y - live.frame.origin.y),
                wid: live.id,
                delta_y,
                delta_x,
                unit: cua_hid::ScrollUnit::Pixel,
                modifiers: Modifiers::empty(),
            },
            &believes_frontmost,
        )
        .map_err(|e| refuse(e.to_string()))?;

        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!(
                "SkyLight pid-routed scrollWheel {} by {points} points at ({x:.0}, {y:.0}){}",
                dir.as_str(),
                if advertises {
                    ""
                } else {
                    " (this element advertises no AXScroll action)"
                }
            ),
            target: desc,
            ui_changed: changed.changed,
            popups: changed.popups,
            delivery: Delivery::Pid,
            point: Some((x, y)),
            overlay_target: Some((live.id, info.pid)),
            // A wheel event is addressed at a point, not at a first responder:
            // nothing here moves keyboard focus, so there is no focus verdict to
            // report. `None` says "not applicable", not "unverified".
            focus: None,
            state: None,
        })
    }
}
