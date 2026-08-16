//! The gates that sit between a tool call and a synthesized event.
//!
//! Everything else in this workspace answers "can cua-rs reach this control".
//! This module answers "should it". The two questions are kept in separate
//! files on purpose: the delivery path is judged on whether the event lands,
//! and a refusal is judged on whether a human reading the transcript afterwards
//! agrees with it. Mixing them makes both harder to review, and `session.rs` is
//! already the largest file here.
//!
//! # The gates
//!
//! | gate | default | flag | scope |
//! |---|:--|:--|:--|
//! | forbidden target | **on** | `CUA_ALLOW_FORBIDDEN_TARGETS=1` disables | actions + screenshots |
//! | screen lock | **on** | none | actions |
//!
//! Both are on by default because the cost of a false refusal is one round trip
//! and an error message that says exactly how to proceed, while the cost of a
//! false permit is a click that cannot be taken back.
//!
//! # Why reads are not gated the same way as actions
//!
//! The blocklist refuses *acting* and allows *reading*, with one exception. The
//! split is not squeamishness about a hard line — it follows from what each
//! operation can do wrong:
//!
//! - An action is irreversible and mis-aimable. cua-rs resolves a target from a
//!   tree it walked some milliseconds ago, and DESIGN.md §10 already records
//!   that its own change-detection is a heuristic. In a password manager the
//!   difference between the control cua-rs meant and the one next to it is the
//!   difference between reading a vault and emptying it.
//! - A tree read is bounded by what accessibility publishes, which is the same
//!   thing any screen reader on the machine can already see, and it is what
//!   makes a refusal diagnosable at all — an agent that cannot even call
//!   `list_apps` on a blocked app cannot explain to its user why it stopped.
//!
//! The exception is the screenshot. Pixels are the one read that reproduces the
//! secret itself rather than a description of the UI holding it, so
//! `get_app_state` on a forbidden target returns the tree and drops the image,
//! with a warning saying so. That keeps the blocked app observable enough to
//! reason about and stops the most direct exfiltration path.
//!
//! # Bias
//!
//! Every heuristic here is tuned to over-refuse. A false positive costs one
//! extra tool call whose error text names the exact parameter that clears it; a
//! false negative costs a deleted conversation. Where the two could be traded
//! off, this module takes the false positive.

use std::sync::OnceLock;

use crate::apps::AppInfo;
use crate::session::Target;

// ── refusals ─────────────────────────────────────────────────────────────────

/// Why an action was refused before it was attempted.
///
/// Every variant names what was refused and what the caller can do about it.
/// An error an agent cannot act on just becomes a retry loop.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Refused {
    #[error(
        "refusing to {verb} in `{app}` ({bundle_id}): cua-rs treats this app as a forbidden \
         target because it {why}. A synthesized event here can expose or destroy a credential, \
         and nothing in cua-rs can tell the safe control from the unsafe one reliably enough to \
         risk it. Reading is still allowed (get_app_state, find, list_apps), except for \
         screenshots. To act on it anyway, set CUA_ALLOW_FORBIDDEN_TARGETS=1 in the environment \
         of the process that launches cua-rs and restart the server"
    )]
    ForbiddenTarget {
        verb: &'static str,
        app: String,
        bundle_id: String,
        why: &'static str,
    },

    #[error(
        "refusing to {verb}: this login session is locked or its screen saver is running. cua-rs \
         will not deliver input to a session no human is watching — the window that would receive \
         it cannot be seen, and neither can the result. Reads still work. Unlock the session and \
         retry"
    )]
    SessionLocked { verb: &'static str },
}

// ── environment flags ────────────────────────────────────────────────────────

/// Read a `1`/`true` switch once, the way the rest of the workspace does.
fn flag(name: &str) -> bool {
    std::env::var(name).map(|v| truthy(&v)).unwrap_or(false)
}

