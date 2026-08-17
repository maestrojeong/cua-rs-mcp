//! Crash-isolated per-window screen capture on macOS.
//!
//! # Why not `CGWindowListCreateImage`
//!
//! The old CoreGraphics call is one line and works, which is why almost every
//! automation tool still uses it. It is also deprecated as of macOS 14, and it
//! has a defect that matters specifically for agents: it can only return what
//! the window server has actually composited, so a window that is occluded,
//! minimized, or on another Space comes back blank or stale.
//!
//! ScreenCaptureKit is still used to enumerate stable window identities and
//! frames. Pixel capture is delegated by window id to macOS's one-shot
//! `/usr/sbin/screencapture` process. This preserves background/off-Space
//! capture while putting a process boundary around WindowServer assertions:
//! malformed transient window state can fail one screenshot, not the MCP server.
//!
//! # Why per-window and not full-screen
//!
//! A full-screen grab of a 5K display is ~15 MB of pixels, most of it wallpaper
//! and the human's unrelated windows. Downscaled to something an LLM can
//! actually ingest, the target app's text is illegible. Capturing one window
//! spends the entire pixel budget on the thing being driven, and it means an
//! agent's screenshots do not silently exfiltrate whatever else the user had
//! open.
//!
//! # Coordinates
//!
//! [`WindowShot::scale`] is the bridge between the two coordinate spaces this
//! project has to keep straight: the Accessibility API talks in points, and
//! screenshots are in pixels. On a Retina display those differ by 2x, and on a
//! mixed-DPI multi-monitor setup they differ *per display*. Getting this wrong
//! produces clicks that land at half or double the intended offset, so the
//! scale is captured per shot rather than assumed.

use std::fs;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::CGWindowID;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCShareableContent;

// ── errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, thiserror::Error)]
pub enum CaptureError {
    /// The Screen Recording grant is missing. Like the Accessibility grant this
    /// is user-actionable, so it carries its own remedy.
    #[error("screen recording permission denied. Grant it in System Settings > Privacy & Security > Screen Recording, then restart this server")]
    NotPermitted,

    #[error("window {0} not found (it may have closed)")]
    WindowGone(CGWindowID),

    /// ScreenCaptureKit never called back. Distinct from an explicit failure:
    /// it usually means the owning app is wedged and cannot render itself.
    #[error("screen capture timed out after {0:?} (the app may not be responding)")]
    Timeout(Duration),

    #[error("screen capture failed: {0}")]
    Failed(String),

    #[error("window {window_id} has an invalid transient frame ({x}, {y}, {width} x {height}); retry after the app finishes rebuilding its windows")]
    InvalidFrame {
        window_id: CGWindowID,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },

    /// Window capture is deliberately delegated to a one-shot macOS process so
    /// framework assertions cannot terminate the persistent MCP server.
    #[error("isolated screen-capture worker failed: {0}")]
    WorkerFailed(String),

