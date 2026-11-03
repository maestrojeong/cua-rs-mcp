//! Plain-Rust stand-ins for the leaf types the macOS backend borrows from
//! CoreGraphics, `cua-ax`, `cua-hid` and `cua-capture`.
//!
//! On macOS none of this is used: [`super::leaf`] re-exports the real types,
//! so every public signature in this crate is bit-for-bit what it was before
//! the platform split. Everywhere else these definitions take their place, with
//! the same names, the same fields, the same constructors and the same parse
//! vocabulary, so the platform-neutral half of the crate — and every consumer
//! of it — compiles unchanged against either.
//!
//! Compiled on macOS too under `cfg(test)`, so `tests` below can hold each
//! stand-in against the real type it replaces. Drift between the two would
//! otherwise surface only as "the same argument means something different on
//! Linux", which nothing else here would catch.

use std::fmt;

// ── geometry ─────────────────────────────────────────────────────────────────

/// A point in global screen points, top-left origin. Same fields as
/// CoreGraphics' `CGPoint`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

impl CGPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A size in points. Same fields as CoreGraphics' `CGSize`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

impl CGSize {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// A rectangle in global points. Same fields as CoreGraphics' `CGRect`.
///
/// Named after the CoreGraphics type rather than something neutral because
/// that is the name every public result in this crate already uses for a
/// frame; a second name for the same four numbers would be churn with no
/// information in it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

impl CGRect {
    pub const fn new(origin: CGPoint, size: CGSize) -> Self {
        Self { origin, size }
    }
}

// ── input vocabulary ─────────────────────────────────────────────────────────

/// A modifier-key set, with the same associated constants as the
/// `CGEventFlags` it stands in for so `Modifiers::MaskCommand` reads the same
/// on every platform. The bit values are CoreGraphics' own; nothing outside a
/// backend interprets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modifiers(pub u64);

#[allow(non_upper_case_globals)]
impl Modifiers {
    pub const MaskShift: Self = Self(0x0002_0000);
    pub const MaskControl: Self = Self(0x0004_0000);
    pub const MaskAlternate: Self = Self(0x0008_0000);
    pub const MaskCommand: Self = Self(0x0010_0000);
    pub const MaskSecondaryFn: Self = Self(0x0080_0000);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Which mouse button a gesture carries. Same variants and spellings as
/// `cua_hid::MouseButton`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// Same vocabulary as `cua_hid::MouseButton::parse`: blank is left.
    pub fn parse(button: &str) -> Result<Self, InputParseError> {
        match button.trim().to_lowercase().as_str() {
            "" | "left" | "primary" => Ok(MouseButton::Left),
            "right" | "secondary" | "context" => Ok(MouseButton::Right),
            "middle" | "center" | "centre" | "wheel" => Ok(MouseButton::Middle),
            _ => Err(InputParseError::UnknownButton(button.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
}

/// A modifier or button name that did not parse. Worded exactly as
/// `cua_hid::HidError`'s matching variants, because the message is what a
/// caller sees and it must not depend on which backend produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputParseError {
    UnknownModifier { modifiers: String, token: String },
    UnknownButton(String),
}

impl fmt::Display for InputParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModifier { modifiers, token } => write!(
                f,
                "unknown modifier `{token}` in {modifiers:?}. Modifiers: cmd, shift, alt/option, ctrl, fn"
            ),
            Self::UnknownButton(b) => {
                write!(f, "unknown mouse button {b:?}. Buttons: left, right, middle")
            }
        }
    }
}

impl std::error::Error for InputParseError {}

/// The modifier a token names. Same table as `cua_hid`'s `modifier_flag`.
pub fn modifier_flag(token: &str) -> Option<Modifiers> {
    match token {
        "cmd" | "command" | "meta" | "super" => Some(Modifiers::MaskCommand),
        "shift" => Some(Modifiers::MaskShift),
        "alt" | "opt" | "option" => Some(Modifiers::MaskAlternate),
        "ctrl" | "control" => Some(Modifiers::MaskControl),
        "fn" | "function" => Some(Modifiers::MaskSecondaryFn),
        _ => None,
    }
}

/// Split a chord-shaped string into tokens: `-` separates only when there is
/// no `+`, because `-` is also a key name. Same rule as `cua_hid`.
pub fn chord_tokens(s: &str) -> impl Iterator<Item = &str> {
    let separators: &[char] = if s.contains('+') { &['+'] } else { &['+', '-'] };
    s.split(separators).map(str::trim).filter(|t| !t.is_empty())
}

/// Same contract as `cua_hid::parse_modifiers`.
pub fn parse_modifiers(modifiers: &str) -> Result<Modifiers, InputParseError> {
    let mut flags = Modifiers::empty();
    for raw in chord_tokens(modifiers) {
        match modifier_flag(&raw.to_lowercase()) {
            Some(flag) => flags |= flag,
            None => {
                return Err(InputParseError::UnknownModifier {
                    modifiers: modifiers.to_string(),
                    token: raw.to_string(),
                })
            }
        }
    }
    Ok(flags)
}

// ── tree walk caps ───────────────────────────────────────────────────────────

/// Caps for one tree walk. Same fields and defaults as `cua_ax::Limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_depth: u32,
    pub max_children: usize,
    pub skip_offscreen: bool,
    pub budget: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 1500,
            max_depth: 40,
            max_children: 200,
            skip_offscreen: true,
            budget: std::time::Duration::from_secs(10),
        }
    }
}

