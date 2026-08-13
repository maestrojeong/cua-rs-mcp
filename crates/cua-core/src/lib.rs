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
pub mod session;
pub mod snapshot;

pub use apps::{list_apps, resolve_app, AppInfo, ResolveError};
pub use session::{
    allow_hid, hid_allowed, ActionResult, AppState, CoreError, Cua, Delivery, FindResult,
    Observed, Permissions, Presence, Screenshot, ScrollDir, StateOptions, Target,
    WaitOutcome,
};
pub use snapshot::{render_tree, RenderOptions};

/// Re-exported so callers can tune tree limits without depending on `cua-ax`
/// directly.
pub use cua_ax::Limits;
