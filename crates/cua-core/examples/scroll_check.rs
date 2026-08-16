//! Does a pid-routed wheel event scroll anything?
//!
//! ```console
//! $ cargo run -p cua-core --example scroll_check -- wheel TextEdit 1
//! $ cargo run -p cua-core --example scroll_check -- key   TextEdit 8   # control
//! ```
//!
//! # Why the pixels, and why the control arm
//!
//! A tree diff is not sensitive enough — a text view reports the same elements
//! at every offset, and a virtualized list reports the same rows — while
//! `AXVisibleCharacterRange` is a range value rather than a string, so it cannot
//! be read back through the string accessor. The window's own image is the one
//! signal that is both unambiguous and app-agnostic.
//!
//! The `key` arm is the control, and it is what makes a negative result from the
//! `wheel` arm mean anything. It scrolls the same window in the same session
//! through a mechanism already measured to work (`tests/live_keyboard.rs`), so a
//! run where `key` moves the pixels and `wheel` does not has isolated the
//! failure to the scroll event rather than to pid routing, the window number,
//! the aim point, or the instrument.

use cua_core::{Cua, ScrollAmount, ScrollDir, StateOptions, Target};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arm = a.get(1).map(String::as_str).unwrap_or("help");
    let app = a.get(2).cloned().unwrap_or_else(|| "TextEdit".into());
    let index: usize = match a.get(3).and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => {
            eprintln!("usage: scroll_check <wheel|key> <app> <element_index>");
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

    let before = shoot();
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
        _ => {
            eprintln!("usage: scroll_check <wheel|key> <app> <element_index>");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(700));

    let after = shoot();
    let moved = !before.is_empty() && before != after;
    println!("image {} bytes -> {} bytes", before.len(), after.len());
    println!(
        "\n==> the `{arm}` arm moved the content: {}",
        if moved { "YES" } else { "NO" }
    );
}
