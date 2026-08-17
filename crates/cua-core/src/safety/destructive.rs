//! The gates that sit between a tool call and a synthesized event.
//!
//! Everything else in this workspace answers "can cua-rs reach this control".
//! This module answers "should it". The two questions are kept in separate
//! files on purpose: the delivery path is judged on whether the event lands,
//! and a refusal is judged on whether a human reading the transcript afterwards
//! agrees with it. Mixing them makes both harder to review, and `session.rs` is
//! already the largest file here.
//!
//! # The five gates
//!
//! | gate | default | flag | scope |
//! |---|:--|:--|:--|
//! | session scope | **off** | `CUA_ALLOWED_APPS=id,id` enables | actions |
//! | forbidden target | **on** | `CUA_ALLOW_FORBIDDEN_TARGETS=1` disables | actions + screenshots |
//! | destructive label or question | **on** | none — per-call `confirm_destructive` | activation-shaped actions |
//! | screen lock | **on** | none | actions |
//! | yield to human | **off** | `CUA_YIELD_TO_HUMAN=1` enables | actions |
//!
//! The three in the middle are on by default because the cost of a false refusal
//! is one round trip and an error message that says exactly how to proceed, while
//! the cost of a false permit is a click that cannot be taken back. Yield is off
//! because turning it on installs an event tap, which is a reversal of a
//! documented policy (DESIGN.md §9) and should be a choice rather than a
//! surprise. The session scope is off because it is the one gate that cannot be
//! guessed: only the human launching the server knows what the run is for, and
//! defaulting it to anything would either break every existing install or mean
//! nothing.
//!
//! # Why reads are not gated the same way as actions
//!
//! The blocklist refuses *acting* and allows *reading*, with one exception. The
//! split is not squeamishness about a hard line — it follows from what each
//! operation can do wrong:
//!
//! - An action is irreversible and mis-aimable. cua-rs resolves a target from a
//!   tree it walked some milliseconds ago, and DESIGN.md §10 already records
//!   that its own change-detection is a heuristic. In a password manager the
//!   difference between the control cua-rs meant and the one next to it is the
//!   difference between reading a vault and emptying it.
//! - A tree read is bounded by what accessibility publishes, which is the same
//!   thing any screen reader on the machine can already see, and it is what
//!   makes a refusal diagnosable at all — an agent that cannot even call
//!   `list_apps` on a blocked app cannot explain to its user why it stopped.
//!
//! The exception is the screenshot. Pixels are the one read that reproduces the
//! secret itself rather than a description of the UI holding it, so
//! `get_app_state` on a forbidden target returns the tree and drops the image,
//! with a warning saying so. That keeps the blocked app observable enough to
//! reason about and stops the most direct exfiltration path.
//!
//! # Bias
//!
//! Every heuristic here is tuned to over-refuse. A false positive costs one
//! extra tool call whose error text names the exact parameter that clears it; a
//! false negative costs a deleted conversation. Where the two could be traded
//! off, this module takes the false positive.

use std::sync::OnceLock;

use crate::apps::AppInfo;
use crate::session::Target;

// ── refusals ─────────────────────────────────────────────────────────────────

