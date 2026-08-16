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

    /// The same gate, reached through the *question the dialog is asking*.
    ///
    /// Kept separate from [`Refused::NeedsConfirmation`] because the two accuse
    /// different things. That one says "this control removes something"; this
    /// one says "this control is terse and the sheet around it is not", which is
    /// the ordinary shape of a macOS alert. The message quotes the question, so
    /// a human reading the transcript can see the evidence rather than trusting
    /// a classifier they cannot inspect.
    #[error(
        "refusing to {verb} {target}: it answers a {context} that reads as asking a destructive \
         question — {question:?} (matched {matched:?}). The button is terse but the sheet it sits \
         in is not, and cua-rs classifies the decision, not the wording of the answer. Pass \
         confirm_destructive: true on this same call to proceed. Dismissing answers (Cancel, No, \
         취소 …) are never refused here, so backing out of this dialog needs no confirmation"
    )]
    NeedsConfirmationInContext {
        verb: &'static str,
        target: String,
        context: String,
        question: String,
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
    /// What the control says in a child element rather than an attribute, for
    /// the toolkits that do that. See [`caption`].
    pub caption: Option<String>,
    /// How the tree rendered it, for the error message.
    pub description: String,
    /// The question the nearest enclosing decision context is asking, if the
    /// element sits inside one. See [`decision_context`] for what qualifies and
    /// how far the search goes.
    pub context: Option<DecisionContext>,
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
    /// Label, help and caption always — all three are the control describing
    /// itself, and which of them an app populates is a toolkit accident. The
    /// *value* only when the element is not writable: a button's value is part
    /// of what it says, while a text field's value is the user's own content,
    /// and classifying `set_value` on a note containing the word "delete" as a
    /// destructive action would be a refusal no confirmation could make
    /// sensible.
    fn classifiable_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(l) = &self.label {
            parts.push(l);
        }
        if let Some(c) = &self.caption {
            parts.push(c);
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

    /// Whether the question around this element may be held against it.
    ///
    /// Two exemptions, both narrow and both for the same reason — the gate has
    /// to stay rare enough that clearing it is a decision rather than a habit:
    ///
    /// - **A dismissing answer.** Cancel is how a caller *avoids* the
    ///   destruction the dialog is offering. Refusing it would leave an agent
    ///   holding a modal sheet it cannot back out of, and the only way through
    ///   would be to send `confirm_destructive: true` — teaching it to confirm
    ///   its way out of alerts, which is precisely the habit that would make the
    ///   gate useless when it matters.
    /// - **Typing.** A text field inside a dialog is not the decision; the
    ///   button underneath it is. `set_value` into the name field of a sheet
    ///   that mentions deleting changes nothing on its own, and refusing it is
    ///   the "note that says delete" mistake one level out.
    fn accepts_context_evidence(&self) -> bool {
        if self.is_text_entry() {
            return false;
        }
        // Either place the toolkit put the answer's wording. A Chromium button
        // whose caption is a child still says "Cancel".
        let says = self.label.as_deref().or(self.caption.as_deref());
        match says {
            Some(label) => !is_dismissing_answer(label),
            None => true,
        }
    }
}

// ── the question a dialog is asking ──────────────────────────────────────────
//
// A label heuristic that reads only the control it is about to press cannot see
// the most common destructive arrangement on macOS: the alert holds the verb and
// the button holds one word. "OK" under "Delete 4 items?" is not a labelling
// accident, it is what an alert *is* — the question is asked once, at the top,
// and the answers are terse by design.
//
// Widening to "the ancestor chain" is the obvious move and it is wrong. Every
// element in a mail window descends from a window whose title is a subject line;
// every control in a chat window descends from a window containing the whole
// conversation. Reading ancestors indiscriminately makes the gate fire on any
// app whose *content* mentions deleting, and a gate that fires constantly is
// worse than no gate, because `confirm_destructive: true` then gets attached to
// every call reflexively and stops meaning anything.
//
// So the rule here is about *kind*, not distance:
//
//  1. **Only a decision context is evidence.** An `AXSheet`, an `AXDialog`, or a
//     window whose subrole marks it as a dialog exists for one reason — to ask a
//     question and collect an answer — so its text *is* the question. An
//     ordinary `AXWindow`, `AXGroup` or `AXScrollArea` is layout, and its text is
//     the user's content. No amount of depth turns content into a question.
//  2. **The nearest one, and no further.** The search walks up from the target
//     and stops at the first decision context it finds, and at the first
//     ordinary window if it finds one first. That bound is structural rather
//     than numeric: an alert's message sits one or two levels above its buttons
//     in AppKit and rather more in a cross-platform toolkit, and "N levels" would
//     break differently in each. Stopping at the enclosing question also gets
//     nesting right — a confirmation sheet raised on top of a disk-erase dialog
//     is answering *its own* question, not the one behind it.
//  3. **Prose, not answers.** Inside the context, only its own title/description
//     and its static text count. Sibling buttons are deliberately not read: an
//     alert offering "Delete" and "Cancel" would otherwise make "Cancel"
//     destructive, and refusing the way *out* of a destructive dialog is not
//     caution, it is the gate causing the harm it exists to prevent.
//  4. **Content stays excluded, at every depth.** The reason a text field's own
//     value is never classified — `set_value` on a note saying "delete the old
//     files" must not be refused — applies unchanged to document bodies, message
//     lists and table rows that happen to be inside a dialog. The walk does not
//     descend into them.

/// Roles that exist to ask a question.
const DECISION_ROLES: &[&str] = &["AXSheet", "AXDialog", "AXAlert"];

/// Subroles that mark an otherwise ordinary window as a question.
///
/// This is how AppKit ships an `NSAlert`: role `AXWindow`, subrole `AXDialog`
/// (or `AXSystemDialog` for the ones the system raises). Matching the subrole is
/// what keeps the rule from having to guess which windows are modal.
const DECISION_SUBROLES: &[&str] = &["AXDialog", "AXSystemDialog", "AXAlertDialog"];

/// Where the upward search gives up.
///
/// Reaching an ordinary window means the target is in content, not in a
/// question, and nothing above a window can be one either.
const CONTEXT_BOUNDARY_ROLES: &[&str] = &["AXWindow", "AXApplication", "AXSystemWide"];

/// Roles whose text is the user's own material rather than the dialog's prose.
///
/// The walk neither reads these nor descends into them, however shallow they
/// are. A "Move to…" sheet listing a folder called "delete me" is still a list
/// of the user's files.
const CONTENT_CONTAINER_ROLES: &[&str] = &[
    "AXScrollArea",
    "AXTable",
    "AXOutline",
    "AXList",
    "AXBrowser",
    "AXGrid",
    "AXRow",
    "AXCell",
    "AXWebArea",
    "AXDocument",
    "AXTextArea",
];

/// Roles that are *answers* to the question, not part of it.
///
/// Not read and not descended into, so one destructive button in an alert does
/// not contaminate every other button beside it.
const ANSWER_ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXLink",
    "AXDisclosureTriangle",
    "AXTab",
    "AXSlider",
    "AXIncrementor",
    "AXStepper",
    "AXTextField",
    "AXComboBox",
    "AXSearchField",
];

