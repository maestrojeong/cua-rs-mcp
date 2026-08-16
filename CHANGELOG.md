# Changelog

Versions are `0.MINOR.PATCH`. While the crate is pre-1.0, a change an existing
caller can *notice* takes the minor slot, even when it is a bug fix — the tool
descriptions are the API here, and an agent that learned the old behaviour is a
caller.

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
