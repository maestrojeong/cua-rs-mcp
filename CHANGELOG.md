# Changelog

Versions are `0.MINOR.PATCH`. While the crate is pre-1.0, a change an existing
caller can *notice* takes the minor slot, even when it is a bug fix — the tool
descriptions are the API here, and an agent that learned the old behaviour is a
caller.

## Unreleased

### `is_transient_popup()` requiring `isOnScreen()` was right

A menu of a buried app had been seen missing from the pop-up list while
apparently open, which would have made the predicate wrong. Measured on one app
at one moment with a terminal frontmost, and the answer depends on how the menu
was opened:

| opened by | `isOnScreen` | reported |
|---|:-:|:-:|
| a right click, over its own window | `true` | yes |
| an `AXPress` on its top-level menu bar item | `false` | no |

Both correct. A context menu belongs to the window it was opened over, so a
background app can present one. A menu *bar* menu belongs to the **active** app's
menu bar: pressing a background app's top-level item creates the window and macOS
never puts it on screen, because the menu bar on screen is somebody else's. No
code changed; `examples/popup_visibility.rs` is new and keeps the reading
re-takeable, and DESIGN §10 records it.

## 0.8.0

The safety gate learned to read the question a dialog is asking, a shortcut-less
menu row became reachable, and the function that could steal your keyboard is
gone. Four measurements settled questions 0.7.0 shipped as open — one of them by
proving the fix I had reasoned my way to was worthless.

### A terse button under a destructive question is now refused

`OK` in a sheet asking *Delete 4 items?* used to sail through, because the
classifier read the button and the verb was in the alert. It now reads the
**nearest decision context** — an `AXSheet`, an `AXDialog`, or a window whose
subrole marks it an alert — and judges the button against the question that
context is asking. Four rules, each pinned by a test that fails if the rule is
removed:

- Only a decision context counts. An ordinary window, group or scroll area is
  layout, and its text is content **at any depth** — so a mail thread about
  deleting still clicks normally.
- The nearest one, and no further. Bounded by construction, and it gets nested
  sheets right for free.
- The question only, never the other answers. A Cancel sitting beside a Delete
  is not evidence about Cancel.
- Content is excluded everywhere: no descent into tables, rows, web areas or text
  areas, and a writable value is never the question. A text field inside a delete
  sheet stays writable.

**Cancel, No, Keep, Save, 취소, 저장 are never refused.** An answer that names its
own harmlessness is judged by itself: refusing the way *out* of a destructive
dialog would leave an agent holding a sheet it could only escape by confirming,
which is the exact habit that makes the gate worthless when it matters. Whole
label only, so `Don't Save` is not excused by `Save`, nor `Close Account` by
`Close`. This one is evidence-driven — a live run caught the gate refusing 저장 on
Apple's own save-or-delete sheet, which is one of the most-used sheets on the
system.

Live-verified on this Korean machine, both directions: `OK` under *Delete 4
items?* refused and then pressed with `confirm_destructive: true`; `확인` under
*4개 항목을 삭제할까요?* refused; `Cancel` and `취소` on the same dialogs allowed
unconfirmed; `OK` under *Save these settings?* allowed. The refusal quotes the
question it read.

### Return is judged by the button it will actually press

The same gate had a hole underneath it. Every other check judges the element the
caller named, which is right for a click — but inside an alert, Return activates
the **default** button whatever was addressed. So `press_key return` aimed at a
dialog's Cancel was judged against Cancel, found exempt, and pressed **Delete**.
Measured before the fix on a real dialog: allowed, and `osascript` reported
`button returned:Delete`.

For an unmodified Return the gate now resolves `AXDefaultButton` on the nearest
window-like ancestor — bottom-up, so a sheet's own default wins over its parent
window's — and judges that control instead. The aimed element's value,
settability and caption are dropped in the swap, so a text field cannot excuse the
button Return presses. Escape is untouched (it activates *cancel*, safe by
construction), space presses the focused control, and a modified Return is an app
shortcut rather than "confirm this dialog".

