//! Process-routed macOS input synthesis.
//!
//! # Read this before using this crate
//!
//! This is the only crate in the workspace that can synthesize input outside
//! the accessibility API. The server uses one entry point:
//! [`click_background_pid`], which targets one process without moving the
//! shared pointer. The older shared-input helpers below are retained only for
//! standalone diagnostic probes:
//!
//! - [`post_chord`] writes into the session's single, shared HID event
//!   stream. It is global by necessity — there is no per-app keyboard focus
//!   API worth trusting — so it moves the cursor, takes keyboard focus, and
//!   competes with whatever the human is physically doing.
//! - [`click_by_moving_pointer`] moves the real pointer to a screen point,
//!   clicks through the same shared stream, and puts the pointer back.
//!
//! [`click_background_pid`] delivers a stamped mouse event
//! straight into one process's window queue — no pointer warp, no window raise,
//! no focus steal — via the private `SLEventPostToPid` SPI that cua-driver
//! proved works where the public `CGEventPostToPid` (kept as [`post_click_to_pid`]
//! for reference) silently dropped the event. It composes six layers into one
//! call; see its documentation for the recipe and the tradeoff.
//!
//! The events themselves are built in [`nsevent`], through `NSEvent`'s class
//! factories rather than `CGEventCreateMouseEvent`, so they carry the AppKit
//! identity — event number, click count, window number — that custom-drawn views
//! validate before they will act on a click. That module also synthesizes the
//! `NSEventTypeAppKitDefined` activation notices that let a background app
//! believe it is active for the duration of a click without the real frontmost
//! app or the user's keyboard focus ever changing.
//!
//! `post_chord` exists because the Accessibility API has no general keyboard
//! verb: there is `AXConfirm` for Return and `AXCancel` for Escape, and after
//! that nothing — no way to express `⌘⇧P`, no way to drive a terminal, no way
//! to reach a canvas app that only listens for real key events.
//! `click_by_moving_pointer` exists for the mouse equivalent: some custom-drawn
//! controls (a chat app's conversation-list row, for instance) advertise no
//! `AXPress`, `AXPick`, or `AXConfirm` and only ever respond to a real click.
//! Refusing to implement either (which is effectively what OpenAI's
//! implementation does) leaves a real hole; implementing them silently would
//! destroy the property that makes the rest of this project worth using.
//!
//! So both are isolated here, and the isolation is enforced by the dependency
//! graph rather than by a comment: `cua-ax` and `cua-capture` do not depend on
//! this crate and cannot reach it. `grep -rl cua_hid crates/` enumerates every
//! call site that can touch real input.
//!
//! The cua-rs server only calls [`click_background_pid`]. Legacy shared-input
//! helpers remain as diagnostic APIs for the probe examples; no CLI flag or MCP
//! tool can reach them.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2_app_kit::NSEventType;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGWarpMouseCursorPosition,
};

mod nsevent;
mod skylight;

#[derive(Debug, Clone, thiserror::Error)]
pub enum HidError {
    /// The chord did not parse. Carries the offending token rather than the
    /// whole string, because a model that wrote `cmd+shft+p` needs to see which
    /// word was wrong.
    #[error("unknown key or modifier `{token}` in {chord:?}. Modifiers: cmd, shift, alt/option, ctrl, fn. Keys: a-z, 0-9, f1-f20, return, tab, space, escape, delete, arrows, home, end, pageup, pagedown")]
    UnknownToken { chord: String, token: String },

    #[error("chord {0:?} has modifiers but no key")]
    NoKey(String),

    #[error("could not create a HID event source; the Accessibility grant may have been revoked")]
    NoSource,

    #[error("the native background-input primitive `{0}` is unavailable on this macOS version")]
    PrimitiveUnavailable(&'static str),
}

pub type Result<T> = std::result::Result<T, HidError>;

/// A parsed key chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: u16,
    pub flags: CGEventFlags,
}

/// Parse a chord like `cmd+shift+p`, `escape`, `f5`, `ctrl+alt+delete`.
///
/// Accepts `+` or `-` as the separator and is case-insensitive, because models
/// produce all of `Cmd+Shift+P`, `cmd-shift-p` and `COMMAND+SHIFT+P`. The last
/// non-modifier token is the key; order does not matter otherwise.
///
/// `-` is only treated as a separator when the chord contains no `+`, because
/// `-` is also a key name: splitting `cmd+-` on both characters would throw the
/// key away and report "no key". Write `cmd+-` (or `cmd+minus`) for that one.
///
/// Pure and unit-tested: no events are posted, nothing is touched. This is the
/// half of the crate that can be verified without a display.
pub fn parse_chord(chord: &str) -> Result<Chord> {
    let table = key_table();
    let mut flags = CGEventFlags::empty();
    let mut key: Option<u16> = None;

    let separators: &[char] = if chord.contains('+') {
        &['+']
    } else {
        &['+', '-']
    };

    for raw in chord
        .split(separators)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let token = raw.to_lowercase();
        match token.as_str() {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::MaskCommand,
            "shift" => flags |= CGEventFlags::MaskShift,
            "alt" | "opt" | "option" => flags |= CGEventFlags::MaskAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::MaskControl,
            "fn" | "function" => flags |= CGEventFlags::MaskSecondaryFn,
            other => match table.get(other) {
                Some(&code) => key = Some(code),
                None => {
                    return Err(HidError::UnknownToken {
                        chord: chord.to_string(),
                        token: raw.to_string(),
                    })
                }
            },
        }
    }

    match key {
        Some(key) => Ok(Chord { key, flags }),
        None => Err(HidError::NoKey(chord.to_string())),
    }
}

