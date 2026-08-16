//! Process-routed macOS input synthesis.
//!
//! # Read this before using this crate
//!
//! This is the only crate in the workspace that can synthesize input outside
//! the accessibility API. Every entry point the server uses targets one process
//! and never touches the shared pointer:
//!
//! | | |
//! |---|:--|
//! | [`click_background_pid`] | a press and release at a point, with a button, modifiers and a click count |
//! | [`drag_background_pid`] | a press, a run of interpolated moves, and a release |
//! | [`move_mouse_background_pid`] | a bare `mouseMoved`, for hover-revealed UI |
//! | [`scroll_background_pid`] | a `scrollWheel` with a pixel or line delta |
//! | [`press_chord_background_pid`] | a key or chord |
//!
//! They share one model — `{origin, destination, button, modifiers,
//! click_count}` — and one delivery path, so widening what cua-rs can express
//! never widens *how* it is delivered.
//!
//! **No shared-input helper is left.** There is no function here that writes to
//! the session's single HID event stream, and the absence is checkable rather
//! than promised: `grep -rn 'CGEvent::post' crates/*/src/` returns only
//! `post_to_pid` calls, which name a target process. `CGEventPost` itself, the
//! one call that writes to the shared stream, appears nowhere. Its
//! `CGEventTapLocation` argument survives in exactly one place, `humanwatch`,
//! where it names where to *create* a listen-only tap and is never handed to a
//! post. The same kind of check covers the cursor: `CGWarpMouseCursorPosition`
//! is not imported anywhere in the workspace.
//!
//! Two functions used to be here and are gone, both for the same reason and in
//! the same shape. `click_by_moving_pointer` warped the real pointer to a screen
//! point, clicked through the shared stream, and put the pointer back; it
//! existed for custom-drawn controls that publish no `AXPress` and only respond
//! to a real click. `post_chord` posted a key or chord through the shared
//! keyboard stream to whatever app held focus; it existed because at the time
//! there was believed to be no per-app keyboard route, so an arbitrary chord and
//! a terminal were reachable only by taking the human's focus. Both premises
//! expired: `click_background_pid` needs no `Element`, so a bare point in a
//! window is deliverable without ever touching the cursor, and
//! [`press_chord_background_pid`] / [`type_text_background_pid`] deliver keys
//! per-pid, which is what `press_key` and `type_text mechanism: "keystrokes"`
//! now use. Keeping either one in the tree once its whole justification had
//! evaporated was leaving a temptation, not a fallback.
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
//! Everything that can synthesize input at all is isolated here, and the
//! isolation is enforced by the dependency graph rather than by a comment:
//! `cua-ax` and `cua-capture` do not depend on this crate and cannot reach it.
//! `grep -rl cua_hid crates/` enumerates every call site that can touch real
//! input.
//!
//! # `press_chord_background_pid`, promoted from unverified to primary
//!
//! This used to sit here unreachable from the server — "written but unproven"
//! in the original docs, gated out because a keystroke that lands in the
//! wrong process is worse than a click that does not land, and nothing had
//! measured that it lands in the right one. `cua-core`'s `press_key` now calls
//! it as the *only* tier (no AX verb attempted at all, not even as a fallback),
//! because accessibility has no vocabulary for a key press beyond `AXConfirm`
//! and `AXCancel`: a real event is the only thing an arbitrary chord could ever
//! become, so there is no second tier to fall back to.
//!
//! That is an argument for the design, not evidence that it lands correctly.
//! It reverses the caution the original gate encoded rather than satisfying it,
//! and the risk that gate named has not gone away: these events carry
//! no target *element*, only a target *pid*, so they land wherever that
//! process's own first responder currently is. `cua-core` mitigates this by
//! best-effort-focusing the addressed element (`AXFocused`) before sending a
//! chord through this crate, but accessibility does not make every element
//! settably focused, and there is no query here that could tell a caller
//! whether the focus actually moved before the keystrokes did.
//! `CUA_KEY_AX_ONLY=1` (read in `cua-core`) is the way back to the old,
//! AX-verb-only keyboard path (`return`/`escape`/`up`/`down` only) if this
//! proves untrustworthy on a given app.
//!
//! [`type_text_background_pid`] is written for the same reason and reached only
//! on request, for the opposite one: a bulk text write is the single operation
//! accessibility expresses better than events can. One `AXValue` write replaces
//! the whole string atomically, addressed at the element, where the same text as
//! keystrokes is a long stream landing on whatever holds focus — multiplying the
//! focus risk above by the length of the string for nothing in return. So
//! `cua-core`'s `set_value`/`type_text` keep the AX write by default and do not
//! follow this crate's click/key precedent; `type_text mechanism: "keystrokes"`
//! is the explicit opt-in, for the terminals and canvas editors where the better
//! mechanism does nothing at all.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2_app_kit::NSEventType;
use objc2_core_foundation::{CFRetained, CGPoint};
// Two names are deliberately absent from this list, and their absence is
// checkable rather than promised. `CGWarpMouseCursorPosition` is the only API
// that can move the user's cursor, and nothing in the workspace imports it.
// `CGEventTapLocation` is the only argument `CGEventPost` takes, and it left this
// file with `post_chord`: without it, no code below can write to the session's
// shared event stream at all. Every post here is `post_to_pid`, which names a
// target process. The one surviving use of `CGEventTapLocation` in this crate's
// `src/` is in `humanwatch`, where it says where to create a listen-only tap and
// is never handed to a post.
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};

pub mod humanwatch;
mod nsevent;
mod skylight;

/// Re-exported so a caller can name a modifier set without taking its own
/// dependency on `objc2-core-graphics`. [`parse_modifiers`] and [`parse_chord`]
/// both produce one of these, and `CGEventFlags::empty()` is "no modifiers".
pub use objc2_core_graphics::CGEventFlags as Modifiers;

#[derive(Debug, Clone, thiserror::Error)]
pub enum HidError {
    /// The chord did not parse. Carries the offending token rather than the
    /// whole string, because a model that wrote `cmd+shft+p` needs to see which
    /// word was wrong.
    #[error("unknown key or modifier `{token}` in {chord:?}. Modifiers: cmd, shift, alt/option, ctrl, fn. Keys: a-z, 0-9, f1-f20, return, tab, space, escape, delete, arrows, home, end, pageup, pagedown")]
    UnknownToken { chord: String, token: String },

    #[error("chord {0:?} has modifiers but no key")]
    NoKey(String),

    /// A modifier list contained something that is not a modifier. Separate
    /// from [`HidError::UnknownToken`] because the vocabularies differ: a
    /// modifier list has no key in it, so naming the key table in the message
    /// would send the caller looking in the wrong place.
    #[error(
        "unknown modifier `{token}` in {modifiers:?}. Modifiers: cmd, shift, alt/option, ctrl, fn"
    )]
    UnknownModifier { modifiers: String, token: String },

    #[error("unknown mouse button {0:?}. Buttons: left, right, middle")]
    UnknownButton(String),

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
    /// The literal character the caller asked for, when they asked for one.
    ///
    /// `Some('x')` for `"x"`, `None` for `"escape"`, `"f5"` or anything with a
    /// modifier. It exists because a keycode is not a character: a keycode is an
    /// instruction to the *input method*, which is free to turn it into
    /// something else. Under a Korean 2-set source, keycode 7 arrives as `ㅌ`,
    /// which is the correct behaviour for a person typing and the wrong one for
    /// a caller who asked for `x`. See [`press_chord_background_pid`].
    pub literal: Option<char>,
}