### A shortcut-less menu row is reachable — through the menu bar

0.7.0 established that a pop-up menu opens fine, has no accessibility inside it,
and only yields to an item's own keyboard shortcut. That left rows with no
shortcut with no route at all. There is one, and it was never in the pop-up:
`AXMenuBar` is published **in full** — every menu, submenu and row, each with
`AXPress`, a live `AXEnabled`, its checkmark, and its key equivalent as data.

New **`menu_bar`** tool: read a level, press a row. It refuses a submenu (naming
its rows), refuses a disabled row (pressing one does nothing and reporting success
would be a lie), and runs the destructive gate on the row's own title, because a
menu bar reaches Quit and Leave Chat in two steps.

Measured: `Edit > Transformations > Make Upper Case` — no key equivalent —
changed `alpha bravo charlie` to `alpha BRAVO charlie` with another app frontmost
throughout, and the inverse row put it back byte-identically. Nothing was drawn
on screen and the frontmost app never changed.

Also new: **`menu_shortcut`** reports key equivalents already spelled the way
`press_key` takes them (`cmd+z`, `cmd+alt+shift+v`), so no agent has to recognise
⌘ glyphs in a screenshot. The encoding is a trap worth naming —
`AXMenuItemCmdModifiers` is the Carbon byte where Command is the *default* and
bit 3 removes it, so `0` means ⌘.

And a bound on the no: KakaoTalk's menu bar publishes `카카오톡`, `편집`, `창`,
`도움말`, and none of them contains 톡게시판 or 채팅방 서랍. A row an app draws
*only* in a pop-up is genuinely unreachable without moving the real pointer. The
refusal is now specific rather than general.

### An `AXMenuItem` does act on the first `AXPress`

DESIGN.md §10 carried, from one observation, that it does not — that the first
press only selects the item and a second is needed to make it act. Characterized
now with `cargo run -p cua-ax --example menu_item_press`, a new probe: six arms ×
10 trials × 3 menu-bar toggles in two apps, **180 presses, all 180 acting on the
first press**. No trial in any arm was ever rescued by a second press.

What the original observation saw was the read. Polling the pressed item every
50 ms, the change takes 50 ms to 1.7 s to become readable — up to fourteen times
the 120 ms settle `ui_changed` uses. A read at a fixed short delay reports a
press that worked as having done nothing, and pressing again then undoes it on a
toggle or looks like success on a dialog. `AXSelected` on a menu item is settable
and inert: the write never reads back, and it changes nothing about whether the
press works.

Nothing in the shipped code changed. The practical note for callers is in §10:
after acting on a menu item, re-read that element rather than trusting the
returned diff.

### CI notices a directory under `crates/` that is not a crate

`crates/cua-sky/examples/` sat in the working tree as debris from an abandoned
experiment. Git never carried it — it holds no files, and git does not track
empty directories — so it was never in a commit, a release, or a clone. What let
it linger is that nothing referenced it: `cua-sky` was not a workspace member, so
no build, lint or test step had any reason to name it, and CI's per-crate build
loop spelled its six crates out by hand.

CI now derives that list from `cargo metadata` and asserts it matches `ls crates`
in both directions. A directory that is not a member fails the job, and so does a
member whose directory is gone. Checked against the case that motivated it:
recreating `crates/cua-sky/examples/` makes the step fail with `> cua-sky`.

No other reference to a crate that does not exist was found — not in
`Cargo.toml`, `.github/workflows/`, `install.sh`, `README.md` or `DESIGN.md`.

### `cua_hid::post_chord` is deleted

It posted a key or chord through `CGEventPost(kCGHIDEventTap)` — the session's
one shared keyboard stream — so it went to whatever app had focus and took that
focus from the human. It was the only function in `cua-hid` that did, and it
existed because the crate was built believing there was no per-app keyboard
route: without it, an arbitrary chord, a terminal and a canvas app that reads
only real key events were unreachable.

