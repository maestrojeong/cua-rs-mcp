//! Does a pid-routed wheel event scroll anything?
//!
//! ```console
//! $ cargo run -p cua-core --example scroll_check -- idle  TextEdit 1   # instrument
//! $ cargo run -p cua-core --example scroll_check -- key   TextEdit 8   # control
//! $ CUA_WHEEL_SCROLL=1 cargo run -p cua-core --example scroll_check -- wheel TextEdit 1
//! $ CUA_WHEEL_SCROLL=1 CUA_WHEEL_RECIPE=nsevent \
//!     cargo run -p cua-core --example scroll_check -- wheel TextEdit 1
//! ```
//!
//! # Three signals, and why the control arm
//!
//! - **The scroll bar's `AXValue`**, a 0–1 fraction the app recomputes from its
//!   own content offset. The best signal where it exists, because it is a number
//!   the app derived rather than a difference someone measured, and it is immune
//!   to anything else in the window repainting. Not every scroller publishes one:
//!   a web area does not.
//! - **The rendered tree**, which catches a scroll position a page publishes as
//!   text — `assets/scroll_fixture.html` prints its own `scrollY` for exactly
//!   this reason, since that is the only readable offset Chromium web content
//!   offers.
//! - **The window's own image**, the app-agnostic fallback. Weakest of the
//!   three, because a text view reports the same elements at every offset while
//!   its caret repaints on a timer, which is what the `idle` arm is for.
//!
//! The `key` arm is the control, and it is what makes a negative result from the
//! `wheel` arm mean anything. It scrolls the same window in the same session
//! through a mechanism already measured to work (`tests/live_keyboard.rs`), so a
//! run where `key` moves the pixels and `wheel` does not has isolated the
//! failure to the scroll event rather than to pid routing, the window number,
//! the aim point, or the instrument.
//!
//! The `idle` arm is the control for the *instrument*. It reads all three
//! signals twice with nothing sent to the window, and a window that fails it on
//! the pixels — a blinking caret is enough, and TextEdit's moves the capture by
//! a couple of hundred bytes — has only the first two signals left. Measured on
//! the 400-line bed: `idle` reported the pixels changing while the scroll bar's
//! `AXValue` held at `0.000000`, which is the whole reason the value is read.
//!
//! # The recipes
//!
//! `CUA_WHEEL_SCROLL=1` re-enables a tier that is refused by default because it
//! was measured not to work (DESIGN §11), and `CUA_WHEEL_RECIPE` chooses *how*
//! the event is built: `plain` (what ships), `nsevent` (round-tripped through
//! `+[NSEvent eventWithCGEvent:]`), `phased` (the continuous-and-phased fields a
//! trackpad carries), `gesture` (a Began/Changed/Ended sequence). `CUA_PUBLIC_POST=1`
//! is orthogonal and switches the *route* from `SLEventPostToPid` to the public
//! `CGEventPostToPid`; it applies to scrolls as well as clicks, because both go
//! out through the same post. Each arm prints the recipe it ran, so a
//! misspelled variable shows up in the output rather than in the conclusion.

use cua_ax::{attr, Element};
use cua_core::{apps, Cua, ScrollAmount, ScrollDir, StateOptions, Target};

/// The first `AXScrollBar` under the app's front window, depth-first.
///
/// Looked up once and held, because the element has to survive the scroll for
/// its value to be comparable across it.
fn scroll_bar(pid: i32) -> Option<Element> {
    let app = Element::for_pid(pid);
    let window = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.element(attr::MAIN_WINDOW))
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())?;
    let mut stack = vec![window];
    while let Some(el) = stack.pop() {
        if el.role().as_deref() == Some("AXScrollBar") {
            return Some(el);
        }
        stack.extend(el.children());
    }
    None
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arm = a.get(1).map(String::as_str).unwrap_or("help");
    let app = a.get(2).cloned().unwrap_or_else(|| "TextEdit".into());
    let index: usize = match a.get(3).and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => {
            eprintln!("usage: scroll_check <wheel|key|idle> <app> <element_index>");
            return;
        }
    };

    let cua = Cua::new();
    let st = cua
        .get_app_state(
            &app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    let wid = st.window_id.expect("no verified window id");
    let target = || Target::Index {
        index,
        snapshot_id: Some(st.snapshot_id),
        expected_role: None,
    };
    let shoot = || {
        cua_capture::capture_window(wid, 900)
            .map(|s| s.png)
            .unwrap_or_default()
    };
    println!(
        "window {wid}, scroll recipe `{}`, route {}",
        cua_hid::ScrollRecipe::from_env().as_str(),
        if std::env::var("CUA_PUBLIC_POST").is_ok() {
            "CGEventPostToPid (public)"
        } else {
            "SLEventPostToPid (private)"
        }
    );

    let bar = scroll_bar(apps::resolve_app(&app).expect("resolve app").pid);
    let position = || bar.as_ref().and_then(|b| b.string(attr::VALUE));
    let outline = || {
        cua.get_app_state(
            &app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .map(|s| s.tree)
        .unwrap_or_default()
    };

    let before = shoot();
    let before_value = position();
    // The snapshot this run's index was chosen from, *not* a fresh read: reading
    // again here would bump the generation and the arm below would be refused
    // for naming a stale index.
    let before_tree = st.tree.clone();
    match arm {
        "wheel" => {
            let r = cua.scroll(
                &app,
                target(),
                ScrollDir::Down,
                ScrollAmount::Points(400),
                false,
            );
            println!("scroll = {:?}", r.map(|x| x.verb));
        }
        "key" => {
            let r = cua.press_key(&app, target(), "pagedown", false, false);
            println!("press_key pagedown = {:?}", r.map(|x| x.verb));
        }
        // Nothing is sent. Any difference the `idle` arm reports is a difference
        // the other two arms would have reported as a success.
        "idle" => println!("nothing sent"),
        _ => {
            eprintln!("usage: scroll_check <wheel|key|idle> <app> <element_index>");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(700));

    let after = shoot();
    let after_value = position();
    let after_tree = outline();

    let px_moved = !before.is_empty() && before != after;
    let value_moved = before_value.is_some() && before_value != after_value;
    let tree_moved = !before_tree.is_empty() && before_tree != after_tree;
    println!("scroll bar AXValue {before_value:?} -> {after_value:?}");
    println!("image {} bytes -> {} bytes", before.len(), after.len());
    println!(
        "tree {} lines -> {} lines, changed = {tree_moved}",
        before_tree.lines().count(),
        after_tree.lines().count()
    );
    println!(
        "\n==> the `{arm}` arm moved the content: {}",
        match (value_moved, tree_moved, px_moved) {
            (true, _, _) => "YES, by the scroll bar's own value",
            // Two independent signals agreeing is not the same as one of them
            // alone. The tree is derived from accessibility and the image from
            // the window server, so a change in both is not one reading being
            // noisy — and `idle` establishes that the image is stable at rest.
            // Ranking this as "maybe" was wrong: it read the `key` control arm,
            // which demonstrably scrolls, as inconclusive.
            (false, true, true) => "YES, by the tree and the pixels together",
            (false, true, false) =>
                "MAYBE — the tree changed but the pixels did not, so \
                                     something was re-read rather than moved",
            (false, false, true) =>
                "PIXELS ONLY — and `idle` on this window is what says whether \
                                     that means anything",
            (false, false, false) => "NO",
        }
    );
}
