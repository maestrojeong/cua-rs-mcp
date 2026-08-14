//! Find the minimum sufficient recipe for a click that reaches a *background*
//! app without moving the real pointer.
//!
//! Background: an earlier measurement concluded `CGEventPostToPid` "does not
//! work", from a bare mouse-down/up pair carrying nothing but a location and a
//! click state. A symbol survey of a shipping implementation of this technique
//! shows the event it sends carries considerably more, and that the click is
//! bracketed by *synthetic activation notifications* — the target app is made
//! to believe it is active before the click arrives. This probe reproduces
//! that structure from the outside and reports which parts are load-bearing.
//!
//! The ingredients, each independently switchable:
//!
//! | flag | what it adds |
//! |---|---|
//! | `winlocal` | point in window-local coordinates instead of global screen |
//! | `flip` | flip y within the window (AppKit's bottom-left origin) |
//! | `winid` | the real window number, in the event's window field |
//! | `evnum` | a unique, monotonically increasing event number |
//! | `focus` | bracket the click with AppKit-defined activation events |
//! | `nsevent` | build the mouse event as an `NSEvent` carrying a real `windowNumber` |
//!
//! Usage: pid_click_probe <pid> <global-x> <global-y> [flag ...]
//! Example, the full recipe:
//!   pid_click_probe 841 500 300 winlocal flip winid evnum focus

use std::sync::atomic::{AtomicI64, Ordering};

use objc2_app_kit::{NSEvent, NSEventSubtype, NSEventType};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventSource, CGEventSourceStateID, CGEventType, CGMouseButton,
};

/// Real mouse events carry a unique, increasing number. A synthetic one that
/// leaves it at zero looks like the same event repeated.
static EVENT_NUMBER: AtomicI64 = AtomicI64::new(0x5000);

/// `NSEventTypeAppKitDefined`. These are the events AppKit itself uses to tell
/// an app it has become active and that one of its windows is key. They are
/// not synthesizable through `CGEvent` — the type does not exist at that layer
/// — so they are built as `NSEvent` and converted down.
fn appkit_event(
    subtype: NSEventSubtype,
    window_number: isize,
    data1: isize,
    data2: isize,
) -> Option<objc2::rc::Retained<CGEvent>> {
    let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::AppKitDefined,
        CGPoint::new(0.0, 0.0),
        objc2_app_kit::NSEventModifierFlags::empty(),
        0.0,
        window_number,
        None,
        subtype.0,
        data1,
        data2,
    )?;
    ev.CGEvent()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: pid_click_probe <pid> <global-x> <global-y> [winlocal flip winid evnum focus]"
        );
        std::process::exit(2);
    }
    // Posting events at all requires this process to be trusted for
    // Accessibility, and TCC grants attach to a binary, not to a project. This
    // probe is a *different* binary from the server that holds the grant, so
    // printing the answer is the difference between a real negative result and
    // ten experiments that were never delivered.
    println!("AXIsProcessTrusted = {}", cua_ax::is_trusted());

    let pid: i32 = args[1].parse().unwrap();
    let gx: f64 = args[2].parse().unwrap();
    let gy: f64 = args[3].parse().unwrap();
    let flags: Vec<&str> = args[4..].iter().map(|s| s.as_str()).collect();
    let has = |f: &str| flags.contains(&f);

    // The target's frontmost on-screen window, straight from the window
    // server: it answers whether or not the app is active, and it is the only
    // source of a real window number.
    let windows = cua_capture::list_windows().expect("list_windows");
    let w = windows
        .iter()
        .filter(|w| w.pid == pid && w.layer == 0 && w.frame.size.width > 100.0)
        .max_by(|a, b| {
            (a.frame.size.width * a.frame.size.height)
                .total_cmp(&(b.frame.size.width * b.frame.size.height))
        })
        .expect("no ordinary window for that pid");
    println!("window id={} title={:?} frame={:?}", w.id, w.title, w.frame);

    let mut point = CGPoint::new(gx, gy);
    if has("winlocal") {
        point.x = gx - w.frame.origin.x;
        point.y = gy - w.frame.origin.y;
        if has("flip") {
            point.y = w.frame.size.height - point.y;
        }
    }
    println!(
        "flags={flags:?}  ->  posting at ({:.0}, {:.0})",
        point.x, point.y
    );

    let source =
        CGEventSource::new(CGEventSourceStateID::CombinedSessionState).expect("event source");

    if has("focus") {
        // Tell the app it just became active, and that this window took key
        // focus. Without this a background app routinely discards a click it
        // would otherwise honour, because as far as it knows nobody is
        // interacting with it.
        for (subtype, label) in [
            (NSEventSubtype::ApplicationActivated, "ApplicationActivated"),
            (NSEventSubtype::WindowExposed, "WindowExposed"),
        ] {
            match appkit_event(subtype, w.id as isize, 0, 0) {
                Some(ev) => {
                    CGEvent::post_to_pid(pid, Some(&ev));
                    println!("  sent {label}");
                }
                None => println!("  FAILED to build {label}"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    for (kind, click_state) in [
        (CGEventType::LeftMouseDown, 1i64),
        (CGEventType::LeftMouseUp, 1i64),
    ] {
        // `CGEvent` has no field for an event's *window number* — the closest
        // it offers is "the window under the mouse pointer", which is a hint
        // about the screen, not an address. `NSEvent` takes the window number
        // directly, and converting the result down to a `CGEvent` keeps it.
        if has("nsevent") {
            let ns_type = match kind {
                CGEventType::LeftMouseDown => NSEventType::LeftMouseDown,
                _ => NSEventType::LeftMouseUp,
            };
            let n = EVENT_NUMBER.fetch_add(1, Ordering::Relaxed);
            let ev = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                ns_type,
                point,
                objc2_app_kit::NSEventModifierFlags::empty(),
                0.0,
                w.id as isize,
                None,
                n as isize,
                click_state as isize,
                if matches!(kind, CGEventType::LeftMouseDown) { 1.0 } else { 0.0 },
            )
            .expect("ns mouse event");
            if let Some(cg) = ev.CGEvent() {
                CGEvent::post_to_pid(pid, Some(&cg));
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            continue;
        }

        let event = CGEvent::new_mouse_event(Some(&source), kind, point, CGMouseButton::Left)
            .expect("mouse event");
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventClickState,
            click_state,
        );
        if has("evnum") {
            let n = EVENT_NUMBER.fetch_add(1, Ordering::Relaxed);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventNumber, n);
        }
        if has("winid") {
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventWindowUnderMousePointer,
                w.id as i64,
            );
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent,
                w.id as i64,
            );
        }
        CGEvent::post_to_pid(pid, Some(&event));
        // A real click is not instantaneous. The shipping implementation names
        // this a "human click interval"; views that track the pointer between
        // down and up need the gap to exist at all.
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    println!("  sent mouse down/up");

    if has("focus") {
        std::thread::sleep(std::time::Duration::from_millis(30));
        if let Some(ev) = appkit_event(NSEventSubtype::ApplicationDeactivated, w.id as isize, 0, 0)
        {
            CGEvent::post_to_pid(pid, Some(&ev));
            println!("  sent ApplicationDeactivated");
        }
    }
}
