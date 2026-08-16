//! How long does a transient pop-up live after a pid-routed click, and does the
//! click's own result say it opened?
//!
//! Usage: `menu_life <app> <element_index> [ms]`
//!
//! Two things are printed that a test cannot assert without a grant and a
//! running app: what `ActionResult::popups` reported in the same call that did
//! the clicking, and then the pop-up's lifetime, polled directly against the
//! window server so the report can be checked rather than trusted.
//!
//! Measured on KakaoTalk's chat-room hamburger (`[7]`): a level-101 window,
//! 202x318, present at +50 ms and still present 2.5 s later. Not a race.
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let app = a[1].clone();
    let index: usize = a[2].parse().expect("element index");
    let budget: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);

    let info = cua_core::apps::resolve_app(&app).expect("resolve");
    println!("target {} pid {}", info.name, info.pid);
    // Printed because it is the claim under test: none of this needs the target
    // to be frontmost, so this should name some other process throughout.
    println!("frontmost pid = {:?}", cua_core::frontmost_pid());

    let cua = cua_core::Cua::new();
    let state = cua
        .get_app_state(
            &app,
            cua_core::StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    println!("window = {:?} id={:?}", state.window_title, state.window_id);
    println!("popups before = {}", render(&state.popups));

    let t0 = Instant::now();
    let result = cua.click(
        &app,
        cua_core::Target::Index {
            index,
            snapshot_id: Some(state.snapshot_id),
            expected_role: None,
        },
        cua_core::MouseOptions::default(),
        false,
        false,
    );
    match &result {
        Ok(r) => {
            println!("click at +{:?} -> {}", t0.elapsed(), r.verb);
            // The point of the whole feature: the action that opened the menu
            // says so itself, without a second round trip.
            println!("  ui_changed = {}", r.ui_changed.as_str());
            println!("  popups reported by the click = {}", render(&r.popups));
        }
        Err(e) => println!("click at +{:?} -> ERROR {e}", t0.elapsed()),
    }

    // Now poll the window server directly, which is the independent check on
    // what the result above claimed.
    let mut last: Vec<u32> = vec![];
    while t0.elapsed() < Duration::from_millis(budget) {
        let now = live_popups(info.pid);
        let ids: Vec<u32> = now.iter().map(|w| w.0).collect();
        if ids != last {
            println!("+{:>5}ms  {:?}", t0.elapsed().as_millis(), now);
            last = ids;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("final: {:?}", live_popups(info.pid));
}

fn render(popups: &[cua_core::TransientWindow]) -> String {
    if popups.is_empty() {
        return "none".into();
    }
    popups
        .iter()
        .map(|p| {
            format!(
                "id={} layer={} {:.0},{:.0} {:.0}x{:.0} appeared={:?}",
                p.id,
                p.layer,
                p.frame.origin.x,
                p.frame.origin.y,
                p.frame.size.width,
                p.frame.size.height,
                p.appeared
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The same question asked of the window server rather than of cua-rs, using the
/// shipped predicate so the two cannot silently disagree.
fn live_popups(pid: i32) -> Vec<(u32, i64, String)> {
    cua_capture::list_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid && w.is_transient_popup())
        .map(|w| {
            (
                w.id,
                w.layer,
                format!(
                    "{:?} {:.0}x{:.0}",
                    w.title, w.frame.size.width, w.frame.size.height
                ),
            )
        })
        .collect()
}
