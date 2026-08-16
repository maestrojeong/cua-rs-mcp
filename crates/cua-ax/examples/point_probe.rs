//! Ask `AXUIElementCopyElementAtPosition` what is at a point, for a pid.
//!
//! Usage: point_probe <pid> <x> <y>
use cua_ax::{attr, require_trusted, Element};

fn main() {
    require_trusted().expect("not trusted");
    let a: Vec<String> = std::env::args().collect();
    let pid: i32 = a[1].parse().unwrap();
    let x: f32 = a[2].parse().unwrap();
    let y: f32 = a[3].parse().unwrap();

    let app = Element::for_pid(pid);
    match app.element_at(x, y) {
        Ok(el) => println!(
            "role={:?} subrole={:?} label={:?} frame={:?} actions={:?}",
            el.role(),
            el.string(attr::SUBROLE),
            el.string(attr::TITLE),
            el.frame(),
            el.actions()
        ),
        Err(e) => println!("error: {e}"),
    }
}