/// Why an action was refused before it was attempted.
///
/// Every variant names what was refused and what the caller can do about it.
/// An error an agent cannot act on just becomes a retry loop.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Refused {
    #[error(
        "refusing to {verb} in `{app}` ({bundle_id}): cua-rs treats this app as a forbidden \
         target because it {why}. A synthesized event here can expose or destroy a credential, \
         and nothing in cua-rs can tell the safe control from the unsafe one reliably enough to \
         risk it. Reading is still allowed (get_app_state, find, list_apps), except for \
         screenshots. To act on it anyway, set CUA_ALLOW_FORBIDDEN_TARGETS=1 in the environment \
         of the process that launches cua-rs and restart the server"
    )]
    ForbiddenTarget {
        verb: &'static str,
        app: String,
        bundle_id: String,
        why: &'static str,
    },

    #[error(
        "refusing to {verb}: this login session is locked or its screen saver is running. cua-rs \
         will not deliver input to a session no human is watching — the window that would receive \
         it cannot be seen, and neither can the result. Reads still work. Unlock the session and \
         retry"
    )]
    SessionLocked { verb: &'static str },

    /// The app is outside the scope the human granted when they started the
    /// server.
    ///
    /// Deliberately a different refusal from [`Refused::ForbiddenTarget`]: that
    /// one says "nobody may drive this", this one says "this run may drive only
    /// what it was scoped to". The distinction matters to the caller, because
    /// only one of them is resolvable at all, and neither is resolvable by the
    /// agent itself.
    #[error(
        "refusing to {verb} in `{app}` ({bundle_id}): this cua-rs run is scoped to \
         CUA_ALLOWED_APPS={allowed}, and `{bundle_id}` is not in it. Reading is still allowed \
         (get_app_state, find, list_apps) — the scope limits what may be *acted on*. Only the \
         human who launched the server can widen it: add the bundle identifier to \
         CUA_ALLOWED_APPS and restart. list_apps prints the identifier for every running app"
    )]
    NotInAllowlist {
        verb: &'static str,
        app: String,
        bundle_id: String,
        allowed: String,
    },

    /// Scoped run, and the target does not present a bundle identifier at all.
    ///
    /// Fails closed: an allowlist that silently admits everything it cannot name
    /// is not an allowlist. Kept separate from
    /// [`Refused::NotInAllowlist`] because "not on the list" and "cannot be
    /// compared to the list" call for different fixes.
    #[error(
        "refusing to {verb} in `{app}` (pid {pid}): this cua-rs run is scoped to \
         CUA_ALLOWED_APPS={allowed}, and this process publishes no bundle identifier, so it \
         cannot be matched against that scope. Scoping fails closed rather than admitting what it \
         cannot name. Reading is still allowed. Unset CUA_ALLOWED_APPS to drive unbundled \
         processes"
    )]
    UnidentifiableUnderAllowlist {
        verb: &'static str,
        app: String,
        pid: i32,
        allowed: String,
    },

    #[error(
        "refusing to {verb} {target}: this reads as a destructive control (matched {matched:?}). \
         Pass confirm_destructive: true on this same call to proceed — that is the whole gate, \
         and it exists so the decision is visible in the transcript rather than made silently by \
         the server. cua-rs deliberately over-reports here: if this is a false positive, \
         confirming is the correct answer and costs one round trip"
    )]
    NeedsConfirmation {
        verb: &'static str,
        target: String,
        matched: String,
    },

    /// The same gate, reached through the *question the dialog is asking*.
    ///
    /// Kept separate from [`Refused::NeedsConfirmation`] because the two accuse
    /// different things. That one says "this control removes something"; this
    /// one says "this control is terse and the sheet around it is not", which is
    /// the ordinary shape of a macOS alert. The message quotes the question, so
    /// a human reading the transcript can see the evidence rather than trusting
    /// a classifier they cannot inspect.
    #[error(
        "refusing to {verb} {target}: it answers a decision context ({context}) whose own text \
         reads as a destructive question — {question:?} (matched {matched:?}). The answer is \
         terse but the question is not, and cua-rs classifies the decision rather than the \
         wording of the button. Pass confirm_destructive: true on this same call to proceed. \
         An answer that names its own harmlessness (Cancel, No, Keep, Save, 취소, 저장 …) is \
         never refused here, so backing out of this dialog — or saving instead of discarding — \
         needs no confirmation"
    )]
    NeedsConfirmationInContext {
        verb: &'static str,
        target: String,
        context: String,
        question: String,
        matched: String,
    },

    /// The same gate, reached through the *key* rather than the label.
    ///
    /// Kept separate so the message cannot claim a scroll bar is "a destructive
    /// control" when what is destructive is the Delete being pressed on it.
    #[error(
        "refusing to press {key:?} on {target}: this key removes something here (matched \
         {matched:?}) — a modified Delete is Move to Trash in most apps, and a bare Delete \
         outside a text field acts on whatever is selected. Pass confirm_destructive: true on \
         this same call to proceed"
    )]
    NeedsConfirmationForKey {
        key: String,
        target: String,
        matched: String,
    },

    #[error(
        "refusing to {verb} in `{app}`: the human took over. Real input arrived {ago_ms}ms ago \
         while `{app}` was the frontmost app, and CUA_YIELD_TO_HUMAN=1 tells cua-rs to stand down \
         rather than compete for the window somebody is using. Retry once they have left it alone \
         for {idle_ms}ms, drive a different app, or unset CUA_YIELD_TO_HUMAN"
    )]
    HumanTookOver {
        verb: &'static str,
        app: String,
        ago_ms: u64,
        idle_ms: u64,
    },

    #[error(
        "refusing to {verb}: CUA_YIELD_TO_HUMAN=1 asks cua-rs to stop when the human starts using \
         the app it is driving, but the listen-only input tap that would notice could not be \
         created ({reason}), so it cannot tell. This fails closed on purpose. Grant Accessibility \
         to the process that launched cua-rs, or unset CUA_YIELD_TO_HUMAN"
    )]
    YieldWatchUnavailable { verb: &'static str, reason: String },
}

