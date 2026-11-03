//! The Linux backend for cua-rs: [`cua_core::PlatformAdapter`] over AT-SPI2.
//!
//! - **Tree and semantic actions**: AT-SPI2 over D-Bus (the `atspi` crate on
//!   zbus). Works under X11 and Wayland alike, for any app whose toolkit has
//!   accessibility enabled (GTK and Qt do; Electron needs
//!   `--force-renderer-accessibility`).
//! - **Pixels, displays, window-manager requests, the clipboard, the
//!   yield-to-human idle reading**: X11 (`x11rb`). Unavailable on a Wayland
//!   session without an X display, and reported as `PLATFORM_NOT_SUPPORTED`
//!   rather than faked.
//! - **Synthesized pointer and keyboard input**: XTEST, opt-in only
//!   (`CUA_LINUX_XTEST=1`), because on Linux it is input on the *shared*
//!   devices — see `actions` for why that is off by default.
//! - **Notifications, `close_app`**: not supported, with the reason in the
//!   error.
//!
//! README's OS support table and DESIGN.md §15 are the user-facing versions of
//! this list. On any other target this crate is empty.

#[cfg(target_os = "linux")]
mod actions;
#[cfg(target_os = "linux")]
mod adapter;
#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod clipboard;
mod keys;
#[cfg(target_os = "linux")]
mod tree;
#[cfg(target_os = "linux")]
mod worker;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
pub use worker::{LinuxCua, Op};