Nothing needs it now. `press_chord_background_pid` and
`type_text_background_pid` deliver keys per-pid, `press_key` uses the former as
its only tier, and `type_text mechanism: "keystrokes"` uses the latter — which
took away its last stated reason to exist, being the only way to type into a
terminal. It has been unreachable from the server since 0.3.1 removed the flag
that reached it, and no example or probe called it either, so nothing was
rewired: the function went, and with it `cua-hid`'s `CGEventTapLocation` import.

That import is the point. It is the only argument `CGEventPost` takes, so with it
gone from the file that does the posting, a shared-stream write cannot be added
there without putting it back first — the same check the absent
`CGWarpMouseCursorPosition` gives for the cursor. `grep -rn 'CGEvent::post'
crates/*/src/` now returns only `post_to_pid`, and the one surviving
`CGEventTapLocation` in `src/` is `humanwatch`'s listen-only tap, which creates a
tap and posts nothing. DESIGN.md §11 has the reasoning; §10 and the crate docs no
longer describe a fallback that exists.

`parse_chord` is untouched: it is shared with the pid keyboard path. Nothing
tested `post_chord` — there was no way to assert on a write to the shared stream
— so the workspace test count is unchanged at 212.

### The shell capture that refused a pop-up id was a dead window id

An unexplained observation from the pop-up work: a shell
`/usr/sbin/screencapture -x -o -l<menu_window_id>` exited 1 with `could not
create image from window` while every in-process `capture_window` around it
succeeded. Chased with `cargo run -p cua-core --example popup_capture_probe`,
which opens a pop-up and alternates both paths against its window id while
reading the window's liveness between every step.

129 live rounds across four runs: both paths succeeded every time. Asked for the
same id *after* the pop-up closed, the shell command fails with exactly that
message, 3 out of 3. The in-process path normally reports `window <id> not found
(it may have closed)` instead, because it enumerates first — which is the whole of
the apparent disagreement. One round caught the raw refusal in-process too, when
the window died inside the gap between that enumeration and the capture.

So a pop-up id is not a durable handle, and §10's answer — ask for the parent
window, which macOS photographs with the pop-up attached — remains the way to get
a pop-up's pixels. `capture_failure_warning` is unchanged and undiminished: it
describes a *live* window of an app with a menu open, which is a different case
and still an unestablished correlation.

### Measurements that closed open questions

- **`hover` drives web content, and nothing else tried.** A local fixture page
  publishes a button CSS hides until its row is hovered, and rewrites a paragraph
  with the coordinate a `mousemove` handed it — a coordinate the app *computed*
  cannot be produced by anything but the event arriving with it. Confirmed in two
  engines. A Finder list row, with a click as the control, showed nothing.
- **The wheel tier's explanation was wrong.** §6 says a click only lands once the
  `NSEvent` is built first, and `NSEvent` has no scroll-wheel factory, so "that is
  why scrolling fails" was the convenient guess. Six constructions — as shipped,
  round-tripped through `+[NSEvent eventWithCGEvent:]`, phased, and phased with
  momentum — across both the private and public post routes: all delivered, all
  moving zero pixels (`260063 bytes -> 260063 bytes`) while the keystroke control
  scrolled the same document before and after. Not the header, not the phases, not
  the route. Recorded as unexplained rather than guessed at twice.
- **The `AXMenuItem` first-press rumour is refuted.** 180 presses across six arms,
  two apps and three toggles: 180 acted on the first press. What lags is the
  *read* — 50 ms to 1.7 s before the change is visible.
- **A pop-up window id is not a durable handle.** The one unexplained
  `screencapture` failure from 0.7.0 reproduces exactly when the id has closed,
  3 of 3, and `capture_window`'s preflight enumeration is the whole apparent
  asymmetry between the two paths.

