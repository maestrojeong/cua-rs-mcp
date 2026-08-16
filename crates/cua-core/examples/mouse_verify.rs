//! Does the widened mouse model actually drive a real app?
//!
//! DESIGN §11 ships right-click, modifier-click, drag, hover and the wheel
//! scroll tier as "built, not yet verified". This is the instrument that settles
//! each one, and it is built around the same principle as the rest of the
//! checks here: every arm has a signal that can be *read back*, not eyeballed.
//!
//! ```console
//! $ cargo run -p cua-core --example mouse_verify -- drag-select TextEdit
//! $ cargo run -p cua-core --example mouse_verify -- shift-click TextEdit
//! $ cargo run -p cua-core --example mouse_verify -- right-click <app> <index>
//! $ cargo run -p cua-core --example mouse_verify -- wheel <app> <index>
//! $ cargo run -p cua-core --example mouse_verify -- hover <app> <index>
//! ```
//!
//! `right-click` and `hover` are checked against the window server rather than
//! the accessibility tree, because a context menu is a separate window that AX
//! does not publish at all — see the popup measurements in DESIGN.

use cua_ax::{attr, Element};
use cua_core::{
    apps, Cua, Modifiers, MouseOptions, PointerLocation, ScrollAmount, ScrollDir, StateOptions,
    Target,
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arm = a.get(1).map(String::as_str).unwrap_or("help");
    let app = a.get(2).cloned().unwrap_or_else(|| "TextEdit".into());

    if !cua_ax::is_trusted() {
        eprintln!("no Accessibility grant for this host process");
        std::process::exit(1);
    }

    match arm {
        "drag-select" => drag_select(&app),
        "shift-click" => shift_click(&app),
        "right-click" => right_click(&app, index_arg(&a)),
        "wheel" => wheel(&app, index_arg(&a)),
        "hover" => hover(&app, index_arg(&a)),
        _ => eprintln!(
            "usage: mouse_verify <drag-select|shift-click|right-click|wheel|hover> <app> [index]"
        ),
    }
}

fn index_arg(a: &[String]) -> usize {
    a.get(3)
        .and_then(|s| s.parse().ok())
        .expect("this arm needs an element index from get_app_state")
}

/// Snapshot, plus the window origin every window-local point is measured from.
struct Read {
    cua: Cua,
    pid: i32,
    snapshot_id: u64,
    origin: (f64, f64),
    tree: String,
}

fn read(app: &str) -> Read {
    let info = apps::resolve_app(app).expect("resolve app");
    let cua = Cua::new();
    let state = cua
        .get_app_state(
            app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    let frame = state.window_frame.expect("no window frame");
    println!(
        "{} pid {} window {:?} at {:.0},{:.0} snapshot {}",
        info.name, info.pid, state.window_title, frame.origin.x, frame.origin.y, state.snapshot_id
    );
    if std::env::var("MOUSE_VERIFY_TREE").is_ok() {
        for line in state
            .tree
            .lines()
            .filter(|l| l.trim_start().starts_with('['))
        {
            println!("  {line}");
        }
    }
    Read {
        cua,
        pid: info.pid,
        snapshot_id: state.snapshot_id,
        origin: (frame.origin.x, frame.origin.y),
        tree: state.tree,
    }
}

/// Depth-first search for the first element with a role, and its screen frame.
fn find_role(pid: i32, role: &str) -> Option<Element> {
    let app = Element::for_pid(pid);
    let window = app
        .element(attr::FOCUSED_WINDOW)
        .or_else(|| app.element(attr::MAIN_WINDOW))
        .or_else(|| app.elements(attr::WINDOWS).into_iter().next())?;
    let mut stack = vec![window];
    while let Some(el) = stack.pop() {
        if el.role().as_deref() == Some(role) {
            return Some(el);
        }
        stack.extend(el.children());
    }
    None
}

/// Every above-ordinary-layer window of a pid: how a context menu is detected,
/// since accessibility does not publish one.
fn popups(pid: i32) -> Vec<String> {
    cua_capture::list_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid && w.layer > 3 && w.on_screen)
        .map(|w| {
            format!(
                "id={} layer={} {:.0}x{:.0}",
                w.id, w.layer, w.frame.size.width, w.frame.size.height
            )
        })
        .collect()
}

// ── drag ─────────────────────────────────────────────────────────────────────

