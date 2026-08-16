//! Does an `AXMenuItem` act on the first `AXPress`?
//!
//! §10 of DESIGN.md once carried the claim that it does not — that the first
//! press merely selects the item and a second one is needed to make it act,
//! observed once and never characterized. This is the characterization, and the
//! answer is that it does: 180 presses across six arms and three items, all of
//! them acting on the first press. What the original observation saw was the
//! *read*, which can trail the press by over 1.7 s. §10 has the numbers.
//!
//! The target has to be a menu that publishes accessibility at all. A menu
//! opened by a *click* is a `CGWindow` with no AX representation (§10), so it is
//! out of scope here; a menu-bar menu is the case that does publish
//! `AXMenuBar` → `AXMenuBarItem` → `AXMenu` → `AXMenuItem`, and it is where the
//! original observation came from.
//!
//! The read-back is the pressed element's own title and `AXMenuItemMarkChar`,
//! which covers both ways an app writes a toggle: the title flips ("Show X" ↔
//! "Hide X"), or the title is fixed and a checkmark appears. Nothing is inferred
//! from a window diff, and nothing is read off a second element. Rather than
//! settling for a fixed delay and calling an unchanged read a failure — the
//! mistake that produced the claim in the first place — each press is polled for
//! up to three seconds and the time it took to become readable is reported.
//!
//! Point it at something harmless and reversible: a View-menu toggle, never a
//! File operation. Every trial that acts is undone before the next one, and the
//! run hands the app back in the state it found it.
//!
//! ```sh
//! cargo run -p cua-ax --example menu_item_press -- Calculator View "RPN"
//! cargo run -p cua-ax --example menu_item_press -- TextEdit View "Dark" 20
//! ```
//!
//! Arguments: app name or pid, the menu-bar menu's title, a substring of the
//! item's title that survives the toggle, and optionally a trial count. The
//! titles are whatever the app is localized to, so on a non-English system pass
//! the localized ones — the enumeration this prints first is there to be read
//! before choosing.
//!
//! Requires the Accessibility grant on the launching process.

use std::thread::sleep;
use std::time::Duration;

use cua_ax::{attr, Element};

