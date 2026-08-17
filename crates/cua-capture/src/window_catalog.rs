use super::*;

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