#[path = "apps.rs"]
mod apps_policy;
pub use apps_policy::*;
#[cfg(test)]
use apps_policy::{AUTH_PROMPT, CREDENTIALS, SECURITY_SURFACE};

// ── destructive labels ───────────────────────────────────────────────────────

/// English word stems whose presence in a control's label means pressing it
/// probably removes something.
///
/// Stems rather than whole words so `Delete`, `Deletes`, `Deleting` and
/// `Deleted` all match from one entry, and matched against *word* starts rather
/// than anywhere in the string so `Presets` does not match `reset` and
/// `Undelete` does not match `delete`. That is the one place this module trades
/// a little recall for precision, and it is safe to do here because the words
/// it declines to match are longer words with different meanings, not
/// destructive controls in disguise.
const DESTRUCTIVE_STEMS: &[&str] = &[
    "delet",      // Delete, Deletes, Deleting
    "remov",      // Remove, Removing
    "eras",       // Erase, Erasing
    "discard",    //
    "reset",      //
    "trash",      // Move to Trash, Empty Trash
    "uninstall",  //
    "revok",      // Revoke, Revoking
    "destroy",    //
    "wipe",       //
    "purge",      //
    "shred",      //
    "overwrite",  //
    "unsend",     //
    "deauthoriz", // Deauthorize
    "clear",      // Clear History, Clear All
];

/// Phrases that only read as destructive as a whole.
///
/// Matched against the label with its whitespace collapsed, so a line-wrapped
/// button still matches.
const DESTRUCTIVE_PHRASES: &[&str] = &[
    "don't save",
    "dont save",
    "do not save",
    "don’t save", // typographic apostrophe: what AppKit actually ships
    "empty trash",
    "move to trash",
    "shut down",
    "log out",
    "sign out",
    "factory settings",
];

/// Korean substrings that mean the same thing.
///
/// Korean has no spaces inside a verb stem the way English does, and the
/// maintainer's standing test targets (KakaoTalk among them) are Korean, so
/// these are matched as plain substrings anywhere in the label. That is more
/// permissive than the English rule on purpose: there is no `Presets` problem
/// here, because these sequences do not occur inside unrelated words.
const DESTRUCTIVE_KOREAN: &[&str] = &[
    "삭제",      // delete
    "제거",      // remove
    "지우",      // erase (지우기, 지움)
    "초기화",    // reset
    "버리",      // discard (버리기)
    "휴지통",    // trash
    "비우기",    // empty
    "폐기",      // dispose
    "탈퇴",      // close account
    "나가기",    // leave (a KakaoTalk room; takes its history with it)
    "저장 안",   // don't save
    "저장하지",  // 저장하지 않음
    "되돌리기",  // revert
    "강제 종료", // force quit
];

/// The element a gate is about to act on, flattened out of the snapshot.
///
/// A copy of the recorded node rather than a live AX read: the classification
/// has to describe the thing the *caller* chose, which is the thing the tree
/// showed them, and re-reading could quietly classify a different control if
/// the app recycled the handle. The action's own resolution catches that drift
/// separately.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub role: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    /// Whether the element's value can be written, i.e. whether this is a text
    /// field rather than a control.
    pub settable: bool,
    /// What the control says in a child element rather than an attribute, for
    /// the toolkits that do that. See [`caption`].
    pub caption: Option<String>,
    /// How the tree rendered it, for the error message.
    pub description: String,
    /// The question the nearest enclosing decision context is asking, if the
    /// element sits inside one. See [`decision_context`] for what qualifies and
    /// how far the search goes.
    pub context: Option<DecisionContext>,
}

