//! Does the synthesized focus protocol actually move the target's own idea of
//! being frontmost — and does a control that gates on it then accept a click?
//!
//! The pid-routed click path posts three things before the click that are all
//! *claims* rather than calls: `kCPSNotifyKeyFocusReturned`,
//! `ApplicationActivated`, and a click on the window's activation point. Whether
//! the target believes any of it is only observable through the app's own
//! `AXFrontmost`, and the server's `tracing` output is not reachable when the MCP
//! host owns stderr. So this probe reads the attribute directly, on both sides of
//! a real click, and prints the poll trace the server cannot show.
//!
//! Usage: focus_probe <pid> <screen-x> <screen-y>
//!
//! Example, KakaoTalk's chat-room header menu button:
//!   cargo run -p cua-hid --example focus_probe -- 34667 810 103
//!
//! Requires Accessibility *and* Screen Recording on the launching process, the
//! latter because the window id has to be revalidated the same way the server
//! does it.

use std::sync::atomic::{AtomicU32, Ordering};

use cua_ax::{attr, require_trusted, Element};

fn frontmost(pid: libc::pid_t) -> Option<bool> {
    Element::for_pid(pid).bool("AXFrontmost")
}

fn main() {
    require_trusted().expect("Accessibility is not granted to this process");

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: focus_probe <pid> <screen-x> <screen-y>");
        std::process::exit(2);
    }
    let pid: libc::pid_t = args[1].parse().expect("pid");
    let x: f64 = args[2].parse().expect("x");
    let y: f64 = args[3].parse().expect("y");

    // Same window resolution the server performs: the live frame, not a cached
    // one, decides the window-local coordinates.
    let windows = cua_capture::list_windows().expect("list_windows (Screen Recording?)");
    let win = windows
        .iter()
        .filter(|w| w.pid == pid && w.layer == 0)
        .find(|w| {
            let f = w.frame;
            x >= f.origin.x
                && x <= f.origin.x + f.size.width
                && y >= f.origin.y
                && y <= f.origin.y + f.size.height
        })
        .expect("no layer-0 window of that pid contains the point");

    println!(
        "window {} {:?} frame=({}, {}) {}x{}",
        win.id,
        win.title.as_deref().unwrap_or("<untitled>"),
        win.frame.origin.x,
        win.frame.origin.y,
        win.frame.size.width,
        win.frame.size.height
    );

    // The window's own activation point, gated exactly as the server gates it.
    let app_el = Element::for_pid(pid);
    let window_el = app_el
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app_el.element(attr::MAIN_WINDOW))
        .or_else(|| app_el.elements(attr::WINDOWS).into_iter().next());
    let assist = window_el.as_ref().and_then(|w| {
        let p = w.activation_point()?;
        let owner = Element::system_wide()
            .element_at(p.x as f32, p.y as f32)
            .ok()?;
        let owner_pid = owner.pid().ok()?;
        let role = owner.role();
        println!(
            "activation point = ({:.0}, {:.0}) -> owner pid={} role={:?}",
            p.x, p.y, owner_pid, role
        );
        if owner_pid != pid || role.as_deref() != Some("AXWindow") {
            println!("  -> assist REFUSED (hit test does not resolve to this app's window)");
            return None;
        }
        println!("  -> assist accepted");
        Some(cua_hid::ActivationAssist {
            window_origin: (win.frame.origin.x, win.frame.origin.y),
            activation_point: (p.x, p.y),
        })
    });

    println!("AXFrontmost before = {:?}", frontmost(pid));

    // Count how many times the wait asked, so a silent success and a silent
    // two-second timeout are told apart.
    let polls = AtomicU32::new(0);
    let believes = || {
        polls.fetch_add(1, Ordering::Relaxed);
        frontmost(pid).unwrap_or(false)
    };

    let started = std::time::Instant::now();
    let result = cua_hid::click_background_pid(
        cua_hid::PidClick {
            pid,
            point: (x, y),
            window_local: (x - win.frame.origin.x, y - win.frame.origin.y),
            wid: win.id,
            count: 1,
            button: cua_hid::MouseButton::Left,
            modifiers: cua_hid::Modifiers::empty(),
        },
        assist,
        &believes,
    );
    let elapsed = started.elapsed();

    println!(
        "click -> {:?} in {} ms, {} frontmost polls",
        result,
        elapsed.as_millis(),
        polls.load(Ordering::Relaxed)
    );
    println!("AXFrontmost after  = {:?}", frontmost(pid));

    // A menu, if one opened, is not a child of the window — it is its own
    // top-level element on the application. Report whatever appeared.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let children = app_el.elements(attr::CHILDREN);
    println!("app children after click:");
    for c in &children {
        println!(
            "  {:?} {:?}",
            c.role(),
            c.string(attr::TITLE)
                .or_else(|| c.string(attr::DESCRIPTION))
        );
    }
}
