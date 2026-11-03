//! Native session orchestration and public session types.
//!
//! `types` is the platform-neutral half — every type a [`crate::PlatformAdapter`]
//! takes or returns — and compiles everywhere. `orchestration` and its children
//! are the macOS backend: the native worker thread, the snapshot store and
//! every action, written against `cua-ax`, `cua-hid` and `cua-capture`.

mod types;
pub use types::*;

#[cfg(target_os = "macos")]
mod orchestration;

#[cfg(all(test, target_os = "macos"))]
pub(crate) use orchestration::flag_is_on;
#[cfg(target_os = "macos")]
pub use orchestration::*;