impl Candidate {
    /// Replace the *answer* being judged, keeping the question.
    ///
    /// For a key that presses a dialog's default button rather than the control
    /// it was aimed at (see [`key_activates_default_button`]). Both controls are
    /// answers to the same question, so `context` is deliberately untouched —
    /// what changes is which answer is about to be given.
    ///
    /// `value`, `settable` and `caption` are cleared rather than carried over:
    /// they described the element the caller named, and keeping them would let a
    /// text field the caller aimed at excuse a button it did not.
    pub fn substitute_answer(
        &mut self,
        role: impl Into<String>,
        label: Option<String>,
        description: String,
    ) {
        self.role = role.into();
        self.label = label;
        self.value = None;
        self.settable = false;
        self.caption = None;
        self.description = format!("{description} (the default button `return` will press)");
    }

    /// Whether this looks like somewhere a human types.
    ///
    /// Used only to decide what a bare Delete key press means: in a text field
    /// it is one character, and everywhere else it is a row, a file or a
    /// conversation.
    fn is_text_entry(&self) -> bool {
        if self.settable {
            return true;
        }
        let role = self.role.as_str();
        matches!(
            role,
            "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
        )
    }

    /// The text a label heuristic is allowed to read.
    ///
    /// Label, help and caption always — all three are the control describing
    /// itself, and which of them an app populates is a toolkit accident. The
    /// *value* only when the element is not writable: a button's value is part
    /// of what it says, while a text field's value is the user's own content,
    /// and classifying `set_value` on a note containing the word "delete" as a
    /// destructive action would be a refusal no confirmation could make
    /// sensible.
    fn classifiable_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(l) = &self.label {
            parts.push(l);
        }
        if let Some(c) = &self.caption {
            parts.push(c);
        }
        if let Some(h) = &self.help {
            parts.push(h);
        }
        if !self.settable {
            if let Some(v) = &self.value {
                parts.push(v);
            }
        }
        parts.join(" ")
    }

    /// Whether the question around this element may be held against it.
    ///
    /// Two exemptions, both narrow and both for the same reason — the gate has
    /// to stay rare enough that clearing it is a decision rather than a habit:
    ///
    /// - **An answer that names its own harmlessness** (see [`SAFE_ANSWERS`]).
    ///   Cancel is how a caller *avoids* the destruction the dialog is
    ///   offering. Refusing it would leave an agent holding a modal sheet it
    ///   cannot back out of, and the only way through would be to send
    ///   `confirm_destructive: true` — teaching it to confirm its way out of
    ///   alerts, which is precisely the habit that would make the gate useless
    ///   when it matters. Save and Keep are there for the same reason from the
    ///   other side: a sheet offering "save or delete" is a destructive
    ///   question, and gating the answer that preserves the work would put a
    ///   confirmation on one of the most-used sheets on the system.
    /// - **Typing.** A text field inside a dialog is not the decision; the
    ///   button underneath it is. `set_value` into the name field of a sheet
    ///   that mentions deleting changes nothing on its own, and refusing it is
    ///   the "note that says delete" mistake one level out.
    fn accepts_context_evidence(&self) -> bool {
        if self.is_text_entry() {
            return false;
        }
        // Either place the toolkit put the answer's wording. A Chromium button
        // whose caption is a child still says "Cancel".
        let says = self.label.as_deref().or(self.caption.as_deref());
        match says {
            Some(label) => !is_safe_answer(label),
            None => true,
        }
    }
}

