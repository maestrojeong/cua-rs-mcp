//! Click a window *without moving the pointer and without raising the app*,
//! using the window server's own focus SPI.
//!
//! Why this exists: ten variants of a plain `CGEventPostToPid` click were
//! measured against a known-good control and none of them landed. A symbol
//! survey of a shipping implementation shows why — the public path is not the
//! one it uses. It reaches for:
//!
//! - `ApplicationRegistrySPI.setFrontProcess(_:windowID:options:) -> CGError`,
//!   i.e. `_SLPSSetFrontProcessWithOptions`, which makes a process frontmost
//!   *for input purposes* without raising or activating it;
//! - AppKit-defined events carrying **private** subtypes — `kCPSNotifyNewFront`,
//!   `kCPSNotifyKeyFocusTaken`, `kCPSNotifyKeyFocusReturned` — rather than the
//!   public `NSEventSubtypeApplicationActivated` this probe's predecessor sent.
//!
//! Both live in SkyLight, resolved at run time, which is exactly why a static
//! symbol scan of that binary reported "no input synthesis at all". The same
//! technique is long-established in open-source window managers, so this is a
//! reimplementation from the documented behaviour of a public interface, not a
//! port of anything.
//!
//! Usage: slps_click_probe <pid> <global-x> <global-y> [flag ...]
//!   front   call setFrontProcess for the target window first
//!   cps     send the private kCPSNotify* focus events
//!   flip    y is window-local, bottom-left origin (AppKit's own space)
//!   global  post through the shared HID tap instead of to the pid
//!   psn     stamp the event with kCGEventTargetProcessSerialNumber
//!   full    fill the fields a shipping implementation sets and we did not:
//!           button number, mouse subtype, and the target unix pid
//!   nsapp   become a real (accessory) application before posting
//!   hidsrc  build the event from a HIDSystemState source rather than
//!           CombinedSessionState
//!   awr     activate-without-raise via SLPSPostEventRecordTo's 0xF8 record
//!   sl      deliver with SkyLight's `SLEventPostToPid` instead of the public
//!           `CGEventPostToPid`
//!   winloc  also set the window-local point with `CGEventSetWindowLocation`
//!   annot   post to the *annotated session* tap: the session's own event
//!           queue, which does not drag the hardware cursor with it
//!
//! The interesting run is `front cps flip`: if that toggles the control while
//! the pointer never moves, the "no cursor" property is real.

use std::ffi::c_void;

use objc2_app_kit::{NSEvent, NSEventType};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton,
};

/// `ProcessSerialNumber`. Carbon's process identity, which is what the window
/// server's focus SPI speaks — pids are not accepted there.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct ProcessSerialNumber {
    high_long_of_psn: u32,
    low_long_of_psn: u32,
}

/// Private AppKit-defined event subtypes: the notifications the window server
/// sends an app to tell it that it is now the front process and that one of
/// its windows holds key focus.
///
/// These values are not guesses. They were read out of a shipping binary's
/// own constants — each is a `mov w9, #imm; strh w9, [slot]` in the lazy
/// initializer behind the correspondingly named accessor. Note `NEW_FRONT`
/// collides numerically with the public `NSEventSubtypeApplicationDeactivated`;
/// the AppKit-defined subtype space is interpreted per sender, so that is not
/// a contradiction, and it is exactly why an earlier attempt that sent the
/// *public* subtypes had no effect.
const K_CPS_NOTIFY_NEW_FRONT: i16 = 0x2;
#[allow(dead_code)]
const K_CPS_NOTIFY_LOST_KEY_FOCUS: i16 = 0x1000;
const K_CPS_NOTIFY_KEY_FOCUS_TAKEN: i16 = 0x4000u16 as i16;
const K_CPS_NOTIFY_KEY_FOCUS_RETURNED: i16 = 0x8000u16 as i16;

/// `kCGEventTargetProcessSerialNumber`. Addresses an event at a process by its
/// Carbon serial number rather than by pid, and is the field behind the
/// shipping implementation's `CGEventRef.subjectProcessSerialNumber` accessor
/// (which reads it back as a High/Low `UInt32` pair). Not exposed by the Rust
/// binding's `CGEventField` enum, so it is used by its documented raw value.
const K_CG_EVENT_TARGET_PSN: u32 = 39;

/// Fields a shipping implementation sets on every synthesized mouse event and
/// this probe's earlier attempts did not. Read out of its `mouseDown` builder
/// as literal field numbers passed to `CGEventSetIntegerValueField`.
///
/// `SUBTYPE = 3` is the interesting one: it is the value the window server
/// puts on events that came from a real pointing device, so an event without
/// it is self-identifying as synthetic. `TARGET_UNIX_PID` addresses the event
/// at a process from inside the event itself, rather than relying only on the
/// pid argument to the post call.
const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
const K_CG_MOUSE_EVENT_SUBTYPE: u32 = 7;
const K_CG_MOUSE_SUBTYPE_DEFAULT: i64 = 3;
const K_CG_EVENT_TARGET_UNIX_PID: u32 = 40;

