# Changelog

Versions are `0.MINOR.PATCH`. While the crate is pre-1.0, a change an existing
caller can *notice* takes the minor slot, even when it is a bug fix — the tool
descriptions are the API here, and an agent that learned the old behaviour is a
caller.

## 0.5.1

### The drawn cursor was not drawing at all

0.4.2 fixed "the arrow stays visible over another app after a Space switch" by
polling `NSWorkspace.frontmostApplication()` and hiding whenever the pinned pid
was not frontmost. That gate is unsatisfiable here. cua-rs exists to drive
windows the human is *not* looking at and never steals focus (DESIGN §9), so the
pinned pid is essentially never the frontmost app — and the check runs in the
same loop iteration that applies the command, before `advance()` and before any
paint. The arrow was suppressed before its first frame: not a flicker, zero
frames, on every action against a background window. The feature has been inert
since 0.4.2.

The gate now asks about the **window** instead of about who holds the keyboard.
Each tick it looks the pinned CGWindowID up in
`CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly)` and keeps drawing
only while that id is present *and* still owned by the pinned pid. Measured on
one machine with Terminal frontmost throughout: a background app's ordinary
layer-0 window was in the on-screen list, so a background target draws; a
KakaoTalk window that had been closed was absent while its pid lived on, so a
closed, minimized, or off-Space target stops drawing. Matching the owner pid too
is what keeps a recycled window id from pointing the arrow at a stranger.

That also closes a hole the pid check could never see: the pinned pid could stay
frontmost while the pinned *window* was closed or moved to another Space, and
nothing was watching the window itself.

### Hiding did not erase

Independently, every hide path left the arrow on screen. `advance()` returned
`false` whenever the marker was invisible — correct as "nothing to animate", but
the main loop uses that return value as "call `setNeedsDisplay:`", so the view was
never invalidated, `drawRect:` was never called, and the last painted arrow simply
stayed. The explicit `hide` command, a command with no target window, and the
visibility gate were all affected; the gate had been firing correctly all along
and had no way to reach the screen.

The view now records what its last paint actually rendered and keeps requesting a
repaint until an erase has really happened, rather than assuming that
`setNeedsDisplay:` — which AppKit coalesces and defers — took effect inside the
run-loop slice that asked for it. An idle overlay still costs no redraws.

Verified by pixels, driving the installed binary by hand and capturing its own
window: a background target painted the arrow, `hide` produced a blank frame, and
a nonexistent window id produced a frame byte-identical to the blank one.

### The stdin protocol required a field it documented as absent

The module documentation listed `move <x> <y> <window-id>` while the parser read
an optional fourth `pid`. A hand-typed three-argument line parsed happily into a
*visible* marker with no pid — which silently disarmed the visibility gate for
exactly the manual use the line protocol exists to support. `pid` is now required
and documented as required.

Parsing is strict rather than forgiving, because every lenient default here had a
failure attached. Missing or unparseable coordinates defaulted to `0` and drew a
confident arrow in the corner; a non-finite coordinate never satisfied the spring's
settle test, so the view would redraw forever and hand non-finite points to
`NSBezierPath`; and window id `0` is AppKit's documented "order in front of
everything at my level", the one placement this process must never request. All of
these are now refused, and a malformed line is dropped whole instead of
half-applied.

### `cargo build -p cua-overlay` did not compile

`NSRunningApplication::processIdentifier` is gated behind objc2-app-kit's `libc`
feature, which `cua-core` and `cua-hid` declare and `cua-overlay` did not. Cargo
unifies features across a workspace, so every workspace build supplied it and the
crate compiled; built on its own it failed, from 0.4.2 through the tagged and
released 0.5.0. CI only ever built the workspace.

The frontmost check is gone, so the call is too, and the feature is not needed
after all. CI now builds each crate separately with its own `CARGO_TARGET_DIR`
*before* the workspace commands, which is the only arrangement that can catch this
class — a shared target directory lets an earlier unified build leave usable
artifacts behind. Confirmed against `git archive` of the previous release: the new
step fails there. It immediately earned its place by catching a second instance in
this very change, where trimming the now-unused features also removed the one
`NSApplicationActivationPolicy` needs.

## 0.5.0

### `click_in_window`: a click with no element behind it

The gap this closes is a canvas. A custom-drawn map, chart, or game view
genuinely publishes no children, so `click` has nothing to resolve and no better
tree walk would help. An agent reading the screenshot has a pixel, and until now
that was a dead end — by *policy*, it turns out, not by capability. `PidClick` is
`{pid, point, window_local, wid, count}` and never contained an `Element`.
Accessibility is how cua-rs decides *where* to click; it was never how the click
is delivered.

It is a separate tool and never a fallback from `click`, because "this point
covers nothing" is exactly the shape of a typo and blind-clicking a typo is the
worst outcome available here. Callers have to ask for it by name.

