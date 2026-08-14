//! AppKit-level event synthesis: `NSEvent` factories bridged to `CGEventRef`.
//!
//! # Why not `CGEventCreateMouseEvent`
//!
//! The rest of this crate used to build mouse events with
//! `CGEventCreateMouseEvent` and then stamp the AppKit-visible fields back on by
//! hand (`kCGMouseEventClickState`, `kCGMouseEventNumber`, the window fields).
//! That is the wrong direction of travel. A `CGEvent` synthesized from scratch
//! has no AppKit identity: `-[NSEvent eventNumber]` reads back 0, the window
//! number is 0, and `-[NSEvent window]` resolves to nil. Custom-drawn `NSView`s
//! that do their own hit-testing and click-counting — a chat app's conversation
//! list being the measured case — treat that as "not a real click" and drop it.
//! Stamping the private fields afterwards does not fix it, because AppKit
//! reconstructs its `NSEvent` from the event record's *own* header, not from the
//! fields a caller patched in.
//!
//! Building the `NSEvent` first and asking AppKit for its `CGEvent` inverts the
//! dependency: AppKit fills in the header it will later validate, and we only
//! adjust the CG-space fields it does not own (screen location, private window
//! routing). This is the same construction OpenAI's `SkyComputerUseService`
//! uses — `AccessibilitySupport.SynthesizedEvent.mouseEvent(eventNumber:type:
//! clickCount:at:mouseButton:flags:inWindow:windowBounds:
//! windowUsesFlippedCoordinates:)` wraps
//! `-[NSEvent mouseEventWithType:location:modifierFlags:timestamp:windowNumber:
//! context:eventNumber:clickCount:pressure:]` and then converts with
//! `-[NSEvent CGEvent]`.
//!
//! # Thread safety
//!
//! These are `NSEvent` *class* factories. They allocate an immutable value
//! object and touch no window, no responder chain and no `NSApp`, so they are
//! safe off the main thread — which matters because the session runs actions on
//! its own dedicated thread.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventSubtype, NSEventType};
use objc2_core_graphics::CGEvent;
use objc2_foundation::NSPoint;

/// `NSEventTypeAppKitDefined`. AppKit routes these to `NSApplication`'s
/// internal handling rather than to a view, which is what makes them usable as
/// out-of-band activation signals.
const APPKIT_DEFINED: NSEventType = NSEventType(13);

/// The undocumented `NSEventType` that CoreProcessSwitching focus notifications
/// travel on. Not in the public `NSEventType` enum; the reference implementation
/// declares it as an extension constant with the value 21
/// (`sky_decomp.c:1664962`).
const PROCESS_NOTIFICATION: NSEventType = NSEventType(21);

/// `kCPSNotifyKeyFocusReturned`, one of the private CPS focus-notification
/// subtypes (`sky_decomp.c:1665140`). The neighbouring constants in the same
/// initializer are `kCPSNotifyNewFront` = 2, `kCPSNotifyLostKeyFocus` = 0x1000,
/// `kCPSNotifyKeyFocusTaken` = 0x4000 and `kCPSNotifyKeyFocusChanged` = 0xf102;
/// only this one is used here.
///
/// `NSEventSubtype` is a `short`, so the value is negative once narrowed. That is
/// what the reference stores too — it declares the constant on `NSEventSubtype`
/// itself rather than as an integer.
const CPS_NOTIFY_KEY_FOCUS_RETURNED: i16 = 0x8000_u16 as i16;

/// Monotonic source for `-[NSEvent eventNumber]`.
///
/// AppKit uses the event number to correlate a mouse-up with the mouse-down
/// that started the same tracking session, and views that implement drag
/// thresholds or double-click detection compare consecutive numbers. Every
/// synthesized event must therefore get a *fresh, increasing* value; reusing one
/// makes a second click look like a duplicate delivery of the first, and leaving
/// it at 0 makes every click look like the same click.
///
/// The window server's own numbering is per-session and unknowable from here, so
/// this counter only has to be internally consistent and never repeat. It starts
/// high enough to sit clear of the small numbers a freshly launched app will
/// have seen from real input.
static NEXT_EVENT_NUMBER: AtomicI64 = AtomicI64::new(0x0010_0000);

/// Claim the next event number. Wrapping is not a concern: at one event per
/// nanosecond this counter lasts longer than the machine.
pub(crate) fn next_event_number() -> isize {
    NEXT_EVENT_NUMBER.fetch_add(1, Ordering::Relaxed) as isize
}

