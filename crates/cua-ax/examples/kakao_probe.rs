//! Diagnostic: can AXSelected / AXSelectedRows open a KakaoTalk chat row
//! without any CGEvent at all? Usage: kakao_probe <pid> <x> <y>
use cua_ax::{action, attr, require_trusted, Element};

fn main() {
    require_trusted().expect("not trusted");
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().unwrap();
    let x: f32 = args[2].parse().unwrap();
    let y: f32 = args[3].parse().unwrap();

    let app = Element::for_pid(pid);
    let el = app.element_at(x, y).expect("hit-test failed");
    println!("hit element: role={:?}", el.role());
    println!("  actions: {:?}", el.actions());
    println!("  AXSelected settable: {}", el.is_settable(attr::SELECTED));
    println!("  AXSelected value: {:?}", el.bool(attr::SELECTED));

    println!("  AXFocused settable: {}", el.is_settable(attr::FOCUSED));
    match el.set_bool(attr::FOCUSED, true) {
        Ok(()) => println!("set AXFocused=true on hit element: OK"),
        Err(e) => println!("set AXFocused=true on hit element: FAILED ({e})"),
    }

    let row = el.element(attr::PARENT);
    if let Some(row) = &row {
        println!("parent (row): role={:?}", row.role());
        println!("  actions: {:?}", row.actions());
        println!("  AXSelected settable: {}", row.is_settable(attr::SELECTED));
        match row.set_bool(attr::SELECTED, true) {
            Ok(()) => println!("  set AXSelected=true on row: OK"),
            Err(e) => println!("  set AXSelected=true on row: FAILED ({e})"),
        }
    }

    if let Some(table) = row.as_ref().and_then(|r| r.element(attr::PARENT)) {
        println!("grandparent (table): role={:?}", table.role());
        println!("  actions: {:?}", table.actions());
        for name in ["AXSelectedRows", "AXSelectedChildren", "AXSelectedCells"] {
            println!("  {name} settable: {}", table.is_settable(name));
        }
        // Try the classic "select this row via the table's selection
        // attribute" trick: write an array containing just the row element.
        match table.set_element_array("AXSelectedRows", &[row.clone().unwrap()]) {
            Ok(()) => println!("  set AXSelectedRows=[row]: OK"),
            Err(e) => println!("  set AXSelectedRows=[row]: FAILED ({e})"),
        }
    }

    // Try the direct route: set AXSelected = true on the hit element itself.
    match el.set_bool(attr::SELECTED, true) {
        Ok(()) => println!("set AXSelected=true on hit element: OK"),
        Err(e) => println!("set AXSelected=true on hit element: FAILED ({e})"),
    }
    println!("  AXSelected value after: {:?}", el.bool(attr::SELECTED));

    // Selection alone may only highlight the row. Try the keyboard-equivalent
    // "open" action on the row, the cell, and the table, since a real user
    // could select with arrow keys and press Return to open.
    if let Some(row) = &row {
        for (label, target) in [("row", row), ("cell", &el)] {
            match target.perform(action::CONFIRM) {
                Ok(()) => println!("AXConfirm on {label}: OK"),
                Err(e) => println!("AXConfirm on {label}: FAILED ({e})"),
            }
        }
    }
}
