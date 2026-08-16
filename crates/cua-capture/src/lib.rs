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

// ── enumeration ──────────────────────────────────────────────────────────────

/// Every window ScreenCaptureKit is willing to share, newest content first.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    if !has_screen_recording_permission() {
        return Err(CaptureError::NotPermitted);
    }
    let content = shareable_content()?;
    let windows = unsafe { content.windows() };

    let mut out = Vec::with_capacity(windows.len());
    for w in windows.iter() {
        let app = unsafe { w.owningApplication() };
        out.push(WindowInfo {
            id: unsafe { w.windowID() },
            title: unsafe { w.title() }
                .map(|t| t.to_string())
                .filter(|t| !t.is_empty()),
            pid: app.as_ref().map(|a| unsafe { a.processID() }).unwrap_or(-1),
            bundle_id: app
                .as_ref()
                .map(|a| unsafe { a.bundleIdentifier() }.to_string()),
            app_name: app
                .as_ref()
                .map(|a| unsafe { a.applicationName() }.to_string()),
            frame: unsafe { w.frame() },
            on_screen: unsafe { w.isOnScreen() },
            layer: unsafe { w.windowLayer() } as i64,
        });
    }
    Ok(out)
}

// ── capture ──────────────────────────────────────────────────────────────────

/// Capture one window as PNG, downscaled so neither side exceeds `max_dim`.
///
/// `max_dim` exists because the consumer is a vision model with a fixed image
/// budget, not a human zooming in. Passing `0` disables the downscale.
pub fn capture_window(window_id: CGWindowID, max_dim: u32) -> Result<WindowShot> {
    if !has_screen_recording_permission() {
        return Err(CaptureError::NotPermitted);
    }

    // Re-enumerate immediately before capture. Besides providing the live
    // frame, this rejects a closed/recycled window before invoking a tool.
    // Costs p50 ~28 ms with a couple of hundred windows live, which is why the
    // same list is also mined for the pop-ups that may end up in the image
    // rather than enumerated a second time below.
    let live = list_windows()?;
    let window = live
        .iter()
        .find(|window| window.id == window_id)
        .ok_or(CaptureError::WindowGone(window_id))?;
    let frame = window.frame;
    let pid = window.pid;
    // Every pop-up of the same app is a candidate for having been swept into
    // the image, because `screencapture -l<id>` photographs the window group.
    let attached: Vec<CGRect> = live
        .iter()
        .filter(|w| w.pid == pid && w.id != window_id && w.is_transient_popup())
        .map(|w| w.frame)
        .collect();
    if !valid_capture_frame(frame) {
        return Err(CaptureError::InvalidFrame {
            window_id,
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        });
    }

    let temp = CaptureTempDir::new()?;
    let png_path = temp.path.join("window.png");
    let mut capture = Command::new("/usr/sbin/screencapture");
    capture
        .arg("-x")
        .arg("-o")
        .arg(format!("-l{window_id}"))
        .arg(&png_path);
    run_capture_process(capture, "screencapture")?;

    let mut png = fs::read(&png_path)
        .map_err(|e| CaptureError::Failed(format!("could not read captured PNG: {e}")))?;
    let (mut width, mut height) = png_dimensions(&png)?;
    if max_dim > 0 && width.max(height) > max_dim {
        let mut resize = Command::new("/usr/bin/sips");
        resize
            .arg("--resampleHeightWidthMax")
            .arg(max_dim.to_string())
            .arg(&png_path);
        run_capture_process(resize, "sips")?;
        png = fs::read(&png_path)
            .map_err(|e| CaptureError::Failed(format!("could not read resized PNG: {e}")))?;
        (width, height) = png_dimensions(&png)?;
    }

    let (covered, scale) = capture_extent(frame, &attached, width, height);
    Ok(WindowShot {
        png,
        width,
        height,
        scale,
        frame: covered,
        window_frame: frame,
    })
}

