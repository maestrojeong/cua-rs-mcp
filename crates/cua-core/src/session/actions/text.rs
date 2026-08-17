//! `Inner` methods for text responsibilities.

use super::*;

impl Inner {
    pub(super) fn set_value(
        &mut self,
        query: &str,
        target: Target,
        value: &str,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.watch(info.pid);
        // `AXValue=` stays the whole mechanism, on purpose. This is the one
        // operation accessibility expresses better than events can: a single
        // write replaces the whole string atomically and is addressed at *this*
        // element. The same text delivered as keystrokes is a long stream landing
        // on whatever the target process's first responder happens to be, one
        // character at a time, which multiplies the pid tier's focus risk by the
        // length of the string and gives nothing back. So the tier decision made
        // for `click`/`press_key` (pid, no AX fallback) deliberately does not
        // transfer here.
        el.set_string(cua_ax::attr::VALUE, value)?;
        let changed = self.changed_since(info.pid, before);
        Ok(
            ActionResult::ax_at("AXValue=", desc, changed, element_point(&el))
                .with_overlay_target(self.overlay_target(info.pid)),
        )
    }

    pub(super) fn type_text(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
        mechanism: Mechanism,
    ) -> Result<ActionResult> {
        if mechanism == Mechanism::Keystrokes {
            return self.type_text_as_keystrokes(query, target, text);
        }
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.watch(info.pid);
        let write = el.append_text(text)?;
        let changed = self.changed_since(info.pid, before);
        // Name the mechanism, not just the intent. "typed" would imply
        // keystrokes were synthesized, which is exactly what did not happen —
        // see `set_value` for why this crate keeps bulk text writes on AX
        // rather than routing them through `cua_hid::type_text_background_pid`.
        Ok(ActionResult::ax_at(
            format!("AXSelectedText+ ({})", write.as_str()),
            desc,
            changed,
            element_point(&el),
        )
        .with_overlay_target(self.overlay_target(info.pid)))
    }

    /// `type_text` with `mechanism: "keystrokes"` — real per-character key
    /// events routed to the target pid, for the targets that ignore `AXValue`.
    ///
    /// Everything `set_value`'s comment says against this mechanism is still
    /// true: it appends wherever the process's first responder is, one
    /// character at a time, with no element addressing. It exists because on a
    /// terminal or a canvas editor the atomic, element-addressed write it
    /// loses to simply does nothing at all. So it is a separate, explicit
    /// choice rather than a fallback, and it reports its [`FocusCheck`] —
    /// which matters more here than for a single chord, since a miss is
    /// repeated once per character.
    pub(super) fn type_text_as_keystrokes(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
    ) -> Result<ActionResult> {
        if text.is_empty() {
            return Err(CoreError::PidKeyUnavailable {
                key: "<empty string>".into(),
                reason: "there is nothing to type".into(),
            });
        }
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let before = self.watch(info.pid);

        if !cua_hid::skylight_available() {
            return Err(CoreError::PidKeyUnavailable {
                key: text.to_string(),
                reason: "SLEventPostToPid is not available on this macOS version".into(),
            });
        }

        let focus = self.prime_for_pid_keyboard(&info, &el);
        self.enforce_strict_focus(
            &focus,
            &format!("{} characters", text.chars().count()),
            &desc,
        )?;

        cua_hid::type_text_background_pid(info.pid, text).map_err(|e| {
            CoreError::PidKeyUnavailable {
                key: text.to_string(),
                reason: e.to_string(),
            }
        })?;
        let changed = self.changed_since(info.pid, before);
        Ok(ActionResult {
            verb: format!(
                "pid-routed keystrokes ({} characters)",
                text.chars().count()
            ),
            target: desc,
            ui_changed: changed.changed,
            popups: changed.popups,
            delivery: Delivery::PidKey,
            point: element_point(&el),
            overlay_target: self.overlay_target(info.pid),
            state: None,
            focus: Some(focus),
        })
    }

    pub(super) fn select_text(
        &mut self,
        query: &str,
        target: Target,
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let range = el.select_text(text, prefix, suffix)?;
        // Selecting text changes no window state the fingerprint can see, and
        // claiming otherwise would be noise. The returned range is the evidence
        // that it worked.
        Ok(ActionResult::ax_at(
            format!(
                "AXSelectedTextRange={{offset:{},length:{}}}",
                range.offset, range.length
            ),
            desc,
            // Selection changes no window state the fingerprint can see, so
            // there is nothing to observe here either way — and no reason to
            // spend an enumeration finding out that no menu opened.
            Settled {
                changed: Observed::Unknown,
                popups: Vec::new(),
            },
            element_point(&el),
        )
        .with_overlay_target(self.overlay_target(info.pid)))
    }
}
