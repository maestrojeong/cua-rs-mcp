//! Can a shell `screencapture -l<id>` fail on a pop-up's window id while the
//! in-process path succeeds on the same id in the same moment?
//!
//! One observation said yes, once: `/usr/sbin/screencapture -x -o -l<menu_id>`
//! exited 1 with `could not create image from window` while every
//! `cua_capture::capture_window` call around it returned an image. It was never
//! chased, and §2 of DESIGN.md holds the honest note. This is the bounded attempt
//! to reproduce it.
//!
//! It opens a pop-up the only way one can be opened here — a pid-routed
//! right-click on an element, which §10 measured as producing a level-101 window
//! — then hammers both capture paths against that window's id, alternating, and
//! reports every exit status and error.
//!
//! `AXPress` on an `AXMenuBarItem` was tried first and is not usable for this:
//! the macOS menu bar only ever shows the *frontmost* app's menus, so pressing a
//! background app's menu-bar item publishes no window at all. Accessibility
//! describes that menu whether or not it is on screen (see the
//! `menu_item_press` probe), but there is nothing for the window server to
//! photograph.
//!
//! ```sh
//! cargo run -p cua-core --example popup_capture_probe -- Calculator Mode 20 left
//! cargo run -p cua-core --example popup_capture_probe -- TextEdit Document 20 right
//! ```
//!
//! Arguments: app name, a substring of the target element's label, optionally how
//! many rounds and which button. Each round is one shell capture and one
//! in-process capture of the same id. The target is named by label rather than by
//! index because the index has to be resolved against the same snapshot the click
//! cites, and a run that prints one numbering and clicks another is how this
//! probe first refused to start.
//!
//! Needs both grants on the launching process and a GUI session.

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(app_name), Some(label)) = (args.next(), args.next()) else {
        eprintln!("usage: popup_capture_probe <app-name> <element label> [rounds] [left|right]");
        std::process::exit(2);
    };
    let rounds: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(20);
    let button = args.next().unwrap_or_else(|| "left".to_string());
    let button = cua_core::MouseButton::parse(&button).expect("button");

    let info = cua_core::apps::resolve_app(&app_name).expect("resolve");
    let pid = info.pid;
    println!(
        "pid {pid}, frontmost pid {:?}, screen recording granted {}",
        cua_core::frontmost_pid(),
        cua_capture::has_screen_recording_permission()
    );

    let cua = cua_core::Cua::new();
    let state = cua
        .get_app_state(
            &app_name,
            cua_core::StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    println!("window {:?} id={:?}", state.window_title, state.window_id);
    let Some(index) = index_of(&state.tree, &label) else {
        eprintln!(
            "no actionable element whose line contains {label:?}:\n{}",
            state.tree
        );
        std::process::exit(1);
    };
    println!("target [{index}] from {label:?}");

    let before = popups(pid);
    let click = cua.click(
        &app_name,
        cua_core::Target::Index {
            index,
            snapshot_id: Some(state.snapshot_id),
            expected_role: None,
        },
        cua_core::MouseOptions {
            button,
            ..Default::default()
        },
        false,
        false,
    );
    match &click {
        Ok(r) => println!("click -> {} ({})", r.verb, r.delivery.as_str()),
        Err(e) => {
            eprintln!("click failed: {e}");
            std::process::exit(1);
        }
    }
    sleep(Duration::from_millis(400));
    let after = popups(pid);
    println!("transient windows before the click: {before:?}");
    println!("transient windows after:            {after:?}");

    let Some(menu) = after.iter().find(|w| !before.iter().any(|b| b.0 == w.0)) else {
        eprintln!("the click published no new transient window; nothing to capture");
        std::process::exit(1);
    };
    let (menu_id, level, ref frame) = *menu;
    println!("menu window id {menu_id}, level {level}, {frame}\n");

    // Both paths, alternating, against the same id. Alternating rather than
    // batched on purpose: the claim is that the two disagreed at the same moment,
    // and two runs of a hundred each would not be the same moment.
    let mut shell_ok = 0usize;
    let mut shell_refused = 0usize;
    let mut shell_other = 0usize;
    let mut in_process_ok = 0usize;
    let mut in_process_err = 0usize;
    let alive = |id: u32| popups(pid).iter().any(|w| w.0 == id);
    for round in 0..rounds {
        // Liveness is read between every step, because the failure text macOS
        // produces for "this window no longer exists" is the same string it
        // produces for "this window refuses to be photographed", and without
        // this the two are indistinguishable — which is how the original
        // observation came to be filed as the second one.
        let live_before = alive(menu_id);
        let (ok, note) = shell_capture(menu_id);
        let live_mid = alive(menu_id);
        if ok {
            shell_ok += 1;
        } else if note.contains("could not create image from window") {
            shell_refused += 1;
        } else {
            shell_other += 1;
        }

        let inproc = match cua_capture::capture_window(menu_id, 1400) {
            Ok(shot) => {
                in_process_ok += 1;
                format!("{} bytes, {:.2} px/pt", shot.png.len(), shot.scale)
            }
            Err(e) => {
                in_process_err += 1;
                format!("ERROR {e}")
            }
        };
        let live_after = alive(menu_id);
        println!(
            "round {round}: menu live {live_before}/{live_mid}/{live_after} | \
             shell {note} | in-process {inproc}"
        );

        // Confirm the menu is still up. A pop-up that closed halfway through
        // turns the rest of the run into a probe of a dead window id, which is a
        // different failure with the same message.
        if !live_after {
            println!("  ! the menu window is gone; stopping at round {round}");
            break;
        }
    }

    println!(
        "\nshell:      {shell_ok} ok, {shell_refused} \"could not create image from window\", \
         {shell_other} other failure"
    );
    println!("in-process: {in_process_ok} ok, {in_process_err} error");

    // Escape is what dismisses a pop-up (§10); it is the one thing measured to
    // reach one.
    let _ = cua.press_key(
        &app_name,
        cua_core::Target::Index {
            index,
            snapshot_id: Some(state.snapshot_id),
            expected_role: None,
        },
        "escape",
        false,
        false,
    );
    sleep(Duration::from_millis(300));
    let still_open = popups(pid).iter().any(|w| w.0 == menu_id);
    println!("menu still open at exit: {still_open}");

    // The control the original observation did not have: ask both paths for the
    // pop-up's id once it is definitely gone. If that produces the same message,
    // then "could not create image from window" on a pop-up id says nothing about
    // the window server refusing a live window.
    if !still_open {
        let (ok, note) = shell_capture(menu_id);
        println!("dead id, shell:      ok={ok} {note}");
        match cua_capture::capture_window(menu_id, 1400) {
            Ok(shot) => println!("dead id, in-process: ok, {} bytes", shot.png.len()),
            Err(e) => println!("dead id, in-process: ERROR {e}"),
        }
    }
}