/// `kCPSUserGenerated` — mark the focus change as if a person caused it, which
/// is what stops the window server from treating it as a programmatic hint it
/// may ignore.
const K_CPS_USER_GENERATED: u32 = 0x200;

unsafe extern "C" {
    fn GetProcessForPID(pid: i32, psn: *mut ProcessSerialNumber) -> i32;
}

/// Focus a window's process without raising it, by posting the window
/// server's own 248-byte event record.
///
/// This is the step `_SLPSSetFrontProcessWithOptions` was standing in for, and
/// why that stand-in returned `CGError 0` and changed nothing. Structure per
/// trycua/cua's `skylight.rs` (MIT, Copyright (c) 2025 Cua AI, Inc.), which
/// documents the same record long used by open-source window managers: byte
/// 0x04 = length, 0x08 = 0x0D, the target window id little-endian at
/// 0x3C..0x40, and byte 0x8A carrying 0x02 to defocus the outgoing process
/// then 0x01 to focus the incoming one.
fn activate_without_raise(target_pid: i32, target_wid: u32) -> bool {
    type PostRecord = unsafe extern "C" fn(*const c_void, *const u8) -> i32;
    type GetFront = unsafe extern "C" fn(*mut c_void) -> i32;
    unsafe {
        let post = libc::dlsym(libc::RTLD_DEFAULT, c"SLPSPostEventRecordTo".as_ptr());
        let front = libc::dlsym(libc::RTLD_DEFAULT, c"_SLPSGetFrontProcess".as_ptr());
        if post.is_null() || front.is_null() {
            println!("  SLPSPostEventRecordTo/_SLPSGetFrontProcess NOT FOUND");
            return false;
        }
        let post: PostRecord = std::mem::transmute::<*mut c_void, PostRecord>(post);
        let get_front: GetFront = std::mem::transmute::<*mut c_void, GetFront>(front);

        let mut prev = [0u8; 8];
        let front_rc = get_front(prev.as_mut_ptr() as *mut c_void);
        let mut target = ProcessSerialNumber::default();
        let psn_rc = GetProcessForPID(target_pid, &mut target);
        println!("  _SLPSGetFrontProcess -> {front_rc}, GetProcessForPID -> {psn_rc}");
        if front_rc != 0 || psn_rc != 0 {
            return false;
        }

        let mut buf = [0u8; 0xF8];
        buf[0x04] = 0xF8;
        buf[0x08] = 0x0D;
        buf[0x3C..0x40].copy_from_slice(&target_wid.to_le_bytes());

        buf[0x8A] = 0x02;
        let defocus = post(prev.as_ptr() as *const c_void, buf.as_ptr());
        buf[0x8A] = 0x01;
        let focus = post(&target as *const _ as *const c_void, buf.as_ptr());
        println!("  activate_without_raise: defocus={defocus} focus={focus}");
        defocus == 0 && focus == 0
    }
}

/// SkyLight's own post, resolved by name.
///
/// Not the same call as the public `CGEventPostToPid`. Per the documented
/// behaviour of trycua/cua's `skylight.rs` (MIT), `SLEventPostToPid` routes
/// through `SLEventPostToPSN` → `CGSTickleActivityMonitor` →
/// `SLSUpdateSystemActivityWithLocation` → `IOHIDPostEvent`, while the public
/// call skips the activity-monitor tickle — so the receiving app never counts
/// the event as live input. That is the single sentence that explains why
/// forty-six variants of `CGEventPostToPid` here changed nothing, and it
/// matches the captured trace: the events that work carry no source pid and
/// `SourceStateID = 1`, which is what arriving via `IOHIDPostEvent` looks like.
fn sl_event_post_to_pid(pid: i32, event: &CGEvent) -> bool {
    type Fp = unsafe extern "C" fn(i32, *mut c_void);
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"SLEventPostToPid".as_ptr());
        if sym.is_null() {
            return false;
        }
        let f: Fp = std::mem::transmute::<*mut c_void, Fp>(sym);
        f(pid, event as *const CGEvent as *mut c_void);
        true
    }
}

/// `CGEventSetWindowLocation`, also SkyLight-side and also not in any SDK.
fn set_window_location(event: &CGEvent, x: f64, y: f64) -> bool {
    type Fp = unsafe extern "C" fn(*mut c_void, f64, f64);
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"CGEventSetWindowLocation".as_ptr());
        if sym.is_null() {
            return false;
        }
        let f: Fp = std::mem::transmute::<*mut c_void, Fp>(sym);
        f(event as *const CGEvent as *mut c_void, x, y);
        true
    }
}