/// How long to wait after the *second* press of the two-press arm.
///
/// Only that arm needs a fixed delay: everywhere else the wait is
/// [`poll_until_change`], which measures rather than assumes. Overridable with
/// `CUA_SETTLE_MS`, which is how the read-latency effect was first cornered — at
/// 600 ms several arms scored half, and at 2000 ms every arm scored full.
fn settle() -> Duration {
    Duration::from_millis(
        std::env::var("CUA_SETTLE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    )
}

/// The checkmark (or lack of one) macOS draws beside a menu item.
///
/// The second of the two read-backs. A menu toggle is written one of two ways:
/// the title flips ("Show Tab Bar" ↔ "Hide Tab Bar"), or the title is fixed and
/// a mark appears. Reading both means the probe does not have to be told which
/// kind of item it was pointed at.
const MARK_CHAR: &str = "AXMenuItemMarkChar";

/// What the item says about itself, which is the whole of the read-back.
///
/// Deliberately re-read from the menu each time rather than held: pressing an
/// item can make the app rebuild the menu, and a stale `AXUIElement` would then
/// answer for an object that is no longer in the tree.
#[derive(Debug, PartialEq, Eq)]
struct State {
    title: Option<String>,
    mark: Option<String>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(query), Some(menu_title), Some(item_needle)) =
        (args.next(), args.next(), args.next())
    else {
        eprintln!("usage: menu_item_press <pid|app-name> <menu title> <item substring> [trials]");
        eprintln!("   eg: menu_item_press TextEdit View \"Tab Bar\" 10");
        std::process::exit(2);
    };
    let trials: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);

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
    let frontmost_before = frontmost_pid();
    println!("pid {pid}, frontmost pid: {frontmost_before:?}");

    let Some(bar) = app.element("AXMenuBar") else {
        eprintln!("this app publishes no AXMenuBar");
        std::process::exit(1);
    };

    // The menu-bar item is the handle on the menu. Its single `AXMenu` child is
    // published whether or not the menu is open, which is the first thing worth
    // recording: accessibility does not need the menu on screen to describe it.
    let bar_items: Vec<Element> = bar.children();
    println!(
        "AXMenuBar children: {}",
        bar_items
            .iter()
            .filter_map(|e| e.label())
            .collect::<Vec<_>>()
            .join(" | ")
    );

    let Some(bar_item) = bar_items
        .iter()
        .find(|e| e.label().is_some_and(|l| l == menu_title))
    else {
        eprintln!("no menu-bar item titled {menu_title:?}");
        std::process::exit(1);
    };
    let Some(menu) = bar_item.children().into_iter().next() else {
        eprintln!("{menu_title:?} has no AXMenu child");
        std::process::exit(1);
    };
    println!(
        "{menu_title:?}: role={:?} actions={:?}",
        bar_item.role(),
        bar_item.actions()
    );
    println!(
        "  its menu: role={:?} children={} (menu closed)",
        menu.role(),
        menu.children().len()
    );

    for (i, item) in menu.children().iter().enumerate() {
        println!(
            "  [{i}] {:?} {:?} enabled={:?} selected={:?} mark={:?} actions={:?}",
            item.role(),
            item.label(),
            item.bool(attr::ENABLED),
            item.bool("AXSelected"),
            item.string(MARK_CHAR),
            item.actions()
        );
    }

    let find_item = || -> Option<Element> {
        menu.children()
            .into_iter()
            .find(|e| e.label().is_some_and(|l| l.contains(&item_needle)))
    };
    let read = || -> Option<State> {
        find_item().map(|i| State {
            title: i.label(),
            mark: i.string(MARK_CHAR),
        })
    };
    let Some(item) = find_item() else {
        eprintln!("no item in {menu_title:?} whose title contains {item_needle:?}");
        std::process::exit(1);
    };
    println!(
        "\ntarget: {:?} {:?} actions={:?} AXSelected settable={} state={:?}",
        item.role(),
        item.label(),
        item.actions(),
        item.is_settable("AXSelected"),
        read()
    );

    // Four arms, each answering a different half of the original claim.
    //
    // `closed` is the one that matters most. If pressing an item of a menu that
    // was never opened acts, then the story is not about `AXPress` needing two
    // goes; and if it does not act, the difference between it and `opened` is
    // what "the first press only selected it" was really describing.
    let start = read();
    let mut results: Vec<Arm> = Vec::new();
    for name in [
        "closed",
        "opened",
        "selected-then-press",
        "opened-twice",
        "opened-alternating",
        "closed-alternating",
    ] {
        let mut arm = Arm {
            name,
            acted_on_one: 0,
            acted_at_all: 0,
            selected_after_one: 0,
            enabled_before: 0,
            trials: 0,
            latencies: Vec::new(),
        };
        for trial in 0..trials {
            let Some(before) = read() else {
                println!("  {name} trial {trial}: the item vanished from the menu");
                break;
            };
            arm.trials += 1;

            if name == "opened" || name == "opened-twice" || name == "opened-alternating" {
                if let Err(e) = bar_item.perform("AXPress") {
                    println!("  {name} trial {trial}: opening {menu_title:?} failed: {e}");
                    break;
                }
                sleep(Duration::from_millis(250));
            }
            if name == "selected-then-press" {
                let write = item.set_bool("AXSelected", true);
                if write.is_err() {
                    println!("  {name} trial {trial}: AXSelected write failed: {write:?}");
                }
                sleep(Duration::from_millis(120));
            }

            // Enablement is read here rather than up front because it is not a
            // static property: AppKit only validates a menu item when the menu
            // is about to be shown, so the same item reads disabled with the
            // menu closed and enabled with it open.
            let target = find_item();
            let enabled = target.as_ref().and_then(|i| i.bool(attr::ENABLED));
            if enabled == Some(true) {
                arm.enabled_before += 1;
            }
            let press = target.map(|i| i.perform("AXPress"));

            // Poll rather than settle once. The original claim came from a
            // single read at some unrecorded delay, and a read taken before the
            // app republishes the item is indistinguishable from a press that
            // did nothing — so the delay is the measurement, not a constant to
            // be tuned until the answer looks right.
            let (after_one, latency) = poll_until_change(&read, &before);
            if let Some(ms) = latency {
                arm.latencies.push(ms);
            }
            let selected = find_item().and_then(|i| i.bool("AXSelected"));
            if selected == Some(true) {
                arm.selected_after_one += 1;
            }
            let acted_on_one = after_one.as_ref() != Some(&before);
            if acted_on_one {
                arm.acted_on_one += 1;
            }

            if name == "opened-twice" && !acted_on_one {
                // Only reached when the poll timed out, so this is the "press it
                // again because nothing happened" move the original claim
                // recommended.
                let _ = find_item().map(|i| i.perform("AXPress"));
                sleep(settle());
            }

            let final_state = read();
            let acted = final_state.as_ref() != Some(&before);
            if acted {
                arm.acted_at_all += 1;
            }
            println!(
                "  {name} trial {trial}: enabled={enabled:?} press={} \n      \
                 before={before:?}\n      after-one={after_one:?} selected={selected:?} \
                 acted-on-one={acted_on_one} after={latency:?}ms\n      \
                 final={final_state:?} acted={acted}",
                match &press {
                    Some(Ok(())) => "ok".to_string(),
                    Some(Err(e)) => format!("{e}"),
                    None => "item gone".to_string(),
                }
            );

            // Leave the app as it was found: an odd number of acting trials
            // would otherwise end with the toggle flipped.
            //
            // The alternating arm deliberately does not, which is what makes it
            // the one arm that measures both directions under identical
            // conditions: trial N leaves the state trial N+1 starts from, so a
            // toggle that moves one way and not the other shows up as strictly
            // alternating successes rather than as a rate.
            if acted && !name.ends_with("-alternating") {
                restore(bar_item, &read, &find_item, &before);
            }
            // A menu left open swallows the next trial's press.
            dismiss(&menu);
        }
        // The alternating arm still has to hand the app back as it found it.
        if name.ends_with("-alternating") {
            if let Some(want) = &start {
                if read().as_ref() != Some(want) {
                    restore(bar_item, &read, &find_item, want);
                }
            }
        }
        results.push(arm);
    }

    println!(
        "\n{trials} trials requested per arm, each press given up to {} ms to become readable",
        POLL_LIMIT.as_millis()
    );
    for arm in &results {
        let mut ms = arm.latencies.clone();
        ms.sort_unstable();
        let span = match (ms.first(), ms.last()) {
            (Some(lo), Some(hi)) => format!("{lo}-{hi} ms"),
            _ => "-".to_string(),
        };
        println!(
            "  {:22} enabled-when-pressed {}/{}, acted on one press {}/{} (readable after {span}), \
             acted within two {}/{}, AXSelected true after one {}/{}",
            arm.name,
            arm.enabled_before,
            arm.trials,
            arm.acted_on_one,
            arm.trials,
            arm.acted_at_all,
            arm.trials,
            arm.selected_after_one,
            arm.trials
        );
    }
    println!(
        "frontmost pid: {frontmost_before:?} before, {:?} after",
        frontmost_pid()
    );
    println!("final item state: {:?}", read());
}

