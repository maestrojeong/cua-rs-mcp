//! Does a pid-routed `mouseMoved` make a real app believe the pointer arrived?
//!
//! ```console
//! $ cargo run -p cua-core --example hover_check -- tree   "Google Chrome"
//! $ cargo run -p cua-core --example hover_check -- idle   "Google Chrome"
//! $ cargo run -p cua-core --example hover_check -- sweep  "Google Chrome" 45
//! $ cargo run -p cua-core --example hover_check -- versus Finder 92
//! $ cargo run -p cua-core --example hover_check -- sweep  Finder 18,14   # a bare pixel
//! ```
//!
//! `versus` is the arm to reach for first: it sends a click and a hover at the
//! same pixel of the same window in the same run, and a click is already
//! measured to work, so a silent hover next to a working click means the event
//! type failed, while two silent arms mean the run proves nothing. `sweep` then
//! characterises a hover that does land. `assets/hover_fixture.html` is the bed
//! the recorded measurements were taken against.
//!
//! # Why a bracketed sweep and not a single hover
//!
//! `hover` has two possible failures and they call for opposite conclusions. The
//! event may be ignored, in which case the capability does not exist; or the app
//! may respond on screen without ever publishing the response to accessibility,
//! in which case the event lands and only the *observation* is missing. A single
//! before/after read cannot tell those apart, so this probe reads two signals:
//!
//! 1. **the tree** — `get_app_state`'s rendered outline. A hover-revealed
//!    `AXButton`, or a label the app rewrote from the event, shows up here, and
//!    that is the signal an agent can actually act on. This is the one that
//!    matters.
//! 2. **the window's own pixels** — the tiebreaker. If the image changes and the
//!    tree does not, the event arrived and the app simply drew its answer without
//!    telling accessibility. That is a completely different result from a hover
//!    nothing happened to.
//!
//! Neither signal is trustworthy on its own, because both drift. So the `sweep`
//! arm hovers a deliberately dull point, *then* the target, *then* the dull point
//! again, and only counts what is present at the target and absent at both
//! neighbouring stops. A single before/after pair credits the hover with anything
//! that happened to change in the same second — a Finder status bar recomputing
//! free disk space from `582.6GB` to `582.59GB` was the false positive that
//! forced the bracket — while drift does not return to where it started.
//!
//! The `idle` arm is the control for signal 2 and reports whether this window
//! repaints with nothing sent to it at all. A window that fails `idle` cannot
//! produce a pixel verdict from any arm.

use cua_core::{Cua, Modifiers, PointerLocation, StateOptions, Target};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arm = a.get(1).map(String::as_str).unwrap_or("help");
    let app = a.get(2).cloned().unwrap_or_else(|| "Google Chrome".into());

    if !cua_ax::is_trusted() {
        eprintln!("no Accessibility grant for this host process");
        std::process::exit(1);
    }

    match arm {
        "tree" => tree(&app),
        "idle" => idle(&app),
        "sweep" => match a.get(3).and_then(|s| Aim::parse(s)) {
            Some(aim) => sweep(&app, &aim),
            None => eprintln!("usage: hover_check sweep <app> <element_index|x,y>"),
        },
        "versus" => match a.get(3).and_then(|s| Aim::parse(s)) {
            Some(aim) => versus(&app, &aim),
            None => eprintln!("usage: hover_check versus <app> <element_index|x,y>"),
        },
        _ => eprintln!("usage: hover_check <tree|idle|sweep|versus> <app> [element_index|x,y]"),
    }
}

/// One read of the app: the outline every arm compares, plus the window handle
/// the pixels come from.
struct Read {
    snapshot_id: u64,
    window_id: u32,
    /// Screen origin of that window, so a resolved screen point can be turned
    /// back into the window-local one every later stop re-uses.
    origin: (f64, f64),
    tree: String,
}

