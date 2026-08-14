//! Talks to the separate `cua-overlay` process that draws where the agent is
//! acting, so a human watching the screen can tell at a glance what a tool
//! call just touched — see `crates/cua-overlay` for why it is a whole
//! process rather than a function call.
//!
//! Everything here is best-effort. The overlay is a debugging aid, not part
//! of the contract: a missing sibling binary, a crashed child, a stdin write
//! that fails — none of it should turn into an error an action tool returns.
//! An agent that cannot draw a cursor should still be able to click.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub(crate) struct Overlay {
    child: Mutex<Option<Child>>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        Self {
            child: Mutex::new(None),
        }
    }

    /// Move the drawn arrow to `(x, y)` in screen points (the same space
    /// `get_app_state` reports element frames in), flashing a click ring
    /// when `clicking` is set. Spawns the overlay process on first call and
    /// respawns it if it has died. Never blocks on anything but a pipe write.
    pub(crate) fn mark(&self, x: f64, y: f64, clicking: bool) {
        let verb = if clicking { "click" } else { "move" };
        self.send(&format!("{verb} {x} {y}\n"));
    }

    fn send(&self, line: &str) {
        let Ok(mut guard) = self.child.lock() else {
            return;
        };
        if guard.is_none() {
            *guard = Self::spawn();
        }
        let Some(child) = guard.as_mut() else {
            return;
        };
        let Some(stdin) = child.stdin.as_mut() else {
            return;
        };
        if stdin.write_all(line.as_bytes()).is_err() {
            // The process is gone; drop the handle so the next mark respawns
            // it instead of writing into a dead pipe forever.
            *guard = None;
        }
    }

    /// The overlay binary ships next to this one — same `cargo build`, same
    /// install step — so the running executable's own directory is the one
    /// place that is guaranteed to be right regardless of PATH or install
    /// layout. `current_exe` can return a symlink (e.g. `~/.local/bin/cua-rs`
    /// pointing into `target/release`); canonicalize so the sibling lookup
    /// follows it to where `cua-overlay` actually was built.
    fn spawn() -> Option<Child> {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        let sibling = exe.parent()?.join("cua-overlay");
        Command::new(sibling)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
}

// No `Drop` impl: `cua-overlay` already exits on stdin EOF (see
// `crates/cua-overlay/src/main.rs`), which is exactly what closing `Child`'s
// stdin produces when this value is dropped. A bespoke quit message plus a
// blocking `wait()` would just be two ways to say the same thing, with the
// added risk of hanging server shutdown on a wedged child.