### The settle polls, and stays short on purpose

`ui_changed`'s 120 ms wait was a fixed sleep; it now polls every 16 ms and returns
the moment the fingerprint moves, so a change that lands in one frame is reported
after one frame. Only "nothing changed" spends the deadline, which it must.

The 1.7 s menu-read figure above invited a longer deadline, and measurement
refused it: a 2 s deadline reported `Unchanged` after waiting the full 2 198 ms on
a press that had plainly worked, because the fingerprint reads the focused
window's title and the focused element — and a tab bar appearing changes neither.
The limit is *what* is compared, not *when*. The patient variant was deleted
rather than shipped as a tax that buys nothing; for a menu action, read the row
back through `menu_bar`, whose title and checkmark say the state outright.

### Also

- A browser is outside this server's one property, measured: **Chrome and Safari
  accept no synthesized pointer input while backgrounded** — a click and a
  `mouseMoved` at the same pixel both ignored, both honoured once active, while a
  background click on TextEdit worked in the same session. Not a gap to close;
  it is a second reason the README already sends the web to browser-rs over CDP.
- CI derives the crate list from `cargo metadata` and asserts it matches
  `ls crates` both ways, which is how an empty `crates/cua-sky/` went unnoticed.
- **212 → 249 tests**, all still passing with no permissions granted.
- New probes: `hover_check`, `menu_item_press`, `popup_capture_probe`, and
  `scroll_check` gained an `idle` instrument arm and `CUA_WHEEL_RECIPE`.

## 0.7.0

The mouse model widened, a safety layer arrived, and four coordinate bugs came
out of finally measuring things that had been shipped as "built, not yet
verified". Two capabilities were retired by measurement rather than added: the
wheel scroll tier now refuses, and picking a row in a pop-up menu by coordinate
is documented as impossible rather than merely unreliable.

Read [DESIGN.md](DESIGN.md) §11 before relying on any new mouse capability. Each
one now says whether it was measured on a real app, and three of them were.

### The mouse model is no longer one left click

`cua-hid`'s primitive went from "a left click with a count" to
`{origin, destination, button, modifiers, click_count}`, and five capabilities
are reachable end to end.

| | status |
|---|:--|
| `button: right \| middle` | **measured** — a right-click opened a context menu on TextEdit's text view, detected as a new level-101 window |
| `modifiers: cmd \| shift \| alt \| ctrl \| fn` | **measured** — a ⇧-click extended a selection an unmodified click leaves empty |
| `drag` | **measured** — dragging 220 points across TextEdit left `AXSelectedText` spanning exactly that run. Down, interpolated `mouseDragged`, up; both ends in one window, either end an element or a bare pixel |
| `hover` | **unproven.** Delivered; no app has been found whose hover state enters the tree. Your cursor does not move, so an app that polls the *real* pointer position cannot react at all |
| wheel scroll tier | **refused.** See below |

The modifier vocabulary is shared with `press_key`, so `cmd+shift` means the same
thing written on a click as on a key.

### The wheel tier is refused, because it does not scroll

Measured against the window's own pixels: a pid-routed `scrollWheel` is delivered
and moves nothing, on a native `AXScrollArea` holding a 400-line document and on
Chromium web content, in both pixel and line units. The control arm is what makes
that conclusive — a pid-routed `pagedown` keystroke scrolls the same window in the
same run, so the failure is the scroll event, not the routing, the window number,
the aim point, or the instrument.

`scroll` therefore **errors** where it would have used that tier, and the error
names `press_key` with `pagedown` / `pageup` / `down` / `up`, which does reach a
scroller publishing no scroll action. Delivering it while documenting it as
unreliable was the worse option: a caller told `delivery: pid` concludes the
scroll happened and reads a stale tree as the new state, and a wrong belief costs
more than an error. `CUA_WHEEL_SCROLL=1` restores delivery for re-running the
experiment. Page scrolls through an advertised accessibility action are unchanged.

