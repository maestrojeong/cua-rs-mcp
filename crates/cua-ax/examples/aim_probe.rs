//! Read-only aim check for the pointer-warp click.
//!
//! For every element in an app's window whose label matches, prints the two
//! candidate aim points — the frame centre and `AXActivationPoint` — and then
//! asks the *system-wide* AX element who actually owns each of those screen
//! points right now. If the answer is not the target pid, a warp click there
//! would land on another app (the classic "window is on another Space" case).
//!
//! Usage: aim_probe <pid> [label-substring]
use cua_ax::{attr, require_trusted, Element, Limits};

fn owner_of(x: f64, y: f64) -> String {
    match Element::system_wide().element_at(x as f32, y as f32) {
        Ok(el) => {
            let pid = el.pid().ok();
            format!(
                "pid={:?} role={:?} label={:?}",
                pid,
                el.role(),
                el.string(attr::TITLE)
                    .or_else(|| el.string(attr::DESCRIPTION))
            )
        }
        Err(e) => format!("nothing ({e})"),
    }
}

fn main() {
    require_trusted().expect("not trusted");
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().expect("usage: aim_probe <pid> [label]");
    let needle = args.get(2).map(|s| s.to_lowercase());

    let app = Element::for_pid(pid);
    let window = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())
        .expect("no window");
    println!("window: {:?} frame={:?}", window.string(attr::TITLE), window.frame());

    let nodes = window.snapshot_tree(Limits::default());
    println!("{} nodes", nodes.len());

    for node in &nodes {
        let hay = format!(
            "{} {} {}",
            node.role,
            node.label.clone().unwrap_or_default(),
            node.value.clone().unwrap_or_default()
        )
        .to_lowercase();
        if let Some(n) = &needle {
            if !hay.contains(n.as_str()) {
                continue;
            }
        }
        let el = &node.element;
        let frame = el.frame();
        println!(
            "\n[{}] {} label={:?} actions={:?}",
            node.index, node.role, node.label, node.actions
        );
        if let Some(f) = frame {
            let cx = f.origin.x + f.size.width / 2.0;
            let cy = f.origin.y + f.size.height / 2.0;
            println!("  frame        = {f:?}");
            println!("  centre       = ({cx:.0}, {cy:.0}) -> owner {}", owner_of(cx, cy));
        }
        match el.activation_point() {
            Some(p) => println!(
                "  AXActivationPoint = ({:.0}, {:.0}) -> owner {}",
                p.x,
                p.y,
                owner_of(p.x, p.y)
            ),
            None => println!("  AXActivationPoint = (absent)"),
        }
    }
}
