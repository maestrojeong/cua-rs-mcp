# Design notes

Why cua-rs is built the way it is, what is deliberately not built yet, what is
planned next (§11), and the largest thing currently declined (§12).

---

## 1. Why the Accessibility API and not `CGEventPost`

This is the decision everything else follows from.

The conventional approach synthesizes input: `CGWarpMouseCursorPosition` to move
the pointer, then `CGEvent::post(tap: .cghidEventTap)` to click. It is easy, it
works on every app, and it has one structural problem — the HID event stream and
the cursor are **singletons owned by the logged-in session**. An agent writing to
them is not "also using the computer"; it is contending for the same channel the
human is using. There is no version of that design where the agent does not
occasionally type into the wrong window.

The Accessibility API addresses an element, not a screen position:

```rust
// what typing actually is here
element.set_string("AXValue", "hello")?;  // AXUIElementSetAttributeValue
```

Neither call has a coordinate, a cursor, or a notion of focus. The app receives a
message and acts on it. (Through 0.4.x, `click` was also `element.perform("AXPress")`
first. As of this change it is not — see "click and press_key moved to pid-only"
below for why that changed and why `set_value`/`type_text` did not.)

### This was verified, not assumed

Most of a computer-use tool can be built out of accessibility actions alone —
`AXUIElementPerformAction`, `AXUIElementSetAttributeValue` and
`AXUIElementCopyAttributeValue`, all available in the public
`objc2-application-services` crate. Not all of it: controls that advertise no AX
action at all still need a synthesized event, which is why `cua-hid` exists and
why the ceiling below is stated plainly rather than implied. "Does not steal focus" is then not a feature flag but the absence of
event-posting code, which is a property a reader can check rather than trust.

cua-rs keeps a public-API event path as its reliable click tier, and additionally
ports one piece of private SPI for a *quieter* tier — the SkyLight
`SLEventPostToPid` recipe, dlopened lazily and confined to `cua-hid` (see the end
of §6). That is a deliberate, documented reversal of the "no private API" rule;
if the framework cannot be loaded, the click fails explicitly rather than falling
back to shared input.

### The cost, stated plainly

AX cannot express everything:

| Capability | AX verb | Status |
|---|:--|:--|
| press a button | `AXPress` | works |
| select a row / tab | `AXPick` | works |
| Return, Escape | `AXConfirm`, `AXCancel` | works |
| context menu | `AXShowMenu` | works |
| page a scroll area | `AXScroll*ByPage` | works |
| set text | `AXValue` write | works — and stays the default for `set_value`/`type_text`, see §1a. `type_text` can be asked for real keystrokes instead, for the targets that ignore the write |
| **arbitrary chord** (`⌘⇧P`) | — | no verb exists, but reachable since the pid-only change below via the pid tier (not a fallback — the only tier) |
| **drag** | — | no verb exists; delivered as a real down/drag/up gesture by the pid tier (§11). Not yet verified against a real drag source |
| **right click where the element advertises nothing** | `AXShowMenu` | works *where the element advertises it*; where it does not, a real `rightMouseDown`/`rightMouseUp` pair by the pid tier |
| **modifier click** (⌘-click, ⇧-click) | — | no verb exists — `AXPress` is "activate this", with no room for a held key. Pid tier only |
| **hover** | — | no verb exists; a synthesized `mouseMoved` by the pid tier. **Measured to work on web content** and measured to do nothing on a Finder list row (§11) — and permanently unable to reach an app that polls the real cursor |
| **scroll by a distance** | — | `AXScroll*ByPage` is whole pages only; a `scrollWheel` event covers the rest, and covers elements that advertise no scroll action at all. Implemented and **unproven** (§11) |
| pixel-only surfaces | — | nothing to address — reachable anyway, see below |

Those rows are softer than they look. "No verb exists" is a statement about
accessibility, not about delivery, and the pid tier delivers events without
consulting accessibility at all.

Most of these rows have since moved, always for the same reason: "no verb
exists" is a statement about the *vocabulary* of accessibility, not about what
can be delivered, and the pid tier delivers events without asking accessibility
anything. `click_in_window` clicks a bare window-local point with no element
behind it, which makes a canvas reachable — deliberately opt-in, labelled
`delivery: pid (no element)`, and explicitly unverified, because there is no
element to read back. §11 has the gates and the measurement. Chords moved too:
`press_key` routes every chord through the pid tier unconditionally (§1a).

Then the mouse model itself widened from "a left click with a count" to
`{origin, destination, button, modifiers, click_count}`, which is what the four
new rows above are: a right or middle button, held modifier keys, a drag with
interpolated intermediate moves, a hover, and a wheel scroll. §11 has the
design and the verification status, which is worth stating here too rather than
burying: the parsing, geometry, interpolation and tier selection are
unit-tested without any grant, and the gestures have since been walked against
real apps one at a time. The drag, the modifier click and the right click work.
The hover works on web content and does nothing to a Finder list row, which is a
split rather than a verdict. The wheel scroll does not work at all and is
refused by default. §11 has each measurement, and says which of them is one app
rather than a survey.

The honest ceiling is now: cua-rs drives *structured* UI extremely well, can be
aimed at an unstructured surface when the caller accepts responsibility for the
aim, and can express every ordinary mouse and keyboard gesture — with one
permanent exception, that it cannot make an app believe the *real* pointer
moved, because it will not move it.

### `press_key`: decided, then reconsidered

Four options across two rounds, none free:

1. **`AXUIElementPostKeyboardEvent`** — app-scoped, in the crate, deprecated, no
   modifier-chord support. Rejected.
2. **AX-only `press_key`** — background-safe semantic verbs remain; arbitrary
   chords are refused. Chosen in 0.3.1.
3. **AX where a verb exists, HID behind an explicit flag otherwise.** Removed
   in 0.3.1 because the flag also enabled shared-pointer fallback.
4. **Pid-only, no AX verb attempted at all.** Chosen below.

Through 0.4.x, `press_key` mapped `return`/`enter` to `AXConfirm`, `escape` to
`AXCancel`, and `up`/`down` to `AXIncrement`/`AXDecrement`, and refused every
chord and every letter. Every successful key action reported `delivery: ax`;
there was no shared keyboard-input delivery mode.

One subtlety from that era is still worth recording, because it produced a
self-contradicting error message before it was fixed: a key can *have* an AX
verb that the *target element* does not accept. A tab button has `AXPress` and
`AXShowMenu` but not `AXCancel`, so `escape` on it used to fall through to the
generic "no accessibility equivalent" refusal — text that named `escape` as
something which works without HID. That case has its own error (still true
under `CUA_KEY_AX_ONLY=1`, see below) naming the verb, listing what the
element does accept, and pointing at where `AXCancel` usually lives (the
window or a dialog's default button).

---

## 1a. `click` and `press_key` moved to pid-only

Through 0.4.x, `click` tried `AXPress`/`AXPick`/`AXConfirm` first and fell back
to the pid tier only when the element advertised no AX action, and `press_key`
never touched the pid tier at all. This change removes both fallbacks: `click` and
`press_key` now go through `cua_hid`'s pid-routed delivery unconditionally,
with no AX attempt in either direction. `set_value` and `type_text` are
**not** part of this change — they still write `AXValue`/`AXSelectedText`
exactly as before.

**Why this asymmetry, and not "AX everywhere" or "pid everywhere":** the split
follows what each API can actually express, not a preference.

A click and a key press are *events*. Accessibility has no vocabulary for
either: `AXPress` is "activate this element" with no notion of click count, so
it cannot say "double-click" at all (§1 measured a chat list that opens on two
and merely selects on one); and there is no verb for a chord, only `AXConfirm`
for Return and `AXCancel` for Escape. Anything a caller might mean by "press
`⌘⇧P`" has to become a real event whatever else happens, so making events the
*only* path costs nothing that AX was providing.

Setting text is not an event. `AXValue` writes replace a whole string in one
call, atomically, addressed at the element — which is precisely what a caller
asking to set a field wants. Delivering the same thing as keystrokes means a
stream of events landing wherever the target process's first responder happens
to be, character by character, with no element addressing. That is strictly
worse for the one operation AX does exactly right, so `set_value` and
`type_text` default to it.

The exception proves rather than dents the rule. A terminal or a canvas editor
ignores an `AXValue` write outright, and there the better mechanism does
nothing at all — so `type_text` takes `mechanism: "ax" | "keystrokes"` and
sends real per-pid key events on request. It is a separate, explicit choice
rather than a fallback from a failed write: cua-rs cannot tell "the app
accepted the write" from "the app took the write and ignored it", so an
automatic retry would be guessing, and guessing with a keystroke stream is the
expensive direction to guess in. A caller reaches for `keystrokes` when they
know what they are aiming at, and gets the same `focus` verdict `press_key`
reports (§10) because a long string multiplies a misdelivery by its own length.

That also reframes what "no AX fallback" costs on the click path. Retrying a
failed pid click through `AXPress` sounds free and is not: it reintroduces the
app-specific quirks the pid tier exists to escape — an element that advertises
`AXPress` but silently ignores it, a control whose action fires but whose visual
state lags, a stale AX handle recycled onto different content that would still
happily accept a press. One delivery mechanism per action, chosen once, is
easier to reason about than "try A, and if that seems to have failed, also try
B"; the second half of that sentence is where the surprising bugs live.

The corollary is that the "arbitrary chord — no verb exists, still refused"
row from §1's capability table is gone: `press_key` no longer has a concept of
"no verb exists" to refuse, because it never consults a verb. `cmd+shift+p`,
`ctrl+alt+delete`, a bare letter — all of it goes through
`cua_hid::parse_chord` and `press_chord_background_pid`, gated only on whether
the chord *parses* and whether the SkyLight primitives are available on this
macOS version, not on whether AX has an opinion.

**What this does not change:** the cursor is still never warped, the shared
keyboard tap is still never posted to, and `NSRunningApplication.activate` is
still never called — every click and key event this crate sends goes out
through `cua_hid`'s per-pid recipe (§6), exactly as the pid tier already
worked in 0.4.x. What changed is which actions reach that tier and whether AX
is tried around it, not the tier's own mechanics.

**Escape hatches, one per action, not one shared flag:** `CUA_AX_FIRST=1`
restores 0.4.x's `click` order (AXPress first, pid only when no AX verb
exists, with a retry through AX if pid then fails). `CUA_KEY_AX_ONLY=1`
restores 0.4.x's `press_key` (`return`/`escape`/`up`/`down` only, chords
refused, no synthesized input at all). `CUA_KEY_STRICT_FOCUS=1` keeps the pid
keyboard tier but refuses to deliver when the app names a different element as
focused (§10) — the one switch here that tightens rather than reverts. None is
a supported "best of both" mode — mixing tiers per call is the exact pattern
the paragraph above argues against — they exist to bisect a specific app or
macOS version where the pid tier turns out to be the less reliable choice.

**The manual-test checklist (§8) predates this change** and still describes
0.4.x's tier order in places; it has not yet been re-walked end to end against
this change's pid-only click path. Treat rows there that assume an AX-first
click as aspirational until they are re-verified. The keyboard rows are the
exception: they are gone, replaced by the automated read-back tests §8 now
describes.

---

## 2. Why isolated window capture and not `CGWindowListCreateImage`

`CGWindowListCreateImage` returns what the window server composited. A window
the user has covered comes back blank or stale — precisely the window an agent
working in the background is driving.

ScreenCaptureKit supplies the live window identity and frame. The PNG itself is
requested by `CGWindowID` through macOS's `/usr/sbin/screencapture` in a
one-shot process; measured visible and off-Space windows both capture correctly.
The process boundary is mandatory because `SCContentFilter` can reach an
unrecoverable SkyLight assertion when an app rebuilds a window and briefly
publishes an invalid rect. A Rust `Result`, panic boundary, or preflight cannot
catch that `SIGABRT` or close the validation/use race.

Two secondary reasons:

- **Per-window, not full-screen.** A 5K grab is ~15 MB, mostly wallpaper and the
  human's unrelated windows; downscaled to fit a vision model, the target app's
  text is illegible. It also means screenshots do not exfiltrate whatever else
  was on screen.
- **Server survival.** The system capture process inherits the responsible
  host's Screen Recording authorization. If WindowServer rejects a transient
  window, only that disposable process fails and the MCP server returns a
  screenshot warning while preserving the AX tree and connection.

### Some windows refuse to be captured, and there is no safe fallback

Measured on KakaoTalk: `screencapture -l<id>` fails with `could not create image
from window` for a window that captured fine moments earlier, while a sibling
window of the same app still captures. Twice the failing window had an `NSMenu`
or a modal up and closing it restored capture, so the warning names an open menu
when the tree contains one — but that correlation is **not established as the
rule**. Those measurements were taken while ScreenCaptureKit on the machine was
in a degraded state (see below), which can make capture fail broadly, and the
condition has not been re-measured against a healthy one.

`on_screen` is not the discriminator, and worse, it is not always true: with a
sick `replayd`, `SCShareableContent` reported `isOnScreen=false` for *every*
window in the system including the visibly frontmost one. `killall replayd`
restored it. Check that before concluding anything about a particular window —
it nearly produced a bug report against this crate's own `on_screen` field.

