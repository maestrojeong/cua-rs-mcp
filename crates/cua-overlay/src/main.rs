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
    kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID, CGWindowListCopyWindowInfo,
    CGWindowListOption,
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
    /// Conversion from a caller's screen point into this view, from
    /// [`screen_space`]. `(0, 0)` on a single-display machine.
    ///
    /// Held by the view rather than applied at parse time because it changes
    /// underneath a marker that is already on screen: plugging in a display or
    /// changing resolution moves the union, and the arrow has to stay on the
    /// pixel it was pointing at rather than jumping by the difference.
    offset: Cell<(f64, f64)>,
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
    fn new(mtm: MainThreadMarker, frame: NSRect, offset: (f64, f64)) -> Retained<Self> {
        let now = Instant::now();
        let this = Self::alloc(mtm).set_ivars(CursorViewState {
            target: Cell::new(Marker::default()),
            pos: Cell::new((0.0, 0.0)),
            vel: Cell::new((0.0, 0.0)),
            click_started: Cell::new(None),
            idle_since: Cell::new(None),
            last_tick: Cell::new(now),
            painted_visible: Cell::new(false),
            offset: Cell::new(offset),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Adopt a new screen layout. The marker keeps its screen coordinates, so
    /// the arrow stays on the pixel it was pointing at across a display change.
    fn set_offset(&self, offset: (f64, f64)) {
        let ivars = self.ivars();
        if ivars.offset.get() == offset {
            return;
        }
        ivars.offset.set(offset);
        // `pos` is in view space, so it is now stale by the same amount the
        // origin moved. Re-derive it from the target instead of sliding the
        // spring across the screen to catch up.
        let target = ivars.target.get();
        let (x, y) = view_point(target, offset);
        ivars.pos.set((x, y));
        ivars.vel.set((0.0, 0.0));
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
            ivars.pos.set(view_point(m, ivars.offset.get()));
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
        // The spring runs in view space; the target is a screen point.
        let (tx, ty) = view_point(target, ivars.offset.get());
        let dx = tx - px;
        let dy = ty - py;
        let ax = SPRING_STIFFNESS * dx - SPRING_DAMPING * vx;
        let ay = SPRING_STIFFNESS * dy - SPRING_DAMPING * vy;
        vx += ax * dt;
        vy += ay * dt;
        px += vx * dt;
        py += vy * dt;

        let settled = (dx * dx + dy * dy).sqrt() < SETTLE_DISTANCE
            && (vx * vx + vy * vy).sqrt() < SETTLE_SPEED;
        if settled {
            px = tx;
            py = ty;
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

    // Sized to every screen, not just the main one: callers speak global screen
    // points, so a window that only covers `mainScreen` cannot reach an element on
    // a second display. `current_screen_space` is re-read in the loop so plugging
    // a display in, unplugging one, or changing resolution moves the window with
    // it rather than leaving it covering an area that no longer exists.
    let mut space = current_screen_space(mtm, None);
    let frame = space.frame;

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
        // Starts at the ordinary level and is raised per command to match the
        // target's own CGWindow layer — see `overlay_level_for`. Ordering alone
        // cannot cross a level boundary, so a fixed level 0 could never annotate
        // the floating and torn-off-menu windows at layer 3 that cua-capture
        // accepts as targets.
        window.setLevel(0);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }

    let view = CursorView::new(mtm, frame, space.offset);
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
    // Mirrors the window's level so it is only written when it changes; every
    // `setLevel:` reorders the window, so calling it per tick would fight the
    // per-command ordering.
    let mut current_level: isize = 0;

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
                // The level has to be set before the ordering, not after:
                // `setLevel:` reorders within the new band, so raising the level
                // afterwards would discard the placement just established.
                (Some(window_id), Some(pid)) => match target_window_layer(window_id, pid) {
                    Some(layer) => {
                        let level = overlay_level_for(layer);
                        if level != current_level {
                            window.setLevel(level);
                            current_level = level;
                        }
                        window.orderWindow_relativeTo(
                            NSWindowOrderingMode::Above,
                            window_id as isize,
                        );
                        Some((window_id, pid))
                    }
                    // Aimed at a window that is not on screen. Refusing here
                    // rather than pinning and letting the gate below undo it
                    // keeps a doomed command from producing one painted frame.
                    None => None,
                },
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
        // `target_window_layer`.
        if let Some((wid, pid)) = pinned {
            match target_window_layer(wid, pid) {
                Some(layer) => {
                    // A window can be promoted between levels while the arrow is
                    // on it — a panel going floating, a menu tearing off — so the
                    // level is re-checked, not just set once at pin time.
                    let level = overlay_level_for(layer);
                    if level != current_level {
                        window.setLevel(level);
                        current_level = level;
                        window.orderWindow_relativeTo(NSWindowOrderingMode::Above, wid as isize);
                    }
                }
                None => {
                    pinned = None;
                    view.set_target(Marker::default());
                }
            }
        }

        // Displays can be added, removed or resized under a running overlay, and
        // a window sized to a layout that no longer exists covers the wrong area.
        // Cheap enough per tick: a handful of `NSScreen` objects, and the setters
        // only run when the layout actually moved.
        let current = current_screen_space(mtm, Some(space));
        if current != space {
            space = current;
            window.setFrame_display(space.frame, false);
            view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), space.frame.size));
            view.set_offset(space.offset);
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
/// Returns the target's CGWindow layer when it is still on screen and still
/// owned by `pid`, and `None` when it is not — which is also the signal to stop
/// drawing.
///
/// The layer comes back because the overlay has to live in the same level band
/// as whatever it is annotating. `NSWindow` levels are ordered bands, not hints:
/// every window at level 3 sits ahead of every window at level 0, so a level-0
/// overlay ordered "above" a floating or torn-off-menu window at layer 3 still
/// renders behind it. cua-capture accepts targets up to layer 3 precisely
/// because menus live there, so the arrow would silently vanish for exactly the
/// controls that need it most.
fn target_window_layer(wid: u32, pid: libc::pid_t) -> Option<i64> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, NULL_WINDOW_ID)?;
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
        // Right id: this is the one entry worth answering about, so answer from
        // it rather than continuing to look for a duplicate id.
        if number_for(dict, unsafe { kCGWindowOwnerPID }) != Some(pid as i64) {
            return None;
        }
        // A window with no reported layer is still a visible, owned window;
        // treating it as level 0 keeps it drawable rather than discarding a
        // target over a missing optional key.
        return Some(number_for(dict, unsafe { kCGWindowLayer }).unwrap_or(0));
    }
    None
}