/// The `[N]` an actionable line in the rendered tree carries, for the first line
/// mentioning `label`.
///
/// Parsing the render rather than walking the tree again on purpose: the render
/// is what a caller reads and what the indices are numbered for, so resolving
/// against it is resolving against the same thing the click will.
fn index_of(tree: &str, label: &str) -> Option<usize> {
    tree.lines().filter(|l| l.contains(label)).find_map(|l| {
        let start = l.find('[')?;
        let end = l.find(']')?;
        l.get(start + 1..end)?.parse().ok()
    })
}

/// The shell invocation from the original observation, verbatim in its flags.
///
/// `-x` silences the shutter, `-o` omits the window's shadow, `-l` names the
/// window id. The temporary file is read for its length and thrown away: the
/// question is whether the window server produced an image at all.
fn shell_capture(id: u32) -> (bool, String) {
    let path = std::env::temp_dir().join(format!("cua-popup-capture-{id}.png"));
    let out = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-o", &format!("-l{id}")])
        .arg(&path)
        .output();
    let result = match out {
        Ok(out) if out.status.success() => {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // `screencapture` can exit 0 and write nothing, which is a failure
            // wearing a success's exit status.
            if bytes > 0 {
                (true, format!("ok, {bytes} bytes"))
            } else {
                (false, "exit 0 but no file".to_string())
            }
        }
        Ok(out) => (
            false,
            format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ),
        Err(e) => (false, format!("spawn failed: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    result
}

/// This app's transient windows, through the shipped predicate so the probe and
/// the server cannot silently disagree about what counts as a pop-up.
fn popups(pid: i32) -> Vec<(u32, i64, String)> {
    cua_capture::list_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid && w.is_transient_popup())
        .map(|w| {
            (
                w.id,
                w.layer,
                format!("{:.0}x{:.0}", w.frame.size.width, w.frame.size.height),
            )
        })
        .collect()
}
