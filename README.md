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

| | HID-synthesis tools | cua-rs |
|---|:--|:--|
| click | move cursor, post mouse down/up | AX action; a process-routed event for custom controls |
| type | post keys to whatever has focus | `AXUIElementSetAttributeValue` |
| screenshot | `CGWindowListCreateImage` (deprecated) | crash-isolated per-window capture |
| your cursor / focus / Space | moves, changes, switches | **untouched** |
| occluded or off-Space window | blank or stale capture | **captures** |
| you working at the same time | input fights the agent | **works** |

Not a runtime mode: there is no flag that warps the pointer or posts to the
shared keyboard stream. Why, and what it costs, is in [DESIGN.md](DESIGN.md).

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
| `click` | `AXPress` → `AXPick` → `AXConfirm` |
| `set_value` · `type_text` · `select_text` | write, append, select a substring |
| `press_key` | Return / Escape / arrows via AX; chords are refused |
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
| Return, Escape, stepper arrows | yes, as AX verbs |
| arbitrary chords (`⌘⇧P`), drag | **no** — no AX verb exists |
| canvas apps, terminals | **no** — nothing to address |

`set_value` replaces, `type_text` appends, and an app that only reacts to real
key events will ignore both. Controls with no AX action at all get a mouse event
routed to the target process by window id, reported as `delivery: pid`; if that
is unavailable the action fails rather than touching your pointer.

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
  larger, cross-platform, further along. Its docs are the best free writing on
  this problem domain.
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