/// The window level the overlay should adopt to annotate a target at
/// `target_layer`.
///
/// Clamped to the same 0..=3 band `cua_capture::is_plausible_target` will accept
/// as a target, and deliberately not a passthrough. The on-screen window list
/// contains the Dock at roughly `i32::MIN` and system UI in the thousands, and a
/// mis-read or recycled entry that moved the overlay into one of those bands
/// would put a click-through window above every app on the machine. Refusing to
/// leave the band the targets live in makes that unreachable.
fn overlay_level_for(target_layer: i64) -> isize {
    target_layer.clamp(0, 3) as isize
}

/// A marker's position in view coordinates.
///
/// `Marker` stores what the caller said — a global screen point — because that is
/// the thing that stays true across a display rearrangement. The view's own
/// coordinates do not, so the conversion happens at each point of use rather than
/// being baked in at parse time. See [`screen_space`] for the derivation; on a
/// single display `offset` is `(0, 0)` and this is the identity.
fn view_point(m: Marker, offset: (f64, f64)) -> (f64, f64) {
    (m.x - offset.0, m.y + offset.1)
}

/// Where the overlay window goes, and how to turn a caller's screen point into a
/// point inside its flipped view.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenSpace {
    /// Union of every screen, in AppKit coordinates (bottom-left origin, y up).
    frame: NSRect,
    /// Subtract from a caller's x, add to a caller's y, to land in view
    /// coordinates. See [`screen_space`] for the derivation.
    offset: (f64, f64),
}

/// [`screen_space`] for the screens attached right now.
///
/// Split from the pure function so the arithmetic is testable without AppKit;
/// this half is only the lookup. `mainScreen` is the origin of the caller's
/// coordinate system, so its absence means there is no coordinate system to draw
/// in and the previous geometry is the safest thing to keep.
fn current_screen_space(mtm: MainThreadMarker, previous: Option<ScreenSpace>) -> ScreenSpace {
    let Some(main) = NSScreen::mainScreen(mtm) else {
        return previous.unwrap_or(screen_space(&[], 0.0));
    };
    let frames: Vec<NSRect> = NSScreen::screens(mtm)
        .to_vec()
        .iter()
        .map(|s| s.frame())
        .collect();
    screen_space(&frames, main.frame().size.height)
}

