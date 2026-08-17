//! Action-level safety gate orchestration.

use super::*;

// ── the gate ─────────────────────────────────────────────────────────────────

/// One action's worth of safety context, assembled by the caller in `session`.
///
/// Built at the call site rather than inferred here so that the choice of which
/// gates apply to which tool is visible in one line next to the tool, instead of
/// being a table in this file that drifts out of sync with the tools.
#[derive(Debug, Clone)]
pub struct Gate {
    pub(super) verb: &'static str,
    pub(super) target: Option<Target>,
    pub(super) confirm_destructive: bool,
    pub(super) key: Option<String>,
    pub(super) labelled: Option<Candidate>,
}

impl Gate {
    /// A gate for an action aimed at an element.
    pub fn at(verb: &'static str, target: &Target) -> Self {
        Self {
            verb,
            target: Some(target.clone()),
            confirm_destructive: false,
            key: None,
            labelled: None,
        }
    }

    /// A gate for an action whose target has a label but no snapshot index — a
    /// menu bar row, which is named by its path rather than by a number.
    ///
    /// The destructive-label heuristic matters *more* here than it does for a
    /// click, not less: a menu bar is where "Log Out", "Quit", "Move to Trash"
    /// and "채팅방 나가기" live, all one press from the top level and none of
    /// them behind a confirmation of the app's own.
    pub fn labelled(verb: &'static str, candidate: Candidate) -> Self {
        Self {
            verb,
            target: None,
            confirm_destructive: false,
            key: None,
            labelled: Some(candidate),
        }
    }

    /// A gate for an action with no element behind it — `click_in_window`.
    ///
    /// The destructive-label heuristic is structurally unavailable here: there
    /// is no label, because there is no element. That is stated rather than
    /// worked around, and it is one more reason the tool's own description
    /// calls it a last resort.
    pub fn elementless(verb: &'static str) -> Self {
        Self {
            verb,
            target: None,
            confirm_destructive: false,
            key: None,
            labelled: None,
        }
    }

    /// Carry the caller's `confirm_destructive` through.
    pub fn confirmed(mut self, confirmed: bool) -> Self {
        self.confirm_destructive = confirmed;
        self
    }

    /// Also classify the key itself, for `press_key`.
    pub fn with_key(mut self, key: &str) -> Self {
        self.key = Some(key.to_string());
        self
    }

    /// The element this action will land on, for the caller to resolve.
    /// The key this gate is for, when it is a `press_key`.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    /// The already-described target, for a gate whose action resolved its own.
    pub fn labelled_candidate(&self) -> Option<&Candidate> {
        self.labelled.as_ref()
    }
}

/// Run every gate, in the order a reader would want them reported.
///
/// Session-wide conditions first, then app-wide, then element-level, so the
/// error a caller sees is the most fundamental reason rather than the last one
/// checked. Refusing a click on "Delete" because it is destructive, when the
/// real problem is that the screen has been locked for an hour, sends the agent
/// down the wrong path.
pub(crate) fn guard(
    app: &AppInfo,
    watch: &HumanWatch,
    gate: &Gate,
    candidate: Option<&Candidate>,
) -> std::result::Result<(), Refused> {
    let verb = gate.verb;

    if session_locked() {
        return Err(Refused::SessionLocked { verb });
    }

    if !forbidden_targets_allowed() {
        if let Some(bundle_id) = app.bundle_id.as_deref() {
            if let Some(why) = forbidden_bundle(bundle_id) {
                return Err(Refused::ForbiddenTarget {
                    verb,
                    app: app.name.clone(),
                    bundle_id: bundle_id.to_string(),
                    why,
                });
            }
        }
    }

    // Checked after the forbidden floor on purpose. When an app is both
    // forbidden and out of scope, "nobody may drive this" is the reason worth
    // reporting: adding it to CUA_ALLOWED_APPS would not help, and a caller told
    // otherwise will send the human to edit the wrong variable.
    if allowed_apps().is_some() {
        match app.bundle_id.as_deref() {
            Some(bundle_id) if !app_allowed(bundle_id) => {
                return Err(Refused::NotInAllowlist {
                    verb,
                    app: app.name.clone(),
                    bundle_id: bundle_id.to_string(),
                    allowed: allowed_apps_display(),
                })
            }
            None => {
                return Err(Refused::UnidentifiableUnderAllowlist {
                    verb,
                    app: app.name.clone(),
                    pid: app.pid,
                    allowed: allowed_apps_display(),
                })
            }
            Some(_) => {}
        }
    }

    match &watch.watch {
        Watch::Off => {}
        Watch::Broken { reason } => {
            return Err(Refused::YieldWatchUnavailable {
                verb,
                reason: reason.clone(),
            })
        }
        Watch::On { .. } => {
            let idle_ms = yield_idle_ms();
            if let Some(ago_ms) = watch.since_human_input_ms() {
                // The pid test is what keeps this from firing on the whole
                // point of the project. cua-rs never activates an app, so an
                // app it is driving is frontmost only because the human put it
                // there; input arriving while that is true is the human's, and
                // input arriving while some other app is frontmost is the human
                // working somewhere else, which is exactly the case cua-rs
                // exists to run alongside.
                if ago_ms < idle_ms && crate::apps::frontmost_pid() == Some(app.pid) {
                    return Err(Refused::HumanTookOver {
                        verb,
                        app: app.name.clone(),
                        ago_ms,
                        idle_ms,
                    });
                }
            }
        }
    }

    if !gate.confirm_destructive {
        if let Some(key) = &gate.key {
            if let Some(matched) = destructive_key(key, candidate) {
                return Err(Refused::NeedsConfirmationForKey {
                    key: key.clone(),
                    target: candidate
                        .map(|c| c.description.clone())
                        .unwrap_or_else(|| format!("`{}`", app.name)),
                    matched,
                });
            }
        }
        if let Some(c) = candidate {
            if let Some(matched) = destructive_token(&c.classifiable_text()) {
                return Err(Refused::NeedsConfirmation {
                    verb,
                    target: c.description.clone(),
                    matched,
                });
            }
            // The control said nothing destructive about itself. Ask what it is
            // answering — checked second so that a control which is destructive
            // on its own evidence is reported that way, which is the shorter
            // and more obvious explanation of the two.
            if let Some((context, matched)) = destructive_context(c) {
                return Err(Refused::NeedsConfirmationInContext {
                    verb,
                    target: c.description.clone(),
                    context: context.kind.clone(),
                    question: context.question.clone(),
                    matched,
                });
            }
        }
    }

    Ok(())
}
