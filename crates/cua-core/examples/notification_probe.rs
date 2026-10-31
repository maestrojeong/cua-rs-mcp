//! Where do notifications live in the accessibility tree, and can they be driven?
//!
//! Notification banners and the Notification Center history panel are drawn by
//! `/System/Library/CoreServices/NotificationCenter.app`, not by the app that
//! posted them. This probe walks that process's AX tree and prints every node
//! with its role, subrole, identifier, text, frame and advertised actions, so the
//! shape can be read off a real machine instead of assumed.
//!
//! Usage:
//!   notification_probe [dump]                    walk and print the whole tree
//!   notification_probe press <path> <action>     perform <action> on the node at
//!                                                <path> (dot-separated child
//!                                                indices, e.g. 0.0.1.2), then
//!                                                dump again
//!   notification_probe open-center              press the menu-bar clock (owned by
//!                                                ControlCenter) to open the history
//!                                                panel, then dump
//!   notification_probe watch <seconds>           re-walk every 250 ms and print
//!                                                only the number of notification
//!                                                groups, to time banner lifetime
//!
//! Needs Accessibility on the *launching* process (see DESIGN.md §7). `press`
//! sends a real AX action to a real notification — use test notifications posted
//! with `osascript -e 'display notification "x" with title "probe"'`.
use cua_ax::Element;

const MAX_DEPTH: usize = 14;

fn notification_center_pid() -> Option<i32> {
    pid_of("NotificationCenter")
}

fn pid_of(name: &str) -> Option<i32> {
    let out = std::process::Command::new("/usr/bin/pgrep")
        .args(["-x", name])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
}

fn describe(e: &Element) -> String {
    let role = e.role().unwrap_or_default();
    let mut s = role;
    if let Some(sr) = e.string("AXSubrole") {
        s.push(':');
        s.push_str(&sr);
    }
    if let Some(id) = e.string("AXIdentifier") {
        s.push_str(&format!(" id={id:?}"));
    }
    if let Some(t) = e.string("AXTitle").filter(|t| !t.is_empty()) {
        s.push_str(&format!(" title={t:?}"));
    }
    if let Some(d) = e.string("AXDescription").filter(|t| !t.is_empty()) {
        s.push_str(&format!(" desc={d:?}"));
    }
    if let Some(v) = e.value_string("AXValue").filter(|t| !t.is_empty()) {
        s.push_str(&format!(" value={v:?}"));
    }
    if let Some(h) = e.string("AXHelp").filter(|t| !t.is_empty()) {
        s.push_str(&format!(" help={h:?}"));
    }
    if let Some(f) = e.frame() {
        s.push_str(&format!(
            " frame=({:.0},{:.0} {:.0}x{:.0})",
            f.origin.x, f.origin.y, f.size.width, f.size.height
        ));
    }
    if std::env::var_os("PROBE_ATTRS").is_some()
        && e.string("AXSubrole")
            .is_some_and(|r| r.starts_with("AXNotificationCenter"))
    {
        let attrs: Vec<String> = e
            .attribute_names()
            .into_iter()
            .map(|a| match e.value_string(&a).or_else(|| e.string(&a)) {
                Some(v) => format!("{a}={v:?}"),
                None => a,
            })
            .collect();
        s.push_str(&format!(" attrs={attrs:?}"));
    }
    let actions = e.actions();
    if !actions.is_empty() {
        let shown: Vec<String> = actions.iter().map(|a| a.replace('\n', " | ")).collect();
        s.push_str(&format!(" actions={shown:?}"));
    }
    s
}

fn walk(e: &Element, path: &str, depth: usize, count: &mut usize) {
    *count += 1;
    println!("{:indent$}[{path}] {}", "", describe(e), indent = depth * 2);
    // The app menu bar is NotificationCenter's own (Apple/File/Edit menus) and
    // has nothing to do with notifications; skip it to keep the dump readable.
    if depth >= MAX_DEPTH || e.role().as_deref() == Some("AXMenuBar") {
        return;
    }
    for (i, c) in e.children().iter().enumerate() {
        let p = if path.is_empty() {
            i.to_string()
        } else {
            format!("{path}.{i}")
        };
        walk(c, &p, depth + 1, count);
    }
}