/// Compute the overlay's geometry from the screens' AppKit frames.
///
/// A single fixed window sized to `mainScreen` could only ever cover one
/// display, and callers speak *global* screen points — so an element on a second
/// monitor was handed to a window that does not reach it, and the arrow was
/// clipped away or drawn somewhere meaningless. The window therefore spans the
/// union of all screens.
///
/// The offset exists because two coordinate systems disagree about the origin.
/// Callers use CoreGraphics screen points: top-left origin at the *main*
/// display's top-left, y increasing downward — the space `get_app_state` reports
/// frames in. AppKit places windows bottom-left-origin from the main display's
/// bottom-left, y up. With the view flipped, a point lands at
///
/// ```text
/// view_x = x - union.origin.x
/// view_y = (union.origin.y + union.height - main_height) + y
/// ```
///
/// because the view's y runs down from the union's *top* edge, whose AppKit
/// height above the main display's bottom is `union.origin.y + union.height`,
/// while the caller's y runs down from the main display's top at `main_height`.
///
/// Single-display machines get `offset == (0, 0)` and the previous behaviour
/// exactly: the union is the main screen, so both terms cancel. That equivalence
/// is the point of writing this as a pure function — it is the part a machine
/// with one monitor can still prove.
fn screen_space(screens: &[NSRect], main_height: f64) -> ScreenSpace {
    let frame = screens
        .iter()
        .copied()
        .reduce(|a, b| {
            let min_x = a.origin.x.min(b.origin.x);
            let min_y = a.origin.y.min(b.origin.y);
            let max_x = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
            let max_y = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
            NSRect::new(
                NSPoint::new(min_x, min_y),
                NSSize::new(max_x - min_x, max_y - min_y),
            )
        })
        // No screens at all is not a state this can draw in, but it must not
        // panic on the way through a display being unplugged.
        .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)));
    ScreenSpace {
        frame,
        offset: (
            frame.origin.x,
            frame.origin.y + frame.size.height - main_height,
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    /// The regression that matters most on a one-display machine: whatever the
    /// multi-screen arithmetic does, it must be the identity here, because that
    /// is the configuration the overlay was measured working in.
    #[test]
    fn a_single_display_is_unchanged_by_the_union_math() {
        let main = rect(0.0, 0.0, 1512.0, 982.0);
        let space = screen_space(&[main], 982.0);
        assert_eq!(space.frame, main);
        assert_eq!(space.offset, (0.0, 0.0));
        let m = Marker {
            x: 700.0,
            y: 400.0,
            visible: true,
            clicking: false,
        };
        assert_eq!(view_point(m, space.offset), (700.0, 400.0));
    }

    /// A display to the left gives the union a negative origin, which is the case
    /// DESIGN called out as untested. The main display's own top-left must still
    /// map to the main display, not to the union's corner.
    #[test]
    fn a_display_to_the_left_shifts_without_moving_the_main_screens_points() {
        let main = rect(0.0, 0.0, 1512.0, 982.0);
        let left = rect(-1920.0, 0.0, 1920.0, 1080.0);
        let space = screen_space(&[main, left], 982.0);
        assert_eq!(space.frame, rect(-1920.0, 0.0, 3432.0, 1080.0));
        // x shifts by the union's left edge; y by how far the union's top rises
        // above the main display's top.
        assert_eq!(space.offset, (-1920.0, 98.0));

        let origin = Marker {
            x: 0.0,
            y: 0.0,
            visible: true,
            clicking: false,
        };
        // The main display starts 1920 points right of the union's left edge, and
        // 98 points below its top.
        assert_eq!(view_point(origin, space.offset), (1920.0, 98.0));
    }

    /// Order must not matter: the union is a fold, and a screen list arrives in
    /// whatever order AppKit reports it.
    #[test]
    fn the_union_does_not_depend_on_screen_order() {
        let a = rect(0.0, 0.0, 1512.0, 982.0);
        let b = rect(-1920.0, -200.0, 1920.0, 1080.0);
        assert_eq!(screen_space(&[a, b], 982.0), screen_space(&[b, a], 982.0));
    }

    /// Unplugging every display must not panic on the way through.
    #[test]
    fn no_screens_is_an_empty_frame_rather_than_a_panic() {
        let space = screen_space(&[], 0.0);
        assert_eq!(space.frame.size.width, 0.0);
        assert_eq!(space.offset, (0.0, 0.0));
    }

    /// The overlay follows its target into the floating band, and refuses to
    /// follow anything into a system band. The on-screen window list contains the
    /// Dock near `i32::MIN` and system UI in the thousands; a click-through window
    /// at those levels would sit above every app on the machine.
    #[test]
    fn the_overlay_level_tracks_the_target_but_stays_inside_the_target_band() {
        assert_eq!(overlay_level_for(0), 0, "ordinary window");
        assert_eq!(overlay_level_for(3), 3, "floating / torn-off menu");
        assert_eq!(overlay_level_for(2997), 3, "system UI is clamped down");
        assert_eq!(overlay_level_for(-2147483622), 0, "the Dock is clamped up");
    }
}
