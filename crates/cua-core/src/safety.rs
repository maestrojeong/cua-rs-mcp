//! The gates that sit between a tool call and a synthesized event.
//!
//! Everything else in this workspace answers "can cua-rs reach this control".
//! This module answers "should it". The two questions are kept in separate
//! files on purpose: the delivery path is judged on whether the event lands,
//! and a refusal is judged on whether a human reading the transcript afterwards
//! agrees with it. Mixing them makes both harder to review, and `session.rs` is
//! already the largest file here.
//!
//! # The five gates
//!
//! | gate | default | flag | scope |
//! |---|:--|:--|:--|
//! | session scope | **off** | `CUA_ALLOWED_APPS=id,id` enables | actions |
//! | forbidden target | **on** | `CUA_ALLOW_FORBIDDEN_TARGETS=1` disables | actions + screenshots |
//! | destructive label | **on** | none — per-call `confirm_destructive` | activation-shaped actions |
//! | screen lock | **on** | none | actions |
//! | yield to human | **off** | `CUA_YIELD_TO_HUMAN=1` enables | actions |
//!
//! The three in the middle are on by default because the cost of a false refusal
//! is one round trip and an error message that says exactly how to proceed, while
//! the cost of a false permit is a click that cannot be taken back. Yield is off
//! because turning it on installs an event tap, which is a reversal of a
//! documented policy (DESIGN.md §9) and should be a choice rather than a
//! surprise. The session scope is off because it is the one gate that cannot be
//! guessed: only the human launching the server knows what the run is for, and
//! defaulting it to anything would either break every existing install or mean
//! nothing.
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

    /// The app is outside the scope the human granted when they started the
    /// server.
    ///
    /// Deliberately a different refusal from [`Refused::ForbiddenTarget`]: that
    /// one says "nobody may drive this", this one says "this run may drive only
    /// what it was scoped to". The distinction matters to the caller, because
    /// only one of them is resolvable at all, and neither is resolvable by the
    /// agent itself.
    #[error(
        "refusing to {verb} in `{app}` ({bundle_id}): this cua-rs run is scoped to \
         CUA_ALLOWED_APPS={allowed}, and `{bundle_id}` is not in it. Reading is still allowed \
         (get_app_state, find, list_apps) — the scope limits what may be *acted on*. Only the \
         human who launched the server can widen it: add the bundle identifier to \
         CUA_ALLOWED_APPS and restart. list_apps prints the identifier for every running app"
    )]
    NotInAllowlist {
        verb: &'static str,
        app: String,
        bundle_id: String,
        allowed: String,
    },

    /// Scoped run, and the target does not present a bundle identifier at all.
    ///
    /// Fails closed: an allowlist that silently admits everything it cannot name
    /// is not an allowlist. Kept separate from
    /// [`Refused::NotInAllowlist`] because "not on the list" and "cannot be
    /// compared to the list" call for different fixes.
    #[error(
        "refusing to {verb} in `{app}` (pid {pid}): this cua-rs run is scoped to \
         CUA_ALLOWED_APPS={allowed}, and this process publishes no bundle identifier, so it \
         cannot be matched against that scope. Scoping fails closed rather than admitting what it \
         cannot name. Reading is still allowed. Unset CUA_ALLOWED_APPS to drive unbundled \
         processes"
    )]
    UnidentifiableUnderAllowlist {
        verb: &'static str,
        app: String,
        pid: i32,
        allowed: String,
    },

    #[error(
        "refusing to {verb} {target}: this reads as a destructive control (matched {matched:?}). \
         Pass confirm_destructive: true on this same call to proceed — that is the whole gate, \
         and it exists so the decision is visible in the transcript rather than made silently by \
         the server. cua-rs deliberately over-reports here: if this is a false positive, \
         confirming is the correct answer and costs one round trip"
    )]
    NeedsConfirmation {
        verb: &'static str,
        target: String,
        matched: String,
    },

    /// The same gate, reached through the *key* rather than the label.
    ///
    /// Kept separate so the message cannot claim a scroll bar is "a destructive
    /// control" when what is destructive is the Delete being pressed on it.
    #[error(
        "refusing to press {key:?} on {target}: this key removes something here (matched \
         {matched:?}) — a modified Delete is Move to Trash in most apps, and a bare Delete \
         outside a text field acts on whatever is selected. Pass confirm_destructive: true on \
         this same call to proceed"
    )]
    NeedsConfirmationForKey {
        key: String,
        target: String,
        matched: String,
    },

    #[error(
        "refusing to {verb} in `{app}`: the human took over. Real input arrived {ago_ms}ms ago \
         while `{app}` was the frontmost app, and CUA_YIELD_TO_HUMAN=1 tells cua-rs to stand down \
         rather than compete for the window somebody is using. Retry once they have left it alone \
         for {idle_ms}ms, drive a different app, or unset CUA_YIELD_TO_HUMAN"
    )]
    HumanTookOver {
        verb: &'static str,
        app: String,
        ago_ms: u64,
        idle_ms: u64,
    },

    #[error(
        "refusing to {verb}: CUA_YIELD_TO_HUMAN=1 asks cua-rs to stop when the human starts using \
         the app it is driving, but the listen-only input tap that would notice could not be \
         created ({reason}), so it cannot tell. This fails closed on purpose. Grant Accessibility \
         to the process that launched cua-rs, or unset CUA_YIELD_TO_HUMAN"
    )]
    YieldWatchUnavailable { verb: &'static str, reason: String },
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
fn parse_allowlist(raw: &str) -> Vec<String> {
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
fn in_scope(list: &[String], bundle_id: &str) -> bool {
    let want = bundle_id.trim().to_ascii_lowercase();
    list.contains(&want)
}

/// How the scope reads back in a refusal.
fn allowed_apps_display() -> String {
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

// ── destructive labels ───────────────────────────────────────────────────────

/// English word stems whose presence in a control's label means pressing it
/// probably removes something.
///
/// Stems rather than whole words so `Delete`, `Deletes`, `Deleting` and
/// `Deleted` all match from one entry, and matched against *word* starts rather
/// than anywhere in the string so `Presets` does not match `reset` and
/// `Undelete` does not match `delete`. That is the one place this module trades
/// a little recall for precision, and it is safe to do here because the words
/// it declines to match are longer words with different meanings, not
/// destructive controls in disguise.
const DESTRUCTIVE_STEMS: &[&str] = &[
    "delet",      // Delete, Deletes, Deleting
    "remov",      // Remove, Removing
    "eras",       // Erase, Erasing
    "discard",    //
    "reset",      //
    "trash",      // Move to Trash, Empty Trash
    "uninstall",  //
    "revok",      // Revoke, Revoking
    "destroy",    //
    "wipe",       //
    "purge",      //
    "shred",      //
    "overwrite",  //
    "unsend",     //
    "deauthoriz", // Deauthorize
    "clear",      // Clear History, Clear All
];

/// Phrases that only read as destructive as a whole.
///
/// Matched against the label with its whitespace collapsed, so a line-wrapped
/// button still matches.
const DESTRUCTIVE_PHRASES: &[&str] = &[
    "don't save",
    "dont save",
    "do not save",
    "don’t save", // typographic apostrophe: what AppKit actually ships
    "empty trash",
    "move to trash",
    "shut down",
    "log out",
    "sign out",
    "factory settings",
];

/// Korean substrings that mean the same thing.
///
/// Korean has no spaces inside a verb stem the way English does, and the
/// maintainer's standing test targets (KakaoTalk among them) are Korean, so
/// these are matched as plain substrings anywhere in the label. That is more
/// permissive than the English rule on purpose: there is no `Presets` problem
/// here, because these sequences do not occur inside unrelated words.
const DESTRUCTIVE_KOREAN: &[&str] = &[
    "삭제",      // delete
    "제거",      // remove
    "지우",      // erase (지우기, 지움)
    "초기화",    // reset
    "버리",      // discard (버리기)
    "휴지통",    // trash
    "비우기",    // empty
    "폐기",      // dispose
    "탈퇴",      // close account
    "나가기",    // leave (a KakaoTalk room; takes its history with it)
    "저장 안",   // don't save
    "저장하지",  // 저장하지 않음
    "되돌리기",  // revert
    "강제 종료", // force quit
];

/// The element a gate is about to act on, flattened out of the snapshot.
///
/// A copy of the recorded node rather than a live AX read: the classification
/// has to describe the thing the *caller* chose, which is the thing the tree
/// showed them, and re-reading could quietly classify a different control if
/// the app recycled the handle. The action's own resolution catches that drift
/// separately.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub role: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    /// Whether the element's value can be written, i.e. whether this is a text
    /// field rather than a control.
    pub settable: bool,
    /// How the tree rendered it, for the error message.
    pub description: String,
}

impl Candidate {
    /// Whether this looks like somewhere a human types.
    ///
    /// Used only to decide what a bare Delete key press means: in a text field
    /// it is one character, and everywhere else it is a row, a file or a
    /// conversation.
    fn is_text_entry(&self) -> bool {
        if self.settable {
            return true;
        }
        let role = self.role.as_str();
        matches!(
            role,
            "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
        )
    }

    /// The text a label heuristic is allowed to read.
    ///
    /// Label and help always. The *value* only when the element is not
    /// writable: a button's value is part of what it says, while a text field's
    /// value is the user's own content, and classifying `set_value` on a note
    /// containing the word "delete" as a destructive action would be a refusal
    /// no confirmation could make sensible.
    fn classifiable_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(l) = &self.label {
            parts.push(l);
        }
        if let Some(h) = &self.help {
            parts.push(h);
        }
        if !self.settable {
            if let Some(v) = &self.value {
                parts.push(v);
            }
        }
        parts.join(" ")
    }
}

/// Collapse whitespace and lowercase, so one label matches one way.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The destructive token `text` contains, if any.
///
/// Public because the classification is the interesting part of this module and
/// deserves to be testable without constructing an app, a snapshot or a gate.
pub fn destructive_token(text: &str) -> Option<String> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }

    for phrase in DESTRUCTIVE_PHRASES {
        if norm.contains(phrase) {
            return Some((*phrase).to_string());
        }
    }
    for token in DESTRUCTIVE_KOREAN {
        if norm.contains(token) {
            return Some((*token).to_string());
        }
    }
    // Word-start matching for the English stems. Splitting on anything that is
    // not alphanumeric handles "Delete…", "(Delete)", "Delete/Remove" and
    // "delete_all" without a regex.
    for word in norm.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if let Some(stem) = DESTRUCTIVE_STEMS.iter().find(|s| word.starts_with(**s)) {
            return Some((*stem).to_string());
        }
    }
    None
}

