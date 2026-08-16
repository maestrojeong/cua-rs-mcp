//! Does the elementless pid click reach a window-local pixel?
//!
//! `click_in_window` is the one action with no element behind it, so it has no
//! automatic verification: the tool reports delivery and stops. That makes a
//! manual probe the only way to see whether the events land, and this is it.
//!
//! It also exercises each refusal in turn, because for a blind click the gates
//! *are* the feature. A run that reaches the final line has proved that a bad
//! window id, another app's window id and an out-of-bounds offset are all
//! rejected before anything is posted.
//!
//! Usage: window_click_probe <app> [x] [y]
//!
//! `x`/`y` are window-local points from the top-left corner, defaulting to the
//! middle of the window — pick something harmless, since nothing here can tell
//! what is at the coordinate.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let app = a
        .get(1)
        .expect("usage: window_click_probe <app> [x] [y]")
        .clone();

    let cua = cua_core::Cua::new();
    let state = cua
        .get_app_state(&app, cua_core::StateOptions::default())
        .expect("get_app_state must succeed before anything can be clicked");
    let wid = state
        .window_id
        .expect("no verified window id; enable Screen Recording and retry");
    let frame = state.window_frame.expect("no window frame");
    println!(
        "{} (pid {}) window {wid} at {:.0},{:.0} {:.0}x{:.0}",
        state.app.name,
        state.app.pid,
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height
    );

    let x: f64 = a
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(frame.size.width / 2.0);
    let y: f64 = a
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(frame.size.height / 2.0);

    // The gates, before the real click. Each of these is a way a blind click
    // could go somewhere the caller never looked.
    for (label, id, px, py) in [
        ("a window id this app never published", wid + 100_000, x, y),
        (
            "an offset past the window's width",
            wid,
            frame.size.width + 50.0,
            y,
        ),
        ("a negative offset", wid, -5.0, y),
    ] {
        match cua.click_in_window(&app, id, px, py, cua_core::MouseOptions::default(), false) {
            Ok(_) => println!("REFUSAL MISSING: {label} was accepted"),
            Err(e) => println!("refused {label}: {e}"),
        }
    }

    println!(
        "\nclicking window-local ({x:.0}, {y:.0}) — screen ({:.0}, {:.0})",
        frame.origin.x + x,
        frame.origin.y + y
    );
    match cua.click_in_window(&app, wid, x, y, cua_core::MouseOptions::default(), true) {
        Ok(r) => {
            println!("{} on {}", r.verb, r.target);
            println!("delivery: {}", r.delivery.as_str());
            println!("ui_changed: {}", r.ui_changed.as_str());
            if let Some(s) = r.state {
                match s.diff {
                    Some(d) => println!(
                        "diff: {} added, {} removed ({} nodes after)",
                        d.added.len(),
                        d.removed.len(),
                        s.node_count
                    ),
                    None => println!("no diff: {}", s.note.unwrap_or_default()),
                }
            }
        }
        Err(e) => println!("FAILED: {e}"),
    }
}