/// Work out which screen rect the returned pixels actually cover.
///
/// `screencapture -l<id>` does not promise one window's bounds; it returns the
/// window and whatever is attached to it, and the caller only finds out from the
/// pixel count. So rather than assume, this tests the hypotheses against the
/// image: a capture of rect `r` must have the same pixels-per-point
/// horizontally and vertically, and only the right `r` does. The requested frame
/// is tried first, then the frame unioned with each same-app pop-up, then with
/// all of them.
///
/// When nothing fits, the requested frame is returned with the horizontal ratio,
/// which is the behaviour this replaced. A guess is not improved by refusing to
/// make one, but it is improved by only making it when the evidence is absent.
fn capture_extent(frame: CGRect, attached: &[CGRect], width: u32, height: u32) -> (CGRect, f64) {
    /// Ratios this far apart are not the same number. Rounding at the edges of a
    /// capture is worth a fraction of a percent; a wrong extent is worth tens.
    const RATIO_TOLERANCE: f64 = 0.01;

    let mut candidates = Vec::with_capacity(attached.len() + 2);
    candidates.push(frame);
    for popup in attached {
        candidates.push(union(frame, *popup));
    }
    if attached.len() > 1 {
        candidates.push(attached.iter().fold(frame, |acc, p| union(acc, *p)));
    }

    let mut best: Option<(f64, CGRect, f64)> = None;
    for rect in candidates {
        if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            continue;
        }
        let sx = width as f64 / rect.size.width;
        let sy = height as f64 / rect.size.height;
        let error = (sx - sy).abs() / sx.max(sy);
        if best.as_ref().is_none_or(|(e, _, _)| error < *e) {
            best = Some((error, rect, (sx + sy) / 2.0));
        }
    }

    match best {
        Some((error, rect, scale)) if error <= RATIO_TOLERANCE => (rect, scale),
        _ => (frame, width as f64 / frame.size.width),
    }
}

fn union(a: CGRect, b: CGRect) -> CGRect {
    let min_x = a.origin.x.min(b.origin.x);
    let min_y = a.origin.y.min(b.origin.y);
    let max_x = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let max_y = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    CGRect {
        origin: objc2_core_foundation::CGPoint { x: min_x, y: min_y },
        size: objc2_core_foundation::CGSize {
            width: max_x - min_x,
            height: max_y - min_y,
        },
    }
}

fn valid_capture_frame(frame: CGRect) -> bool {
    [
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    ]
    .into_iter()
    .all(f64::is_finite)
        && frame.size.width > 0.0
        && frame.size.height > 0.0
}

struct CaptureTempDir {
    path: PathBuf,
}

impl CaptureTempDir {
    fn new() -> Result<Self> {
        for _ in 0..16 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("cua-rs-capture-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(CaptureError::Failed(format!(
                        "could not create capture temp directory: {error}"
                    )))
                }
            }
        }
        Err(CaptureError::Failed(
            "could not allocate a unique capture temp directory".into(),
        ))
    }
}

impl Drop for CaptureTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_capture_process(mut command: Command, name: &str) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CaptureError::WorkerFailed(format!("could not start {name}: {e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CaptureError::WorkerFailed(format!("{name} stderr pipe missing")))?;
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < CAPTURE_PROCESS_TIMEOUT => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(CaptureError::Timeout(CAPTURE_PROCESS_TIMEOUT));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(CaptureError::WorkerFailed(format!(
                    "could not observe {name}: {error}"
                )));
            }
        }
    };

    let stderr = stderr_reader
        .join()
        .map_err(|_| CaptureError::WorkerFailed(format!("{name} stderr reader panicked")))?
        .map_err(|e| CaptureError::WorkerFailed(format!("could not read {name} stderr: {e}")))?;
    if status.success() {
        return Ok(());
    }
    let detail = status.signal().map_or_else(
        || format!("exited with status {}", status.code().unwrap_or(-1)),
        |signal| format!("terminated by signal {signal}"),
    );
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = stderr.trim();
    Err(CaptureError::WorkerFailed(if stderr.is_empty() {
        format!("{name} {detail}")
    } else {
        format!("{name} {detail}: {stderr}")
    }))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return Err(CaptureError::Encode);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("four bytes"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("four bytes"));
    if width == 0 || height == 0 {
        return Err(CaptureError::Encode);
    }
    Ok((width, height))
}

// ── ObjC async bridging ──────────────────────────────────────────────────────

/// Run `SCShareableContent`'s async query synchronously.
fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel::<std::result::Result<usize, String>>();

    // The completion handler runs on an SCK-owned queue, so the pointer has to
    // cross a thread boundary. `Retained<T>` is not `Send`, so the retained
    // object is passed as a `usize` and rebuilt on this side. The retain taken
    // inside the block is what keeps it alive across the handoff.
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let msg = if error.is_null() {
                None
            } else {
                Some(unsafe { &*error }.localizedDescription().to_string())
            };
            let payload = match (content.is_null(), msg) {
                (false, _) => {
                    let retained: Retained<SCShareableContent> =
                        unsafe { Retained::retain(content) }.expect("non-null");
                    Ok(Retained::into_raw(retained) as usize)
                }
                (true, Some(m)) => Err(m),
                (true, None) => Err("ScreenCaptureKit returned no content".to_string()),
            };
            let _ = tx.send(payload);
        },
    );

    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            // Desktop windows are wallpaper and icon layers: never a target.
            true,
            // `false` keeps windows that are occluded or on another Space,
            // which are exactly the ones this whole crate exists to reach.
            false,
            &block,
        );
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(ptr)) => {
            let raw = ptr as *mut SCShareableContent;
            Ok(unsafe { Retained::from_raw(raw) }.expect("non-null"))
        }
        Ok(Err(msg)) => Err(classify(msg)),
        Err(_) => Err(CaptureError::Timeout(CAPTURE_TIMEOUT)),
    }
}