// ── the question a dialog is asking ──────────────────────────────────────────
//
// A label heuristic that reads only the control it is about to press cannot see
// the most common destructive arrangement on macOS: the alert holds the verb and
// the button holds one word. "OK" under "Delete 4 items?" is not a labelling
// accident, it is what an alert *is* — the question is asked once, at the top,
// and the answers are terse by design.
//
// Widening to "the ancestor chain" is the obvious move and it is wrong. Every
// element in a mail window descends from a window whose title is a subject line;
// every control in a chat window descends from a window containing the whole
// conversation. Reading ancestors indiscriminately makes the gate fire on any
// app whose *content* mentions deleting, and a gate that fires constantly is
// worse than no gate, because `confirm_destructive: true` then gets attached to
// every call reflexively and stops meaning anything.
//
// So the rule here is about *kind*, not distance:
//
//  1. **Only a decision context is evidence.** An `AXSheet`, an `AXDialog`, or a
//     window whose subrole marks it as a dialog exists for one reason — to ask a
//     question and collect an answer — so its text *is* the question. An
//     ordinary `AXWindow`, `AXGroup` or `AXScrollArea` is layout, and its text is
//     the user's content. No amount of depth turns content into a question.
//  2. **The nearest one, and no further.** The search walks up from the target
//     and stops at the first decision context it finds, and at the first
//     ordinary window if it finds one first. That bound is structural rather
//     than numeric: an alert's message sits one or two levels above its buttons
//     in AppKit and rather more in a cross-platform toolkit, and "N levels" would
//     break differently in each. Stopping at the enclosing question also gets
//     nesting right — a confirmation sheet raised on top of a disk-erase dialog
//     is answering *its own* question, not the one behind it.
//  3. **Prose, not answers.** Inside the context, only its own title/description
//     and its static text count. Sibling buttons are deliberately not read: an
//     alert offering "Delete" and "Cancel" would otherwise make "Cancel"
//     destructive, and refusing the way *out* of a destructive dialog is not
//     caution, it is the gate causing the harm it exists to prevent.
//  4. **Content stays excluded, at every depth.** The reason a text field's own
//     value is never classified — `set_value` on a note saying "delete the old
//     files" must not be refused — applies unchanged to document bodies, message
//     lists and table rows that happen to be inside a dialog. The walk does not
//     descend into them.

/// Roles that exist to ask a question.
const DECISION_ROLES: &[&str] = &["AXSheet", "AXDialog", "AXAlert"];

/// Subroles that mark an otherwise ordinary window as a question.
///
/// This is how AppKit ships an `NSAlert`: role `AXWindow`, subrole `AXDialog`
/// (or `AXSystemDialog` for the ones the system raises). Matching the subrole is
/// what keeps the rule from having to guess which windows are modal.
const DECISION_SUBROLES: &[&str] = &["AXDialog", "AXSystemDialog", "AXAlertDialog"];

/// Where the upward search gives up.
///
/// Reaching an ordinary window means the target is in content, not in a
/// question, and nothing above a window can be one either.
const CONTEXT_BOUNDARY_ROLES: &[&str] = &["AXWindow", "AXApplication", "AXSystemWide"];

/// Roles whose text is the user's own material rather than the dialog's prose.
///
/// The walk neither reads these nor descends into them, however shallow they
/// are. A "Move to…" sheet listing a folder called "delete me" is still a list
/// of the user's files.
const CONTENT_CONTAINER_ROLES: &[&str] = &[
    "AXScrollArea",
    "AXTable",
    "AXOutline",
    "AXList",
    "AXBrowser",
    "AXGrid",
    "AXRow",
    "AXCell",
    "AXWebArea",
    "AXDocument",
    "AXTextArea",
];

/// Roles that are *answers* to the question, not part of it.
///
/// Not read and not descended into, so one destructive button in an alert does
/// not contaminate every other button beside it.
const ANSWER_ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXLink",
    "AXDisclosureTriangle",
    "AXTab",
    "AXSlider",
    "AXIncrementor",
    "AXStepper",
    "AXTextField",
    "AXComboBox",
    "AXSearchField",
];

/// Roles whose text is read as part of the question.
const QUESTION_ROLES: &[&str] = &["AXStaticText", "AXHeading"];

/// How many pieces of static text one question may be assembled from.
///
/// Not a tuning knob — the shape rules above are what decide *what* is read, and
/// this only bounds the cost of reading a pathological dialog with a thousand
/// labels in it. A real alert uses two: a message and an informative text.
const MAX_QUESTION_PARTS: usize = 24;