/// Post a chord as a real key press to whatever currently has focus.
///
/// This is global: it goes to the focused app, not to a chosen one. There is no
/// `app` parameter on purpose — pretending to target an app while writing to the
/// shared HID stream would be a lie, and the honest contract is "this behaves
/// exactly as if the user pressed the keys".
pub fn post_chord(chord: Chord) -> Result<()> {
    // `CombinedSessionState` rather than a private source so the window server
    // treats these as ordinary session input and modifier state composes with
    // whatever the user is physically holding.
    let source =
        CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok_or(HidError::NoSource)?;

    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), chord.key, down)
            .ok_or(HidError::NoSource)?;
        // Flags must be set on both the down and the up event. Leaving them off
        // the key-up leaves apps that track modifier transitions believing the
        // modifier is still held.
        CGEvent::set_flags(Some(&event), chord.flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

/// Post a left click at a screen point, addressed to one process, via the
/// **public** `CGEventPostToPid` API.
///
/// **This does not deliver a usable click on current macOS.** It is kept only
/// as the reproduction harness for that finding (`cargo run -p cua-hid
/// --example click_probe`), because the API is advertised everywhere as the
/// polite way to click a background window and the failure is completely
/// silent. Use [`click_background_pid`] instead — it layers the private
/// `SLEventPostToPid` SPI and the stamping described there on top of this call.
///
/// Measured, against a TextEdit checkbox — a control accessibility *can*
/// drive, so a miss is unambiguous:
///
/// | path | checkbox value |
/// |---|---|
/// | `post_click_to_pid` (this function) | 0 → 0 |
/// | same coordinates via [`click_by_moving_pointer`]'s global HID tap | 0 → 1 |
/// | this function plus the real window id in `kCGMouseEventWindowUnderMousePointer` | 0 → 0 |
///
/// `CGEventPostToPid` returns no status and is fire-and-forget, so a caller
/// that trusts it reports every click as a success.
pub fn post_click_to_pid(pid: i32, x: f64, y: f64) -> Result<()> {
    let source =
        CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok_or(HidError::NoSource)?;
    let point = CGPoint::new(x, y);

    for (kind, button) in [
        (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (CGEventType::LeftMouseUp, CGMouseButton::Left),
    ] {
        let event = CGEvent::new_mouse_event(Some(&source), kind, point, button)
            .ok_or(HidError::NoSource)?;
        // AppKit's click-to-select/click-to-open handlers read
        // `-[NSEvent clickCount]`, which comes straight from this field. A
        // synthetic event that leaves it at its default of 0 is
        // indistinguishable from a click with no clicks in it, and some
        // custom views (table rows in particular) silently ignore that rather
        // than treating it as a single click.
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 1);
        CGEvent::post_to_pid(pid, Some(&event));
    }
    Ok(())
}

/// Whether the complete pid-routed background click recipe is available.
///
/// Posting alone is not enough: without private field stamping and a
/// window-local location the event is merely addressed to a process, not pinned
/// to the window the caller validated. Fail closed unless all three primitives
/// exist; partial delivery is indistinguishable from a successful mis-click.
pub fn skylight_available() -> bool {
    skylight::is_available()
}

/// Deliver a left click to a background process's window without moving the
/// pointer, raising the window, or stealing focus.
///
/// This is the strict-background subset of cua-driver's `click_at_xy_inner`
/// recipe and composes six layers into one call:
///
/// 1. **AppKit construction** — every event comes from an `NSEvent` class
///    factory, so it carries a fresh event number, the real click count, and the
///    target's window number. See [`nsevent`] for why building the `CGEvent`
///    directly does not work on custom-drawn views.
/// 2. **Synthesized activation** — an `ApplicationActivated` notice before the
///    click and `ApplicationDeactivated` after it, so a view that gates on
///    `NSApp.isActive` accepts the press without the real focus moving.
/// 3. **Window-local location** — `CGEventSetWindowLocation` pins the event to
///    the current target-window frame, so the click lands relative to the target
///    window's origin even if that window is off-screen or moved.
/// 4. **Private field stamping** — `SLEventSetIntegerValueField` writes the
///    button number, subtype, target pid, the two window-under-mouse fields, and
///    the click-group id that lets the server coalesce the down/up into one
///    gesture.
/// 5. **Fresh timestamp + private post route** — the timestamp is re-read
///    immediately before the event is posted exactly once via `SLEventPostToPid`.
///    A stale timestamp reads as a replay and gets coalesced away. The public
///    route is deliberately not duplicated.
/// 6. **Primer + timing** — a leading `mouseMoved` primer, a 12 ms settle,
///    28 ms between a pair's down and up, and `HUMAN_CLICK_INTERVAL_MS` between
///    pairs.
///
/// The real pointer is never moved and `NSRunningApplication.activate` is never
/// called. What replaces the latter is a *synthesized* activation notice: an
/// `NSEventTypeAppKitDefined` event with subtype `ApplicationActivated`, posted
/// to the target and balanced by `ApplicationDeactivated` afterwards. The target
/// believes it became active for the duration of the click, while the real
/// frontmost app, the user's keyboard focus and the current Space are untouched.
/// See [`nsevent::notify_app_activated`].
pub fn click_background_pid(
    target: PidClick,
    assist: Option<ActivationAssist>,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) -> Result<()> {
    let PidClick {
        pid,
        point: (x, y),
        window_local,
        wid,
        count,
    } = target;
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable(
            "SLEventPostToPid + CGEventSetWindowLocation + SLEventSetIntegerValueField",
        ));
    }
    let point = CGPoint::new(x, y);
    // AppKit window numbers and CGWindowIDs are the same integer for ordinary
    // windows, so the id the caller already revalidated doubles as the
    // `windowNumber:` argument to the `NSEvent` factories.
    let window_number = wid as isize;

    // Layer 3/5: a fresh sub-second group id ties the down/up pair together in
    // the server's gesture coalescing.
    let click_group_id = {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as i64)
            .unwrap_or(0)
    };

    // Convince the target it is active before any mouse event arrives. A view
    // that gates on `NSApp.isActive` or `-[NSWindow isKeyWindow]` decides
    // whether to accept the click at mouse-down, so this has to land first.
    // Order matters, and it is the reference implementation's order: key focus
    // first, then activation, then the window click. "You are active" and "your
    // window has key focus again" are two different statements, and a control
    // that gates on `-[NSWindow isKeyWindow]` needs the second one.
    post_activation_notice(pid, nsevent::notify_window_key_focus_returned())?;
    post_activation_notice(pid, nsevent::notify_app_activated(window_number))?;
    if let Some(assist) = assist {
        post_window_focus_click(pid, window_number, assist, click_group_id)?;
    }
    wait_until_believed_frontmost(believes_it_is_frontmost);

    post_mouse_moved_primer(pid, point, window_local, wid, click_group_id)?;
    std::thread::sleep(std::time::Duration::from_millis(12));

    let n = if count == 0 { 1 } else { count };
    let result = (|| -> Result<()> {
        for pair_index in 0..n {
            // `clickCount` counts up across a multi-click gesture: the second
            // press of a double-click carries 2, which is precisely the value
            // `-[NSEvent clickCount]` is compared against by a view that opens
            // on double-click and selects on single-click.
            let click_count = (pair_index + 1) as isize;
            // One event number per down/up pair. AppKit uses it to pair the up
            // with its own down, so the two halves must agree — and the next
            // pair must differ.
            let event_number = nsevent::next_event_number();

            let down = nsevent::mouse_event(
                NSEventType::LeftMouseDown,
                point,
                window_number,
                event_number,
                click_count,
                1.0,
            )
            .ok_or(HidError::NoSource)?;
            post_mouse_event(pid, &down, point, window_local, wid, click_group_id)?;
            std::thread::sleep(std::time::Duration::from_millis(28));

            let up = nsevent::mouse_event(
                NSEventType::LeftMouseUp,
                point,
                window_number,
                event_number,
                click_count,
                0.0,
            )
            .ok_or(HidError::NoSource)?;
            post_mouse_event(pid, &up, point, window_local, wid, click_group_id)?;

            if n > 1 && pair_index + 1 < n {
                std::thread::sleep(std::time::Duration::from_millis(HUMAN_CLICK_INTERVAL_MS));
            }
        }
        Ok(())
    })();

    // Balance the activation notice even when the click itself failed: leaving
    // the target believing it is active would change how it renders and how it
    // treats the user's *next* real click.
    let _ = post_activation_notice(pid, nsevent::notify_app_deactivated(window_number));

    result
}

