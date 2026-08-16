//! What actually happens when a pid-routed keystroke is aimed at a text
//! element: what the app says about focus before and after, and whether the
//! text arrives.
//!
//! ```console
//! $ open -a TextEdit ~/Desktop/scratch.txt
//! $ cargo run -p cua-core --example keyboard_probe -- TextEdit
//! ```
//!
//! This is the instrument behind `tests/live_keyboard.rs`: when that test
//! fails, this says which half failed.

use cua_ax::{attr, Element};
use cua_core::{apps, StateOptions};

fn main() {
    let app_query = std::env::args().nth(1).unwrap_or_else(|| "TextEdit".into());
    if !cua_ax::is_trusted() {
        eprintln!("no Accessibility grant for this host process");
        std::process::exit(1);
    }

    let info = apps::resolve_app(&app_query).expect("resolve app");
    println!("{} pid {}", info.name, info.pid);

    let cua = cua_core::Cua::new();
    let state = cua
        .get_app_state(
            &app_query,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    println!("{}", state.tree);

    let app = Element::for_pid(info.pid);
    let describe = |el: &Option<Element>| match el {
        Some(el) => format!(
            "{} {:?} value={:?}",
            el.role().unwrap_or_default(),
            el.string(attr::TITLE).unwrap_or_default(),
            el.string(attr::VALUE).unwrap_or_default()
        ),
        None => "<none>".into(),
    };

    println!("AXFrontmost      = {:?}", app.bool("AXFrontmost"));
    println!(
        "focused (before) = {}",
        describe(&app.element(attr::FOCUSED_UI_ELEMENT))
    );

    // The element this probe aims at: the window's first text area.
    println!(
        "AXFocusedWindow={:?} AXMainWindow={:?} AXWindows={}",
        app.element(attr::FOCUSED_WINDOW).is_some(),
        app.element(attr::MAIN_WINDOW).is_some(),
        app.elements(attr::WINDOWS).len()
    );
    let window = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.element(attr::MAIN_WINDOW))
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())
        .expect("a window");
    let text = find_role(&window, "AXTextArea", 0).expect("a text area");
    println!("target           = {}", describe(&Some(text.clone())));
    println!(
        "AXFocused settable={} write={:?}",
        text.is_settable(attr::FOCUSED),
        text.set_bool(attr::FOCUSED, true)
    );
    println!(
        "focused (after)  = {}",
        describe(&app.element(attr::FOCUSED_UI_ELEMENT))
    );

    println!("skylight_available = {}", cua_hid::skylight_available());
    let window_number = state.window_id.map(|id| id as isize);
    let believes = {
        let app_el = Element::for_pid(info.pid);
        move || app_el.bool("AXFrontmost").unwrap_or(false)
    };
    cua_hid::prime_keyboard_target(info.pid, window_number, &believes);
    println!(
        "primed (window {window_number:?}); focused now = {}",
        describe(&app.element(attr::FOCUSED_UI_ELEMENT))
    );

    // A pid-routed click first, when asked: the click tier is the one that
    // makes a window key, and "click the element first" is the advice the
    // mismatched verdict gives, so it is worth measuring with and without.
    if std::env::args().any(|a| a == "--click") {
        let index = state
            .tree
            .lines()
            .find_map(|l| {
                let t = l.trim_start();
                let rest = t.strip_prefix('[')?;
                let (i, rest) = rest.split_once("] ")?;
                rest.starts_with("AXTextArea")
                    .then(|| i.parse::<usize>().ok())?
            })
            .expect("an actionable text area");
        let clicked = cua.click(
            &app_query,
            cua_core::Target::Index {
                index,
                snapshot_id: Some(state.snapshot_id),
                expected_role: None,
            },
            cua_core::MouseOptions::default(),
            false,
        );
        println!("click           = {clicked:?}");
        println!(
            "focused (click)  = {}",
            describe(&app.element(attr::FOCUSED_UI_ELEMENT))
        );
    }

    let before = text.string(attr::VALUE).unwrap_or_default();
    println!("value before     = {before:?}");

    match cua_hid::type_text_background_pid(info.pid, "PROBE") {
        Ok(()) => println!("type_text_background_pid: ok"),
        Err(e) => println!("type_text_background_pid: {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!(
        "value after text = {:?}",
        text.string(attr::VALUE).unwrap_or_default()
    );

    let chord = cua_hid::parse_chord("x").expect("chord");
    match cua_hid::press_chord_background_pid(info.pid, &chord) {
        Ok(()) => println!("press_chord_background_pid: ok"),
        Err(e) => println!("press_chord_background_pid: {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!(
        "value after key  = {:?}",
        text.string(attr::VALUE).unwrap_or_default()
    );
}

/// Depth-first search for the `n`-th element with a role.
fn find_role(root: &Element, role: &str, skip: usize) -> Option<Element> {
    let mut seen = 0;
    let mut stack = vec![root.clone()];
    while let Some(el) = stack.pop() {
        if el.role().as_deref() == Some(role) {
            if seen == skip {
                return Some(el);
            }
            seen += 1;
        }
        stack.extend(el.children());
    }
    None
}
