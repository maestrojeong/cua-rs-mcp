# Design notes

Why cua-rs is built the way it is, what is deliberately not built yet, and what is
planned next (§11).

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
| set text | `AXValue` write | works — still the *only* mechanism `set_value`/`type_text` use, see below |
| **arbitrary chord** (`⌘⇧P`) | — | no verb exists, but reachable since the pid-only change below via the pid tier (not a fallback — the only tier) |
| **drag** | — | no verb exists; delivered as a real down/drag/up gesture by the pid tier (§11). Not yet verified against a real drag source |
| **right click where the element advertises nothing** | `AXShowMenu` | works *where the element advertises it*; where it does not, a real `rightMouseDown`/`rightMouseUp` pair by the pid tier |
| **modifier click** (⌘-click, ⇧-click) | — | no verb exists — `AXPress` is "activate this", with no room for a held key. Pid tier only |
| **hover** | — | no verb exists; a synthesized `mouseMoved` by the pid tier. Cannot reach an app that polls the real cursor (§11) |
| **scroll by a distance** | — | `AXScroll*ByPage` is whole pages only; a `scrollWheel` event covers the rest, and covers elements that advertise no scroll action at all |
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
unit-tested without any grant, and the gestures themselves have **not** been
walked against a real drag source, a real hover-revealed control, or a real
Electron list. They are built, they are reachable, and they are unmeasured.

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
asking to set a field wants. Delivering the same thing as keystrokes would mean
a stream of events landing wherever the target process's first responder
happens to be, character by character, with no element addressing and no way to
confirm the focus went where it was aimed. That is strictly worse for the one
operation AX does exactly right, so `set_value` and `type_text` keep it.

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
refused, no synthesized input at all). Neither is a supported "best of both"
mode — mixing tiers per call is the exact pattern the paragraph above argues
against — they exist to bisect a specific app or macOS version where the pid
tier turns out to be the less reliable choice.

**The manual-test checklist (§8) predates this change** and still describes
0.4.x's tier order in places; it has not yet been re-walked end to end against
this change's pid-only click/key path. Treat rows there that assume an AX-first
click as aspirational until they are re-verified.

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

**Screen lock.** Not handled yet. Mutating tools should return a recoverable
error while locked rather than failing opaquely.

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
- [ ] `hover` reveals a tooltip or a hover-only button, and it is in the diff
- [ ] `hover` on an app that polls `NSEvent.mouseLocation` does nothing — confirm
      the failure is the documented one and not a delivery bug
- [ ] `button: right` opens a context menu on a control advertising no AX actions
- [ ] `modifiers: cmd` on a link opens a new tab; `shift` extends a selection
- [ ] `scroll` on an Electron list moves it, and reports `delivery: pid`
- [ ] `scroll` on a native `AXScrollArea` still reports `delivery: ax`
- [ ] cursor is byte-identical before and after each of the above

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

## 9. Deliberately not built

| | Why |
|---|:--|
| **pointer-position spoofing** | an app that reads `NSEvent.mouseLocation` rather than the event it was handed cannot be reached by a synthesized `mouseMoved`, and the only fix is warping the real cursor. Permanently out — §11 |
| **press-and-hold gestures** (a drag that pauses to let a spring-loaded folder open; click-and-hold) | the primitives are there — a drag *is* a down, a run of moves and an up — but holding means owning a mouse-down that outlives one tool call, and a release that never arrives leaves the target mid-gesture |
| **wheel momentum and rubber-banding** | a trackpad fling is a phase-tagged stream of events, not a delta. Nothing has needed it |
| AX notification streams (`AXObserver`) | would replace the fingerprint heuristic in §10 |
| menu invocation | needs temporary activation + focus restore; easy to get wrong |
| Spaces handling | off-Space windows are observation-only, matching prior art |
| per-app leases | needed once two agents share one machine |
| yield-to-human detection | requires watching real input, which needs an event tap |

---

## 10. Known weak spots

**`ui_changed` is a heuristic.** It compares focused-element identity and window
title before and after, with a 120 ms settle. It will report `false` for real
changes it cannot see. It is reported honestly rather than optimistically,
because an agent that believes every action landed is worse than one that knows
it should re-read. The right fix is `AXObserver` notifications.

**One snapshot per app.** Driving two windows of the same app alternately
invalidates indices each time. Correct, but awkward; keying snapshots by window
would fix it.

**No approval gates.** Nothing distinguishes pressing "Cancel" from pressing
"Delete All". A destructive-label heuristic requiring explicit confirmation is
the obvious next safety feature.

**Point coordinates are AX-global.** Multi-display setups with negative origins
are untested.

**A target is left believing it is active.** The `ApplicationActivated` notice is
not revoked after the click (§6), because revoking it broke the following click.
Nothing verifies that the belief ever clears: macOS will not deactivate an app it
never considered active, so it may persist until the app is genuinely activated.
The observable consequences are app-specific — a view that only draws its
selection highlight while active, a toolbar that stays enabled — and the sharp
edge is an app whose own activation handler calls `activateIgnoringOtherApps:`,
which would turn the notice into a real raise. Releasing the belief on a session
boundary rather than per click is the shape of the fix, and it is not built.

