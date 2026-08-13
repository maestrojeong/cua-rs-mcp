//! Make one of an app's windows the focused one, by title substring, using
//! nothing but AX. `AXRaise` restacks it; `AXMain`/`AXFocused` are what
//! `AXFocusedWindow` actually reports, and a raise alone does not always set
//! them.
//!
//! Usage: focus_window <pid> <title-substring>
use cua_ax::{action, attr, require_trusted, Element};

fn main() {
    require_trusted().expect("not trusted");
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().expect("usage: focus_window <pid> <title>");
    let needle = args[2].to_lowercase();

    let app = Element::for_pid(pid);
    let windows = app.elements(attr::WINDOWS);
    println!("{} window(s)", windows.len());

    for w in &windows {
        let title = w.string(attr::TITLE).unwrap_or_default();
        if !title.to_lowercase().contains(&needle) {
            continue;
        }
        println!("raising {title:?}");
        match w.perform(action::RAISE) {
            Ok(()) => println!("  AXRaise: ok"),
            Err(e) => println!("  AXRaise: {e}"),
        }
        for a in ["AXMain", "AXFocused"] {
            match w.set_bool(a, true) {
                Ok(()) => println!("  set {a}=true: ok"),
                Err(e) => println!("  set {a}=true: {e}"),
            }
        }
        let focused = app
            .element(attr::FOCUSED_WINDOW)
            .and_then(|f| f.string(attr::TITLE));
        println!("  AXFocusedWindow is now {focused:?}");
        return;
    }
    println!("no window matching {needle:?}");
}
