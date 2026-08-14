//! Enumerate every AXWindow an app exposes, not just the front one.
//! KakaoTalk opens each conversation as its own window, so a chat can be
//! driven without ever activating the row in the conversation list.
//! Usage: windows_probe <pid>
use cua_ax::{attr, require_trusted, Element};

fn walk(el: &Element, depth: usize, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    let pad = "  ".repeat(depth);
    let role = el.role().unwrap_or_default();
    let title = el.string(attr::TITLE).unwrap_or_default();
    let value = el.string(attr::VALUE).unwrap_or_default();
    let acts = el.actions();
    let editable = el.is_settable(attr::VALUE);
    let mut line = format!("{pad}{role}");
    if !title.is_empty() {
        line += &format!(" title={title:?}");
    }
    if !value.is_empty() {
        let v: String = value.chars().take(40).collect();
        line += &format!(" value={v:?}");
    }
    if !acts.is_empty() {
        line += &format!(" actions={acts:?}");
    }
    if editable {
        line += " [VALUE-SETTABLE]";
    }
    println!("{line}");
    for k in el.elements(attr::CHILDREN).iter().take(40) {
        walk(k, depth + 1, budget);
    }
}

fn main() {
    require_trusted().expect("not trusted");
    let pid: i32 = std::env::args().nth(1).unwrap().parse().unwrap();
    let app = Element::for_pid(pid);
    let windows = app.elements(attr::WINDOWS);
    println!("app exposes {} window(s)\n", windows.len());
    for (i, w) in windows.iter().enumerate() {
        println!(
            "=== window[{i}] title={:?} pos={:?} ===",
            w.string(attr::TITLE),
            w.position()
        );
        let mut budget = 60usize;
        walk(w, 1, &mut budget);
        println!();
    }
}
