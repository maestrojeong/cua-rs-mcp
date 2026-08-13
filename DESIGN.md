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

### The `press_key` question, open

Three options, none free:

1. **`AXUIElementPostKeyboardEvent`** — app-scoped key event, in the crate,
   deprecated, weak modifier support. Notably OpenAI does **not** use it.
2. **`CGEventPost` fallback** — works everywhere, steals focus. The thing this
   project exists to avoid.
3. **Ship no `press_key`.** What OpenAI effectively did.

Current plan: default to AX-only; if a HID path lands it must be enabled by an
explicit flag *and* must report `delivery: hid` in the result, so an agent can
never quietly grab the cursor. A silent fallback would destroy the one property
that distinguishes this server.

---

## 2. Why ScreenCaptureKit and not `CGWindowListCreateImage`

`CGWindowListCreateImage` returns what the window server composited. A window
the user has covered comes back blank or stale — precisely the window an agent
working in the background is driving.

ScreenCaptureKit asks the *owning app* to render. Occluded and off-Space windows
capture correctly.

Two secondary reasons:

- **Per-window, not full-screen.** A 5K grab is ~15 MB, mostly wallpaper and the
  human's unrelated windows; downscaled to fit a vision model, the target app's
  text is illegible. It also means screenshots do not exfiltrate whatever else
  was on screen.
- **One TCC prompt.** In-process SCK avoids the extra Screen Recording prompt
  that shelling out to `screencapture` per capture would trip.

Captures are requested at backing-store resolution, then clamped
(`max_image_dim`, default 1400). Asking for point dimensions instead yields a
soft half-resolution image on Retina where small UI text becomes unreadable.
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

The fix is one attribute write, plus two things that are easy to get wrong:

```rust
app.set_bool("AXManualAccessibility", true);      // modern
app.set_bool("AXEnhancedUserInterface", true);    // legacy fallback
std::thread::sleep(Duration::from_millis(400));   // the tree builds async
```

**Do it once per process, not once per snapshot.** Setting it repeatedly makes
every renderer rebuild its tree in a loop and pegs WindowServer — a documented
production failure in other projects, not a theoretical one.

**Key on `(pid, start_time)`, not pid.** Pids are recycled. A relaunched Electron
app can land on its predecessor's pid, inherit the "already enabled" decision,
skip the poke, and return a permanently empty tree. Start time comes from
`proc_pidinfo(PROC_PIDTBSDINFO)` — `sysctl(KERN_PROC_PID)` would need
`struct kinfo_proc`, which the `libc` crate does not expose on Apple platforms.

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

**Signing is the only durable fix.** TCC keys a grant on the code signature, so
an unsigned or randomly-signed release forces re-approval on every version bump.
Current state, honestly:

- **now:** ad-hoc signature with a stable `--identifier` in `release.yml`. Keeps
  the identity stable across rebuilds. Not sufficient.
- **needed:** Developer ID + notarization. Requires an Apple developer account.
  `MACOS_CERT_P12` / `MACOS_CERT_PASSWORD` / notarytool credentials are the
  intended secret names.

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
- [ ] click in a background window while typing in the foreground: no dropped keystrokes
- [ ] cursor does not move during any action
- [ ] frontmost app is unchanged after `click`, `set_value`, `scroll`
- [ ] active Space is unchanged
- [ ] target window on another Space still captures

**Tree**
- [ ] native app (Notes, Finder): labeled, actionable elements present
- [ ] Electron (Slack, VS Code): non-empty tree; **second** call has no 400 ms delay
- [ ] relaunch that Electron app; if the pid is reused, the tree is still non-empty
- [ ] 10k-row table: walk returns under `max_nodes` and does not hang
- [ ] wedged / modal app: fails with a timeout, does not hang the server

**Snapshots**
- [ ] `click` with a stale `snapshot_id` → `StaleSnapshot`, nothing pressed
- [ ] index out of range → `BadIndex`
- [ ] action before any `get_app_state` → `NoSnapshot`

**Resolution**
- [ ] `Slack` resolves despite helper processes
- [ ] genuinely ambiguous name → error naming both candidates
- [ ] bundle id and `/Applications/X.app` both work

**Geometry**
- [ ] Retina: `scale` ≈ 2.0; external 1x display: ≈ 1.0
- [ ] window moved between snapshot and action: index still hits the right control

---

## 9. Deliberately not built

| | Why |
|---|:--|
| `press_key` | see §1; strategy undecided on purpose |
| `drag` | no AX verb; would require HID |
| `type_text` | `set_value` covers the safe case; append semantics need a key path |
| skeleton traversal / `scope_element_id` | the big token win, not yet needed at 1500 nodes |
| `find` / `wait_for` | valuable; the model can currently grep the tree itself |
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
