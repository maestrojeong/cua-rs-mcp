//! Locked-session and yield-to-human runtime gates.

use super::*;

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
pub(super) enum Watch {
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
    pub(super) watch: Watch,
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
    pub(super) fn since_human_input_ms(&self) -> Option<u64> {
        match &self.watch {
            Watch::On { watch } => watch.since_input_ms(),
            _ => None,
        }
    }
}
