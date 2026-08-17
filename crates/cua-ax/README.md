# cua-ax

Safe, agent-oriented Rust wrappers over the **macOS Accessibility API**, built on
[`objc2-application-services`](https://crates.io/crates/objc2-application-services).

```rust
use cua_ax::{action, attr, Element, Limits};

cua_ax::require_trusted()?;                       // never prompts
let app = Element::for_pid(pid);                  // 2s messaging timeout applied
app.enable_rich_accessibility();                  // wake up Chromium/Electron

let window = app.element(attr::FOCUSED_WINDOW).unwrap();
for node in window.snapshot_tree(Limits::default()) {
    if node.is_actionable() {
        println!("{} {:?} {:?}", node.index, node.role, node.label);
    }
}

// One press, delivered to the element. No cursor movement, no focus change.
window.element(attr::FOCUSED_UI_ELEMENT).unwrap().activate()?;
```

## Why this exists

Every Rust project driving macOS UI today either depends on
[`accessibility-sys`](https://crates.io/crates/accessibility-sys) — which has not
shipped an API change since March 2025 despite ~177k downloads and 11 dependent
crates — or hand-rolls raw `core-foundation` calls. `objc2-application-services`
provides modern raw bindings but no safe layer. This is that layer.

## What it gives you over raw bindings

- **`Element`** — a retained `AXUIElement` with typed attribute reads
  (`string`, `bool`, `number`, `element`, `elements`, `position`, `size`, `frame`),
  writes, and action delivery.
- **`snapshot_tree`** — one breadth-first walk under hard caps
  (`max_nodes`/`max_depth`/`max_children`), returning a flat `Vec<AxNode>` whose
  positions are stable handles. An AX tree can be effectively unbounded and is not
  guaranteed acyclic, so an uncapped walk is a hang, not a slow path.
- **`activate()`** — tries `AXPress` → `AXPick` → `AXConfirm` and reports which
  landed, because AX has no single "click" and elements disagree about which verb
  they accept.
- **`append_text` / `select_text`** — insertion via `AXSelectedText` rather than
  whole-value replacement, with char→UTF-16 offset conversion. AX counts UTF-16
  units, so one emoji earlier in a field shifts a naive selection.
- **`enable_rich_accessibility`** — the `AXManualAccessibility` poke, plus honest
  documentation of the two ways it misleads you (the read-back lies; the tree
  appears seconds later than you expect).
- **Errors that name the remedy.** `AxError::NotTrusted` carries the System
  Settings path; `Stale` says to re-snapshot; `NotImplemented` distinguishes "the
  app advertises this and refuses it" from "unsupported".

Every app element gets `AXUIElementSetMessagingTimeout` on construction — AX is
synchronous IPC, and a modal app otherwise blocks the caller on the first read.

## Threading

`Element` wraps `CFRetained<AXUIElement>` and is deliberately **not `Send`**. Keep
it on the thread that created it. See
[`cua-core`](https://github.com/maestrojeong/cua-rs-mcp/tree/main/crates/cua-core)
for a worker-thread pattern that lets an async server use it without lying to the
borrow checker.

## Diagnostics

```sh
cargo run -p cua-ax --example ax_poke -- Slack
```

Prints settable / write-result / read-back for both enablement attributes and
samples the element count over time — which is how the read-back discrepancy
above was found.

## Requirements

macOS, and the **Accessibility** grant on the process that launches your binary
(not on the binary itself — macOS attributes the request to the responsible
process).

Part of [cua-rs-mcp](https://github.com/maestrojeong/cua-rs-mcp). Apache-2.0.

This crate is only cua-rs's Accessibility layer. In the server, `click` and
`press_key` use `cua-hid`'s process-routed SkyLight/CGEvent synthesis by
default; AX remains responsible for discovery, element addressing, text writes,
and explicitly requested semantic actions. Process routing leaves the shared
cursor untouched, but keyboard delivery is to the target process's current
first responder rather than directly to an AX element.