/// Which physical button a synthesized mouse event carries.
///
/// The three macOS buttons that have their own `NSEventType` family. Anything
/// beyond them travels as `otherMouse*` with a button number, which no app
/// cua-rs targets has been observed to want, so it is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// Parse `left`, `right` or `middle`, case-insensitively.
    ///
    /// An empty or whitespace-only string is [`MouseButton::Left`], so a caller
    /// that always passes the field through can leave it blank and get the
    /// default rather than an error.
    pub fn parse(button: &str) -> Result<Self> {
        match button.trim().to_lowercase().as_str() {
            "" | "left" | "primary" => Ok(MouseButton::Left),
            "right" | "secondary" | "context" => Ok(MouseButton::Right),
            "middle" | "center" | "centre" | "wheel" => Ok(MouseButton::Middle),
            _ => Err(HidError::UnknownButton(button.to_string())),
        }
    }

    /// `kCGMouseEventButtonNumber`: 0 left, 1 right, 2 middle.
    fn number(self) -> i64 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
        }
    }

    /// The three `NSEventType`s this button's gesture is made of, in the order
    /// they are sent: down, dragged, up.
    ///
    /// AppKit keeps a separate type per button rather than a shared type with a
    /// button field, and a view implementing `rightMouseDown:` will never see a
    /// `leftMouseDown` no matter what button number is stamped on it — so the
    /// type, not the stamped field, is what actually selects the handler.
    fn types(self) -> (NSEventType, NSEventType, NSEventType) {
        match self {
            MouseButton::Left => (
                NSEventType::LeftMouseDown,
                NSEventType::LeftMouseDragged,
                NSEventType::LeftMouseUp,
            ),
            MouseButton::Right => (
                NSEventType::RightMouseDown,
                NSEventType::RightMouseDragged,
                NSEventType::RightMouseUp,
            ),
            MouseButton::Middle => (
                NSEventType::OtherMouseDown,
                NSEventType::OtherMouseDragged,
                NSEventType::OtherMouseUp,
            ),
        }
    }

    /// How this button is spelled in a result line.
    pub fn as_str(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
}

/// The modifier flag a token names, or `None` if the token is not a modifier.
///
/// The single source of the modifier vocabulary. [`parse_chord`] and
/// [`parse_modifiers`] both go through it, so `cmd+shift` means the same thing
/// in `press_key` as it does on a click and neither can grow an alias the other
/// does not have.
fn modifier_flag(token: &str) -> Option<CGEventFlags> {
    match token {
        "cmd" | "command" | "meta" | "super" => Some(CGEventFlags::MaskCommand),
        "shift" => Some(CGEventFlags::MaskShift),
        "alt" | "opt" | "option" => Some(CGEventFlags::MaskAlternate),
        "ctrl" | "control" => Some(CGEventFlags::MaskControl),
        "fn" | "function" => Some(CGEventFlags::MaskSecondaryFn),
        _ => None,
    }
}

/// Split a chord-shaped string into its tokens.
///
/// `-` is only a separator when there is no `+`, for the reason [`parse_chord`]
/// explains: `-` is also a key name.
fn chord_tokens(s: &str) -> impl Iterator<Item = &str> {
    let separators: &[char] = if s.contains('+') { &['+'] } else { &['+', '-'] };
    s.split(separators).map(str::trim).filter(|t| !t.is_empty())
}

/// Parse a modifier list like `cmd`, `cmd+shift`, `alt-shift` — the same
/// vocabulary and the same separators [`parse_chord`] accepts, minus the key.
///
/// An empty or whitespace-only string is no modifiers at all, which is what a
/// caller that always forwards an optional field wants. Anything that is not a
/// modifier is an error rather than being ignored: a caller who wrote
/// `cmd+click` meant something, and silently dropping `click` would deliver a
/// plain ⌘-click that looks like it worked.
///
/// Pure and unit-tested; posts nothing.
pub fn parse_modifiers(modifiers: &str) -> Result<CGEventFlags> {
    let mut flags = CGEventFlags::empty();
    for raw in chord_tokens(modifiers) {
        match modifier_flag(&raw.to_lowercase()) {
            Some(flag) => flags |= flag,
            None => {
                return Err(HidError::UnknownModifier {
                    modifiers: modifiers.to_string(),
                    token: raw.to_string(),
                })
            }
        }
    }
    Ok(flags)
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
    let mut literal: Option<char> = None;

    for raw in chord_tokens(chord) {
        let token = raw.to_lowercase();
        if let Some(flag) = modifier_flag(&token) {
            flags |= flag;
            continue;
        }
        match table.get(token.as_str()) {
            Some(&code) => {
                key = Some(code);
                // Remembered only for a single-character token: `"x"` names a
                // character, `"escape"` and `"f5"` name a key that has none.
                let mut chars = token.chars();
                literal = match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                };
            }
            None => {
                return Err(HidError::UnknownToken {
                    chord: chord.to_string(),
                    token: raw.to_string(),
                })
            }
        }
    }

    match key {
        Some(key) => Ok(Chord {
            key,
            flags,
            // A modifier changes what the keystroke *means*, so the literal is
            // dropped: `cmd+x` is Cut, not the letter x, and forcing a character
            // onto it would be a different event.
            literal: literal.filter(|_| !flags.intersects(modifier_mask())),
        }),
        None => Err(HidError::NoKey(chord.to_string())),
    }
}