fn resolve(root: &Element, path: &str) -> Option<Element> {
    let mut cur = root.clone();
    for part in path.split('.').filter(|p| !p.is_empty()) {
        let i: usize = part.parse().ok()?;
        cur = cur.children().into_iter().nth(i)?;
    }
    Some(cur)
}

fn count_groups(e: &Element, depth: usize) -> usize {
    if depth > MAX_DEPTH {
        return 0;
    }
    let own = usize::from(
        e.string("AXSubrole").as_deref() == Some("AXNotificationCenterBanner")
            || e.string("AXSubrole").as_deref() == Some("AXNotificationCenterAlert"),
    );
    own + e
        .children()
        .iter()
        .map(|c| count_groups(c, depth + 1))
        .sum::<usize>()
}

fn dump(root: &Element) {
    let start = std::time::Instant::now();
    let mut n = 0;
    walk(root, "", 0, &mut n);
    println!("-- {n} nodes in {} ms", start.elapsed().as_millis());
}

fn main() {
    println!("accessibility trusted: {}", cua_ax::is_trusted());
    let Some(pid) = notification_center_pid() else {
        println!("NotificationCenter is not running");
        return;
    };
    println!("NotificationCenter pid {pid}");
    let root = Element::for_pid(pid);
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("dump") => dump(&root),
        Some("press") => {
            let (Some(path), Some(action)) = (args.get(1), args.get(2)) else {
                println!("usage: notification_probe press <path> <action>");
                return;
            };
            let Some(target) = resolve(&root, path) else {
                println!("no node at path {path}");
                return;
            };
            println!("target: {}", describe(&target));
            // Custom actions are advertised as "Name:<label>\nTarget:..\nSelector:..";
            // accept just the label and resolve it to the full advertised string.
            let resolved = target
                .actions()
                .into_iter()
                .find(|a| {
                    a == action
                        || a.strip_prefix("Name:").and_then(|r| r.split('\n').next())
                            == Some(action.as_str())
                })
                .unwrap_or_else(|| action.clone());
            let action = &resolved;
            let t = std::time::Instant::now();
            println!(
                "perform({action:?}) -> {:?} in {} ms",
                target.perform(action),
                t.elapsed().as_millis()
            );
            std::thread::sleep(std::time::Duration::from_millis(800));
            dump(&root);
        }
        Some("watch") => {
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let start = std::time::Instant::now();
            let mut last = usize::MAX;
            while start.elapsed().as_secs() < secs {
                let n = count_groups(&root, 0);
                if n != last {
                    println!(
                        "+{:>5} ms  notification groups: {n}",
                        start.elapsed().as_millis()
                    );
                    last = n;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        Some("open-center") => {
            // The history panel opens from the menu-bar clock, which belongs to
            // ControlCenter's extras menu bar rather than to NotificationCenter.
            let Some(cc) = pid_of("ControlCenter") else {
                println!("ControlCenter is not running");
                return;
            };
            let extras = Element::for_pid(cc).element("AXExtrasMenuBar");
            let Some(extras) = extras else {
                println!("ControlCenter publishes no AXExtrasMenuBar");
                return;
            };
            let items = extras.children();
            for i in &items {
                println!("extra: {}", describe(i));
            }
            let clock = items
                .iter()
                .find(|i| i.string("AXIdentifier").as_deref() == Some("com.apple.menuextra.clock"));
            let Some(clock) = clock else {
                println!("no clock menu extra found");
                return;
            };
            println!("press clock -> {:?}", clock.perform("AXPress"));
            std::thread::sleep(std::time::Duration::from_millis(1200));
            dump(&root);
        }
        Some(other) => println!("unknown mode {other:?}"),
    }
}