/// What counts as "on" for a cua-rs environment switch.
///
/// Matches the existing `CUA_AX_FIRST` / `CUA_KEY_AX_ONLY` reading exactly, so
/// there is one answer to "did I spell it right" across the whole server.
/// Anything else — including `0`, `false` and the empty string — is off, which
/// makes `CUA_ALLOW_FORBIDDEN_TARGETS=0` mean what a reader expects rather than
/// silently disabling the blocklist because the variable was merely present.
pub fn truthy(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

/// Whether the credential/security blocklist has been switched off.
pub fn forbidden_targets_allowed() -> bool {
    static ALLOW: OnceLock<bool> = OnceLock::new();
    *ALLOW.get_or_init(|| {
        let allowed = flag("CUA_ALLOW_FORBIDDEN_TARGETS");
        if allowed {
            tracing::warn!(
                "CUA_ALLOW_FORBIDDEN_TARGETS=1: cua-rs will drive password managers, Keychain \
                 Access, System Settings and login surfaces like any other app"
            );
        }
        allowed
    })
}

// ── forbidden targets ────────────────────────────────────────────────────────

/// A bundle identifier this server will not deliver input to, and why.
///
/// Matching is on the bundle identifier, never the display name. A name is
/// user-visible text that any app can choose: an app called "Keychain Access"
/// is trivial to ship, and — the direction that actually matters — the real
/// Keychain Access is called something else in every non-English locale, so a
/// name-based list would silently fall open on the maintainer's own machine.
struct Forbidden {
    /// Matched against the whole bundle id, case-insensitively, and against any
    /// bundle id that extends it with a `.` (so `com.1password.1password` also
    /// covers `com.1password.1password.helper`).
    prefix: &'static str,
    why: &'static str,
}

const CREDENTIALS: &str = "holds credentials";
const SECURITY_SURFACE: &str = "is a system security or privacy surface";
const AUTH_PROMPT: &str = "is a login, unlock or authorization prompt";

/// The blocklist.
///
/// Three groups, and the reason differs per group because the error message
/// quotes it. This is not meant to be exhaustive — no list of third-party
/// password managers can be — which is why [`suspicious_token`] backs it up.
const FORBIDDEN: &[Forbidden] = &[
    // Apple's own credential stores.
    Forbidden {
        prefix: "com.apple.keychainaccess",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.apple.Passwords",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.apple.PasswordManagerBrowserExtensionHelper",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.apple.KeychainCircleNotification",
        why: CREDENTIALS,
    },
    // Third-party password managers, most-used first. Bundle ids differ between
    // the App Store and direct-download builds of the same product, so several
    // products appear more than once.
    Forbidden {
        prefix: "com.1password",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.agilebits.onepassword",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.bitwarden.desktop",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.lastpass",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.dashlane",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "org.keepassxc.keepassxc",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.kyuubi.KeePassium",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "in.sinew.Enpass-Desktop",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "me.proton.pass",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.nordsecurity.nordpass",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.authy.authy-mac",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.stephenradford.MacPass",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.mattreduce.Secrets",
        why: CREDENTIALS,
    },
    Forbidden {
        prefix: "com.strongbox",
        why: CREDENTIALS,
    },
    // System Settings. Blocked whole rather than per-pane: the bundle id is all
    // this gate can see, and the app that holds Privacy & Security, FileVault,
    // Login Items and Touch ID is the same process as the one that holds the
    // wallpaper picker. Refusing the app is over-broad and honest; pretending to
    // know which pane is open would be neither.
    Forbidden {
        prefix: "com.apple.systempreferences",
        why: SECURITY_SURFACE,
    },
    Forbidden {
        prefix: "com.apple.SystemSettings",
        why: SECURITY_SURFACE,
    },
    Forbidden {
        prefix: "com.apple.preference",
        why: SECURITY_SURFACE,
    },
    // Login, unlock and authorization surfaces. A synthesized click on one of
    // these grants something, and the human who would have granted it is by
    // definition not the one clicking.
    Forbidden {
        prefix: "com.apple.SecurityAgent",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.loginwindow",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.CoreAuthUI",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.LocalAuthentication.UIAgent",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.ScreenSaver.Engine",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.screensaver",
        why: AUTH_PROMPT,
    },
    Forbidden {
        prefix: "com.apple.TouchBarServer",
        why: AUTH_PROMPT,
    },
];

/// Bundle-id fragments that make an unknown app a credential store by
/// self-description.
///
/// The curated list above cannot keep up with every password manager, and the
/// failure mode of missing one is the worst this module has. A bundle id is not
/// user-visible marketing text — a developer writing `password` into their
/// reverse-DNS identifier is telling us what the app is for — so a substring
/// match on it is a cheap, deliberately over-broad backstop. The cost of the
/// false positive it will eventually produce is one environment variable.
const SUSPICIOUS: &[&str] = &[
    "1password",
    "authenticator",
    "bitwarden",
    "dashlane",
    "keepass",
    "keychain",
    "lastpass",
    "passwd",
    "password",
];

/// The suspicious fragment `bundle_id` contains, if any.
fn suspicious_token(bundle_id: &str) -> Option<&'static str> {
    let lower = bundle_id.to_lowercase();
    SUSPICIOUS.iter().copied().find(|t| lower.contains(t))
}