Coordinates are **window-local points** — from the window's top-left corner,
which is the screenshot's own space divided by the `px per point` scale
`get_app_state` reports. Screen coordinates would have made the caller add the
window origin itself and would silently address the wrong pixel the moment the
user moved the window between the read and the click. These are re-anchored to
the live origin just before posting, so a window move is harmless.

Three gates, none advisory: the `window_id` must be the one this app's most
recent `get_app_state` read (`get_app_state` now prints `window_id=` for exactly
this purpose); that window must still exist, still belong to this pid, and still
be an ordinary window, re-enumerated rather than trusted from the snapshot; and
the offset must land inside the window's live frame, with negatives refused
outright.

The result is labelled `delivery: pid (no element)` — a distinct label, not a
footnote on `pid`. Every other delivery mode resolved an element first and so
names something accessibility agreed was there; this one names a pixel the caller
chose. **It confirms delivery and nothing else.** There is no element to inspect
afterwards, so the post-action delta is the only feedback, and on a canvas even
that is empty.

Delivery is unchanged: the same pid-routed SkyLight path, the same synthesized
activation notice. The cursor, keyboard focus, frontmost app and Space are still
untouched.

Measured on KakaoTalk's chat-list filter chips with the app in the background and
Terminal frontmost throughout: all three gates refused as specified, and the
accepted click switched the filter and switched it back.

### The pointer warp is gone

`cua_hid::click_by_moving_pointer` warped the real cursor to a screen point,
clicked through the shared HID stream, and warped back. It existed for
custom-drawn controls that advertise no `AXPress` and only respond to a real
click — and every one of those is now reachable through the pid tier instead.
Nothing called it; keeping a working pointer warp in the tree once its
justification had evaporated was leaving a temptation, not a fallback.

Deleting it took the last `CGWarpMouseCursorPosition` reference in the workspace
with it, so the absence is now checkable rather than merely documented: no edit
can reintroduce a cursor warp without adding that import back first. The
`menu_probe` example loses its `warp` arm, whose conclusion — that the real
pointer under the control is *not* what a stubborn menu is waiting for — had
already been drawn.

`post_chord` stays, still unreachable from the server, until chords land in the
pid tier.

### Other

- `AppState` gains `window_id`, printed in the `get_app_state` header.
- `Delivery` gains `PidNoElement`. Callers matching on it exhaustively will need
  the new arm.

## 0.4.2

### The drawn cursor no longer floats over another app

`cua-overlay` positioned itself with `setLevel(0)` plus
`orderWindow(Above, target_window_id)`, which only means anything while the
target window is alive and on the current Space. The overlay itself joins all
Spaces, so switching Space or going full-screen leaves it ordered relative to a
window that is not there — and the arrow could stay visible above whatever the
human switched to.

Ordering is no longer trusted on its own. The overlay polls
`NSWorkspace.frontmostApplication()` each frame and hides the arrow whenever the
pid it is pinned to is not the frontmost app, without asking why. That covers
every way ordering can fail at once — Space switch, full-screen, timing — and it
fails in the safe direction: a false positive costs one hidden arrow that returns
on the next command, a false negative is an arrow drawn over someone else's work.
The `move`/`click` protocol therefore carries a `pid` alongside the window id.

> **Superseded by 0.5.1, and wrong.** "A false positive costs one hidden arrow"
> was the mistake: against a background window the check is *always* positive, so
> the cost was the entire feature, and it left the arrow stranded anyway because
> hiding never reached the screen. See 0.5.1.

Notably **not** included: the focus-stealing machinery a shipped implementation
uses for this (a preventer process tap, re-activating the target). That takes
focus away from other apps, which §9 rules out, and it was not the cause here.

## 0.4.1

Fixes from an external review of 0.4.0. Nothing here is a new feature; the first
three are ways an action could act on the wrong thing.

### The activation assist could click a window it never validated

`window_focus_assist` chose its window with `AXFocusedWindow`, independently of
the window being clicked. In a multi-window app — a chat app with a list window
and several conversation windows, which is the case it was built for — it could
take window A's activation point, localize it against window B's origin, and stamp
B's window number onto a **real** synthesized click aimed at a point inside A. The
live gate proved only "some window of this pid", which A satisfied.

It now selects the AX window that corresponds to the window being clicked, by
frame, and additionally requires the activation point to lie inside that window's
own frame so the window-local coordinate cannot be negative or past the end.

Its two synthesized events also shared no event number — down got N and up got
N+1 — while the main click path hoists one number per pair precisely because
AppKit pairs an up with its own down by that field.

### Coordinates are refused against stale geometry