/// Every flag [`modifier_flag`] can produce, as one mask.
///
/// Used to ask "did the caller name a modifier at all", which is a different
/// question from "is this flag set" — a synthesized event can carry incidental
/// bits, and only the ones a caller asked for should change the recipe.
fn modifier_mask() -> CGEventFlags {
    CGEventFlags::MaskCommand
        | CGEventFlags::MaskShift
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskControl
        | CGEventFlags::MaskSecondaryFn
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
/// | same coordinates through the global HID tap, pointer warped there | 0 → 1 |
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

/// Deliver a click to a background process's window without moving the
/// pointer, raising the window, or stealing focus.
///
/// The button, the modifier flags and the click count all come from
/// [`PidClick`]. A right click here is a real `rightMouseDown`/`rightMouseUp`
/// pair, not `AXShowMenu`: the controls that most need a context menu are
/// exactly the custom-drawn ones that advertise no accessibility action to
/// perform.
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
        button,
        modifiers,
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
    let click_group_id = fresh_click_group_id();

    // Convince the target it is active before any mouse event arrives. A view
    // that gates on `NSApp.isActive` or `-[NSWindow isKeyWindow]` decides
    // whether to accept the click at mouse-down, so this has to land first.
    // Order matters: key focus first, then activation, then the window click.
    // "You are active" and "your
    // window has key focus again" are two different statements, and a control
    // that gates on `-[NSWindow isKeyWindow]` needs the second one.
    post_activation_notice(pid, nsevent::notify_window_key_focus_returned())?;
    post_activation_notice(pid, nsevent::notify_app_activated(window_number))?;
    if let Some(assist) = assist {
        post_window_focus_click(pid, window_number, assist, click_group_id)?;
    }
    wait_until_believed_frontmost(believes_it_is_frontmost);

    let route = MouseRoute {
        pid,
        wid,
        button,
        modifiers,
        click_group_id,
    };
    post_mouse_moved_primer(&route, point, window_local)?;
    std::thread::sleep(std::time::Duration::from_millis(12));

    let (down_type, _, up_type) = button.types();
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

            route.post(MouseEventSpec {
                kind: down_type,
                point,
                window_local,
                event_number,
                click_count,
                pressure: 1.0,
            })?;
            std::thread::sleep(std::time::Duration::from_millis(28));

            route.post(MouseEventSpec {
                kind: up_type,
                point,
                window_local,
                event_number,
                click_count,
                pressure: 0.0,
            })?;

            if n > 1 && pair_index + 1 < n {
                std::thread::sleep(std::time::Duration::from_millis(HUMAN_CLICK_INTERVAL_MS));
            }
        }
        Ok(())
    })();

    // Deliberately *not* balanced with an `ApplicationDeactivated` by default:
    // see `deactivate_after_click` for the measurement that removed it.
    if deactivate_after_click() {
        let _ = post_activation_notice(pid, nsevent::notify_app_deactivated(window_number));
    }

    result
}

/// Gap between the pairs of a multi-click gesture, in milliseconds.
///
/// The pairs have to be far enough apart that the target does not coalesce them,
/// and close enough that they land inside the system double-click interval.
/// macOS defaults that interval to 500 ms, so this sits comfortably inside it.
const HUMAN_CLICK_INTERVAL_MS: u64 = 80;

/// Ask a target process to notice it has key focus, before any keyboard event
/// is posted to it.
///
/// Keyboard events carry no window number, so unlike a click there is no
/// per-event field that pins them to a window — but the same open question
/// applies: does a pid-addressed `CGEvent` even reach the target's real event
/// loop if that process does not believe it is active? Nothing in this crate
/// answers that with certainty (see the module docs' "promoted from unverified
/// to primary" note), so this sends the same two AppKit-level notices
/// [`click_background_pid`] sends before its first mouse event, on the theory
/// that whatever makes a synthesized click land also improves the odds for a
/// synthesized keystroke. `window_number` is best-effort: pass the window the
/// caller already resolved for this pid, or `None` to send only the bare
/// `ApplicationActivated` notice.
///
/// Never fails the caller: an activation notice is an assist, not the
/// keystroke itself, and a target that ignores it is no worse off than if this
/// had not been called.
pub fn prime_keyboard_target(
    pid: i32,
    window_number: Option<isize>,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) {
    let _ = post_activation_notice(pid, nsevent::notify_window_key_focus_returned());
    let _ = post_activation_notice(
        pid,
        nsevent::notify_app_activated(window_number.unwrap_or(0)),
    );
    wait_until_believed_frontmost(believes_it_is_frontmost);
}

/// Send a key chord to one process without touching the shared keyboard.
///
/// # Status: reachable from the MCP surface as of the pid-first keyboard tier
///
/// This is the keyboard counterpart to [`click_background_pid`], and it exists
/// because the premise this crate was built on — "there is no per-app keyboard
/// API worth trusting, so keyboard input must go through the global HID tap and
/// steal focus" — turns out to be wrong. `CGEventCreateKeyboardEvent` against a
/// `kCGEventSourceStateHIDSystemState` source can be posted through the same
/// per-pid route the clicks here use, with no global tap involved.
///
/// See the module docs for what changed: this used to be unreachable from the
/// server, deliberately, pending the verification a missed click does not need
/// and a misdelivered keystroke does. `cua-core` now calls it as the primary
/// keyboard tier; call [`prime_keyboard_target`] first so the target has a
/// chance to notice it should accept the keystroke that follows.
///
/// Nothing here touches `CGEventPost`, so the user's focused app keeps
/// receiving their real typing throughout. There is no longer a shared-stream
/// keyboard function to contrast this with: `post_chord` was deleted once this
/// one became `press_key`'s only tier.
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
        // Say the character as well as the key, when the caller named one.
        //
        // A keycode is an instruction to the input method, not a character.
        // Measured on this machine under a Korean 2-set source: `press_key x`
        // with the keycode alone arrives as `ㅌ`, which is exactly right for a
        // person typing and exactly wrong for a caller who asked for `x`. The
        // event keeps its real keycode — so an app that reads `keyCode` for a
        // shortcut or a game control still sees the physical key it expects —
        // and carries the literal character alongside it, which is what AppKit
        // hands to a text view. Both candidate recipes were measured; this one
        // was chosen over a keycode-0 event because it is the one that does not
        // lie about which key was pressed.
        //
        // Only for an unmodified single character: `chord.literal` is already
        // `None` for `escape`, `f5` and anything with a modifier, because
        // `cmd+x` means Cut rather than the letter x.
        if let Some(c) = chord.literal {
            let encoded: Vec<u16> = c.to_string().encode_utf16().collect();
            // SAFETY: `encoded` outlives the call and its length is the number
            // of initialized code units.
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    encoded.len() as u64,
                    encoded.as_ptr(),
                );
            }
        }
        post_keyboard_event(pid, &event)?;
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    Ok(())
}