/// Roles whose text is read as part of the question.
const QUESTION_ROLES: &[&str] = &["AXStaticText", "AXHeading"];

/// How many pieces of static text one question may be assembled from.
///
/// Not a tuning knob — the shape rules above are what decide *what* is read, and
/// this only bounds the cost of reading a pathological dialog with a thousand
/// labels in it. A real alert uses two: a message and an informative text.
const MAX_QUESTION_PARTS: usize = 24;

/// Exact labels that mean "no".
///
/// Matched against the whole normalized label, never as a substring, so
/// "Close Account" is not excused by "Close". The exactness is the safety
/// property: this list is the one place the gate deliberately *stops* refusing,
/// so it has to be impossible for a destructive control to fall into it by
/// containing a soft word.
///
/// Membership is judged on the answer alone. A dismissing answer in a
/// destructive dialog is the safe outcome, and in a harmless one it is a no-op;
/// there is no dialog in which pressing Cancel is the thing that needed
/// confirming.
const DISMISSING_ANSWERS: &[&str] = &[
    "cancel",
    "no",
    "not now",
    "later",
    "dismiss",
    "keep",
    "back",
    "go back",
    "취소",
    "아니오",
    "아니요",
    "나중에",
    "유지",
    "돌아가기",
];

/// The question the nearest enclosing decision context is asking.
///
/// Assembled once at the gate and carried on the [`Candidate`], so a refusal can
/// quote its own evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionContext {
    /// How the context renders — `AXSheet`, `AXWindow[AXDialog]` — for the
    /// error message.
    pub kind: String,
    /// The context's own title and description plus its static text, in tree
    /// order.
    pub question: String,
}

