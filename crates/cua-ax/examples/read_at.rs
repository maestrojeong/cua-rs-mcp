//! Read an element's value by label, walking the app's window tree.
//!
//! By label rather than by hit-test: `AXUIElementCopyElementAtPosition` on a
//! background app was measured to answer `AXMenuBar` regardless of the point,
//! so it cannot witness a change in an occluded window — which is precisely
//! the situation a pid-targeted click has to be judged in.
//!
//! Usage: read_at <pid> <label-substring>
use cua_ax::{attr, require_trusted, Element, Limits};

fn main() {
    require_trusted().expect("not trusted");
    let a: Vec<String> = std::env::args().collect();
    let pid: i32 = a[1].parse().unwrap();
    let needle = a[2].to_lowercase();

    let app = Element::for_pid(pid);
    let window = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())
        .expect("no window");

    for node in window.snapshot_tree(Limits::default()) {
        let label = node.label.clone().unwrap_or_default();
        if !label.to_lowercase().contains(&needle) {
            continue;
        }
        println!(
            "[{}] {} {label:?} value_num={:?} value_str={:?}",
            node.index,
            node.role,
            node.element.number(attr::VALUE),
            node.element.string(attr::VALUE)
        );
        return;
    }
    println!("no element labelled {needle:?}");
}