/// `_SLPSSetFrontProcessWithOptions` lives in SkyLight and is not in any SDK,
/// so it is resolved by name at run time. Reported as missing rather than
/// crashing if a future macOS drops it.
fn set_front_process(psn: &ProcessSerialNumber, window_id: u32, options: u32) -> Option<i32> {
    type Fp = unsafe extern "C" fn(*const ProcessSerialNumber, u32, u32) -> i32;
    let path = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
    let name = c"_SLPSSetFrontProcessWithOptions";
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let sym = libc::dlsym(handle, name.as_ptr());
        if sym.is_null() {
            return None;
        }
        let f: Fp = std::mem::transmute::<*mut c_void, Fp>(sym);
        Some(f(psn as *const _, window_id, options))
    }
}

fn cps_event(subtype: i16, window_id: u32) -> Option<objc2::rc::Retained<CGEvent>> {
    let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::AppKitDefined,
        CGPoint::new(0.0, 0.0),
        objc2_app_kit::NSEventModifierFlags::empty(),
        0.0,
        window_id as isize,
        None,
        subtype,
        window_id as isize,
        0,
    )?;
    ev.CGEvent()
}

/// Who the workspace currently considers frontmost, as `(pid, name)`.
fn frontmost() -> Option<(i32, String)> {
    let app = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
    Some((
        app.processIdentifier(),
        app.localizedName()
            .map(|n| n.to_string())
            .unwrap_or_default(),
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: slps_click_probe <pid> <global-x> <global-y> [front cps flip global]");
        std::process::exit(2);
    }
    println!("AXIsProcessTrusted = {}", cua_ax::is_trusted());

    let flags_early: Vec<&str> = args[4..].iter().map(|s| s.as_str()).collect();
    if flags_early.contains(&"nsapp") {
        // Become an application before posting anything.
        //
        // The hypothesis this tests: `CGEventPostToPid` may only be honoured
        // from a sender the window server recognises as an app, and a bare
        // command-line tool is not one — no bundle identity, no `NSApplication`,
        // no activation policy. The shipping implementation that does make this
        // work is a signed `.app` with an ordinary developer signature and no
        // private entitlements, so identity is the remaining difference.
        //
        // `Accessory` rather than `Regular`: it gives a real app identity and a
        // window-server connection without putting anything in the Dock or
        // stealing activation, which would confound the measurement.
        let mtm = objc2_foundation::MainThreadMarker::new().expect("main thread");
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        println!(
            "NSApplication ready: bundle={:?}",
            objc2_foundation::NSBundle::mainBundle().bundleIdentifier()
        );
    }

    let pid: i32 = args[1].parse().unwrap();
    let gx: f64 = args[2].parse().unwrap();
    let gy: f64 = args[3].parse().unwrap();
    let flags: Vec<&str> = args[4..].iter().map(|s| s.as_str()).collect();
    let has = |f: &str| flags.contains(&f);

    // Pick the window that actually contains the target point, not the app's
    // biggest one. Choosing by area sent two KakaoTalk runs at an open chat
    // window instead of the conversation list, so the window id stamped on the
    // event and the window-local coordinates were both for the wrong window —
    // which reads as "the recipe failed" when nothing was ever aimed correctly.
    // Falls back to largest only when no window contains the point.
    let windows = cua_capture::list_windows().expect("list_windows");
    let mine: Vec<_> = windows
        .iter()
        .filter(|w| w.pid == pid && w.layer == 0 && w.frame.size.width > 100.0)
        .collect();
    let contains = |w: &&cua_capture::WindowInfo| {
        let f = w.frame;
        gx >= f.origin.x
            && gx < f.origin.x + f.size.width
            && gy >= f.origin.y
            && gy < f.origin.y + f.size.height
    };
    let w = mine
        .iter()
        .copied()
        .find(contains)
        .or_else(|| {
            mine.iter().copied().max_by(|a, b| {
                (a.frame.size.width * a.frame.size.height)
                    .total_cmp(&(b.frame.size.width * b.frame.size.height))
            })
        })
        .expect("no ordinary window for that pid");
    if !contains(&w) {
        println!("  WARNING: no window of this pid contains the point; using the largest");
    }
    println!("window id={} title={:?} frame={:?}", w.id, w.title, w.frame);

    let mut psn = ProcessSerialNumber::default();
    let err = unsafe { GetProcessForPID(pid, &mut psn) };
    println!("GetProcessForPID -> {err} psn={psn:?}");

    let point = if has("flip") {
        CGPoint::new(
            gx - w.frame.origin.x,
            w.frame.size.height - (gy - w.frame.origin.y),
        )
    } else {
        CGPoint::new(gx, gy)
    };
    println!("flags={flags:?} -> point ({:.0}, {:.0})", point.x, point.y);
    println!("  cursor before: {:?}", cua_hid::cursor_position().ok());

    if has("awr") {
        activate_without_raise(pid, w.id);
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    if has("front") {
        // Report who holds the foreground before and after, because
        // `_SLPSSetFrontProcessWithOptions` returning `CGError 0` means the
        // call was accepted, not that anything changed. Trusting a
        // fire-and-forget return value is the exact mistake that made an
        // earlier `CGEventPostToPid` result look like a success for a week.
        println!("  frontmost before: {:?}", frontmost());
        match set_front_process(&psn, w.id, K_CPS_USER_GENERATED) {
            Some(code) => println!("  _SLPSSetFrontProcessWithOptions -> CGError {code}"),
            None => println!("  _SLPSSetFrontProcessWithOptions NOT FOUND"),
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        println!("  frontmost after : {:?}  (target pid {pid})", frontmost());
    }

    if has("cps") {
        for (subtype, label) in [
            (K_CPS_NOTIFY_NEW_FRONT, "kCPSNotifyNewFront"),
            (K_CPS_NOTIFY_KEY_FOCUS_TAKEN, "kCPSNotifyKeyFocusTaken"),
            (
                K_CPS_NOTIFY_KEY_FOCUS_RETURNED,
                "kCPSNotifyKeyFocusReturned",
            ),
        ] {
            match cps_event(subtype, w.id) {
                Some(ev) => {
                    CGEvent::post_to_pid(pid, Some(&ev));
                    println!("  sent {label} (0x{subtype:x})");
                }
                None => println!("  FAILED to build {label}"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }

    // Which state the event source claims to come from. The shipping
    // implementation passes `1` — `HIDSystemState` — where every attempt here
    // so far used `CombinedSessionState`. An event built from the HID system
    // state is the one the window server treats as having come from a real
    // input device, which is exactly the distinction a background click would
    // turn on.
    let state = if has("hidsrc") {
        CGEventSourceStateID::HIDSystemState
    } else {
        CGEventSourceStateID::CombinedSessionState
    };
    println!("  event source state = {state:?}");
    let source = CGEventSource::new(state).expect("event source");
    for kind in [CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let event = CGEvent::new_mouse_event(Some(&source), kind, point, CGMouseButton::Left)
            .expect("mouse event");
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 1);
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::MouseEventWindowUnderMousePointer,
            w.id as i64,
        );
        if has("full") {
            for (field, value) in [
                (K_CG_MOUSE_EVENT_BUTTON_NUMBER, 0),
                (K_CG_MOUSE_EVENT_SUBTYPE, K_CG_MOUSE_SUBTYPE_DEFAULT),
                (K_CG_EVENT_TARGET_UNIX_PID, pid as i64),
            ] {
                CGEvent::set_integer_value_field(Some(&event), CGEventField(field), value);
            }
        }
        if has("psn") {
            let packed = ((psn.high_long_of_psn as u64) << 32) | psn.low_long_of_psn as u64;
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField(K_CG_EVENT_TARGET_PSN),
                packed as i64,
            );
        }
        if has("winloc") {
            let wx = gx - w.frame.origin.x;
            let wy = gy - w.frame.origin.y;
            if !set_window_location(&event, wx, wy) {
                println!("  CGEventSetWindowLocation NOT FOUND");
            }
        }
        if has("sl") {
            if !sl_event_post_to_pid(pid, &event) {
                println!("  SLEventPostToPid NOT FOUND");
            }
        } else if has("annot") {
            // The tap location is the whole answer. `HIDEventTap` is the
            // hardware stream — posting there moves the real pointer, which is
            // what every visible-cursor click in this project has been doing.
            // `AnnotatedSessionEventTap` puts the event into the session's own
            // queue instead: apps and passive taps see it as ordinary input,
            // and the physical cursor stays where the human left it.
            //
            // A captured trace of a shipping implementation clicking a
            // background app shows its events arriving through a session tap
            // with no source pid, `SourceStateID = 1` and `MouseEventSubtype
            // = 3` — i.e. this path, not `CGEventPostToPid`.
            CGEvent::post(CGEventTapLocation::AnnotatedSessionEventTap, Some(&event));
        } else if has("global") {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        } else {
            CGEvent::post_to_pid(pid, Some(&event));
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    println!("  sent mouse down/up");
    // The property under test is not only "did it land" but "did the human's
    // pointer stay put". Print both ends so the answer is in the transcript.
    println!("  cursor after : {:?}", cua_hid::cursor_position().ok());
}