/// Why this bundle identifier may not be acted on, or `None`.
///
/// Ignores [`forbidden_targets_allowed`] — this is the classification, and the
/// override is applied by the callers so that the reason is still available for
/// logging when somebody has opted out.
pub fn forbidden_bundle(bundle_id: &str) -> Option<&'static str> {
    let id = bundle_id.trim();
    if id.is_empty() {
        return None;
    }
    let lower = id.to_lowercase();
    for entry in FORBIDDEN {
        let prefix = entry.prefix.to_lowercase();
        if lower == prefix || lower.starts_with(&format!("{prefix}.")) {
            return Some(entry.why);
        }
    }
    suspicious_token(id).map(|_| CREDENTIALS)
}

/// The warning `get_app_state` attaches when it drops a forbidden app's image.
///
/// `None` when the app is not forbidden or the operator has opted out. Returns
/// the warning text rather than a boolean so the whole explanation lives here.
pub fn screenshot_refusal(app: &AppInfo) -> Option<String> {
    if forbidden_targets_allowed() {
        return None;
    }
    let bundle_id = app.bundle_id.as_deref()?;
    let why = forbidden_bundle(bundle_id)?;
    Some(format!(
        "screenshot withheld: `{}` ({bundle_id}) {why}, and a picture of it would reproduce the \
         secret itself rather than describe the UI around it. The accessibility tree below is \
         still the full read. Actions on this app are refused too; \
         CUA_ALLOW_FORBIDDEN_TARGETS=1 lifts both.",
        app.name
    ))
}

// ── screen lock ──────────────────────────────────────────────────────────────

/// Whether this login session is locked or showing its screen saver.
///
/// Two public reads, both cheap enough to do at every action boundary, which is
/// why there is no observer here. A distributed-notification observer would
/// need a run loop and a piece of state that can go stale between the
/// notification and the action; a direct read cannot be stale, and it keeps the
/// single-native-thread model (DESIGN.md §4) intact. Measured cost is a
/// dictionary copy per action, against an AX round trip that already costs
/// milliseconds.
pub fn session_locked() -> bool {
    screen_is_locked() || screen_saver_running()
}

/// `CGSessionCopyCurrentDictionary()["CGSSessionScreenIsLocked"]`.
///
/// Absent rather than `false` when the screen is unlocked, so a missing key is
/// read as unlocked. Fails *open* on purpose: `CGSessionCopyCurrentDictionary`
/// returns nothing at all for a process with no window-server session, and
/// treating that as "locked" would make cua-rs permanently refuse to act in
/// exactly the headless setups where the lock cannot happen.
fn screen_is_locked() -> bool {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

    let Some(session) = objc2_core_graphics::CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let key = CFString::from_static_str("CGSSessionScreenIsLocked");
    // SAFETY: `session` is a live CFDictionary and `key` outlives the call.
    let value = unsafe {
        CFDictionary::value(
            &session,
            (&*key as *const CFString).cast::<std::ffi::c_void>().cast(),
        )
    };
    if value.is_null() {
        return false;
    }
    // SAFETY: the window server documents this key's value as a CFBoolean.
    let flag: &CFBoolean = unsafe { &*value.cast() };
    flag.value()
}

/// Whether macOS's screen saver process is running.
///
/// The screen saver is a separate app, so its presence in the running-app list
/// is the whole check — no notification observer, no private API. It is not the
/// same condition as a lock (a saver on a machine with "require password" off
/// leaves the session unlocked), but it means the same thing here: whatever the
/// human would have seen the action do, they are not seeing it.
fn screen_saver_running() -> bool {
    crate::apps::list_apps().iter().any(|a| {
        a.bundle_id
            .as_deref()
            .is_some_and(|b| b.eq_ignore_ascii_case("com.apple.ScreenSaver.Engine"))
    })
}

// ── the gate ─────────────────────────────────────────────────────────────────

/// One action's worth of safety context, assembled by the caller in `session`.
///
/// Built at the call site rather than inferred here so that the choice of which
/// gates apply to which tool is visible in one line next to the tool, instead of
/// being a table in this file that drifts out of sync with the tools.
#[derive(Debug, Clone)]
pub struct Gate {
    verb: &'static str,
}

impl Gate {
    /// A gate for an action aimed at an element.
    pub fn at(verb: &'static str, _target: &Target) -> Self {
        Self { verb }
    }

    /// A gate for an action with no element behind it — `click_in_window`.
    pub fn elementless(verb: &'static str) -> Self {
        Self { verb }
    }
}