/// Gap between the pairs of a multi-click gesture, in milliseconds.
///
/// Named after `AccessibilitySupport.SynthesizedEvent.humanClickInterval`, which
/// is the same idea: the pairs have to be far enough apart that the target does
/// not coalesce them, and close enough that they land inside the system
/// double-click interval. macOS defaults that interval to 500 ms, so this sits
/// comfortably inside it.
const HUMAN_CLICK_INTERVAL_MS: u64 = 80;

/// Send a key chord to one process without touching the shared keyboard.
///
/// # Status: unproven, not reachable from the MCP surface
///
/// This is the keyboard counterpart to [`click_background_pid`], and it exists
/// because the premise this crate was built on — "there is no per-app keyboard
/// API worth trusting, so keyboard input must go through the global HID tap and
/// steal focus" — turns out to be wrong. OpenAI's `SkyComputerUseService` builds
/// keyboard events with `CGEventCreateKeyboardEvent` against a
/// `kCGEventSourceStateHIDSystemState` source and posts them through the same
/// per-pid route its clicks use
/// (`SystemSoftware.CGEventAPI.createKeyboardEvent(source:virtualKey:keyDown:)`
/// feeding `SynthesizedEvent.send(to:)`), with no global tap involved.
///
/// It is deliberately *not* wired into the `press_key` MCP tool yet.
/// Keyboard input that silently lands in the wrong process is far more damaging
/// than a missed click — it can type into whatever the user is editing — so this
/// needs the same measured verification the click path got before anything calls
/// it automatically. Drive it from `examples/pid_key_probe.rs` to do that.
///
/// Unlike [`post_chord`], nothing here touches `CGEventPost`, so the user's
/// focused app keeps receiving their real typing throughout.
pub fn press_chord_background_pid(pid: i32, chord: &Chord) -> Result<()> {
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable("SLEventPostToPid"));
    }
    let source =
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(HidError::NoSource)?;

    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), chord.key, down)
            .ok_or(HidError::NoSource)?;
        CGEvent::set_flags(Some(&event), chord.flags);
        post_keyboard_event(pid, &event)?;
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    Ok(())
}

