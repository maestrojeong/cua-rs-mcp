//! Listen-only event tap that dumps every field of the mouse events flowing
//! through the session, so a working synthetic click can be read off the wire
//! instead of guessed at.
//!
//! Forty-four hand-built variants of a pid-targeted click have failed against
//! a control that a real click toggles, while a shipping implementation
//! demonstrably clicks a background app without moving the pointer. Rather
//! than guess a forty-fifth, this watches what actually arrives.
//!
//! The tap is placed at the session level and passive (`ListenOnly`): it
//! observes and never modifies or swallows anything.
//!
//! Two useful outcomes, and they are opposites:
//!
//! - the working click **appears** here — then its field dump is the recipe,
//!   including the one field or source state still missing;
//! - the working click **does not appear** — then it never entered the shared
//!   stream at all, which is itself the answer: it was delivered straight into
//!   one process, where no tap can see it.
//!
//! Usage: event_spy [seconds]   (default 30)

use std::ffi::c_void;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventType,
};

/// Fields worth printing for a mouse event, by raw number so the ones the Rust
/// binding does not name are still readable.
const FIELDS: &[(u32, &str)] = &[
    (0, "MouseEventNumber"),
    (1, "MouseEventClickState"),
    (2, "MouseEventPressure"),
    (3, "MouseEventButtonNumber"),
    (7, "MouseEventSubtype"),
    (39, "TargetProcessSerialNumber"),
    (40, "TargetUnixProcessID"),
    (41, "SourceUnixProcessID"),
    (42, "SourceUserData"),
    (45, "SourceStateID"),
    (91, "WindowUnderMousePointer"),
    (92, "WindowUnderPointerThatCanHandle"),
];

unsafe extern "C-unwind" fn callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    etype: CGEventType,
    event: core::ptr::NonNull<CGEvent>,
    _user: *mut c_void,
) -> *mut CGEvent {
    let ev = unsafe { event.as_ref() };
    let loc = CGEvent::location(Some(ev));
    println!(
        "\n{:?} at ({:.0}, {:.0})",
        etype, loc.x, loc.y
    );
    for (field, name) in FIELDS {
        let v = CGEvent::integer_value_field(Some(ev), CGEventField(*field));
        if v != 0 {
            println!("    {name} ({field}) = {v}");
        }
    }
    event.as_ptr()
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let mask: CGEventMask = (1 << CGEventType::LeftMouseDown.0)
        | (1 << CGEventType::LeftMouseUp.0)
        | (1 << CGEventType::RightMouseDown.0)
        | (1 << CGEventType::MouseMoved.0);

    let tap: CFRetained<CFMachPort> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            // Passive. This must never alter or drop what the user or another
            // app is doing.
            CGEventTapOptions::ListenOnly,
            mask,
            Some(callback),
            std::ptr::null_mut(),
        )
    }
    .expect("event tap (needs Accessibility)");

    let source: CFRetained<CFRunLoopSource> =
        CFMachPort::new_run_loop_source(None, Some(&tap), 0).expect("run loop source");
    let run_loop = CFRunLoop::current().expect("run loop");
    unsafe {
        run_loop.add_source(Some(&source), objc2_core_foundation::kCFRunLoopCommonModes);
        CGEvent::tap_enable(&tap, true);
    }

    eprintln!("watching the session event stream for {secs}s — click something now");
    unsafe {
        CFRunLoop::run_in_mode(
            objc2_core_foundation::kCFRunLoopDefaultMode,
            secs as f64,
            false,
        );
    }
    eprintln!("done");
}
