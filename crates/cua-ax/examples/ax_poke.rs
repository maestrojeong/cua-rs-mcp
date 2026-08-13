//! Diagnose why an app's accessibility tree is empty.
//!
//! Chromium and Electron apps keep their web-content AX tree switched off until
//! an assistive client asks for it, and the ask is a single attribute write. When
//! that write does not take, the app looks like one empty `AXWindow` and there is
//! no way to tell from the outside whether the poke failed, was refused, or
//! simply needs longer.
//!
//! This prints the answer for each possibility:
//!
//! ```sh
//! cargo run -p cua-ax --example ax_poke -- Slack
//! ```
//!
//! Requires the Accessibility grant on the launching process.

use std::time::{Duration, Instant};

use cua_ax::{attr, Element, Limits};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: ax_poke <pid|app-name-substring>");
        std::process::exit(2);
    });

    if !cua_ax::is_trusted() {
        eprintln!("Accessibility permission is not granted for the launching process.");
        std::process::exit(1);
    }

    let pid = match query.parse::<libc::pid_t>() {
        Ok(pid) => pid,
        Err(_) => match find_pid(&query) {
            Some(pid) => pid,
            None => {
                eprintln!("no running process matching {query:?}; pass a pid instead");
                std::process::exit(1);
            }
        },
    };

    let app = Element::for_pid(pid);
    println!("pid {pid}");

    // Is the attribute even offered? An app that does not implement it reports
    // "not settable", which is a different problem from a write that is accepted
    // and then ignored.
    for name in [attr::MANUAL_ACCESSIBILITY, attr::ENHANCED_USER_INTERFACE] {
        let settable = app.is_settable(name);
        let before = app.bool(name);
        let write = app.set_bool(name, true);
        let after = app.bool(name);
        println!(
            "{name:26} settable={settable:<5} before={before:?} write={} after={after:?}",
            match &write {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("{e}"),
            }
        );
    }

    // Then watch the tree grow, if it grows at all. A single measurement cannot
    // distinguish "refused" from "still building".
    println!("\nelement count over time (the tree builds asynchronously):");
    let limits = Limits {
        max_nodes: 4000,
        ..Limits::default()
    };
    let started = Instant::now();
    for wait_ms in [0u64, 200, 400, 800, 1600, 3200] {
        std::thread::sleep(Duration::from_millis(if wait_ms == 0 { 0 } else { 200 }));
        while started.elapsed() < Duration::from_millis(wait_ms) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let Some(window) = app
            .element(attr::FOCUSED_WINDOW)
            .or_else(|| app.element(attr::MAIN_WINDOW))
            .or_else(|| app.elements(attr::WINDOWS).into_iter().next())
        else {
            println!("  +{wait_ms:>5}ms  no window");
            continue;
        };
        let nodes = window.snapshot_tree(limits);
        let actionable = nodes.iter().filter(|n| n.is_actionable()).count();
        let labeled = nodes
            .iter()
            .filter(|n| n.label.is_some() || n.value.is_some())
            .count();
        println!(
            "  +{wait_ms:>5}ms  {} elements, {actionable} actionable, {labeled} with text",
            nodes.len()
        );
    }

    println!(
        "\nIf the count never rises above a handful and nothing carries text, this app does not\n\
         honor either attribute from an unsigned client. Drive its web content over CDP instead."
    );
}

/// Match a running process by name, so the example is usable without looking up
/// a pid first. Deliberately crude: this is a diagnostic, not a product surface.
fn find_pid(needle: &str) -> Option<libc::pid_t> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-Ao", "pid=,comm="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = needle.to_lowercase();
    text.lines()
        .filter(|l| l.to_lowercase().contains(&needle))
        // Prefer the shortest command path: the main app binary rather than one
        // of its helpers, which have longer nested paths.
        .min_by_key(|l| l.len())
        .and_then(|l| l.split_whitespace().next()?.parse().ok())
}
