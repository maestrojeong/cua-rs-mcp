//! Which [`PlatformAdapter`] this binary drives, chosen at compile time.
//!
//! The server is written against the trait, never against a backend's own
//! methods, so the tool surface — names, schemas, rendering — is one piece of
//! code for every platform. The only per-platform line in the whole MCP layer
//! is the `Platform` alias below.

use cua_core::PlatformAdapter;

/// The macOS backend: `cua-core`'s native worker over Accessibility,
/// SkyLight and ScreenCaptureKit.
#[cfg(target_os = "macos")]
pub type Platform = cua_core::Cua;

/// The Linux backend: AT-SPI2 over D-Bus for the tree and semantic actions,
/// X11 for pixels and (opt-in) synthesized input. See `cua-ax-linux`.
#[cfg(target_os = "linux")]
pub type Platform = cua_ax_linux::LinuxCua;

/// One prepared operation for this build's backend.
pub type Op<T> = <Platform as PlatformAdapter>::Op<T>;

/// `op.map_outcome(f)` for [`PlatformAdapter::map`], so the server's
/// `prep_*` functions read as a chain. Deliberately not named `map`: on macOS
/// the prepared type has an inherent `map` of its own, and an extension method
/// of the same name would silently lose to it — this one always goes through
/// the trait.
pub trait MapOutcome<T: Send + 'static> {
    fn map_outcome<U, F>(self, f: F) -> Op<U>
    where
        U: Send + 'static,
        F: FnOnce(cua_core::session::Result<T>) -> U + Send + 'static;
}

impl<T: Send + 'static> MapOutcome<T> for Op<T> {
    fn map_outcome<U, F>(self, f: F) -> Op<U>
    where
        U: Send + 'static,
        F: FnOnce(cua_core::session::Result<T>) -> U + Send + 'static,
    {
        Platform::map(self, f)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("cua-rs has no PlatformAdapter backend for this target");
