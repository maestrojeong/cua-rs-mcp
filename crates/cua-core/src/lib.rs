//! Snapshot model, app resolution and stable element handles.
//!
//! `cua-ax` knows how to talk to the Accessibility API and `cua-capture` knows
//! how to get pixels. This crate is the policy layer that makes them usable by
//! a language model:
//!
//! - [`apps::resolve_app`] turns whatever string a model produced into exactly
//!   one running app, or an error — never a guess.
//! - [`safety`] decides whether an action should happen at all, before
//!   `session` works out how to deliver it.
//! - [`session::Cua`] owns the one thread that may touch native handles, and
//!   holds the latest snapshot per app.
//! - [`snapshot::render_tree`] spends tokens only on elements worth seeing.
//! - [`outcome::ErrorOutcome`] gives every `CoreError` a stable `code` and a
//!   `disposition` next to its existing prose, so a caller can decide whether
//!   a retry is safe without re-parsing English.
//!
//! The unit that ties it together is the *snapshot*: one tree walk plus one
//! screenshot of one window, taken together and numbered. Every action refers
//! back to an index in that walk, and a snapshot id makes a stale reference an
//! error instead of a mis-click.

pub mod apps;
// Private on purpose: the only way to reach the pasteboard from outside this
// crate is through `Cua`, which runs it on the native worker behind the gates.
#[cfg(target_os = "macos")]
mod clipboard;
pub mod menubar;
pub mod notifications;
pub mod outcome;
#[cfg(target_os = "macos")]
mod overlay;
pub mod platform;
pub mod safety;
pub mod session;
pub mod snapshot;

#[cfg(target_os = "macos")]
pub use apps::{activate, frontmost_pid, list_apps, resolve_app};
pub use apps::{AppInfo, ResolveError};
pub use menubar::{menu_path_steps, menu_shortcut, MenuItem, MenuListing, MenuWalkError};
pub use outcome::{disposition_of, Disposition, ErrorCode, ErrorOutcome, PolicyReason};
pub use safety::Refused;
pub use session::{
    ActionResult, AppState, ClipboardAccess, ClipboardContents, ClipboardRead, ClipboardWrite,
    CoreError, Delivery, FindResult, FocusCheck, FocusState, Mechanism, MouseOptions, Observed,
    Permissions, PointerLocation, PostActionState, Presence, Screenshot, ScrollAmount, ScrollDir,
    StateOptions, Target, TransientWindow, WaitOutcome, WindowPixel,
};
pub use session::{
    CloseOutcome, LaunchOutcome, WindowListing, WindowOp, WindowOutcome, WindowSelector,
    WindowSummary,
};
/// The macOS backend: the native worker handle and its prepared operations.
/// `Cua` implements [`PlatformAdapter`]; see `platform::macos`.
#[cfg(target_os = "macos")]
pub use session::{Cua, Op};

/// Re-exported so the MCP layer can render a display, name a frame, a mouse
/// button, a modifier set or tree limits without depending on a platform crate
/// directly. On macOS these are exactly `cua_capture::DisplayInfo`,
/// `objc2_core_foundation::CGRect`, `cua_hid::{Modifiers, MouseButton}` and
/// `cua_ax::Limits`, as they always were — `cua-hid` stays the only crate that
/// synthesizes input, and that boundary stays visible in the dependency graph.
/// Elsewhere they are [`platform::portable`]'s stand-ins.
pub use platform::leaf::{CGRect, DisplayInfo, Limits, Modifiers, MouseButton};
pub use platform::{PlatformAdapter, PLATFORM};
pub use snapshot::{diff_trees, render_tree, RenderOptions, TreeDiff};