### ScreenCaptureKit instead of `screencapture` buys nothing here

The modern alternative is `SCScreenshotManager.captureImageWithFilter` over
`SCContentFilter(desktopIndependentWindow:)`, which is the API behind the
screen-sharing indicator macOS shows for tools that use it. Measured head to head
against `screencapture -l` on five windows, with a signed probe holding its own
Screen Recording grant: **the two agree on every window.** ScreenCaptureKit
succeeded exactly where `screencapture` succeeded and failed exactly where it
failed (`-3811` on all three retries). The block is at the window layer, not in
the API.

So the rewrite is not worth it, and the cost is precisely §7: a signed bundle
with a Team ID and its own user-granted Screen Recording entry. A tool willing to
raise the target app before capturing never has to photograph an occluded window
and can afford that path anyway; §1's cursor-and-focus contract forbids us the
same move.

Two smaller measurements from the same probe, worth keeping:

- A second `SCScreenshotManager` capture *in one process* fails with `-3811`
  whatever window it names, while the first succeeds. Comparing windows requires
  one process each.
- `SCContentFilter` can abort the process outright — `Assertion failed:
  (did_initialize) CGS_REQUIRE_INIT` inside `SLGetDisplaysWithRect` — before any
  capture is attempted. That is an uncatchable `SIGABRT`, so if this path is ever
  adopted it stays behind the isolated worker regardless.

The obvious fallback is to capture the window's screen *region* instead, and it
does succeed while the menu is open. It was rejected after measuring what comes
back: a region capture of a covered KakaoTalk window returned an unrelated app's
window, because a region capture photographs whatever is actually in front. That
is a wrong answer presented as a right one, and it discloses a window the caller
never asked about — the exact thing per-window capture exists to prevent.

So the failure is named instead. When the tree that was just walked contains an
`AXMenu`, the capture warning says the menu is why and how to clear it. The bare
OS text reads like a permission or window-identity problem and invites retrying
what cannot work until the menu goes away.

Captures are requested at backing-store resolution, then clamped with macOS
`sips` (`max_image_dim`, default 1400). Asking for point dimensions instead
yields a soft half-resolution image on Retina where small UI text becomes unreadable.
`WindowShot::scale` is derived from what came back rather than what was asked
for, so a clamp cannot desynchronize point↔pixel mapping.

---

## 3. Snapshots, and why indices are generational

An action needs to name an element. The options:

| Scheme | Problem |
|---|:--|
| coordinates | wrong after any scroll or resize |
| role + label path | ambiguous, and re-resolution is another round trip |
| raw `AXUIElementRef` | not serializable to a model |
| **index into one walk** | needs staleness handling ← chosen |

So: `get_app_state` walks once, numbers everything actionable, retains the
handles, and stamps the batch with a process-global `snapshot_id`.

The subtle part is that index 42 in snapshot 1 and index 42 in snapshot 3 are
*different elements*. Remapping silently would be the single worst bug this
system can have: the wrong control gets pressed, the agent reports success, and
nobody can see it happened. So `snapshot_id` is accepted on every action and, when
supplied, mismatches are a hard error.

It is optional rather than required because the overwhelmingly common flow is
read-then-act inside one turn, and forcing the id would add a failure mode
without adding safety there. Careful callers get the guarantee; casual ones get
the ergonomics.

### Tree rendering

Only actionable nodes get `[N]`. Handing a model an index it cannot act on
invites a call that can only fail.

Layout wrappers (`AXGroup`, `AXScrollArea`, `AXSplitGroup`, …) with no label, no
value and no action are dropped, and — importantly — their children are drawn at
the *wrapper's* indent level, so removing them leaves no phantom step. Real apps
nest these thirty deep.

Indentation rather than JSON: roughly a third of the tokens for the same
information, and a truncated outline is still readable where truncated JSON is
not.

### Skeleton mode

`max_nodes` bounds the walk; skeleton mode bounds the *rendering*, which is a
different problem. A tree truncated at 1500 nodes is still 1500 lines of prompt,
most of them rows the agent did not ask for. Measured: a Slack window is 367
elements and a Chrome window 413, both comfortably under the node cap and both
far more than an agent needs to see at once.

So with `skeleton: true`, a subtree deeper than `skeleton_depth` (2) with more
than `collapse_over` (8) descendants renders as one line naming its size and its
own index:

```text
[5] AXGroup  (+40 elements — pass scope_element_id=5 to expand)
```

Three details that matter:

- **Subtree sizes are computed bottom-up over the flat list**, exploiting the
  fact that a BFS walk always places a parent before its children. Iterating in
  reverse therefore visits every child before its parent, so no second traversal
  is needed. The count is whole-subtree, not direct children — a list of 10 rows
  each holding a button reports 20.
- **Depth 0 and 1 never collapse.** The window and its direct children are the
  map; collapsing them would hide the territory and the map together.
- **The summary states the index outright** rather than relying on the `[N]`
  prefix, because `scope_element_id` works on any element while `[N]` only
  appears on actionable ones. A non-actionable `AXGroup` is a perfectly good
  drill-in root.

The drill-in walk starts from that element instead of the window, and re-numbers
from zero under a new `snapshot_id` — it is a new snapshot, so treating its
indices as continuous with the previous one would be exactly the staleness bug
§3 exists to prevent.

### Budgets are correctness, not tuning

`Limits { max_nodes: 1500, max_depth: 40, max_children: 200 }`.

An AX tree can be effectively unbounded (a virtualized 100k-row table) and is not
guaranteed acyclic. An uncapped walk is a hang.

The walk is **breadth-first**, which matters more than it sounds: depth-first
spends the entire node budget inside the first sidebar and never reaches the main
content area the agent actually wants.

Every app element gets `AXUIElementSetMessagingTimeout(2.0)` immediately on
creation, before any other call. AX is synchronous IPC; a modal or wedged app
otherwise blocks the worker thread on the first attribute read, and a computer-use
server that hangs forever on one bad app is worse than one that reports a
timeout.

---

## 4. Threading

`Element` wraps `CFRetained<AXUIElement>` and is `!Send`. That is correct, not an
oversight — the handles are only meaningful to the thread that established the
AX connection.

The MCP layer is async and multi-threaded, and `rmcp` may poll a tool future on a
different worker between awaits. The tempting fix is `unsafe impl Send`, which
silences the compiler without making anything true.

Instead: one long-lived `cua-native` thread (8 MB stack — AX trees recurse) owns
everything, and the async side ships closures to it and blocks on a reply
channel. `spawn_blocking` keeps that block off the tokio workers.

Two properties fall out for free:

- Handles never cross a thread boundary, so `Element` stays honestly `!Send`.
- Tool calls serialize. Two concurrent calls cannot interleave a tree walk with
  an action, which would otherwise let an agent act on an element a
  half-finished snapshot had already invalidated.

Each job runs inside `catch_unwind`, so one malformed tree cannot take the worker
down and with it every future call. This is why the workspace uses
`panic = "unwind"` where the sibling projects use `abort`.

---

## 5. Electron and Chromium

Chromium keeps its web-content AX tree switched off until an assistive client
asks, because maintaining it is expensive. Without a poke, Slack / VS Code /
Discord / Notion look like one empty `AXWindow` — which reads exactly like a bug
in our walker.

The fix is one attribute write on the *application* element:

```rust
app.set_bool("AXManualAccessibility", true);      // modern
app.set_bool("AXEnhancedUserInterface", true);    // legacy fallback
```

**Do it once per process lifetime, not once per snapshot.** Setting it repeatedly
makes every renderer rebuild its tree in a loop and pegs WindowServer — a
documented production failure in other projects, not a theoretical one. Keyed on
`(pid, start_time)` rather than pid, because pids are recycled and a relaunched
Electron app must not inherit its predecessor's "already enabled" decision.
Start time comes from `proc_pidinfo(PROC_PIDTBSDINFO)`; `sysctl(KERN_PROC_PID)`
would need `struct kinfo_proc`, which the `libc` crate does not expose on Apple
platforms.

### What was actually measured

Three things here are not what the obvious implementation assumes. All measured
on macOS 26.5 with the `ax_poke` example in `crates/cua-ax/examples/`:

**The read-back lies.** Slack accepts `AXManualAccessibility = true`, reports
success, and then reads the attribute back as `false` — permanently, even while
it is demonstrably exposing a 367-element tree containing an `AXWebArea`:

```text
AXManualAccessibility   settable=true  write=ok  after=Some(false)
  +0ms  367 elements, 355 actionable, 106 with text
```

So a `false` read means nothing, and any logic that concludes "this app refused
enablement" from it is wrong. An earlier version of this code did exactly that
and would have emitted a confident, false warning on Slack.

**`AXEnhancedUserInterface` advertises itself and is not implemented.**
`is_settable` returns `true`; the write fails with `NotImplemented` (-25208) on
both apps tested. Kept anyway because it costs one call and older AppKit apps
still honor it.

**The tree does not appear promptly.** Slack held at 13 elements for at least
3.2 seconds after the poke and was at 367 about a minute later. Chrome went from
48 elements (native chrome only) to 413 (web content included) over a similar
span. So the 400 ms settle in `get_app_state` is a courtesy for apps that are
quick, not a wait for completion — and a caller that sleeps briefly and then
declares the window empty will be wrong.

Hence the design: a small tree on the *first* read of an app produces a warning
that says the tree may still be building, says to call again, and says what a
persistently small tree means. That is correct in both branches, which neither
"it is empty" nor "it refuses AX" is.

### Frontmost does not matter, and this was checked

The natural suspicion about the 13 → 367 jump is that the window had to be
frontmost. It did not. Controlled by activating each app and restoring focus:

| App | background | frontmost |
|---|--:|--:|
| Google Chrome | 413 | 413 |
| Finder | 4 | 4 |
| Slack | 367 | 373 |

Identical for Chrome and Finder; Slack's +6 is a focus ring and hover affordances
appearing, not content. Slack was also *occluded* by the terminal throughout the
background measurements.

This is a positive result for the project's central claim rather than a
footnote: reading and driving a window does not require it to be visible, on top,
or active.

The remaining honest gap is that the exact settle time is bounded, not known:
longer than 3.2 s, shorter than ~1 minute, measured on two apps. Pinning it down
needs a never-before-poked app launched under controlled conditions.

