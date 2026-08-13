//! Per-window screen capture via ScreenCaptureKit.
//!
//! # Why not `CGWindowListCreateImage`
//!
//! The old CoreGraphics call is one line and works, which is why almost every
//! automation tool still uses it. It is also deprecated as of macOS 14, and it
//! has a defect that matters specifically for agents: it can only return what
//! the window server has actually composited, so a window that is occluded,
//! minimized, or on another Space comes back blank or stale.
//!
//! ScreenCaptureKit asks the *owning app* to render the window instead. A
//! background window that the human has completely covered with something else
//! still captures correctly, which is the entire premise of an agent that works
//! alongside you rather than taking over your screen.
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

use std::sync::mpsc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::{CGImage, CGWindowID};
use objc2_foundation::{NSDictionary, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

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

    #[error("could not encode the captured image as PNG")]
    Encode,
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// How long to wait for ScreenCaptureKit before giving up.
///
/// Generous, because the first call in a process pays for SCK's one-time setup
/// and the target app has to render a frame on demand.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The window's frame in global points, as ScreenCaptureKit reports it.
    /// This is the origin that AX coordinates are relative to.
    pub frame: CGRect,
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
    /// Window layer. `0` is normal content; anything else is a panel, menu,
    /// dock tile, or overlay, which an agent almost never wants to drive.
    pub layer: i64,
}

impl WindowInfo {
    /// Whether this looks like a real document/content window rather than
    /// chrome.
    ///
    /// SCK reports a lot of windows that are technically real but useless as
    /// automation targets: 1x1 tracking windows, zero-size offscreen buffers,
    /// status item overlays. Filtering on layer plus a minimum area removes
    /// nearly all of them without needing a per-app blocklist.
    pub fn is_plausible_target(&self) -> bool {
        self.layer == 0 && self.frame.size.width >= 40.0 && self.frame.size.height >= 40.0
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

    let content = shareable_content()?;
    let windows = unsafe { content.windows() };
    let target = windows
        .iter()
        .find(|w| unsafe { w.windowID() } == window_id)
        .ok_or(CaptureError::WindowGone(window_id))?;

    let frame = unsafe { target.frame() };
    let shot = capture_sc_window(&target, frame, max_dim)?;
    Ok(shot)
}

fn capture_sc_window(window: &SCWindow, frame: CGRect, max_dim: u32) -> Result<WindowShot> {
    // `initWithDesktopIndependentWindow` is the filter that makes occluded and
    // off-Space windows capture correctly: it asks for that window's content in
    // isolation, rather than a crop out of the composited desktop.
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };

    let config = unsafe { SCStreamConfiguration::new() };

    // Ask for the window at its backing-store resolution, then clamp. Requesting
    // point dimensions would hand back a soft, half-resolution image on Retina
    // and make small UI text unreadable to a vision model.
    let scale = unsafe { SCShareableContent::infoForFilter(&filter) };
    let point_scale = unsafe { scale.pointPixelScale() } as f64;
    let native_w = (frame.size.width * point_scale).round().max(1.0);
    let native_h = (frame.size.height * point_scale).round().max(1.0);

    let (px_w, px_h) = clamp_dimensions(native_w, native_h, max_dim);
    unsafe {
        config.setWidth(px_w as usize);
        config.setHeight(px_h as usize);
        // The agent's cursor is not the human's cursor, and drawing one implies
        // a pointer position that means nothing in an AX-driven session.
        config.setShowsCursor(false);
        // Shadows are transparent padding around the frame. Keeping them would
        // offset every pixel coordinate relative to the AX frame.
        config.setIgnoreShadowsSingleWindow(true);
    }

    let image = capture_image_blocking(&filter, &config)?;
    let png = encode_png(&image)?;

    Ok(WindowShot {
        png,
        width: px_w,
        height: px_h,
        // Derived from what we actually got, not from what we asked for, so a
        // clamp or an SCK adjustment cannot desynchronize the mapping.
        scale: if frame.size.width > 0.0 {
            px_w as f64 / frame.size.width
        } else {
            point_scale
        },
        frame,
    })
}

/// Fit `(w, h)` inside a `max_dim` box, preserving aspect ratio.
fn clamp_dimensions(w: f64, h: f64, max_dim: u32) -> (u32, u32) {
    if max_dim == 0 {
        return (w.max(1.0) as u32, h.max(1.0) as u32);
    }
    let max = max_dim as f64;
    let longest = w.max(h);
    if longest <= max {
        return (w.max(1.0) as u32, h.max(1.0) as u32);
    }
    let k = max / longest;
    (
        ((w * k).round().max(1.0)) as u32,
        ((h * k).round().max(1.0)) as u32,
    )
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

/// Run `SCScreenshotManager`'s async capture synchronously.
fn capture_image_blocking(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<CFRetained<CGImage>> {
    let (tx, rx) = mpsc::channel::<std::result::Result<usize, String>>();

    // Same `usize` handoff as `shareable_content`, for the same reason: the
    // callback lands on an SCK queue and `CFRetained` is not `Send`.
    let block = block2::RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let msg = if error.is_null() {
            None
        } else {
            Some(unsafe { &*error }.localizedDescription().to_string())
        };
        let payload = match (std::ptr::NonNull::new(image), msg) {
            (Some(nn), _) => {
                let retained = unsafe { CFRetained::retain(nn) };
                Ok(CFRetained::into_raw(retained).as_ptr() as usize)
            }
            (None, Some(m)) => Err(m),
            (None, None) => Err("ScreenCaptureKit returned no image".to_string()),
        };
        let _ = tx.send(payload);
    });

    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        );
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(ptr)) => {
            let nn = std::ptr::NonNull::new(ptr as *mut CGImage).ok_or(CaptureError::Encode)?;
            Ok(unsafe { CFRetained::from_raw(nn) })
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

// ── encoding ─────────────────────────────────────────────────────────────────

/// Encode a `CGImage` as PNG.
///
/// `NSBitmapImageRep` is used rather than `CGImageDestination` purely because it
/// keeps this crate off a second image-IO binding; the output is the same
/// PNG. PNG rather than JPEG because UI screenshots are large flat-color regions
/// and crisp text, where PNG is both smaller and lossless — JPEG ringing around
/// glyphs is exactly the artifact that makes a vision model misread a label.
fn encode_png(image: &CGImage) -> Result<Vec<u8>> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
    let props = NSDictionary::new();
    let data =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }
            .ok_or(CaptureError::Encode)?;
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_preserves_aspect_ratio_and_respects_the_box() {
        // Landscape, over budget.
        assert_eq!(clamp_dimensions(3000.0, 1500.0, 1500), (1500, 750));
        // Portrait, over budget: the *longest* side is what gets clamped.
        assert_eq!(clamp_dimensions(1000.0, 4000.0, 1000), (250, 1000));
        // Already inside the box: left exactly alone, never upscaled.
        assert_eq!(clamp_dimensions(800.0, 600.0, 1500), (800, 600));
        // Disabled.
        assert_eq!(clamp_dimensions(4000.0, 3000.0, 0), (4000, 3000));
    }

    #[test]
    fn clamp_never_returns_a_zero_dimension() {
        // A sliver window must not encode to a zero-width image, which would
        // make PNG encoding fail rather than produce a useless-but-valid shot.
        let (w, h) = clamp_dimensions(2000.0, 1.0, 100);
        assert!(w >= 1 && h >= 1, "got {w}x{h}");
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
