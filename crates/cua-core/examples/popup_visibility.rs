//! Does an occluded app report its own open pop-up as on screen?
//!
//! Asked because `is_transient_popup()` requires `isOnScreen()`, and a menu of a
//! buried app was once observed missing from `get_app_state`'s pop-up list while
//! apparently open — which would make the predicate wrong. It is not. The answer
//! depends on *how the menu was opened*, and both answers are correct.
//!
//! Measured on TextEdit with a terminal frontmost, same app, same moment:
//!
//! ```text
//! right click     id=49345 layer=101 on_screen=true  181x342 transient=true
//! menu bar press  id=49395 layer=101 on_screen=false 265x211 transient=false
//! ```
//!
//! A context menu belongs to the window it was opened over, so a background app
//! can present one and the window server says so. A menu *bar* menu belongs to
//! the **active** app's menu bar — pressing a background app's top-level item
//! creates the window but macOS never puts it on screen, because the menu bar on
//! screen is somebody else's. `on_screen=false` is the truth, and a predicate
//! that answers "did this app put something on screen" has to agree with it.
//!
//! Which also means nothing is lost: the shipped `menu_bar` tool presses the row
//! through accessibility and never opens a menu at all, so this window is not a
//! thing cua-rs produces on purpose.
//!
//! ```console
//! $ cargo run -p cua-core --example popup_visibility -- TextEdit 8
//! $ POPUP_VIA=menubar POPUP_MENU=View \
//!     cargo run -p cua-core --example popup_visibility -- TextEdit 8
//! ```
use cua_core::{apps, Cua, MouseOptions, StateOptions, Target};

fn main() {
    let app = std::env::args().nth(1).unwrap_or_else(|| "TextEdit".into());
    let idx: usize = std::env::args().nth(2).unwrap().parse().unwrap();
    let pid = apps::resolve_app(&app).expect("resolve").pid;
    let cua = Cua::new();
    let st = cua
        .get_app_state(
            &app,
            StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("state");

    println!(
        "frontmost pid = {:?}, target pid = {pid}",
        cua_core::frontmost_pid()
    );
    let dump =
        |when: &str| {
            for w in cua_capture::list_windows().unwrap_or_default() {
                if w.pid == pid && w.layer > 3 {
                    println!(
                    "  {when}: id={} layer={} on_screen={} {:.0}x{:.0} transient={} addressable={}",
                    w.id, w.layer, w.on_screen, w.frame.size.width, w.frame.size.height,
                    w.is_transient_popup(), w.is_addressable_target()
                );
                }
            }
        };
    dump("before");

    // `menubar` opens a menu the way the menu bar does -- AXPress on a
    // top-level item -- because that is the shape the on-screen doubt was
    // raised about, and a context menu is a different one.
    if std::env::var("POPUP_VIA").as_deref() == Ok("menubar") {
        let title = std::env::var("POPUP_MENU").unwrap_or_else(|_| "\u{ba85}\u{ba85}".into());
        let app_el = cua_ax::Element::for_pid(pid);
        let bar = app_el
            .element(cua_ax::attr::MENU_BAR)
            .expect("no AXMenuBar");
        let item = bar
            .children()
            .into_iter()
            .find(|c| c.label().as_deref() == Some(title.as_str()))
            .unwrap_or_else(|| panic!("no menu bar item titled {title:?}"));
        println!(
            "AXPress on menu bar item {title:?} = {:?}",
            item.perform(cua_ax::action::PRESS)
        );
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            dump("after");
        }
        let _ = cua.press_key(
            &app,
            Target::Index {
                index: idx,
                snapshot_id: None,
                expected_role: None,
            },
            "escape",
            false,
            false,
        );
        return;
    }

    let right = MouseOptions::parse("right", "").unwrap();
    let r = cua.click(
        &app,
        Target::Index {
            index: idx,
            snapshot_id: Some(st.snapshot_id),
            expected_role: None,
        },
        right,
        false,
        false,
    );
    println!("right click = {:?}", r.map(|x| x.verb));
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        dump("after");
    }
}