fn read(cua: &Cua, app: &str) -> Read {
    let st = cua
        .get_app_state(
            app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    let frame = st.window_frame.expect("no window frame");
    Read {
        snapshot_id: st.snapshot_id,
        window_id: st.window_id.expect("no verified window id"),
        origin: (frame.origin.x, frame.origin.y),
        tree: st.tree,
    }
}

fn shoot(window_id: u32) -> Vec<u8> {
    cua_capture::capture_window(window_id, 900)
        .map(|s| s.png)
        .unwrap_or_default()
}

/// Element lines of the outline, so an index can be picked for `sweep`.
fn tree(app: &str) {
    let cua = Cua::new();
    let r = read(&cua, app);
    println!("window_id={} snapshot={}", r.window_id, r.snapshot_id);
    for line in r.tree.lines().filter(|l| l.trim_start().starts_with('[')) {
        println!("{line}");
    }
}

/// Is this window's image stable when nothing is sent to it?
///
/// Without this the pixel tiebreaker is unusable: a window with a blinking
/// caret, a spinner or a clock in it produces a byte difference for every arm,
/// including the ones that did nothing.
fn idle(app: &str) {
    let cua = Cua::new();
    let r = read(&cua, app);
    let first = shoot(r.window_id);
    std::thread::sleep(std::time::Duration::from_millis(900));
    let second = shoot(r.window_id);
    let after_tree = read(&cua, app).tree;

    let stable = !first.is_empty() && first == second;
    println!("idle pixels {} -> {} bytes", first.len(), second.len());
    println!(
        "idle: pixels stable = {stable}, tree stable = {}",
        after_tree == r.tree
    );
    println!(
        "\n==> the pixel tiebreaker is usable on this window: {}",
        if stable {
            "YES"
        } else {
            "NO — this window repaints on its own, so a byte difference means nothing"
        }
    );
}

/// Where a hover in this probe is aimed: an element of the last snapshot, or a
/// bare window-local pixel, for UI accessibility does not publish at all.
enum Aim {
    Index(usize),
    Pixel(f64, f64),
}

impl Aim {
    /// `42` is an element index; `18,14` is a window-local point.
    fn parse(s: &str) -> Option<Aim> {
        match s.split_once(',') {
            Some((x, y)) => Some(Aim::Pixel(x.trim().parse().ok()?, y.trim().parse().ok()?)),
            None => Some(Aim::Index(s.trim().parse().ok()?)),
        }
    }

    fn location(&self, snapshot_id: u64) -> PointerLocation {
        match *self {
            Aim::Index(index) => PointerLocation::Element(Target::Index {
                index,
                snapshot_id: Some(snapshot_id),
                expected_role: None,
            }),
            Aim::Pixel(x, y) => PointerLocation::WindowPoint { x, y },
        }
    }
}

/// Hover a dull point, then the target, then the dull point again, reading both
/// signals at each of the three stops.
///
/// `HOVER_AWAY=x,y` moves the dull point. The default is near the window's own
/// top-left corner, which is chrome on most apps and therefore quiet; it is the
/// wrong default when the thing being probed is itself in that corner.
fn sweep(app: &str, aim: &Aim) {
    let cua = Cua::new();
    let base = read(&cua, app);
    let away = away_point();
    println!(
        "window_id={} snapshot={} away point = window-local ({:.0},{:.0})",
        base.window_id, base.snapshot_id, away.0, away.1
    );

    // Every stop reads the app, and every read bumps the snapshot generation
    // that element indices are numbered against — measured on Chrome, where the
    // index that was a page element in snapshot 1 was a browser tab in snapshot
    // 2 and the hover was refused for leaving the window. So an index is
    // resolved exactly once, on the first stop, and the *screen point* it
    // resolved to is what the rest of the sweep re-uses. A point is not
    // generational; it is the same place at every snapshot.
    let stop = |what: &str, at: PointerLocation| -> Option<(Vec<u8>, Read, (f64, f64))> {
        let point = match cua.hover(app, at, Modifiers::empty(), None, false) {
            Ok(res) => {
                println!("hover {what} = {}", res.verb);
                point_in(&res.verb).expect("a delivered hover names its point in the verb")
            }
            Err(e) => {
                println!("hover {what} FAILED = {e}");
                return None;
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(900));
        let px = shoot(base.window_id);
        Some((px, read(&cua, app), point))
    };
    let away_at = || PointerLocation::WindowPoint {
        x: away.0,
        y: away.1,
    };

    // A first, uncounted visit, only to find out where the target is.
    let Some((.., point)) = stop("target (resolving)", aim.location(base.snapshot_id)) else {
        return;
    };
    let on_at = || PointerLocation::WindowPoint {
        x: point.0 - base.origin.0,
        y: point.1 - base.origin.1,
    };
    println!(
        "target resolved to screen ({:.0},{:.0}) = window-local ({:.0},{:.0})",
        point.0,
        point.1,
        point.0 - base.origin.0,
        point.1 - base.origin.1
    );

    let Some((a0_px, a0, _)) = stop("away (before)", away_at()) else {
        return;
    };
    let Some((on_px, on, _)) = stop("target", on_at()) else {
        return;
    };
    let Some((a1_px, a1, _)) = stop("away (after)", away_at()) else {
        return;
    };
    let (a0_tree, on_tree, a1_tree) = (a0.tree, on.tree, a1.tree);

    report_tree("arriving on the target", &a0_tree, &on_tree);
    report_tree("leaving the target", &on_tree, &a1_tree);
    println!(
        "pixels: away {} -> target {} -> away {} bytes",
        a0_px.len(),
        on_px.len(),
        a1_px.len()
    );

    // Only lines the target has and *neither* away stop does. Anything else is
    // either permanent or drifting, and neither of those is a hover.
    let only_on_target: Vec<&str> = on_tree
        .lines()
        .map(str::trim)
        .filter(|l| !a0_tree.contains(*l) && !a1_tree.contains(*l))
        .collect();
    let usable = !a0_px.is_empty();
    let away_agrees = usable && a0_px == a1_px;
    let px_moved = usable && a0_px != on_px;

    println!("\nlines present only while the target was hovered: {only_on_target:#?}");
    println!(
        "pixels differed at the target = {px_moved}, the two away stops agree = {away_agrees}"
    );
    println!(
        "\n==> {}",
        match (
            !only_on_target.is_empty(),
            away_agrees && px_moved,
            px_moved
        ) {
            (true, _, _) =>
                "the hover is VISIBLE IN THE TREE — an agent can read what it revealed. Check the \
                 lines above are a hover state and not the app's own drift",
            (false, true, _) =>
                "the event LANDED and the tree cannot see it: the app drew its hover state without \
                 publishing it. The pixels differed at the target and the two away stops agree, \
                 which drift does not do",
            (false, false, true) =>
                "INCONCLUSIVE. The pixels moved but the two away stops disagree, so this window is \
                 changing on its own — run `idle`, and pick a quieter window",
            (false, false, false) =>
                "NOTHING OBSERVED. Either the event was ignored or this app neither draws nor \
                 publishes a hover state at this point — confirm the point visibly changes under a \
                 real pointer before concluding anything about the event",
        }
    );
}

/// A click and a hover at the same pixel of the same window in the same run.
///
/// This is the control arm, and it is what makes a silent hover mean anything.
/// A hover that reveals nothing has two explanations that call for opposite
/// conclusions: the `mouseMoved` event is not something this app acts on, or
/// nothing at all is reaching the app right now — wrong window number, an aim
/// the window no longer covers, a target that has stopped drawing. A click is
/// the same delivery path with a different event type (DESIGN §6, §11), and it
/// is measured to work, so sending one at the same pixel splits the two:
///
/// - click lands, hover does not → the event *type* is what fails. Routing, the
///   window number, the aim and the instrument are all ruled out, exactly as the
///   `key` arm of `scroll_check` rules them out for the wheel tier.
/// - neither lands → this run says nothing about `hover`. Fix the bed first.
/// - both land → both work here.
///
/// The click is a real press, so aim it at something harmless. The bundled
/// fixture page is a body with no click handler beyond a counter for exactly
/// this reason.
fn versus(app: &str, aim: &Aim) {
    let cua = Cua::new();
    let base = read(&cua, app);
    println!("window_id={} snapshot={}", base.window_id, base.snapshot_id);

    // Resolving the point does not depend on the event landing: `hover` reports
    // where it aimed whether or not the app reacted.
    let resolved = match cua.hover(
        app,
        aim.location(base.snapshot_id),
        Modifiers::empty(),
        None,
        false,
    ) {
        Ok(res) => point_in(&res.verb).expect("a delivered hover names its point in the verb"),
        Err(e) => {
            println!("could not resolve the target: {e}");
            return;
        }
    };
    let (lx, ly) = (resolved.0 - base.origin.0, resolved.1 - base.origin.1);
    println!("both arms aim at window-local ({lx:.0},{ly:.0})");
    std::thread::sleep(std::time::Duration::from_millis(600));

    // The same stability check the `idle` arm runs, inline, because the pixel
    // half of this arm is worthless without it: TextEdit's blinking caret alone
    // moves the capture between 281388 and 281791 bytes with nothing sent to it,
    // which is larger than the difference a hover state makes.
    let settle = shoot(base.window_id);
    std::thread::sleep(std::time::Duration::from_millis(900));
    let before_px = shoot(base.window_id);
    let px_usable = !settle.is_empty() && settle == before_px;
    println!(
        "pixel signal usable = {px_usable} (idle {} -> {} bytes)",
        settle.len(),
        before_px.len()
    );
    let before = read(&cua, app);

    let click = cua.click_in_window(
        app,
        cua_core::WindowPixel {
            window_id: base.window_id,
            x: lx,
            y: ly,
            snapshot_id: None,
        },
        cua_core::MouseOptions::default(),
        false,
    );
    println!("click arm = {:?}", click.map(|r| r.verb));
    std::thread::sleep(std::time::Duration::from_millis(800));
    let after_click = read(&cua, app);
    let after_click_px = shoot(base.window_id);

    let hover = cua.hover(
        app,
        PointerLocation::WindowPoint { x: lx, y: ly },
        Modifiers::empty(),
        None,
        false,
    );
    println!("hover arm = {:?}", hover.map(|r| r.verb));
    std::thread::sleep(std::time::Duration::from_millis(800));
    let after_hover = read(&cua, app);
    let after_hover_px = shoot(base.window_id);

    report_tree("after the click", &before.tree, &after_click.tree);
    report_tree("after the hover", &after_click.tree, &after_hover.tree);
    // Pixels as well as the tree, because a native hover state is very often
    // drawn and not published — that is the whole reason `sweep` carries a
    // tiebreaker, and an arm that only watched the tree would call such an app
    // "did not land" when it did.
    println!(
        "pixels: before {} -> after the click {} -> after the hover {} bytes",
        before_px.len(),
        after_click_px.len(),
        after_hover_px.len()
    );
    let click_landed =
        before.tree != after_click.tree || (px_usable && before_px != after_click_px);
    let hover_landed =
        after_click.tree != after_hover.tree || (px_usable && after_click_px != after_hover_px);
    println!(
        "\n==> {}",
        match (click_landed, hover_landed) {
            (true, true) =>
                "BOTH LAND. A pid-routed click and a pid-routed mouseMoved both drive \
                             this app at this pixel",
            (true, false) =>
                "the CLICK LANDS AND THE HOVER DOES NOT. Same pixel, same window, \
                              same run, so the event type is the only thing left — this app does \
                              not act on a synthesized mouseMoved",
            (false, true) =>
                "only the hover moved the tree, which is not a shape this probe \
                              expects — read the diffs above before believing either arm",
            (false, false) =>
                "NEITHER ARM LANDS, so this run says nothing about hover. The bed is \
                               wrong: check the window is the one on screen, that the point is \
                               over something that reacts, and that the app is running",
        }
    );
}

/// The screen point a hover verb names, e.g. `… mouseMoved to (756, 224) — …`.
///
/// Read out of the sentence rather than out of the result, because
/// `ActionResult::point` is deliberately crate-private: it feeds the cursor
/// overlay and is not part of anything callers are promised. A probe that wants
/// it can pay the price of parsing the line the tool already prints.
fn point_in(verb: &str) -> Option<(f64, f64)> {
    let (_, rest) = verb.split_once('(')?;
    let (inside, _) = rest.split_once(')')?;
    let (x, y) = inside.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// The dull point every sweep returns to, in window-local coordinates.
fn away_point() -> (f64, f64) {
    std::env::var("HOVER_AWAY")
        .ok()
        .and_then(|v| {
            let (x, y) = v.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        })
        .unwrap_or((6.0, 6.0))
}

/// Lines that appeared and disappeared between two outlines.
///
/// Printed rather than counted because a hover that swaps one label for another
/// leaves the line count identical, and the line count is what the first version
/// of this check looked at.
fn report_tree(what: &str, before: &str, after: &str) {
    let gone: Vec<&str> = before.lines().filter(|l| !after.contains(*l)).collect();
    let new: Vec<&str> = after.lines().filter(|l| !before.contains(*l)).collect();
    println!(
        "tree {what}: {} lines -> {} lines, {} gone, {} new",
        before.lines().count(),
        after.lines().count(),
        gone.len(),
        new.len()
    );
    for line in new.iter().take(12) {
        println!("  + {}", line.trim());
    }
    for line in gone.iter().take(12) {
        println!("  - {}", line.trim());
    }
}
