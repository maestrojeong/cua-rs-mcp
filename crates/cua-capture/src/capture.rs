use super::*;

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
