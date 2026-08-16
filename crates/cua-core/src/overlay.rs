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
    /// when `clicking` is set. `target` pins the overlay immediately above
    /// that target window and tells it which pid it is currently pointing
    /// at, so it can hide itself the moment a *different* app becomes
    /// frontmost rather than relying on window ordering alone; without a
    /// target the marker is hidden rather than shown globally. Spawns the
    /// overlay process on first call and respawns it if it has died. Never
    /// blocks on anything but a pipe write.
    pub(crate) fn mark(&self, x: f64, y: f64, clicking: bool, target: Option<(u32, libc::pid_t)>) {
        let verb = if clicking { "click" } else { "move" };
        match target {
            Some((window_id, pid)) => self.send(&format!("{verb} {x} {y} {window_id} {pid}\n")),
            None => self.send("hide\n"),
        }
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

    fn spawn() -> Option<Child> {
        let sibling = sibling_overlay(&std::env::current_exe().ok()?)?;
        Command::new(sibling)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
}

/// The overlay binary ships next to this one — same `cargo build`, same
/// `install.sh`, same release — so the running executable's own directory is
/// the one place that is guaranteed to be right regardless of PATH or install
/// layout. Nothing here consults PATH on purpose: a `cua-overlay` from some
/// other install is a worse answer than no overlay at all.
///
/// `exe` can be a symlink — `~/bin/cua-rs` pointing at `~/.local/bin/cua-rs`,
/// or a dev symlink into `target/release` — so canonicalize before taking the
/// parent. Without that the sibling lookup would search the *link's*
/// directory, which is exactly the directory that does not contain the
/// overlay.
fn sibling_overlay(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    Some(exe.canonicalize().ok()?.parent()?.join("cua-overlay"))
}

// No `Drop` impl: `cua-overlay` already exits on stdin EOF (see
// `crates/cua-overlay/src/main.rs`), which is exactly what closing `Child`'s
// stdin produces when this value is dropped. A bespoke quit message plus a
// blocking `wait()` would just be two ways to say the same thing, with the
// added risk of hanging server shutdown on a wedged child.

#[cfg(test)]
mod tests {
    use super::sibling_overlay;

    /// The installed layout is the one that used to be broken: `install.sh`
    /// puts both binaries in `~/.local/bin`, but a user (or Homebrew, or a
    /// dotfiles repo) may well reach `cua-rs` through a symlink from
    /// somewhere else on PATH. Resolve to the real file first, or the overlay
    /// is looked for beside the link and silently never spawns.
    #[test]
    fn the_overlay_is_looked_for_beside_the_real_binary_not_beside_a_symlink_to_it() {
        let root = std::env::temp_dir().join(format!("cua-overlay-lookup-{}", std::process::id()));
        let installed = root.join("installed");
        let on_path = root.join("on-path");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(installed.join("cua-rs"), b"#!/bin/sh\n").unwrap();
        std::fs::write(installed.join("cua-overlay"), b"#!/bin/sh\n").unwrap();

        let link = on_path.join("cua-rs");
        std::os::unix::fs::symlink(installed.join("cua-rs"), &link).unwrap();

        let found = sibling_overlay(&link).expect("a resolvable symlink has a sibling");
        assert_eq!(
            found,
            installed.canonicalize().unwrap().join("cua-overlay"),
            "lookup followed the link's own directory instead of the target's"
        );
        assert!(
            found.exists(),
            "the overlay next to the real binary is there"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A path that does not resolve yields no overlay rather than a guess:
    /// `Command::new` on a bogus path would fail anyway, but returning `None`
    /// keeps the "best effort, never an error" contract at the top of this
    /// file explicit.
    #[test]
    fn an_unresolvable_executable_path_yields_no_overlay() {
        assert!(sibling_overlay(std::path::Path::new("/nope/cua-rs")).is_none());
    }
}
