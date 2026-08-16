//! Read-back tests for the pid-routed keyboard path: send keys at one element,
//! then read that element's text and check the keys arrived there.
//!
//! # Why these are `#[ignore]`d
//!
//! Everything else in this workspace runs with no macOS grants, which is what
//! lets CI mean anything. These cannot: they need Accessibility, a GUI session
//! with a window server, and TextEdit. A hosted runner has none of the three,
//! so they are `#[ignore]`d and run by hand — DESIGN.md §8 has the command.
//!
//! # Why they are worth the inconvenience
//!
//! DESIGN.md §10 said for several releases that the pid keyboard path was
//! "written but unproven": a keystroke goes wherever the target process's own
//! first responder is, `AXFocused` is best-effort, and nothing confirmed the
//! focus moved before the keys did. That is a claim only measurement can
//! settle, and the measurement needs a target whose text can be read back.
//! TextEdit is that target: it ships with macOS and publishes its document as a
//! plain `AXValue` on an `AXTextArea`.
//!
//! Three claims, one test each:
//!
//! 1. keys sent at a text element arrive **in that element**;
//! 2. keys sent at element A do not arrive in element B, its sibling in the
//!    same window — the only place a pid-routed keystroke can go wrong at all,
//!    since the event never leaves the target process;
//! 3. when the keys were going to land somewhere other than the addressed
//!    element, the reported `focus` verdict says so instead of claiming
//!    success. This is the honesty check, and it is what justifies the field
//!    existing: a verdict allowed to be wrong is worse than no verdict.
//!
//! # What they do to the machine
//!
//! They open scratch documents in TextEdit, which activates TextEdit for real —
//! the harness doing that through `open(1)`, not cua-rs. Nothing here calls
//! `activate`, warps the pointer or posts to the shared event stream; the
//! clicks and keystrokes under test all go out cua-rs's per-pid route. Scratch
//! documents are closed without saving at the end of each test.
//!
//! # One measured prerequisite, learned from these tests failing
//!
//! A window that has never been clicked can be frontmost and still have no key
//! window: TextEdit then publishes no `AXFocusedUIElement` at all and swallows
//! pid-routed keystrokes silently. cua-rs reports that state as
//! `focus: unverified`, which is exactly what it means — nothing here says
//! where the keys went. So these tests click the target first, which is also
//! the advice the tool gives, and the honesty test is the one that pins down
//! what happens when they do not.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::{Duration, Instant};

use cua_core::{Cua, FocusState, Mechanism, StateOptions, Target};

/// Reading the tree is all these tests need; the screenshot is the expensive
/// half of a snapshot and nothing here looks at pixels.
fn no_screenshot() -> StateOptions {
    StateOptions {
        include_screenshot: false,
        ..Default::default()
    }
}

/// Prefix every scratch document shares, so cleanup can find them and cannot
/// close a document the human was working on.
const SCRATCH_PREFIX: &str = "cuarsdoc";

/// A string no app would produce on its own, so finding it anywhere is
/// evidence rather than coincidence.
fn sentinel(tag: &str) -> String {
    format!(
        "cuars{tag}{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000
    )
}

/// One actionable line of a rendered tree, split into the parts these tests
/// match on.
#[derive(Clone)]
struct Line {
    index: usize,
    role: String,
    text: String,
}

/// Actionable lines of a rendered outline, in order.
///
/// Deliberately parsed from the rendered tree rather than reached around it
/// into `AxNode`: this is the surface an agent reads, and a test that bypassed
/// it could pass while what a caller sees was wrong.
fn lines(tree: &str) -> Vec<Line> {
    tree.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let (index, rest) = trimmed.strip_prefix('[')?.split_once("] ")?;
            Some(Line {
                index: index.parse().ok()?,
                role: rest.split([' ', ':']).next()?.to_string(),
                text: trimmed.to_string(),
            })
        })
        .collect()
}

fn find_role(tree: &str, role: &str) -> Option<Line> {
    lines(tree).into_iter().find(|l| l.role == role)
}