/// One snapshot node, as the context rule sees it.
///
/// Borrowed rather than cloned: the caller in `session` already holds the
/// snapshot, and copying a 1500-node tree per action to classify one button
/// would be paying for the whole window to judge one control. The fields are
/// exactly what the rule reads, which also documents what it cannot read.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextNode<'a> {
    pub parent: Option<usize>,
    pub role: &'a str,
    pub subrole: Option<&'a str>,
    pub label: Option<&'a str>,
    pub value: Option<&'a str>,
    pub help: Option<&'a str>,
    pub settable: bool,
}

fn any_eq_ignore_case(list: &[&str], want: &str) -> bool {
    list.iter().any(|entry| entry.eq_ignore_ascii_case(want))
}

/// Whether this node exists to ask a question rather than to lay one out.
fn is_decision_context(node: &ContextNode<'_>) -> bool {
    any_eq_ignore_case(DECISION_ROLES, node.role)
        || node
            .subrole
            .is_some_and(|s| any_eq_ignore_case(DECISION_SUBROLES, s))
}

/// The nearest enclosing decision context of `target`, if it has one.
///
/// Walks parents only, stops at the first decision context, and gives up at the
/// first ordinary window. The step count is capped at the node count so a
/// malformed parent cycle cannot hang an action.
fn nearest_decision_context(nodes: &[ContextNode<'_>], target: usize) -> Option<usize> {
    let mut current = nodes.get(target)?.parent;
    for _ in 0..nodes.len() {
        let index = current?;
        let node = nodes.get(index)?;
        if is_decision_context(node) {
            return Some(index);
        }
        if any_eq_ignore_case(CONTEXT_BOUNDARY_ROLES, node.role) {
            return None;
        }
        current = node.parent;
    }
    None
}

/// The text `node` contributes to a question, under the same value rule the
/// target itself is judged by: never a writable value, because that is the
/// user's typing and not the dialog's prose.
fn question_text_of(node: &ContextNode<'_>) -> Vec<String> {
    let mut parts = Vec::new();
    for part in [node.label, node.help] {
        if let Some(p) = part.map(str::trim).filter(|p| !p.is_empty()) {
            parts.push(p.to_string());
        }
    }
    if !node.settable {
        if let Some(v) = node.value.map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(v.to_string());
        }
    }
    parts
}

/// The prose inside `root`, pruned at answers, at content and at any nested
/// decision context.
///
/// A depth-first walk, so the text collected is bounded by the subtree's own
/// prose no matter how large the window behind it is. `include_root` is what
/// distinguishes the two callers: a decision context contributes its own
/// title, while a control's own attributes are already classified elsewhere and
/// only its caption children are wanted here.
fn prose_in(nodes: &[ContextNode<'_>], root: usize, include_root: bool) -> String {
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent {
            if parent < nodes.len() && parent != index {
                children[parent].push(index);
            }
        }
    }

    let mut parts = if include_root {
        question_text_of(&nodes[root])
    } else {
        Vec::new()
    };
    let mut stack: Vec<usize> = children[root].iter().rev().copied().collect();
    while let Some(index) = stack.pop() {
        if parts.len() >= MAX_QUESTION_PARTS {
            break;
        }
        let node = &nodes[index];
        if is_decision_context(node)
            || any_eq_ignore_case(CONTENT_CONTAINER_ROLES, node.role)
            || any_eq_ignore_case(ANSWER_ROLES, node.role)
        {
            continue;
        }
        if any_eq_ignore_case(QUESTION_ROLES, node.role) {
            parts.extend(question_text_of(node));
            // Static text has no children worth walking, and an accessible
            // label rendered as a child of one would be the same string twice.
            continue;
        }
        stack.extend(children[index].iter().rev().copied());
    }

    parts.join(" ")
}