`acted_on` was honored by `find` but not by coordinate resolution, so an action
with `return_state: false` followed by an x/y click hit-tested pre-action frames.
Opening a disclosure and clicking the same point would resolve to whatever used
to be there. An index survives an action because it names an element; a point
names a place, so it now errors instead.

### The post-action diff refuses three more incomparable bases

- A walk that did not finish. Equal caps do not imply equal coverage, because the
  time budget depends on how fast the app answers: 300 nodes before against 500
  after reported 200 nodes as newly appeared.
- A snapshot an action already ran against, which attributed two actions' changes
  to the second one.
- Two windows that could not be identified. `None == None` was treated as "same
  window", so without Screen Recording it diffed two entirely different windows.

### The diff is documented as what it is

A textual multiset delta, not verification. Because lines are compared without
index or indentation, two elements with identical text are interchangeable: if a
selection moves between two rows that read the same, the delta is empty. That is
now stated in the tool description and in the code, rather than left for a caller
to discover. The behaviour is unchanged — the noise reduction it buys is worth
more than the identity it gives up, but only if callers know which they have.

### Also

- Window level 3 admits ordinary floating windows **and** `kCGTornOffMenuWindowLevel`,
  which share that level. A comment claiming menu levels were all above the cutoff
  was simply false, checked against the installed SDK. Frame matching is what
  keeps a menu from being chosen, so the one path with no frame evidence — the
  largest-window fallback — is now restricted to level 0.
- Equal-frame overlaps break toward the deeper element. A row and its only cell
  usually share a rectangle exactly, and the walk order always favoured the row.
- A failed capture no longer asserts an open menu is the cause. It hedges, and
  only for the specific window-server refusal it was observed with, not for
  timeouts and encode errors that happen to coincide with a menu.
- A failed post-action re-read is reported instead of looking identical to
  `return_state: false`. A click that closes the only window lands here.
- Corrected documentation: §6 claimed the activation notice is balanced per click
  when it has not been since 0.4.0, and §10 now records the belief left standing
  as a residual contract risk; §6's window-matching rule said level 0; §1
  overstated that accessibility covers the whole capability.

## 0.4.0

Behaviour a caller can see changed in three places, which is what makes this a
minor bump rather than a patch.

### Actions report what changed

Every action tool takes `return_state`, **on by default**, and answers with a
diff of the window against the tree from before the action. This replaces
`ui_changed`, which was a heuristic — it compared the focused element and the
window title, and answered `no` for real changes it could not see, a menu opening
being the measured case.

It is strictly cheaper than the `get_app_state` that would otherwise follow: one
tree walk either way, but one round trip instead of two and a few lines instead of
the whole outline. Pass `return_state: false` for a run of actions whose
intermediate states nobody will read.

The diff refuses to answer rather than mislead. It is computed only when the two
snapshots describe the same window, the same scope, and the same walk caps;
otherwise it returns the reason and a fresh `snapshot_id`. A capped read followed
by a click used to report 278 "appeared" lines on a dense app, all of them nodes
the capped walk had never reached.

Renumbering and re-parenting no longer count as change, so a chat app that
regroups its message table on every click no longer buries the one line that
matters.

### Coordinates are resolved against the snapshot

`AXUIElementCopyElementAtPosition` answers `AXMenuBar` for every point in a
background app, and every app cua-rs drives is a background app — so every x/y
click was silently retargeted at the menu bar and failed with a message about
window frames drifting apart. Coordinates are now hit-tested against the
snapshot's own element frames, preferring the actionable element and then the
smallest, and a point covering nothing is an error instead of a guess.

**This requires a prior `get_app_state`,** which the documented contract already
did.

### Clicks that used to do nothing

- Floating windows at layer 1-3 are valid targets. Requiring `layer == 0`
  excluded a chat app's own conversation windows.
- No `ApplicationDeactivated` notice after each click; it destroyed the
  key-window state the next click depended on. `CUA_DEACTIVATE_AFTER_CLICK=1`
  restores the old behaviour for comparison.

Together these fix menu-opening controls that advertise no AX actions.

### Also

- `find` re-walks when an action has run since the last read, instead of
  answering from the pre-action tree. It reported "no menu" about a menu that was
  open on screen.
- A failed capture says an open menu is the likely cause when the tree contains
  one, instead of passing through the bare `could not create image from window`.
  There is deliberately no region-capture fallback: measured, it returns whatever
  is actually in front, which was an unrelated app's window.
- `DESIGN.md` records that ScreenCaptureKit's `SCScreenshotManager` was measured
  head-to-head against `screencapture -l` and captures nothing extra, and that a
  degraded `replayd` makes every window report `isOnScreen=false`.

## 0.3.x

Not tagged. `press_key` became AX-only in 0.3.1, dropping the flag that also
enabled shared-pointer fallback.

## 0.2.0

## 0.1.0
