//! How long does a transient pop-up live after a pid-routed click, does the
//! click's own result say it opened — and can anything reach a row inside it?
//!
//! ```console
//! # lifetime only, the original arm
//! menu_life KakaoTalk --index 7
//!
//! # open TextEdit's context menu and walk it with the arrow keys
//! menu_life TextEdit --right 300,120 --keys down,down,right --watch AXTextArea
//! ```
//!
//! Three things are printed that a test cannot assert without a grant and a
//! running app: what `ActionResult::popups` reported in the same call that did
//! the clicking, the pop-up's lifetime polled directly against the window
//! server so the report can be checked rather than trusted, and — after *every*
//! chord `--keys` sends — a read-back triple that can tell "the item activated"
//! apart from "the menu just closed".
//!
//! That distinction is the whole point of the `--keys` arm. DESIGN §10's
//! measurement 5 is exactly the case where activation and dismissal look
//! identical from outside, so "the menu is gone" is never accepted as evidence
//! here. What is accepted:
//!
//! - **the pop-up set**, printed after each key. A *second* level-101 window
//!   appearing is a submenu opening, which nothing but the menu's own tracking
//!   loop can do; one vanishing is dismissal.
//! - **window levels**, printed as a multiset. An always-on-top toggle moves a
//!   window from level 0 to level 3 without opening or closing anything.
//! - **`--watch <AXRole>`**, the `AXValue` of the first element of that role in
//!   the app's focused window. TextEdit's Transformations ▸ Make Upper Case
//!   rewrites the document, which is a signal no dismissal can forge.
//!
//! Measured on KakaoTalk's chat-room hamburger (`[7]`): a level-101 window,
//! 202x318, present at +50 ms and still present 2.5 s later. Not a race.
use std::time::{Duration, Instant};

use cua_ax::{attr, Element};

fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let info = cua_core::apps::resolve_app(&args.app).expect("resolve");
    println!("target {} pid {}", info.name, info.pid);
    // Printed because it is the claim under test: none of this needs the target
    // to be frontmost, so this should name some other process throughout.
    println!("frontmost pid = {:?}", cua_core::frontmost_pid());

    let cua = cua_core::Cua::new();
    let state = cua
        .get_app_state(
            &args.app,
            cua_core::StateOptions {
                include_screenshot: false,
                ..Default::default()
            },
        )
        .expect("get_app_state");
    println!("window = {:?} id={:?}", state.window_title, state.window_id);
    println!("popups before = {}", render(&state.popups));
    if args.tree {
        println!("{}", state.tree);
        // Every window of the pid, unfiltered — the raw material
        // `is_transient_popup()` is a predicate over. Printed because a menu
        // this probe cannot see is indistinguishable from a menu that did not
        // open, and those are different failures.
        for w in cua_capture::list_windows().unwrap_or_default() {
            if w.pid == info.pid {
                println!(
                    "  window id={} layer={} on_screen={} {:.0},{:.0} {:.0}x{:.0} {:?}",
                    w.id,
                    w.layer,
                    w.on_screen,
                    w.frame.origin.x,
                    w.frame.origin.y,
                    w.frame.size.width,
                    w.frame.size.height,
                    w.title
                );
            }
        }
    }

    if let Some(needle) = &args.select {
        let picked = cua.select_text(
            &args.app,
            cua_core::Target::Index {
                index: args.key_target,
                snapshot_id: Some(state.snapshot_id),
                expected_role: None,
            },
            needle,
            None,
            None,
            false,
        );
        println!("select {needle:?} -> {:?}", picked.map(|r| r.verb));
    }

    if args.show_menu {
        let el = cua_ax::Element::for_pid(info.pid);
        let shown = cua.perform_action(
            &args.app,
            cua_core::Target::Index {
                index: args.key_target,
                snapshot_id: Some(state.snapshot_id),
                expected_role: None,
            },
            cua_ax::action::SHOW_MENU,
            false,
            false,
        );
        println!("AXShowMenu -> {:?}", shown.map(|r| r.verb));
        std::thread::sleep(Duration::from_millis(400));
        println!("popups now = {:?}", live_popups(info.pid));
        // The question this arm exists for: DESIGN §10 measurement 2 found the
        // application element's only children were its two menu bars. If a menu
        // opened *through* accessibility is published, an `AXMenu` shows up here.
        println!(
            "application children = {:?}",
            el.children()
                .iter()
                .map(|c| c.role().unwrap_or_default())
                .collect::<Vec<_>>()
        );
        return;
    }

    // Candidate: the app's own menu bar, which accessibility *does* publish.
    // A pop-up row that is duplicated there is reachable the ordinary way, and
    // this arm is what says whether "the ordinary way" works from the
    // background at all.
    if let Some(path) = &args.menu_bar {
        // Through the shipped API, not a copy of it: this arm is the live check
        // on `Cua::menu_bar` / `Cua::press_menu_bar`, and a probe that walked
        // the tree itself could pass while the tool was broken.
        match cua.menu_bar(&args.app, path) {
            Ok(listing) => {
                println!("menu `{}`", listing.path);
                for item in &listing.items {
                    println!(
                        "  {:?} enabled={} submenu={} shortcut={:?} mark={:?}",
                        item.title, item.enabled, item.has_submenu, item.shortcut, item.mark
                    );
                }
            }
            Err(e) => println!("menu_bar -> ERROR {e}"),
        }
        if args.press {
            println!(
                "press -> {:?}",
                cua.press_menu_bar(&args.app, path, false, args.confirm)
                    .map(|r| format!("{} ui_changed={}", r.verb, r.ui_changed.as_str()))
                    .map_err(|e| e.to_string())
            );
        }
        return;
    }

    let probe = Probe {
        pid: info.pid,
        watch: args.watch.clone(),
    };
    println!("readback before = {}", probe.read());

    let t0 = Instant::now();
    let result = match args.open {
        Open::Index(index) => Some(cua.click(
            &args.app,
            cua_core::Target::Index {
                index,
                snapshot_id: Some(state.snapshot_id),
                expected_role: None,
            },
            mouse(args.button),
            false,
            false,
        )),
        Open::WindowPixel(x, y) => Some(cua.click_in_window(
            &args.app,
            cua_core::WindowPixel {
                window_id: state.window_id.expect("no verified window id"),
                x,
                y,
                snapshot_id: Some(state.snapshot_id),
            },
            mouse(args.button),
            false,
        )),
        Open::Nothing => {
            println!("no open arm; assuming a menu is already up");
            None
        }
    };
    match &result {
        Some(Ok(r)) => {
            println!("open at +{:?} -> {}", t0.elapsed(), r.verb);
            // The point of the whole feature: the action that opened the menu
            // says so itself, without a second round trip.
            println!("  ui_changed = {}", r.ui_changed.as_str());
            println!("  popups reported by the click = {}", render(&r.popups));
        }
        Some(Err(e)) => println!("open at +{:?} -> ERROR {e}", t0.elapsed()),
        None => {}
    }

    if args.keys.is_empty() {
        watch_lifetime(info.pid, t0, args.hold);
        return;
    }

    // The `--keys` arm. Wait for the pop-up to actually be up before pressing
    // anything: a chord that arrives before the menu exists is a keystroke sent
    // to the plain window, and it would answer a different question.
    let opened = wait_for_popup(info.pid, Duration::from_millis(1500));
    println!(
        "+{:>5}ms  menu up = {} {:?}",
        t0.elapsed().as_millis(),
        !opened.is_empty(),
        opened
    );
    if opened.is_empty() && !args.force {
        println!("no pop-up to send keys into; stopping rather than typing at the window");
        println!("(--force sends them anyway, which is the control this arm needs: the same");
        println!(" chords with no menu up, to show the route reaches the window at all)");
        return;
    }

    for key in &args.keys {
        let stamped = args.in_window.map(|w| match w {
            Window::Popup => live_popups(info.pid).first().map(|p| p.0).unwrap_or(0) as isize,
            Window::Parent => state.window_id.unwrap_or(0) as isize,
            Window::None => 0,
            Window::Id(id) => id as isize,
        });
        let sent = if let Some(window_number) = stamped {
            cua_hid::parse_chord(key)
                .and_then(|c| cua_hid::press_chord_in_window_pid(info.pid, window_number, &c))
                .map(|()| format!("AppKit key event for window {window_number}"))
                .map_err(|e| e.to_string())
        } else if args.raw {
            // Straight through cua-hid: no element resolution, no `AXFocused`
            // write, no activation notice. `press_key` primes a *element* in a
            // window, and priming might well be what a menu's tracking loop
            // objects to, so the two routes are measured separately.
            cua_hid::parse_chord(key)
                .and_then(|c| cua_hid::press_chord_background_pid(info.pid, &c))
                .map(|()| "raw pid post".to_string())
                .map_err(|e| e.to_string())
        } else {
            cua.press_key(
                &args.app,
                cua_core::Target::Index {
                    index: args.key_target,
                    snapshot_id: Some(state.snapshot_id),
                    expected_role: None,
                },
                key,
                false,
                false,
            )
            .map(|r| format!("{} focus={:?}", r.verb, r.focus.map(|f| f.state.as_str())))
            .map_err(|e| e.to_string())
        };
        std::thread::sleep(Duration::from_millis(args.gap));
        println!(
            "+{:>5}ms  key {key:<8} -> {}",
            t0.elapsed().as_millis(),
            match &sent {
                Ok(s) => s.clone(),
                Err(e) => format!("ERROR {e}"),
            }
        );
        println!("            popups   {:?}", live_popups(info.pid));
        println!("            readback {}", probe.read());
    }

    println!("final popups: {:?}", live_popups(info.pid));
    println!("readback after = {}", probe.read());
}