### Pop-up menus: visible now, and only operable by shortcut

A click on a control that advertises no accessibility action — a chat app's
chat-room hamburger is the standing case — does open its menu. That was never the
problem. The menu is a separate window at level 101 with **no accessibility
representation at all**, so it was in neither the tree nor the screenshot, and
`ui_changed` reported `no` while a 202x318 menu sat on screen.

- `get_app_state` **and every action's own result** now list the app's transient
  windows: id, level, frame, and whether each appeared while the action ran. In
  the action's own response deliberately — a caller told one round trip later has
  already concluded the control did nothing.
- A new transient window makes `ui_changed` say `yes` on its own evidence.
- The window screenshot already contains the menu: macOS photographs a window
  together with the pop-up attached to it.
- `click_in_window` may now be aimed at a pop-up, which works for a popover or
  panel and is honestly labelled as **not** the way to pick a menu row.

Picking a menu row by coordinate was measured twice on two rows: the event is
delivered, the menu closes, and no item activates — including on a run where the
human's pointer hovered a different row, so it selects none rather than the wrong
one. A macOS menu tracks the *real* pointer, which cua-rs does not move, so this
is §9's pointer-position case and permanent. **The item's keyboard shortcut does
work**: `press_key cmd+t` activated `항상 위에 유지`, verified by the window
moving from level 0 to level 3 and back. No OCR, and no parsing shortcuts out of
the image — a misread `⌥⌘⌫` presses "leave the chat room".

### Four coordinate bugs

All four were silent: the event was delivered, the result said success, and the
coordinate was wrong.

- **Chromium's activation point is a lie.** It answers
  `AXActivationPoint = (0, 982)` for every element in the window, and
  `element_point` asked it first, so every pid-routed click, hover, drag and wheel
  aimed at a Chrome element went to one corner of the display. An activation point
  outside its own element's frame is self-contradictory and is now discarded for
  the frame centre. No app-specific knowledge, no allow-list.
- **A capture's `scale` was computed against a frame the image did not cover.**
  Because macOS includes an attached pop-up in the picture, `width / frame.width`
  read 2.37 px/pt for a window and 10.83 for a menu instead of 2.0 — so every
  pixel-to-point conversion was wrong while a menu was up. The real extent is now
  recovered by testing candidate rects against the pixel count.
- **A scrollable element's frame is not its viewport.** A web area's frame is the
  whole document, so its centre can be far outside the window; the aim is pulled
  into the intersection.
- **`Target::Point` was resolved and then discarded.** A caller who named a
  coordinate to scroll at was scrolled at the element's point instead.

### A keycode is not a character

`press_key x` delivered `ㅌ` under a Korean 2-set input source, which is the
correct answer to "the user pressed keycode 7" and the wrong answer to "the caller
asked for x". The event was under-specified. `Chord` now carries the literal
character and the event carries **both**: the real keycode, so an app reading
`keyCode` for a game control or a shortcut still sees the physical key, plus the
Unicode string AppKit hands to a text view. Not applied to a chord or a named key
— `cmd+x` is Cut, and `escape` has no character to force.

### `press_key` and `type_text`: where the keys actually went

- Every keyboard result reports `focus: verified | unverified | mismatched`,
  derived from the app's own `AXFocusedUIElement`. `AXFocused`'s write result is
  reported instead of discarded. `CUA_KEY_STRICT_FOCUS=1` refuses on
  `mismatched`.
- `type_text` takes `mechanism: "ax" | "keystrokes"`. The default is unchanged —
  one atomic, element-addressed `AXValue` write — and `keystrokes` sends real
  per-pid key events for the targets that ignore `AXValue`, measured on TextEdit
  and not yet on a terminal.
- The read-back debt §10 owed since 0.6.0 is paid: three live tests address a text
  element, send keys, and read the element's `AXValue` back, plus a negative test
  that addresses element A and asserts element B did not receive it.
  `cargo test -p cua-core --test live_keyboard -- --ignored --test-threads=1`.
