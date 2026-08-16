# cua-rs

**Your Mac, driven by an agent. Your cursor stays yours.**

[![ci](https://github.com/maestrojeong/cua-rs-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/maestrojeong/cua-rs-mcp/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/maestrojeong/cua-rs-mcp)](https://github.com/maestrojeong/cua-rs-mcp/releases)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![platforms](https://img.shields.io/badge/platform-macOS%20arm64-lightgrey)

A macOS computer-use MCP server that drives native apps through the
**Accessibility API** — addressing a UI element instead of moving a pointer to a
coordinate. Nothing moves your cursor, takes your keyboard focus, or switches
your Space, so an agent can work in a background window while you keep typing in
another. One Rust binary.

```mermaid
flowchart LR
    subgraph agent["agent's lane — no cursor, no keyboard focus"]
        A["Agent"] -->|MCP| M["cua-rs"] -->|"addresses an element"| B["any app<br/>background window"]
    end
    subgraph human["your lane — untouched"]
        H["You"] -->|"real cursor + keyboard"| F["whatever you are in<br/>foreground"]
    end
```

There is no arrow between the lanes, and that is the whole product.

### Three ways to drive a Mac

"Synthesize input or use accessibility" is the wrong axis, because the
interesting middle exists: you can synthesize an event and still deliver it to
one process instead of the shared stream. What separates the approaches is not
the API, it is **what they are willing to take from you**.

| | shared input | per-process, plus focus | cua-rs |
|---|:--|:--|:--|
| how a click/key travels | cursor warp / global HID queue | synthesized event routed to one pid | synthesized event routed to one pid — no AX-action attempt or fallback |
| bulk text | posted as keystrokes | posted as keystrokes | a single AX write (`AXValue`), not synthesized |
| the pointer | moves | stays | stays |
| keyboard focus | goes wherever the click lands | target is activated and held | unchanged |
| your Space | can switch | can switch | never |
| an occluded window | blank or stale capture | raised first, so never occluded | captured where it is |
| you, working meanwhile | input collides | you lose focus | works |

The middle column is a real design, not a strawman — routing per pid is the
entire click and key-press story in cua-rs too, not just the fallback for
controls with no AX action. That split follows what each API can say: a click
count and a chord have no accessibility verb at all, so those have to be events,
while replacing a field's text is one atomic `AXValue` write addressed at the
element and is worse as a stream of keystrokes. The difference from the middle column is still the second
half: per-process delivery there activates the target and keeps it frontmost,
which is a reasonable trade when a human is not sitting there, and the wrong
one when they are — cua-rs never does that, even for its pid-routed clicks and
keys.

[trycua/cua](https://github.com/trycua/cua/tree/main/libs/cua-driver)'s driver
does not pick one: it ships both contracts side by side. Its `click_at_xy` routes
a SkyLight event to a pid — the same recipe cua-rs ports — while
`click_at_xy_desktop` posts to the global HID tap so the OS delivers it to
whatever owns the pixel, "the foreground, vision-driven model that complements
the background contract" in its own words. Which transport you get depends on how
the agent found the target: a tree gives you a pid, a screenshot gives you a
pixel.

cua-rs implements only the background half — and it turns out the background half
reaches further than "accessibility only" suggests. The pid route needs a window
and a point, not an element, so `click_in_window` can be aimed at a bare pixel of
a canvas without a cursor warp. It just cannot tell you whether anything was
there, which is why it is a separate opt-in tool rather than a fallback.

So cua-rs is best described by what it will not do. There is no flag that warps
the pointer, posts to the shared keyboard stream, or raises a window: those paths
are absent, and [DESIGN.md](DESIGN.md) covers what that costs — no drag, no AX
fallback for a click or key that the pid tier cannot deliver, and no
verification once you aim at a pixel yourself. Arbitrary chords (`⌘⇧P`) are no
longer on that list: `press_key` routes every key through the same pid tier as
`click`, which has no notion of "no verb exists" to refuse in the first place.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/maestrojeong/cua-rs-mcp/main/install.sh | sh
cua-rs --help
```

Pin a release with `CUA_VERSION=v0.4.2`, or build from source with
`cargo install --git https://github.com/maestrojeong/cua-rs-mcp cua-mcp`.

> Downloading the binary by hand from the Releases page? Clear the quarantine
> flag or it hangs instead of erroring: `xattr -d com.apple.quarantine ./cua-rs`.
> The installer above does this for you.

## Grant permissions

Two grants, and **they attach to the process that launches `cua-rs`, not to
`cua-rs` itself** — macOS credits the responsible process, so a grant given to
iTerm does not carry to Claude Desktop, Cursor, or Codex CLI. The upside: a
`cua-rs` upgrade never costs a re-approval, because the grant was never on this
binary.

| Grant | Needed for | Without it |
|---|:--|:--|
| Accessibility | reading UI structure, every action | nothing works |
| Screen Recording | screenshots, and last-moment window validation for process-routed clicks | tree and AX actions still work; no images |

```bash
cua-rs permissions      # never prompts
# accessibility:    true
# screen_recording: true
```

System Settings → Privacy & Security → Accessibility (then Screen Recording),
add the **host** app, restart it.

## Connect

```json
{ "mcpServers": { "cua": { "command": "cua-rs" } } }
```

Or Streamable HTTP for a client that attaches to an already-running server:

```bash
cua-rs 9331     # http://127.0.0.1:9331/mcp  ·  /health
```

Loopback only, always. It exposes full desktop control and has no authentication
of its own, so a reachable port would hand your machine to the network.

## Use

`get_app_state` first, always: it walks one window, captures it in the same
moment, and numbers everything actionable. Those numbers are what actions take.

```text
Notes (pid 41277)  snapshot_id=1
window: Groceries

AXWindow "Groceries"
  AXToolbar
    [3] AXButton "New Note"
    [7] AXTextField:SearchField "Search" (editable)
  [12] AXOutline "Notes"
    [13] AXRow "Groceries" (selected)
  [21] AXTextArea = "milk\neggs" (editable, focused)
```

```json
{ "app": "Notes", "element_token": "1-3-AXButton" }
```

Lines with `[N]` are targetable; the rest is context, so a model does not try to
press a scroll area. `element_token` bundles the snapshot, index and role, which
makes acting on a stale handle an error instead of a mis-click — the one failure
mode a retry cannot fix and you cannot see. `x`/`y` also works, resolved against
the snapshot's own geometry, but an index names an element while a point only
names a place.

| Tool | |
|---|:--|
| `get_app_state` | **call first** — tree + screenshot from one snapshot |
| `find` · `wait_for` | search the snapshot; poll until text appears or goes |
| `click` | a pid-routed event; no `AXPress`/`AXPick`/`AXConfirm` attempt |
| `click_in_window` | last resort: a bare point, no element, nothing verified |
| `set_value` · `type_text` · `select_text` | write, append, select a substring — a single AX call |
| `press_key` | any key or chord (`⌘⇧P`, `ctrl+alt+delete`, …), pid-routed |
| `scroll` · `perform_secondary_action` | page a scroll area; any AX verb |
| `list_apps` · `check_permissions` | running apps; grant status |

Actions re-read the window and return a delta by default, so acting and looking
cost one round trip instead of two. **Read it as a textual delta, not as proof:**
lines are compared without index or indentation — which is what stops an app that
regroups its own subtrees from reporting hundreds of lines as change — so two
identically-worded rows are interchangeable and a selection moving between them
shows as nothing. It is reliable for structure arriving or leaving, like a menu
opening. To know one element's state, read that element.

For a dense app, `skeleton: true` summarizes big subtrees, then
`scope_element_id` spends the whole budget inside one of them.

## Limits

Honest ones, not a roadmap.

| | |
|---|:--|
| buttons, menus, tabs, rows, text fields | yes |
| Electron apps | yes — the tree builds lazily, so read twice |
| any key or chord (`⌘⇧P`, `ctrl+alt+delete`, …) | yes, pid-routed — see `CUA_KEY_AX_ONLY` below |
| drag | **no** — no AX verb, and the pid tier has no drag primitive wired in yet |
| canvas apps, games | clickable, but you supply the coordinate and the confidence |
| terminals | reading yes; typing no — `set_value`/`type_text` write `AXValue`, which terminals ignore; `press_key` reaches them (real key events) but only one key or chord at a time |

`set_value` replaces, `type_text` appends, and an app that only reacts to real
key events will ignore both — unchanged from AX-only cua-rs, on purpose: one
atomic `AXValue` write addressed at the element beats the same text typed
character by character into whatever happens to hold focus. `click` and
`press_key`, by contrast, never attempt an AX action at all: every click is a
mouse event routed to the target process by window id, and every key is a
keyboard event routed to the target process by pid, both reported as
`delivery: pid`; if the pid tier is unavailable the action fails rather than
touching your pointer or the shared keyboard.

Two environment variables restore the pre-pid-tier behavior for comparison or
if the pid tier proves untrustworthy on a given app: `CUA_AX_FIRST=1` puts
`click` back to `AXPress` first, pid only when no AX verb exists; `CUA_KEY_AX_ONLY=1`
puts `press_key` back to AX-verb-only (`return`/`escape`/`up`/`down`, chords
refused). Neither is a supported "best of both" mode — see DESIGN.md §1.

For a surface that publishes no elements at all, `click_in_window` takes a
window-local point in points — a screenshot pixel divided by the scale
`get_app_state` reports — and posts the click through the same pid route. It
refuses a window id your last read did not produce, and an offset outside the
window's live frame. What it will not do is confirm: there is no element to read
back, so the result says `delivery: pid (no element)` and means the event was
delivered to that pixel, not that anything was hit.

## The drawn cursor

The AX path leaves nothing on screen, which is the point — and also means you
cannot see the agent working. `cua-overlay` is a separate binary that draws an
arrow where an action landed: click-through, never focused, never your real
cursor. It is ordered just above the target window, and hides itself whenever
that app is not frontmost, so it cannot end up floating over your work.

<p align="center"><img src="assets/cursor-demo.png" width="640" alt="A mirrored presence-pointer arrow on move, the same arrow plus a small ring on click"></p>

`cua-rs` spawns it if it sits next to the binary. **The prebuilt release ships
`cua-rs` alone**, so build the workspace to get both.

## Development

```bash
cargo build --workspace
cargo test --workspace          # 98 tests, no permissions needed
cargo clippy --workspace --all-targets -- -D warnings
```

```text
cua-ax        safe AXUIElement wrapper + budgeted tree walker
cua-capture   window discovery + crash-isolated per-window PNG
cua-core      app resolution, one native worker thread, snapshots
cua-hid       process-routed input — the only crate that synthesizes events
cua-mcp       the server, binary `cua-rs`
cua-overlay   the drawn cursor
```

Two constraints worth knowing before touching it: every native call runs on one
thread, because `AXUIElement` handles are honestly `!Send`; and every tree walk
is budgeted, because an AX tree can be unbounded and is not guaranteed acyclic.
[DESIGN.md](DESIGN.md) has the reasoning, the measurements, and the known weak
spots.

`cua-ax` is publishable on its own, and is the piece the Rust ecosystem is
missing — [`accessibility-sys`](https://crates.io/crates/accessibility-sys) has
not shipped an API change since March 2025, and
[`objc2-application-services`](https://crates.io/crates/objc2-application-services)
gives raw bindings with no safe layer.

## Prior art

- [**trycua/cua**](https://github.com/trycua/cua/tree/main/libs/cua-driver) —
  larger, cross-platform, further along, and a superset of the approaches above.
  Its docs are the best free writing on this problem domain.
- [**lahfir/agent-desktop**](https://github.com/lahfir/agent-desktop) — Rust AX
  engine, CLI rather than MCP.
- [**minghinmatthewlam/computer-use-mcp**](https://github.com/minghinmatthewlam/computer-use-mcp) —
  same positioning, in Swift.

Chromium content degrades under any AX-only tool; the intended answer is to hand
the web to [browser-rs](https://github.com/maestrojeong/browser-rs-mcp) over CDP
rather than fight `AXManualAccessibility`.

| | |
|---|:--|
| [browser-rs](https://github.com/maestrojeong/browser-rs-mcp) | web, over CDP |
| [bash-rs](https://github.com/maestrojeong/bash-rs-mcp) | shell, backgrounded |
| **cua-rs** | native macOS apps, over AX |

## License

Apache-2.0