fn require_role(tree: &str, role: &str) -> Line {
    find_role(tree, role).unwrap_or_else(|| panic!("no {role} in\n{tree}"))
}

fn target(index: usize, snapshot_id: u64) -> Target {
    Target::Index {
        index,
        snapshot_id: Some(snapshot_id),
        // Left off on purpose: these tests re-read between every step, and the
        // role guard would fire on TextEdit relabelling its own text view
        // rather than on the staleness it exists to catch.
        expected_role: None,
    }
}

/// Snapshot TextEdit's front window.
fn snapshot(cua: &Cua) -> (u64, String) {
    let state = cua
        .get_app_state("TextEdit", no_screenshot())
        .expect("read TextEdit");
    (state.snapshot_id, state.tree)
}

/// Open a scratch document and wait until cua-rs can read it back.
struct Scratch {
    cua: Cua,
    path: std::path::PathBuf,
}

impl Scratch {
    fn open(body: &str) -> Self {
        let cua = Cua::new();
        let path = std::env::temp_dir().join(format!("{SCRATCH_PREFIX}{}.txt", sentinel("")));
        std::fs::write(&path, body).expect("write scratch document");
        Command::new("/usr/bin/open")
            .arg("-a")
            .arg("TextEdit")
            .arg(&path)
            .status()
            .expect("open the scratch document in TextEdit");

        // "The app is running" and "the window publishes its text" are
        // different moments, and only the second one is useful.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(state) = cua.get_app_state("TextEdit", no_screenshot()) {
                if state.tree.contains(body) && find_role(&state.tree, "AXTextArea").is_some() {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "TextEdit never published a text area containing {body:?}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }

        let scratch = Self { cua, path };
        // Make the window key. A window that has never been clicked can be
        // frontmost with no key window at all, and pid-routed keys are
        // swallowed in that state — see the module docs.
        scratch.click_text_area();
        scratch
    }

    /// A pid-routed click on the document's text area, which is what makes the
    /// window key and puts the first responder in the text.
    fn click_text_area(&self) {
        let (snapshot_id, tree) = snapshot(&self.cua);
        let area = require_role(&tree, "AXTextArea");
        self.cua
            .click(
                "TextEdit",
                target(area.index, snapshot_id),
                cua_core::MouseOptions::default(),
                false,
                false,
            )
            .expect("click the text area");
        std::thread::sleep(Duration::from_millis(300));
    }

    /// The document's current text, read back through a fresh snapshot.
    fn document_text(&self) -> String {
        require_role(&snapshot(&self.cua).1, "AXTextArea").text
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Close only what this test opened, by name. Never `quit`: the human
        // may have their own unsaved documents open.
        let _ = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!(
                "tell application \"TextEdit\" to close (every document whose name starts with \"{SCRATCH_PREFIX}\") saving no"
            ))
            .status();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open the find bar, whose search field is the sibling element the negative
/// test checks. Returns its line from a fresh snapshot.
fn open_find_bar(scratch: &Scratch) -> Line {
    let (snapshot_id, tree) = snapshot(&scratch.cua);
    let area = require_role(&tree, "AXTextArea");
    scratch
        .cua
        .press_key(
            "TextEdit",
            target(area.index, snapshot_id),
            "cmd+f",
            false,
            false,
        )
        .expect("cmd+f");
    std::thread::sleep(Duration::from_millis(700));
    let (_, tree) = snapshot(&scratch.cua);
    require_role(&tree, "AXTextField")
}

#[test]
#[ignore = "needs an Accessibility grant, a GUI session and TextEdit"]
fn sends_keys_into_the_addressed_element() {
    let scratch = Scratch::open("read-back target");
    let (snapshot_id, tree) = snapshot(&scratch.cua);
    let area = require_role(&tree, "AXTextArea");

    let typed = sentinel("hit");
    let result = scratch
        .cua
        .type_text(
            "TextEdit",
            target(area.index, snapshot_id),
            &typed,
            Mechanism::Keystrokes,
            false,
        )
        .expect("keystrokes must be delivered");

    let focus = result
        .focus
        .as_ref()
        .expect("the keystroke path must report a focus verdict");
    let after = scratch.document_text();
    assert!(
        after.contains(&typed),
        "the addressed text area did not receive {typed:?} (focus was {}): {after}",
        focus.state.as_str()
    );
    // The label and the verdict are what a caller reads. Landing the text and
    // then describing it wrongly is still a bug.
    assert_eq!(result.delivery.as_str(), "pid (keyboard)");
    assert_eq!(
        focus.state,
        FocusState::Verified,
        "the text arrived at the addressed element, so the verdict must say so"
    );
}

#[test]
#[ignore = "needs an Accessibility grant, a GUI session and TextEdit"]
fn does_not_type_into_the_other_text_element() {
    // Two text elements, same window, same process: the document and the find
    // bar's search field. A pid-routed keystroke cannot reach another app at
    // all, so this pair is the whole blast radius.
    let scratch = Scratch::open("negative control");
    let field = open_find_bar(&scratch);
    // ⌘F leaves focus in the find field, so put it back in the document —
    // the state a caller who clicked the element they are addressing is in.
    scratch.click_text_area();

    let (snapshot_id, tree) = snapshot(&scratch.cua);
    let area = require_role(&tree, "AXTextArea");
    let typed = sentinel("neg");
    let result = scratch
        .cua
        .type_text(
            "TextEdit",
            target(area.index, snapshot_id),
            &typed,
            Mechanism::Keystrokes,
            false,
        )
        .expect("keystrokes must be delivered");
    assert_eq!(
        result.focus.as_ref().map(|f| f.state),
        Some(FocusState::Verified),
        "this test only means something when the addressed element held focus"
    );

    let (_, after) = snapshot(&scratch.cua);
    let field_after = lines(&after)
        .into_iter()
        .find(|l| l.role == "AXTextField")
        .map(|l| l.text)
        .unwrap_or_else(|| panic!("the find field vanished:\n{after}"));
    assert!(
        !field_after.contains(&typed),
        "the find field received text addressed at the document: {field_after}"
    );
    assert!(
        require_role(&after, "AXTextArea").text.contains(&typed),
        "the addressed document did not receive it either — where did it go?\n{after}"
    );
    // Belt and braces: the sentinel appears exactly where it was aimed and
    // nowhere else in the window.
    assert_eq!(
        after.matches(&typed).count(),
        1,
        "the sentinel turned up in more than one element:\n{after}"
    );
    let _ = field;
}

#[test]
#[ignore = "needs an Accessibility grant, a GUI session and TextEdit"]
fn the_focus_verdict_predicts_where_the_text_lands() {
    // Deliberately address something that cannot take focus — the scroll area
    // wrapping the text — while the text area holds it. The keys go to the
    // first responder regardless, so this is a real misdelivery, and the only
    // question is whether cua-rs admits it.
    let scratch = Scratch::open("verdict check");
    let (snapshot_id, tree) = snapshot(&scratch.cua);
    let container = require_role(&tree, "AXScrollArea");

    let typed = sentinel("verdict");
    let result = scratch
        .cua
        .type_text(
            "TextEdit",
            target(container.index, snapshot_id),
            &typed,
            Mechanism::Keystrokes,
            false,
        )
        .expect("keystrokes must be delivered");
    let focus = result.focus.expect("focus verdict");
    let landed_in_document = scratch.document_text().contains(&typed);

    match focus.state {
        FocusState::Verified => panic!(
            "the scroll area is not the app's focused element, so `verified` cannot be right"
        ),
        FocusState::Mismatched => {
            assert!(
                landed_in_document,
                "`mismatched` named the text area as the real recipient, and the text is not there"
            );
            assert!(
                focus
                    .focused_instead
                    .as_deref()
                    .unwrap_or_default()
                    .contains("AXTextArea"),
                "the verdict must name what held focus instead, got {:?}",
                focus.focused_instead
            );
        }
        // Says nothing, so nothing to contradict — but it is a weaker answer
        // than this situation deserves, so record it rather than pass quietly.
        FocusState::Unverified => {
            eprintln!("focus was unverified; text in document: {landed_in_document}");
        }
    }
}