- Found by those tests: **a window that has never been clicked swallows
  keystrokes silently.** An app can be frontmost while publishing no
  `AXFocusedUIElement` at all; pid-routed keys then land nowhere and nothing
  errors. That state is exactly what `focus: unverified` reports.
- Also found: the staleness guard rejected *every* click on TextEdit's document
  view, because the tree walk resolved a label through five attributes while the
  live re-read compared only two.

### A safety layer, and a scope

Six gates, checked once in the single place every action already passes through,
so a tool added later is gated by default.

| gate | default | control |
|---|:--|:--|
| session scope | **off** | `CUA_ALLOWED_APPS=id,id` |
| credential and security apps | on | `CUA_ALLOW_FORBIDDEN_TARGETS=1` |
| destructive label or key | on | per-call `confirm_destructive: true` |
| locked screen or screen saver | on | — |
| yield to the human | **off** | `CUA_YIELD_TO_HUMAN=1`, `CUA_YIELD_IDLE_MS` |
| HTTP bearer token | on in HTTP mode | `CUA_HTTP_TOKEN` |

`CUA_ALLOWED_APPS` is the recommended posture and the only gate that is a scope
rather than a heuristic: the other five enumerate danger and therefore admit every
app nobody thought of. It narrows only — scoping a run *to* a password manager
does not lift the floor — matches whole bundle identifiers so `com.apple` cannot
mean every Apple app, refuses a process with no bundle id, and can only be set by
the human who launched the server. There is deliberately no tool to widen it. An
*empty* value refuses everything rather than reopening the scope, because
`CUA_ALLOWED_APPS=$TYPO` expands to exactly that and a gate that opens on a
misspelling fails in the wrong direction.

`CUA_WHEEL_SCROLL=1` is the one switch here that turns on something known not to
work, and exists so the wheel measurement can be re-run.

Reads stay allowed on a blocked app, because a refusal you cannot explain is
worse; the screenshot is the exception, since pixels reproduce the secret rather
than describing it. The destructive classifier over-reports on purpose and covers
Korean labels (삭제, 제거, 초기화, 나가기). Yield uses a listen-only tap that
returns every event unchanged — it reads the shared input stream and never writes
to it — and fails closed if the tap cannot be created.

**HTTP mode now requires a bearer token.** Loopback is not an authorization
boundary: any local process, including a web page, could previously drive the
whole desktop. `/health` stays open.

### The drawn cursor ships in the release

`cua-overlay` is built, signed and uploaded alongside `cua-rs`, and `install.sh`
puts it in the same directory. Until now the prebuilt release shipped `cua-rs`
alone, so everyone who installed the documented way had no on-screen feedback at
all. A pinned older `CUA_VERSION` without the asset still installs cleanly.

Also fixed: the overlay was resolved as a sibling of `cua-rs` without following
symlinks, so it was never found when `cua-rs` was reached through one — which is
every `curl | sh` install.

### Picture-in-picture: measured and declined

Mirroring the driven window into an always-visible panel would be the complete
answer to "you cannot see what the agent is doing". On the current capture path it
tops out at **5–8 fps while holding ~39% of one core**: `screencapture -l` alone
is 70–100 ms per frame. Clearing that needs `SCStream`, which needs a signed
notarized bundle with its own Screen Recording grant — a second permission prompt
and the end of the one-binary install. Declined, with the numbers and the one
measurement that would change the answer, in DESIGN §12.

### Internals

- The listen-only input tap moved from `cua-core` to `cua-hid::humanwatch`.
  "Only `cua-hid` links the event APIs" is checkable from five `Cargo.toml` files;
  it had degraded to a claim about code. `cua-core` links `CGSession` alone.
- `is_plausible_target()` still caps at level 3 — it answers which window a
  snapshot is *of*, and a menu chosen there would stamp its number onto content
  clicks. The new `is_addressable_target()` answers the different question of what
  an event may be aimed at.
