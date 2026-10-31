//! A closed, machine-checkable classification of every [`CoreError`], plus a
//! `disposition` saying whether an action's input ever reached the target.
//!
//! `CoreError`'s messages are already careful, agent-tuned prose — see
//! `session::orchestration` and `safety::destructive`. This module does not
//! change a single word of that prose. It adds a second, structural axis next
//! to it: a stable `code` an agent can match on without re-parsing English
//! (wording is free to keep improving; the code is not), and a `disposition`
//! that answers the one question an agent actually needs before deciding
//! whether to retry — did this call's input ever reach the app at all.
//!
//! # Why disposition is not just "ok or error"
//!
//! A refusal from a safety gate never dispatched anything: the event was
//! never built, so retrying costs nothing and changes nothing until the
//! human clears the gate. That is [`Disposition::NotDelivered`].
//!
//! A *successful* action is not automatically more certain. `ui_changed`
//! (`Observed`) is already documented elsewhere as a heuristic: it compares a
//! fingerprint before and after, and a menu opening as its own window or two
//! untitled sibling text fields can both leave it reporting no change while
//! something plainly happened. So a success whose own heuristic says
//! "unchanged" or "unknown" is [`Disposition::DeliveredUnverified`] rather
//! than a bare `ok` — the input landed, but nothing independently confirms
//! its effect. Only a success the fingerprint itself saw change is
//! [`Disposition::DeliveredVerified`].
//!
//! `retry_safe` is derived, not chosen per call site, so it cannot drift from
//! `disposition`: true only for `NotDelivered`. A `DeliveredUnverified`
//! result might already have landed, so repeating it risks a duplicate (a
//! second click, a second keystroke) even though nothing proves the first one
//! worked.

use crate::safety::Refused;
use crate::session::{CoreError, Observed};

/// A closed set of reasons an MCP call can fail, independent of wording.
///
/// Serialized as the upper-case strings in this doc comment's table, matching
/// the convention the rest of the MCP surface already uses for enum-shaped
/// data. Exhaustive: every `CoreError` variant maps to exactly one of these in
/// [`ErrorOutcome::classify`], and that match has no wildcard arm, so a new
/// `CoreError` variant fails to compile here until it is given a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Accessibility or Screen Recording is not granted to the process that
    /// launched cua-rs.
    PermDenied,
    /// `app` matched no running process.
    AppNotFound,
    /// `app` matched more than one running process and cua-rs will not guess
    /// which one was meant.
    AmbiguousTarget,
    /// No element answers the index, token, or point this call named.
    ElementNotFound,
    /// The app has no accessibility window cua-rs can address right now.
    WindowNotFound,
    /// The index, token, point, or snapshot id this call named belongs to a
    /// snapshot that is no longer current. Call `get_app_state` again.
    StaleRef,
    /// The target cannot do what was asked — not right now, or not ever
    /// through accessibility (e.g. no AX verb and no pid-routed fallback
    /// applies to this key).
    ActionNotSupported,
    /// The action was attempted and the app or the platform failed to carry
    /// it out.
    ActionFailed,
    /// The app, or an explicit `wait_for`, did not answer within its budget.
    Timeout,
    /// This macOS version or build lacks a primitive the action needs.
    PlatformNotSupported,
    /// The call's own arguments are malformed, independent of app state.
    InvalidArgs,
    /// A safety gate refused the action before it was attempted. See
    /// [`PolicyReason`] for which one.
    PolicyDenied,
    /// A bug, a native panic recovered by the worker, or a platform call that
    /// failed in a way none of the above names.
    Internal,
}

/// Which of the five safety gates produced a [`ErrorCode::PolicyDenied`]
/// refusal. See `safety::destructive` for what each gate checks and its
/// escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyReason {
    /// Outside `CUA_ALLOWED_APPS`, or the target publishes no bundle
    /// identifier to check against a scope.
    Scope,
    /// A credential store, System Settings, or another app cua-rs refuses to
    /// act on regardless of scope.
    ForbiddenTarget,
    /// The target reads as destructive and `confirm_destructive` was not set.
    Destructive,
    /// The session is locked or the screen saver is running.
    ScreenLocked,
    /// `CUA_YIELD_TO_HUMAN` is set and a human is actively using the target,
    /// or the watch that would tell could not be created.
    YieldToHuman,
    /// `CUA_KEY_STRICT_FOCUS` refused because the app names a different
    /// element as focused than the one addressed.
    StrictFocus,
}

/// Whether an action's input ever reached the app, and whether its effect is
/// confirmed. See the module docs for the reasoning behind each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Nothing was dispatched — a gate refused it, or the delivery mechanism
    /// itself reported failure before anything reached the target.
    NotDelivered,
    /// Dispatched, and the native call reported success, but nothing
    /// independently confirms the app acted on it.
    DeliveredUnverified,
    /// Dispatched, and a re-read of the target independently confirms it
    /// changed.
    DeliveredVerified,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::NotDelivered => "not_delivered",
            Disposition::DeliveredUnverified => "delivered_unverified",
            Disposition::DeliveredVerified => "delivered_verified",
        }
    }
}