/// The characters AppKit should report for a chord: `(characters,
/// charactersIgnoringModifiers)`.
///
/// `-[NSEvent characters]` is not derivable from a keycode by anything outside
/// the input method, so this is a deliberate, partial reconstruction of it —
/// enough for the keys a menu cares about. Three cases:
///
/// - a caller-named literal (`press_key x`) is used verbatim, for the same
///   reason [`press_chord_background_pid`] stamps it onto the Unicode string:
///   the caller asked for a character, not for whatever the current input
///   source makes of that keycode;
/// - a navigation or editing key becomes its `NSxxxFunctionKey` constant, which
///   is the private-use code point AppKit itself puts in `characters` for it —
///   an arrow key is `U+F700..U+F703`, not an empty string;
/// - anything else falls back to the single-character name the key table knows
///   it by, so `cmd+t` reports `"t"`.
///
/// Empty for a key this does not model — a function key, a modifier — which is
/// honest rather than approximate: `characters` is documented to be empty for a
/// modifier key, and no menu reads F13.
///
/// Pure and unit-tested; posts nothing.
pub fn key_characters(chord: &Chord) -> (String, String) {
    // `NSUpArrowFunctionKey` and friends, from `NSEvent.h`. Spelled out rather
    // than computed because the block is not contiguous in a useful way.
    const FUNCTION_KEYS: [(u16, u32); 13] = [
        (126, 0xF700), // up
        (125, 0xF701), // down
        (123, 0xF702), // left
        (124, 0xF703), // right
        (114, 0xF746), // help
        (115, 0xF729), // home
        (119, 0xF72B), // end
        (116, 0xF72C), // page up
        (121, 0xF72D), // page down
        (117, 0xF728), // forward delete
        (36, 0x000D),  // return
        (76, 0x0003),  // enter (ETX, which is what AppKit reports)
        (53, 0x001B),  // escape
    ];
    const CONTROL_KEYS: [(u16, char); 3] = [(48, '\t'), (49, ' '), (51, '\u{8}')];

    let literal = chord
        .literal
        .map(String::from)
        .or_else(|| {
            FUNCTION_KEYS
                .iter()
                .find(|(code, _)| *code == chord.key)
                .and_then(|(_, ch)| char::from_u32(*ch))
                .map(String::from)
        })
        .or_else(|| {
            CONTROL_KEYS
                .iter()
                .find(|(code, _)| *code == chord.key)
                .map(|(_, ch)| String::from(*ch))
        })
        .or_else(|| {
            key_table()
                .iter()
                .filter(|(name, code)| **code == chord.key && name.chars().count() == 1)
                .map(|(name, _)| (*name).to_string())
                .next()
        })
        .unwrap_or_default();
    // The two strings differ only under a modifier that changes the glyph — ⌥E
    // gives `´` and `e` — and reconstructing that would mean reimplementing the
    // active keyboard layout. Reporting the same string for both is the honest
    // approximation: it is right for every unmodified key and for ⌘-chords,
    // which is what a menu reads.
    (literal.clone(), literal)
}

/// Send a key chord to one process, stamped with the window it is meant for.
///
/// # Status: experimental, and see DESIGN §10 for what it was measured to do
///
/// [`press_chord_background_pid`] builds its events with
/// `CGEventCreateKeyboardEvent`, which produces an event with no AppKit
/// identity: window number 0, `-[NSEvent window]` nil. That is correct for a
/// key aimed at an application's first responder, and it is the shipped path.
///
/// A pop-up menu is not a first responder, though — it is a window running its
/// own tracking loop, and `windowNumber` is the field that says which window an
/// event belongs to. This builds the event through
/// `-[NSEvent keyEventWithType:…windowNumber:…]` instead, so the number can be
/// the menu's own, and reconstructs `characters` with [`key_characters`]
/// because AppKit will not derive it for a synthesized event.
///
/// Same delivery route as everything else here: stamped for the target pid and
/// posted with `SLEventPostToPid`. Nothing touches the shared keyboard.
pub fn press_chord_in_window_pid(pid: i32, window_number: isize, chord: &Chord) -> Result<()> {
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable("SLEventPostToPid"));
    }
    let (characters, ignoring) = key_characters(chord);
    let modifiers = nsevent::appkit_modifiers(chord.flags);

    for down in [true, false] {
        let event = nsevent::key_event(
            down,
            modifiers,
            window_number,
            &characters,
            &ignoring,
            chord.key,
        )
        .ok_or(HidError::NoSource)?;
        // The CG record carries its own flags, read by anything that looks at
        // `CGEventGetFlags` rather than at the AppKit header.
        CGEvent::set_flags(Some(&event), chord.flags);
        CGEvent::set_timestamp(Some(&event), nsevent::uptime_nanos());
        post_raw_to_pid(pid, nsevent::as_raw(&event))?;
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    Ok(())
}

/// Type literal text into one process without touching the shared keyboard.
///
/// # Status: reachable from the MCP surface as of the pid-first keyboard tier
///
/// See [`press_chord_background_pid`] for why this exists and what changed.
/// The mechanism is `CGEventKeyboardSetUnicodeString` on a keycode-0
/// event, which is how you type a character that has no virtual keycode on the
/// current layout.
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
    CGEvent::set_timestamp(Some(event), nsevent::uptime_nanos());
    post_raw_to_pid(pid, CFRetained::as_ptr(event).as_ptr() as *mut c_void)
}

/// Stamp a raw `CGEventRef` for `pid` and post it. The half of
/// [`post_keyboard_event`] that does not care how the event was built.
fn post_raw_to_pid(pid: i32, ptr: *mut c_void) -> Result<()> {
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
    /// Which button. Right produces a real `rightMouseDown`/`rightMouseUp`
    /// pair rather than `AXShowMenu`, which is the only thing that reaches a
    /// control advertising no accessibility actions.
    pub button: MouseButton,
    /// Modifier keys the click should appear to be held down with, e.g.
    /// `MaskCommand` for a ⌘-click. Build one with [`parse_modifiers`].
    pub modifiers: CGEventFlags,
}

/// A press at one point, a run of moves, and a release at another — all pinned
/// to one window.
///
/// The two endpoints are given as screen points plus the window's current frame
/// origin, rather than as two pre-converted window-local pairs the way
/// [`PidClick`] does it, because the intermediate points are computed *here*:
/// handing the conversion back to the caller would mean converting a list whose
/// length this module chooses. One origin covers the whole gesture, which is
/// also the statement that a drag never crosses a window boundary.
#[derive(Debug, Clone, Copy)]
pub struct PidDrag {
    pub pid: i32,
    /// `CGWindowID` of the window both endpoints were validated against.
    pub wid: u32,
    /// Frame origin of that window, in screen points, read immediately before
    /// the drag.
    pub window_origin: (f64, f64),
    /// Where the button goes down, in screen points.
    pub origin: (f64, f64),
    /// Where it comes back up, in screen points.
    pub destination: (f64, f64),
    pub button: MouseButton,
    pub modifiers: CGEventFlags,
}

/// A single `mouseMoved` event at a point in a window.
///
/// The real pointer is not involved and does not move; this is a synthetic
/// event that makes the target *believe* the pointer arrived, which is what
/// hover-revealed UI reacts to.
#[derive(Debug, Clone, Copy)]
pub struct PidMouseMove {
    pub pid: i32,
    pub point: (f64, f64),
    pub window_local: (f64, f64),
    pub wid: u32,
    pub modifiers: CGEventFlags,
}

/// A `scrollWheel` event with a delta, aimed at a point in a window.
#[derive(Debug, Clone, Copy)]
pub struct PidScroll {
    pub pid: i32,
    pub point: (f64, f64),
    pub window_local: (f64, f64),
    pub wid: u32,
    /// Vertical delta. Positive scrolls *up* — that is, content moves down —
    /// which is the sign convention `CGEventCreateScrollWheelEvent2` uses.
    pub delta_y: i32,
    /// Horizontal delta. Positive scrolls left.
    pub delta_x: i32,
    pub unit: ScrollUnit,
    pub modifiers: CGEventFlags,
}

/// What a scroll delta is counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollUnit {
    /// Points of content. What a trackpad sends, and what a web view or an
    /// Electron list expects.
    #[default]
    Pixel,
    /// Wheel notches. What a physical mouse wheel sends; a receiver is free to
    /// turn one line into any number of points.
    Line,
}

