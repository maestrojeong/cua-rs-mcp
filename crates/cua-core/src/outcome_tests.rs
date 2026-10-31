use super::*;
use crate::safety::Refused;
use crate::session::CoreError;

fn bundle_scoped_refusal() -> Refused {
    Refused::NotInAllowlist {
        verb: "click",
        app: "Notes".into(),
        bundle_id: "com.apple.Notes".into(),
        allowed: "com.apple.TextEdit".into(),
    }
}

#[test]
fn stale_reference_errors_classify_as_stale_ref_and_are_safe_to_retry() {
    let err = CoreError::BadIndex { index: 9, count: 3 };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::StaleRef);
    assert_eq!(out.policy_reason, None);
    assert_eq!(out.disposition, Disposition::NotDelivered);
    assert!(out.retry_safe);
}

#[test]
fn every_stale_variant_shares_the_same_code() {
    let variants = [
        CoreError::StaleSnapshot {
            index: 1,
            given: 1,
            current: 2,
        },
        CoreError::NoSnapshot {
            app: "Notes".into(),
        },
        CoreError::ProcessReplaced {
            app: "Notes".into(),
        },
        CoreError::BadIndex { index: 1, count: 1 },
        CoreError::TargetChanged {
            index: 1,
            expected: "a".into(),
            found: "b".into(),
        },
        CoreError::TokenRoleMismatch {
            index: 1,
            expected: "AXButton".into(),
            found: "AXRow".into(),
        },
        CoreError::StalePointGeometry {
            app: "Notes".into(),
        },
        CoreError::StaleCoordinate {
            app: "Notes".into(),
            given: 1,
            current: 2,
            x: 0.0,
            y: 0.0,
        },
    ];
    for v in variants {
        let out = ErrorOutcome::classify(&v);
        assert_eq!(out.code, ErrorCode::StaleRef, "{v:?}");
        assert_eq!(out.disposition, Disposition::NotDelivered, "{v:?}");
        assert!(out.retry_safe, "{v:?}");
    }
}

#[test]
fn no_window_classifies_as_window_not_found() {
    let err = CoreError::NoWindow {
        app: "Finder".into(),
    };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::WindowNotFound);
    assert!(out.retry_safe);
}

#[test]
fn window_query_timeout_classifies_as_timeout() {
    let err = CoreError::WindowQueryTimedOut {
        app: "KakaoTalk".into(),
    };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::Timeout);
    assert!(out.retry_safe);
}

#[test]
fn unparseable_chord_classifies_as_invalid_args() {
    let err = CoreError::KeyChordUnparseable {
        key: "cmd+".into(),
        reason: "trailing modifier with no base key".into(),
    };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::InvalidArgs);
}

#[test]
fn key_with_no_accessibility_equivalent_is_action_not_supported() {
    let err = CoreError::KeyNoAccessibilityEquivalent { key: "F13".into() };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::ActionNotSupported);
}

#[test]
fn pid_click_failed_is_action_failed_not_policy_denied() {
    let err = CoreError::PidClickFailed {
        reason: "SkyLight framework unavailable".into(),
    };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::ActionFailed);
    assert_eq!(out.policy_reason, None);
    assert!(out.retry_safe);
}

#[test]
fn focus_mismatch_is_policy_denied_with_strict_focus_reason() {
    let err = CoreError::FocusMismatch {
        what: "hello".into(),
        addressed: "AXTextField".into(),
        focused: "AXTextArea".into(),
    };
    let out = ErrorOutcome::classify(&err);
    assert_eq!(out.code, ErrorCode::PolicyDenied);
    assert_eq!(out.policy_reason, Some(PolicyReason::StrictFocus));
}

#[test]
fn worker_gone_is_internal_but_still_safe_to_retry() {
    // Nothing was in flight when the worker died between calls: the next
    // request starts a fresh one, so the usual retry rule still applies.
    let out = ErrorOutcome::classify(&CoreError::WorkerGone);
    assert_eq!(out.code, ErrorCode::Internal);
    assert_eq!(out.disposition, Disposition::NotDelivered);
    assert!(out.retry_safe);
}

#[test]
fn native_panic_is_internal_and_unverified_not_not_delivered() {
    // The one variant where delivery is genuinely unknown: the panic could
    // have landed on either side of the native call it interrupted. This
    // must NOT be `retry_safe`, unlike every other `Internal`/refusal path,
    // because assuming it's safe to repeat risks a duplicate action.
    let out = ErrorOutcome::classify(&CoreError::NativePanic);
    assert_eq!(out.code, ErrorCode::Internal);
    assert_eq!(out.disposition, Disposition::DeliveredUnverified);
    assert!(!out.retry_safe);
}

#[test]
fn forbidden_target_refusal_is_policy_denied_with_forbidden_target_reason() {
    let refused = Refused::ForbiddenTarget {
        verb: "click",
        app: "1Password".into(),
        bundle_id: "com.1password.1password".into(),
        why: "it manages credentials",
    };
    let out = ErrorOutcome::classify(&CoreError::Refused(refused));
    assert_eq!(out.code, ErrorCode::PolicyDenied);
    assert_eq!(out.policy_reason, Some(PolicyReason::ForbiddenTarget));
    assert_eq!(out.disposition, Disposition::NotDelivered);
    assert!(out.retry_safe);
}