/// Type literal text into one process without touching the shared keyboard.
///
/// # Status: unproven, not reachable from the MCP surface
///
/// See [`press_chord_background_pid`] for why this exists and why nothing calls
/// it yet. The mechanism is `CGEventKeyboardSetUnicodeString` on a keycode-0
/// event, which is how you type a character that has no virtual keycode on the
/// current layout — the same call OpenAI's
/// `SystemSoftware.CGEventAPI.keyboardSetUnicodeString(_:stringLength:
/// unicodeString:)` wraps for `SynthesizedEvent.type(string:)`.
///
/// This is the path that would finally reach the targets `set_value` and
/// `type_text` cannot: terminals, canvas editors and anything else that only
/// reacts to real key events. Those are also exactly the targets where a
/// misdelivered keystroke does the most damage, hence the gating.
pub fn type_text_background_pid(pid: i32, text: &str) -> Result<()> {
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable("SLEventPostToPid"));
    }
    let source =
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(HidError::NoSource)?;

    // One event per grapheme-ish unit rather than one for the whole string:
    // `CGEventKeyboardSetUnicodeString` is documented to handle short strings,
    // and a long one is silently truncated by some receivers. UTF-16 code units
    // are the unit the API takes, and a surrogate pair must stay together, so
    // characters are the safe granularity.
    for ch in text.chars() {
        let mut utf16 = [0u16; 2];
        let encoded = ch.encode_utf16(&mut utf16);

        for down in [true, false] {
            // Keycode 0 with an explicit unicode string: the receiver reads the
            // string, not the keycode, so no layout mapping is needed.
            let event =
                CGEvent::new_keyboard_event(Some(&source), 0, down).ok_or(HidError::NoSource)?;
            // SAFETY: `encoded` points into `utf16`, which outlives this call,
            // and its length is the number of initialized code units.
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    encoded.len() as u64,
                    encoded.as_ptr(),
                );
            }
            post_keyboard_event(pid, &event)?;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    Ok(())
}

/// Stamp and post one keyboard event to `pid`.
///
/// Keyboard events carry no location and no window, so the mouse-only private
/// fields are all meaningless here; only the target pid and a fresh timestamp
/// apply.
fn post_keyboard_event(pid: i32, event: &CFRetained<CGEvent>) -> Result<()> {
    let ptr = CFRetained::as_ptr(event).as_ptr() as *mut c_void;

    CGEvent::set_timestamp(Some(event), nsevent::uptime_nanos());
    skylight::set_integer_field(ptr, skylight::TARGET_PID, pid as i64)
        .then_some(())
        .ok_or(HidError::PrimitiveUnavailable(
            "SLEventSetIntegerValueField",
        ))?;
    skylight::post_to_pid(pid, ptr)
        .then_some(())
        .ok_or(HidError::PrimitiveUnavailable("SLEventPostToPid"))
}

/// Where a pid-routed click should land, in both coordinate systems the recipe
/// needs.
///
/// The two points are not redundant. `point` is what the window server routes on
/// and what `-[NSEvent locationInWindow]` is ultimately derived from; the
/// window-local pair is what `CGEventSetWindowLocation` pins the event to, so the
/// click stays correct for a window that has moved since the snapshot. Keeping
/// both explicit means the caller — which is the only party that revalidated the
/// window — owns the conversion.
#[derive(Debug, Clone, Copy)]
pub struct PidClick {
    pub pid: i32,
    /// Screen point, in points.
    pub point: (f64, f64),
    /// The same point relative to the target window's *current* frame origin.
    pub window_local: (f64, f64),
    /// `CGWindowID` of the window the caller validated, which doubles as the
    /// AppKit window number.
    pub wid: u32,
    /// Click count: 1, or 2 for a target that only opens on a double-click.
    pub count: u8,
}

/// Everything needed to send the reference implementation's *localized* form of
/// an activation notice, rather than the bare one.
///
/// `SynthesizedEvent.notifyAppActivated(windowID:windowBounds:activationPoint:)`
/// behaves differently depending on whether the last two arguments are present.
/// With them absent it emits only the `ApplicationActivated` event — which is all
/// cua-rs used to send. With them present it emits that event *plus a mouse
/// down/up pair* aimed at the window's own activation point and pinned to the
/// window with `CGEventSetWindowLocation(point - bounds.origin)`
/// (`sky_decomp.c:1678202-1678260`). That pair is the canonical "click a window
/// to make it key" gesture, and skipping it is why an activation notice alone
/// left some controls — a chat app's header menu button, measured — still
/// refusing the click that followed.
///
/// The activation point must be the *window's*, not the target element's, and the
/// caller is expected to have confirmed that the point really belongs to the
/// window: an app is free to publish an activation point that overlaps a close
/// button, and synthesizing a click there would close the window instead of
/// focusing it.
#[derive(Debug, Clone, Copy)]
pub struct ActivationAssist {
    /// Window frame origin in screen points, for the window-local conversion.
    pub window_origin: (f64, f64),
    /// The window's `AXActivationPoint`, in screen points.
    pub activation_point: (f64, f64),
}