impl ScrollUnit {
    fn cg(self) -> CGScrollEventUnit {
        match self {
            ScrollUnit::Pixel => CGScrollEventUnit::Pixel,
            ScrollUnit::Line => CGScrollEventUnit::Line,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScrollUnit::Pixel => "pixel",
            ScrollUnit::Line => "line",
        }
    }
}

/// Everything needed to send the *localized* form of an activation notice, rather
/// than the bare one.
///
/// The bare notice is just the `ApplicationActivated` event — which is all
/// cua-rs used to send. The localized form is that event *plus a mouse down/up
/// pair* aimed at the window's own activation point and pinned to the window
/// with `CGEventSetWindowLocation(point - frame.origin)`. That pair is the
/// canonical "click a window to make it key" gesture, and skipping it is why an
/// activation notice alone left some controls — a chat app's header menu
/// button, measured — still refusing the click that followed. Telling the
/// *application* it is active and giving one of its *windows* key status are
/// two different statements, and only the second one is a click.
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
/// Two seconds is the ceiling because past that the click is not worth waiting
/// for; 16 ms is one display frame, which is the finest granularity the answer
/// can actually change at.
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
/// attempting, so this reports the outcome and never fails.
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

/// The second half of the localized activation notice: a click on the window
/// itself, at the point the window nominates.
///
/// This is what actually makes AppKit treat the window as key. The
/// `ApplicationActivated` event tells the *application* it is active; a window
/// only becomes key when something clicks it, which is why this pair is emitted
/// alongside the notice whenever the window's bounds and activation point are
/// known.
///
/// It is a real click, so it is only as safe as the point it is aimed at. The
/// caller owns that check — see [`ActivationAssist`].
///
/// Fresh event numbers are used here rather than fixed literals, because what
/// AppKit needs from the field is uniqueness
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
    // One event number for the pair, as in the main click path: AppKit pairs an
    // up with its own down by this field, so allocating per half hands a view a
    // mouse-up that belongs to no down it saw.
    let event_number = nsevent::next_event_number();

    let route = MouseRoute {
        pid,
        wid,
        button: MouseButton::Left,
        // No modifiers, ever: this is a click the caller did not ask for, sent
        // only to make the window key. Carrying the caller's ⌘ or ⇧ into it
        // could turn a focus assist into a range-select or an open-in-new-tab
        // on whatever the activation point covers.
        modifiers: CGEventFlags::empty(),
        click_group_id,
    };
    for (kind, pressure) in [
        (NSEventType::LeftMouseDown, 1.0_f32),
        (NSEventType::LeftMouseUp, 0.0_f32),
    ] {
        route.post(MouseEventSpec {
            kind,
            point,
            window_local,
            event_number,
            click_count: 1,
            pressure,
        })?;
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    Ok(())
}

/// Press at one point, move through interpolated intermediate points, release
/// at another — all inside one window, without the real pointer moving.
///
/// # Why the moves are interpolated
///
/// A down at A followed immediately by an up at B is not a drag to anything
/// that implements one. AppKit's own drag sources arm on a `mouseDragged` that
/// exceeds a small threshold and then track each subsequent move; a single jump
/// gives them one move that they may or may not see before the up arrives, and
/// a web view or an Electron list — which reconstructs the gesture from
/// `mousemove` events — sees no movement at all. So the gesture is a run of
/// `mouseDragged` events along the straight line between the endpoints, sent
/// one per display frame. See [`drag_step_count`] for the count and
/// [`DRAG_STEP_INTERVAL_MS`] for the interval, both of which are chosen rather
/// than tuned.
///
/// # The mouse-up is not optional
///
/// A drag that fails halfway leaves the target mid-gesture: a row lifted out of
/// a list, a selection rectangle still growing, a window stuck to a pointer
/// that will never move again. So the release is attempted even when a move
/// fails, and the *first* error is what gets returned — the failure that
/// explains what happened, not the cleanup's opinion of it.
pub fn drag_background_pid(
    drag: PidDrag,
    assist: Option<ActivationAssist>,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) -> Result<()> {
    let PidDrag {
        pid,
        wid,
        window_origin,
        origin,
        destination,
        button,
        modifiers,
    } = drag;
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable(
            "SLEventPostToPid + CGEventSetWindowLocation + SLEventSetIntegerValueField",
        ));
    }
    let window_number = wid as isize;
    let click_group_id = fresh_click_group_id();
    let local = |(x, y): (f64, f64)| (x - window_origin.0, y - window_origin.1);

    post_activation_notice(pid, nsevent::notify_window_key_focus_returned())?;
    post_activation_notice(pid, nsevent::notify_app_activated(window_number))?;
    if let Some(assist) = assist {
        post_window_focus_click(pid, window_number, assist, click_group_id)?;
    }
    wait_until_believed_frontmost(believes_it_is_frontmost);

    let route = MouseRoute {
        pid,
        wid,
        button,
        modifiers,
        click_group_id,
    };
    let start = CGPoint::new(origin.0, origin.1);
    post_mouse_moved_primer(&route, start, local(origin))?;
    std::thread::sleep(std::time::Duration::from_millis(12));

    let (down_type, dragged_type, up_type) = button.types();
    // One event number for the whole gesture. AppKit correlates every event of
    // a tracking session — the down, each drag, and the up — by this field, so
    // allocating a fresh one per move would hand a view a stream of unrelated
    // single events instead of one drag.
    let event_number = nsevent::next_event_number();

    route.post(MouseEventSpec {
        kind: down_type,
        point: start,
        window_local: local(origin),
        event_number,
        click_count: 1,
        pressure: 1.0,
    })?;
    // Long enough for the target to have processed the press and armed whatever
    // tracking loop the drags are for. This is the same 28 ms a click leaves
    // between its own down and up.
    std::thread::sleep(std::time::Duration::from_millis(28));

    let moves = (|| -> Result<()> {
        for point in drag_path(origin, destination) {
            route.post(MouseEventSpec {
                kind: dragged_type,
                point: CGPoint::new(point.0, point.1),
                window_local: local(point),
                event_number,
                // A drag carries no click count. `-[NSEvent clickCount]` on a
                // real `mouseDragged` is the count of the press it belongs to,
                // but views read it on the down; stamping it here has been
                // observed to matter nowhere and 1 is the honest value.
                click_count: 1,
                pressure: 1.0,
            })?;
            std::thread::sleep(std::time::Duration::from_millis(DRAG_STEP_INTERVAL_MS));
        }
        Ok(())
    })();

    let release = route.post(MouseEventSpec {
        kind: up_type,
        point: CGPoint::new(destination.0, destination.1),
        window_local: local(destination),
        event_number,
        click_count: 1,
        pressure: 0.0,
    });

    if deactivate_after_click() {
        let _ = post_activation_notice(pid, nsevent::notify_app_deactivated(window_number));
    }

    moves.and(release)
}