/// Whether pressing `key` on `candidate` destroys something.
///
/// The label heuristic cannot see this case at all: a Delete key press carries
/// its meaning in the key, not in the element it lands on. Two rules, both
/// biased toward refusing:
///
/// - any chord that combines a modifier with Delete or Backspace — `cmd+delete`
///   is Move to Trash in the Finder and Delete Conversation in most chat apps;
/// - a bare Delete or Backspace anywhere that is not a text entry field,
///   including anywhere cua-rs could not identify the element at all.
pub fn destructive_key(key: &str, candidate: Option<&Candidate>) -> Option<String> {
    let key = key.trim().to_lowercase();
    let mut parts: Vec<&str> = key
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let base = parts.pop()?;
    let modified = !parts.is_empty();

    let is_delete = matches!(
        base,
        "delete" | "del" | "backspace" | "forwarddelete" | "forward_delete" | "forward-delete"
    );
    if !is_delete {
        return None;
    }
    if modified {
        return Some(key.clone());
    }
    match candidate {
        Some(c) if c.is_text_entry() => None,
        _ => Some(base.to_string()),
    }
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

// ── yield to human ───────────────────────────────────────────────────────────

/// What the listen-only tap turned out to be.
enum Watch {
    /// `CUA_YIELD_TO_HUMAN` is not set. The gate never fires.
    Off,
    /// The tap is up and reporting.
    ///
    /// Holding the [`cua_hid::humanwatch::InputWatch`] here is what keeps it
    /// alive: dropping this variant tears the tap down.
    On {
        watch: cua_hid::humanwatch::InputWatch,
    },
    /// The flag is set but the tap could not be created. Every action is
    /// refused, because a yield gate that cannot see is worse than no gate: it
    /// would silently promise a property it is not providing.
    Broken { reason: String },
}

/// Whether to watch real input, and what a refusal says when it fires.
///
/// # Where the tap itself lives
///
/// In `cua-hid`, alongside every other line in the workspace that names
/// `CGEvent`. That split is not tidiness. "Only `cua-hid` touches the event
/// APIs" is a claim a reader can check by reading `Cargo.toml` files, and it
/// stops being checkable the moment a second crate links the event surface for a
/// good reason. So the policy lives here and the mechanism lives there.
///
/// See [`cua_hid::humanwatch`] for why a listen-only tap is compatible with the
/// promise this project makes, and for what the tap records — a timestamp, and
/// nothing else.
pub struct HumanWatch {
    watch: Watch,
}

impl Default for HumanWatch {
    fn default() -> Self {
        Self { watch: Watch::Off }
    }
}

impl HumanWatch {
    /// Start watching, if the flag asks for it.
    ///
    /// Fails closed: a tap that could not be created leaves [`Watch::Broken`],
    /// and `guard` then refuses every action rather than proceeding unguarded.
    pub fn start() -> Self {
        if !yield_to_human_enabled() {
            return Self::default();
        }

        match cua_hid::humanwatch::InputWatch::start() {
            Ok(watch) => {
                tracing::info!(
                    "CUA_YIELD_TO_HUMAN=1: watching real input through a listen-only tap; \
                     actions on an app the human is using will be refused"
                );
                Self {
                    watch: Watch::On { watch },
                }
            }
            Err(reason) => {
                tracing::error!("yield-to-human tap unavailable: {reason}");
                Self {
                    watch: Watch::Broken { reason },
                }
            }
        }
    }

    /// Milliseconds since the last human input event, when the watch is up.
    fn since_human_input_ms(&self) -> Option<u64> {
        match &self.watch {
            Watch::On { watch } => watch.since_input_ms(),
            _ => None,
        }
    }
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
    target: Option<Target>,
    confirm_destructive: bool,
    key: Option<String>,
    labelled: Option<Candidate>,
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
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(label: &str) -> Candidate {
        Candidate {
            role: "AXButton".to_string(),
            label: Some(label.to_string()),
            description: format!("[1] AXButton {label:?}"),
            ..Candidate::default()
        }
    }

    fn field(value: &str) -> Candidate {
        Candidate {
            role: "AXTextField".to_string(),
            label: Some("Message".to_string()),
            value: Some(value.to_string()),
            settable: true,
            description: "[2] AXTextField".to_string(),
            ..Candidate::default()
        }
    }

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

    // ── destructive labels ───────────────────────────────────────────────

    #[test]
    fn plain_english_destructive_labels_are_caught() {
        for label in [
            "Delete",
            "Delete All",
            "Delete History",
            "Remove Account",
            "Erase All Content and Settings",
            "Discard Changes",
            "Reset",
            "Move to Trash",
            "Empty Trash",
            "Uninstall",
            "Revoke Access",
            "Don't Save",
            "Don’t Save",
            "Clear History",
            "Deleting…",
        ] {
            assert!(
                destructive_token(label).is_some(),
                "{label:?} should need confirmation"
            );
        }
    }

    #[test]
    fn korean_destructive_labels_are_caught() {
        for label in [
            "삭제",
            "모두 삭제",
            "채팅방 나가기",
            "대화 내용 삭제",
            "계정 제거",
            "설정 초기화",
            "휴지통으로 이동",
            "휴지통 비우기",
            "저장 안 함",
            "저장하지 않음",
            "회원 탈퇴",
        ] {
            assert!(
                destructive_token(label).is_some(),
                "{label:?} should need confirmation"
            );
        }
    }

    #[test]
    fn harmless_labels_are_not_caught() {
        for label in [
            "Cancel", "취소", "OK", "확인", "Save", "저장", "Send", "New Note", "Search", "Close",
            "닫기", "Reply", "Settings", "설정",
        ] {
            assert!(
                destructive_token(label).is_none(),
                "{label:?} must not need confirmation"
            );
        }
    }

    #[test]
    fn a_word_that_merely_contains_a_stem_is_not_destructive() {
        // The precision half of the heuristic. `Presets` contains `reset` and
        // `Undelete` contains `delete`, and neither one removes anything.
        assert!(destructive_token("Presets").is_none());
        assert!(destructive_token("Undelete").is_none());
        assert!(destructive_token("Preset Manager").is_none());
    }

    #[test]
    fn punctuation_and_wrapping_do_not_hide_a_destructive_label() {
        assert!(destructive_token("Delete…").is_some());
        assert!(destructive_token("(Delete)").is_some());
        assert!(destructive_token("Delete\n   All Messages").is_some());
        assert!(destructive_token("DELETE ALL").is_some());
    }

    #[test]
    fn an_empty_label_is_not_destructive() {
        assert!(destructive_token("").is_none());
        assert!(destructive_token("   ").is_none());
    }

    #[test]
    fn a_text_fields_own_contents_are_never_classified() {
        // Otherwise `set_value` on a note that happens to say "delete the old
        // files" would be refused, and no confirmation would make that sensible.
        let f = field("remind me to delete the old files");
        assert!(destructive_token(&f.classifiable_text()).is_none());
    }

    #[test]
    fn a_buttons_value_is_classified_because_it_is_part_of_what_it_says() {
        let mut c = button("");
        c.label = None;
        c.value = Some("Delete All".to_string());
        assert!(destructive_token(&c.classifiable_text()).is_some());
    }

    #[test]
    fn a_tooltip_counts_as_a_label() {
        let mut c = button("⌫");
        c.help = Some("Move this conversation to the trash".to_string());
        assert!(destructive_token(&c.classifiable_text()).is_some());
    }

    // ── destructive keys ─────────────────────────────────────────────────

    #[test]
    fn command_delete_is_destructive_wherever_it_lands() {
        assert!(destructive_key("cmd+delete", Some(&field("hello"))).is_some());
        assert!(destructive_key("command+backspace", None).is_some());
    }

    #[test]
    fn a_bare_delete_is_editing_in_a_text_field_and_destruction_anywhere_else() {
        assert!(destructive_key("delete", Some(&field("hello"))).is_none());
        assert!(destructive_key("backspace", Some(&field("hello"))).is_none());

        let row = Candidate {
            role: "AXRow".to_string(),
            label: Some("Mom".to_string()),
            description: "[9] AXRow \"Mom\"".to_string(),
            ..Candidate::default()
        };
        assert!(destructive_key("delete", Some(&row)).is_some());
    }

    #[test]
    fn an_unidentifiable_target_makes_a_bare_delete_destructive() {
        // Fail closed: not knowing what the key lands on is not a reason to
        // assume it lands somewhere harmless.
        assert!(destructive_key("delete", None).is_some());
    }

    #[test]
    fn ordinary_keys_and_chords_are_not_destructive() {
        for key in ["return", "escape", "cmd+s", "cmd+shift+p", "a", "tab", "up"] {
            assert!(
                destructive_key(key, Some(&field("hello"))).is_none(),
                "{key:?} must not need confirmation"
            );
        }
    }

    #[test]
    fn key_classification_ignores_case_and_spacing() {
        assert!(destructive_key("  CMD + Delete ", None).is_some());
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

    fn app(name: &str, bundle: &str) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            bundle_id: Some(bundle.to_string()),
            pid: 4242,
            active: false,
            regular: true,
        }
    }

    fn a_gate() -> Gate {
        Gate::at(
            "click",
            &Target::Index {
                index: 1,
                snapshot_id: None,
                expected_role: None,
            },
        )
    }

    #[test]
    fn confirming_clears_the_destructive_gate_and_nothing_else() {
        // Built without touching the environment or the window server: this
        // asserts on the classification the gate consults, which is the part a
        // permission-free test can see. The wiring itself is exercised by the
        // MCP surface tests.
        let confirmed = a_gate().confirmed(true);
        assert!(confirmed.confirm_destructive);
        assert!(!a_gate().confirm_destructive);
    }

    #[test]
    fn a_gate_carries_the_key_for_press_key_and_the_target_for_everything_else() {
        let g = a_gate().with_key("cmd+delete");
        assert_eq!(g.key.as_deref(), Some("cmd+delete"));
        assert!(g.target().is_some());
        assert!(Gate::elementless("click_in_window").target().is_none());
    }

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
    fn a_confirmation_error_names_the_parameter_that_clears_it() {
        let text = Refused::NeedsConfirmation {
            verb: "click",
            target: "[7] AXButton \"Delete All\"".to_string(),
            matched: "delet".to_string(),
        }
        .to_string();
        assert!(text.contains("confirm_destructive: true"));
        assert!(text.contains("Delete All"));
    }

    #[test]
    fn a_key_refusal_blames_the_key_and_not_the_control() {
        let text = Refused::NeedsConfirmationForKey {
            key: "cmd+delete".to_string(),
            target: "[9] AXRow \"Mom\"".to_string(),
            matched: "cmd+delete".to_string(),
        }
        .to_string();
        assert!(text.contains("cmd+delete"));
        assert!(
            !text.contains("destructive control"),
            "a scroll bar is not a destructive control just because Delete was pressed on it"
        );
        assert!(text.contains("confirm_destructive: true"));
    }

    #[test]
    fn a_yield_error_says_how_to_resume() {
        let text = Refused::HumanTookOver {
            verb: "click",
            app: "KakaoTalk".to_string(),
            ago_ms: 120,
            idle_ms: 3_000,
        }
        .to_string();
        assert!(text.contains("3000ms"));
        assert!(text.contains("CUA_YIELD_TO_HUMAN"));
    }

    #[test]
    fn a_watch_that_was_never_started_never_refuses() {
        let watch = HumanWatch::default();
        assert!(watch.since_human_input_ms().is_none());
        assert!(matches!(watch.watch, Watch::Off));
    }

    #[test]
    fn the_screenshot_refusal_explains_itself_when_it_fires() {
        // Calls the classifier directly rather than `screenshot_refusal`, which
        // consults a process-wide env flag another test could have set.
        let a = app("1Password", "com.1password.1password");
        assert!(forbidden_bundle(a.bundle_id.as_deref().unwrap()).is_some());
        let a = app("TextEdit", "com.apple.TextEdit");
        assert!(forbidden_bundle(a.bundle_id.as_deref().unwrap()).is_none());
    }

    #[test]
    fn the_idle_window_is_clamped_into_something_usable() {
        // The parse-and-clamp rule, exercised without mutating the process
        // environment (`yield_idle_ms` caches its answer for the process).
        let parse = |v: &str| {
            v.trim()
                .parse::<u64>()
                .ok()
                .map(|n| n.clamp(250, 60_000))
                .unwrap_or(3_000)
        };
        assert_eq!(parse("5000"), 5_000);
        assert_eq!(parse("1"), 250);
        assert_eq!(parse("999999"), 60_000);
        assert_eq!(parse("nonsense"), 3_000);
    }

    // ── the session scope ────────────────────────────────────────────────────

    #[test]
    fn an_allowlist_is_split_trimmed_and_lowercased() {
        assert_eq!(
            parse_allowlist("com.kakao.KakaoTalkMac , com.apple.TextEdit"),
            vec![
                "com.kakao.kakaotalkmac".to_string(),
                "com.apple.textedit".to_string()
            ]
        );
        assert_eq!(
            parse_allowlist("  com.apple.Safari  "),
            vec!["com.apple.safari".to_string()]
        );
    }

    #[test]
    fn a_scope_that_names_nothing_admits_nothing() {
        // `CUA_ALLOWED_APPS=$TYPO` expands to an empty value, and a gate that
        // opens itself on a misspelling fails in the wrong direction. So an
        // empty scope is an empty scope: everything is refused, loudly and
        // immediately. Unsetting the variable is how to ask for unscoped.
        for raw in ["", "   ", ",,", " , , "] {
            let list = parse_allowlist(raw);
            assert!(list.is_empty(), "{raw:?} should name no app");
            assert!(
                !in_scope(&list, "com.apple.TextEdit"),
                "{raw:?} must not admit anything"
            );
        }
    }

    #[test]
    fn scope_matching_ignores_case_and_surrounding_space() {
        let list = parse_allowlist("com.kakao.KakaoTalkMac");
        assert!(in_scope(&list, "com.kakao.KakaoTalkMac"));
        assert!(in_scope(&list, "com.kakao.kakaotalkmac"));
        assert!(in_scope(&list, "  com.kakao.KakaoTalkMac  "));
        assert!(!in_scope(&list, "com.apple.TextEdit"));
    }

    #[test]
    fn a_scope_entry_never_matches_a_prefix() {
        // The failure this rules out is a scope of `com.apple.Safari` silently
        // admitting Safari Technology Preview, and a scope of `com.apple`
        // admitting every Apple app on the machine.
        let one = parse_allowlist("com.apple.Safari");
        assert!(!in_scope(&one, "com.apple.SafariTechnologyPreview"));

        let vendor = parse_allowlist("com.apple");
        assert!(!in_scope(&vendor, "com.apple.Safari"));
        assert!(!in_scope(&vendor, "com.apple.TextEdit"));
    }

    #[test]
    fn the_scope_and_the_forbidden_floor_are_independent() {
        // Scoping a run to a password manager must not lift the floor: the
        // allowlist widens nothing, it only narrows. `guard` checks the floor
        // first for exactly this reason.
        let list = parse_allowlist("com.1password.1password");
        assert!(in_scope(&list, "com.1password.1password"));
        assert!(forbidden_bundle("com.1password.1password").is_some());
    }
}