/// Exact labels for answers that name their own harmlessness.
///
/// Two kinds, and the membership test is the same for both: does the word
/// itself promise that nothing is lost? *Refusing* the offer — Cancel, No, Not
/// now, 취소 — and *preserving* what is at stake — Keep, Save, 유지, 저장.
/// Everything else, including OK, 확인, Continue and Yes, promises nothing and
/// is judged by the question.
///
/// Matched against the whole normalized label, never as a substring, so "Close
/// Account" is not excused by "Close" and "Don't Save" is not excused by
/// "Save". The exactness is the safety property: this list is the one place the
/// gate deliberately *stops* refusing, so it has to be impossible for a
/// destructive control to fall into it by containing a soft word.
///
/// Save earned its place from a measurement rather than an argument. macOS's
/// own close-without-saving sheet reads "…you can save it now, or delete it
/// immediately", so the question is destructive and the Save button was being
/// refused on one of the most-used sheets on the system. That is exactly the
/// shape that trains a caller to attach `confirm_destructive: true` to
/// everything, and the overwrite prompt it might otherwise have caught spells
/// its button "Replace", which is not on this list.
const SAFE_ANSWERS: &[&str] = &[
    "cancel",
    "no",
    "not now",
    "later",
    "dismiss",
    "back",
    "go back",
    "keep",
    "save",
    "취소",
    "아니오",
    "아니요",
    "나중에",
    "돌아가기",
    "유지",
    "저장",
];

/// The question the nearest enclosing decision context is asking.
///
/// Assembled once at the gate and carried on the [`Candidate`], so a refusal can
/// quote its own evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionContext {
    /// How the context renders — `AXSheet`, `AXWindow[AXDialog]` — for the
    /// error message.
    pub kind: String,
    /// The context's own title and description plus its static text, in tree
    /// order.
    pub question: String,
}

/// One snapshot node, as the context rule sees it.
///
/// Borrowed rather than cloned: the caller in `session` already holds the
/// snapshot, and copying a 1500-node tree per action to classify one button
/// would be paying for the whole window to judge one control. The fields are
/// exactly what the rule reads, which also documents what it cannot read.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextNode<'a> {
    pub parent: Option<usize>,
    pub role: &'a str,
    pub subrole: Option<&'a str>,
    pub label: Option<&'a str>,
    pub value: Option<&'a str>,
    pub help: Option<&'a str>,
    pub settable: bool,
}

fn any_eq_ignore_case(list: &[&str], want: &str) -> bool {
    list.iter().any(|entry| entry.eq_ignore_ascii_case(want))
}

/// Whether this node exists to ask a question rather than to lay one out.
fn is_decision_context(node: &ContextNode<'_>) -> bool {
    any_eq_ignore_case(DECISION_ROLES, node.role)
        || node
            .subrole
            .is_some_and(|s| any_eq_ignore_case(DECISION_SUBROLES, s))
}

/// The nearest enclosing decision context of `target`, if it has one.
///
/// Walks parents only, stops at the first decision context, and gives up at the
/// first ordinary window. The step count is capped at the node count so a
/// malformed parent cycle cannot hang an action.
fn nearest_decision_context(nodes: &[ContextNode<'_>], target: usize) -> Option<usize> {
    let mut current = nodes.get(target)?.parent;
    for _ in 0..nodes.len() {
        let index = current?;
        let node = nodes.get(index)?;
        if is_decision_context(node) {
            return Some(index);
        }
        if any_eq_ignore_case(CONTEXT_BOUNDARY_ROLES, node.role) {
            return None;
        }
        current = node.parent;
    }
    None
}

/// The text a decision context itself contributes: its title, its help, and its
/// value when that value is not the user's to write.
///
/// Never a writable value, for the reason a text field's contents are never
/// classified — that is typing, not prose.
fn own_text_of(node: &ContextNode<'_>) -> Vec<String> {
    let mut parts = Vec::new();
    for part in [node.label, node.help] {
        if let Some(p) = part.map(str::trim).filter(|p| !p.is_empty()) {
            parts.push(p.to_string());
        }
    }
    if !node.settable {
        if let Some(v) = node.value.map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(v.to_string());
        }
    }
    parts
}