- 107 → **212 tests**, all still passing with no permissions granted. One was
  deleted rather than added: a coordinate-guard test asserted only that an
  unresolvable app cannot succeed, while its name promised the generation guard
  was covered. The ones
  that need a grant or a GUI session are `#[ignore]`d and documented.
- New probes: `scroll_check` (wheel versus keystroke, judged on pixels),
  `mouse_verify`, `menu_life`, `keyboard_probe`.

## 0.6.0

The tier order flipped. `click` and `press_key` now go through pid-routed
delivery unconditionally, with no accessibility attempt in either direction, and
`press_key` accepts arbitrary chords for the first time. `set_value` and
`type_text` are deliberately untouched.

Read the last section of this entry before turning it on anywhere that matters:
the keyboard half ships **unverified**, on purpose and on the record.

### `click`: pid only, no AX and no retry

Through 0.5.x, `click` tried `AXPress`/`AXPick`/`AXConfirm` first and used the
pid tier only where an element advertised no action. Now every click is a mouse
event routed to the target process, and a failure is a failure — there is no
`AXPress` attempt before it and no retry through one afterwards.

The reason is not that pid delivery is faster; it is not. It pays a window
re-enumeration, a window-identity match and an `AXFrontmost` poll before it sends
anything, where `AXPress` is one synchronous IPC call. The reason is that
accessibility was never the delivery mechanism, only the way cua-rs decides
*where* to click, and it cannot express a click count at all — so a double-click
was already pid-only and the tier boundary was arbitrary. One route per action
removes a case analysis rather than adding a mechanism.

Dropping the retry is the substantive part. Retrying a failed pid click through
`AXPress` reads as free insurance and is not: it reintroduces exactly the quirks
the pid tier exists to escape — an element that advertises `AXPress` and silently
ignores it, an action that fires while the visual state lags, a stale handle
recycled onto other content that would still happily accept a press. "Try A, and
if that seems to have failed, also try B" is where the surprising bugs live.

`CUA_AX_FIRST=1` restores the 0.5.x order. It is a bisecting tool, not a
supported "best of both" mode.

### `press_key`: arbitrary chords, and a delivery mode with a caveat

`press_key` used to map `return`/`enter` to `AXConfirm`, `escape` to `AXCancel`,
`up`/`down` to `AXIncrement`/`AXDecrement`, and refuse everything else. It now
parses and sends real key events, so `cmd+shift+p`, `ctrl+alt+delete`, plain
letters and digits all work — closing the "arbitrary chord — no verb exists,
still refused" row that §1 of DESIGN.md had listed as a permanent ceiling.

Nothing was discovered to make this possible; `press_chord_background_pid` has
been written and unreachable for several releases. What changed is the decision to
call it, and the argument for calling it is that accessibility has no vocabulary
for a key press beyond `AXConfirm` and `AXCancel`. A real event is the only thing
`⌘⇧P` could ever be, so there is no second tier to fall back to.

Results carry `delivery: pid (keyboard)`, a distinct label from `pid`, because
what it promises is different — see below.

`CUA_KEY_AX_ONLY=1` restores the AX-verb-only path (`return`/`escape`/`up`/`down`
only, chords refused).

### `set_value` and `type_text` did not move, on purpose

They still write `AXValue`/`AXSelectedText`. A bulk text write is the one
operation accessibility expresses *better* than events can: one call, atomic,
addressed at the element. The same text as keystrokes is a long stream landing on
whatever holds focus, character by character, which multiplies the risk below by
the length of the string and buys nothing. `type_text_background_pid` stays
written and unwired for the same reason.

So an app that only reacts to real key events still ignores both, exactly as
before. `press_key` is the way to reach it, one key or chord at a time.

### What this release does not have: verification of the keyboard path