/// Tell one process's window that the pointer moved to a point, without moving
/// the real pointer.
///
/// This is the whole of "hover": a `mouseMoved` event carrying the target
/// point, delivered by pid. A view with an `NSTrackingArea`, a web page with a
/// `:hover` rule, or a toolbar that reveals a button under the cursor all react
/// to the event, so the revealed UI shows up in the next snapshot.
///
/// **What it cannot reach:** anything that asks where the pointer *is* rather
/// than reading where the event says it went — `NSEvent.mouseLocation`,
/// `-[NSWindow mouseLocationOutsideOfEventStream]`, a poll of the `NSCursor`
/// position. Those answer with the real pointer, which is still wherever the
/// human left it, and cua-rs will not move it. An app built that way will not
/// respond, and there is no version of this call that changes that.
pub fn move_mouse_background_pid(
    target: PidMouseMove,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) -> Result<()> {
    let PidMouseMove {
        pid,
        point: (x, y),
        window_local,
        wid,
        modifiers,
    } = target;
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable(
            "SLEventPostToPid + CGEventSetWindowLocation + SLEventSetIntegerValueField",
        ));
    }
    // The notices, but never the assist click: an assist is a real click on the
    // window's activation point, and a caller asking to hover has not asked to
    // press anything.
    prime_pointer_target(pid, wid as isize, believes_it_is_frontmost);

    let route = MouseRoute {
        pid,
        wid,
        button: MouseButton::Left,
        modifiers,
        click_group_id: fresh_click_group_id(),
    };
    post_mouse_moved_primer(&route, CGPoint::new(x, y), window_local)
}

/// Deliver a scroll-wheel event with a delta to one process's window.
///
/// Unlike every other event in this module, this one is *not* built through an
/// `NSEvent` factory, because there is no scroll-wheel factory to build it
/// with: the AppKit constructors cover mouse, keyboard and the "other" family
/// only. That costs nothing here. The AppKit header the mouse path exists to
/// obtain — event number, click count, window number — is what a view uses to
/// validate a *click*; `-[NSEvent scrollingDeltaY]` reads the CG record's own
/// wheel fields, which `CGEventCreateScrollWheelEvent2` fills in correctly.
/// The window routing, the pid stamp and the fresh timestamp are all applied
/// exactly as they are to a mouse event.
pub fn scroll_background_pid(
    scroll: PidScroll,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) -> Result<()> {
    let PidScroll {
        pid,
        point: (x, y),
        window_local,
        wid,
        delta_y,
        delta_x,
        unit,
        modifiers,
    } = scroll;
    if !skylight::is_available() {
        return Err(HidError::PrimitiveUnavailable(
            "SLEventPostToPid + CGEventSetWindowLocation + SLEventSetIntegerValueField",
        ));
    }
    prime_pointer_target(pid, wid as isize, believes_it_is_frontmost);

    // A scroll is only believable where the pointer is, so the same move that
    // precedes a click precedes this: a view that decides which of its subviews
    // owns the wheel by hit-testing the last known pointer position needs to
    // have been told the pointer is over the target first.
    let route = MouseRoute {
        pid,
        wid,
        button: MouseButton::Left,
        modifiers,
        click_group_id: fresh_click_group_id(),
    };
    let point = CGPoint::new(x, y);
    post_mouse_moved_primer(&route, point, window_local)?;
    std::thread::sleep(std::time::Duration::from_millis(12));

    let source =
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(HidError::NoSource)?;
    // Two wheels: vertical first, horizontal second, which is the order
    // `CGEventCreateScrollWheelEvent2` documents. Asking for one wheel and then
    // stamping the horizontal axis afterwards leaves the event describing
    // itself as one-dimensional.
    let event = CGEvent::new_scroll_wheel_event2(Some(&source), unit.cg(), 2, delta_y, delta_x, 0)
        .ok_or(HidError::NoSource)?;
    CGEvent::set_flags(Some(&event), modifiers);
    CGEvent::set_location(Some(&event), point);

    let ptr = CFRetained::as_ptr(&event).as_ptr() as *mut c_void;
    route.stamp(ptr, window_local)?;
    CGEvent::set_timestamp(Some(&event), nsevent::uptime_nanos());
    post_once(pid, &event, ptr)
}

/// How many `mouseDragged` events to interpolate between a drag's endpoints.
///
/// Distance-aware rather than fixed, because the two failure modes pull in
/// opposite directions and a single constant cannot avoid both:
///
/// - **too few** and each step is a jump. A drop target the drag passes over
///   never sees a move inside itself, so it never highlights, and a source that
///   arms on exceeding a threshold may be handed its whole distance in one
///   event.
/// - **too many** and the gesture takes long enough that a list's drag-autoscroll
///   starts, or the target coalesces the moves it cannot draw anyway.
///
/// So the step *length* is what is held constant, at
/// [`DRAG_MAX_STEP_POINTS`] — about the height of a list row or a toolbar
/// button, so no ordinarily-sized drop target is stepped over entirely. The
/// floor of [`DRAG_MIN_STEPS`] keeps a short drag from degenerating into the
/// single jump this exists to avoid; the ceiling of [`DRAG_MAX_STEPS`] bounds
/// the whole gesture at about half a second.
pub fn drag_step_count(origin: (f64, f64), destination: (f64, f64)) -> usize {
    let distance = ((destination.0 - origin.0).powi(2) + (destination.1 - origin.1).powi(2)).sqrt();
    let wanted = (distance / DRAG_MAX_STEP_POINTS).ceil();
    // `as usize` after the clamp, so a NaN distance (two non-finite endpoints)
    // lands on the floor rather than saturating or wrapping.
    if wanted.is_nan() {
        return DRAG_MIN_STEPS;
    }
    (wanted as usize).clamp(DRAG_MIN_STEPS, DRAG_MAX_STEPS)
}

/// The points a drag's `mouseDragged` events are sent at, in order.
///
/// The origin is **not** repeated here — the mouse-down already delivered it —
/// and the destination is always the last element, exactly, rather than
/// whatever the interpolation arithmetic rounds to. A drag that ends a fraction
/// of a point away from where the caller aimed is a drop into the wrong row.
pub fn drag_path(origin: (f64, f64), destination: (f64, f64)) -> Vec<(f64, f64)> {
    let steps = drag_step_count(origin, destination);
    (1..=steps)
        .map(|i| {
            if i == steps {
                destination
            } else {
                let t = i as f64 / steps as f64;
                (
                    origin.0 + (destination.0 - origin.0) * t,
                    origin.1 + (destination.1 - origin.1) * t,
                )
            }
        })
        .collect()
}

/// Longest straight-line distance one interpolated drag step may cover, in
/// points. Roughly one list row.
const DRAG_MAX_STEP_POINTS: f64 = 24.0;
/// Fewest interpolated steps, however short the drag.
const DRAG_MIN_STEPS: usize = 6;
/// Most interpolated steps, however long the drag. At
/// [`DRAG_STEP_INTERVAL_MS`] each, this caps the moving part of a gesture at
/// about half a second.
const DRAG_MAX_STEPS: usize = 32;

/// Gap between two interpolated drag steps, in milliseconds.
///
/// One display frame at 60 Hz. A target redraws at most once per frame, so
/// moves sent faster than this are moves it cannot act on separately and may
/// coalesce; moves sent slower make an ordinary drag take visibly longer than a
/// human one, which is what starts autoscroll timers.
pub const DRAG_STEP_INTERVAL_MS: u64 = 16;