/// Roles whose caption may be a child element instead of an attribute.
///
/// Chromium and the toolkits built on it publish a button as a container with
/// its caption as an `AXStaticText` inside it, so a control that says "Delete"
/// on screen arrives here with no label at all. Reading one level of prose out
/// of *the control itself* is not a widening of scope — it is the same target
/// the classifier always judged, reassembled from where this toolkit put it.
///
/// Restricted to button-shaped roles on purpose. `AXRow` and `AXCell` also hold
/// their text in children, and that text is the user's mail, chat and files.
const CAPTIONED_ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXLink",
    "AXTab",
    "AXDisclosureTriangle",
    "AXToolbarButton",
];

/// What the control at `target` says, when it says it in a child.
///
/// `None` for anything that is not button-shaped, and for a button that carries
/// its caption in an attribute like every native control does.
pub fn caption(nodes: &[ContextNode<'_>], target: usize) -> Option<String> {
    let node = nodes.get(target)?;
    if !any_eq_ignore_case(CAPTIONED_ROLES, node.role) {
        return None;
    }
    let text = prose_in(nodes, target, false);
    Some(text).filter(|t| !t.trim().is_empty())
}

/// The question the element at `target` is being asked to answer, if any.
///
/// Public because this is the interesting half of the classifier and deserves to
/// be testable from a plain array of nodes, with no app, no snapshot and no
/// accessibility permission.
pub fn decision_context(nodes: &[ContextNode<'_>], target: usize) -> Option<DecisionContext> {
    let index = nearest_decision_context(nodes, target)?;
    let question = prose_in(nodes, index, true);
    if question.trim().is_empty() {
        return None;
    }
    let node = &nodes[index];
    let kind = match node.subrole {
        Some(subrole) if !subrole.is_empty() => format!("{}[{subrole}]", node.role),
        _ => node.role.to_string(),
    };
    Some(DecisionContext { kind, question })
}

/// Whether a label is an exact "no".
///
/// Trailing punctuation and the ellipsis AppKit likes are stripped before the
/// comparison, so "Cancel…" is still a cancel; nothing else about the string is
/// allowed to vary.
fn is_dismissing_answer(label: &str) -> bool {
    let norm = normalize(label);
    let trimmed = norm.trim_matches(|c: char| !c.is_alphanumeric());
    !trimmed.is_empty() && DISMISSING_ANSWERS.contains(&trimmed)
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

/// The question held against this candidate, and the token that convicted it.
///
/// The whole context half of the gate in one call, so a test can exercise the
/// decision — not just its pieces — without an app, a window server or a
/// permission. `None` means the context is no evidence: there is no enclosing
/// question, the question is harmless, or the candidate is exempt from being
/// judged by it (see [`Candidate::accepts_context_evidence`]).
pub fn destructive_context(candidate: &Candidate) -> Option<(&DecisionContext, String)> {
    if !candidate.accepts_context_evidence() {
        return None;
    }
    let context = candidate.context.as_ref()?;
    let matched = destructive_token(&context.question)?;
    Some((context, matched))
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
}

impl Gate {
    /// A gate for an action aimed at an element.
    pub fn at(verb: &'static str, target: &Target) -> Self {
        Self {
            verb,
            target: Some(target.clone()),
            confirm_destructive: false,
            key: None,
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

    // ── the question a dialog is asking ──────────────────────────────────
    //
    // These build snapshot-shaped trees rather than asserting on strings,
    // because the rule being tested is about *shape*: which ancestor, how far
    // up, which text inside it. A string-level test would pass with the
    // ancestor rule deleted.

    /// A flat tree in the form the snapshot records one: parents before
    /// children, every node naming its parent by index.
    #[derive(Default)]
    struct Tree {
        nodes: Vec<ContextNode<'static>>,
    }

    impl Tree {
        fn push(&mut self, node: ContextNode<'static>) -> usize {
            self.nodes.push(node);
            self.nodes.len() - 1
        }

        /// An ordinary document window. Layout, never a question.
        fn window(&mut self, title: &'static str) -> usize {
            self.push(ContextNode {
                parent: None,
                role: "AXWindow",
                subrole: Some("AXStandardWindow"),
                label: Some(title),
                ..ContextNode::default()
            })
        }

        /// What AppKit publishes for a free-standing `NSAlert`: an ordinary
        /// window role carrying the dialog subrole.
        fn dialog_window(&mut self, title: &'static str) -> usize {
            self.push(ContextNode {
                parent: None,
                role: "AXWindow",
                subrole: Some("AXDialog"),
                label: Some(title),
                ..ContextNode::default()
            })
        }

        /// The document-modal form: a sheet attached to a window.
        fn sheet(&mut self, parent: usize, title: Option<&'static str>) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXSheet",
                label: title,
                ..ContextNode::default()
            })
        }

        fn group(&mut self, parent: usize) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXGroup",
                ..ContextNode::default()
            })
        }

        fn container(&mut self, parent: usize, role: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role,
                ..ContextNode::default()
            })
        }

        fn text(&mut self, parent: usize, body: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXStaticText",
                label: Some(body),
                ..ContextNode::default()
            })
        }

        fn button(&mut self, parent: usize, label: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXButton",
                label: Some(label),
                ..ContextNode::default()
            })
        }

        fn field(&mut self, parent: usize, value: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXTextField",
                label: Some("Name"),
                value: Some(value),
                settable: true,
                ..ContextNode::default()
            })
        }

        /// The candidate `session::safety_candidate` would hand the gate for
        /// this node — same fields, same context lookup.
        fn candidate(&self, index: usize) -> Candidate {
            let n = self.nodes[index];
            Candidate {
                role: n.role.to_string(),
                label: n.label.map(str::to_string),
                value: n.value.map(str::to_string),
                help: n.help.map(str::to_string),
                settable: n.settable,
                caption: caption(&self.nodes, index),
                description: format!("[{index}] {} {:?}", n.role, n.label.unwrap_or_default()),
                context: decision_context(&self.nodes, index),
            }
        }

        /// What `guard` would conclude about this node, in the same order:
        /// the control's own words first, then the question it answers.
        fn verdict(&self, index: usize) -> Option<String> {
            let c = self.candidate(index);
            destructive_token(&c.classifiable_text())
                .or_else(|| destructive_context(&c).map(|(_, matched)| matched))
        }
    }

    /// The commonest destructive shape on macOS: a terse button under a
    /// sheet whose text carries the whole meaning.
    fn confirm_sheet(
        message: &'static str,
        informative: &'static str,
        answers: &[&'static str],
    ) -> (Tree, Vec<usize>) {
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        let body = t.group(sheet);
        t.text(body, message);
        t.text(body, informative);
        let answers = answers.iter().map(|a| t.button(sheet, a)).collect();
        (t, answers)
    }

    #[test]
    fn a_terse_button_inherits_the_question_its_sheet_is_asking() {
        let (t, answers) = confirm_sheet(
            "Delete 4 items?",
            "This action cannot be undone.",
            &["OK", "Cancel"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("delet"));
    }

    #[test]
    fn the_korean_form_of_the_same_dialog_is_caught_too() {
        // 확인 says nothing on its own, exactly like OK, and the maintainer's
        // apps are Korean.
        let (t, answers) = confirm_sheet(
            "4개 항목을 삭제할까요?",
            "이 동작은 되돌릴 수 없습니다.",
            &["확인", "취소"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("삭제"));
    }

    #[test]
    fn cancelling_a_destructive_dialog_is_never_refused() {
        // The load-bearing exemption. Cancel is how a caller *avoids* the
        // destruction on offer; refusing it would leave an agent stuck in a
        // modal sheet whose only exit is to send confirm_destructive: true,
        // which is the habit that would make this gate meaningless everywhere
        // else.
        let (t, answers) = confirm_sheet(
            "Delete 4 items?",
            "This action cannot be undone.",
            &["OK", "Cancel"],
        );
        assert_eq!(t.verdict(answers[1]), None);

        let (t, answers) = confirm_sheet(
            "4개 항목을 삭제할까요?",
            "이 동작은 되돌릴 수 없습니다.",
            &["확인", "취소"],
        );
        assert_eq!(t.verdict(answers[1]), None);
    }

    #[test]
    fn a_dismissing_answer_is_matched_whole_and_never_as_a_substring() {
        for yes in [
            "Cancel",
            "cancel",
            "Cancel…",
            "(Cancel)",
            "No",
            "Not now",
            "취소",
            "유지",
        ] {
            assert!(is_dismissing_answer(yes), "{yes:?} is a way out");
        }
        // The direction that matters: a destructive control must not be
        // excused by containing a soft word.
        for no in [
            "Close Account",
            "No Backup, Delete",
            "Cancel Subscription",
            "Keep Nothing",
            "취소선 삭제",
            "",
        ] {
            assert!(!is_dismissing_answer(no), "{no:?} is not a way out");
        }
    }

    #[test]
    fn a_sheet_that_erases_a_disk_catches_continue_as_well_as_ok() {
        let (t, answers) = confirm_sheet(
            "Are you sure?",
            "Erasing will permanently remove all data on “Backup”.",
            &["Continue", "Cancel"],
        );
        assert!(t.verdict(answers[0]).is_some());
        assert_eq!(t.verdict(answers[1]), None);
    }

    #[test]
    fn an_alert_window_is_a_question_even_though_its_role_says_window() {
        // AppKit's free-standing NSAlert: role AXWindow, subrole AXDialog. If
        // the rule keyed on role alone this shape would sail through.
        let mut t = Tree::default();
        let alert = t.dialog_window("");
        t.text(alert, "Delete “Report.pdf”?");
        let ok = t.button(alert, "OK");
        assert!(t.verdict(ok).is_some());
        assert_eq!(
            t.candidate(ok).context.unwrap().kind,
            "AXWindow[AXDialog]".to_string()
        );
    }

    #[test]
    fn an_ordinary_window_full_of_the_word_delete_is_not_a_question() {
        // The failure mode that made this feature hard: a mail window whose
        // content is a thread about deleting an account, a chat window whose
        // history says 삭제 twenty times. None of it is a decision context, so
        // none of it is evidence, at any depth.
        let mut t = Tree::default();
        let window = t.window("Re: please delete my account");
        let scroll = t.container(window, "AXScrollArea");
        let group = t.group(scroll);
        t.text(group, "Can you delete the old backups and erase the disk?");
        let reply = t.button(group, "Reply");
        let send = t.button(window, "Send");

        assert_eq!(t.verdict(reply), None);
        assert_eq!(t.verdict(send), None);
        assert!(t.candidate(reply).context.is_none());
    }

    #[test]
    fn a_window_title_alone_is_not_a_question() {
        // Deliberate: an ordinary window is layout even when its title reads
        // like an alert. The dialog subrole is what distinguishes them, and a
        // toolkit that publishes neither gets the benefit of the doubt rather
        // than making every button in the app confirmable.
        let mut t = Tree::default();
        let window = t.window("Delete 4 items?");
        let ok = t.button(window, "OK");
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_answers_are_not_part_of_the_question() {
        // An alert offering Delete and Cancel must not make Cancel — or any
        // other sibling — destructive by association. Only the prose counts.
        let (t, answers) = confirm_sheet(
            "Are you sure?",
            "You can change this later.",
            &["Delete", "Cancel", "More Info"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("delet")); // its own label
        assert_eq!(t.verdict(answers[1]), None);
        assert_eq!(t.verdict(answers[2]), None);

        // The same alert as a toolkit that renders each button as a container
        // with its caption inside it — Chromium and the Electron apps built on
        // it publish exactly this. If the walk descended into answers, the
        // Delete button's caption would become part of the question and Cancel
        // would be refused for standing next to it.
        let mut t = Tree::default();
        let alert = t.dialog_window("");
        t.text(alert, "Are you sure?");
        let delete = t.button(alert, "");
        t.text(delete, "Delete");
        let cancel = t.button(alert, "");
        t.text(cancel, "Cancel");

        assert_eq!(
            t.candidate(cancel).context.unwrap().question,
            "Are you sure?".to_string()
        );
        assert_eq!(t.verdict(cancel), None);
        // …and the button beside it is still caught, by its own caption rather
        // than by the question. A control that says "Delete" on screen must not
        // read as unlabeled just because the toolkit put the word in a child.
        assert_eq!(t.verdict(delete).as_deref(), Some("delet"));
    }

    #[test]
    fn a_caption_is_read_from_a_control_and_never_from_a_row() {
        // The boundary that keeps the caption rule from becoming a content
        // reader: a button's children are its wording, a row's children are the
        // user's mail.
        let mut t = Tree::default();
        let window = t.window("Mail");
        let table = t.container(window, "AXTable");
        let row = t.container(table, "AXRow");
        t.text(row, "Please delete my account");
        assert_eq!(caption(&t.nodes, row), None);
        assert_eq!(t.verdict(row), None);

        let button = t.button(window, "");
        t.text(button, "Empty Trash");
        assert_eq!(caption(&t.nodes, button).as_deref(), Some("Empty Trash"));
        assert!(t.verdict(button).is_some());
    }

    #[test]
    fn content_inside_a_dialog_is_still_content() {
        // A "Move to…" sheet listing the user's own files. The sheet is a
        // decision context, but a table of file names is not its question, and
        // a folder called "delete me" must not confirm-gate the Move button.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, Some("Move 3 items to:"));
        let table = t.container(sheet, "AXTable");
        let row = t.container(table, "AXRow");
        t.text(row, "delete me");
        let move_button = t.button(sheet, "Move");

        assert_eq!(t.verdict(move_button), None);
        assert_eq!(
            t.candidate(move_button).context.unwrap().question,
            "Move 3 items to:".to_string()
        );
    }

    #[test]
    fn a_text_field_inside_a_destructive_dialog_is_still_writable() {
        // Two exclusions at once, and both have to hold. The field's own value
        // is never classified, and the question around it is not held against
        // typing either: the decision is the button underneath, not the name
        // being typed into the sheet.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        t.text(sheet, "Deleting this project will remove 42 files.");
        let name = t.field(sheet, "delete the old files first");
        assert_eq!(t.verdict(name), None);
        assert!(
            t.candidate(name).context.is_some(),
            "the sheet is still a question — the field is just exempt from being asked it"
        );
    }

    #[test]
    fn a_writable_label_inside_a_dialog_never_becomes_the_question() {
        // The same rule one level out: the walk reads static text, but a
        // settable value inside the sheet is the user's own typing.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, Some("Rename item"));
        t.push(ContextNode {
            parent: Some(sheet),
            role: "AXStaticText",
            value: Some("delete me"),
            settable: true,
            ..ContextNode::default()
        });
        let ok = t.button(sheet, "OK");
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_nearest_question_is_the_one_being_answered() {
        // A confirmation raised on top of a destructive dialog. The inner
        // sheet is what the button answers; inheriting the outer one would
        // make "OK" on "Rename this file?" a deletion.
        let mut t = Tree::default();
        let outer = t.dialog_window("");
        t.text(outer, "Erase “Macintosh HD”?");
        let inner = t.sheet(outer, None);
        t.text(inner, "Rename this file?");
        let ok = t.button(inner, "OK");
        assert_eq!(t.verdict(ok), None);

        // …and the outer dialog's own buttons still see their own question.
        let outer_ok = t.button(outer, "OK");
        assert!(t.verdict(outer_ok).is_some());
    }

    #[test]
    fn a_nested_question_is_read_when_it_is_the_destructive_one() {
        // The other direction of the same boundary: a harmless outer dialog
        // must not shield a destructive inner sheet.
        let mut t = Tree::default();
        let outer = t.dialog_window("Export");
        t.text(outer, "Choose a format.");
        let inner = t.sheet(outer, None);
        t.text(inner, "Overwrite the existing file?");
        let ok = t.button(inner, "OK");
        assert_eq!(t.verdict(ok).as_deref(), Some("overwrite"));
        assert_eq!(
            t.candidate(ok).context.unwrap().question,
            "Overwrite the existing file?".to_string()
        );
    }

    #[test]
    fn a_nested_question_does_not_leak_upward_either() {
        // The outer dialog's own buttons must not inherit the inner sheet's
        // text: the walk goes up from the target, never down into a sibling
        // question.
        let mut t = Tree::default();
        let outer = t.dialog_window("Export");
        t.text(outer, "Choose a format.");
        let inner = t.sheet(outer, None);
        t.text(inner, "Delete the original?");
        let outer_ok = t.button(outer, "OK");
        assert_eq!(t.verdict(outer_ok), None);
    }

    #[test]
    fn the_question_survives_the_layout_between_it_and_the_button() {
        // Real alerts bury their text a few groups down, and a cross-platform
        // toolkit buries it further. Depth inside the context is not the rule;
        // kind of ancestor is.
        let mut t = Tree::default();
        let window = t.window("Project");
        let sheet = t.sheet(window, None);
        let mut parent = sheet;
        for _ in 0..6 {
            parent = t.group(parent);
        }
        t.text(parent, "This will permanently erase 12 recordings.");
        let ok = t.button(sheet, "Continue");
        assert_eq!(t.verdict(ok).as_deref(), Some("eras"));
    }

    #[test]
    fn an_unlabeled_control_in_a_destructive_dialog_fails_closed() {
        // No label means no dismissal exemption. An icon button in a delete
        // sheet is judged by the sheet, which is the fail-closed reading.
        let mut t = Tree::default();
        let window = t.window("Photos");
        let sheet = t.sheet(window, None);
        t.text(sheet, "Delete 12 photos?");
        let icon = t.push(ContextNode {
            parent: Some(sheet),
            role: "AXButton",
            ..ContextNode::default()
        });
        assert!(t.verdict(icon).is_some());
    }

    #[test]
    fn a_silent_dialog_produces_no_evidence() {
        // A sheet with no readable prose is not evidence of anything. It must
        // not refuse by virtue of being a sheet.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        let ok = t.button(sheet, "OK");
        assert!(t.candidate(ok).context.is_none());
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_search_terminates_on_a_malformed_tree() {
        // Parent indices come from a walk of a live app. A cycle should cost a
        // bounded loop, not the server.
        let nodes = vec![
            ContextNode {
                parent: Some(1),
                role: "AXGroup",
                ..ContextNode::default()
            },
            ContextNode {
                parent: Some(0),
                role: "AXGroup",
                ..ContextNode::default()
            },
        ];
        assert!(decision_context(&nodes, 0).is_none());
        assert!(decision_context(&[], 0).is_none());
        // A parent pointing past the end of the tree is not a panic either.
        let dangling = vec![ContextNode {
            parent: Some(99),
            role: "AXButton",
            ..ContextNode::default()
        }];
        assert!(decision_context(&dangling, 0).is_none());
    }

    #[test]
    fn one_question_cannot_cost_an_unbounded_read() {
        // The shape rules decide what is read; this only bounds how much of a
        // pathological dialog is read at all.
        let mut t = Tree::default();
        let window = t.window("Stress");
        let sheet = t.sheet(window, None);
        for _ in 0..200 {
            t.text(sheet, "lorem ipsum");
        }
        let ok = t.button(sheet, "OK");
        let question = t.candidate(ok).context.unwrap().question;
        assert_eq!(
            question.split("lorem ipsum").count() - 1,
            MAX_QUESTION_PARTS
        );
    }

    #[test]
    fn a_context_refusal_quotes_the_question_and_the_way_out() {
        let text = Refused::NeedsConfirmationInContext {
            verb: "click",
            target: "[7] AXButton \"OK\"".to_string(),
            context: "AXSheet".to_string(),
            question: "Delete 4 items?".to_string(),
            matched: "delet".to_string(),
        }
        .to_string();
        assert!(text.contains("Delete 4 items?"));
        assert!(text.contains("AXSheet"));
        assert!(text.contains("confirm_destructive: true"));
        // The distinction the separate variant exists for: the button is not
        // being accused of saying anything destructive.
        assert!(!text.contains("reads as a destructive control"));
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