Longer term the better answer for Chromium content is not AX at all: hand it to
[browser-rs](https://github.com/maestrojeong/browser-rs-mcp) over CDP.

---

## 6. Window identity without private API

Actions come from AX (which knows `AXWindow` elements). Pixels come from SCK
(which knows `CGWindowID`). Bridging them is normally done with
`_AXUIElementGetWindow` — a private symbol.

cua-rs matches on public API instead: same pid, plausible target (window level
0-3, ≥40pt each side), then the smallest frame distance to the AX frame. With no
AX frame there is no evidence tying any window to what accessibility is showing,
so that fallback is narrower: the largest on-screen window *at level 0*. Level 3
is shared by ordinary floating windows and `kCGTornOffMenuWindowLevel`, so
admitting it without frame evidence could stamp a menu's window number onto a
click meant for content.

Trade-off, accepted deliberately: slightly fuzzier than the SPI, but it cannot
break on a macOS update. Tolerant comparison is required because AX reports
points while SCK's numbers can drift a pixel or two mid-animation.

### Input synthesis and the SkyLight SPI

The private-API rule above is about *identifying* a window. There is one
deliberate exception, for a different job: *input synthesis*. The quiet click
tier (after the AX tier) routes a click to a
specific process without moving the cursor, by porting cua-driver's SkyLight
`SLEventPostToPid` recipe — the only route that reaches custom-drawn and
background controls that AX cannot express and that must not be clicked by
warping the pointer over whatever is on top.

The tradeoff is explicit and bounded:

- **Confined to `cua-hid`.** `cua-ax` and `cua-capture` never touch it, so the
  "does not steal focus / does not warp the cursor" claims of the AX and
  capture paths stay exactly as strong as they were.
- **Lazy and fail-closed.** The framework is `dlopen`ed at first use and every
  symbol is `dlsym`ed. PID delivery is enabled only when posting, private field
  stamping, and window-local positioning are all available; a partial recipe
  returns an error rather than claiming an unpinned event succeeded. No
  link-time dependency, no hard crash surface.
- **No shared-input fallback.** Its results are tagged `delivery: pid`, distinct
  from `ax`. If the SPI is unavailable, the action fails; it never warps the
  pointer or posts to the shared keyboard stream.
- **Synthesized activation, never real activation.** `NSRunningApplication.activate`
  is still never called: it can divert a physical keystroke even without a window
  raise, and if the target's windows live on another Space macOS switches Spaces
  under the user. What *is* sent is an `NSEventTypeAppKitDefined` event with
  subtype `ApplicationActivated`, posted into the target's own event queue. The
  window server's key focus never changes, so the user's typing keeps going where
  it was going; only the target's private idea of "am I active" moves.

  That belief is **not** revoked per click. Sending a matching
  `ApplicationDeactivated` after every click was measured to destroy the
  key-window state the *next* click depended on, so it is off by default
  (`CUA_DEACTIVATE_AFTER_CLICK=1` restores it for comparison). The belief is
  therefore left standing, and the earlier claim that real AppKit events correct
  it as soon as the user touches anything is **unverified** — macOS has no reason
  to deactivate an app it never considered active. A target that keeps behaving as
  though it were active is a residual contract risk; see §10.

  This reverses an earlier "no focus assist at all" stance, which was
  measured to be the reason clicks on views that gate on `NSApp.isActive` — a
  chat app's conversation rows being the case that forced the issue — silently
  did nothing. The residual risk is second-order and app-specific: an app whose
  own activation handler calls `activateIgnoringOtherApps:` could turn the notice
  into a real raise. No such app has been observed, and the alternative was a
  click path that provably did not work.
- **No duplicate post.** Each stamped event is sent exactly once through
  `SLEventPostToPid`; the public process-post route is retained only as a probe.
- **Last-moment window validation.** Before posting, the captured window id must
  still belong to the same pid and contain the live AX activation point. Its
  current frame, not the snapshot frame, determines window-local coordinates.
- **Not window identity.** It does not weaken the §6 conclusion: this crate
  still never calls `_AXUIElementGetWindow`.

---

## 7. TCC and distribution — the actual hard problem

This is where projects in this category die, and it is not a code problem.

**Grants attach to the responsible process.** A user who grants Accessibility to
iTerm must grant it again for Claude Desktop, for Cursor, for Codex CLI. There is
no way to pre-authorize from the binary's side. All cua-rs can do is say so
clearly — which `check_permissions`, `cua-rs permissions`, the startup warning,
and `install.sh` all do.

**Never prompt from a status check.** An MCP server usually runs headless under a
supervisor where a system prompt appears detached from anything the user is
looking at, or not at all. `is_trusted()` is the non-prompting
`AXIsProcessTrusted`, deliberately not `AXIsProcessTrustedWithOptions`.

**A version bump does *not* cost a re-approval, and an earlier draft of this
document was wrong to say it did.** The claim was inherited from projects whose
binary is its own responsible process. It does not apply here, and the test is
three lines:

```console
$ cp target/debug/cua-rs /tmp/elsewhere/cua-rs        # different path
$ printf '\x00' | dd of=/tmp/elsewhere/cua-rs bs=1 \   # different cdhash
    seek=$(($(stat -f%z /tmp/elsewhere/cua-rs) - 1)) conv=notrunc
$ /tmp/elsewhere/cua-rs permissions
accessibility:    true
screen_recording: true
```

A byte-mutated copy at an unrelated path still holds both grants, because TCC
never keyed them to this binary — it keyed them to the terminal that launched it.
So Developer ID signing would not buy fewer re-approvals for the MCP use case;
the grant is not on our artifact to begin with.

Re-approval happens when the **host** changes, which is the same statement as the
first point above, not a second problem.

What the lack of a Developer ID actually costs is **Gatekeeper on download**, and
only on one path:

| How the binary arrives | `com.apple.quarantine` | Result |
|---|:--|:--|
| `curl ... \| sh` (install.sh) | absent — measured | runs immediately |
| downloaded in a browser from Releases | present | **blocks on launch** |

The second row hangs rather than erroring, which is the worst possible failure
mode for something an MCP client spawns: the client waits forever on a handshake
that will never arrive. `install.sh` therefore strips the attribute defensively
even though its own `curl` path never sets it, and the README says what to run if
someone downloaded the asset by hand.

Signing state, then, is a Gatekeeper question rather than a TCC one:

- **now:** ad-hoc signature with a stable `--identifier` in `release.yml`, plus
  quarantine handling in `install.sh`. Sufficient for `curl | sh`.
- **would be nicer:** Developer ID + notarization, so browser downloads also just
  work. Requires an Apple developer account; `MACOS_CERT_P12` /
  `MACOS_CERT_PASSWORD` / notarytool credentials are the intended secret names.
  Deliberately not pursued — it buys one download path, not fewer permission
  prompts.

**Screen lock.** Handled — see §7a, which is where the whole safety layer now
lives. Mutating tools return a recoverable error while locked; reads continue.

---

## 7a. The safety layer

Everything above answers "can cua-rs reach this control". This section answers
"should it". The two questions live in separate files —
`cua-core/src/safety.rs` holds all of the policy, and `session.rs` calls it from
one place — because the delivery path is judged on whether the event lands and a
refusal is judged on whether a human reading the transcript afterwards agrees
with it.

| gate | default | flag | what it refuses |
|---|:--|:--|:--|
| session scope | **off** | `CUA_ALLOWED_APPS=id,id` | actions on any app outside the scope the launcher named |
| forbidden target | **on** | `CUA_ALLOW_FORBIDDEN_TARGETS=1` | actions on credential and security apps, plus their screenshots |
| destructive label | **on** | per-call `confirm_destructive` | `click`, `press_key`, `perform_secondary_action` on a target that reads as removing something |
| screen lock | **on** | — | every action while the session is locked or the saver is up |
| yield to human | **off** | `CUA_YIELD_TO_HUMAN=1` | actions on an app the human is currently using |
| HTTP bearer token | **on** | `CUA_HTTP_TOKEN` sets it | any `/mcp` request without `Authorization: Bearer` |

Every action gate is checked once, in `Inner::acting`, which every action
already passes through. That is deliberate: a tool added later is gated by
default instead of by somebody remembering, and the order (session → app →
element) means the error names the most fundamental reason rather than the last
one checked. Refusing a click on "Delete" as destructive when the real problem
is that the screen has been locked for an hour sends the agent down the wrong
path.

### A scope beats a blocklist, and the two are not alternatives

The forbidden list and the five heuristics beside it all answer the same
question — *is this dangerous?* — and they answer it by enumerating danger. That
is structurally lossy: every app nobody thought of is admitted, and the list can
only ever grow behind the ecosystem. `CUA_ALLOWED_APPS` inverts it and asks a
question that has an actual answer, *what is this run for?* Everything outside
that fails closed by construction rather than by vigilance.

Three properties are load-bearing.

**It is set by the launcher, and there is no tool to widen it.** A `grant_app`
tool would let the agent extend its own reach, which is not a scope at all. So it
is an environment variable, read once through a `OnceLock`, never writable at
runtime. The refusal says the human has to change it and restart, because that is
true and the agent should stop trying.

**It narrows and never widens.** Scoping a run *to* a password manager does not
lift the forbidden floor — `guard` checks the floor first, and there is a test
pinning that both are true of the same bundle id at once. The two gates compose in
one direction only. That also fixes the coarseness the blocklist had on its own:
System Settings is blocked whole because a bundle id cannot say which pane is
open, and the only escape used to be `CUA_ALLOW_FORBIDDEN_TARGETS=1`, which lifts
*everything*. With a scope you can hand one app the permission it needs without
disarming the floor for the rest.

**An empty value is an empty scope, not an absent one.** `CUA_ALLOWED_APPS=""`
refuses every action rather than running unscoped, and the deciding case is
`CUA_ALLOWED_APPS=$TYPO`, which expands to exactly that. A scope that opens
itself when its value fails to arrive is a gate that fails open on a
misspelling — the wrong direction for the one gate here whose whole job is to
fail closed. Refusing everything is loud, immediate, and names what to fix;
silently permitting everything looks like success until it is not. Unsetting the
variable is how a caller asks for unscoped.

Two smaller rules, both tested. Matching is exact on the whole identifier, never
a prefix, so `com.apple.Safari` does not admit Safari Technology Preview and
`com.apple` does not quietly mean every Apple app. And a process publishing no
bundle identifier at all is refused under a scope rather than admitted, because
an allowlist that waves through what it cannot name is not one; the refusal is a
separate variant from "not on the list" so the two call for different fixes.

Reads are untouched, including screenshots. The scope answers what may be
*driven*, not what may be looked at — and unlike the forbidden list there is no
secrecy argument here, since the human chose the machine's contents, not the
scope's silence.

### The yield tap lives in `cua-hid`, and that placement is load-bearing

`§4` and the README both claim that `cua-hid` is the only crate in the workspace
that touches the event APIs. That claim is how a reader verifies the product's
central promise without taking anybody's word for it: they open five
`Cargo.toml` files and see which one links `CGEvent`.

The listen-only tap was first written inside `safety.rs`, which meant `cua-core`
linked `CGEvent` too. Nothing was wrong with the code — it constructs no event
and posts none — but the invariant had quietly degraded from something checkable
in a manifest to something checkable only by grepping for `CGEventCreate` and
believing the result. That is a worse property even when the behaviour is
identical, because the next person to need "just a read" from the event surface
has a precedent instead of a wall.

So the mechanism moved to `cua-hid::humanwatch` and the policy stayed here.
`cua-core` now links `objc2-core-graphics` for `CGSession` alone — a state read
about whether the screen is locked, which is not an event API by any reading.
`safety::HumanWatch` decides *whether* to watch and what a refusal says;
`humanwatch::InputWatch` owns the tap, the callback, the run loop and the
teardown. Holding the `InputWatch` is what keeps the tap alive, so the lifetime
is the type rather than a convention.

### Reading is allowed; acting is not; photographing is not either

The blocklist refuses actions and permits `get_app_state`, `find`, `wait_for`
and `list_apps`. The split follows what each operation can do wrong.

An action is irreversible and mis-aimable. cua-rs resolves a target from a tree
it walked milliseconds ago, and §10 already records that its own
change-detection is a heuristic; in a password manager the gap between the
control cua-rs meant and its neighbour is the gap between reading a vault and
emptying it. A tree read is bounded by what accessibility publishes, which is
what any screen reader on the machine already sees, and it is what makes a
refusal diagnosable at all — an agent that cannot even call `list_apps` on a
blocked app cannot explain to its user why it stopped.

The screenshot is the exception, and it is the interesting one. Pixels are the
one read that reproduces the secret rather than describing the UI around it, so
`get_app_state` on a forbidden target returns the tree, drops the image, and
says so in a warning. That keeps the app observable enough to reason about and
closes the most direct exfiltration path.

### Bundle identifiers, not names

Matching is on the bundle id. A display name is text an app chooses — shipping
one called "Keychain Access" is trivial — and, in the direction that actually
matters here, the real Keychain Access is called something else in every
non-English locale. A name-based list would fall open on the maintainer's own
machine.

Three groups are listed with three different reasons, because the error message
quotes the reason: credential stores (Keychain Access, Passwords, the major
third-party managers), System Settings, and login/unlock/authorization surfaces.
System Settings is blocked *whole* rather than per-pane: the bundle id is all
this gate can see, and the process that holds Privacy & Security, FileVault and
Login Items is the same process that holds the wallpaper picker. Refusing the
app is over-broad and honest; pretending to know which pane is open would be
neither.

Behind the curated list is a substring rule on the bundle id — `password`,
`keychain`, `authenticator`, `bitwarden`, and a handful more. No list of
third-party password managers can stay complete, and missing one is the worst
failure this module has. A bundle id is not marketing text: a developer writing
`password` into their reverse-DNS identifier is telling us what the app is for.
The false positive it will eventually produce costs one environment variable.

### Destructive labels: a parameter, not a prompt

§10 named this as "the obvious next safety feature". It is a heuristic over the
target's label, help text and — only when the element is not writable — its
value, plus the key itself on `press_key`.

The confirmation is an explicit `confirm_destructive: true` argument rather than
an interactive prompt, because an MCP server has no channel to a human. The
refusal names the exact parameter that clears it, which turns the decision into
two lines of transcript a person can scroll back to. A prompt would have to be
answered by the model anyway, invisibly.

The classifier is tuned to over-refuse. A false positive costs one round trip
with an error that says precisely what to send; a false negative costs a deleted
conversation. Two details fall out of that:

- **English stems, matched at word starts.** `delet`, `remov`, `eras`,
  `discard`, `reset`, `trash`, `revok`, `clear` and friends, so `Delete`,
  `Deletes` and `Deleting` all match from one entry. Word-start rather than
  substring is the single place precision wins, and it wins cheaply: it stops
  `Presets` matching `reset` and `Undelete` matching `delete`, neither of which
  removes anything.
- **Korean as plain substrings.** 삭제, 제거, 지우, 초기화, 버리, 휴지통, 비우기,
  탈퇴, 나가기, 저장 안. Korean has no word boundary to anchor to, and the
  `Presets` problem does not arise, so the looser rule is the right one. This is
  not an afterthought: the maintainer works in Korean apps and KakaoTalk is a
  standing test target, where 나가기 leaves a chat room and takes its history
  with it.

A text field's own *contents* are never classified. Otherwise `set_value` on a
note reading "remind me to delete the old files" would be refused, and no
confirmation could make that sensible.

`press_key` classifies the key as well, because a Delete carries its meaning in
the key rather than in the element: any modified Delete or Backspace (`cmd+delete`
is Move to Trash almost everywhere), and a bare Delete anywhere that is not a
text entry field — including anywhere cua-rs could not identify the element at
all, which is the fail-closed reading.

`click_in_window` gets no confirmation parameter. There is no element behind its
point, so there is no label to judge, and offering a confirmation that classifies
nothing would be a promise the tool cannot keep. The other three gates still
apply to it.

### Screen lock: a read at the boundary, not an observer

`CGSessionCopyCurrentDictionary()["CGSSessionScreenIsLocked"]`, plus a check for
`com.apple.ScreenSaver.Engine` in the running-app list. Both are public, and
both are cheap enough to do at every action.

The alternative was a distributed-notification observer for the screensaver
start/stop pair. It was rejected for the reason §4 exists: an observer needs a
run loop, and it caches an answer that can go stale between the notification and
the action. A direct read cannot be stale, costs a dictionary copy against an AX
round trip that already costs milliseconds, and adds no thread. The saver check
is a running-app scan rather than a notification for the same reason.

The lock read fails **open** deliberately: `CGSessionCopyCurrentDictionary`
returns nothing at all to a process with no window-server session, and reading
that as "locked" would make cua-rs permanently refuse in exactly the headless
setups where a lock cannot happen. The absent-key case is likewise unlocked,
which is what the window server means by it.

Reads continue while locked. They return whatever the app still publishes, which
may be a lock screen or nothing; refusing them would break polling loops for no
gain, since a read cannot change anything a human will later be surprised by.

**What the screen-lock check does not cover, stated plainly.** It is the cheap
90%, and the gap is a race rather than a hole in the API:

- **The window between "lock requested" and "screen locked".**
  `CGSSessionScreenIsLocked` flips when the lock takes effect, not when the user
  hits the hot corner or closes the lid. An action already in flight — or one
  that read the flag a moment before it flipped — proceeds. The exposure is
  bounded by one action, and by an action cua-rs was authorized to take a second
  earlier, but it is real.
- **The same race on the way out.** Nothing re-checks mid-action, so a long
  action that begins unlocked finishes unlocked as far as this gate is concerned.
- **Display sleep, and a locked *other* session.** A dark screen with no lock is
  not detected, and neither is fast user switching to another console, where the
  driven app's session is no longer the one on screen.
- **"Require password after N minutes".** A screen saver inside its grace period
  is caught by the saver check but is not, strictly, a locked session — the check
  is deliberately broader than the lock flag, and will refuse where a purist
  would allow.

Closing the race properly means being *notified* rather than polling, and by
something whose lifetime does not depend on this process. That is a separate
helper, not a flag.

### HTTP: loopback is not authorization

The Streamable HTTP transport used to rely on binding to `127.0.0.1`. That keeps
the network out and keeps nothing else out: every process on the machine can
reach loopback, and so can any page in the user's browser, since JavaScript will
happily `fetch("http://127.0.0.1:9331/mcp")` on behalf of whatever site is
loaded. On the other side of that request is full desktop control. Loopback is a
*reachability* boundary and was being used as an *authorization* boundary.

So: one bearer token, compared in constant time, required on `/mcp`. Supplied
via `CUA_HTTP_TOKEN` or generated per run from `/dev/urandom` and printed on
stderr at startup. `/health` stays open — a supervisor needs it before it holds
any credential, and closing it would make "the server is down" and "my token is
wrong" the same observation. The 401 does not distinguish a wrong token from a
missing one.

Both boundaries are kept. The bind check still refuses anything but loopback,
because a token is not a reason to put this on a network.

---

## 8. Manual test checklist

CI has no grants, so these are verified by hand. `cargo test` covers the
permission-free logic: rendering, resolution tiers, window matching, clamping.

**Permissions**
- [ ] `cua-rs permissions` with neither grant → both `false`, no prompt
- [ ] Accessibility only → tree works, screenshot warns, no hard failure
- [ ] launched from a different host app → grants do not carry over

**Coexistence — the point of the project**
- [x] cursor does not move during any action on the AX path (Chrome,
      verified byte-identical `CGEvent(source: nil).location` before and after
      `AXPress`)
- [ ] pid-routed click leaves cursor byte-identical before and after on custom
      controls (0.3.1 live verification pending)
- [x] frontmost app unchanged after `click` (Terminal stayed frontmost)
- [x] tree is identical whether the target is frontmost or occluded — Chrome
      413/413, Finder 4/4, Slack 367/373 (+6 = focus ring). See §5.
- [ ] click in a background window while typing in the foreground: no dropped keystrokes
- [ ] active Space is unchanged
- [ ] target window on another Space still captures

**The widened mouse model (§11)** — all unrun
- [ ] `drag` reorders a row in a real list (Finder, Mail, a Kanban board)
- [ ] `drag` with one end a bare pixel draws a selection rectangle on a canvas
- [ ] a drag whose ends are in two different windows is refused, not delivered
- [ ] a `drag` interrupted mid-move still releases the button (no stuck gesture)
- [ ] `hover` on a Mail message row makes its hover buttons appear *in the tree*
      (a tooltip is a worse test — it may never enter the tree at all, so its
      absence would prove nothing)
- [ ] `hover` on an app that polls `NSEvent.mouseLocation` does nothing — confirm
      the failure is the documented one and not a delivery bug
- [ ] `button: right` opens a context menu on a control advertising no AX actions
- [ ] `modifiers: cmd` on a link opens a new tab; `shift` extends a selection
- [ ] `scroll` on an Electron list moves it, and reports `delivery: pid`
- [ ] `scroll pixels=N` on a native `AXScrollArea` moves `AXValue` by the
      expected fraction — the only check that catches an inverted delta sign
- [ ] `scroll` on a native `AXScrollArea` in pages still reports `delivery: ax`
- [ ] a coordinate cited against an older `snapshot_id` is refused by
      `click_in_window`, `drag` and `hover` alike
- [ ] cursor is byte-identical before and after each of the above

**Keyboard delivery — automated, and no longer a checklist item**

The rows that used to sit here ("a chord reaches the target", "typing lands in
the right field") were the ones a human could not check reliably, because the
failure they were looking for is a keystroke arriving somewhere nobody was
looking. They are now three read-back tests against TextEdit — address a text
element, send keys, read that element's `AXValue` back — plus a negative test
that addresses element A and asserts element B, its sibling in the same window,
did not receive the text. Run them on a machine with an Accessibility grant and
a GUI session:

```console
$ cargo test -p cua-core --test live_keyboard -- --ignored --test-threads=1
running 3 tests
test does_not_type_into_the_other_text_element ... ok
test sends_keys_into_the_addressed_element ... ok
test the_focus_verdict_predicts_where_the_text_lands ... ok
```

They open and close their own scratch documents. `--test-threads=1` is not
optional: they drive one app.

Three things they established, none of which was known before:

- **Keys addressed at a text element arrive in it.** The `focus: verified`
  case is now a measurement rather than a hope, on macOS 26.5 / TextEdit.
- **They do not reach the sibling field.** With the find bar open, text
  addressed at the document turns up in the document exactly once and nowhere
  else in the window.
- **A window that has never been clicked swallows them silently.** TextEdit
  can be frontmost with no key window, publishing no `AXFocusedUIElement` at
  all; pid-routed keys then land nowhere and no error is raised anywhere. That
  is exactly what `focus: unverified` reports, and it is the reason a caller
  should click a target before typing into it. Sending a click first is what
  the tests do.

Two smaller measurements from the same session, recorded because they will
otherwise be rediscovered:

- `type_text_background_pid` carries a Unicode string and arrives verbatim
  under a non-Latin input source; `press_chord_background_pid` carried a
  keycode, so with a Korean source active `press_key x` delivered `ㅌ`. Chords
  went through the input method, literal text did not. **Fixed — see below.**

### A keycode is not a character

`press_key x` delivering `ㅌ` was not a bug in the input method. Under a Korean
2-set source, keycode 7 *is* `ㅌ`: that is the correct answer to "the user pressed
this key", and the wrong answer to "the caller asked for the letter x". The event
was under-specified, and macOS filled the gap the way it should for a person
typing.

The fix is to say both things. `Chord` now carries the literal character the
caller named, and `press_chord_background_pid` attaches it with
`CGEventKeyboardSetUnicodeString` **while keeping the real keycode** — so an app
reading `keyCode` for a game control or a shortcut still sees the physical key it
expects, and an app reading `characters` gets the letter that was asked for.

Both candidate recipes were measured on this machine, with the Korean source
active, through the shipping code path rather than a probe resembling it:

| recipe | delivered |
|---|:--|
| keycode alone (before) | `ㅌ` |
| real keycode + Unicode string | **`x`** |
| keycode 0 + Unicode string | `x` |

The last one is what `type_text_background_pid` does and it works, but it claims a
key was pressed that was not — keycode 0 is `a`. The middle one was chosen because
it is the only one that is true about both the key and the character.

Deliberately not applied to a chord or a named key. `chord.literal` is `None` for
`escape`, `f5`, and anything with a modifier: `cmd+x` means Cut rather than the
letter x, and forcing a character onto it would change what the keystroke is. The
keycode is unchanged in every case, so nothing that worked before behaves
differently — only the character an input method would otherwise have substituted.
- The staleness guard used to reject every click on TextEdit's document view,
  because the tree walk resolves a label from `AXTitle`, `AXDescription`,
  `AXPlaceholderValue`, `AXIdentifier` or a linked title element while the live
  re-read compared only `AXTitle` and `AXValue`. Both sides now call
  `Element::label`.

**Tree**
- [x] native app: Finder 4 elements, Chrome 413 — labeled, actionable elements present
- [x] Electron: Slack 367 elements including an `AXWebArea`, **but not on the
      first read** — see §5. The 400 ms settle is not enough for Slack.
- [ ] relaunch that Electron app; if the pid is reused, the tree is still non-empty
- [ ] 10k-row table: walk returns under `max_nodes` and does not hang
- [ ] wedged / modal app: fails with a timeout, does not hang the server
- [ ] settle time pinned down with a never-before-poked app (currently bounded
      only: >3.2 s, <~1 min, n=2)

**Snapshots** — all three now covered by `crates/cua-mcp/tests/mcp_surface.rs`
or verified live
- [x] `click` with a stale `snapshot_id` → `StaleSnapshot`, nothing pressed
- [x] index out of range → `BadIndex`
- [x] action before any `get_app_state` → `NoSnapshot`

**Resolution**
- [ ] `Slack` resolves despite helper processes
- [ ] genuinely ambiguous name → error naming both candidates
- [ ] bundle id and `/Applications/X.app` both work

**Geometry**
- [ ] Retina: `scale` ≈ 2.0; external 1x display: ≈ 1.0
- [ ] window moved between snapshot and action: index still hits the right control

**Safety gates** — the classification itself is unit-tested and needs no grants;
these are the wiring, which does
- [x] `press_key delete` on a non-text element is refused, and the same call
      with `confirm_destructive: true` goes through (verified live on Terminal)
- [x] `CUA_YIELD_TO_HUMAN=1`: a click on the app the human is typing in is
      refused, naming the idle window; the tap starts and tears down cleanly
      (verified live)
- [ ] `CUA_YIELD_TO_HUMAN=1` with no Accessibility grant → every action refused
      with the "tap unavailable" message, not silently unguarded
- [ ] a click on Keychain Access / 1Password is refused; `get_app_state` on it
      still returns a tree, with the screenshot withheld and a warning
- [ ] `CUA_ALLOW_FORBIDDEN_TARGETS=1` lifts both
- [ ] lock the screen mid-session: the next action is refused, `find` still works
- [ ] HTTP mode: `/mcp` is 401 without the token and works with it; `/health`
      answers either way (covered by an `#[ignore]`d test that binds a port)

## 9. Deliberately not built

| | Why |
|---|:--|
| **pointer-position spoofing** | an app that reads `NSEvent.mouseLocation` rather than the event it was handed cannot be reached by a synthesized `mouseMoved`, and the only fix is warping the real cursor. Permanently out — §11 |
| **press-and-hold gestures** (a drag that pauses to let a spring-loaded folder open; click-and-hold) | the primitives are there — a drag *is* a down, a run of moves and an up — but holding means owning a mouse-down that outlives one tool call, and a release that never arrives leaves the target mid-gesture |
| **wheel momentum and rubber-banding** | a trackpad fling is a phase-tagged stream of events, not a delta. Nothing has needed it |
| AX notification streams (`AXObserver`) | would replace the fingerprint heuristic in §10 |
| ~~menu invocation~~ | **the menu opens, and cua-rs now reports and photographs it — but a coordinate cannot pick a row. §10** |
| reading a pop-up menu's items | it has no accessibility representation to read them from, and cua-rs does no OCR. The caller reads the screenshot |
| parsing key equivalents out of a menu image | the shortcut is the way to activate an item, and recognising `⌥⌘⌫` from pixels well enough to *press* it is a different risk from describing it. The caller reads it |
| Spaces handling | off-Space windows are observation-only, matching prior art |
| per-app leases | needed once two agents share one machine |
| ~~yield-to-human detection~~ | **built, opt-in, `CUA_YIELD_TO_HUMAN=1` — see below** |
| picture-in-picture mirror of the driven window | measured 5–8 fps at 39% of a core on the current capture path; clearing that means `SCStream`, a signed bundle and a second Screen Recording grant — §12 |

### The yield-to-human row changed, and why

That row read "requires watching real input, which needs an event tap", and the
implied conclusion was that an event tap is the same kind of thing as posting to
the shared input stream. It is not, and the distinction is one word in the API.

A tap created with `kCGEventTapOptionListenOnly` is not in the delivery path. It
cannot swallow, delay, rewrite or reorder an event; the callback's return value
is ignored, and macOS forwards the original regardless. cua-rs's tap goes
further and returns every event unchanged anyway, so the code reads the way the
promise does. What this project refuses to do is *write* to the shared cursor
and keyboard — that is what steals focus, that is what makes an agent contend
with the human for one channel, and none of it is what a listen-only tap does.

It is still a policy reversal, because the process now reads a stream it
previously did not open, and a reader who checked "no event tap" as a proxy for
"no interference" deserves to be told the proxy changed rather than to discover
a `CGEventTapCreate` call. So it ships **off by default**, behind
`CUA_YIELD_TO_HUMAN=1`.

What the tap records is a timestamp, and nothing else — not the key, not the
position, not the app. One atomic store, no allocation, no lock, no framework
call, on a thread that is not the single AX worker (it owns its own run loop,
polled in 250 ms slices so teardown is a flag and a join rather than a
cross-thread `CFRunLoopStop` against a `!Send` handle).

The gate's real question — "is the human working in the app I am about to
drive" — is answered at the action boundary instead, by pairing that timestamp
with `NSWorkspace.frontmostApplication`. cua-rs never activates an app, so an
app it is driving is frontmost only because the human put it there; input
arriving while that is true is theirs. Input arriving while something else is
frontmost is the human working elsewhere, which is the case this whole project
exists to run alongside — and the reason the naive "any input at all" rule would
have been unusable.

Two consequences worth stating. The gate clears itself after
`CUA_YIELD_IDLE_MS` (default 3000) of quiet, which is what the refusal tells the
caller to wait for; there is no separate resume call, because a latch with no UI
to clear it is a deadlock. And if the flag is set but the tap cannot be created
— no Accessibility grant, typically — every action is refused rather than
silently unguarded. A yield gate that cannot see is worse than no gate, because
it promises a property it is not providing.

---

## 10. Known weak spots

**`ui_changed` is a heuristic.** It compares focused-element identity and window
title before and after, with a 120 ms settle. It will report `false` for real
changes it cannot see. It is reported honestly rather than optimistically,
because an agent that believes every action landed is worse than one that knows
it should re-read. The right fix is `AXObserver` notifications.

One blind spot this section used to name has been closed, and it is worth
recording how narrowly. "A menu opening in its own window" changed neither the
focused element nor the window title, so the fingerprint was byte-identical and
the action reported `ui_changed: no` while a 202x318 menu sat on screen. The
settle now also enumerates the app's windows and compares the set of transient
ones — anything of that pid, on screen, above ordinary content level, excluding
the menu-bar and status levels — so a pop-up appearing *or* vanishing is
`ui_changed: yes` on its own evidence, and the pop-up's id, level and frame ride
back in the action's own result rather than waiting for the caller to ask. That
costs one window enumeration, p50 ~28 ms, on top of the 120 ms. It does not make
the fingerprint less of a heuristic for anything that happens *inside* one
window.

One failure of it is specific enough to name: the fingerprint is *focused
window title | focused element role | focused element title*, and two text
fields of one window usually share a role and have no title. Text landing in
the wrong one of them therefore leaves the fingerprint byte-identical, so the
action reports `ui_changed: no` while something certainly happened. That
particular blind spot is what the `focus` field on pid-routed keyboard results
covers — it compares the *element*, not a string built from it.

**One snapshot per app.** Driving two windows of the same app alternately
invalidates indices each time. Correct, but awkward; keying snapshots by window
would fix it.

**The approval gate is a heuristic, and heuristics on labels are shallow.**
"Cancel" and "Delete All" are now distinguished (§7a), but only by what the
control says about itself. A button labelled "OK" in a dialog whose *title* says
"Delete 4 items?" is not caught, because the classifier reads the target and not
its context; nor is a menu item whose destructive meaning lives in the row it
was invoked from. Widening it to the ancestor chain is the obvious next step and
would need a rule for how far up to look before every button in a settings
window inherits the word "Reset" from a section header. Until then the gate
stops the labelled cases, which are most of them, and `snapshot_id` plus
`element_token` remain the defence against acting on the wrong element at all.

**Point coordinates are AX-global.** Multi-display setups with negative origins
are untested.

**A browser in the background accepts nothing.** Chrome and Safari each took a
pid-routed click and a pid-routed `mouseMoved` at the same pixel of the same
window and acted on neither while their application was not frontmost, and acted
on both the moment it was. Measured with the `hover_check versus` arm; §11 has
the readings. This is not the hover event and not the pid route — a background
pid-routed ⇧-click on TextEdit selected text in the same session — so the shape
of it is "some apps only process synthesized pointer input while active", with
two browsers as the known members and no idea how large the set is. It is
recorded here rather than in §11 because it bounds *what can be driven*, which
is a different question from which gesture is expressible. The synthesized
activation notices of §6 do not buy past it; nothing tried does, short of a real
activation, which is out of scope.

**A target is left believing it is active.** The `ApplicationActivated` notice is
not revoked after the click (§6), because revoking it broke the following click.
Nothing verifies that the belief ever clears: macOS will not deactivate an app it
never considered active, so it may persist until the app is genuinely activated.
The observable consequences are app-specific — a view that only draws its
selection highlight while active, a toolbar that stays enabled — and the sharp
edge is an app whose own activation handler calls `activateIgnoringOtherApps:`,
which would turn the notice into a real raise. Releasing the belief on a session
boundary rather than per click is the shape of the fix, and it is not built.

**Menu-opening controls: the click lands, the menu is now visible, and a
coordinate still cannot pick a row.** This section used to say "solved", and it
was wrong in a specific and instructive way, so the correction is kept rather
than the claim.

Two real bugs were fixed and are still fixed:

- `is_plausible_target()` required `layer == 0`, which excluded the app's own
  floating chat windows at layer 3. The cap is now 3.
- An `ApplicationDeactivated` notice was sent after *every* click, which
  destroyed the key-window state the next click depended on. It is gone by
  default, and `CUA_DEACTIVATE_AFTER_CLICK=1` restores it for comparison.

Ruled out along the way, each by measurement rather than argument: the
coordinate; the AppKit event header; the timestamp; the ordering of the focus
notices; whether the target believes it is frontmost (`AXFrontmost` does flip, in
about 150 ms); and the private versus public per-pid post route.

What was wrong was the sentence that followed: "its items are readable in the
tree". They are not, and the `AXMenu`/`AXMenuItem` paragraph that used to be here
described an object that is not in the tree at all. Re-measured on KakaoTalk's
chat-room hamburger (`[7]`, an `AXButton` advertising **zero** AX actions —
`AXShowMenu` returns "this element supports []"):

1. **The click lands and a menu opens.** A new window of the same pid appears
   within ~50 ms: level 101, 202x318, on screen, no title. It persists — polled
   every 20 ms for 2.5 s, present throughout. No activation is needed; the
   frontmost app was a terminal the whole time.
2. **Accessibility cannot see it.** The application element has only its two
   `AXMenuBar` children. `AXUIElementCopyElementAtPosition` at a point *inside*
   the menu returns the `AXMenuBar` at `0,0 1512x33` — a fallback for a point
   outside every frame it knows about. There is no `AXMenu` and no `AXMenuItem`
   anywhere. The pop-up is a CGWindow with no accessibility representation.
3. **Pixels can see it.** `screencapture -x -o -l<id>` renders it legibly.
4. **The menu is drawn at the real cursor, not at the control.** The button is at
   `(949,145)`; the menu appeared at `(677,572)` with the pointer at
   `(678.3,574.5)`, and moved to `(982,599)` after the pointer moved. Cosmetic in
   itself, and a strong hint about what follows.
5. **A pid-routed click inside the menu delivers but selects nothing.** Measured
   twice, on two rows: the menu closes and no item acts. The menu's own state is
   unchanged afterwards — including on a run where the human's pointer was
   hovering a *different* row, so it does not pick the wrong item; it picks none.
6. **The item's keyboard shortcut does work.** `press_key cmd+t` against the app
   with that menu open activated `항상 위에 유지`: the chat window moved from
   level 0 to level 3, and a second `cmd+t` moved it back.

Point 5 with point 4 is the finding. A macOS menu tracks the *pointer* to decide
what is highlighted, and cua-rs does not move the pointer — so menu-row selection
by coordinate is the same impossibility as §9's pointer-position spoofing, not a
tuning problem. Nothing in the widened mouse model changes that, and a caller
told to click a menu row would be told to do something measured not to work.

What ships instead is observation plus the keyboard:

- Every `get_app_state`, and **every action's own result**, lists the app's
  transient windows: id, level, frame, and whether each one appeared while the
  action ran. In the action's own response deliberately — a caller told one round
  trip later has already concluded the control did nothing, which is exactly the
  wrong conclusion and exactly the one users drew.
- `is_addressable_target()` widens what a pid-routed event may be stamped with,
  so a pop-up *can* be aimed at: right for a popover or panel with ordinary
  event-driven views, right for dismissing a menu, and honestly labelled as not
  the way to pick a menu row. `is_plausible_target()` is deliberately left
  capped at level 3, because it answers the different question of which window a
  snapshot is *of*, and a menu chosen there would have its number stamped onto
  clicks meant for content — the §6 failure that the cap was raised to fix.
- The tool descriptions for `press_key` and `click_in_window` say which is which,
  so the path that works is tried first.

Not built, on purpose: reading the menu's items, and recognising their key
equivalents, from the image. The shortcut is what activates an item, and a
misread `⌥⌘⌫` presses "leave the chat room". Describing pixels and *acting* on a
guess about pixels are different risks, and cua-rs does the first by handing over
the screenshot and refuses the second.

The capture question resolved itself in passing, and not the way it looked.
`screencapture -l<menu_id>` succeeds, but it does not return the menu: asking for
the menu's id and asking for its parent window's id returned **the same
2188x1662 image**, covering the union of the parent at `46,86 924x770` and the
menu at `938,599 202x318` — exactly 1094x831 points at 2x. macOS photographs a
window together with the pop-up attached to it. So a pop-up needs no capture of
its own, and there is no second image to order; what it needed was for
`WindowShot` to stop assuming the image covers the requested window's frame,
which had `scale` reporting 2.37 px/pt for the parent and 10.83 for the menu
instead of 2.0 — every pixel-to-point conversion against that image wrong for as
long as a menu was up. The extent is now recovered by testing candidate rects
against the pixel count and keeping the one whose horizontal and vertical
px-per-point agree, and both the covered rect and the window's own frame are
reported.

**Both per-pid keyboard functions are wired in now.**
`press_chord_background_pid` and `type_text_background_pid` are built the same
way — `CGEventCreateKeyboardEvent` against a `HIDSystemState` source,
`CGEventKeyboardSetUnicodeString` for characters with no keycode, posted
per-pid — which invalidates this crate's founding assumption that keyboard
input must go through the global tap and steal focus. `press_key` calls the
former as its only tier (§1a), because accessibility cannot express a chord at
all: there is `AXConfirm` and `AXCancel` and then nothing. `type_text` calls
the latter only when a caller asks for `mechanism: "keystrokes"`, which stays
opt-in for the reason §1a gives: a bulk text write is the one operation
accessibility expresses *better*, one atomic element-addressed `AXValue` write
against a long stream of events landing one character at a time. The events
path exists because on a terminal or a canvas editor the better mechanism does
nothing at all.

**The measurement this paragraph used to demand has been taken.** §8 has the
command and the results: keys addressed at a text element arrive in it, they do
not arrive in its sibling, and a window that has never been clicked swallows
them silently while reporting `focus: unverified`. What remains unproven is
breadth, not the mechanism — TextEdit on macOS 26.5 is one target, and an app
you have not tried is still an app you have not tried. `CUA_KEY_AX_ONLY=1`
gets the old AX-verb-only path back.

**A keystroke can still be misdelivered, and the bound on that is worth stating
precisely.** It goes to whatever the target process's own first responder is;
cua-rs best-effort-focuses the addressed element first (`AXFocused`, which is
not settable on every element) and now reads the app's own
`AXFocusedUIElement` back to say whether that worked — `focus: verified |
unverified | mismatched` on every pid-routed keyboard result. Because the post
is addressed to a pid, a miss can only land on another element **of the same
process**; it cannot reach the human's foreground app when that is a different
process, which is precisely what the shared HID tap this crate refuses to use
would do. An earlier version of this section said a miss "types into whatever
the human was editing", which was too strong. The risk is real and bounded to
"the human and the agent are in the same app at the same time" — a condition a
caller can test, and one `CUA_KEY_STRICT_FOCUS=1` refuses to deliver into.

What is still not solved is the `unverified` case: an app that publishes no
focused element gives cua-rs nothing to check against, and the honest report is
that nothing is known. `AXObserver` notifications would be the real fix here as
they would be for `ui_changed`.

`post_chord` remains the unreachable, focus-stealing shared-input path. Its
last stated reason to exist — being the only way to type real keystrokes into a
terminal — is gone with `mechanism: "keystrokes"`, and §11 says it should be
deleted.

---

## 11. Widening the pid tier, never the shared tier

The gaps in §1's capability table — no chords, no drag, no canvas, one button,
no modifiers, no hover, no scrolling anything Chromium draws — read like
consequences of choosing accessibility. Almost none of them are. They were
consequences of `Target::Point` requiring an element, of two written-but-unused
functions in `cua-hid`, and of one primitive that happened to hard-code a left
click. The shared-input tier stays permanently closed; what follows widens the
*pid* tier instead, and none of it needs the cursor, the global HID queue, or
`NSRunningApplication.activate`.

The enabling fact: `PidClick` was `{pid, point, window_local, wid, count}` and
is now `{…, button, modifiers}`, with a sibling `PidDrag`, `PidMouseMove` and
`PidScroll`. There is no `Element` in any of them. Accessibility is how cua-rs
*decides where* to click; it is not how the click is delivered, and the delivery
path never needed an element to exist.

### The tiers

| | before | now |
|---|:--|:--|
| element with an AX action | `AXPress`/`AXPick`/`AXConfirm` | **pid click at its point — see §1a; `CUA_AX_FIRST=1` restores this row** |
| element with a frame, no action | pid click at its point | unchanged |
| **point with no element** | refused (`NoElementAtPoint`) | **`click_in_window`, window-scoped and opt-in** |
| drag | refused | **`drag`: a down, interpolated `mouseDragged` moves, and an up, all pinned to one window** |
| chords | refused | **pid-routed via `press_chord_background_pid` — see §1a; `CUA_KEY_AX_ONLY=1` restores the refusal** |
| right / middle button | left only | **`button` on `click`, `click_in_window` and `drag`** |
| modifier click (⌘, ⇧, ⌥, ⌃) | not expressible | **`modifiers` on the same three tools, in the vocabulary `press_key` already used** |
| hover | not expressible | **`hover`: one synthesized `mouseMoved`, nothing pressed** |
| pixel-precise scroll | `AXScroll*ByPage` only | **`scroll` keeps the AX page verb where the element advertises one, and sends a `scrollWheel` event where it does not** |

The bottom half of that table is new, and the first row moved after this
section was originally written —
§1a has the reasoning (accessibility cannot express a click count or a chord, so
events are the only mechanism that could serve either) and the environment
variables that undo it per action. The rest of this section's argument (pid delivery needs a point, not
an element) is unaffected: it explains *why* the pid tier could reach further
than accessibility-only, not which actions choose it by default.

Shipped in tiers rather than at once. The elementless click went first — only a
policy gate stood in its way — then chords, then the widened mouse model, each
because the earlier one's safety question was answerable on its own: a drag
needed a story for the mouse-up that must be sent even when a move fails
partway, and a chord needed one for a keystroke that lands in the wrong process.
Bundling them would have delayed the finished piece and made each one's review
harder to read.

### Point with no element — the one that unlocked canvas

An agent that reads a screenshot has a pixel, not an element. That was a dead
end by policy, not by capability. `click_in_window` is the distinct opt-in, never
a silent fallback from `click`, because "the point covers nothing" is exactly the
shape of a typo and blind-clicking a typo is the worst outcome in this project.

Coordinates are **window-local points**, measured from the window's top-left
corner — the screenshot's own space, divided by the `px per point` scale
`get_app_state` reports. Screen coordinates were the obvious alternative and are
worse in the way that matters: the caller would have to add the window origin
itself, and the sum would silently address the wrong pixel the moment the user
moved the window between the read and the click. Window-local coordinates are
re-anchored to the live origin immediately before posting, so a window move
between the two calls is harmless instead of invisible. It also means this path
consults no snapshot geometry, and so has no reason to reject an `acted_on`
snapshot: there is no element whose position could have gone stale.

Three gates, checked in order, none of them advisory:

1. the caller names a `window_id`, and it must be the one this app's most recent
   `get_app_state` read. Without an element the window is the whole of the
   addressing, and an id from anywhere else is an id whose contents the caller
   has never seen. `get_app_state` now prints `window_id=` in its header for
   exactly this purpose;
2. that window still exists, still belongs to this pid, and is still an ordinary
   application window — re-enumerated here rather than trusted from the
   snapshot, because a pid-addressed event carrying a stale or recycled window id
   is precisely the thing that must not be sent;
3. the offset lands inside the window's *live* frame. Negative offsets are
   refused outright, since window-local coordinates cannot be negative, and an
   offset past the window's width would otherwise be a perfectly valid screen
   point over the window next door.

The result is labelled `delivery: pid (no element)` — a distinct label, not a
footnote on `pid`, because the difference is not the mechanism but what the
result can be trusted to mean. Every other delivery mode resolved an element
first and so names something accessibility agreed was there. This one names a
pixel the caller chose.

What it cannot do is verify. There is no element to inspect afterwards, so the
post-action delta is the only feedback, and on a canvas even that is empty. The
tool description says so plainly: aiming is the caller's job, and cua-rs is only
promising the event reached that pixel of that window.

Measured on KakaoTalk's chat-list filter chips, background, with Terminal
frontmost throughout: all three gates refused as specified, and the accepted
click switched the filter to 안읽음 and back. The window's traffic lights went
from grey to coloured across the click, which is the app agreeing it became key.
`ui_changed` reported `no` both times — the fingerprint heuristic sees neither a
title nor a focus change here — which is the §10 false negative doing exactly
what §10 says it does, and the reason `return_state` exists.

### The mouse model, widened

`click_background_pid` used to take a point and a count and always send a left
click. The primitive now takes `{origin, destination, button, modifiers,
click_count}`, and the four capabilities below are what falls out of that one
generalization rather than four separate mechanisms. All of them go out through
the same recipe §6 describes — `NSEvent` factory first, `CGEvent` taken from it,
stamped, posted once by pid — so none of them widens *how* input is delivered.

**Button.** Three `NSEventType` families, selected by button:
`leftMouseDown`/`Dragged`/`Up`, the `right` equivalents, and `otherMouse*` for
middle. The type is what selects the handler — a view implementing
`rightMouseDown:` will never see a `leftMouseDown` however the button-number
field is stamped — so the type comes from the button and the stamped
`kCGMouseEventButtonNumber` merely agrees with it.

A right click is *not* routed to `AXShowMenu`. Where an element advertises that
action, `perform_secondary_action` already reaches it and is the better call:
no coordinate, no window pinning, no aim to get wrong. The controls that need a
real right click are precisely the ones that advertise no actions at all, which
is the same population `click` needed the pid tier for in the first place.

**Modifiers.** On macOS a held modifier is not a garnish on a click, it is a
*different command*. ⌘-click opens a link in a new background tab in Safari and
every Chromium browser, and opens a Finder item in a new window. ⇧-click extends
a selection from the anchor to the clicked row in every list, table and text
view AppKit ships. ⌥-click reveals the alternate item in a great many menus,
expands an entire outline subtree in Finder, and duplicates instead of moving in
a drag. An agent that can only send a plain click cannot select a range of
messages, cannot open a link without navigating away from the page it is
reading, and has to reach every alternate action through a context menu that may
not exist. None of that is an accessibility problem — it is one bit of event
state that the primitive had no field for.

`parse_modifiers` shares its table with `parse_chord`, so `cmd`,
`command`, `meta` and `super` mean the same thing on a click as they do in
`press_key` and neither can grow an alias the other lacks. The flags go into the
`NSEvent` factory *and* onto the `CGEvent` afterwards, because
`-[NSEvent modifierFlags]` reads the AppKit header while anything Chromium-based
reads `CGEventGetFlags`; the two enums share their bit values, so one set
satisfies both. Accessibility has nothing to say here at all: `AXPress` means
"activate this" and has no room for a held key, which is why ⌘-click was
unreachable rather than merely awkward.

**Drag.** A down, a run of `mouseDragged`, an up, all carrying one event number
— AppKit correlates a tracking session by that field, so allocating a fresh one
per move would hand a view a stream of unrelated single events instead of one
gesture. Both endpoints are checked against the *live* frame of the one window
the app's last `get_app_state` read; a request whose ends fall in different
windows is refused rather than interpolated across the boundary, because every
event of a gesture has to carry the same window number and a drag whose up
lands elsewhere is not a drag anywhere. The release is sent even when a move
fails partway, and the first error is what surfaces: a lost mouse-up leaves the
target mid-drag, which is worse than the failure that caused it.

Either end may be an element or a bare window-local pixel, and they may be
different elements of the same app. That is a deliberate softening of the rule
`click` follows — `click` keeps its elementless form in a separate opt-in tool
because "the point covers nothing" is the shape of a typo and blind-clicking a
typo is the worst outcome available. A drag frequently has one end on a real row
and the other on empty space by design: a reorder into a gap, a rectangle drawn
across background. Where either end is a pixel the whole result is labelled
`pid (no element)`.

*The interpolation, and the numbers.* A down at A followed immediately by an up
at B is not a drag to anything that implements one: AppKit drag sources arm on a
`mouseDragged` that exceeds a small threshold and then track each subsequent
move, and a web view reconstructs the gesture from `mousemove` events it never
receives. So the moves are interpolated, and two constants say how:

- **24 points per step**, roughly one list row, so no ordinarily-sized drop
  target is stepped clean over without a move landing inside it. The step
  *length* is what is held constant; the step *count* falls out of the distance,
  floored at 6 so a short drag does not degenerate back into a single jump and
  capped at 32 so a long one cannot run for seconds.
- **16 ms between steps**, one display frame at 60 Hz. A target redraws at most
  once per frame, so moves sent faster are moves it cannot act on separately and
  may coalesce; moves sent slower make an ordinary drag take visibly longer than
  a human one, which is what starts a list's drag-autoscroll timer.

Both are reasoned rather than measured, and both are one constant each in
`cua-hid` if a real app disagrees.

**Hover.** A surprising amount of macOS UI does not exist until the pointer is
over it, and none of it is in the accessibility tree beforehand: the delete and
archive buttons on a Mail message row, a Finder column's expand triangle, a
tooltip that carries the only full text of a truncated label, a menu bar item's
submenu, the value readout on a chart. An agent reading the tree sees a row with
no buttons on it and concludes correctly that there is nothing to press —
because at that moment there is not. Without a way to move the pointer, that UI
is not merely hard to reach, it is invisible, and no amount of better tree
walking finds it.

So: one `mouseMoved` at a point, no press, and — unlike a click — no
activation-assist click on the window's own activation point, because a caller
asking to hover has not asked to press anything. The activation *notices* are
still sent. `return_state` is on by default here more meaningfully than
anywhere else: the post-action diff *is* the result, since what appeared is the
entire point of the call.

This is the one capability with a limit that is not a missing feature. The event
says the pointer arrived; the pointer did not. An app reading
`NSEvent.mouseLocation`, `-[NSWindow mouseLocationOutsideOfEventStream]`, or
polling the cursor gets the truth — the human's pointer, wherever they left it —
and does not respond. Anything driven by the event (`NSTrackingArea`, every web
view) does. The only fix would be moving the real cursor, which is the thing
this project exists not to do, so the tool description says so plainly instead.

**Scroll.** Two tiers, and the older one is kept because it is genuinely better
where it applies. `AXScroll*ByPage` needs no coordinate, lets the app decide what
a page of its own content is, and cannot be swallowed by whatever subview
happens to sit under a point. But it only exists on elements that advertise it,
and the elements an agent most often needs to scroll advertise nothing at all.
Measured populations from §5's own numbers: Slack's 367 elements are mostly
inside an `AXWebArea`, Chrome's 413 likewise, and a web area publishes no
`AXScroll*ByPage`. A custom-drawn list, a map, a chart, a code editor's text
view — same. So the single most common thing an agent wants to do to a
long-running app's main content pane was a refusal rather than a mechanism, and
"scroll down to see the rest" is not an exotic request. So: a page request on an element that
advertises the verb uses it and reports `delivery: ax`; anything else is a
`scrollWheel` event at the element's point and reports `delivery: pid`, with the
verb line naming which and why.

A request in *points* always takes the event tier, because accessibility cannot
express a distance — it has whole pages and nothing else. On the event tier a
"page" is 90% of the element's own height, which keeps about a line of overlap
across the boundary the way a real page-down does, clamped at both ends so an
element that reports no frame still scrolls by something usable.

*Unlike a mouse event, a scroll event is not built through AppKit* — there is no
`NSEvent` scroll-wheel factory to build it with. That costs nothing: the AppKit
header the mouse path goes out of its way to obtain (event number, click count,
window number) is what a view validates a *click* against, while
`-[NSEvent scrollingDeltaY]` reads the CG record's own wheel fields, which
`CGEventCreateScrollWheelEvent2` fills correctly. The window routing, the pid
stamp and the fresh timestamp are applied exactly as they are to a mouse event.

### A coordinate has to cite the state it was chosen from

Everything in this section is coordinate-first. A drag is two points, a hover is
one, and the wheel tier aims at a point even when it started from an element —
so the weakest guard in the system became the most load-bearing one at exactly
the moment this shipped.

The staleness argument in §3 was made about *indices*, and the mechanism that
came out of it — `snapshot_id`, generational, mismatch is a hard error — was
only ever applied to them. Points were addressed two other ways and neither
carried it. `Target::Point` (a screen coordinate, hit-tested against the
snapshot's frames) accepted a `snapshot_id` and **ignored it**, which is worse
than not offering the field: the guard reads as present and is not.
`click_in_window` checked that the window id came from this app's most recent
read, which proves the caller is aiming at a window it has *seen* and says
nothing about the *state* it saw — a window id survives any number of re-reads,
so a pixel picked off screenshot 3 was accepted verbatim against snapshot 9.

Why that is worse for a pixel than for an index, and not merely equally bad: a
stale index can often be caught by something. The role changed, or the text the
element used to show is gone — §3's `TokenRoleMismatch` and `TargetChanged` are
both built on that. A stale coordinate offers nothing to catch. The point is
still inside the window, it still names a real place, and the place is still
occupied; it is simply occupied by something else now. There is no inspection
that distinguishes a good point from a bad one, so the generation number is not
a convenience here, it is the only available evidence.

So every coordinate-addressed action — `click` with `x`/`y`, `click_in_window`,
and both ends of a `drag`, and a `hover` destination — now honours
`snapshot_id`, and `WindowPixel` carries the pixel and the generation as one
value so the two cannot be passed separately and one of them forgotten again.
It stays opt-in, like the index guard and for the same reason: the overwhelming
flow is read-then-act inside one turn, and requiring it would add a failure mode
where there is no risk. What changed is that a caller who supplies it now gets
what it says.

### Every action still returns a result

Worth stating as an invariant rather than leaving it as an accident of how these
were written: none of the new actions is fire-and-forget. `drag`, `hover` and
the wheel tier of `scroll` each return the same `ActionResult` a click does —
the verb that ran (naming the tier, the button, the modifiers, the interpolated
step count), what was aimed at, `delivery`, `ui_changed`, and by default the
post-action tree diff.

That matters most for exactly the actions added here, because they are the least
verifiable ones. A click on a named element can be confirmed by re-reading that
element. A drag onto a pixel and a hover over one have no element to re-read,
so the diff is the only feedback that exists — and for a hover it is not
feedback about the action, it *is* the action's output, since the revealed UI is
what was wanted. An action here that returned nothing would leave the caller
with a blind retry loop over a gesture that may have already taken effect, which
is the failure mode §10 says is worse than reporting a timeout.

### What the widened model was measured to do, and the one thing it does not

The experiments the next section asks for have now been run. Three of the five
capabilities work, one is fixed, and **the wheel tier does not scroll anything**.
Recorded here rather than in §8's checklist because two of the results changed
the code and one changed what the tool descriptions may claim.

| | measured | evidence |
|---|:-:|:--|
| `drag` | **works** | dragging 220 points along a line of TextEdit left `AXSelectedText = "The quick brown fox jumps over th"`. A string the app computed from where it believes the gesture went, so nothing but a tracked press-move-release produces it |
| ⇧-click | **works** | a plain `click_in_window` planted the caret, then a `shift` click 200 points along left `AXSelectedText = "The quick brown fox jumps over"`. An unmodified click leaves the selection empty, so the selection *is* the modifier arriving |
| right click | **works** | on TextEdit's text view: a new window of the same pid, level 101, 181x342, within 50 ms. Unambiguous because the element it was aimed at is not a menu and nothing else opens one |
| `hover` | **works on web content, does nothing on the AppKit surfaces tried** | a hover-only `AXButton` appears in the tree while the point is hovered and is gone when the pointer leaves, and the page reads back the exact coordinate the event carried. Measured in two engines. A Finder list row, with a click as the control arm, showed nothing at all. See below |
| wheel tier | **does not work** | see below |

**`hover` drives web content, and nothing else that has been tried.** The
experiment this section asked for has been run, with one change: a Mail row was
not available, so the tree-visible hover state was built rather than found. A
local page publishes three rows, each with a button that CSS keeps at
`display: none` until the row is hovered, and a paragraph that a `mousemove`
listener rewrites with the coordinate it was handed (`assets/hover_fixture.html`,
kept so the reading can be re-taken). The first is the signal an
agent could act on — a hidden button is not in the accessibility tree and a shown
one is. The second is stronger than any hover state could be, because a
coordinate the app *computed* cannot be produced by anything except the event
arriving with that coordinate in it.

```console
$ hover_check versus "Google Chrome" 45        # Chrome frontmost
both arms aim at window-local (450,189)
click arm = SkyLight pid-routed 1-click (left) at window-local (450, 189)
hover arm = SkyLight pid-routed mouseMoved to (650, 222)
  + [55] AXStaticText = "mousemove #5 at 450,102"     # the click's own primer
  + [56] AXStaticText = "mousedown #1 at 450,102"
  + [55] AXStaticText = "mousemove #6 at 450,102"     # the hover
==> BOTH LAND
```

`450,102` is `450,189` less the 87 points of browser chrome above the page. The
page was told the pointer arrived at exactly the pixel cua-rs aimed at. Safari
reproduces it — aimed at window-local `(662,234)`, the page logged
`mousemove #3 at 662,102` — so this is not one engine's quirk, and the
`hover_check sweep` arm shows the revealed control appearing and withdrawing:
`[42] AXButton "HOVER-REVEALED-ONE"` is present in the tree at the target stop
and absent at the dull-point stops on either side of it.

**A Finder list row is not moved by the same event.** Same probe, same run
structure, Finder frontmost, both arms at one pixel of one row:

```console
$ hover_check versus Finder 92
pixel signal usable = true (idle 395233 -> 395233 bytes)
click arm  -> the row reads (selected), pixels 395233 -> 395052
hover arm  -> tree 524 lines -> 524 lines, 0 gone, 0 new; pixels 395052 -> 395052
==> the CLICK LANDS AND THE HOVER DOES NOT
```

Reproduced on a second row. This is the negative case the section wanted handled
carefully, and the two signals answer the two questions it splits into. The tree
did not change, so accessibility saw no hover state — and the window's own image
did not change *by a single byte*, so the app did not draw one either. It is
therefore not a hover that happened where cua-rs cannot see it; nothing happened.
The click at the same pixel of the same window in the same run is what rules out
the routing, the window number, the aim and the instrument, exactly as
`scroll_check`'s `key` arm does for the wheel tier.

Why the two surfaces differ is a **hypothesis, not a measurement**: AppKit's
hover affordances are `NSTrackingArea` crossings, and a crossing is computed by
the window server from where the *real* pointer is, not from a `mouseMoved` a
process was handed — while a web engine recomputes `:hover` from the event it
receives. That would make the split permanent rather than a bug, and it fits
every reading here, but nothing above proves it. What is measured is the split
itself, on Chrome, Safari, Finder and KakaoTalk.

**A browser accepts nothing at all while it is not frontmost, and that is not
about hover.** In the same session, with Finder frontmost, the click arm and the
hover arm both produced nothing on Chrome and on Safari — no `mousedown`, no
`mousemove`, no pixel change. A pid-routed ⇧-click on TextEdit, in the
background, in the same session, selected `"alpha bravo charlie"` as §11 already
records. So the pid route is healthy for background apps and these two browsers
are the exception; §10 carries it as a limitation of what can be driven, not of
which gesture is used. It is also the reason the probe has a click arm at all:
without it, four separate runs would have been recorded as "hover does not work"
when what they showed was "nothing reaches this app right now".

**The wheel tier delivers and scrolls nothing.** Measured against the window's
own pixels, captured before and after:

```console
$ scroll_check wheel TextEdit 1     # AXScrollArea, 400-line document
image 208189 bytes -> 208189 bytes        ==> NO
$ scroll_check key   TextEdit 8     # control, same window, same session
image 208189 bytes -> 206656 bytes        ==> YES
```

The control arm is what makes that mean something. A pid-routed `pagedown`
*keystroke* scrolls the same window in the same run, so the failure is not pid
routing, not the window number, not the aim point, and not the instrument — it is
the scroll event itself. Also measured and also negative: Chromium web content,
and both `ScrollUnit::Pixel` and `ScrollUnit::Line` on both apps. The most likely
explanation is §6's finding applied to a different event family: a `CGEvent`
synthesized from scratch has no AppKit identity, a click can be given one by
building the `NSEvent` first, and `NSEvent` offers no scroll-wheel factory to
build from.

So the tier **refuses by default**. Documenting a tier as unreliable and then
delivering it anyway is the worst of the options: the caller is told
`delivery: pid`, concludes the scroll happened, and reads a stale tree as the new
state — a wrong belief is more expensive than an error, which is the same
reasoning `ui_changed` is reported honestly for in §10. `scroll` now returns a
refusal that names the measurement and points at `press_key` with `pagedown` /
`pageup` / `down` / `up`, which is measured above to reach a scroller that
publishes no scroll action.

The mechanism stays in the tree rather than being deleted, because it is
*correctly built* — the failure is in what macOS does with a scroll event that
has no AppKit identity, not in the construction — and an app that reads the event
record directly may yet accept it. `CUA_WHEEL_SCROLL=1` re-enables delivery so
the next person can re-run the experiment without reverting a commit. It is the
only switch in this file that turns on something known not to work, and it is
named for what it does rather than for a policy.

### Chromium's activation point is a lie, and it aimed every event at a corner

Found while running the experiments above, and the more expensive of the two bugs
they turned up.

`element_point()` asked `AXActivationPoint` first and the frame centre second,
which is right in principle: the activation point is the app's own answer, and for
a wide list row or a control with a large transparent hit area it is a better
point than the middle. Chromium answers `(0, 982)` — for every element in the
window. Measured on Chrome, a button whose frame is `15,239 194x34` reports that
corner as its activation point, and so does the web area containing it.

Every pid-routed click, hover, drag and wheel event aimed at a Chrome element
therefore went to one corner of the display. Nothing errored. `screen_point_inside`
was satisfied because the corner *is* inside the browser window, the event was
delivered, and the result said `delivery: pid` — the shape of failure this project
is most exposed to, since `click` became pid-only in §1a and no longer has an
`AXPress` to succeed behind the scenes.

The rule that catches it needs no app-specific knowledge and no allow-list of
misbehaving toolkits: **a point that activates an element has to be on that
element**, so an activation point outside its own element's frame is
self-contradictory and is discarded in favour of the frame centre. An element
with no frame keeps the benefit of the doubt, because then there is nothing better
to use. Verified: the same `hover` that reported `(0, 982)` now reports
`(112, 256)`, which is the button.

The general lesson is the one §2 and §6 keep teaching in different costumes. An
app's own answer is better than a computed one *when it is an answer*, and a
sanity check against geometry the app also published is cheap. Two coordinate
bugs in this release were of exactly this shape — this one, and a capture whose
`scale` was computed against a frame the image did not actually cover.

### A scrollable element's frame is not its viewport

The second aiming bug, in the wheel tier specifically. The aim came from the
element's own point, and an `AXWebArea`'s frame is the whole document while a long
list's frame covers every row — so the centre of either can sit far outside the
window that shows it. On Chrome that put the aim at the bottom edge of the
display.

Two changes. The point is pulled back into the intersection of the element's frame
and the window's, so a tall container is aimed at the part of it a person can see;
and a caller who named a coordinate now has it honoured instead of discarded.
`Target::Point` used to be resolved only to find out *which* element covers the
point, after which the element's own point replaced it — meaning a caller who said
exactly where to scroll was scrolled somewhere else. Neither change makes the
wheel tier work, per the measurement above. Both were wrong independently of it.

### What the widened model has not been shown to do

Everything above is reachable and none of it is measured on a real app. The
permission-free tests cover what can be tested without a grant — the modifier
and button vocabularies, the interpolation's endpoints and monotonicity, the
step-count bounds, the wheel-delta signs, the scroll tier choice, the page
sizing, the coordinate generation guard, and every argument-validation refusal.
That is a statement about the logic. It is not evidence that any app accepts the
events, and it should not be read as one.

`hover` used to carry the strongest warning in this file: nobody had seen a
synthesized per-pid `mouseMoved` drive anything, there was no corroboration
available anywhere, and the construction was reasoning rather than measurement.
That has been settled — see the measurement above. A synthesized per-pid
`mouseMoved` does drive an app, and the app reads back the exact coordinate it
carried. What replaces the warning is narrower and still worth distrusting: it
has only ever been *seen* to work on web content, an AppKit list row measurably
ignores it, and how many of the AppKit surfaces an agent cares about fall on
which side of that line is unknown. Two apps is not a survey.

The wheel tier used to carry the same warning. It has now been measured, and the
answer was no; see the section above. That is the outcome this paragraph called a
plausible one, and it is worth leaving the prediction next to the result.

What would prove each, concretely:

| | the experiment |
|---|:--|
| `hover` | **run** — `hover_check`, against a page whose rows reveal a button on `:hover` and whose script writes the event's own coordinate into the document. Both signals arrived, in two engines. What is still owed is the *population*: which AppKit surfaces respond, if any, given that a Finder row measurably does not |
| wheel tier | anything with a readable scroll position. A native `AXScrollArea` publishes `AXValue` as a 0–1 fraction — scroll it by an exact `pixels` amount and read the value back. That also cross-checks the delta *sign*, which is the easiest thing here to have backwards and is invisible on a symmetric list |
| `drag` | Finder's icon view, which persists icon positions: drag one icon and re-read its `AXPosition`. A list reorder is the more useful case but a worse first test, since a failed reorder and a refused drag look identical |
| right click | a control with no AX actions at all — §10's chat-app hamburger is the standing example — where a menu appearing is unambiguous, because there is no `AXShowMenu` that could have done it |
| ⌘-click | a browser link, where the tab count before and after is a count rather than a judgement |
| all of them | `CGEvent(source: nil).location` byte-identical before and after, which is the §8 coexistence check the whole project rests on |

Only the first two have been run. §8's checklist carries the rest so that stays
visible.

### Chords and literal text — both wired, both verified

This section used to say `press_chord_background_pid` and
`type_text_background_pid` existed and nothing called them, and that turning
them on was a verification problem rather than an implementation one. That was
right, and the verification has now been done the way this paragraph asked for
it: against a target whose text can be read back, so a miss is provable rather
than assumed. §8 has the command and the results, §10 the residual risk.

`press_key` routes every chord through the pid tier (§1a). `type_text` reaches
the same tier on request, through `mechanism: "keystrokes"`, and defaults to
the `AXValue` write. Every pid-routed keyboard result carries
`focus: verified | unverified | mismatched`, compared against the app's own
`AXFocusedUIElement` rather than the fingerprint string, and
`CUA_KEY_STRICT_FOCUS=1` refuses to deliver on `mismatched`.

The asymmetry that kept these gated — a click that misses does nothing, a
keystroke that misses types somewhere — is still the reason the keyboard path
is the one that reports a focus verdict while the click path does not. What
changed is that "somewhere" turned out to be bounded: the event is addressed to
a pid, so a miss stays inside the target process.

### What stays closed

`post_chord` remains unreachable from the server. It exists so the shared-input
keyboard path is visible in one file and can be reasoned about. Its last
argument for existing — being the only way to type real keystrokes into a
terminal — is gone now that `type_text` can do it per-pid, so it should be
deleted; it is left in this change only because deleting it is a separate edit
from the one that made it redundant.

Its mouse counterpart is already gone. `click_by_moving_pointer` warped the real
pointer, clicked through the shared HID stream, and warped back; every control it
existed for is now reachable by the elementless pid click, so keeping a working
pointer warp in the tree was leaving a temptation rather than a fallback. Its
deletion took the last `CGWarpMouseCursorPosition` reference with it, and that
absence is now the check: nothing in the workspace imports the only API that can
move the user's cursor, so no edit elsewhere can reintroduce a warp without
adding the import back first.

Global HID delivery and cursor warping are not planned, not gated behind a flag,
and not a future option: they are the thing this project exists to not do.

---

## 12. Picture-in-picture: mirroring the driven window

A spike, not a plan. §11 is what is planned next; this is the strongest
alternative to it that was considered and is currently declined, with the
measurements that decided it.

It lives in this file rather than a `docs/` tree because §2 is already the same
genre — a rejected capture alternative argued with head-to-head numbers — and
because a reader asking "why does cua-rs not mirror the window" looks here,
next to §9 and §11, not in a second document.

### The problem it would solve

The drawn cursor (§ the overlay, `crates/cua-overlay`) tells you *where* the
agent acted. It cannot tell you *what happened*, because the window it is
annotating may be behind another window or on another Space — which is the
normal case, since §1's contract forbids raising it. So the honest description
of the shipped state is: you see a marker floating over a window you may not be
able to read.

Picture-in-picture is the complete answer: a small always-visible window
showing live pixels of the driven window, so the human watches the work without
the work being brought to them. It is the same idea as the overlay, taken to
its end.

### What the current capture path would cost as a mirror

`cua-capture::capture_window` is one `/usr/sbin/screencapture -l<id>` process,
optionally one `/usr/bin/sips` process, a temp directory, and a PNG read — per
frame. §2 explains why every one of those is deliberate for *screenshots*: the
process boundary catches a SkyLight `SIGABRT` that no Rust `Result` can, and a
screenshot is a once-per-tool-call event where 150 ms is free.

Measured on this machine (M-series, macOS 15, 211 windows live), 12 iterations
per window, full `capture_window` including the pre-capture re-enumeration:

| window | size | `max_dim=1400` | no resize |
|---|:--|:--|:--|
| Slack | 1393×884 | p50 187 ms · mean 188 · **5.3 fps** | p50 147 ms · mean 146 · 6.9 fps |
| Xcode welcome | 740×460 | p50 188 ms · mean 185 · **5.4 fps** | p50 135 ms · mean 134 · 7.5 fps |
| Calendar | 935×598 | p50 163 ms · mean 163 · **6.1 fps** | p50 122 ms · mean 123 · 8.2 fps |

Broken down, on the same windows:

| stage | cost |
|---|:--|
| bare `screencapture -x -o -l<id>` | **70–100 ms** |
| `sips --resampleHeightWidthMax 1400` | 10–40 ms |
| `list_windows()` (re-enumerated every frame) | p50 **28 ms**, max 56 |
| temp dir, PNG read, IHDR parse, `Vec` | the rest, ~15–25 ms |

And the sustained cost, 30 back-to-back captures of the 935×598 window:
**4.90 s wall, 1.37 s user + 0.52 s sys** — about **39% of one core held
continuously to produce 6 fps of a single medium window.** That is the number
that decides this. A mirror is not a screenshot; it runs for the whole session,
next to an agent that also wants CPU, on a laptop the human is using.

So: a 5–8 fps ceiling, ~160 ms of latency before compositing, and a third of a
core. A mirror at that rate is not "slow video", it is a slideshow that lags far
enough behind that a human cannot tell whether the agent has stalled. The
threshold for "watchable" is roughly 15 fps and under 150 ms end to end; the
current path misses both by a factor of two to three, and no amount of tuning
closes it, because the floor is `screencapture`'s own 70–100 ms process launch.

### Whether §2's conclusion transfers. It does not.

§2 concludes ScreenCaptureKit "buys nothing here", and that conclusion is sound
for what it was about: **correctness**. The head-to-head was whether
`SCScreenshotManager` returns a *better image* than `screencapture -l` — it does
not; the two agree window for window, succeeding and failing together, because
the block is at the window layer rather than in the API.

A continuous mirror asks a different question: **throughput**. On that axis the
two are not equivalent at all, and the measurements above are the reason.
`SCStream` is a persistent stream — one content filter, one session, frames
delivered to a callback as `CMSampleBuffer`s at a requested interval, with no
process launch, no file, no PNG encode and no re-enumeration per frame. It is
built for exactly the workload the subprocess path is worst at. Two of §2's own
findings actually reinforce this rather than contradict it: that a second
`SCScreenshotManager` capture in one process fails with `-3811`, and that the
per-frame process boundary exists to survive `SIGABRT`, are both statements
about *repeated one-shot captures*, which is the pattern a stream replaces.

So the honest position is that §2 rejected ScreenCaptureKit for the screenshot
requirement and was right to, and that a mirror would have to revisit it and
would probably land the other way — which is exactly why the mirror is
expensive rather than cheap. It is not "reuse `cua-capture` and add a window".

Two things carry over unchanged and must not be lost in a rewrite:
`SCContentFilter` can abort the process outright inside `SLGetDisplaysWithRect`
before any capture is attempted, so a stream stays behind a process boundary
regardless — the boundary moves from per-frame to per-session, which is the
whole saving. And the mirror must never composite a *region*: §2 measured a
region capture of a covered window returning an unrelated app's pixels, and a
mirror that silently shows the wrong window while claiming to show the agent's
is worse than no mirror.

### TCC is the blocker, and it is not a code problem

§7 is the constraint: Screen Recording is granted to the **host** process that
launches `cua-rs` — the terminal, Claude Desktop, Cursor — not to `cua-rs`.
Today's subprocess path works precisely because of that. `screencapture` is an
Apple-signed system binary launched as a child of the responsible host, and it
inherits the host's authorization. cua-rs holds no grant of its own and needs
none.

`SCStream` does not inherit like that in practice. The API path that makes a
stream work is a signed bundle with a Team ID holding its **own** Screen
Recording entry, hardened runtime, `LSUIElement`. That means:

- an Apple Developer account and Developer ID signing plus notarization — which
  §7 already declines, on the grounds that it buys one download path and no
  fewer permission prompts. A mirror would make it mandatory rather than nice.
- a **second, separate** Screen Recording approval the user must give, to a
  binary, on top of the grants they already gave their host app. Every §7
  paragraph about how badly this project's users are served by per-host grants
  applies twice as hard to a second grant on a different principal.
- the persistent screen-sharing indicator in the menu bar for the whole
  session. Arguably correct — something *is* watching the screen — but it is a
  visible, permanent change to the user's machine that the current design does
  not make.
- a `.app` bundle as the shipped artifact. `install.sh` currently drops two
  bare executables into a directory on PATH; a bundle is a different
  distribution story end to end, and it would break the `curl | sh` install
  that is the documented entry point.

That last chain is the real cost. Ship a mirror and the project stops being
"one Rust binary you can `curl | sh`" and becomes a signed, notarized macOS app
that asks for its own screen-recording permission. That is a change of category,
not a feature.

### Where it would live, if it were built

Not in `cua-mcp`, and not in a third binary. `cua-overlay` is already the right
shape and nearly all of the hard-won parts are already solved in it: a
borderless `NSWindow` that is never focused and never key, click-through so it
cannot intercept the human's input, `CanJoinAllSpaces | Stationary |
IgnoresCycle`, level tracking that follows the target's own `CGWindow` layer
band and refuses to promote itself above every app on the machine, hiding
itself when the target app is not frontmost, a line-oriented stdin protocol, and
exit on EOF. A mirror is that window with a different `drawRect`. Rebuilding
those properties in a third binary would be duplicating the subtlest 891 lines
in the workspace.

But the process boundary should move. Today `cua-rs` captures and `cua-overlay`
draws; a mirror should have the overlay hold the stream and draw frames it owns,
because piping frames over the stdin pipe would add a copy and a serialization
per frame to a budget that is already the problem. That inverts which process
needs the Screen Recording grant — the overlay, not the server — which is
convenient in one way (only the drawn-cursor binary needs a bundle) and awkward
in another (`cua-rs` is a plain executable and `cua-overlay` is a signed app,
installed side by side, which `install.sh` and the sibling lookup in
`cua-core/src/overlay.rs` would both have to learn).

### Recommendation: do not build it

Not now, and not as an incremental step on `cua-capture`. The measured ceiling
of the current path is 5–8 fps at 39% of a core, which is not a watchable
mirror; clearing it means an `SCStream` rewrite; that rewrite means a signed,
notarized bundle with its own Screen Recording grant, a second permission
prompt, and the end of the single-binary `curl | sh` install. The observability
problem it addresses is real, but the overlay shipped in the release now covers
the cheap 80% of it — you can see, live and on your own Space, where every
action landed — and PIP is the expensive remaining 20%.

What would change the answer, in the order it would change it:

1. **A Developer ID and notarization becoming necessary for another reason.**
   If §7's "would be nicer" ever happens for Gatekeeper's sake, the dominant
   cost of PIP is already paid and the calculation flips. This is the likeliest
   path, and it means PIP should be re-evaluated *after* signing, never as the
   justification for it.
2. **An `SCStream` prototype delivering ≥15 fps under 10% of a core, on a
   grant the host already provides.** The second clause matters more than the
   first: if a stream turns out to work under inherited host authorization,
   without its own bundle grant, the entire TCC objection evaporates and this
   becomes an afternoon's work in `cua-overlay`. Worth one day of measurement
   before ever writing the feature. Nothing here establishes that it does not
   work; it was not tested, because §2 had no reason to test throughput.
3. **Users reporting that the arrow is not enough.** The overlay is new to the
   release as of this section; the honest thing is to find out whether the
   cheap answer was sufficient before costing the expensive one. If the common
   complaint turns out to be "I can see where it clicked but not what
   happened", that is the signal.
4. **A second agent, or a headless Space.** Per-app leases (§9) and off-Space
   work make the window genuinely unreachable rather than merely inconvenient,
   and at that point a marker over an invisible window annotates nothing.

What would *not* change the answer: making the subprocess path faster. Dropping
the per-frame `list_windows` saves 28 ms, skipping `sips` saves 10–40, keeping
one temp directory saves a few — call it 60 ms off a 163 ms frame, landing at
~10 fps and still a third of a core. Optimizing toward a target that is a
factor of two away is how a slideshow gets shipped.
