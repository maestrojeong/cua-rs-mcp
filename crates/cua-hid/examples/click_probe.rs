//! Standalone sanity check comparing pid-targeted vs global mouse click posting,
//! and pid-targeted with an explicit window-under-pointer field set.
//! Usage: click_probe <pid> <x> <y> [global|windowed]
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args[1].parse().unwrap();
    let x: f64 = args[2].parse().unwrap();
    let y: f64 = args[3].parse().unwrap();
    let mode = args.get(4).map(|s| s.as_str()).unwrap_or("pid");

    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).expect("source");
    let point = CGPoint::new(x, y);

    let window_id: Option<i64> = if mode == "windowed" {
        let windows = cua_capture::list_windows().expect("list_windows");
        let w = windows.iter().find(|w| w.pid == pid).expect("window for pid");
        eprintln!("using window id {} ({:?})", w.id, w.title);
        Some(w.id as i64)
    } else {
        None
    };

    for (kind, button) in [
        (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (CGEventType::LeftMouseUp, CGMouseButton::Left),
    ] {
        let event = CGEvent::new_mouse_event(Some(&source), kind, point, button).expect("event");
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 1);
        if let Some(wid) = window_id {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventWindowUnderMousePointer,
                wid,
            );
        }
        match mode {
            "global" => CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event)),
            _ => CGEvent::post_to_pid(pid, Some(&event)),
        }
    }
    println!("posted [{mode}] click to pid {pid} at ({x}, {y})");
}