/// A fresh sub-second group id, tying the events of one gesture together in the
/// window server's coalescing.
fn fresh_click_group_id() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0)
}

/// Send the activation notices — and only the notices — before a pointer event
/// that is not a press.
///
/// [`click_background_pid`] can additionally synthesize a real click on the
/// window's activation point ([`ActivationAssist`]) to make the window key.
/// That is appropriate when the caller asked for a click and inappropriate when
/// they asked for a hover or a scroll, so this half is what those get.
fn prime_pointer_target(
    pid: i32,
    window_number: isize,
    believes_it_is_frontmost: &dyn Fn() -> bool,
) {
    let _ = post_activation_notice(pid, nsevent::notify_window_key_focus_returned());
    let _ = post_activation_notice(pid, nsevent::notify_app_activated(window_number));
    wait_until_believed_frontmost(believes_it_is_frontmost);
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
    route: &MouseRoute,
    point: CGPoint,
    window_local: (f64, f64),
) -> Result<()> {
    route.post(MouseEventSpec {
        kind: NSEventType::MouseMoved,
        point,
        window_local,
        event_number: nsevent::next_event_number(),
        click_count: 0,
        pressure: 0.0,
    })
}

/// Everything about *where a gesture is going* that does not change between its
/// events: the process, the window, the button and the modifier keys.
///
/// Split out from [`MouseEventSpec`] because a click, a drag and a hover differ
/// only in the per-event half. Keeping the constant half in one value means the
/// button number and the pid stamp cannot disagree between the down and the up
/// of the same gesture.
#[derive(Debug, Clone, Copy)]
struct MouseRoute {
    pid: i32,
    wid: u32,
    button: MouseButton,
    modifiers: CGEventFlags,
    click_group_id: i64,
}

/// The per-event half: what distinguishes this one event from the others in its
/// gesture.
#[derive(Debug, Clone, Copy)]
struct MouseEventSpec {
    kind: NSEventType,
    point: CGPoint,
    window_local: (f64, f64),
    event_number: isize,
    click_count: isize,
    pressure: f32,
}

impl MouseRoute {
    /// Build one AppKit mouse event, stamp it, and post it to the target pid.
    fn post(&self, spec: MouseEventSpec) -> Result<()> {
        let event = nsevent::mouse_event(
            spec.kind,
            spec.point,
            nsevent::appkit_modifiers(self.modifiers),
            self.wid as isize,
            spec.event_number,
            spec.click_count,
            spec.pressure,
        )
        .ok_or(HidError::NoSource)?;
        let ptr = nsevent::as_raw(&event);

        // AppKit built this event from a flipped, window-relative location. The
        // window server routes on the CG-space one, so overwrite it with the
        // screen point the caller actually validated.
        CGEvent::set_location(Some(&event), spec.point);
        // Also re-stamped rather than trusted from the `NSEvent` header,
        // because a receiver reading `CGEventGetFlags` — anything Chromium
        // based — reads the CG record, not the AppKit one.
        CGEvent::set_flags(Some(&event), self.modifiers);

        self.stamp(ptr, spec.window_local)?;

        // A stale timestamp is treated as a replayed event and can be coalesced
        // away or dropped outright, so it is read as late as possible —
        // immediately before the post, after all stamping is done.
        CGEvent::set_timestamp(Some(&event), nsevent::uptime_nanos());

        // Post exactly once. Duplicating this through CGEventPostToPid can turn
        // one logical click into two queue entries on macOS versions where both
        // routes are accepted, and the public post carries no success signal
        // anyway.
        post_once(self.pid, &event, ptr)
    }

    /// Stamp the private window-routing fields (layers 2 and 3) onto a prepared
    /// event record.
    ///
    /// Applies to any pointer event, whether or not AppKit built it — the
    /// scroll path shares this exactly, which is why it takes a raw pointer
    /// rather than a typed event.
    ///
    /// Deliberately *not* stamped: `kCGMouseEventClickState` and the window
    /// number, which `-[NSEvent clickCount]` and `-[NSEvent windowNumber]`
    /// already carry from construction. Writing them again was at best
    /// redundant and at worst contradicted the header AppKit validates against.
    fn stamp(&self, ptr: *mut c_void, window_local: (f64, f64)) -> Result<()> {
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
        set(skylight::BUTTON_NUMBER, self.button.number())?;
        set(skylight::SUBTYPE, 3)?;

        let window_id = self.wid as i64;
        set(skylight::CLICK_GROUP, self.click_group_id)?;
        set(skylight::WINDOW_UNDER_MOUSE, window_id)?;
        set(skylight::WINDOW_UNDER_MOUSE_HANDLING, window_id)?;

        // Target pid is always stamped — it is the whole point of the route.
        set(skylight::TARGET_PID, self.pid as i64)
    }
}

/// Whether to deliver through the public `CGEventPostToPid` instead of the
/// private SkyLight route, read once from `CUA_PUBLIC_POST`.
///
/// The public call is the same API §1 of DESIGN.md records as non-functional, yet
/// it does work once an event is built properly. Both observations can hold: the
/// measurement that condemned the public route sent an
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

