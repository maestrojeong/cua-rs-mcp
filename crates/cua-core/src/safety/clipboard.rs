//! Clipboard policy: when a read keeps its text back, and which gates a write
//! passes.
//!
//! The mechanism is `crate::clipboard`; nothing here touches AppKit, which is
//! what lets every rule below be pinned by a test that never needs a pasteboard.
//!
//! # A clipboard read is a screenshot, not a tree read
//!
//! The split in DESIGN.md §7a is by what a read reproduces. A tree read
//! describes the UI around a secret — a secure text field publishes no value to
//! accessibility at all. A screenshot reproduces the secret, so it is withheld
//! for forbidden apps. A clipboard read is the screenshot case and then some: it
//! is the secret itself, as text, with nothing to blur. So text is withheld on
//! the evidence that it may be one, and the type list — which names formats,
//! never contents — is still returned so the refusal is diagnosable.
//!
//! # Writes are session-wide, not app-wide
//!
//! The clipboard belongs to no app, and a write delivers nothing to one: the
//! text reaches an app only through a paste, which is itself an action and is
//! gated as one when cua-rs does it. So the app-shaped gates — scope, forbidden
//! floor, destructive label — have nothing to judge. The session-shaped ones
//! do: a locked session is still nobody watching, and the yield gate fires on
//! *any* recent human input, because the clipboard is the one piece of state
//! the human and cua-rs genuinely share and the human may be halfway through a
//! copy and paste of their own.

use super::*;

/// Why `clipboard_get` must not return the text, or `None` when it may.
///
/// Checked in the order a reader wants reported: the writer's own word first,
/// then where the contents came from, then what the human is looking at.
///
/// - `types` is the current type list.
/// - `source` is what the writer declared under `org.nspasteboard.source`, if
///   it did. The caller reads it only when it is present, and only after this
///   function has already passed on `source: None`.
/// - `frontmost` is the app the human is in right now.
/// - `allow_forbidden` is `CUA_ALLOW_FORBIDDEN_TARGETS`, passed in so tests do
///   not depend on the process environment.
pub(crate) fn clipboard_read_refusal(
    types: &[String],
    source: Option<&str>,
    frontmost: Option<&AppInfo>,
    allow_forbidden: bool,
) -> Option<String> {
    // Exact match on the whole UTI, as AppKit reports it. Deliberately not
    // lifted by CUA_ALLOW_FORBIDDEN_TARGETS: that flag overrides cua-rs's own
    // classification of *apps*, which can be wrong. This marker is not a
    // classification — it is the writer saying what the data is — so there is
    // nothing of cua-rs's to override.
    if types.iter().any(|t| t == crate::clipboard::CONCEALED_TYPE) {
        return Some(format!(
            "clipboard text withheld: the app that wrote it marked it as a secret \
             ({}), the nspasteboard.org convention password managers use so that \
             clipboard tools do not display or record a copied password. cua-rs \
             honors that marker unconditionally. Paste it where it is needed with \
             press_key cmd+v instead — the text then goes from the clipboard to the \
             app without passing through this transcript",
            crate::clipboard::CONCEALED_TYPE
        ));
    }
    if allow_forbidden {
        return None;
    }
    if let Some(src) = source {
        if let Some(why) = forbidden_bundle(src) {
            return Some(format!(
                "clipboard text withheld: the contents were written by `{src}`, which \
                 cua-rs treats as a forbidden target because it {why}, so they may be a \
                 credential. Paste with press_key cmd+v to move the text without reading \
                 it. CUA_ALLOW_FORBIDDEN_TARGETS=1 lifts this"
            ));
        }
    }
    if let Some(app) = frontmost {
        if let Some(bundle_id) = app.bundle_id.as_deref() {
            if let Some(why) = forbidden_bundle(bundle_id) {
                return Some(format!(
                    "clipboard text withheld: the frontmost app is `{}` ({bundle_id}), \
                     which {why}, and what is on the clipboard right now may be a secret \
                     the user just copied out of it. Retry once a different app is \
                     frontmost. CUA_ALLOW_FORBIDDEN_TARGETS=1 lifts this",
                    app.name
                ));
            }
        }
    }
    None
}

/// The yield watch, reduced to what the session gate needs to know.
#[derive(Debug, Clone, Copy)]
pub(crate) enum YieldReading<'a> {
    Off,
    Broken(&'a str),
    Watching { since_input_ms: Option<u64> },
}

impl HumanWatch {
    pub(crate) fn reading(&self) -> YieldReading<'_> {
        match &self.watch {
            Watch::Off => YieldReading::Off,
            Watch::Broken { reason } => YieldReading::Broken(reason),
            Watch::On { .. } => YieldReading::Watching {
                since_input_ms: self.since_human_input_ms(),
            },
        }
    }
}

/// The gates a clipboard write passes: screen lock, then yield.
pub(crate) fn guard_session(verb: &'static str, watch: &HumanWatch) -> Result<(), Refused> {
    session_gate(verb, session_locked(), watch.reading(), yield_idle_ms())
}

/// [`guard_session`] with every input passed in, for tests.
pub(crate) fn session_gate(
    verb: &'static str,
    locked: bool,
    watch: YieldReading<'_>,
    idle_ms: u64,
) -> Result<(), Refused> {
    if locked {
        return Err(Refused::SessionLocked { verb });
    }
    match watch {
        YieldReading::Off => Ok(()),
        YieldReading::Broken(reason) => Err(Refused::YieldWatchUnavailable {
            verb,
            reason: reason.to_string(),
        }),
        YieldReading::Watching { since_input_ms } => match since_input_ms {
            Some(ago_ms) if ago_ms < idle_ms => Err(Refused::HumanActive {
                verb,
                ago_ms,
                idle_ms,
            }),
            _ => Ok(()),
        },
    }
}