#[test]
fn session_locked_refusal_maps_to_screen_locked_reason() {
    let refused = Refused::SessionLocked { verb: "click" };
    let out = ErrorOutcome::classify(&CoreError::Refused(refused));
    assert_eq!(out.policy_reason, Some(PolicyReason::ScreenLocked));
}

#[test]
fn scope_refusals_both_map_to_scope_reason() {
    let out = ErrorOutcome::classify(&CoreError::Refused(bundle_scoped_refusal()));
    assert_eq!(out.policy_reason, Some(PolicyReason::Scope));

    let unidentifiable = Refused::UnidentifiableUnderAllowlist {
        verb: "click",
        app: "mystery".into(),
        pid: 42,
        allowed: "com.apple.TextEdit".into(),
    };
    let out = ErrorOutcome::classify(&CoreError::Refused(unidentifiable));
    assert_eq!(out.policy_reason, Some(PolicyReason::Scope));
}

#[test]
fn destructive_confirmation_refusals_all_map_to_destructive_reason() {
    let plain = Refused::NeedsConfirmation {
        verb: "click",
        target: "Delete".into(),
        matched: "delet".into(),
    };
    assert_eq!(
        ErrorOutcome::classify(&CoreError::Refused(plain)).policy_reason,
        Some(PolicyReason::Destructive)
    );

    let in_context = Refused::NeedsConfirmationInContext {
        verb: "click",
        target: "OK".into(),
        context: "AXDialog".into(),
        question: "Delete 4 items?".into(),
        matched: "delet".into(),
    };
    assert_eq!(
        ErrorOutcome::classify(&CoreError::Refused(in_context)).policy_reason,
        Some(PolicyReason::Destructive)
    );

    let for_key = Refused::NeedsConfirmationForKey {
        key: "delete".into(),
        target: "AXTextArea".into(),
        matched: "delet".into(),
    };
    assert_eq!(
        ErrorOutcome::classify(&CoreError::Refused(for_key)).policy_reason,
        Some(PolicyReason::Destructive)
    );
}

#[test]
fn yield_to_human_refusals_both_map_to_yield_to_human_reason() {
    let took_over = Refused::HumanTookOver {
        verb: "click",
        app: "Notes".into(),
        ago_ms: 200,
        idle_ms: 3000,
    };
    assert_eq!(
        ErrorOutcome::classify(&CoreError::Refused(took_over)).policy_reason,
        Some(PolicyReason::YieldToHuman)
    );

    let watch_unavailable = Refused::YieldWatchUnavailable {
        verb: "click",
        reason: "Accessibility not granted".into(),
    };
    assert_eq!(
        ErrorOutcome::classify(&CoreError::Refused(watch_unavailable)).policy_reason,
        Some(PolicyReason::YieldToHuman)
    );
}

#[test]
fn every_policy_denied_refusal_is_not_delivered_and_safe_to_retry() {
    // A gate refusal never dispatches anything, by construction: `guard` is
    // checked before an action is built, not after it fails. If any refusal
    // variant ever stopped being `NotDelivered`, every caller watching
    // `retry_safe` to decide whether to retry a gated call automatically
    // would start retrying something it should not.
    let refusal = CoreError::Refused(bundle_scoped_refusal());
    let out = ErrorOutcome::classify(&refusal);
    assert_eq!(out.disposition, Disposition::NotDelivered);
    assert!(out.retry_safe);
}

#[test]
fn disposition_of_changed_is_verified() {
    assert_eq!(
        disposition_of(Observed::Changed),
        Disposition::DeliveredVerified
    );
}

#[test]
fn disposition_of_unchanged_and_unknown_are_both_unverified() {
    // `Unknown` means nobody looked, not "looked and found nothing" — but
    // from a caller's perspective both are exactly as uncertain, so both map
    // to the same disposition rather than `Unknown` quietly reading as a
    // weaker kind of failure.
    assert_eq!(
        disposition_of(Observed::Unchanged),
        Disposition::DeliveredUnverified
    );
    assert_eq!(
        disposition_of(Observed::Unknown),
        Disposition::DeliveredUnverified
    );
}

#[test]
fn retry_safe_is_always_derived_from_disposition_never_independent() {
    // Every disposition this crate currently produces, checked against the
    // one-line rule in the doc comment, so the rule and the implementation
    // cannot quietly diverge.
    let cases = [
        (CoreError::WorkerGone, true),
        (CoreError::NativePanic, false),
        (CoreError::BadIndex { index: 0, count: 0 }, true),
        (CoreError::Refused(bundle_scoped_refusal()), true),
    ];
    for (err, expected_retry_safe) in cases {
        let out = ErrorOutcome::classify(&err);
        assert_eq!(
            out.retry_safe,
            out.disposition == Disposition::NotDelivered,
            "{err:?}"
        );
        assert_eq!(out.retry_safe, expected_retry_safe, "{err:?}");
    }
}
