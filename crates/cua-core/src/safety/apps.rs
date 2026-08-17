//! App scope, forbidden-target, and screenshot policy.

use super::*;

// ── environment flags ────────────────────────────────────────────────────────

/// Read a `1`/`true` switch once, the way the rest of the workspace does.
pub(super) fn flag(name: &str) -> bool {
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

/// The bundle identifiers this run is allowed to *act* on, if it was scoped.
///
/// `None` means unscoped — every app is actionable, which is what cua-rs has
/// always done and stays the default so an existing install does not break on
/// upgrade. `Some` means the human who launched the process named the apps this
/// run is for, and everything else is refused.
///
/// # Why an environment variable and not a tool
///
/// A `grant_app` tool would let the agent widen its own scope, which is not a
/// scope. The whole value here is that the boundary is set by the human who
/// started the process and cannot be moved from inside it — so it lives in the
/// environment, is read once, and is never writable at runtime.
///
/// # Why bundle identifiers
///
/// Same reason [`FORBIDDEN`] matches on them: display names are localized, and
/// a Korean-language machine defeats a list of English names. `list_apps`
/// already prints the identifier next to every app, so the value a user needs
/// is one call away.
///
/// Comparison is case-insensitive and entries are trimmed, because a bundle id
/// pasted out of a plist or a log should not fail on whitespace.
pub fn allowed_apps() -> Option<&'static [String]> {
    static ALLOWED: OnceLock<Option<Vec<String>>> = OnceLock::new();
    ALLOWED
        .get_or_init(|| {
            let list = parse_allowlist(&std::env::var("CUA_ALLOWED_APPS").ok()?);
            if list.is_empty() {
                tracing::warn!(
                    "CUA_ALLOWED_APPS is set but names no bundle identifier, so this run may act \
                     on nothing. That is what an empty scope means; unset the variable to run \
                     unscoped"
                );
            } else {
                tracing::info!(
                    "scoped to {} app(s) for actions: {}",
                    list.len(),
                    list.join(", ")
                );
            }
            Some(list)
        })
        .as_deref()
}

/// Split and normalize the raw variable. Separate from [`allowed_apps`] so the
/// rules are testable without a process-wide environment or a `OnceLock` that
/// can only be initialized once per test binary.
///
/// # An empty value is an empty scope, not an absent one
///
/// `CUA_ALLOWED_APPS=""` refuses every action rather than running unscoped, and
/// the deciding case is `CUA_ALLOWED_APPS=$TYPO`, which expands to exactly that.
/// A scope that opens itself when its value fails to arrive is a gate that fails
/// open on a misspelling, which is the wrong direction for the one gate here
/// whose whole job is to fail closed. Refusing everything is loud, immediate, and
/// says exactly what to fix; silently permitting everything looks like success
/// until it is not. Unsetting the variable is how a caller asks for unscoped.
pub(super) fn parse_allowlist(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether a normalized scope admits a bundle identifier.
///
/// Exact match on the whole identifier, never a prefix: `com.apple.Safari` must
/// not admit `com.apple.SafariTechnologyPreview`, and a scope of `com.apple`
/// must not quietly mean "all of Apple's apps".
pub(super) fn in_scope(list: &[String], bundle_id: &str) -> bool {
    let want = bundle_id.trim().to_ascii_lowercase();
    list.contains(&want)
}

/// How the scope reads back in a refusal.
pub(super) fn allowed_apps_display() -> String {
    allowed_apps()
        .map(|apps| apps.join(","))
        .unwrap_or_default()
}

/// Whether `bundle_id` is inside this run's scope. `true` when unscoped.
pub fn app_allowed(bundle_id: &str) -> bool {
    match allowed_apps() {
        None => true,
        Some(apps) => in_scope(apps, bundle_id),
    }
}

/// Whether cua-rs should stop acting on an app the human has started using.
pub fn yield_to_human_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| flag("CUA_YIELD_TO_HUMAN"))
}

/// How long the human has to be idle before cua-rs picks the app back up.
///
/// Long enough that a pause between two keystrokes does not read as "they are
/// done", short enough that an agent is not stalled for a whole turn.
/// `CUA_YIELD_IDLE_MS` overrides it; values outside 250..=60000 are clamped
/// rather than rejected, because a typo in an environment variable should not
/// take the safety gate out of service in either direction.
pub fn yield_idle_ms() -> u64 {
    static IDLE: OnceLock<u64> = OnceLock::new();
    *IDLE.get_or_init(|| {
        std::env::var("CUA_YIELD_IDLE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|v| v.clamp(250, 60_000))
            .unwrap_or(3_000)
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

pub(super) const CREDENTIALS: &str = "holds credentials";
pub(super) const SECURITY_SURFACE: &str = "is a system security or privacy surface";
pub(super) const AUTH_PROMPT: &str = "is a login, unlock or authorization prompt";

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
pub(super) fn suspicious_token(bundle_id: &str) -> Option<&'static str> {
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