/// Run every gate, in the order a reader would want them reported.
///
/// Session-wide conditions first, then app-wide, so the error a caller sees is
/// the most fundamental reason rather than the last one checked. Refusing a
/// click because the app holds credentials, when the real problem is that the
/// screen has been locked for an hour, sends the agent down the wrong path.
pub(crate) fn guard(app: &AppInfo, gate: &Gate) -> std::result::Result<(), Refused> {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── blocklist ────────────────────────────────────────────────────────

    #[test]
    fn the_blocklist_matches_apples_credential_stores() {
        assert!(forbidden_bundle("com.apple.keychainaccess").is_some());
        assert!(forbidden_bundle("com.apple.Passwords").is_some());
    }

    #[test]
    fn the_blocklist_matches_third_party_password_managers() {
        for id in [
            "com.1password.1password",
            "com.agilebits.onepassword7",
            "com.bitwarden.desktop",
            "com.lastpass.LastPass",
            "org.keepassxc.keepassxc",
            "me.proton.pass.electron",
        ] {
            assert!(forbidden_bundle(id).is_some(), "{id} should be forbidden");
        }
    }

    #[test]
    fn the_blocklist_covers_helper_processes_of_a_blocked_app() {
        // A password manager's helper can hold the same window a click would
        // land in, so an entry has to cover the family, not one process.
        assert!(forbidden_bundle("com.1password.1password.helper").is_some());
        assert!(forbidden_bundle("com.apple.keychainaccess.SomeHelper").is_some());
    }

    #[test]
    fn the_blocklist_does_not_match_a_longer_unrelated_identifier() {
        // The `.` boundary is what stops a prefix rule from swallowing an
        // identifier that merely starts with the same characters.
        assert!(forbidden_bundle("com.apple.systempreferencesque").is_none());
        // …though a name-based backstop still fires when the id says
        // "keychain" anywhere, which is the deliberate over-reach documented on
        // `SUSPICIOUS`.
        assert!(forbidden_bundle("com.apple.keychainaccessorize").is_some());
    }

    #[test]
    fn the_blocklist_matches_security_surfaces_and_login_prompts() {
        assert_eq!(
            forbidden_bundle("com.apple.systempreferences"),
            Some(SECURITY_SURFACE)
        );
        assert_eq!(forbidden_bundle("com.apple.loginwindow"), Some(AUTH_PROMPT));
        assert_eq!(
            forbidden_bundle("com.apple.SecurityAgent"),
            Some(AUTH_PROMPT)
        );
    }

    #[test]
    fn an_unknown_password_manager_is_still_caught_by_its_bundle_id() {
        assert!(forbidden_bundle("com.example.SuperPasswordVault").is_some());
        assert!(forbidden_bundle("net.example.totp-authenticator").is_some());
    }

    #[test]
    fn ordinary_apps_are_not_forbidden() {
        for id in [
            "com.apple.TextEdit",
            "com.apple.Notes",
            "com.kakao.KakaoTalk",
            "com.tinyspeck.slackmacgap",
            "com.google.Chrome",
            "com.apple.Terminal",
        ] {
            assert!(forbidden_bundle(id).is_none(), "{id} should be drivable");
        }
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_surrounding_space() {
        assert!(forbidden_bundle("  COM.APPLE.KEYCHAINACCESS  ").is_some());
    }

    #[test]
    fn an_app_with_no_bundle_identifier_is_not_matched_by_accident() {
        assert!(forbidden_bundle("").is_none());
        assert!(forbidden_bundle("   ").is_none());
    }

    // ── flags ────────────────────────────────────────────────────────────

    #[test]
    fn only_one_and_true_switch_a_flag_on() {
        assert!(truthy("1"));
        assert!(truthy("true"));
        assert!(truthy("TRUE"));
        assert!(truthy("True"));
        for off in ["0", "false", "", "yes", "on", "2"] {
            assert!(!truthy(off), "{off:?} must not enable a gate");
        }
    }

    // ── the gate as a whole ──────────────────────────────────────────────

    #[test]
    fn a_forbidden_target_error_names_the_app_and_the_way_out() {
        let text = Refused::ForbiddenTarget {
            verb: "click",
            app: "1Password".to_string(),
            bundle_id: "com.1password.1password".to_string(),
            why: CREDENTIALS,
        }
        .to_string();
        assert!(text.contains("1Password"));
        assert!(text.contains("com.1password.1password"));
        assert!(text.contains("CUA_ALLOW_FORBIDDEN_TARGETS=1"));
    }

    #[test]
    fn a_lock_refusal_says_reads_still_work() {
        let text = Refused::SessionLocked { verb: "click" }.to_string();
        assert!(text.contains("locked"));
        assert!(text.contains("Reads still work"));
    }

    #[test]
    fn a_gate_can_be_built_with_or_without_an_element() {
        let target = Target::Index {
            index: 1,
            snapshot_id: None,
            expected_role: None,
        };
        assert_eq!(Gate::at("click", &target).verb, "click");
        assert_eq!(Gate::elementless("click_in_window").verb, "click_in_window");
    }
}
