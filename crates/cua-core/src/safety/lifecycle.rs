//! App-lifecycle policy: which processes `close_app` may never quit.
//!
//! Separate from the forbidden-target list on purpose. That list answers "may
//! cua-rs drive this app at all" and is liftable by the operator; this one
//! answers "may cua-rs end this process", and its answer for these entries is
//! never. Folding them together would either make Dock undrivable (it is not
//! dangerous to click) or make Dock quittable under the forbidden-target escape
//! hatch (it is dangerous to quit).

use super::*;

/// Why a process is protected, quoted in the refusal.
const SESSION_SHELL: &str = "is part of the macOS desktop shell";
const SESSION_CORE: &str = "owns the login session itself";
const SELF_HOSTED: &str = "is the process running this cua-rs server";
const HOST: &str = "is the process that launched this cua-rs server (the agent's own host)";

/// Bundle identifiers `close_app` refuses outright, matched case-insensitively
/// on the whole identifier.
///
/// The criterion is "quitting it breaks or visibly resets the session", not
/// "Apple ships it". Finder, Dock and SystemUIServer are relaunched by launchd
/// the moment they exit, so a quit is never what it looks like: the desktop
/// flashes, the menu bar extras reset, and the agent reports success for an
/// action whose only effect was disruption. loginwindow and WindowServer end
/// the session outright. Control Center, Notification Center and Window
/// Manager (Stage Manager) are the same shell split into newer processes.
const PROTECTED_BUNDLES: &[(&str, &str)] = &[
    ("com.apple.finder", SESSION_SHELL),
    ("com.apple.dock", SESSION_SHELL),
    ("com.apple.systemuiserver", SESSION_SHELL),
    ("com.apple.controlcenter", SESSION_SHELL),
    ("com.apple.notificationcenterui", SESSION_SHELL),
    ("com.apple.WindowManager", SESSION_SHELL),
    ("com.apple.loginwindow", SESSION_CORE),
    ("com.apple.WindowServer", SESSION_CORE),
];

/// Process names refused when no bundle identifier is published.
///
/// WindowServer and loginwindow are not always reported with a bundle id, and
/// a protection that only works when the process cooperates is not one. Names
/// are a fallback only: a bundled app is judged by its identifier, so an app
/// that merely calls itself "Dock" is not protected by accident.
const PROTECTED_NAMES: &[(&str, &str)] = &[
    ("WindowServer", SESSION_CORE),
    ("loginwindow", SESSION_CORE),
];

/// Why `close_app` must not quit this process, or `None` when it may.
///
/// `self_pid` and `parent_pid` are passed in rather than read here so the rule
/// is testable: quitting the server's own process ends the tool call that asked
/// for it, and quitting its parent — the terminal or agent app that launched
/// it — ends the agent's own session, which no caller can have meant.
pub fn protected_process(
    app: &AppInfo,
    self_pid: libc::pid_t,
    parent_pid: libc::pid_t,
) -> Option<&'static str> {
    if app.pid == self_pid {
        return Some(SELF_HOSTED);
    }
    if app.pid == parent_pid && parent_pid > 1 {
        return Some(HOST);
    }
    match app.bundle_id.as_deref() {
        Some(bundle_id) => PROTECTED_BUNDLES
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(bundle_id))
            .map(|(_, why)| *why),
        None => PROTECTED_NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&app.name))
            .map(|(_, why)| *why),
    }
}

/// Every check `close_app` adds on top of [`guard`], in reporting order.
///
/// Protected first, confirmation second: a confirmation cannot make quitting
/// Dock acceptable, so asking for one would send the caller round a loop that
/// ends in the other refusal anyway.
pub fn close_app_policy(
    app: &AppInfo,
    confirm_destructive: bool,
    self_pid: libc::pid_t,
    parent_pid: libc::pid_t,
) -> std::result::Result<(), Refused> {
    if let Some(why) = protected_process(app, self_pid, parent_pid) {
        return Err(Refused::ProtectedProcess {
            verb: "close_app",
            app: app.name.clone(),
            why,
        });
    }
    if !confirm_destructive {
        return Err(Refused::QuitNeedsConfirmation {
            app: app.name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn app(name: &str, bundle_id: Option<&str>, pid: libc::pid_t) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            bundle_id: bundle_id.map(str::to_string),
            pid,
            active: false,
            regular: true,
        }
    }

    #[test]
    fn the_desktop_shell_is_never_quit() {
        for id in [
            "com.apple.finder",
            "com.apple.dock",
            "com.apple.systemuiserver",
            "com.apple.loginwindow",
            "com.apple.WindowServer",
            "com.apple.controlcenter",
        ] {
            assert!(
                protected_process(&app("x", Some(id), 500), 1, 2).is_some(),
                "{id} must be protected"
            );
        }
    }

    #[test]
    fn matching_ignores_case_but_not_length() {
        assert!(protected_process(&app("Finder", Some("com.apple.Finder"), 500), 1, 2).is_some());
        // A longer identifier is a different app, not a helper of Finder.
        assert!(
            protected_process(&app("Plugin", Some("com.apple.finder.plugin"), 500), 1, 2).is_none()
        );
    }

    #[test]
    fn an_unbundled_session_process_is_protected_by_name() {
        assert!(protected_process(&app("WindowServer", None, 500), 1, 2).is_some());
        assert!(protected_process(&app("loginwindow", None, 500), 1, 2).is_some());
    }

    #[test]
    fn a_bundled_app_cannot_borrow_a_protected_name() {
        assert!(protected_process(&app("loginwindow", Some("com.example.x"), 500), 1, 2).is_none());
    }

    #[test]
    fn ordinary_apps_are_not_protected() {
        assert!(
            protected_process(&app("TextEdit", Some("com.apple.TextEdit"), 500), 1, 2).is_none()
        );
    }

    #[test]
    fn the_server_and_its_host_are_protected() {
        let te = app("TextEdit", Some("com.apple.TextEdit"), 500);
        assert_eq!(protected_process(&te, 500, 2), Some(SELF_HOSTED));
        assert_eq!(protected_process(&te, 1, 500), Some(HOST));
        // launchd as a parent is not "the host": a server it started directly
        // has no agent app to protect, and pid 1 is never an AppInfo anyway.
        assert_eq!(protected_process(&app("launchd", None, 1), 9, 1), None);
    }

    #[test]
    fn closing_always_needs_confirmation() {
        let te = app("TextEdit", Some("com.apple.TextEdit"), 500);
        assert!(matches!(
            close_app_policy(&te, false, 1, 2),
            Err(Refused::QuitNeedsConfirmation { .. })
        ));
        assert!(close_app_policy(&te, true, 1, 2).is_ok());
    }

    #[test]
    fn protection_outranks_confirmation() {
        let dock = app("Dock", Some("com.apple.dock"), 500);
        assert!(matches!(
            close_app_policy(&dock, true, 1, 2),
            Err(Refused::ProtectedProcess { .. })
        ));
        assert!(matches!(
            close_app_policy(&dock, false, 1, 2),
            Err(Refused::ProtectedProcess { .. })
        ));
    }
}
