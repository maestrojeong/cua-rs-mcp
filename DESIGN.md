# Design notes

Why cua-rs is built the way it is, and what is deliberately not built yet.

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
// what a click actually is here
element.perform("AXPress")?;              // AXUIElementPerformAction
// what typing actually is here
element.set_string("AXValue", "hello")?;  // AXUIElementSetAttributeValue
```

Neither call has a coordinate, a cursor, or a notion of focus. The app receives a
message and acts on it.

### This was verified, not assumed

The decision was informed by disassembling OpenAI's shipped implementation
(`com.openai.sky.CUAService`, the backend of Codex's bundled computer-use
plugin). Its undefined-symbol table is the whole argument:

```text
CGEventPost                0      AXUIElementPerformAction      1
CGEventPostToPid           0      AXUIElementSetAttributeValue  1
CGEventCreateMouseEvent    0      AXUIElementCopyAttributeValue 2
CGEventTapCreate           0      AXObserverCreate              1
IOHIDPostEvent             0      SCStream                      5
CGEventGetFlags            1  ← reads modifier state only
```

Zero event-posting symbols. Their "does not steal focus" claim is not a feature
flag, it is the absence of that code. All 29 AX symbols they link are available
in the public `objc2-application-services` crate, which is what made an
independent implementation viable in the first place.

This table documents Codex's *input* path: it posts events through public
CoreGraphics/IOHID APIs and never raises a window. cua-rs keeps that public-API
path as its reliable click tier, but it additionally ports one piece of private
SPI for a *quieter* tier — the SkyLight `SLEventPostToPid` recipe, dlopened
lazily and confined to `cua-hid` (see the end of §6). That is a deliberate,
documented reversal of the "no private API" rule; if the framework cannot be
loaded, the click fails explicitly rather than falling back to shared input.

### The cost, stated plainly

AX cannot express everything:

| Capability | AX verb | Status |
|---|:--|:--|
| press a button | `AXPress` | works |
| select a row / tab | `AXPick` | works |
| Return, Escape | `AXConfirm`, `AXCancel` | works |
| context menu | `AXShowMenu` | works |
| page a scroll area | `AXScroll*ByPage` | works |
| set text | `AXValue` write | works, but *replaces* |
| **arbitrary chord** (`⌘⇧P`) | — | **no verb exists** |
| **drag** | — | **no verb exists** |
| **pixel-only surfaces** | — | nothing to address |

So the honest ceiling is: cua-rs drives *structured* UI extremely well and
cannot drive canvas. That is an acceptable trade for the coexistence property,
and it is written into the README rather than hidden.

### `press_key`: decided

Three options, none free:

1. **`AXUIElementPostKeyboardEvent`** — app-scoped, in the crate, deprecated, no
   modifier-chord support. OpenAI does not link it. Rejected.
2. **AX-only `press_key`** — background-safe semantic verbs remain; arbitrary
   chords are refused. Chosen in 0.3.1.
3. **AX where a verb exists, HID behind an explicit flag otherwise.** Removed
   in 0.3.1 because the flag also enabled shared-pointer fallback.

So `press_key` maps `return`/`enter` to `AXConfirm`, `escape` to `AXCancel`, and
`up`/`down` to `AXIncrement`/`AXDecrement`. Those stay in the background. Anything
else — every chord, every letter — is refused.

Every successful key action therefore reports `delivery: ax`; there is no
shared keyboard-input delivery mode.

One subtlety worth recording, because it produced a self-contradicting error
message before it was fixed: a key can *have* an AX verb that the *target element*
does not accept. A tab button has `AXPress` and `AXShowMenu` but not `AXCancel`, so
`escape` on it used to fall through to the generic "no accessibility equivalent"
refusal — text that named `escape` as something which works without HID. That case
now has its own error naming the verb, listing what the element does accept, and
pointing at where `AXCancel` usually lives (the window or a dialog's default
button).

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

### An open menu blocks window capture, and there is no safe fallback

Measured on KakaoTalk: while an `NSMenu` is up, `screencapture -l<id>` fails with
`could not create image from window` for that app's windows, and the *same*
window id captures fine seconds later once the menu closes. `on_screen` is not
the discriminator — every window of the app reported `on_screen=false` in both
the failing and the succeeding case.

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

cua-rs matches on public API instead: same pid, plausible target (layer 0, ≥40pt
each side), then the smallest frame distance to the AX frame. With no AX frame,
the largest on-screen window wins.

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
  subtype `ApplicationActivated`, posted into the target's own event queue and
  balanced by `ApplicationDeactivated` after the click. The window server's key
  focus never changes, so the user's typing keeps going where it was going; only
  the target's private idea of "am I active" moves, for the duration of the
  click. This reverses an earlier "no focus assist at all" stance, which was
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

---

## 8b. Calibration against a shipping implementation

macOS ships a working, signed instance of exactly this problem: OpenAI's
`SkyComputerUseService` (`com.openai.sky.CUAService`), the native helper behind
Codex's Computer Use. Its `AccessibilitySupport` and `ComputerUse` Swift modules
were read symbol-by-symbol from a full decompilation and used as ground truth for
the input path. What follows is what differed, and what was changed here as a
result.

| | reference (`SkyComputerUseService`) | cua-rs before | now |
|---|:--|:--|:--|
| mouse event construction | `-[NSEvent mouseEventWithType:…eventNumber:clickCount:pressure:]` → `-[NSEvent CGEvent]` | `CGEventCreateMouseEvent`, private fields patched on afterwards | matches reference |
| event number | monotonic `SynthesizedEvent.nextEventNumber`, one per down/up pair | never set, so always 0 | monotonic counter in `cua-hid::nsevent` |
| click count | `clickCount:` argument, counting up across a gesture | `kCGMouseEventClickState` stamped after the fact | carried by the `NSEvent` header |
| window identity | `windowNumber:` argument | field 51 stamped after the fact | carried by the `NSEvent` header |
| timestamp | `setTimestamp(DispatchTime.now().uptimeNanoseconds)` immediately before each post | never set | `CLOCK_UPTIME_RAW`, read immediately before each post |
| activation | `SyntheticAppFocusEnforcer.enforceActiveState(for:)` + synthesized `notifyAppActivated` / `notifyAppDeactivated` | none, by policy | synthesized `ApplicationActivated`/`Deactivated` notices; real activation still refused |
| waiting for activation to land | `waitUntilAppBelievesItIsFrontmost(2.0)`, polled | fixed 12 ms sleep | polls the target's `AXFrontmost` to the same 2 s ceiling |
| keeping a background menu open | `SystemFocusStealPreventer` process taps whose callback returns NULL, registered per target pid *and* menu pid | none | not built — see §10 |
| holding focus during a session | `clickEventTap`, listen-only, re-activating the target with `NSRunningApplication.activate` when something else takes focus | none, by policy | not built — see §10 |
| menu lifecycle | `ComputerUseAppController` tracks `_currentlyOpenedMenu` / `_currentlyFocusedMenuBarItem` and feeds the menu's pid to suppression | none — only `sendClick` was reproduced | not built |
| post route | public `CGEventPostToPid` | private `SLEventPostToPid` | unchanged — see below |
| deactivation | `deactivateFocusEnforcer()`, a lifecycle step | `ApplicationDeactivated` after *every* click | not sent per click — see below |
| keyboard | `CGEventCreateKeyboardEvent` + `keyboardSetUnicodeString`, posted per-pid | global HID tap only; arbitrary keys refused outright | per-pid path written, gated (see §10) |
| cursor feedback | `ComputerUseCursor`, a spring-animated overlay window; the visible "mouse" is drawn, not the system pointer | none | not built |

Three of these deserve more than a table row.

**Balancing every click with a deactivation was actively harmful.** Telling the
target it went inactive immediately after telling it the opposite is not a
no-op: measured on KakaoTalk, the chat window's own menu-bar item ("채팅")
disappeared the instant that notice landed, and stayed gone. The control was
still mid-gesture. Suppressing the notice kept the menu bar intact across the
click. Leaving the target believing it is active is the smaller cost, it is what
the reference does, and the real frontmost app was never touched either way.

**Window level is not a reliability signal, and treating it as one broke
clicks.** `is_plausible_target` required level 0. KakaoTalk publishes chat-room
windows at level 3 (`NSFloatingWindowLevel`), so they were dropped from the
candidate set; the click path then matched a *different* window of the same
process and stamped that window's number onto the event, which the target
discarded. The symptom — "this control ignores synthetic clicks" — pointed at
the event, and the cause was the window lookup. The ceiling is now level 3, with
menus, status items and overlays still excluded because they live far above it.

**The construction order was backwards, and that was the bug.** A `CGEvent`
synthesized from scratch has no AppKit identity: `-[NSEvent eventNumber]` reads
back 0, the window number is 0, `-[NSEvent window]` is nil. Custom-drawn
`NSView`s that hit-test and count clicks themselves read those and conclude the
event is not a real click. Stamping the private fields afterwards does not help,
because AppKit rebuilds its `NSEvent` from the event record's own header rather
than from fields a caller patched in. Building the `NSEvent` first inverts the
dependency: AppKit fills in the header it will later validate. The measured
symptom this explains is a chat app's conversation-list row accepting a click
from the reference implementation and ignoring an otherwise identical one from
here.

**The public post route works.** The reference uses `CGEventPostToPid`, the very
call §1 and `post_click_to_pid` record as non-functional. Both observations can
be true at once: the earlier measurement posted an event that was missing the
AppKit header, the fresh timestamp, and the activation notice, so it is not
evidence about the route. `SLEventPostToPid` is kept for now because it is what
the current recipe was verified against, but the private-SPI dependency is no
longer *justified* by the public route failing, and dropping it is a live option.

**The visible cursor is a lie, in both implementations.** The reference's
`createVirtualCursorIfNeeded` builds a `ComputerUseCursor` — an overlay window
with its own spring-physics parameters (`springResponseScaler`,
`scootStretchResponse`, `springDampingFraction`). What a user watching the screen
sees glide across and click is that overlay, not the system pointer, which never
moves. This matters for calibration: "OpenAI's version moves the real mouse and
that is why it works" is false, and the actual difference was the event header.

---

## 9. Deliberately not built

| | Why |
|---|:--|
| `drag` | no AX verb; would need HID pointer synthesis, which `cua-hid` does not do yet |
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

**The reference does use real activation, just not where we looked.** An earlier
reading of the symbol table concluded it never calls
`NSRunningApplication.activate`, and §1 still repeats that its undefined-symbol
table contains no event-posting calls. Both statements were too strong. Its
`clickEventTap` is `kCGAnnotatedSessionEventTap` with `kCGEventTapOptionListenOnly`
— the raw options word at `0x100e8a730` is `{placement: 0, options: 1}` — and its
callback passes every event through unchanged (`sky_decomp.c:1670393-1670405`).
What that callback *does* is watch for another process taking focus and answer by
calling `activateWithOptions(0)` on the target (`sky_decomp.c:1670407-1670547`).

So the model is not "never disturb focus". It is "hold the target frontmost for
the duration of a session, and put it back if something takes it away". That
reframes the one measurement where the reference appeared to drive a background
app: the observer could not confirm real frontmost state at the time, and this
tap would have re-activated the target within a frame of the test raising another
window. Treat "the reference works in the background" as unproven.

**Menu-opening controls are still unsolved.** KakaoTalk's chat-room hamburger is
the standing case: it advertises no AX actions, so a synthesized click is the
only route, and it opens for the reference implementation at coordinates cua-rs
also computes correctly. What has been ruled out, each by measurement rather than
argument: the coordinate; the AppKit event header; the timestamp; the ordering of
the focus notices; whether the target believes it is frontmost (`AXFrontmost`
does flip, in about 150 ms); the private versus public per-pid post route (both
behave the same); and — the most informative one — moving the real pointer onto
the control and clicking it there, which also does nothing. A real HID click
failing is what rules out "menu tracking needs the cursor over the control" and
says the missing piece is around the click rather than in it.

Two candidates remain, both from the reference and neither built here:

- **Menu-dismissal suppression.** Not the `clickEventTap` — that one is
  listen-only and passes events through. It is `SystemFocusStealPreventer`,
  which installs *process* taps on the target pid and, separately, on the menu's
  own pid, whose callback returns NULL for the events that would close the menu
  (`sky_decomp.c:1675580-1675665`). This is a real filter on input, which is the
  boundary §9 draws deliberately, so it is a decision rather than a task.
- **Menu lifecycle tracking.** `ComputerUseAppController` keeps
  `_currentlyOpenedMenu` and `_currentlyFocusedMenuBarItem`, and the menu pid it
  hands to suppression comes from there. Only `sendClick` was reproduced here;
  the controller layer above it was not, and a menu pid cannot be supplied
  without it.

Until one of those is taken on, treat menus as observable but not operable, and
note that `AXShowMenu` via `perform_secondary_action` reaches some controls with
none of this machinery — though not this one, which exposes no actions at all.

**The per-pid keyboard path is written but unproven.**
`press_chord_background_pid` and `type_text_background_pid` exist in `cua-hid`
and nothing calls them. They follow the reference implementation's construction
(`CGEventCreateKeyboardEvent` against a `HIDSystemState` source,
`CGEventKeyboardSetUnicodeString` for characters with no keycode, posted per-pid),
which invalidates this crate's founding assumption that keyboard input must go
through the global tap and steal focus. They stay gated because a keystroke that
lands in the wrong process is far worse than a click that does not land: it types
into whatever the user is editing. Verifying them needs the same
control-and-measure treatment the click path got, against a target where a miss
is unambiguous. Until then `press_key` remains AX-only and `post_chord` remains
the honest, focus-stealing fallback.
