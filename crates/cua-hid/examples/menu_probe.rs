//! Why does a menu button ignore a click that every other control accepts?
//!
//! A lot has been ruled out on the way here: the coordinate is the one the
//! reference implementation uses, the event carries a real AppKit header, the
//! timestamp is fresh, the focus notices land (`AXFrontmost` flips), and the
//! private and public per-pid post routes behave identically.
//!
//! This probe bisects the two arms that remain interesting, back to back on the
//! same target:
//!
//! | mode | what it does |
//! |---|---|
//! | `quiet` | the server's path — pid-routed click, cursor untouched |
//! | `warp`  | move the real pointer there first, click, put it back |
//!
//! Measured so far on KakaoTalk's chat-room header menu: **neither opens it**.
//! `warp` failing is the more informative half — it disproves "menu tracking
//! needs the real pointer under the control", which was the leading theory, and
//! means the remaining difference is in what the reference does *around* the
//! click rather than in the click itself.
//!
//! Two real bugs did fall out of running it, both now fixed in the library: the
//! window-level filter excluded floating-level windows (so the click was stamped
//! with another window's number), and the per-click `ApplicationDeactivated`
//! tore down the target's key-window state mid-gesture.
//!
//! Usage: menu_probe <pid> <screen-x> <screen-y> [quiet|warp|both]
//!
//! Detection is deliberately unfiltered: it dumps every top-level application
//! child *and* every window of the pid. A pop-up menu is neither a child of the
//! window nor reliably an `AXMenu`, so guessing at either would report a menu
//! that did open as a failure.

use cua_ax::{attr, require_trusted, Element};

/// Every top-level child of the application, with the labels of its own
/// children.
///
/// Deliberately not filtered by role. The first version of this probe only
/// looked for roles containing "Menu" and reported "no menu" — but the
/// reference implementation describes what it opens as a top-level element
/// titled 메뉴 carrying `AXCancel`, whose role is not documented. Filtering on a
/// guessed role means a menu that *did* open reads as a failure, which is the
/// most expensive kind of wrong answer here.
fn app_children(app: &Element) -> Vec<String> {
    app.elements(attr::CHILDREN)
        .iter()
        .map(|c| {
            let label = c
                .string(attr::TITLE)
                .or_else(|| c.string(attr::DESCRIPTION))
                .unwrap_or_default();
            let kids: Vec<String> = c
                .elements(attr::CHILDREN)
                .iter()
                .filter_map(|i| i.string(attr::TITLE))
                .filter(|t| !t.is_empty())
                .take(14)
                .collect();
            let actions = c.actions();
            format!(
                "{} {label:?} actions={actions:?} kids={kids:?}",
                c.role().unwrap_or_else(|| "?".into())
            )
        })
        .collect()
}

/// Every window the capture layer can see for this pid, unfiltered.
///
/// A pop-up menu is not an accessibility child of the application and not a
/// child of the window either — it is its own window at a high level. Listing
/// windows is therefore the detector that can actually see one open.
fn pid_windows(pid: libc::pid_t) -> Vec<String> {
    cua_capture::list_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid)
        .map(|w| {
            format!(
                "id={} layer={} onscreen={} {}x{} at ({}, {}) {:?}",
                w.id,
                w.layer,
                w.on_screen,
                w.frame.size.width,
                w.frame.size.height,
                w.frame.origin.x,
                w.frame.origin.y,
                w.title
            )
        })
        .collect()
}

fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(600));
}

/// The pid the window server currently considers frontmost — the real one, not
/// the target's own `AXFrontmost` belief, which the synthesized notices move.
fn real_frontmost() -> Option<libc::pid_t> {
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
}