/// A drag across text must leave text selected.
///
/// The strongest signal available for a drag: `AXSelectedText` is a string the
/// app itself computed from where it believes the gesture went, so a non-empty
/// value cannot be produced by anything except a tracked press-move-release.
fn drag_select(app: &str) {
    let r = read(app);
    let text = find_role(r.pid, "AXTextArea").expect("no AXTextArea in the front window");
    let frame = text.frame().expect("no frame for the text area");
    println!(
        "text area at {:.0},{:.0} {:.0}x{:.0}",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    );

    let before = text.string(attr::SELECTED_TEXT).unwrap_or_default();
    println!("selected before = {before:?}");

    // Window-local, a few points inside the top-left of the text, dragging
    // right along the first line.
    let lx = frame.origin.x - r.origin.0 + 6.0;
    let ly = frame.origin.y - r.origin.1 + 10.0;
    let from = PointerLocation::WindowPoint { x: lx, y: ly };
    let to = PointerLocation::WindowPoint {
        x: lx + 220.0,
        y: ly,
    };
    println!("drag ({lx:.0},{ly:.0}) -> ({:.0},{ly:.0})", lx + 220.0);

    match r.cua.drag(
        app,
        from,
        to,
        MouseOptions::default(),
        Some(r.snapshot_id),
        false,
    ) {
        Ok(res) => println!("drag  = {}", res.verb),
        Err(e) => println!("drag FAILED = {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let after = text.string(attr::SELECTED_TEXT).unwrap_or_default();
    println!("selected after  = {after:?}");
    verdict("drag selects text", !after.is_empty() && after != before);
}

// ── modifier click ───────────────────────────────────────────────────────────

/// A ⇧-click must extend a selection from where the caret already is.
///
/// Read back through `AXSelectedTextRange`'s length, which the app maintains.
/// A plain click would leave length 0, so a non-zero length is the modifier
/// arriving rather than the click arriving.
fn shift_click(app: &str) {
    let r = read(app);
    let text = find_role(r.pid, "AXTextArea").expect("no AXTextArea in the front window");
    let frame = text.frame().expect("no frame");

    let lx = frame.origin.x - r.origin.0 + 6.0;
    let ly = frame.origin.y - r.origin.1 + 10.0;

    // Plant the caret with a plain click, then extend with shift.
    let plain = r.cua.click_in_window(
        app,
        cua_core::WindowPixel {
            window_id: window_id(app, &r),
            x: lx,
            y: ly,
            snapshot_id: Some(r.snapshot_id),
        },
        MouseOptions::default(),
        false,
    );
    println!("caret click = {:?}", plain.map(|x| x.verb));
    std::thread::sleep(std::time::Duration::from_millis(250));
    println!(
        "range after caret = {:?}",
        text.string("AXSelectedTextRange")
    );

    let shift = MouseOptions::parse("left", "shift").expect("shift parses");
    let res = r.cua.click_in_window(
        app,
        cua_core::WindowPixel {
            window_id: window_id(app, &r),
            x: lx + 200.0,
            y: ly,
            snapshot_id: Some(r.snapshot_id),
        },
        shift,
        false,
    );
    match res {
        Ok(res) => println!("shift click = {}", res.verb),
        Err(e) => println!("shift click FAILED = {e}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let selected = text.string(attr::SELECTED_TEXT).unwrap_or_default();
    println!("selected after shift = {selected:?}");
    verdict("shift-click extends a selection", !selected.is_empty());
}

fn window_id(app: &str, _r: &Read) -> u32 {
    let cua = Cua::new();
    cua.get_app_state(
        app,
        StateOptions {
            include_screenshot: false,
            ..Default::default()
        },
    )
    .ok()
    .and_then(|s| s.window_id)
    .expect("no verified window id; Screen Recording is required for window-local clicks")
}

// ── right click ──────────────────────────────────────────────────────────────

/// A right click on a control must open a context menu.
///
/// Detected as a new above-ordinary-layer window of the same pid, because that
/// is what a macOS popup menu is and accessibility does not publish it.
fn right_click(app: &str, index: usize) {
    let r = read(app);
    println!("popups before = {:?}", popups(r.pid));

    let right = MouseOptions::parse("right", "").expect("right parses");
    let res = r.cua.click(
        app,
        Target::Index {
            index,
            snapshot_id: Some(r.snapshot_id),
            expected_role: None,
        },
        right,
        false,
        false,
    );
    match res {
        Ok(res) => println!("right click = {}", res.verb),
        Err(e) => {
            println!("right click FAILED = {e}");
            return;
        }
    }

    // Poll: a menu appears within a frame or two, and this is also how its
    // lifetime gets recorded if it turns out to be short.
    let t0 = std::time::Instant::now();
    let mut seen: Vec<String> = vec![];
    while t0.elapsed() < std::time::Duration::from_millis(1500) {
        let now = popups(r.pid);
        if now != seen {
            println!("+{:>5}ms {:?}", t0.elapsed().as_millis(), now);
            seen = now;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    verdict("right-click opens a context menu", !seen.is_empty());
}

// ── wheel ────────────────────────────────────────────────────────────────────

/// A wheel scroll must move content that has no AX scroll verb.
///
/// Signal: the scroll bar's own `AXValue`, which the app recomputes from its
/// content offset. The tier that was used is printed too — the point of the arm
/// is a `delivery: pid` that still moved something.
fn wheel(app: &str, index: usize) {
    let r = read(app);
    let bar = find_role(r.pid, "AXScrollBar");
    let before = bar.as_ref().and_then(|b| b.string(attr::VALUE));
    println!("scrollbar before = {before:?}");
    let tree_before = r.tree.clone();

    // `WHEEL_AT=x,y` aims at an explicit screen point instead of the element's
    // own. Needed because a scrollable element's frame is not its viewport: a
    // web area's frame is the whole document, so the point derived from it can
    // land outside the window entirely.
    let target = match std::env::var("WHEEL_AT").ok().and_then(|v| {
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
    }) {
        Some((x, y)) => {
            println!("aiming at explicit screen point ({x}, {y})");
            Target::Point {
                x,
                y,
                snapshot_id: Some(r.snapshot_id),
            }
        }
        None => Target::Index {
            index,
            snapshot_id: Some(r.snapshot_id),
            expected_role: None,
        },
    };

    let res = r.cua.scroll(
        app,
        target,
        ScrollDir::Down,
        ScrollAmount::Points(300),
        false,
    );
    match res {
        Ok(res) => println!("scroll = {}  delivery={:?}", res.verb, res.delivery),
        Err(e) => {
            println!("scroll FAILED = {e}");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    let after = bar.as_ref().and_then(|b| b.string(attr::VALUE));
    println!("scrollbar after  = {after:?}");

    // Two signals, because not every scrollable surface publishes a scroll bar:
    // web content generally does not, and then the only readable evidence is
    // that the tree now describes different content.
    let tree_after = read(app).tree;
    let bar_moved = before.is_some() && before != after;
    let tree_moved = tree_after != tree_before;
    println!(
        "scrollbar moved = {bar_moved}, tree changed = {tree_moved} ({} -> {} lines)",
        tree_before.lines().count(),
        tree_after.lines().count()
    );
    verdict("a wheel event moves the content", bar_moved || tree_moved);
}

// ── hover ────────────────────────────────────────────────────────────────────

/// A hover must reveal UI that was not in the tree before.
///
/// Two signals, because hover-only UI shows up in different places depending on
/// the app: a change in the rendered tree, and any new above-ordinary-layer
/// window (a tooltip is often its own window).
fn hover(app: &str, index: usize) {
    let r = read(app);
    let before_lines = r.tree.lines().count();
    println!(
        "tree lines before = {before_lines}, popups {:?}",
        popups(r.pid)
    );

    let res = r.cua.hover(
        app,
        PointerLocation::Element(Target::Index {
            index,
            snapshot_id: Some(r.snapshot_id),
            expected_role: None,
        }),
        Modifiers::empty(),
        Some(r.snapshot_id),
        false,
    );
    match res {
        Ok(res) => println!("hover = {}", res.verb),
        Err(e) => {
            println!("hover FAILED = {e}");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(900));

    let after = read(app);
    let after_lines = after.tree.lines().count();
    let after_popups = popups(r.pid);
    println!("tree lines after  = {after_lines}, popups {after_popups:?}");
    verdict(
        "hover reveals something readable",
        after_lines != before_lines || !after_popups.is_empty(),
    );
}

fn verdict(what: &str, ok: bool) {
    println!(
        "\n==> {}: {}",
        what,
        if ok { "CONFIRMED" } else { "NOT SEEN" }
    );
}
