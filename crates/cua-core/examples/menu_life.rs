//! How long does a transient popup live after a pid-routed click?
//! Usage: menu_life <app> <element_index> [ms]
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let app = a[1].clone();
    let index: usize = a[2].parse().expect("element index");
    let budget: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);

    let info = cua_core::apps::resolve_app(&app).expect("resolve");
    println!("target {} pid {}", info.name, info.pid);
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

    let transient = |pid: i32| -> Vec<(u32, i64, String)> {
        cua_capture::list_windows()
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.pid == pid && w.layer > 3)
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
    };

    println!("transient before: {:?}", transient(info.pid));

    let t0 = Instant::now();
    let r = cua.click(
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
    println!("click at +{:?} -> {:?}", t0.elapsed(), r.map(|r| r.verb));

    let mut last: Vec<u32> = vec![];
    while t0.elapsed() < Duration::from_millis(budget) {
        let now = transient(info.pid);
        let ids: Vec<u32> = now.iter().map(|w| w.0).collect();
        if ids != last {
            println!("+{:>5}ms  {:?}", t0.elapsed().as_millis(), now);
            last = ids;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("final: {:?}", transient(info.pid));
}