const USAGE: &str = "usage: menu_life <app> [--index N | --right X,Y] [--keys k1,k2,...]\n\
     \x20            [--button left|right] [--watch AXRole] [--key-target N]\n\
     \x20            [--raw] [--in-window popup|parent|none|N] [--force] [--gap MS] [--hold MS] [--tree]\n\x20             [--menu-bar 'Edit>Paste' [--press] [--confirm]]";

/// How the menu gets opened. Three arms because a pop-up has three plausible
/// origins and they are not interchangeable: a control that owns a menu, a
/// right-click on a view that has a context menu, and one a human opened.
enum Open {
    Index(usize),
    WindowPixel(f64, f64),
    Nothing,
}

/// Which window number `--in-window` should stamp onto a key event.
///
/// `none` is not a redundant spelling of leaving the flag off: it isolates the
/// two variables the flag changes at once — how the event is *built* (AppKit
/// factory versus `CGEventCreateKeyboardEvent`) and what window it *names*.
#[derive(Clone, Copy)]
enum Window {
    Popup,
    Parent,
    None,
    Id(u32),
}

struct Args {
    app: String,
    open: Open,
    button: cua_core::MouseButton,
    keys: Vec<String>,
    /// Element the `press_key` route addresses. Irrelevant to where the keys
    /// end up — a pid-routed keystroke carries no element — but `press_key`
    /// resolves one to focus and to report against, so it has to name something.
    key_target: usize,
    raw: bool,
    /// Route the chords through the AppKit key-event factory, stamped with a
    /// window number: the pop-up's own by default, or the one named here.
    in_window: Option<Window>,
    /// Send the chords even with no pop-up up — the control run.
    force: bool,
    watch: Option<String>,
    gap: u64,
    hold: u64,
    tree: bool,
    /// Perform `AXShowMenu` on `--key-target` instead of clicking it, then
    /// report whether *that* menu is any more visible to accessibility than a
    /// click-opened one.
    show_menu: bool,
    /// Substring to select in `--key-target` before anything else runs.
    ///
    /// A menu bar validates its items against the current responder, so
    /// "is this row enabled" is a different question with and without a
    /// selection, and the arm that presses one has to be able to set it up.
    select: Option<String>,
    /// `>`-separated titles to walk down the app's `AXMenuBar`, e.g.
    /// `편집>붙여넣기`. An empty leaf lists the level instead of pressing it.
    menu_bar: Option<String>,
    /// Press the row `--menu-bar` names, rather than only listing it.
    press: bool,
    /// Clear the destructive-label gate for that press.
    confirm: bool,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Self {
            app: String::new(),
            open: Open::Nothing,
            button: cua_core::MouseButton::Left,
            keys: vec![],
            key_target: 0,
            raw: false,
            in_window: None,
            force: false,
            watch: None,
            gap: 250,
            hold: 3000,
            tree: false,
            menu_bar: None,
            press: false,
            confirm: false,
            select: None,
            show_menu: false,
        };
        let mut positional: Vec<String> = vec![];
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            let mut value = || argv.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--index" => args.open = Open::Index(parse_num(&value()?)?),
                "--right" => {
                    let raw = value()?;
                    let (x, y) = raw.split_once(',').ok_or("--right wants X,Y")?;
                    args.open = Open::WindowPixel(parse_num(x)?, parse_num(y)?);
                    args.button = cua_core::MouseButton::Right;
                }
                "--button" => {
                    args.button =
                        cua_hid::MouseButton::parse(&value()?).map_err(|e| e.to_string())?
                }
                "--keys" => args.keys = value()?.split(',').map(str::to_string).collect(),
                "--key-target" => args.key_target = parse_num(&value()?)?,
                "--watch" => args.watch = Some(value()?),
                "--gap" => args.gap = parse_num(&value()?)?,
                "--hold" => args.hold = parse_num(&value()?)?,
                "--raw" => args.raw = true,
                "--in-window" => {
                    args.in_window = Some(match value()?.as_str() {
                        "popup" => Window::Popup,
                        "parent" => Window::Parent,
                        "none" => Window::None,
                        other => Window::Id(parse_num(other)?),
                    })
                }
                "--force" => args.force = true,
                "--tree" => args.tree = true,
                "--menu-bar" => args.menu_bar = Some(value()?),
                "--press" => args.press = true,
                "--confirm" => args.confirm = true,
                "--select" => args.select = Some(value()?),
                "--show-menu" => args.show_menu = true,
                other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
                other => positional.push(other.to_string()),
            }
        }
        // The original positional form, `menu_life <app> <index> [ms]`, still
        // works: this probe's earlier measurements are quoted in DESIGN §10 by
        // the command that produced them, and a command in the record that no
        // longer runs is worse than a slightly baroque parser.
        let mut positional = positional.into_iter();
        args.app = positional.next().ok_or("no app")?;
        if let Some(index) = positional.next() {
            args.open = Open::Index(parse_num(&index)?);
        }
        if let Some(ms) = positional.next() {
            args.hold = parse_num(&ms)?;
        }
        Ok(args)
    }
}

fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.trim().parse().map_err(|_| format!("bad number {s:?}"))
}

fn mouse(button: cua_core::MouseButton) -> cua_core::MouseOptions {
    cua_core::MouseOptions {
        button,
        ..Default::default()
    }
}

/// The three read-back signals, sampled together.
///
/// Together rather than separately because they are read as one line and a
/// menu moves fast; three calls spread over three prints would not describe one
/// moment.
struct Probe {
    pid: i32,
    watch: Option<String>,
}

impl Probe {
    fn read(&self) -> String {
        let app = Element::for_pid(self.pid);
        let mut levels: Vec<i64> = cua_capture::list_windows()
            .unwrap_or_default()
            .into_iter()
            // Every window of the pid, on screen or not: a window the human has
            // buried under a browser reports `isOnScreen() == false`, and its
            // level is exactly the signal an always-on-top toggle moves.
            .filter(|w| w.pid == self.pid)
            .map(|w| w.layer)
            .collect();
        levels.sort_unstable();
        let watched = match &self.watch {
            Some(role) => {
                let found = app
                    .element(attr::FOCUSED_WINDOW)
                    .or_else(|| app.element(attr::MAIN_WINDOW))
                    .or_else(|| app.elements(attr::WINDOWS).into_iter().next())
                    .and_then(|w| find_role(&w, role, 0));
                match found {
                    Some(el) => format!(" {role}={:?}", truncate(el.string(attr::VALUE))),
                    None => format!(" {role}=<not found>"),
                }
            }
            None => String::new(),
        };
        format!(
            "levels={levels:?} focused={:?}{watched}",
            app.element(attr::FOCUSED_UI_ELEMENT)
                .and_then(|el| el.role())
        )
    }
}

/// First descendant with this role, breadth-first within a depth budget.
///
/// Budgeted for the same reason every walk in this workspace is: an AX tree is
/// not guaranteed finite or acyclic.
fn find_role(root: &Element, role: &str, depth: usize) -> Option<Element> {
    if depth > 12 {
        return None;
    }
    let children = root.children();
    if let Some(hit) = children
        .iter()
        .find(|c| c.role().as_deref() == Some(role))
        .cloned()
    {
        return Some(hit);
    }
    children
        .into_iter()
        .find_map(|c| find_role(&c, role, depth + 1))
}

fn truncate(value: Option<String>) -> String {
    let value = value.unwrap_or_default().replace('\n', "\\n");
    match value.char_indices().nth(72) {
        Some((cut, _)) => format!("{}…", &value[..cut]),
        None => value,
    }
}

/// Poll until this pid has a transient pop-up, or the budget runs out.
fn wait_for_popup(pid: i32, budget: Duration) -> Vec<(u32, i64, String)> {
    let start = Instant::now();
    loop {
        let now = live_popups(pid);
        if !now.is_empty() || start.elapsed() > budget {
            return now;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The original arm: poll the window server and print every change to the set.
fn watch_lifetime(pid: i32, t0: Instant, budget: u64) {
    let mut last: Vec<u32> = vec![];
    while t0.elapsed() < Duration::from_millis(budget) {
        let now = live_popups(pid);
        let ids: Vec<u32> = now.iter().map(|w| w.0).collect();
        if ids != last {
            println!("+{:>5}ms  {:?}", t0.elapsed().as_millis(), now);
            last = ids;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("final: {:?}", live_popups(pid));
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

/// The same question asked of the window server rather than of cua-rs.
///
/// Every above-content window of the pid, each tagged with whether the shipped
/// `is_transient_popup()` predicate agrees it is a pop-up — so the two cannot
/// silently disagree, and so a menu the predicate drops is visible as a
/// disagreement rather than as an empty list. It does drop some: an app the
/// human has buried reports `isOnScreen() == false` on every window it owns,
/// its menus included.
fn live_popups(pid: i32) -> Vec<(u32, i64, String)> {
    cua_capture::list_windows()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid && w.layer > 0)
        .map(|w| {
            (
                w.id,
                w.layer,
                format!(
                    "{:?} {:.0}x{:.0} on_screen={} popup={}",
                    w.title,
                    w.frame.size.width,
                    w.frame.size.height,
                    w.on_screen,
                    w.is_transient_popup()
                ),
            )
        })
        .collect()
}
