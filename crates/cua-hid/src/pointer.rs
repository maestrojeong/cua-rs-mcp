use super::*;

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
    pub(super) fn number(self) -> i64 {
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
    pub(super) fn types(self) -> (NSEventType, NSEventType, NSEventType) {
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