/// Current uptime in nanoseconds — the clock `CGEventTimestamp` is measured on.
///
/// `CLOCK_UPTIME_RAW` is exactly what `DispatchTime.now().uptimeNanoseconds`
/// reports, and it is already in nanoseconds, so it needs none of the
/// `mach_timebase_info` scaling that raw `mach_absolute_time` would on Apple
/// silicon.
pub(crate) fn uptime_nanos() -> u64 {
    // Declared here because the `libc` crate does not expose this Darwin-only
    // entry point. `CLOCK_UPTIME_RAW` is 8 on all supported macOS versions; it
    // counts nanoseconds since boot and, unlike `CLOCK_MONOTONIC`, excludes time
    // the machine spent asleep — which is what the event timestamp clock does.
    const CLOCK_UPTIME_RAW: u32 = 8;
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock_id: u32) -> u64;
    }
    // SAFETY: takes a clock id by value and returns a scalar; no pointers are
    // involved, and an unrecognized clock id would return 0 rather than trap.
    unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
}

/// Build a mouse event through AppKit and hand back its `CGEventRef`.
///
/// `location` is passed to AppKit only to keep it from rejecting the event; the
/// caller is expected to overwrite the CG-space location afterwards, because
/// `NSEvent` interprets its `location:` in flipped, window-relative AppKit
/// coordinates while everything downstream here works in CG screen points.
/// OpenAI's implementation does the same thing — it calls `CGEventSetLocation`
/// immediately after the conversion.
///
/// `window_number` is the AppKit window number of the target window. Passing the
/// real one is what lets `-[NSEvent window]` resolve inside the target process;
/// passing 0 produces a windowless event that only a global tracking area would
/// ever see.
pub(crate) fn mouse_event(
    kind: NSEventType,
    location: NSPoint,
    window_number: isize,
    event_number: isize,
    click_count: isize,
    pressure: f32,
) -> Option<Retained<CGEvent>> {
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,
        location,
        NSEventModifierFlags::empty(),
        // Timestamp 0 here on purpose: `send` re-stamps every event with a
        // fresh uptime reading immediately before posting, and a value baked in
        // at construction time would already be stale by then.
        0.0,
        window_number,
        None,
        event_number,
        click_count,
        pressure,
    )?;
    event.CGEvent()
}

/// Build a locationless notification event of `kind` carrying `subtype`.
///
/// This is the mechanism behind every notice below. The event is not aimed at a
/// view — AppKit consumes it in `NSApplication`'s own event loop and updates its
/// idea of activation and key focus from the type/subtype pair. The reference
/// implementation builds all of them through one helper of exactly this shape
/// (`sky_decomp.c:1678337`), with location, flags, timestamp, window number,
/// `data1` and `data2` all zero.
fn notification_event(
    kind: NSEventType,
    subtype: i16,
    window_number: isize,
) -> Option<Retained<CGEvent>> {
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        kind,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        window_number,
        None,
        subtype,
        0,
        0,
    )?;
    event.CGEvent()
}

/// "Keyboard focus has come back to you."
///
/// The reference implementation posts this *before* the activation notice, as the
/// first step of `SyntheticAppFocusEnforcer.enforceActiveState(for:)`
/// (`sky_decomp.c:1669727-1669778`). Telling an app it is active is not the same
/// as telling it that its window owns key focus again, and a control that only
/// arms itself when its window is key — a menu button being the measured case —
/// needs the second statement as well as the first.
///
/// Unlike the activation notice this one has no window number to carry: the
/// reference passes zero, and the notification is understood as applying to
/// whichever window the app already considers key.
pub(crate) fn notify_window_key_focus_returned() -> Option<Retained<CGEvent>> {
    notification_event(PROCESS_NOTIFICATION, CPS_NOTIFY_KEY_FOCUS_RETURNED, 0)
}

/// "Your application just became active."
///
/// Posted to the target process before a click, this makes the target's AppKit
/// believe it is the active application *without* `NSRunningApplication.activate`
/// ever being called — so the real frontmost app, the user's keyboard focus and
/// the current Space are all left alone. That distinction is the whole point:
/// many custom-drawn controls check `NSApp.isActive` (directly, or indirectly
/// through `-[NSWindow isKeyWindow]`) before they will act on a click, and
/// refusing to satisfy that check is what made background clicks silently
/// no-op.
///
/// `window_number` names the window that should be treated as key.
pub(crate) fn notify_app_activated(window_number: isize) -> Option<Retained<CGEvent>> {
    notification_event(
        APPKIT_DEFINED,
        NSEventSubtype::ApplicationActivated.0,
        window_number,
    )
}

/// "Your application is no longer active." The counterpart to
/// [`notify_app_activated`], posted after the click so the target does not sit
/// indefinitely believing it owns an activation it never really had.
pub(crate) fn notify_app_deactivated(window_number: isize) -> Option<Retained<CGEvent>> {
    notification_event(
        APPKIT_DEFINED,
        NSEventSubtype::ApplicationDeactivated.0,
        window_number,
    )
}

/// The raw `CGEventRef` behind a retained event, for the private SkyLight
/// stamping and posting helpers, which take `*mut c_void`.
///
/// The pointer borrows from `event` and must not outlive it.
pub(crate) fn as_raw(event: &Retained<CGEvent>) -> *mut c_void {
    let ptr: *const CGEvent = Retained::as_ptr(event);
    ptr as *mut c_void
}
