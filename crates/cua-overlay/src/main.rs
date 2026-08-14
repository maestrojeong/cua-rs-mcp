//! The agent's drawn cursor.
//!
//! A transparent, click-through window covering the screen, with an arrow
//! drawn wherever the agent is acting. It delivers no input of any kind: the
//! window ignores mouse events entirely, so a click aimed at what is underneath
//! it passes straight through. Its only job is to answer "where is the agent
//! working right now" for the human watching.
//!
//! # Why this is a separate process
//!
//! `cua-rs` is a short-lived stdio MCP server: one process per turn, no run
//! loop, no window. That is what lets it be spawned by a host that knows
//! nothing about AppKit. A drawn cursor needs the opposite — an `NSWindow`, a
//! run loop, and a lifetime longer than one tool call — so it lives here
//! instead of being bolted onto the server. Same split a shipping
//! implementation uses: a long-lived process that draws, a thin caller that
//! tells it where.
//!
//! # Protocol
//!
//! Line-oriented on stdin, so a caller needs no library and a human can drive
//! it by hand:
//!
//! ```text
//! move <x> <y>    put the arrow at a screen point
//! click <x> <y>   put it there and flash a click marker
//! hide            keep running, draw nothing
//! quit            exit
//! ```
//!
//! Coordinates are screen points with a top-left origin — the same space
//! `get_app_state` reports element frames in, so a caller can pass a frame
//! centre straight through without converting.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DeclaredClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath, NSColor,
    NSScreen, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// What the arrow is doing, in screen points with a top-left origin.
#[derive(Clone, Copy, Default)]
struct Marker {
    x: f64,
    y: f64,
    /// Drawn only when true; `hide` clears it.
    visible: bool,
    /// Draw the click ring as well as the arrow.
    clicking: bool,
}

struct CursorViewState {
    marker: Cell<Marker>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CursorViewState]
    struct CursorView;

    impl CursorView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let m = self.ivars().marker.get();
            if !m.visible {
                return;
            }
            // The view is flipped so callers can speak screen coordinates.
            let (x, y) = (m.x, m.y);

            if m.clicking {
                // A small, quiet ring at the click point: a faint glow plus a
                // crisp thin outline, not a big flat disc — the old radius-18
                // filled circle read as an alert, not a cursor. Drawn first so
                // the arrow sits on top of it rather than being swallowed by
                // it.
                let r = 8.0;
                let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                    NSPoint::new(x - r, y - r),
                    NSSize::new(r * 2.0, r * 2.0),
                ));
                NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.38, 0.95, 0.18).setFill();
                ring.fill();
                ring.setLineWidth(1.2);
                NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.38, 0.95, 0.75).setStroke();
                ring.stroke();
            }

            // The "presence cursor" silhouette used by Figma, Notion and
            // similar multiplayer tools for showing where someone else is
            // pointing — ported from Lucide's `mouse-pointer-2` icon
            // (MIT-licensed, https://lucide.dev), corner rounding dropped
            // since it disappears at this size anyway, then mirrored so it
            // leans up-right instead of up-left. Nothing forces that
            // particular handedness; picking the one the real pointer never
            // uses means a glance is enough to tell this arrow apart from
            // yours, which a human never needs from their own cursor but an
            // agent's drawn one benefits from. Still a filled path rather
            // than an image, so there is no asset to ship or scale.
            let path = NSBezierPath::bezierPath();
            let pts = [
                (0.0, 0.0),
                (-0.488, -0.488),
                (-12.488, 4.387),
                (-12.441, 5.097),
                (-7.848, 6.282),
                (-6.770, 7.358),
                (-5.585, 11.953),
                (-4.875, 12.000),
            ];
            path.moveToPoint(NSPoint::new(x + pts[0].0, y + pts[0].1));
            for (dx, dy) in &pts[1..] {
                path.lineToPoint(NSPoint::new(x + dx, y + dy));
            }
            path.closePath();

            // A thin white outline instead of black keeps the silhouette
            // crisp over both light and dark content; the fill is a deep
            // indigo rather than a stock blue, for a quieter, less
            // "system alert" feel.
            NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.9).setStroke();
            path.setLineWidth(1.2);
            path.stroke();
            NSColor::colorWithSRGBRed_green_blue_alpha(0.25, 0.32, 0.88, 1.0).setFill();
            path.fill();
        }

        /// Screen coordinates run downward; so should the view's.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl CursorView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CursorViewState {
            marker: Cell::new(Marker::default()),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_marker(&self, m: Marker) {
        self.ivars().marker.set(m);
        self.setNeedsDisplay(true);
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("the overlay must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // `Accessory`: a real app identity and a window, with no Dock icon and no
    // menu bar, and — importantly — it never takes activation from whatever the
    // human is using.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let screen = NSScreen::mainScreen(mtm).expect("no main screen");
    let frame = screen.frame();

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    {
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        // The whole point: input passes through to whatever is underneath, so
        // this window can never intercept a click meant for an app.
        window.setIgnoresMouseEvents(true);
        // Above ordinary windows, and present on every Space so the arrow does
        // not vanish when the user switches desktops.
        // Just under the screen-saver level: above every ordinary window,
        // below the things the system reserves for itself.
        window.setLevel(objc2_app_kit::NSScreenSaverWindowLevel - 1);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }

    let view = CursorView::new(mtm, frame);
    window.setContentView(Some(&view));
    // `orderFrontRegardless`, not `makeKeyAndOrderFront`: showing the arrow
    // must not steal key focus from the app the agent is driving.
    window.orderFrontRegardless();

    eprintln!("cua-overlay ready on {:.0}x{:.0}", frame.size.width, frame.size.height);

    // Commands arrive on a reader thread; drawing has to happen on the main
    // thread, so the reader hands work back through the run loop.
    let (tx, rx) = std::sync::mpsc::channel::<Marker>();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                // stdin closed: the caller went away, so should we.
                std::process::exit(0);
            }
            let mut it = line.split_whitespace();
            let m = match it.next() {
                Some("move") | Some("click") => {
                    let clicking = line.starts_with("click");
                    let x: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let y: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    Marker { x, y, visible: true, clicking }
                }
                Some("hide") => Marker::default(),
                Some("quit") => std::process::exit(0),
                _ => continue,
            };
            if tx.send(m).is_err() {
                return;
            }
        }
    });

    // Pump the run loop in short slices and apply whatever the reader queued.
    // Simpler than a custom run-loop source, and 20 ms is far below the
    // threshold where a moving arrow looks stepped.
    loop {
        while let Ok(m) = rx.try_recv() {
            view.set_marker(m);
        }
        unsafe {
            objc2_core_foundation::CFRunLoop::run_in_mode(
                objc2_core_foundation::kCFRunLoopDefaultMode,
                0.02,
                true,
            );
        }
    }
}