**Menu-opening controls: solved, and the fix was two bugs rather than a
mechanism.** KakaoTalk's chat-room hamburger was the standing case — no AX
actions, so a synthesized click is the only route, and for a long time it did
nothing. Neither missing piece was exotic:

- `is_plausible_target()` required `layer == 0`, which excluded the app's own
  floating chat windows at layer 3. The cap is now 3.
- An `ApplicationDeactivated` notice was sent after *every* click, which
  destroyed the key-window state the next click depended on. It is gone by
  default, and `CUA_DEACTIVATE_AFTER_CLICK=1` restores it for comparison.

With both fixed the menu opens reliably, its items are readable in the tree, and
`press_key escape` on the `AXMenu` closes it. Ruled out along the way, each by
measurement rather than argument: the coordinate; the AppKit event header; the
timestamp; the ordering of the focus notices; whether the target believes it is
frontmost (`AXFrontmost` does flip, in about 150 ms); and the private versus
public per-pid post route.

What remains unsolved is one step further in: an `AXMenuItem` does not reliably
act on the first `AXPress`. The first press selected it and the second opened the
dialog, observed once. `return_state` makes that visible — the diff reported
exactly one changed line, `(selected)` — so it is measurable now, but it has not
been characterized.

**`press_chord_background_pid` is wired in; `type_text_background_pid` still
is not.** Both exist in `cua-hid`, built the same way —
`CGEventCreateKeyboardEvent` against a `HIDSystemState` source,
`CGEventKeyboardSetUnicodeString` for characters with no keycode, posted
per-pid — which invalidates this crate's founding assumption that keyboard
input must go through the global tap and steal focus. `press_key` now
calls the former as its only tier (§1a), because accessibility cannot express a
chord at all: there is `AXConfirm` and `AXCancel` and then nothing, so a real
event is the only thing `cmd+shift+p` could ever become.

**The control-and-measure treatment this paragraph used to demand is still
owed.** Being the only possible mechanism is an argument for the design, not
evidence that it lands correctly, and the risk originally named here has not
moved: a keystroke goes to wherever the target process's own first responder is,
`cua-core` can only best-effort-focus the addressed element first (`AXFocused`),
and no query confirms the focus actually moved before the keystrokes did. A miss
therefore types into whatever the human was editing — the asymmetry that kept
this gated for several releases. Until that is measured on a target whose text
can be read back, treat `press_key` on an app you have not tried as unproven,
and use `CUA_KEY_AX_ONLY=1` to get the old AX-verb-only path back.

`type_text_background_pid` stays unwired, for the reason §1a gives: a bulk text
write is the one operation accessibility expresses better than events can. One
`AXValue` write replaces the whole string atomically and is addressed at the
element, where the same text typed as events becomes a long stream landing on
whatever holds focus, one character at a time, multiplying the focus risk above
by the length of the string. So `set_value`/`type_text` keep the AX-only
mechanism they always had, and `post_chord` remains the honest, focus-stealing
fallback for a caller that specifically needs real keystrokes typed into a
terminal or canvas that ignores `AXValue`.

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

**Modifiers.** `parse_modifiers` shares its table with `parse_chord`, so `cmd`,
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

**Hover.** One `mouseMoved` at a point, no press, and — unlike a click — no
activation-assist click on the window's own activation point, because a caller
asking to hover has not asked to press anything. The activation *notices* are
still sent.

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
and the elements an agent most often needs to scroll advertise nothing at all —
an Electron list, a canvas, a web area inside a native shell — which was a
refusal rather than a mechanism. So: a page request on an element that
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

### What the widened model has not been shown to do

Everything above is reachable and none of it is measured. The permission-free
tests cover what can be tested without a grant — the modifier and button
vocabularies, the interpolation's endpoints and monotonicity, the step-count
bounds, the wheel-delta signs, the scroll tier choice, the page sizing, and
every argument-validation refusal — and that is a statement about the logic, not
about whether a real app accepts the events.

Specifically **unverified**: that a Finder or Mail list reorders under `drag`;
that a hover-revealed control appears under `hover` on any app; that an Electron
list scrolls under the wheel tier; that a right click opens a context menu on a
control with no AX actions; that ⌘-click opens a link in a new tab. Each is one
manual run away and none of them has been run. §8's checklist is where they
belong once they have been.

### Chords

`press_chord_background_pid` and `type_text_background_pid` exist and nothing
calls them (§10). Turning them on is a verification problem, not an
implementation one, and the asymmetry is why they are still off: a click that
misses does nothing, while a keystroke that misses types into whatever the human
is editing. Gate on the same activation evidence the click path already collects
— the synthesized notice plus the `AXFrontmost` poll — and require a target whose
text can be read back, so a miss is provable rather than assumed.

### What stays closed

`post_chord` remains unreachable from the server. It exists so the shared-input
keyboard path is visible in one file and can be reasoned about, and it should be
deleted once chords land in the pid tier.

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