    #[error("could not encode the captured image as PNG")]
    Encode,
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// How long to wait for ScreenCaptureKit before giving up.
///
/// Generous, because the first call in a process pays for SCK's one-time setup
/// and the target app has to render a frame on demand.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

const CAPTURE_PROCESS_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

// ── permission ───────────────────────────────────────────────────────────────

/// Whether this process holds the Screen Recording grant, without prompting.
///
/// Checked up front because SCK's failure mode when the grant is missing is
/// unhelpful: `getShareableContent` succeeds and simply returns an empty window
/// list, which is indistinguishable from "no windows open". Preflighting turns
/// that into an error the user can act on.
pub fn has_screen_recording_permission() -> bool {
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Trigger the system Screen Recording prompt once.
///
/// Only useful when the server was launched from a context that can show UI.
/// Returns whether access is granted; macOS requires a relaunch after the user
/// approves, so a `false` here is expected on first run.
pub fn request_screen_recording_permission() -> bool {
    extern "C" {
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    unsafe { CGRequestScreenCaptureAccess() }
}

// ── types ────────────────────────────────────────────────────────────────────

/// One captured window.
#[derive(Debug, Clone)]
pub struct WindowShot {
    pub png: Vec<u8>,
    /// Pixel dimensions of `png`, after any downscale.
    pub width: u32,
    pub height: u32,
    /// Pixels per point for this capture: `width / frame.size.width`.
    ///
    /// Multiply an AX point coordinate by this to get a screenshot pixel, and
    /// divide to go back. Includes both the display's backing scale and any
    /// downscale [`capture_window`] applied.
    pub scale: f64,
    /// The screen rect this image actually covers, in global points. Subtract
    /// its origin from an AX point and multiply by [`WindowShot::scale`] to get
    /// a pixel in `png`.
    ///
    /// Usually the requested window's own frame, and deliberately **not**
    /// assumed to be. `screencapture -l<id>` photographs the window *group*:
    /// while a pop-up menu is open, asking for either the parent window's id or
    /// the menu's returns the same image, covering the union of the two. On
    /// KakaoTalk that was a parent at `46,86 924x770` plus a menu at
    /// `938,599 202x318`, and both ids returned one 2188x1662 image — exactly
    /// `1094x831` points at 2x. Taking the requested window's frame as the
    /// extent made `scale` read 2.37 for the parent and 10.83 for the menu
    /// instead of 2.0, so every pixel-to-point conversion against that image was
    /// wrong while a menu was up. See [`WindowShot::window_frame`].
    pub frame: CGRect,
    /// The frame of the window that was actually asked for.
    ///
    /// Differs from [`WindowShot::frame`] exactly when the image came back
    /// larger than the window — the caller can compare the two to find out that
    /// something else of this app is in the picture, and where the window it
    /// asked about sits inside it.
    pub window_frame: CGRect,
}

/// A window as seen by ScreenCaptureKit.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: CGWindowID,
    pub title: Option<String>,
    pub pid: libc::pid_t,
    pub bundle_id: Option<String>,
    pub app_name: Option<String>,
    pub frame: CGRect,
    pub on_screen: bool,
    /// Window layer, in `NSWindow` level terms. `0` is normal content and `3`
    /// is floating; the high levels are menus, status items and overlays.
    pub layer: i64,
}

/// Highest window level still treated as ordinary content.
///
/// `kCGNormalWindowLevel` is 0 and `kCGFloatingWindowLevel` is 3. Read from the
/// installed SDK rather than assumed: `kCGMainMenuWindowLevel` is 24,
/// `kCGStatusWindowLevel` is 25 and `kCGPopUpMenuWindowLevel` is 101, all far
/// above this ceiling — **but `kCGTornOffMenuWindowLevel` is also 3**, sharing
/// its level with ordinary floating panels. Level alone therefore cannot
/// separate a floating content window from a torn-off menu, and this constant
/// must not be read as "menus are excluded".
///
/// What keeps that from mattering is the caller. A target is chosen by matching
/// the AX window's frame, so a menu window has to coincide with the frame of the
/// window accessibility is showing to be picked at all. The one place that
/// evidence is absent — the no-AX-frame fallback, which just takes the largest
/// window — is restricted to level 0 for exactly this reason.
///
/// This ceiling was raised from 0 after a measured failure: KakaoTalk publishes
/// its chat-room windows at level 3, so a layer-0 rule dropped them from the
/// candidate set entirely. The click path then matched some *other* window of
/// the same process, stamped that window's number onto the event, and the
/// target discarded input aimed at a window it was not for — which looked
/// exactly like "this control ignores synthetic clicks".
const MAX_ORDINARY_WINDOW_LEVEL: i64 = 3;

/// `kCGMainMenuWindowLevel`. The system menu bar itself, never a pop-up.
const MAIN_MENU_WINDOW_LEVEL: i64 = 24;

/// `kCGStatusWindowLevel` — status items, and the level cua-rs's own
/// drawn-cursor overlay uses.
///
/// Excluded from the pop-up rule in both directions. A menu-bar extra is not a
/// thing an agent opened, and cua-rs must never report or click its own overlay:
/// that would be the tool reading its own drawing back as if it were the app's.
/// Per-pid filtering already keeps the overlay out, since it is a separate
/// process; this is the second lock on the same door.
const OVERLAY_WINDOW_LEVEL: i64 = 25;

/// Smallest side, in points, a window can have and still be worth addressing.
const MIN_TARGET_SIDE: f64 = 40.0;

impl WindowInfo {
    /// Whether this looks like a real document/content window rather than
    /// chrome.
    ///
    /// SCK reports a lot of windows that are technically real but useless as
    /// automation targets: 1x1 tracking windows, zero-size offscreen buffers,
    /// status item overlays. Filtering on window level plus a minimum area
    /// removes nearly all of them without needing a per-app blocklist.
    ///
    /// Negative levels are desktop and wallpaper backing stores, which are
    /// never a target either.
    pub fn is_plausible_target(&self) -> bool {
        (0..=MAX_ORDINARY_WINDOW_LEVEL).contains(&self.layer) && self.is_big_enough()
    }

    /// Whether this looks like transient UI an app put up above its own
    /// content: a pop-up menu, a context menu, a menu the menu bar opened.
    ///
    /// This is the other half of the window world, and until now cua-rs had no
    /// name for it. Measured on KakaoTalk: clicking a chat window's hamburger
    /// creates a second window of the same process at level 101, 202x318, on
    /// screen within ~50 ms and still there 2.5 s later. Accessibility does not
    /// describe it at all — the application element has only its two
    /// `AXMenuBar` children, and a hit test inside the menu's own frame returns
    /// the menu bar as a fallback. The window server is the only thing that can
    /// see it, so the window list is the only place it can be reported from.
    ///
    /// Above ordinary content, on screen, and big enough to hold a control. The
    /// menu bar and the status level are cut out by name rather than by height,
    /// because a status item is not a pop-up an action opened and the overlay is
    /// cua-rs's own.
    pub fn is_transient_popup(&self) -> bool {
        self.on_screen
            && self.layer > MAX_ORDINARY_WINDOW_LEVEL
            && self.layer != MAIN_MENU_WINDOW_LEVEL
            && self.layer != OVERLAY_WINDOW_LEVEL
            && self.is_big_enough()
    }

    /// Whether a pid-routed event may be stamped with this window's number.
    ///
    /// Wider than [`WindowInfo::is_plausible_target`], and the gap between them
    /// is deliberate. That predicate answers "is this the window the
    /// accessibility tree is describing", which chooses what a snapshot is *of*
    /// and must stay narrow: a menu picked there would have its number stamped
    /// onto clicks meant for content, which is the failure the level cap was
    /// raised to fix in the first place. This one answers a different question —
    /// "may the caller aim at this window on purpose" — and a pop-up has to
    /// answer yes, because accessibility cannot see inside one and a
    /// window-local coordinate is the only way in.
    ///
    /// Desktop and wallpaper levels, the menu bar, the status level and anything
    /// too small to hold a control stay refused under both.
    pub fn is_addressable_target(&self) -> bool {
        self.is_plausible_target() || self.is_transient_popup()
    }

    fn is_big_enough(&self) -> bool {
        self.frame.size.width >= MIN_TARGET_SIDE && self.frame.size.height >= MIN_TARGET_SIDE
    }
}

mod capture;
mod window_catalog;

pub use capture::capture_window;
pub use window_catalog::list_windows;
