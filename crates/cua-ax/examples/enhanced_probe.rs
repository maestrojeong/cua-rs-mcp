//! Hypothesis: AppKit exposes richer AX actions once AXEnhancedUserInterface
//! is set on the *application* element. Sky's binary references that attribute;
//! cua-rs only ever tried AXManualAccessibility. Test whether KakaoTalk's
//! custom-drawn rows start advertising a real activation verb after the poke.
//! Usage: enhanced_probe <pid> <x> <y>
use cua_ax::{attr, require_trusted, Element};

fn report(label: &str, el: &Element) {
    println!("  {label}: role={:?} actions={:?}", el.role(), el.actions());
}

fn main() {
    require_trusted().expect("not trusted");
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().unwrap();
    let x: f32 = args[2].parse().unwrap();
    let y: f32 = args[3].parse().unwrap();

    let app = Element::for_pid(pid);

    println!("BEFORE any poke:");
    let el = app.element_at(x, y).expect("hit-test");
    report("hit", &el);
    if let Some(row) = el.element(attr::PARENT) {
        report("row", &row);
    }

    for flag in [attr::ENHANCED_USER_INTERFACE, attr::MANUAL_ACCESSIBILITY] {
        println!("\nsetting {flag}=true on the app element:");
        println!("  settable: {}", app.is_settable(flag));
        match app.set_bool(flag, true) {
            Ok(()) => println!("  write: OK"),
            Err(e) => println!("  write: FAILED ({e})"),
        }
        println!("  reads back: {:?}", app.bool(flag));

        // Re-hit-test: the tree may have been rebuilt underneath us.
        std::thread::sleep(std::time::Duration::from_millis(600));
        match app.element_at(x, y) {
            Ok(el2) => {
                report("hit after", &el2);
                if let Some(row2) = el2.element(attr::PARENT) {
                    report("row after", &row2);
                }
            }
            Err(e) => println!("  re-hit-test failed: {e}"),
        }
    }
}