/// Whether to send the balancing `ApplicationDeactivated` after a click.
///
/// Off by default, because sending it was measured to do harm. On KakaoTalk the
/// chat window's own menu-bar item ("채팅") vanished the instant the notice
/// landed: the target treated it as the window losing key status, mid-gesture,
/// immediately after being told the opposite. Suppressing it kept the menu bar
/// intact across the click.
///
/// Leaving the target believing it is active is the lesser problem: deactivation
/// belongs to a session-shaped lifecycle step rather than to every click. The
/// belief is corrected by real AppKit events as soon as the user touches
/// anything, and the *real* frontmost app was never changed in the first place.
///
/// `CUA_DEACTIVATE_AFTER_CLICK=1` restores the old behaviour for bisecting.
fn deactivate_after_click() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("CUA_DEACTIVATE_AFTER_CLICK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Deliver one prepared event to `pid` through whichever route is selected.
///
/// The public call returns nothing at all — not even "the process is gone" — so a
/// `true` here means "handed over", never "delivered". The private route is not
/// much better, but it at least reports a missing symbol.
fn post_once(pid: i32, event: &CGEvent, ptr: *mut c_void) -> Result<()> {
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
    fn a_modifier_list_shares_the_chord_vocabulary() {
        // The point of routing both parsers through one table: whatever spells
        // a modifier in `press_key` must spell it on a click too.
        for alias in ["cmd", "command", "meta", "super"] {
            assert_eq!(
                parse_modifiers(alias).unwrap(),
                CGEventFlags::MaskCommand,
                "{alias} must mean command on a click as well as in a chord"
            );
        }
        assert_eq!(
            parse_modifiers("cmd+shift").unwrap(),
            parse_chord("cmd+shift+a").unwrap().flags,
            "a modifier list and the modifier half of a chord must agree"
        );
        // Same separator rules, so a model that writes dashes in one place and
        // pluses in the other gets the same answer.
        assert_eq!(
            parse_modifiers("alt-ctrl").unwrap(),
            parse_modifiers("ctrl+alt").unwrap()
        );
    }

    #[test]
    fn an_empty_modifier_list_is_no_modifiers_not_an_error() {
        // A caller forwarding an optional field should not have to special-case
        // the absent case.
        assert!(parse_modifiers("").unwrap().is_empty());
        assert!(parse_modifiers("   ").unwrap().is_empty());
    }

    #[test]
    fn a_key_name_in_a_modifier_list_is_refused_not_ignored() {
        // `cmd+click` is a thing a model will write. Dropping the `click`
        // silently would deliver a plain command-click that looks correct.
        let err = parse_modifiers("cmd+click").unwrap_err();
        match err {
            HidError::UnknownModifier { token, .. } => assert_eq!(token, "click"),
            other => panic!("expected UnknownModifier, got {other:?}"),
        }
        // Even a token that IS a valid key: a modifier list has no key in it.
        assert!(matches!(
            parse_modifiers("shift+p").unwrap_err(),
            HidError::UnknownModifier { .. }
        ));
    }

    #[test]
    fn buttons_parse_by_name_and_default_to_left() {
        assert_eq!(MouseButton::parse("").unwrap(), MouseButton::Left);
        assert_eq!(MouseButton::parse("  Right ").unwrap(), MouseButton::Right);
        assert_eq!(MouseButton::parse("MIDDLE").unwrap(), MouseButton::Middle);
        assert!(matches!(
            MouseButton::parse("mouse2").unwrap_err(),
            HidError::UnknownButton(_)
        ));
    }

    #[test]
    fn each_button_gets_its_own_event_type_family() {
        // A view implementing `rightMouseDown:` never sees a `leftMouseDown`,
        // whatever button number is stamped on it, so the type is what selects
        // the handler and the three families must not be mixed up.
        let (down, dragged, up) = MouseButton::Right.types();
        assert_eq!(down, NSEventType::RightMouseDown);
        assert_eq!(dragged, NSEventType::RightMouseDragged);
        assert_eq!(up, NSEventType::RightMouseUp);

        let (down, dragged, up) = MouseButton::Middle.types();
        assert_eq!(down, NSEventType::OtherMouseDown);
        assert_eq!(dragged, NSEventType::OtherMouseDragged);
        assert_eq!(up, NSEventType::OtherMouseUp);

        assert_eq!(MouseButton::Left.number(), 0);
        assert_eq!(MouseButton::Right.number(), 1);
        assert_eq!(MouseButton::Middle.number(), 2);
    }

    #[test]
    fn a_drag_path_ends_exactly_where_it_was_aimed() {
        // Not "within a rounding error": a drop a fraction of a point short is
        // a drop into the neighbouring row.
        let path = drag_path((100.0, 100.0), (300.0, 250.0));
        assert_eq!(*path.last().unwrap(), (300.0, 250.0));
        assert!(
            !path.contains(&(100.0, 100.0)),
            "the origin belongs to the mouse-down, not to the move run"
        );
    }

    #[test]
    fn a_drag_path_is_monotone_along_both_axes() {
        let path = drag_path((0.0, 0.0), (100.0, -50.0));
        for pair in path.windows(2) {
            assert!(pair[1].0 > pair[0].0, "x must advance: {path:?}");
            assert!(pair[1].1 < pair[0].1, "y must advance: {path:?}");
        }
    }

    #[test]
    fn step_count_holds_the_step_length_constant_between_its_bounds() {
        // 24 points per step in the middle of the range...
        assert_eq!(drag_step_count((0.0, 0.0), (0.0, 240.0)), 10);
        // ...the floor for anything short, so a 5-point drag is still a run of
        // moves and not one jump...
        assert_eq!(drag_step_count((0.0, 0.0), (5.0, 0.0)), DRAG_MIN_STEPS);
        assert_eq!(drag_step_count((7.0, 7.0), (7.0, 7.0)), DRAG_MIN_STEPS);
        // ...and the ceiling for anything long, so one gesture cannot run for
        // seconds.
        assert_eq!(drag_step_count((0.0, 0.0), (4000.0, 0.0)), DRAG_MAX_STEPS);
    }

    #[test]
    fn a_zero_length_drag_still_produces_a_usable_path() {
        // Origin == destination is a caller error the tiers above catch, but it
        // must not produce an empty path here: an empty path would mean a
        // mouse-down with no moves, which is the shape a stuck drag has.
        let path = drag_path((10.0, 10.0), (10.0, 10.0));
        assert_eq!(path.len(), DRAG_MIN_STEPS);
        assert!(path.iter().all(|p| *p == (10.0, 10.0)));
    }

    #[test]
    fn a_non_finite_endpoint_does_not_blow_up_the_step_count() {
        // `f64 as usize` saturates rather than wrapping, but NaN converts to 0,
        // and 0 steps is a drag with no moves in it.
        assert_eq!(
            drag_step_count((0.0, 0.0), (f64::NAN, f64::NAN)),
            DRAG_MIN_STEPS
        );
        assert_eq!(
            drag_step_count((0.0, 0.0), (f64::INFINITY, 0.0)),
            DRAG_MAX_STEPS
        );
    }

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

    // ── the literal character, and when it is not one ────────────────────────

    #[test]
    fn a_bare_character_key_remembers_its_character() {
        // The whole point: under a Korean source the keycode alone arrives as a
        // different letter, so the character has to travel with the event.
        assert_eq!(parse_chord("x").unwrap().literal, Some('x'));
        assert_eq!(parse_chord("X").unwrap().literal, Some('x'));
        assert_eq!(parse_chord("7").unwrap().literal, Some('7'));
        // A bare `-` is a separator, not a key — pre-existing, and why the
        // spelled name exists. Its keycode form still carries no literal,
        // because `minus` names a key rather than a character.
        assert!(parse_chord("-").is_err());
        assert_eq!(parse_chord("cmd+-").unwrap().literal, None);
    }

    #[test]
    fn a_named_key_has_no_character_to_force() {
        // `escape` and `f5` produce no character at all; claiming one would put
        // a literal "e" on the event.
        assert_eq!(parse_chord("escape").unwrap().literal, None);
        assert_eq!(parse_chord("return").unwrap().literal, None);
        assert_eq!(parse_chord("f5").unwrap().literal, None);
        assert_eq!(parse_chord("tab").unwrap().literal, None);
        assert_eq!(parse_chord("minus").unwrap().literal, None);
    }

    #[test]
    fn a_modifier_drops_the_character() {
        // `cmd+x` is Cut, not the letter x. Forcing a character onto a chord
        // would change what the keystroke means.
        assert_eq!(parse_chord("cmd+x").unwrap().literal, None);
        assert_eq!(parse_chord("shift+a").unwrap().literal, None);
        assert_eq!(parse_chord("ctrl+alt+delete").unwrap().literal, None);
        // ...but the keycode is still the one the letter names.
        assert_eq!(
            parse_chord("cmd+x").unwrap().key,
            parse_chord("x").unwrap().key
        );
    }
}
