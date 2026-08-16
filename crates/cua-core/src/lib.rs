//! Snapshot model, app resolution and stable element handles.
//!
//! `cua-ax` knows how to talk to the Accessibility API and `cua-capture` knows
//! how to get pixels. This crate is the policy layer that makes them usable by
//! a language model:
//!
//! - [`apps::resolve_app`] turns whatever string a model produced into exactly
//!   one running app, or an error — never a guess.
//! - [`session::Cua`] owns the one thread that may touch native handles, and
//!   holds the latest snapshot per app.
//! - [`snapshot::render_tree`] spends tokens only on elements worth seeing.
//!
//! The unit that ties it together is the *snapshot*: one tree walk plus one
//! screenshot of one window, taken together and numbered. Every action refers
//! back to an index in that walk, and a snapshot id makes a stale reference an
//! error instead of a mis-click.

pub mod apps;
mod overlay;
pub mod session;
pub mod snapshot;

pub use apps::{activate, frontmost_pid, list_apps, resolve_app, AppInfo, ResolveError};
pub use session::{
    ActionResult, AppState, CoreError, Cua, Delivery, FindResult, FocusCheck, FocusState,
    Mechanism, MouseOptions, Observed, Permissions, PointerLocation, PostActionState, Presence,
    Screenshot, ScrollAmount, ScrollDir, StateOptions, Target, WaitOutcome, WindowPixel,
};

/// Re-exported so a caller can name a mouse button or a modifier set without
/// depending on `cua-hid` directly. `cua-hid` is the only crate that
/// synthesizes input, and that boundary is worth keeping visible in the
/// dependency graph rather than spreading across every consumer.
pub use cua_hid::{Modifiers, MouseButton};
pub use snapshot::{diff_trees, render_tree, RenderOptions, TreeDiff};

/// Re-exported so callers can tune tree limits without depending on `cua-ax`
/// directly.
pub use cua_ax::Limits;