/// How long to let the target notice its own synthesized activation, and how
/// often to re-ask.
///
/// The timeout is generous because it is a *ceiling*, not a delay: the common
/// case is that the predicate holds on the first or second poll and this returns
/// in well under a frame. A target that never agrees costs the full budget once,
/// which is the right trade against posting a click the app will discard.
///
/// Both numbers come from the reference implementation:
/// `ApplicationUIElement.waitUntilAppBelievesItIsFrontmost(timeout:)` is called
/// with `2.0` from the inactive-app path of `sendClick`.
const ACTIVATION_WAIT_TIMEOUT_MS: u64 = 2_000;
const ACTIVATION_POLL_INTERVAL_MS: u64 = 16;

/// Block until the target agrees it is frontmost, or the budget runs out.
///
/// The activation notice from [`nsevent::notify_app_activated`] is a message, not
/// a function call: it lands in the target's event queue and only takes effect
/// once that process's own run loop drains it. Posting a mouse-down before then
/// races the target's `NSApp.isActive` flip, and a control that gates on
/// activation loses the race silently — measured on a chat app's header menu
/// button, which ignored a click that arrived 12 ms after the notice and accepted
/// the same click at the same coordinates when the activation had landed first.
///
/// Timing out is not an error. Plenty of targets are already active, never
/// publish the attribute, or simply do not care; a click is still worth
/// attempting. The reference implementation makes the same call
/// (`waitUntilAppBelievesItIsFrontmost` is `async -> ()`, not `throws`).
///
/// The predicate is injected rather than read here on purpose: answering "does
/// this app believe it is frontmost" means reading the accessibility API, and
/// `cua-hid` deliberately has no path to `cua-ax` — the dependency graph is what
/// keeps synthesized input isolated from the observation crates. The caller,
/// which already owns an accessibility handle, supplies the question.
/// Both outcomes are traced. Which one happens is the difference between "the
/// notices worked and this cost nothing" and "the notices do not move the
/// attribute, so every pid-routed click now pays the full budget" — and that is
/// not a thing to guess at by tuning the constant down.
fn wait_until_believed_frontmost(believes_it_is_frontmost: &dyn Fn() -> bool) {
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_millis(ACTIVATION_WAIT_TIMEOUT_MS);
    let mut polls = 0_u32;
    loop {
        polls += 1;
        if believes_it_is_frontmost() {
            tracing::debug!(
                polls,
                waited_ms = started.elapsed().as_millis(),
                "target reports AXFrontmost after the synthesized focus notices"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                polls,
                waited_ms = started.elapsed().as_millis(),
                "target never reported AXFrontmost; the synthesized focus notices do not move that \
                 attribute for this app, so this click paid the full activation budget"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            ACTIVATION_POLL_INTERVAL_MS,
        ));
    }
}

