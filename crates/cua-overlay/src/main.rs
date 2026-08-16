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
//! # Motion
//!
//! The arrow never teleports between commands. Each `move`/`click` sets a
//! *target*; a critically-ish-damped spring chases it every frame, with the
//! constants tuned here by eye. Left alone at rest for a moment it also
//! breathes — a slow, faint vertical bob plus a barely-there halo — which says
//! "the agent is still here, just not moving" so a stationary arrow does not
//! read as a stuck or dead process.
//!
//! # Protocol
//!
//! Line-oriented on stdin, so a caller needs no library and a human can drive
//! it by hand:
//!
//! ```text
//! move <x> <y> <window-id>    put the arrow above that target window
//! click <x> <y> <window-id>   put it there and flash a click marker
//! hide                        keep running, draw nothing
//! quit                        exit
//! ```
//!
//! Coordinates are screen points with a top-left origin — the same space
//! `get_app_state` reports element frames in, so a caller can pass a frame
//! centre straight through without converting.

use std::cell::Cell;
use std::f64::consts::PI;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DeclaredClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath, NSColor,
    NSScreen, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowOrderingMode,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Spring stiffness (`k`, in px/s² per px of displacement) and damping (`c`,
/// in 1/s) driving the chase toward the target. Slightly under critical
/// damping (critical for these values is `2*sqrt(k)` ≈ 32) on purpose: a
/// small, quick overshoot reads as physical motion rather than a slide, the
/// same reason the shipping overlay this is calibrated against bothers with
/// spring parameters at all instead of a linear tween.
const SPRING_STIFFNESS: f64 = 260.0;
const SPRING_DAMPING: f64 = 27.0;
/// Below this distance *and* speed, snap exactly onto the target and stop
/// integrating — otherwise floating point noise keeps the spring "settling"
/// forever, which would keep the idle-breathing clock from ever starting.
const SETTLE_DISTANCE: f64 = 0.4;
const SETTLE_SPEED: f64 = 4.0;
/// How long the arrow must sit still before it starts breathing.
const IDLE_DELAY: f64 = 0.5;
/// Seconds per breath cycle, and how far it bobs.
const BREATH_PERIOD: f64 = 2.4;
const BREATH_AMPLITUDE: f64 = 1.6;
/// How long the click ring takes to fade out after a click command.
const CLICK_FADE: f64 = 0.22;

/// What the arrow is doing, in screen points with a top-left origin. This is
/// the *target* state a command sets; `CursorViewState` tracks where the
/// arrow actually is separately, since those two are no longer the same
/// point once motion is animated.
#[derive(Clone, Copy, Default)]
struct Marker {
    x: f64,
    y: f64,
    /// Drawn only when true; `hide` clears it.
    visible: bool,
    /// Draw the click ring as well as the arrow.
    clicking: bool,
}

#[derive(Clone, Copy, Default)]
struct OverlayCommand {
    marker: Marker,
    window_id: Option<u32>,
    /// The pid the marker is currently pointing at. Kept alongside
    /// `window_id` (rather than looked up from it) so the frontmost-mismatch
    /// check in the main loop costs no extra window-list query.
    pid: Option<libc::pid_t>,
}

struct CursorViewState {
    /// Where the next command wants the arrow.
    target: Cell<Marker>,
    /// Where it actually is right now, mid-spring.
    pos: Cell<(f64, f64)>,
    vel: Cell<(f64, f64)>,
    /// When the arrow last started a click flash; `None` once it has fully
    /// faded, so a stale click can't relight itself.
    click_started: Cell<Option<Instant>>,
    /// When the arrow came to rest, for the breathing clock. `None` while
    /// still moving.
    idle_since: Cell<Option<Instant>>,
    last_tick: Cell<Instant>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CursorViewState]
    struct CursorView;

    impl CursorView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let ivars = self.ivars();
            let target = ivars.target.get();
            if !target.visible {
                return;
            }
            // The view is flipped so callers can speak screen coordinates.
            let (x, mut y) = ivars.pos.get();

            // A slow, faint bob once the arrow has been at rest a moment —
            // "the agent is still here" rather than "the agent just did
            // something", which the spring alone can't say since it goes
            // silent the instant it settles.
            if let Some(since) = ivars.idle_since.get() {
                let idle_for = since.elapsed().as_secs_f64();
                if idle_for > IDLE_DELAY {
                    let phase = (idle_for - IDLE_DELAY) * (2.0 * PI / BREATH_PERIOD);
                    y += phase.sin() * BREATH_AMPLITUDE;

                    let r = 11.0;
                    let glow_alpha = 0.05 + 0.05 * (0.5 + 0.5 * phase.sin());
                    let halo = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                        NSPoint::new(x - r, y - r),
                        NSSize::new(r * 2.0, r * 2.0),
                    ));
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.38, 0.95, glow_alpha)
                        .setFill();
                    halo.fill();
                }
            }

            if let Some(started) = ivars.click_started.get() {
                let elapsed = started.elapsed().as_secs_f64();
                let alpha = (1.0 - elapsed / CLICK_FADE).max(0.0);
                if alpha > 0.0 {
                    // A small, quiet ring at the click point that expands
                    // slightly as it fades — a quick ripple rather than a
                    // static disc, so a repeated click at the same point
                    // still reads as a new event. Drawn first so the arrow
                    // sits on top of it rather than being swallowed by it.
                    let r = 8.0 + (1.0 - alpha) * 5.0;
                    let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                        NSPoint::new(x - r, y - r),
                        NSSize::new(r * 2.0, r * 2.0),
                    ));
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.38, 0.95, 0.18 * alpha)
                        .setFill();
                    ring.fill();
                    ring.setLineWidth(1.2);
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.38, 0.95, 0.75 * alpha)
                        .setStroke();
                    ring.stroke();
                }
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
        let now = Instant::now();
        let this = Self::alloc(mtm).set_ivars(CursorViewState {
            target: Cell::new(Marker::default()),
            pos: Cell::new((0.0, 0.0)),
            vel: Cell::new((0.0, 0.0)),
            click_started: Cell::new(None),
            idle_since: Cell::new(None),
            last_tick: Cell::new(now),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Record a new command. Motion toward it happens later, in `advance`;
    /// this only decides what changed *as of this command* — a fresh click
    /// flash, or an instant (non-animated) appearance the first time the
    /// arrow becomes visible.
    fn set_target(&self, m: Marker) {
        let ivars = self.ivars();
        let prev = ivars.target.get();

        if m.visible && !prev.visible {
            // Reappearing after being hidden: snap straight there rather than
            // sliding in from wherever it last was (or the origin, for the
            // very first command), which would read as the arrow crossing
            // the screen for no reason.
            ivars.pos.set((m.x, m.y));
            ivars.vel.set((0.0, 0.0));
            ivars.idle_since.set(Some(Instant::now()));
        }
        if m.clicking {
            // Every click command restarts the flash, even a repeated one at
            // the same point — a double-click should ripple twice.
            ivars.click_started.set(Some(Instant::now()));
        }
        ivars.target.set(m);
    }

    /// Step the spring and the idle clock by however much time actually
    /// passed. Returns whether the view has anything left to animate, so the
    /// caller only pays for a redraw while something is visible.
    fn advance(&self) -> bool {
        let ivars = self.ivars();
        let now = Instant::now();
        // Clamped so a paused/backgrounded process resuming doesn't feed the
        // spring a huge `dt` and fling the arrow across the screen.
        let dt = (now - ivars.last_tick.get()).as_secs_f64().min(0.05);
        ivars.last_tick.set(now);

        let target = ivars.target.get();
        if !target.visible {
            return false;
        }

        let (mut px, mut py) = ivars.pos.get();
        let (mut vx, mut vy) = ivars.vel.get();
        let dx = target.x - px;
        let dy = target.y - py;
        let ax = SPRING_STIFFNESS * dx - SPRING_DAMPING * vx;
        let ay = SPRING_STIFFNESS * dy - SPRING_DAMPING * vy;
        vx += ax * dt;
        vy += ay * dt;
        px += vx * dt;
        py += vy * dt;

        let settled = (dx * dx + dy * dy).sqrt() < SETTLE_DISTANCE
            && (vx * vx + vy * vy).sqrt() < SETTLE_SPEED;
        if settled {
            px = target.x;
            py = target.y;
            vx = 0.0;
            vy = 0.0;
            if ivars.idle_since.get().is_none() {
                ivars.idle_since.set(Some(now));
            }
        } else {
            ivars.idle_since.set(None);
        }

        ivars.pos.set((px, py));
        ivars.vel.set((vx, vy));
        true
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
        // Keep the overlay at the ordinary window level. Each command orders
        // it immediately above the exact target CGWindowID, so unrelated
        // foreground windows naturally occlude both the target and its cursor.
        window.setLevel(0);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }

    let view = CursorView::new(mtm, frame);
    window.setContentView(Some(&view));
    eprintln!(
        "cua-overlay ready on {:.0}x{:.0}",
        frame.size.width, frame.size.height
    );

    // Commands arrive on a reader thread; drawing has to happen on the main
    // thread, so the reader hands work back through the run loop.
    let (tx, rx) = std::sync::mpsc::channel::<OverlayCommand>();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                // stdin closed: the caller went away, so should we.
                std::process::exit(0);
            }
            let mut it = line.split_whitespace();
            let command = match it.next() {
                Some("move") | Some("click") => {
                    let clicking = line.starts_with("click");
                    let x: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let y: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    let window_id = it.next().and_then(|v| v.parse().ok());
                    let pid = it.next().and_then(|v| v.parse().ok());
                    OverlayCommand {
                        marker: Marker {
                            x,
                            y,
                            visible: window_id.is_some(),
                            clicking,
                        },
                        window_id,
                        pid,
                    }
                }
                Some("hide") => OverlayCommand::default(),
                Some("quit") => std::process::exit(0),
                _ => continue,
            };
            if tx.send(command).is_err() {
                return;
            }
        }
    });

    // The pid the marker currently claims to point at, so the frontmost check
    // below has something to compare against. `None` whenever nothing is
    // showing, so a hidden marker can't be "hidden again" every tick.
    let mut pinned_pid: Option<libc::pid_t> = None;

    // Pump the run loop in short slices, apply whatever the reader queued,
    // then step the spring/idle animation and repaint if it changed
    // anything. 20 ms is far below the threshold where either the chase or
    // the breathing motion would look stepped.
    loop {
        while let Ok(command) = rx.try_recv() {
            if let Some(window_id) = command.window_id {
                window.orderWindow_relativeTo(NSWindowOrderingMode::Above, window_id as isize);
            }
            pinned_pid = command.pid;
            view.set_target(command.marker);
        }

        // Window ordering alone is what is supposed to keep the arrow from
        // showing above whatever the human just switched to — `orderWindow:
        // relativeTo:` puts it just above the target's current stacking
        // position, so a real foreground app should already bury it. This is
        // the belt to that suspenders: if the pinned pid is no longer the
        // frontmost app at all (a Space switch, a full-screen app, or any
        // other case ordering alone doesn't cover), hide outright rather than
        // trust that ordering caught it. A false positive here costs one
        // hidden arrow that reappears on the next command; a false negative
        // is an arrow floating over someone else's work.
        if let Some(pid) = pinned_pid {
            if frontmost_pid() != Some(pid) {
                pinned_pid = None;
                view.set_target(Marker::default());
            }
        }

        if view.advance() {
            view.setNeedsDisplay(true);
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

/// The pid of whatever app is frontmost right now, per `NSWorkspace` — the
/// same source of truth a click's activation notice targets, and distinct
/// from what an individual app's own `AXFrontmost` believes (see
/// `window_focus_assist` in `cua-core` for why that distinction matters
/// there too).
fn frontmost_pid() -> Option<libc::pid_t> {
    Some(
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()?
            .processIdentifier(),
    )
}
