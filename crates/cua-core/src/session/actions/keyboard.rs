//! `Inner` methods for keyboard responsibilities.

use super::*;

impl Inner {
    /// Refuse the send when strict focus mode is on and the app names a
    /// different element as focused. Nothing has been posted at this point.
    pub(super) fn enforce_strict_focus(
        &self,
        focus: &FocusCheck,
        what: &str,
        addressed: &str,
    ) -> Result<()> {
        if DELIVERY.keyboard_capability().strict_focus && focus.state == FocusState::Mismatched {
            return Err(CoreError::FocusMismatch {
                what: what.to_string(),
                addressed: addressed.to_string(),
                focused: focus
                    .focused_instead
                    .clone()
                    .unwrap_or_else(|| "another element".into()),
            });
        }
        Ok(())
    }

    /// Best-effort preparation before any pid-routed keyboard event: try to
    /// move accessibility focus onto the addressed element, then send the same
    /// activation notices [`Inner::pid_click_result`] sends, in case the
    /// target's willingness to accept a synthesized keystroke depends on
    /// believing it is frontmost the same way its willingness to accept a
    /// synthesized click does.
    ///
    /// Neither step *gates* the send. `AXFocused` is not settable on every
    /// element — Terminal's own text view is a measured case — and a failure
    /// here is not a reason to refuse the keystrokes that follow: the element
    /// may already have focus, or the app may accept real key events
    /// regardless of what accessibility reports.
    ///
    /// What changed is that the outcome is no longer discarded. The returned
    /// [`FocusCheck`] carries the `AXFocused` write result *and* a read-back
    /// of the app's own `AXFocusedUIElement`, so a caller is told whether the
    /// element it addressed is the one the keys will reach instead of being
    /// handed an unqualified `Ok`. The read-back is the load-bearing half: a
    /// successful write does not prove the AppKit first responder moved, and
    /// only the app can say where it actually is.
    pub(super) fn prime_for_pid_keyboard(&mut self, info: &AppInfo, el: &Element) -> FocusCheck {
        let write = el.set_bool(cua_ax::attr::FOCUSED, true);
        let window_number = self
            .snapshots
            .get(&info.pid)
            .and_then(|snap| snap.window.as_ref())
            .map(|w| w.id as isize);
        let believes_frontmost = {
            let app_el = Element::for_pid(info.pid);
            move || app_el.bool("AXFrontmost").unwrap_or(false)
        };
        cua_hid::prime_keyboard_target(info.pid, window_number, &believes_frontmost);
        // Read focus *after* the activation notices, not before: making the
        // target believe it is active is part of what can move its first
        // responder, so a reading taken earlier would describe a state the
        // keystrokes were never delivered into.
        self.check_focus(info.pid, el, write)
    }

    pub(super) fn press_key(
        &mut self,
        query: &str,
        target: Target,
        key: &str,
    ) -> Result<ActionResult> {
        // Capability first, before `require_trusted`/`resolve`, the same
        // shape the old AX-only path used — so a key this build cannot
        // express at all is reported without needing a grant or a snapshot,
        // and so the pid tier's own tests can run with no permissions.
        //
        // Pid-only by default, no AX attempt at all, because accessibility has
        // no vocabulary for a key press: `AXConfirm` for Return, `AXCancel` for
        // Escape, and then nothing. A real event is the only thing `cmd+shift+p`
        // could ever become, so events are the only tier. `cua_hid::parse_chord`
        // understands arbitrary chords (`cmd+shift+p`, `ctrl+alt+delete`,
        // plain letters and digits) — the capability DESIGN.md §1/§9 listed as
        // permanently absent ("arbitrary chord — no verb exists, still
        // refused") is reachable now for exactly that reason.
        //
        // `CUA_KEY_AX_ONLY=1` is the way back to the old, AX-verb-only path
        // (`return`/`escape`/`up`/`down` only, nothing else, no synthesized
        // input at all) — see [`pid_keyboard_enabled`].
        if DELIVERY.keyboard_capability().strategy == KeyboardStrategy::Pid {
            let chord = cua_hid::parse_chord(key).map_err(|e| CoreError::KeyChordUnparseable {
                key: key.to_string(),
                reason: e.to_string(),
            })?;

            cua_ax::require_trusted()?;
            let (info, el, desc) = self.resolve(query, &target)?;
            let before = self.watch(info.pid);

            if !cua_hid::skylight_available() {
                return Err(CoreError::PidKeyUnavailable {
                    key: key.to_string(),
                    reason: "SLEventPostToPid is not available on this macOS version".into(),
                });
            }
            let focus = self.prime_for_pid_keyboard(&info, &el);
            // Refuse *before* posting, when asked to. `unverified` still
            // delivers: the app publishing no focused element is silence, not
            // evidence of a miss.
            self.enforce_strict_focus(&focus, &format!("key `{key}`"), &desc)?;
            cua_hid::press_chord_background_pid(info.pid, &chord).map_err(|e| {
                CoreError::PidKeyUnavailable {
                    key: key.to_string(),
                    reason: e.to_string(),
                }
            })?;
            let changed = self.changed_since(info.pid, before);
            return Ok(ActionResult {
                verb: format!("pid-routed key `{key}`"),
                target: desc,
                ui_changed: changed.changed,
                popups: changed.popups,
                delivery: Delivery::PidKey,
                point: element_point(&el),
                overlay_target: self.overlay_target(info.pid),
                state: None,
                focus: Some(focus),
            });
        }

        // Legacy path (`CUA_KEY_AX_ONLY=1`): only the handful of verbs AX
        // actually expresses (`AXConfirm`, `AXCancel`,
        // `AXIncrement`/`AXDecrement`), no synthesized input at all.
        let Some(ax_verb) = ax_verb_for_key(key) else {
            return Err(CoreError::KeyNoAccessibilityEquivalent {
                key: key.to_string(),
            });
        };

        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.watch(info.pid);

        let available = el.actions();
        if !available.iter().any(|a| a == ax_verb) {
            return Err(CoreError::KeyVerbUnsupported {
                key: key.to_string(),
                verb: ax_verb,
                available: format!("{available:?}"),
            });
        }

        el.perform(ax_verb)?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult::ax_at(
            format!("{ax_verb} (for {key})"),
            desc,
            changed,
            element_point(&el),
        )
        .with_overlay_target(self.overlay_target(info.pid)))
    }
}
