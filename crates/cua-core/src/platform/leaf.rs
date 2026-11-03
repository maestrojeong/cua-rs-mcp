//! The leaf types every public signature in this crate is written in terms of.
//!
//! On macOS these are the real CoreGraphics / `cua-ax` / `cua-hid` /
//! `cua-capture` types, re-exported unchanged — the macOS backend and its
//! callers see exactly what they saw before the platform split. Elsewhere they
//! are the plain-Rust stand-ins in [`super::portable`], which keep the same
//! names, fields and parse vocabulary.

#[cfg(target_os = "macos")]
pub use cua_ax::Limits;
#[cfg(target_os = "macos")]
pub use cua_capture::DisplayInfo;
#[cfg(target_os = "macos")]
pub use cua_hid::{parse_modifiers, Modifiers, MouseButton};
#[cfg(target_os = "macos")]
pub use objc2_core_foundation::{CGPoint, CGRect, CGSize};

#[cfg(not(target_os = "macos"))]
pub use super::portable::{
    parse_modifiers, CGPoint, CGRect, CGSize, DisplayInfo, Limits, Modifiers, MouseButton,
};