/// Make `pid` genuinely frontmost and wait for the window server to agree.
///
/// This is `NSRunningApplication.activate`, the thing the rest of cua-rs refuses
/// to call. The reference implementation does call it — its focus tap answers a
/// focus steal with `activateWithOptions(0)` — so this arm measures whether real
/// activation is what the menu has been waiting for.
fn really_activate(pid: libc::pid_t) -> bool {
    let Some(app) =
        objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
    else {
        return false;
    };
    app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::ActivateAllWindows);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    while std::time::Instant::now() < deadline {
        if real_frontmost() == Some(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    false
}

fn dismiss(app: &Element) {
    // Escape through the app element, then give tracking time to unwind. Leaving
    // a menu open would poison the next arm of the experiment.
    let _ = app.perform("AXCancel");
    std::thread::sleep(std::time::Duration::from_millis(400));
}

fn main() {
    require_trusted().expect("Accessibility is not granted to this process");
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: menu_probe <pid> <screen-x> <screen-y> [quiet|activate|warp|both]");
        std::process::exit(2);
    }
    let pid: libc::pid_t = args[1].parse().expect("pid");
    let x: f64 = args[2].parse().expect("x");
    let y: f64 = args[3].parse().expect("y");
    let mode = args.get(4).map(String::as_str).unwrap_or("both");

    let app = Element::for_pid(pid);
    let windows = cua_capture::list_windows().expect("list_windows (Screen Recording?)");
    let Some(win) = windows
        .iter()
        .filter(|w| w.pid == pid && w.is_plausible_target())
        .find(|w| {
            let f = w.frame;
            x >= f.origin.x
                && x <= f.origin.x + f.size.width
                && y >= f.origin.y
                && y <= f.origin.y + f.size.height
        })
    else {
        // "not found" is ambiguous between a bad coordinate and a window the
        // capture layer cannot see, and those need opposite fixes. Say which.
        eprintln!("no ordinary window of pid {pid} contains ({x}, {y}). Candidates:");
        for w in windows.iter().filter(|w| w.pid == pid) {
            eprintln!(
                "  id={} layer={} onscreen={} frame=({}, {}) {}x{} title={:?}",
                w.id,
                w.layer,
                w.on_screen,
                w.frame.origin.x,
                w.frame.origin.y,
                w.frame.size.width,
                w.frame.size.height,
                w.title
            );
        }
        std::process::exit(1);
    };
    println!(
        "window {} {:?}",
        win.id,
        win.title.as_deref().unwrap_or("?")
    );
    println!("children before:");
    for line in app_children(&app) {
        println!("   {line}");
    }
    println!("windows before:");
    for line in pid_windows(pid) {
        println!("   {line}");
    }

    if mode == "quiet" || mode == "both" {
        dismiss(&app);
        let assist = window_assist(pid, win);
        let r = cua_hid::click_background_pid(
            cua_hid::PidClick {
                pid,
                point: (x, y),
                window_local: (x - win.frame.origin.x, y - win.frame.origin.y),
                wid: win.id,
                count: 1,
            },
            assist,
            &|| Element::for_pid(pid).bool("AXFrontmost").unwrap_or(false),
        );
        settle();
        println!("[quiet] click={r:?}");
        for line in app_children(&app) {
            println!("   {line}");
        }
        println!("   -- windows --");
        for line in pid_windows(pid) {
            println!("   {line}");
        }
        dismiss(&app);
    }

    if mode == "activate" || mode == "both" {
        dismiss(&app);
        let previous = real_frontmost();
        let became = really_activate(pid);
        println!(
            "[activ] previous frontmost={previous:?} target now frontmost={became} (real={:?})",
            real_frontmost()
        );
        // No activation assist here, deliberately. Its whole job is to make a
        // window key without real activation, which this arm has already done
        // for real — and it was measured to drag the window: the target moved
        // from (1140, 48) to (397, 95) across one run, which is a synthesized
        // title-bar press being read as the start of a drag.
        //
        // Re-reading the frame matters for the same reason: a stale origin makes
        // the window-local conversion wrong, and a wrong window-local point is
        // exactly what turns a click into a drag.
        let live = cua_capture::list_windows()
            .unwrap_or_default()
            .into_iter()
            .find(|w| w.id == win.id)
            .unwrap_or_else(|| win.clone());
        let r = cua_hid::click_background_pid(
            cua_hid::PidClick {
                pid,
                point: (x, y),
                window_local: (x - live.frame.origin.x, y - live.frame.origin.y),
                wid: live.id,
                count: 1,
            },
            None,
            &|| Element::for_pid(pid).bool("AXFrontmost").unwrap_or(false),
        );
        settle();
        println!(
            "[activ] click={r:?} (window at ({}, {}))",
            live.frame.origin.x, live.frame.origin.y
        );
        for line in app_children(&app) {
            println!("   {line}");
        }
        println!("   -- windows --");
        for line in pid_windows(pid) {
            println!("   {line}");
        }
        dismiss(&app);
        // Put the human's app back, which is the whole cost of this arm.
        if let Some(prev) = previous.filter(|p| *p != pid) {
            really_activate(prev);
            println!("[activ] restored frontmost={:?}", real_frontmost());
        }
    }

    if mode == "warp" || mode == "both" {
        dismiss(&app);
        let r = cua_hid::click_by_moving_pointer(x, y, 1);
        settle();
        println!("[warp ] click={r:?}");
        for line in app_children(&app) {
            println!("   {line}");
        }
        println!("   -- windows --");
        for line in pid_windows(pid) {
            println!("   {line}");
        }
        dismiss(&app);
    }
}

/// Same gate the server applies: only offer the window-focus click when the
/// window publishes an activation point that really belongs to it.
fn window_assist(
    pid: libc::pid_t,
    win: &cua_capture::WindowInfo,
) -> Option<cua_hid::ActivationAssist> {
    let app = Element::for_pid(pid);
    let window_el = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.element(attr::MAIN_WINDOW))
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())?;
    let p = window_el.activation_point()?;
    let owner = Element::system_wide()
        .element_at(p.x as f32, p.y as f32)
        .ok()?;
    if owner.pid().ok()? != pid || owner.role().as_deref() != Some("AXWindow") {
        return None;
    }
    Some(cua_hid::ActivationAssist {
        window_origin: (win.frame.origin.x, win.frame.origin.y),
        activation_point: (p.x, p.y),
    })
}