/// The second half of the reference implementation's localized activation notice:
/// a click on the window itself, at the point the window nominates.
///
/// This is what actually makes AppKit treat the window as key. The
/// `ApplicationActivated` event tells the *application* it is active; a window
/// only becomes key when something clicks it, which is why the reference emits
/// this pair alongside the notice whenever it knows the window's bounds and
/// activation point (`sky_decomp.c:1678202-1678260`).
///
/// It is a real click, so it is only as safe as the point it is aimed at. The
/// caller owns that check — see [`ActivationAssist`].
///
/// Fresh event numbers are used here rather than the literal `1` and `2` the
/// decompilation shows, because what AppKit needs from the field is uniqueness
/// and a down/up that belong together; reusing low constants across every click
/// in the process would make consecutive gestures look like replays of one.
fn post_window_focus_click(
    pid: i32,
    window_number: isize,
    assist: ActivationAssist,
    click_group_id: i64,
) -> Result<()> {
    let point = CGPoint::new(assist.activation_point.0, assist.activation_point.1);
    let window_local = (
        assist.activation_point.0 - assist.window_origin.0,
        assist.activation_point.1 - assist.window_origin.1,
    );
    let wid = window_number as u32;

    for (kind, pressure) in [
        (NSEventType::LeftMouseDown, 1.0_f32),
        (NSEventType::LeftMouseUp, 0.0_f32),
    ] {
        let event = nsevent::mouse_event(
            kind,
            point,
            window_number,
            nsevent::next_event_number(),
            1,
            pressure,
        )
        .ok_or(HidError::NoSource)?;
        post_mouse_event(pid, &event, point, window_local, wid, click_group_id)?;
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    Ok(())
}

/// Post one `NSEventTypeAppKitDefined` activation notice, stamped and routed the
/// same way a mouse event is.
///
/// The event carries no location, so only the timestamp and the target pid are
/// stamped; window-local placement and the mouse-only private fields would be
/// meaningless on a non-mouse event.
fn post_activation_notice(pid: i32, event: Option<Retained<CGEvent>>) -> Result<()> {
    let Some(event) = event else {
        // AppKit declined to build the event. This is not worth failing a click
        // over: the notice is an assist, not the click itself.
        return Ok(());
    };
    let ptr = nsevent::as_raw(&event);
    CGEvent::set_timestamp(Some(&event), nsevent::uptime_nanos());
    skylight::set_integer_field(ptr, skylight::TARGET_PID, pid as i64)
        .then_some(())
        .ok_or(HidError::PrimitiveUnavailable(
            "SLEventSetIntegerValueField",
        ))?;
    post_once(pid, &event, ptr)
}

/// Layer 5: a `mouseMoved` primer sent before the click so the target app has an
/// event to react to before the press arrives — a view that arms itself on
/// mouse-entered or mouse-moved never sees a press that appears out of nowhere.
///
/// Built through AppKit like the click itself, and with `clickCount` 0, which is
/// what a real move carries.
fn post_mouse_moved_primer(
    pid: i32,
    point: CGPoint,
    window_local: (f64, f64),
    wid: u32,
    click_group_id: i64,
) -> Result<()> {
    if let Some(event) = nsevent::mouse_event(
        NSEventType::MouseMoved,
        point,
        wid as isize,
        nsevent::next_event_number(),
        0,
        0.0,
    ) {
        post_mouse_event(pid, &event, point, window_local, wid, click_group_id)?;
    }
    Ok(())
}

/// Stamp (layers 2 and 3) and post (layer 4) one prepared mouse event to `pid`.
///
/// The event arrives already carrying its AppKit identity — type, event number,
/// click count and window number all came from the `NSEvent` factory — so this
/// only fills in what AppKit does not own:
///
/// - the CG-space screen location, because `NSEvent` interpreted the location it
///   was handed in flipped AppKit coordinates;
/// - the window-local location, so the click lands correctly even for a window
///   that has moved since the snapshot;
/// - the private window-routing fields the window server reads to decide which
///   window should handle the event.
///
/// Deliberately *not* stamped any more: `kCGMouseEventClickState` and the window
/// number, which `-[NSEvent clickCount]` and `-[NSEvent windowNumber]` already
/// carry from construction. Writing them again was at best redundant and at
/// worst contradicted the header AppKit validates against.
fn post_mouse_event(
    pid: i32,
    event: &Retained<CGEvent>,
    point: CGPoint,
    window_local: (f64, f64),
    wid: u32,
    click_group_id: i64,
) -> Result<()> {
    let ptr = nsevent::as_raw(event);

    // AppKit built this event from a flipped, window-relative location. The
    // window server routes on the CG-space one, so overwrite it with the screen
    // point the caller actually validated.
    CGEvent::set_location(Some(event), point);

    // A stale timestamp is treated as a replayed event and can be coalesced away
    // or dropped outright, so it is read as late as possible — immediately
    // before the post, after all stamping is done.
    CGEvent::set_timestamp(Some(event), nsevent::uptime_nanos());

    if !skylight::set_window_location(ptr, window_local.0, window_local.1) {
        return Err(HidError::PrimitiveUnavailable("CGEventSetWindowLocation"));
    }

    let set = |field: u32, value: i64| -> Result<()> {
        skylight::set_integer_field(ptr, field, value)
            .then_some(())
            .ok_or(HidError::PrimitiveUnavailable(
                "SLEventSetIntegerValueField",
            ))
    };
    set(skylight::BUTTON_NUMBER, 0)?; // left
    set(skylight::SUBTYPE, 3)?;

    let window_id = wid as i64;
    set(skylight::CLICK_GROUP, click_group_id)?;
    set(skylight::WINDOW_UNDER_MOUSE, window_id)?;
    set(skylight::WINDOW_UNDER_MOUSE_HANDLING, window_id)?;

    // Target pid is always stamped — it is the whole point of the route.
    set(skylight::TARGET_PID, pid as i64)?;

    // Post exactly once. Duplicating this through CGEventPostToPid can turn one
    // logical click into two queue entries on macOS versions where both routes
    // are accepted, and the public post carries no success signal anyway.
    post_once(pid, event, ptr)
}

/// Whether to deliver through the public `CGEventPostToPid` instead of the
/// private SkyLight route, read once from `CUA_PUBLIC_POST`.
///
/// The reference implementation posts everything through the *public* call
/// (`SystemSoftware.CGEventAPI.postToPid`, reached from `SynthesizedEvent.send`),
/// which is the same API §1 of DESIGN.md records as non-functional. Both
/// observations can hold: the measurement that condemned the public route sent an
/// event with no AppKit header, no fresh timestamp and no activation notice, so it
/// was never evidence about the route itself. Now that those are all in place the
/// comparison is finally meaningful, and it matters — if the public route works,
/// the private SPI dependency has no remaining justification.
///
/// An environment switch rather than a parameter because this is a question about
/// the machine, not about any one call site: every event in a click has to travel
/// the same way for the answer to mean anything.
fn use_public_post() -> bool {
    static PUBLIC: OnceLock<bool> = OnceLock::new();
    *PUBLIC.get_or_init(|| {
        std::env::var("CUA_PUBLIC_POST")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Deliver one prepared event to `pid` through whichever route is selected.
///
/// The public call returns nothing at all — not even "the process is gone" — so a
/// `true` here means "handed over", never "delivered". The private route is not
/// much better, but it at least reports a missing symbol.
fn post_once(pid: i32, event: &Retained<CGEvent>, ptr: *mut c_void) -> Result<()> {
    if use_public_post() {
        CGEvent::post_to_pid(pid, Some(event));
        return Ok(());
    }
    skylight::post_to_pid(pid, ptr)
        .then_some(())
        .ok_or(HidError::PrimitiveUnavailable("SLEventPostToPid"))
}

/// Read the pointer's current screen position, in points.
///
/// Uses a null-type `CGEvent` purely as a carrier: `CGEventCreate` fills it
/// with the *current* input state, so its location field is where the pointer
/// actually is. Cheaper and more accurate than tracking it ourselves.
pub fn cursor_position() -> Result<CGPoint> {
    let event = CGEvent::new(None).ok_or(HidError::NoSource)?;
    Ok(CGEvent::location(Some(&event)))
}

/// Click at a screen point by *actually moving the pointer there*, then put it
/// back where the user left it. `count` is the click count: 1 for a single
/// click, 2 for a double-click.
///
/// This is the honest last resort, and the only path measured to work on
/// controls that ignore both accessibility actions and [`post_click_to_pid`].
/// Some custom-drawn views (KakaoTalk's conversation rows, for one) ignore
/// accessibility entirely — writes and actions both report success and do
/// nothing — and only respond to input that arrived through the real event
/// stream.
///
/// There is no outbound warp. A mouse event posted to the HID tap carries a
/// location, and the window server moves the pointer to it; warping first was
/// measured to be redundant (the control toggles either way). What the warp
/// cannot be replaced by is the *return* trip, which is why one call remains
/// at the end: without it every fallback click strands the pointer somewhere
/// the user did not put it.
///
/// The pointer visibly jumps and returns within a frame or two, and the click
/// lands on whatever is topmost at that point, so the caller is responsible for
/// the window being unobscured. Results that came through here are tagged
/// `delivery: hid` for exactly that reason.
pub fn click_by_moving_pointer(x: f64, y: f64, count: u8) -> Result<()> {
    let source =
        CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok_or(HidError::NoSource)?;
    let restore_to = cursor_position()?;
    let target = CGPoint::new(x, y);

    // Announce the arrival. A view that only arms itself on mouse-entered or
    // mouse-moved never sees a pointer that teleports, and discards the click
    // that follows.
    let moved = CGEvent::new_mouse_event(
        Some(&source),
        CGEventType::MouseMoved,
        target,
        CGMouseButton::Left,
    )
    .ok_or(HidError::NoSource)?;
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&moved));
    std::thread::sleep(std::time::Duration::from_millis(30));

    for click in 1..=count.max(1) {
        // Real double-clicks are separated in time. Posting both pairs in the
        // same instant has been observed to leave the second one unprocessed;
        // 60 ms is well inside the system double-click interval (500 ms by
        // default) while still reading as deliberate.
        if click > 1 {
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        for (kind, button) in [
            (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (CGEventType::LeftMouseUp, CGMouseButton::Left),
        ] {
            let event = CGEvent::new_mouse_event(Some(&source), kind, target, button)
                .ok_or(HidError::NoSource)?;
            // This field, not the timing, is what makes a double-click a
            // double-click: AppKit reads `-[NSEvent clickCount]` straight from
            // it. Two separate events both carrying 1 are two single clicks no
            // matter how fast they arrive.
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::MouseEventClickState,
                click as i64,
            );
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }

    // Let the target process pull the events off its queue before the pointer
    // leaves. Returning too early has been observed to cancel the click on
    // views that track the pointer between down and up.
    std::thread::sleep(std::time::Duration::from_millis(40));
    CGWarpMouseCursorPosition(restore_to);
    Ok(())
}

/// Virtual key codes, keyed by the names a model is likely to produce.
///
/// These are the ANSI positions from `Carbon/Events.h`. They are *positional*:
/// `kVK_ANSI_A` is 0 regardless of the user's layout, so this table is correct
/// on a Dvorak or Korean keyboard as long as the caller means "the key where A
/// is on a US layout" — which is what a shortcut like `⌘A` actually means.
fn key_table() -> HashMap<&'static str, u16> {
    let mut m = HashMap::new();
    let letters: [(&str, u16); 26] = [
        ("a", 0),
        ("s", 1),
        ("d", 2),
        ("f", 3),
        ("h", 4),
        ("g", 5),
        ("z", 6),
        ("x", 7),
        ("c", 8),
        ("v", 9),
        ("b", 11),
        ("q", 12),
        ("w", 13),
        ("e", 14),
        ("r", 15),
        ("y", 16),
        ("t", 17),
        ("o", 31),
        ("u", 32),
        ("i", 34),
        ("p", 35),
        ("l", 37),
        ("j", 38),
        ("k", 40),
        ("n", 45),
        ("m", 46),
    ];
    m.extend(letters);

    let digits: [(&str, u16); 10] = [
        ("1", 18),
        ("2", 19),
        ("3", 20),
        ("4", 21),
        ("5", 23),
        ("6", 22),
        ("7", 26),
        ("8", 28),
        ("9", 25),
        ("0", 29),
    ];
    m.extend(digits);

    let punct: [(&str, u16); 13] = [
        ("=", 24),
        ("-", 27),
        ("minus", 27),
        ("hyphen", 27),
        ("]", 30),
        ("[", 33),
        ("'", 39),
        (";", 41),
        ("\\", 42),
        (",", 43),
        ("/", 44),
        (".", 47),
        ("`", 50),
    ];
    m.extend(punct);

    let named: [(&str, u16); 20] = [
        ("return", 36),
        ("enter", 36),
        ("tab", 48),
        ("space", 49),
        ("delete", 51),
        ("backspace", 51),
        ("escape", 53),
        ("esc", 53),
        ("forwarddelete", 117),
        ("home", 115),
        ("end", 119),
        ("pageup", 116),
        ("pagedown", 121),
        ("left", 123),
        ("right", 124),
        ("down", 125),
        ("up", 126),
        ("help", 114),
        ("clear", 71),
        ("numpadenter", 76),
    ];
    m.extend(named);

    // F1-F20. Non-contiguous in Carbon, so spelled out.
    let fkeys: [(&str, u16); 20] = [
        ("f1", 122),
        ("f2", 120),
        ("f3", 99),
        ("f4", 118),
        ("f5", 96),
        ("f6", 97),
        ("f7", 98),
        ("f8", 100),
        ("f9", 101),
        ("f10", 109),
        ("f11", 103),
        ("f12", 111),
        ("f13", 105),
        ("f14", 107),
        ("f15", 113),
        ("f16", 106),
        ("f17", 64),
        ("f18", 79),
        ("f19", 80),
        ("f20", 90),
    ];
    m.extend(fkeys);

    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_key() {
        let c = parse_chord("escape").unwrap();
        assert_eq!(c.key, 53);
        assert!(c.flags.is_empty(), "a bare key must carry no modifiers");
    }

    #[test]
    fn parses_modifiers_in_any_order_and_any_case() {
        let a = parse_chord("cmd+shift+p").unwrap();
        let b = parse_chord("Shift+Command+P").unwrap();
        let c = parse_chord("SHIFT-CMD-p").unwrap();
        assert_eq!(a, b, "order must not matter");
        assert_eq!(a, c, "case and separator must not matter");
        assert_eq!(a.key, 35);
        assert!(a.flags.contains(CGEventFlags::MaskCommand));
        assert!(a.flags.contains(CGEventFlags::MaskShift));
    }

    #[test]
    fn accepts_every_alias_for_the_same_modifier() {
        for alias in ["alt", "opt", "option"] {
            let c = parse_chord(&format!("{alias}+a")).unwrap();
            assert!(
                c.flags.contains(CGEventFlags::MaskAlternate),
                "{alias} failed"
            );
        }
        for alias in ["cmd", "command", "meta", "super"] {
            let c = parse_chord(&format!("{alias}+a")).unwrap();
            assert!(
                c.flags.contains(CGEventFlags::MaskCommand),
                "{alias} failed"
            );
        }
    }

    #[test]
    fn function_keys_are_not_off_by_one() {
        // F1-F4 are non-contiguous and descending in Carbon; a naive
        // `112 + n` table gets every one of them wrong.
        assert_eq!(parse_chord("f1").unwrap().key, 122);
        assert_eq!(parse_chord("f2").unwrap().key, 120);
        assert_eq!(parse_chord("f3").unwrap().key, 99);
        assert_eq!(parse_chord("f5").unwrap().key, 96);
        assert_eq!(parse_chord("f12").unwrap().key, 111);
    }

    #[test]
    fn digit_five_and_six_are_not_swapped() {
        // kVK_ANSI_5 is 23 and kVK_ANSI_6 is 22 -- the one pair that is out of
        // order, and the easiest thing in this table to get wrong.
        assert_eq!(parse_chord("5").unwrap().key, 23);
        assert_eq!(parse_chord("6").unwrap().key, 22);
    }

    #[test]
    fn aliases_resolve_to_the_same_code() {
        assert_eq!(
            parse_chord("return").unwrap().key,
            parse_chord("enter").unwrap().key
        );
        assert_eq!(
            parse_chord("delete").unwrap().key,
            parse_chord("backspace").unwrap().key
        );
        assert_eq!(
            parse_chord("escape").unwrap().key,
            parse_chord("esc").unwrap().key
        );
    }

    #[test]
    fn a_typo_names_the_offending_token_not_the_whole_chord() {
        let err = parse_chord("cmd+shft+p").unwrap_err();
        match err {
            HidError::UnknownToken { token, .. } => assert_eq!(token, "shft"),
            other => panic!("expected UnknownToken, got {other:?}"),
        }
    }

    #[test]
    fn modifiers_without_a_key_are_rejected() {
        assert!(matches!(
            parse_chord("cmd+shift").unwrap_err(),
            HidError::NoKey(_)
        ));
        assert!(matches!(parse_chord("").unwrap_err(), HidError::NoKey(_)));
    }

    #[test]
    fn separators_and_whitespace_are_tolerated() {
        let c = parse_chord(" cmd + shift + p ").unwrap();
        assert_eq!(c.key, 35);
        assert!(c.flags.contains(CGEventFlags::MaskCommand));
    }

    #[test]
    fn the_minus_key_survives_being_the_separator_character() {
        // "-" is both a separator and a key name. When a "+" is present it is
        // the separator, so "-" can be the key.
        let c = parse_chord("cmd+-").unwrap();
        assert_eq!(c.key, 27, "cmd+- must reach the minus key");
        assert!(c.flags.contains(CGEventFlags::MaskCommand));

        // The spelled-out alias always works, whichever separator is in use.
        assert_eq!(parse_chord("cmd-minus").unwrap().key, 27);
        assert_eq!(parse_chord("cmd+minus").unwrap().key, 27);
    }

    #[test]
    fn dash_separated_chords_still_work() {
        let dashed = parse_chord("cmd-shift-p").unwrap();
        let plussed = parse_chord("cmd+shift+p").unwrap();
        assert_eq!(dashed, plussed);
    }
}
