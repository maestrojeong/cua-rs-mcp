//! Display geometry, read through the public CoreGraphics display API.
//!
//! Lives here rather than in `cua-core` because this crate already owns the
//! window server's view of the screen — window frames, levels, pixels — and a
//! display's bounds are the same coordinate space those frames are in. Keeping
//! `CGDirectDisplay` in one crate keeps `cua-core`'s CoreGraphics surface at the
//! single `CGSession` read its manifest promises.
//!
//! No grant is needed. Display bounds are not screen contents, and they are
//! what any app can read to place its own windows.

use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayPixelsWide, CGGetActiveDisplayList,
    CGMainDisplayID,
};

use super::{CaptureError, Result};

/// One active display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    /// `CGDirectDisplayID`. Stable while the display stays connected; a
    /// re-plugged monitor may come back with a different one.
    pub id: CGDirectDisplayID,
    /// Bounds in global points, top-left origin — the same space AX reports
    /// window positions in, so a window frame can be compared to it directly.
    pub frame: CGRect,
    /// Whether this is the main display, the one whose origin is global `0,0`.
    pub main: bool,
    /// Pixels per point in the display's current mode. `2.0` on Retina.
    pub scale: f64,
}

/// Upper bound on displays asked for. macOS supports far fewer than this.
const MAX_DISPLAYS: usize = 32;

/// Every active display, main display first.
///
/// "Active" rather than "online": a mirrored or sleeping display is online but
/// draws nothing, and a window placed only on it would be invisible — which is
/// exactly what the window tools use this list to refuse.
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // SAFETY: `ids` holds MAX_DISPLAYS entries, which is the capacity passed,
    // and `count` is a live u32.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        return Err(CaptureError::Failed(format!(
            "CGGetActiveDisplayList failed with CGError {}",
            err.0
        )));
    }
    let main = CGMainDisplayID();
    let mut out: Vec<DisplayInfo> = ids
        .iter()
        .take((count as usize).min(MAX_DISPLAYS))
        .map(|&id| {
            let frame = CGDisplayBounds(id);
            DisplayInfo {
                id,
                frame,
                main: id == main,
                scale: pixels_per_point(CGDisplayPixelsWide(id) as f64, frame.size.width),
            }
        })
        .collect();
    out.sort_by_key(|d| (!d.main, d.id));
    Ok(out)
}

/// Backing scale from a pixel width and a point width, `1.0` when the point
/// width is degenerate rather than a division by zero.
fn pixels_per_point(pixels: f64, points: f64) -> f64 {
    if points > 0.0 && pixels > 0.0 {
        pixels / points
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_reads_as_two_pixels_per_point() {
        assert_eq!(pixels_per_point(3024.0, 1512.0), 2.0);
    }

    #[test]
    fn a_degenerate_display_does_not_divide_by_zero() {
        assert_eq!(pixels_per_point(0.0, 0.0), 1.0);
        assert_eq!(pixels_per_point(1920.0, 0.0), 1.0);
    }
}
