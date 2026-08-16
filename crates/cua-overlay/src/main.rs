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
//! move <x> <y> <window-id> <pid>    put the arrow above that target window
//! click <x> <y> <window-id> <pid>   put it there and flash a click marker
//! hide                              keep running, draw nothing
//! quit                              exit
//! ```
//!
//! Coordinates are screen points with a top-left origin — the same space
//! `get_app_state` reports element frames in, so a caller can pass a frame
//! centre straight through without converting.
//!
//! `pid` is not optional, and this documentation used to say it was — it listed
//! three arguments while the parser accepted a fourth. A line without it parsed
//! fine and produced a visible arrow with nothing to check it against, which
//! disarmed the visibility gate for exactly the hand-driven case the protocol is
//! shaped for. Every field is now required, and a malformed line is dropped
//! whole rather than partly applied.

use std::cell::Cell;
use std::f64::consts::PI;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DeclaredClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath, NSColor,
    NSScreen, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString};
use objc2_core_graphics::{
    kCGWindowNumber, kCGWindowOwnerPID, CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// `CGWindowListCopyWindowInfo`'s `kCGNullWindowID` — "not relative to any
/// window". Named rather than inlined because a bare `0` in that argument reads
/// like a window id, which is the one thing it is not.
const NULL_WINDOW_ID: u32 = 0;

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
    /// The pid that owned `window_id` when the command was issued. Carried
    /// alongside the id rather than looked up from it because it is what makes a
    /// *recycled* id detectable: the visibility check compares both against the
    /// live window list, and an id that has been handed to another process fails
    /// the pid half.
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
    /// Whether the last actual paint put an arrow on screen.
    ///
    /// The difference between this and `target.visible` is the difference
    /// between what the screen shows and what it is supposed to show, and it
    /// exists because hiding is not self-executing. `advance` only reports
    /// "something left to animate", and an invisible marker has nothing to
    /// animate — so it returned `false`, the run loop never marked the view
    /// dirty, `drawRect:` was never called, and the arrow already on screen
    /// stayed there. Every hide path was affected: the visibility gate, an
    /// explicit `hide`, and a command with no target window. Recording what was
    /// really drawn lets `advance` keep asking for a repaint until an erase has
    /// actually happened — `setNeedsDisplay:` is coalesced and deferred, so the
    /// paint is not guaranteed inside the same run-loop slice that requested it,
    /// and this converges instead of assuming.
    painted_visible: Cell<bool>,
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
            // Set before the early return, so "this paint deliberately drew
            // nothing" is recorded as such. That is what lets `advance` stop
            // asking for repaints once the erase has actually happened.
            ivars.painted_visible.set(target.visible);
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
            painted_visible: Cell::new(false),
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
            // Nothing to animate, but "nothing to animate" is not the same as
            // "nothing to do": an arrow painted before the marker was hidden is
            // still on screen until a repaint erases it. Report work exactly
            // while the screen disagrees with the target, so the erase happens
            // once and an idle overlay still costs no redraws.
            return ivars.painted_visible.get();
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
                Some(verb @ ("move" | "click")) => {
                    // Every field is required and every field is validated, and
                    // a bad line is dropped whole rather than half-applied.
                    // Defaulting x/y to 0 used to turn a typo into a confident
                    // arrow in the corner of the screen, and a non-finite
                    // coordinate would poison the spring permanently: NaN never
                    // satisfies the settle test, so the view would redraw forever
                    // and hand non-finite points to `NSBezierPath`.
                    let Some(x) = it
                        .next()
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|v| v.is_finite())
                    else {
                        continue;
                    };
                    let Some(y) = it
                        .next()
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|v| v.is_finite())
                    else {
                        continue;
                    };
                    // 0 is not a usable window number: `orderWindow:relativeTo:0`
                    // is AppKit's documented "put me in front of everything at my
                    // level", which is the one placement this process must never
                    // ask for.
                    let Some(window_id) = it
                        .next()
                        .and_then(|v| v.parse::<u32>().ok())
                        .filter(|id| *id != 0)
                    else {
                        continue;
                    };
                    // Required, not optional. The visibility gate needs the pid to
                    // detect a recycled window id, and accepting a pid-less line
                    // silently disarmed it.
                    let Some(pid) = it
                        .next()
                        .and_then(|v| v.parse::<libc::pid_t>().ok())
                        .filter(|p| *p > 0)
                    else {
                        continue;
                    };
                    OverlayCommand {
                        marker: Marker {
                            x,
                            y,
                            visible: true,
                            clicking: verb == "click",
                        },
                        window_id: Some(window_id),
                        pid: Some(pid),
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
    // The window+pid the marker currently claims to point at, so the visibility
    // check below has something to verify against. `None` whenever nothing is
    // showing, so a hidden marker can't be "hidden again" every tick. Both
    // halves are needed: the id is the thing that can disappear, and the pid is
    // what makes a recycled id detectable.
    let mut pinned: Option<(u32, libc::pid_t)> = None;

    // Pump the run loop in short slices, apply whatever the reader queued,
    // then step the spring/idle animation and repaint if it changed
    // anything. 20 ms is far below the threshold where either the chase or
    // the breathing motion would look stepped.
    loop {
        while let Ok(command) = rx.try_recv() {
            // Both halves or neither. A marker with an id but no pid could not be
            // checked for a recycled id, and one with a pid but no id could not
            // be checked for the window closing — either way the gate below would
            // be inert, which is how the arrow used to get stranded.
            pinned = match (command.window_id, command.pid) {
                (Some(window_id), Some(pid)) => {
                    window.orderWindow_relativeTo(NSWindowOrderingMode::Above, window_id as isize);
                    Some((window_id, pid))
                }
                _ => None,
            };
            view.set_target(if pinned.is_some() {
                command.marker
            } else {
                Marker::default()
            });
        }

        // Window ordering is the first line of defence: `orderWindow:relativeTo:`
        // puts the overlay just above the target's stacking position, so a real
        // foreground app in the same Space already buries it. What ordering
        // cannot express is the target ceasing to be on screen at all — the
        // overlay joins every Space, so a Space switch leaves it ordered against
        // a window that is not there, and a closed or minimized window leaves it
        // ordered against nothing. This is the check for that, and it asks about
        // the *window*, not about who has the keyboard: see
        // `target_window_visible`.
        if let Some((wid, pid)) = pinned {
            if !target_window_visible(wid, pid) {
                pinned = None;
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

/// Whether the window the arrow is pinned to is still on screen and still owned
/// by the pid that was pinned with it.
///
/// This is the visibility gate, and what it deliberately does *not* ask is
/// whether the target is frontmost. Requiring that was the 0.4.2 mistake: cua-rs
/// exists to drive windows the human is not looking at (DESIGN §9 — no focus
/// steal, ever), so the pinned pid is essentially never `NSWorkspace`'s
/// frontmost app, and gating on it suppressed the marker on the same iteration
/// that set it. Not a flicker — the arrow never reached a paint at all, which is
/// why the feature looked broken rather than merely twitchy.
///
/// `kCGWindowListOptionOnScreenOnly` answers the question that actually matters:
/// it lists windows currently composited on the active Space, in front-to-back
/// order, regardless of which app owns the keyboard. Measured on this machine
/// while Terminal was frontmost: a background app's ordinary layer-0 window was
/// present, so a background target still draws; a KakaoTalk window that had been
/// closed was absent from the on-screen list while its pid lived on, so a closed,
/// minimized, or off-Space target stops drawing. That second half also covers the
/// window-lifecycle hole the pid check could never see — the pinned pid can stay
/// frontmost while the pinned *window* goes away.
///
/// Matching the owner pid as well as the id is what makes a recycled CGWindowID
/// safe: ids are reused, and pointing at a stranger's window because it inherited
/// a number is exactly the outcome this gate exists to prevent.
///
/// Deliberately fails closed. Any missing key, unexpected type, or a null list
/// means "cannot prove it is there", and a hidden arrow costs one frame while a
/// wrongly shown one is drawn over someone's work.
fn target_window_visible(wid: u32, pid: libc::pid_t) -> bool {
    let Some(list) =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, NULL_WINDOW_ID)
    else {
        return false;
    };
    let count = CFArray::count(&list);
    for i in 0..count {
        // The array is documented to hold CFDictionary descriptions; anything
        // else is a contract break, and skipping is the fail-closed reading.
        let raw = unsafe { CFArray::value_at_index(&list, i) };
        if raw.is_null() {
            continue;
        }
        let dict: &CFDictionary = unsafe { &*raw.cast() };
        if number_for(dict, unsafe { kCGWindowNumber }) != Some(i64::from(wid)) {
            continue;
        }
        // Right id: this is the one entry worth answering about, so report what
        // the owner check says rather than continuing to look for a duplicate.
        return number_for(dict, unsafe { kCGWindowOwnerPID }) == Some(pid as i64);
    }
    false
}

/// Read one integer out of a `CGWindowListCopyWindowInfo` description.
///
/// `None` for absent or non-numeric, which every caller treats as "unprovable"
/// rather than as a value.
fn number_for(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    let value = unsafe {
        CFDictionary::value(
            dict,
            (key as *const CFString).cast::<std::ffi::c_void>().cast(),
        )
    };
    if value.is_null() {
        return None;
    }
    let number: &CFNumber = unsafe { &*value.cast() };
    number.as_i64()
}