/// The sentence a piece of static text is displaying.
///
/// For a static text the *value* is the string on screen; its title is often an
/// internal identifier — a real save sheet publishes `whereLabel`, `_NS:246`,
/// `fileFormatLabel` — which is noise in a refusal a human has to read, and
/// noise the matcher would otherwise have to be trusted not to trip over. So
/// the value wins, and the label is a fallback for the toolkits that put the
/// sentence in `AXTitle`/`AXDescription` instead.
fn displayed_text_of(node: &ContextNode<'_>) -> Option<String> {
    if !node.settable {
        if let Some(v) = node.value.map(str::trim).filter(|v| !v.is_empty()) {
            return Some(v.to_string());
        }
    }
    node.label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
}

/// The prose inside `root`, pruned at answers, at content and at any nested
/// decision context.
///
/// A depth-first walk, so the text collected is bounded by the subtree's own
/// prose no matter how large the window behind it is. `include_root` is what
/// distinguishes the two callers: a decision context contributes its own
/// title, while a control's own attributes are already classified elsewhere and
/// only its caption children are wanted here.
fn prose_in(nodes: &[ContextNode<'_>], root: usize, include_root: bool) -> String {
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent {
            if parent < nodes.len() && parent != index {
                children[parent].push(index);
            }
        }
    }

    let mut parts = if include_root {
        own_text_of(&nodes[root])
    } else {
        Vec::new()
    };
    let mut stack: Vec<usize> = children[root].iter().rev().copied().collect();
    while let Some(index) = stack.pop() {
        if parts.len() >= MAX_QUESTION_PARTS {
            break;
        }
        let node = &nodes[index];
        if is_decision_context(node)
            || any_eq_ignore_case(CONTENT_CONTAINER_ROLES, node.role)
            || any_eq_ignore_case(ANSWER_ROLES, node.role)
        {
            continue;
        }
        if any_eq_ignore_case(QUESTION_ROLES, node.role) {
            parts.extend(displayed_text_of(node));
            // Static text has no children worth walking, and an accessible
            // label rendered as a child of one would be the same string twice.
            continue;
        }
        stack.extend(children[index].iter().rev().copied());
    }

    parts.join(" ")
}

/// Roles whose caption may be a child element instead of an attribute.
///
/// Chromium and the toolkits built on it publish a button as a container with
/// its caption as an `AXStaticText` inside it, so a control that says "Delete"
/// on screen arrives here with no label at all. Reading one level of prose out
/// of *the control itself* is not a widening of scope — it is the same target
/// the classifier always judged, reassembled from where this toolkit put it.
///
/// Restricted to button-shaped roles on purpose. `AXRow` and `AXCell` also hold
/// their text in children, and that text is the user's mail, chat and files.
const CAPTIONED_ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXLink",
    "AXTab",
    "AXDisclosureTriangle",
    "AXToolbarButton",
];

/// What the control at `target` says, when it says it in a child.
///
/// `None` for anything that is not button-shaped, and for a button that carries
/// its caption in an attribute like every native control does.
pub fn caption(nodes: &[ContextNode<'_>], target: usize) -> Option<String> {
    let node = nodes.get(target)?;
    if !any_eq_ignore_case(CAPTIONED_ROLES, node.role) {
        return None;
    }
    let text = prose_in(nodes, target, false);
    Some(text).filter(|t| !t.trim().is_empty())
}

/// The question the element at `target` is being asked to answer, if any.
///
/// Public because this is the interesting half of the classifier and deserves to
/// be testable from a plain array of nodes, with no app, no snapshot and no
/// accessibility permission.
pub fn decision_context(nodes: &[ContextNode<'_>], target: usize) -> Option<DecisionContext> {
    let index = nearest_decision_context(nodes, target)?;
    let question = prose_in(nodes, index, true);
    if question.trim().is_empty() {
        return None;
    }
    let node = &nodes[index];
    let kind = match node.subrole {
        Some(subrole) if !subrole.is_empty() => format!("{}[{subrole}]", node.role),
        _ => node.role.to_string(),
    };
    Some(DecisionContext { kind, question })
}

/// Whether a label is an exact promise that nothing is destroyed.
///
/// Trailing punctuation and the ellipsis AppKit likes are stripped before the
/// comparison, so "Cancel…" is still a cancel; nothing else about the string is
/// allowed to vary.
fn is_safe_answer(label: &str) -> bool {
    let norm = normalize(label);
    let trimmed = norm.trim_matches(|c: char| !c.is_alphanumeric());
    !trimmed.is_empty() && SAFE_ANSWERS.contains(&trimmed)
}

/// Collapse whitespace and lowercase, so one label matches one way.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The destructive token `text` contains, if any.
///
/// Public because the classification is the interesting part of this module and
/// deserves to be testable without constructing an app, a snapshot or a gate.
pub fn destructive_token(text: &str) -> Option<String> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }

    for phrase in DESTRUCTIVE_PHRASES {
        if norm.contains(phrase) {
            return Some((*phrase).to_string());
        }
    }
    for token in DESTRUCTIVE_KOREAN {
        if norm.contains(token) {
            return Some((*token).to_string());
        }
    }
    // Word-start matching for the English stems. Splitting on anything that is
    // not alphanumeric handles "Delete…", "(Delete)", "Delete/Remove" and
    // "delete_all" without a regex.
    for word in norm.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if let Some(stem) = DESTRUCTIVE_STEMS.iter().find(|s| word.starts_with(**s)) {
            return Some((*stem).to_string());
        }
    }
    None
}