/// Map an SCK error string onto a typed error.
///
/// SCK reports a missing Screen Recording grant as a generic
/// `SCStreamErrorUserDeclined` / "declined" failure rather than anything
/// structured, so the actionable case is recovered from the text.
fn classify(msg: String) -> CaptureError {
    let lower = msg.to_lowercase();
    if lower.contains("declined")
        || lower.contains("permission")
        || lower.contains("not authorized")
    {
        CaptureError::NotPermitted
    } else {
        CaptureError::Failed(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_transient_frames_are_rejected_before_capture() {
        let frame = |x, y, width, height| CGRect {
            origin: objc2_core_foundation::CGPoint { x, y },
            size: objc2_core_foundation::CGSize { width, height },
        };
        assert!(valid_capture_frame(frame(-100.0, 20.0, 800.0, 600.0)));
        assert!(!valid_capture_frame(frame(0.0, 0.0, 0.0, 600.0)));
        assert!(!valid_capture_frame(frame(0.0, 0.0, 800.0, -1.0)));
        assert!(!valid_capture_frame(frame(f64::NAN, 0.0, 800.0, 600.0)));
        assert!(!valid_capture_frame(frame(
            0.0,
            f64::INFINITY,
            800.0,
            600.0
        )));
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect {
            origin: objc2_core_foundation::CGPoint { x, y },
            size: objc2_core_foundation::CGSize { width, height },
        }
    }

    fn window(layer: i64, width: f64, height: f64) -> WindowInfo {
        WindowInfo {
            id: 1,
            title: None,
            pid: 1,
            bundle_id: None,
            app_name: None,
            frame: rect(0.0, 0.0, width, height),
            on_screen: true,
            layer,
        }
    }

    #[test]
    fn a_popup_is_recognised_above_the_ordinary_levels_only() {
        // The boundary, from both sides. Level 3 is `kCGFloatingWindowLevel`
        // *and* `kCGTornOffMenuWindowLevel`, so it stays ordinary content: an
        // app's floating chat window is not transient UI a click just opened.
        assert!(!window(3, 800.0, 600.0).is_transient_popup());
        assert!(window(4, 800.0, 600.0).is_transient_popup());
        // The measured case: KakaoTalk's chat-room hamburger menu.
        assert!(window(101, 202.0, 318.0).is_transient_popup());
    }

    #[test]
    fn the_menu_bar_the_overlay_and_the_desktop_are_not_popups() {
        assert!(
            !window(24, 1512.0, 300.0).is_transient_popup(),
            "the system menu bar is not a pop-up an action opened"
        );
        assert!(
            !window(25, 800.0, 600.0).is_transient_popup(),
            "cua-rs must never read its own drawn-cursor overlay back as app UI"
        );
        assert!(
            !window(-2147483623, 1512.0, 982.0).is_transient_popup(),
            "desktop backing stores are below ordinary content, not above it"
        );
        for w in [window(24, 1512.0, 300.0), window(25, 800.0, 600.0)] {
            assert!(!w.is_addressable_target(), "and none of them is clickable");
        }
    }

    #[test]
    fn a_popup_has_to_be_on_screen_and_big_enough() {
        let mut offscreen = window(101, 202.0, 318.0);
        offscreen.on_screen = false;
        assert!(
            !offscreen.is_transient_popup(),
            "a menu nobody can see is not one a caller can click"
        );

        assert!(
            !window(101, 39.0, 318.0).is_transient_popup(),
            "below the size floor on either side"
        );
        assert!(!window(101, 202.0, 39.0).is_transient_popup());
        assert!(
            window(101, 40.0, 40.0).is_transient_popup(),
            "the floor itself is allowed"
        );
    }

    #[test]
    fn the_capture_extent_is_read_off_the_pixels_not_assumed() {
        // Measured on KakaoTalk: parent window 46,86 924x770 with its menu at
        // 938,599 202x318, and `screencapture -l<id>` returned the same
        // 2188x1662 image for *either* id. That is the union, 1094x831 points
        // at 2x — and taking the requested frame as the extent would have
        // reported the parent at 2.37 px/pt and the menu at 10.83.
        let parent = rect(46.0, 86.0, 924.0, 770.0);
        let menu = rect(938.0, 599.0, 202.0, 318.0);

        let (covered, scale) = capture_extent(parent, &[menu], 2188, 1662);
        assert_eq!((covered.size.width, covered.size.height), (1094.0, 831.0));
        assert!((scale - 2.0).abs() < 1e-9, "scale was {scale}");

        // Asking for the menu returns the same picture, and must describe it
        // the same way.
        let (covered, scale) = capture_extent(menu, &[parent], 2188, 1662);
        assert_eq!((covered.origin.x, covered.origin.y), (46.0, 86.0));
        assert!((scale - 2.0).abs() < 1e-9, "scale was {scale}");
    }

    #[test]
    fn a_window_with_no_popup_open_keeps_its_own_frame() {
        let frame = rect(46.0, 86.0, 924.0, 770.0);
        let (covered, scale) = capture_extent(frame, &[], 1848, 1540);
        assert_eq!((covered.size.width, covered.size.height), (924.0, 770.0));
        assert!((scale - 2.0).abs() < 1e-9);

        // A pop-up that is open but was plainly not in this picture must not
        // rewrite the extent either.
        let elsewhere = rect(2000.0, 2000.0, 202.0, 318.0);
        let (covered, _) = capture_extent(frame, &[elsewhere], 1848, 1540);
        assert_eq!(covered.origin.x, 46.0);
        assert_eq!(covered.size.width, 924.0);
    }

    #[test]
    fn an_unexplainable_image_falls_back_to_the_requested_frame() {
        // No hypothesis fits, so the old behaviour stands rather than a
        // union being adopted on no evidence.
        let frame = rect(0.0, 0.0, 800.0, 600.0);
        let popup = rect(100.0, 100.0, 200.0, 200.0);
        let (covered, scale) = capture_extent(frame, &[popup], 999, 111);
        assert_eq!(covered.size.width, 800.0);
        assert!((scale - 999.0 / 800.0).abs() < 1e-9);
    }

    #[test]
    fn png_dimensions_read_ihdr_without_decoding_pixels() {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1200_u32.to_be_bytes());
        png.extend_from_slice(&800_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png).unwrap(), (1200, 800));
        assert!(png_dimensions(b"not a png").is_err());
    }

    #[test]
    fn a_crashing_capture_process_becomes_an_error() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("kill -ABRT $$");
        let error = run_capture_process(command, "crash-probe").unwrap_err();
        assert!(
            error.to_string().contains("signal 6"),
            "unexpected error: {error}"
        );
        // Reaching this assertion is the contract: the persistent Rust process
        // survived an abort in its disposable capture boundary.
    }

    #[test]
    fn tiny_and_overlay_windows_are_not_targets() {
        let base = WindowInfo {
            id: 1,
            title: None,
            pid: 1,
            bundle_id: None,
            app_name: None,
            frame: CGRect {
                origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
                size: objc2_core_foundation::CGSize {
                    width: 800.0,
                    height: 600.0,
                },
            },
            on_screen: true,
            layer: 0,
        };
        assert!(base.is_plausible_target());

        let overlay = WindowInfo {
            layer: 25,
            ..base.clone()
        };
        assert!(
            !overlay.is_plausible_target(),
            "status overlays are not targets"
        );

        // Measured against KakaoTalk, which puts chat-room windows at the
        // floating level. Excluding these made the click path stamp events with
        // a different window's number, so they were silently discarded.
        let floating = WindowInfo {
            layer: 3,
            ..base.clone()
        };
        assert!(
            floating.is_plausible_target(),
            "floating-level content windows are ordinary targets"
        );

        // A pop-up menu is still not the window an accessibility tree is
        // describing — that is what this predicate answers, and answering yes
        // here would let a menu be chosen as an app's snapshot window and have
        // its number stamped onto clicks meant for content. Being *addressable*
        // is a different question, settled by the test below.
        let popup_menu = WindowInfo {
            layer: 101,
            ..base.clone()
        };
        assert!(
            !popup_menu.is_plausible_target(),
            "a pop-up menu is never the window a snapshot is of"
        );
        assert!(
            popup_menu.is_addressable_target(),
            "but a caller may aim at one on purpose"
        );

        let desktop = WindowInfo {
            layer: -2147483623,
            ..base.clone()
        };
        assert!(
            !desktop.is_plausible_target(),
            "desktop backing stores are not targets"
        );

        let sliver = WindowInfo {
            frame: CGRect {
                origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
                size: objc2_core_foundation::CGSize {
                    width: 1.0,
                    height: 1.0,
                },
            },
            ..base
        };
        assert!(
            !sliver.is_plausible_target(),
            "1x1 tracking windows are not targets"
        );
    }
}
