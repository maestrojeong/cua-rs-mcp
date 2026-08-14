//! Private SkyLight SPI, resolved lazily through `dlopen`/`dlsym`.
//!
//! This module is the single place in `cua-hid` that touches undocumented
//! WindowServer symbols. It exists for exactly one capability: posting a
//! *stamped* mouse event straight to a target process's window via
//! `SLEventPostToPid`, so a background or custom-drawn control can receive a
//! click without the pointer moving and without the app being raised. The
//! project previously avoided these symbols on principle (see `DESIGN.md`);
//! that stance is reversed here, deliberately and narrowly, to back the
//! pid-routed tier of the click ladder.
//!
//! Nothing is resolved until first use. If a symbol is missing every function
//! degrades to a `false`/no-op, and the caller treats that as "fall through to
//! the next layer". Resolution is per-symbol and memoized in a `OnceLock`, so a
//! missing symbol costs one failed `dlsym` per process, not per click.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

#[allow(non_camel_case_types)]
type pid_t = libc::pid_t;

type PostToPidFn = unsafe extern "C" fn(pid_t, *mut c_void);
type SetWindowLocFn = unsafe extern "C" fn(*mut c_void, f64, f64);
type SetIntFieldFn = unsafe extern "C" fn(*mut c_void, u32, i64);

/// Field indices for `SLEventSetIntegerValueField`. These are the raw
/// `CGEventField` values from the private SkyLight headers (the public
/// `CGEventField` enum has no matching arms for the window-routing ones).
///
/// `kCGMouseEventClickState` (1) and the window number (51) are deliberately
/// absent: events are now built through `NSEvent`, which sets both from its
/// `clickCount:` and `windowNumber:` arguments. Re-stamping them afterwards
/// risked contradicting the header AppKit validates the event against.
pub(crate) const BUTTON_NUMBER: u32 = 3; // kCGMouseEventButtonNumber (0=left,1=right,2=middle)
pub(crate) const SUBTYPE: u32 = 7; // kCGMouseEventSubtype (3 = touch/click)
pub(crate) const TARGET_PID: u32 = 40; // Chromium synthetic-event filter
pub(crate) const CLICK_GROUP: u32 = 58; // gesture-coalescing group id
pub(crate) const WINDOW_UNDER_MOUSE: u32 = 91; // kCGMouseEventWindowUnderMousePointer
pub(crate) const WINDOW_UNDER_MOUSE_HANDLING: u32 = 92; // ...ThatCanHandleThisEvent

static SKYLIGHT_LOADED: OnceLock<()> = OnceLock::new();

fn ensure_skylight_loaded() {
    let _ = SKYLIGHT_LOADED.get_or_init(|| {
        let path = c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
        // SAFETY: `dlopen` is handed a literal NUL-terminated path; the handle is
        // deliberately leaked so the framework stays resident for the process
        // lifetime (which is what RTLD_GLOBAL + the leaked handle achieves).
        unsafe {
            libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
        }
    });
}

fn find_sym(name: *const c_char) -> Option<*mut c_void> {
    // SAFETY: `name` is a NUL-terminated string literal. `RTLD_DEFAULT` is a
    // valid pseudo-handle that searches the already-loaded image list (which
    // includes SkyLight once `ensure_skylight_loaded` has run).
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

fn as_fn<T>(sym: *mut c_void) -> Option<T> {
    if sym.is_null() {
        None
    } else {
        // `dlsym` returns `void *`; a function pointer and a data pointer are
        // the same size on macOS, so this is the standard POSIX conversion.
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&sym) })
    }
}

fn post_to_pid_fn() -> Option<PostToPidFn> {
    static F: OnceLock<Option<PostToPidFn>> = OnceLock::new();
    *F.get_or_init(|| {
        ensure_skylight_loaded();
        find_sym(c"SLEventPostToPid".as_ptr()).and_then(as_fn)
    })
}

fn set_window_location_fn() -> Option<SetWindowLocFn> {
    static F: OnceLock<Option<SetWindowLocFn>> = OnceLock::new();
    *F.get_or_init(|| {
        ensure_skylight_loaded();
        find_sym(c"CGEventSetWindowLocation".as_ptr()).and_then(as_fn)
    })
}

fn set_integer_field_fn() -> Option<SetIntFieldFn> {
    static F: OnceLock<Option<SetIntFieldFn>> = OnceLock::new();
    *F.get_or_init(|| {
        ensure_skylight_loaded();
        find_sym(c"SLEventSetIntegerValueField".as_ptr()).and_then(as_fn)
    })
}

/// Whether the pid-routed background-click primitive is available. This is the
/// gate for the whole non-AX synthesis tier: without `SLEventPostToPid` the
/// stamped, no-warp click cannot be delivered at all.
pub(crate) fn is_available() -> bool {
    post_to_pid_fn().is_some()
        && set_window_location_fn().is_some()
        && set_integer_field_fn().is_some()
}

/// Post a prepared event record directly to `pid` via the private SkyLight
/// route. `event` is the raw `CGEventRef`. Returns false when the symbol is
/// missing; a present symbol that the server rejects still counts as attempted.
pub(crate) fn post_to_pid(pid: pid_t, event: *mut c_void) -> bool {
    match post_to_pid_fn() {
        // SAFETY: `event` is a live `CGEventRef` for the duration of the call.
        Some(f) => unsafe {
            f(pid, event);
        },
        None => return false,
    }
    true
}

/// Set an event's window-local location. No-op when the symbol is missing.
pub(crate) fn set_window_location(event: *mut c_void, x: f64, y: f64) -> bool {
    match set_window_location_fn() {
        Some(f) => unsafe {
            f(event, x, y);
        },
        None => return false,
    }
    true
}

/// Set a private integer field on an event. No-op when the symbol is missing.
pub(crate) fn set_integer_field(event: *mut c_void, field: u32, value: i64) -> bool {
    match set_integer_field_fn() {
        Some(f) => unsafe {
            f(event, field, value);
        },
        None => return false,
    }
    true
}