/// The structured half of a failed call. Carried next to the existing
/// human-readable `message`, never instead of it — see the module docs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ErrorOutcome {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_reason: Option<PolicyReason>,
    pub disposition: Disposition,
    /// Derived from `disposition`, never set independently: `true` only for
    /// `Disposition::NotDelivered`. Kept as its own field because "is retrying
    /// safe" is the question a caller actually asks, and spelling that out
    /// saves every caller from re-deriving the same one-line rule.
    pub retry_safe: bool,
}

impl ErrorOutcome {
    pub fn classify(err: &CoreError) -> Self {
        use CoreError::*;

        let (code, policy_reason) = match err {
            Refused(r) => (ErrorCode::PolicyDenied, Some(policy_reason_of(r))),

            Resolve(_) => (ErrorCode::AppNotFound, None),

            NoWindow { .. } => (ErrorCode::WindowNotFound, None),
            WindowQueryTimedOut { .. } => (ErrorCode::Timeout, None),

            StaleSnapshot { .. }
            | NoSnapshot { .. }
            | ProcessReplaced { .. }
            | BadIndex { .. }
            | TargetChanged { .. }
            | TokenRoleMismatch { .. }
            | StalePointGeometry { .. }
            | StaleCoordinate { .. } => (ErrorCode::StaleRef, None),

            NoElementAtPoint { .. } => (ErrorCode::ElementNotFound, None),

            KeyNoAccessibilityEquivalent { .. } | KeyVerbUnsupported { .. } => {
                (ErrorCode::ActionNotSupported, None)
            }

            KeyChordUnparseable { .. } => (ErrorCode::InvalidArgs, None),

            PidKeyUnavailable { .. }
            | PidClickUnavailable { .. }
            | PidClickFailed { .. }
            | WindowClickRefused { .. }
            | DragRefused { .. }
            | PointerEventRefused { .. }
            | MenuPath { .. } => (ErrorCode::ActionFailed, None),

            // A different gate from the other four `PolicyDenied` reasons —
            // it fires on a mismatch the keyboard tier itself detects, not on
            // one of `safety::Refused`'s variants — but it is still a policy
            // choice (CUA_KEY_STRICT_FOCUS) rather than a platform failure, so
            // it gets the same code with its own reason.
            FocusMismatch { .. } => (ErrorCode::PolicyDenied, Some(PolicyReason::StrictFocus)),

            Ax(_) | Capture(_) | WorkerGone | NativePanic => (ErrorCode::Internal, None),
        };

        // `NativePanic` is the one variant where "was anything dispatched"
        // genuinely cannot be answered: the worker recovered from a panic
        // mid-call, which can land on either side of the native action it was
        // running. Every other variant above is reached either before an
        // event is built (a gate, a stale reference, an unresolved app or
        // window) or *as* the delivery mechanism's own failure report, which
        // by this project's one-mechanism-per-action design (DESIGN.md §1a)
        // means nothing was sent. Treating `NativePanic` as unverified rather
        // than not-delivered is the conservative read in the direction that
        // matters: assuming it *did* land costs nothing here, since the
        // disposition only changes whether a caller treats a retry as free.
        let disposition = match err {
            NativePanic => Disposition::DeliveredUnverified,
            _ => Disposition::NotDelivered,
        };
        let retry_safe = disposition == Disposition::NotDelivered;

        Self {
            code,
            policy_reason,
            disposition,
            retry_safe,
        }
    }
}

fn policy_reason_of(r: &Refused) -> PolicyReason {
    use Refused::*;
    match r {
        ForbiddenTarget { .. } => PolicyReason::ForbiddenTarget,
        SessionLocked { .. } => PolicyReason::ScreenLocked,
        NotInAllowlist { .. } | UnidentifiableUnderAllowlist { .. } => PolicyReason::Scope,
        NeedsConfirmation { .. }
        | NeedsConfirmationInContext { .. }
        | NeedsConfirmationForKey { .. } => PolicyReason::Destructive,
        HumanTookOver { .. } | YieldWatchUnavailable { .. } => PolicyReason::YieldToHuman,
    }
}

/// Disposition for a result that already succeeded, derived from its own
/// `ui_changed` heuristic (`ActionResult::ui_changed`, passed by value since
/// `Observed` is `Copy` — this takes the field, not the whole result, so it
/// stays usable without exposing `ActionResult`'s crate-private fields
/// outside the module that defines them). A result with no re-read attempted
/// at all (`Observed::Unknown` — nobody looked, not "looked and saw nothing")
/// is treated the same as `Unchanged`: both mean the caller has no
/// independent confirmation, which is exactly what `DeliveredUnverified`
/// promises and nothing more.
pub fn disposition_of(ui_changed: Observed) -> Disposition {
    match ui_changed {
        Observed::Changed => Disposition::DeliveredVerified,
        Observed::Unchanged | Observed::Unknown => Disposition::DeliveredUnverified,
    }
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod tests;
