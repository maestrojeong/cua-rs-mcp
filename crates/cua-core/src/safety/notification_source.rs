//! Judging the app a notification came from.
//!
//! A notification tool drives NotificationCenter, and that process is gated the
//! ordinary way. But what the press *means* belongs to the posting app: an
//! action button can run code on that app's behalf — approving an
//! authenticator's sign-in push is the case that settles it — and Close was
//! measured to delete the record from NotificationCenter's storage, not just
//! hide a banner, so dismissing a password manager's "your vault was opened"
//! alert removes the one trace a human would have seen. Both are acting on that
//! app, and both are refused where acting on the app directly would be.
//!
//! The difficulty is identity. NotificationCenter publishes the source only as a
//! *localized display name* inside the group description — no bundle identifier
//! anywhere in the attribute list (measured, macOS 26.2). The caller resolves
//! that name against running apps by exact match where it can; this module then
//! judges with whatever identity it got, and fails closed where a scope demands
//! an identity it does not have. The name itself is also run through the same
//! substring heuristic the bundle-id floor uses, because a name is weaker
//! evidence than a bundle id, not none.

use super::*;

/// Refuse acting on a notification whose source app may not be acted on.
///
/// `bundle_id` is the source app's identifier when the display name resolved to
/// exactly one running app, `None` otherwise.
pub fn guard_notification_source(
    verb: &'static str,
    app_name: Option<&str>,
    bundle_id: Option<&str>,
) -> std::result::Result<(), Refused> {
    judge_notification_source(
        verb,
        app_name,
        bundle_id,
        forbidden_targets_allowed(),
        allowed_apps(),
    )
}

/// [`guard_notification_source`] with its two process-wide inputs passed in, so
/// every branch is testable without touching the environment.
pub(crate) fn judge_notification_source(
    verb: &'static str,
    app_name: Option<&str>,
    bundle_id: Option<&str>,
    forbidden_allowed: bool,
    scope: Option<&[String]>,
) -> std::result::Result<(), Refused> {
    let shown = app_name.unwrap_or("(unknown app)").to_string();
    if !forbidden_allowed {
        if let Some(b) = bundle_id {
            if let Some(why) = forbidden_bundle(b) {
                return Err(Refused::ForbiddenNotificationSource {
                    verb,
                    app: shown,
                    evidence: format!("bundle id {b}"),
                    why,
                });
            }
        }
        if let Some(token) = app_name.and_then(apps_policy::suspicious_token) {
            return Err(Refused::ForbiddenNotificationSource {
                verb,
                app: shown,
                evidence: format!("its display name contains `{token}`"),
                why: apps_policy::CREDENTIALS,
            });
        }
    }
    if let Some(list) = scope {
        match bundle_id {
            Some(b) if apps_policy::in_scope(list, b) => {}
            _ => {
                return Err(Refused::NotificationSourceOutOfScope {
                    verb,
                    app: shown,
                    bundle_id: bundle_id
                        .map(str::to_string)
                        .unwrap_or_else(|| "not identifiable".into()),
                    allowed: list.join(","),
                })
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod notification_source_tests {
    use super::*;

    fn scope(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ordinary_source_unscoped_is_allowed() {
        assert!(judge_notification_source(
            "dismiss",
            Some("Mail"),
            Some("com.apple.mail"),
            false,
            None
        )
        .is_ok());
    }

    #[test]
    fn unidentified_source_unscoped_is_allowed() {
        // Same precedent as `guard`: the forbidden floor judges what it can name.
        assert!(
            judge_notification_source("dismiss", Some("스크립트 편집기"), None, false, None)
                .is_ok()
        );
        assert!(judge_notification_source("dismiss", None, None, false, None).is_ok());
    }

    #[test]
    fn forbidden_bundle_source_is_refused() {
        let e = judge_notification_source(
            "notification_action",
            Some("Passwords"),
            Some("com.apple.Passwords"),
            false,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(e, Refused::ForbiddenNotificationSource { .. }),
            "{e}"
        );
        assert!(e.to_string().contains("CUA_ALLOW_FORBIDDEN_TARGETS"));
    }

    #[test]
    fn credential_looking_name_is_refused_without_a_bundle_id() {
        for name in ["1Password", "Google Authenticator", "Keychain Access"] {
            let e =
                judge_notification_source("dismiss", Some(name), None, false, None).unwrap_err();
            assert!(
                matches!(e, Refused::ForbiddenNotificationSource { .. }),
                "{name}: {e}"
            );
        }
    }

    #[test]
    fn opting_out_lifts_the_floor_but_not_the_scope() {
        assert!(judge_notification_source(
            "dismiss",
            Some("1Password"),
            Some("com.1password.1password"),
            true,
            None
        )
        .is_ok());
        let e = judge_notification_source(
            "dismiss",
            Some("1Password"),
            Some("com.1password.1password"),
            true,
            Some(&scope(&["com.apple.mail"])),
        )
        .unwrap_err();
        assert!(matches!(e, Refused::NotificationSourceOutOfScope { .. }));
    }

    #[test]
    fn scope_admits_only_identified_in_scope_sources() {
        let s = scope(&["com.apple.mail"]);
        assert!(judge_notification_source(
            "dismiss",
            Some("Mail"),
            Some("com.apple.mail"),
            false,
            Some(&s)
        )
        .is_ok());
        let out = judge_notification_source(
            "dismiss",
            Some("Notes"),
            Some("com.apple.Notes"),
            false,
            Some(&s),
        )
        .unwrap_err();
        assert!(matches!(out, Refused::NotificationSourceOutOfScope { .. }));
        // Unidentified under a scope fails closed, like an unbundled process does.
        let unknown =
            judge_notification_source("dismiss", Some("스크립트 편집기"), None, false, Some(&s))
                .unwrap_err();
        assert!(
            unknown.to_string().contains("not identifiable"),
            "{unknown}"
        );
    }

    // Dismissal presses Close through the ordinary destructive-label gate. If
    // that gate ever read Close as destructive, every dismissal would demand a
    // confirmation the tool does not take; Clear All, which removes a whole
    // app's history at once, is the one that should.
    #[test]
    fn close_is_not_destructive_but_clear_all_is() {
        for close in ["Close", "닫기"] {
            assert_eq!(destructive_token(close), None, "{close}");
        }
        for clear in ["Clear All", "모두 지우기", "지우기"] {
            assert!(destructive_token(clear).is_some(), "{clear}");
        }
    }

    #[test]
    fn floor_is_reported_before_scope() {
        let s = scope(&["com.apple.Passwords"]);
        let e = judge_notification_source(
            "dismiss",
            Some("Passwords"),
            Some("com.apple.Passwords"),
            false,
            Some(&s),
        )
        .unwrap_err();
        assert!(matches!(e, Refused::ForbiddenNotificationSource { .. }));
    }
}