/// One arm's tally. `acted_on_one` versus `acted_at_all` is the whole question:
/// they are equal if one press is enough, and they differ by exactly the trials
/// that needed a second.
struct Arm {
    name: &'static str,
    trials: usize,
    enabled_before: usize,
    acted_on_one: usize,
    acted_at_all: usize,
    selected_after_one: usize,
    /// How long after the press the change became readable, per acting trial.
    latencies: Vec<u128>,
}

/// The ceiling on how long a single `AXPress` is given to become readable.
const POLL_LIMIT: Duration = Duration::from_millis(3000);

/// Read the item until it differs from `before`, and report how long that took.
///
/// Returns the last state read either way, so a timeout is reported as the state
/// that was actually there rather than as an absence.
fn poll_until_change(
    read: &dyn Fn() -> Option<State>,
    before: &State,
) -> (Option<State>, Option<u128>) {
    let started = std::time::Instant::now();
    let mut last = None;
    while started.elapsed() < POLL_LIMIT {
        sleep(Duration::from_millis(50));
        last = read();
        if last.as_ref() != Some(before) {
            return (last, Some(started.elapsed().as_millis()));
        }
    }
    (last, None)
}

/// Put a toggle back, re-opening the menu if that is what it takes.
///
/// Pressing again is the whole of it — the item is a toggle — but the press has
/// to be aimed at a freshly read element, because the menu may have been rebuilt
/// since.
fn restore(
    bar_item: &Element,
    read: &dyn Fn() -> Option<State>,
    find_item: &dyn Fn() -> Option<Element>,
    want: &State,
) {
    for open_first in [false, true, true] {
        if open_first {
            let _ = bar_item.perform("AXPress");
            sleep(Duration::from_millis(250));
        }
        let Some(now) = read() else { return };
        if &now == want {
            return;
        }
        if let Some(item) = find_item() {
            let _ = item.perform("AXPress");
        }
        // Wait the restoring press out on the same terms as a measured one. An
        // earlier version slept a fixed 600 ms here, and that alone made the
        // arms that restore score exactly half of the arms that do not: the
        // restore's own press was still in flight when the next trial's press
        // went out, so the two cancelled. The read latency below is the whole
        // subject of this probe, and the probe was tripping over it.
        poll_until_change(read, &now);
        if read().as_ref() == Some(want) {
            return;
        }
    }
    println!("  ! could not restore the item to {want:?}");
}

/// Close the menu if it is open, using accessibility rather than a keystroke.
fn dismiss(menu: &Element) {
    let _ = menu.perform("AXCancel");
    sleep(Duration::from_millis(200));
}

/// Which app the system considers frontmost, via `lsappinfo`, which needs no
/// grant of any kind — the system-wide `AXFocusedApplication` attribute reads
/// back as absent here, and an `osascript` round trip would have asked for an
/// Automation grant this probe has no other use for.
///
/// Recorded before and after because pressing a *background* app's
/// `AXMenuBarItem` is the obvious way this probe could cheat: the macOS menu bar
/// only ever shows the frontmost app's menus, so an implementation that quietly
/// activated the app first would answer a different question than the one asked.
fn frontmost_pid() -> Option<i64> {
    let front = std::process::Command::new("/usr/bin/lsappinfo")
        .arg("front")
        .output()
        .ok()?;
    let asn = String::from_utf8_lossy(&front.stdout).trim().to_string();
    let out = std::process::Command::new("/usr/bin/lsappinfo")
        .args(["info", "-only", "pid", &asn])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split('=')
        .nth(1)?
        .trim()
        .parse()
        .ok()
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
        .min_by_key(|l| l.len())
        .and_then(|l| l.split_whitespace().next()?.parse().ok())
}