A pid key event carries a target **pid** and no target **element**. It lands
wherever that process's own first responder currently is. `cua-core`
best-effort-focuses the addressed element first via `AXFocused`, but accessibility
does not make every element settably focused, and there is no query that confirms
the focus moved before the keystrokes did.

That is why this sat gated for several releases: a click that misses does nothing,
while a keystroke that misses types into whatever the human was editing. Being the
only possible mechanism is an argument for the design — it is not evidence that a
key lands where it was aimed, and no control-and-measure run has been done. Earlier
drafts of DESIGN.md papered over this by pointing at another implementation's
behaviour as though it were cua-rs's own measurement; §10 now states the debt
plainly instead.

Treat `press_key` on an app you have not tried yourself as unproven. Verify on a
target whose text can be read back, and use `CUA_KEY_AX_ONLY=1` if it misbehaves.

### Other

- `Delivery` gains `PidKey` (`pid (keyboard)`). Callers matching exhaustively need
  the arm.
- New errors: `PidClickFailed`, `PidKeyUnavailable` — the pid tier's own failure
  modes, distinct from `PidClickUnavailable`, which meant "accessibility was tried
  and the quiet fallback also failed".
- README trimmed to essentials.

## 0.5.2

Three defects an external review of 0.5.1 found in `cua-overlay`. All of them are
about the arrow being in the wrong place or absent, none about input: this process
still delivers nothing and ignores mouse events entirely.

### The arrow could not appear above a menu

The overlay sat at `setLevel(0)` and relied on `orderWindow(Above, target)` to
place it. Ordering cannot cross a level boundary — `NSWindow` levels are bands,
and every window at level 3 renders ahead of every window at level 0 — so a
level-0 overlay ordered "above" a floating or torn-off-menu window at layer 3
still drew behind it. `cua_capture::is_plausible_target` accepts targets up to
layer 3 precisely because menus live there, so the arrow went missing for exactly
the controls that are hardest to hit and most worth annotating.

The overlay now reads the target's `kCGWindowLayer` from the same on-screen
window-list lookup the visibility gate already performs, and adopts a matching
level before ordering itself — order matters, since `setLevel:` reorders within
the new band and would otherwise discard the placement. The level is clamped to
the same 0..=3 band the capture layer will accept as a target, and deliberately
not passed through: the on-screen list also contains the Dock near `i32::MIN` and
system UI in the thousands, and a mis-read entry that moved a click-through
window into one of those bands would put it above every app on the machine. The
level is re-checked each tick, not just at pin time, so a panel that goes floating
or a menu that tears off while the arrow is on it is followed rather than lost.

### The overlay covered only the main display, forever

The window was created once from `NSScreen::mainScreen().frame()` and never
touched again. Callers pass *global* screen points, so an element on a second
display was handed to a window that does not reach it and the arrow was clipped
away; and a resolution change or a display being unplugged left the window
covering an area that no longer existed.

It now spans the union of every screen and re-reads that union each tick, moving
with the layout. Converting a caller's point into the view needs a real
translation once the union is not the main screen — the caller's origin is the
main display's top-left with y down, AppKit's is the main display's bottom-left
with y up, and the flipped view's origin is the union's top-left — so the
arithmetic is a pure function with unit tests, including the negative-origin
"second display to the left" case DESIGN §6 had flagged as untested. The
single-display result is asserted to be the identity, which is the part a machine
with one monitor can actually prove, and it was: `ready on 1512x982`, unchanged.

When the layout does move, the marker keeps its screen coordinates and the spring
is re-seeded from them, so the arrow stays on the pixel it was pointing at instead
of sliding across the screen by the difference.

### Re-verified

Driving the release binary by hand against a visible background window: the arrow
painted; `hide`, a nonexistent window id, a `NaN` coordinate, window id `0` and a
missing pid each produced a frame byte-identical to the blank baseline from the
0.5.1 verification. The layer-3 path is covered by unit tests and by inspection
only — no floating-level target was available to point at live.

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
