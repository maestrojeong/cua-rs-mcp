# cua-rs

**An AI agent drives your Mac. Your mouse and keyboard stay yours.**

[![ci](https://github.com/maestrojeong/cua-rs-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/maestrojeong/cua-rs-mcp/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/maestrojeong/cua-rs-mcp)](https://github.com/maestrojeong/cua-rs-mcp/releases)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![platforms](https://img.shields.io/badge/platform-macOS%20arm64-lightgrey)

## What is this?

`cua-rs` is an **MCP server** that lets an AI agent (Claude, Codex, and the
like) drive real macOS apps. The common way to build a "computer-using
agent" is to compute screen coordinates and move the mouse to click — which
means the agent takes over the mouse, keyboard and screen you are currently
using.

`cua-rs` instead uses macOS's **Accessibility API** to find "that button" or
"this text field" as an **element**, not a set of screen coordinates. Text
values and explicit AX actions go straight to the element; the default
`click`/`press_key` send a synthesized event that `cua-hid` builds and routes
through SkyLight to the target process only. It never touches the shared HID
stream, but because the key event still lands on whatever element that
process currently has focused, every result also reports a focus-verification
status. As a result:

- Your mouse cursor never moves.
- Keystrokes never go to another app, and the active app never switches.
- Nothing switches Spaces (virtual desktops).

In other words, while you are writing a document, the agent can quietly
operate some other app in the background (Notes, a messenger, whatever) at
the same time. It's a single Rust binary.

<p align="center"><img src="assets/architecture.svg" width="820" alt="Top, blue lane: the agent goes through MCP to cua-rs, which addresses an element via the Accessibility API to operate the target app. Bottom, orange lane: you keep using your real mouse and keyboard on whatever window is frontmost. No arrow crosses between the two lanes."></p>

There is no arrow between the two lanes. That's the whole point of this
project. The real cursor never moves, nothing joins the shared keyboard input
stream, and no window gets pulled to the front. There is one caveat: a
process-routed key event lands on that app's first responder rather than the
AX element itself. The full design rationale is in [DESIGN.md](DESIGN.md).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/maestrojeong/cua-rs-mcp/main/install.sh | sh
cua-rs --help
```

Pin a version with `CUA_VERSION=v0.4.2`, or build from source so both
binaries land in the same Cargo bin directory:

```bash
cargo install --git https://github.com/maestrojeong/cua-rs-mcp cua-mcp cua-overlay
```

> If you downloaded a binary by hand from the Releases page, macOS's
> quarantine flag can make it hang instead of erroring. Clear it with
> `xattr -d com.apple.quarantine ./cua-rs`. (The install script above does
> this for you.)

## Grant permissions

Only two permissions are needed. **Importantly, they're granted to whatever
app launched `cua-rs`** (Claude Desktop, Cursor, etc.) — so running it from a
different terminal means granting again, but upgrading `cua-rs` itself never
requires re-approval.

| Permission | Needed for | Without it |
|---|:--|:--|
| Accessibility | reading screen structure, every action | nothing works |
| Screen Recording | screenshots, a last-moment safety check before clicking | reading and acting still work; no images |

```bash
cua-rs permissions      # checks status, never prompts
```

In macOS **System Settings → Privacy & Security → Accessibility** (then
Screen Recording), add the app that runs `cua-rs` and restart it.

Note that `cua-rs` **refuses to act** on System Settings itself, Keychain
Access, password managers, and the like (reading is still fine). See
[Safety](#safety) for details.

## Connect

```json
{ "mcpServers": { "cua": { "command": "cua-rs" } } }
```

Streamable HTTP is also supported, for a client that attaches to an
already-running server:

```bash
cua-rs 9331     # http://127.0.0.1:9331/mcp
```

This mode only accepts local connections and is protected by a bearer token
(either a random one printed at startup, or one you pin with
`CUA_HTTP_TOKEN`). The default stdio mode needs no token: the client already
owns the process.

## Safety

Several safety gates are on by default so an agent can't accidentally do
something dangerous. Whenever something is blocked, the response always says
why and how to unblock it.

- **You can scope which apps can be driven.** Set `CUA_ALLOWED_APPS` and
  everything outside that list is refused for actions (reading still works).
  This is the recommended way to run it.
- **Credential and security apps are never driven.** Keychain Access,
  password managers, 1Password, Bitwarden, System Settings, login/lock
  screens, and similar. Reading still works, but screenshots are withheld.
- **Destructive actions need confirming.** If a button's label reads as
  "Delete", "Remove", "Erase", "Reset" and the like, it's refused unless you
  pass `confirm_destructive: true`. That rule also covers pressing Return in
  a dialog and clicking "OK" buttons. Reversing actions like "Cancel" or
  "Save" are always allowed.
- **Nothing is delivered to a locked screen or an active screensaver.**
- **Optionally, cua-rs can yield to a human actively using an app**
  (`CUA_YIELD_TO_HUMAN=1`, off by default). When enabled, the agent backs off
  while a human is using that window.

These gates lean toward "block first, then explain" whenever something might
be risky. The most reliable one is `CUA_ALLOWED_APPS`, which scopes what can
be acted on at all — once set, the agent itself can never widen it. The
detailed decision criteria live in [DESIGN.md §7a](DESIGN.md).

## How agents use it

An agent always starts with `get_app_state`, which returns what's currently
on screen for the frontmost (or a chosen) window.

```text
Notes (pid 41277)  snapshot_id=1
  AXWindow "Groceries"
    [3] AXButton "New Note"
    [7] AXTextField:SearchField "Search" (editable)
  [21] AXTextArea = "milk\neggs" (editable, focused)
```

Any numbered element (`[3]`, `[7]`, `[21]`) can be clicked or filled in.

| Tool | What it does |
|---|:--|
| `get_app_state` | call first — grabs the tree and a screenshot together |
| `find` / `wait_for` | look for text to appear or disappear |
| `click` / `drag` / `hover` | click, drag or hover an element |
| `set_value` / `type_text` / `select_text` | write, append, or select text |
| `press_key` | press a key or a shortcut |
| `menu_bar` | read the menu bar and click an item |
| `scroll` | scroll a view |
| `list_apps` / `check_permissions` | list running apps; check permission status |

Every action returns what changed on screen afterward, so an agent gets
"act, then verify" in a single round trip.

## What works, what doesn't (summary)

- Buttons, menus, tabs, lists, text fields: works well in most native and
  Electron apps.
- Pop-up menus opened by a click: clicking an item directly isn't supported;
  use the item's keyboard shortcut or the menu bar instead (a pop-up menu is
  a special kind of window the Accessibility API can't see into).
- Hover effects: work on web pages (Chrome, Safari), but not on native lists
  like Finder's.
- Views with no Accessibility scroll action (a canvas, some Electron lists):
  `scroll` isn't attempted — page up/down keys are suggested instead.
- Chrome/Safari drop clicks and hover entirely when they aren't the
  frontmost app. If you need to drive the web,
  [browser-rs](https://github.com/maestrojeong/browser-rs-mcp) (CDP-based) is
  a better fit than this project.
- Terminals: reading works; typing needs the `mechanism: "keystrokes"`
  option.

More detailed experiments and numbers live in [DESIGN.md](DESIGN.md).

## The on-screen cursor

Because the Accessibility path leaves no trace on screen, it's hard to see
what the agent is doing. That's why a small companion program, `cua-overlay`,
gets installed alongside it and draws a **click-through, transparent arrow**
wherever the agent acts. It's not a real cursor and never intercepts mouse
input. It only shows up while the app being driven is frontmost, and hides
otherwise.

`cua-rs` looks for a binary named exactly `cua-overlay` next to its own,
resolved executable. If it's missing or fails to launch, cua-rs still works —
it just logs one warning to stderr and disables the on-screen indicator. The
install script and the source install command above both install the two
binaries together.

<p align="center"><img src="assets/cursor-demo.png" width="640" alt="An arrow and ring marking where the agent clicked"></p>

## For developers

```bash
cargo build --workspace
cargo test --workspace          # 249 tests, no special permissions needed
cargo clippy --workspace --all-targets -- -D warnings
```

The project is split into several crates by responsibility:

```text
cua-ax        a safe wrapper over the macOS Accessibility (AXUIElement) API
cua-capture   finds windows and takes screenshots
cua-core      snapshots, app resolution, and the core safety gates
cua-hid       delivers clicks/keys to a specific process
cua-mcp       the MCP server itself, binary `cua-rs`
cua-overlay   the on-screen cursor
```

The deeper design rationale and constraints live in [DESIGN.md](DESIGN.md).

## Related projects

- [**trycua/cua**](https://github.com/trycua/cua/tree/main/libs/cua-driver) —
  bigger, cross-platform, and further along. Some of the best writing on
  this space.
- [**lahfir/agent-desktop**](https://github.com/lahfir/agent-desktop) — a
  Rust accessibility engine, shipped as a CLI rather than MCP.
- [**minghinmatthewlam/computer-use-mcp**](https://github.com/minghinmatthewlam/computer-use-mcp) —
  similar goals, written in Swift.

## License

Apache-2.0