/// The question held against this candidate, and the token that convicted it.
///
/// The whole context half of the gate in one call, so a test can exercise the
/// decision — not just its pieces — without an app, a window server or a
/// permission. `None` means the context is no evidence: there is no enclosing
/// question, the question is harmless, or the candidate is exempt from being
/// judged by it (see [`Candidate::accepts_context_evidence`]).
pub fn destructive_context(candidate: &Candidate) -> Option<(&DecisionContext, String)> {
    if !candidate.accepts_context_evidence() {
        return None;
    }
    let context = candidate.context.as_ref()?;
    let matched = destructive_token(&context.question)?;
    Some((context, matched))
}

/// Whether pressing `key` on `candidate` destroys something.
///
/// The label heuristic cannot see this case at all: a Delete key press carries
/// its meaning in the key, not in the element it lands on. Two rules, both
/// biased toward refusing:
///
/// - any chord that combines a modifier with Delete or Backspace — `cmd+delete`
///   is Move to Trash in the Finder and Delete Conversation in most chat apps;
/// - a bare Delete or Backspace anywhere that is not a text entry field,
///   including anywhere cua-rs could not identify the element at all.
pub fn destructive_key(key: &str, candidate: Option<&Candidate>) -> Option<String> {
    let key = key.trim().to_lowercase();
    let mut parts: Vec<&str> = key
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let base = parts.pop()?;
    let modified = !parts.is_empty();

    let is_delete = matches!(
        base,
        "delete" | "del" | "backspace" | "forwarddelete" | "forward_delete" | "forward-delete"
    );
    if !is_delete {
        return None;
    }
    if modified {
        return Some(key.clone());
    }
    match candidate {
        Some(c) if c.is_text_entry() => None,
        _ => Some(base.to_string()),
    }
}

/// Whether this key presses a dialog's **default** button rather than whatever
/// the caller aimed at.
///
/// The gap this closes: every other gate here judges the element the caller
/// named, which is right for a click because a click lands where it is aimed.
/// Return does not. Inside an alert it activates the default button — the one
/// AppKit drew with a pulsing highlight — no matter which control the caller
/// addressed. So `press_key return` aimed at an alert's Cancel button was judged
/// against *Cancel*, found exempt because cancelling is safe, and then pressed
/// **Delete**. The one arrangement where the aimed element and the affected
/// element are different, in the one place it costs the most.
///
/// Only Return and Enter. Escape has the mirror-image property — it activates
/// the cancel button — but that direction is safe by construction, and space
/// presses the focused control, which is the aimed-element case the rest of the
/// gate already covers correctly.
pub fn key_activates_default_button(key: &str) -> bool {
    let key = key.trim().to_lowercase();
    // A modified Return is an app-specific shortcut rather than "confirm this
    // dialog", so it is left to the ordinary aimed-element judgement.
    if key.contains('+') {
        return false;
    }
    matches!(
        key.as_str(),
        "return" | "enter" | "kp_enter" | "keypad_enter" | "numpad_enter"
    )
}

#[path = "runtime.rs"]
mod runtime;
pub use runtime::*;

#[path = "gate.rs"]
mod gate;
pub use gate::*;

#[cfg(test)]
include!("tests.rs");