// ── displays ─────────────────────────────────────────────────────────────────

/// One active display. Same fields as `cua_capture::DisplayInfo`; `id` is
/// whatever the backend's own display identifier is (a RandR output on X11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub frame: CGRect,
    pub main: bool,
    pub scale: f64,
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    //! Each stand-in against the real type it replaces on macOS.

    #[test]
    fn limits_defaults_match_cua_ax() {
        let real = cua_ax::Limits::default();
        let ours = super::Limits::default();
        assert_eq!(real.max_nodes, ours.max_nodes);
        assert_eq!(real.max_depth, ours.max_depth);
        assert_eq!(real.max_children, ours.max_children);
        assert_eq!(real.skip_offscreen, ours.skip_offscreen);
        assert_eq!(real.budget, ours.budget);
    }

    #[test]
    fn modifier_bits_match_cgeventflags() {
        use cua_hid::Modifiers as Real;
        for (real, ours) in [
            (Real::MaskShift, super::Modifiers::MaskShift),
            (Real::MaskControl, super::Modifiers::MaskControl),
            (Real::MaskAlternate, super::Modifiers::MaskAlternate),
            (Real::MaskCommand, super::Modifiers::MaskCommand),
            (Real::MaskSecondaryFn, super::Modifiers::MaskSecondaryFn),
        ] {
            assert_eq!(real.0, ours.0);
        }
    }

    #[test]
    fn modifier_and_button_parsing_matches_cua_hid() {
        for input in [
            "",
            " ",
            "cmd",
            "Cmd+Shift",
            "alt-shift",
            "option",
            "ctrl+fn",
            "meta",
            "super",
            "command+control",
            "cmd+click",
            "shft",
            "cmd++shift",
        ] {
            let real = cua_hid::parse_modifiers(input)
                .map(|m| m.0)
                .map_err(|e| e.to_string());
            let ours = super::parse_modifiers(input)
                .map(|m| m.0)
                .map_err(|e| e.to_string());
            assert_eq!(real, ours, "modifiers {input:?}");
        }
        for input in ["", "left", "RIGHT", "context", "wheel", "centre", "fourth"] {
            let real = cua_hid::MouseButton::parse(input)
                .map(|b| b.as_str())
                .map_err(|e| e.to_string());
            let ours = super::MouseButton::parse(input)
                .map(|b| b.as_str())
                .map_err(|e| e.to_string());
            assert_eq!(real, ours, "button {input:?}");
        }
    }
}
