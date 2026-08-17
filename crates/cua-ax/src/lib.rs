//! Safe, agent-oriented wrappers over the macOS Accessibility API.
//!
//! # Why this crate exists
//!
//! Every mainstream "computer use" implementation drives macOS by *pretending
//! to be a human*: warp the cursor with `CGWarpMouseCursorPosition`, then post
//! synthetic events into the global HID tap with `CGEventPost`. That works, but
//! it is fundamentally a shared, single-writer channel — there is exactly one
//! mouse cursor and one keyboard focus on a Mac, so an agent driving the
//! machine that way is *competing with the human sitting at it*. It steals the
//! cursor mid-sentence, it steals keyboard focus, it drags the active Space
//! out from under you.
//!
//! This crate provides the element-addressed half of cua-rs: tree inspection,
//! `AXUIElementPerformAction` semantic actions, and
//! `AXUIElementSetAttributeValue` text writes. It never synthesizes input.
//! The higher-level `cua-core` crate also uses `cua-hid`: `click` and
//! `press_key` default to process-routed SkyLight/CGEvent synthesis, while text
//! writes and explicitly requested secondary AX actions remain element-based.
//! Those events are routed to one pid and do not warp the shared cursor, but
//! keyboard events still land on that process's first responder.
//!
//! # The two things that make AX usable for an agent
//!
//! Raw AX is a chatty, deeply-nested, cyclic object graph with no stable
//! identifiers. Two pieces of policy turn it into something an LLM can drive:
//!
//! 1. [`Element::snapshot_tree`] walks it *once*, breadth-first, with hard
//!    caps, and hands back a flat [`AxNode`] list. One walk per turn, not one
//!    IPC round-trip per question.
//! 2. Every node gets an `index` — its position in that flat list. The agent
//!    says "click 42"; we look up node 42's retained `AXUIElement`. No
//!    coordinates, no fragile "third button in the second group" paths.
//!
//! # Threading
//!
//! AX calls are synchronous IPC into the target app's main run loop. A hung or
//! modal app will block the caller, which is why [`Element::set_timeout`] is
//! applied to every app element we create. None of these types are `Send`:
//! `CFRetained` is not thread-safe here by design. Callers must confine an
//! `Element` to the thread that made it (see `cua-core`'s worker thread).

use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
    Type,
};

// ── errors ───────────────────────────────────────────────────────────────────

/// A failed AX call, mapped to something a tool response can explain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AxError {
    /// The process is not trusted for Accessibility. This is the error the user
    /// actually has to *do* something about, so it carries its own remedy text.
    #[error("accessibility permission denied. Grant it in System Settings > Privacy & Security > Accessibility, then restart this server")]
    NotTrusted,

    /// The element went away — window closed, view recycled, app quit. Callers
    /// should treat this as "your snapshot is stale, take a new one", not as a
    /// hard failure.
    #[error("stale element (the UI changed since the last snapshot; call get_app_state again)")]
    Stale,

    /// The element does not expose that attribute or action at all.
    #[error("{what} `{name}` is not supported by this element")]
    Unsupported { what: &'static str, name: String },

    /// The attribute exists but currently holds nothing.
    #[error("no value for `{0}`")]
    NoValue(String),

    /// AX refused, usually because the app is busy, modal, or wedged.
    #[error("the app did not complete the request in time (busy, modal, or not responding)")]
    CannotComplete,

    /// The app advertises the attribute or action and then declines to implement
    /// it. Not the caller's mistake, and not fixable by retrying.
    #[error("the app advertises `{0}` but has not implemented it")]
    NotImplemented(String),

    /// Anything else, kept verbatim so bug reports stay useful.
    #[error("accessibility error: {0:?}")]
    Other(i32),
}

impl AxError {
    fn from_ax(err: AXError, ctx: Ctx<'_>) -> Self {
        match err {
            AXError::APIDisabled => Self::NotTrusted,
            AXError::InvalidUIElement | AXError::InvalidUIElementObserver => Self::Stale,
            AXError::CannotComplete => Self::CannotComplete,
            AXError::NoValue => Self::NoValue(ctx.name().to_string()),
            AXError::AttributeUnsupported | AXError::ParameterizedAttributeUnsupported => {
                Self::Unsupported {
                    what: "attribute",
                    name: ctx.name().to_string(),
                }
            }
            AXError::ActionUnsupported => Self::Unsupported {
                what: "action",
                name: ctx.name().to_string(),
            },
            // Distinct from Unsupported: the app declares the attribute and even
            // reports it as settable, then refuses the write. Common for
            // AXEnhancedUserInterface, which many apps advertise and none of the
            // modern ones implement.
            AXError::NotImplemented => Self::NotImplemented(ctx.name().to_string()),
            other => Self::Other(other.0),
        }
    }
}

/// What we were asking for when a call failed, so the error can name it.
#[derive(Copy, Clone)]
enum Ctx<'a> {
    Attr(&'a str),
    Action(&'a str),
    None,
}

impl Ctx<'_> {
    fn name(&self) -> &str {
        match self {
            Ctx::Attr(n) | Ctx::Action(n) => n,
            Ctx::None => "",
        }
    }
}

pub type Result<T> = std::result::Result<T, AxError>;

// ── attribute / action name constants ────────────────────────────────────────

/// AX attribute names.
///
/// These are spelled out rather than pulled from the framework's `kAX*`
/// globals on purpose: the C constants are just `CFSTR("AXRole")` and friends,
/// so string literals are exactly equivalent while keeping this crate's
/// feature surface (and build time) small.
pub mod attr {
    pub const ROLE: &str = "AXRole";
    pub const SUBROLE: &str = "AXSubrole";
    pub const ROLE_DESCRIPTION: &str = "AXRoleDescription";
    pub const TITLE: &str = "AXTitle";
    pub const VALUE: &str = "AXValue";
    pub const DESCRIPTION: &str = "AXDescription";
    pub const HELP: &str = "AXHelp";
    pub const PLACEHOLDER: &str = "AXPlaceholderValue";
    pub const IDENTIFIER: &str = "AXIdentifier";
    pub const CHILDREN: &str = "AXChildren";
    pub const PARENT: &str = "AXParent";
    pub const WINDOWS: &str = "AXWindows";
    /// The window an arbitrary element belongs to. Published by most elements,
    /// which makes it much cheaper than walking `AXParent` to the top.
    pub const WINDOW: &str = "AXWindow";
    pub const MAIN_WINDOW: &str = "AXMainWindow";
    pub const FOCUSED_WINDOW: &str = "AXFocusedWindow";
    pub const FOCUSED_UI_ELEMENT: &str = "AXFocusedUIElement";
    pub const POSITION: &str = "AXPosition";
    pub const SIZE: &str = "AXSize";
    /// The point the *app itself* nominates as where this element is clicked.
    ///
    /// Optional and often absent, but when present it beats the frame centre:
    /// for a wide list row, a disclosure triangle, or a control with a large
    /// transparent hit area, the geometric middle can be dead space while this
    /// is the live pixel. AppKit fills it in automatically for standard
    /// controls, and VoiceOver uses it for exactly the same purpose.
    pub const ACTIVATION_POINT: &str = "AXActivationPoint";
    pub const ENABLED: &str = "AXEnabled";
    pub const FOCUSED: &str = "AXFocused";
    pub const SELECTED: &str = "AXSelected";
    pub const SELECTED_TEXT: &str = "AXSelectedText";
    pub const SELECTED_TEXT_RANGE: &str = "AXSelectedTextRange";
    pub const NUMBER_OF_CHARACTERS: &str = "AXNumberOfCharacters";
    pub const MENU_BAR: &str = "AXMenuBar";
    /// A menu item's key equivalent, as the character it is drawn with: `"i"`
    /// for ⌘I. Absent on an item with no shortcut, which is the case this whole
    /// area exists for.
    pub const MENU_ITEM_CMD_CHAR: &str = "AXMenuItemCmdChar";
    /// The modifier mask that goes with [`MENU_ITEM_CMD_CHAR`]. See
    /// `cua_core::menu_shortcut` for the encoding, which is not the obvious one.
    pub const MENU_ITEM_CMD_MODIFIERS: &str = "AXMenuItemCmdModifiers";
    /// The mark drawn to the left of a menu item: `"✓"` for a checked toggle,
    /// `"-"` for a mixed one, absent for an unmarked row.
    pub const MENU_ITEM_MARK_CHAR: &str = "AXMenuItemMarkChar";
    pub const TITLE_UI_ELEMENT: &str = "AXTitleUIElement";
    pub const LINKED_UI_ELEMENTS: &str = "AXLinkedUIElements";

    /// Setting this on an *application* element makes Chromium and Electron
    /// apps build and expose their accessibility tree.
    ///
    /// Chromium keeps its AX tree switched off until something signals that an
    /// assistive client is watching, because maintaining it is expensive. Without
    /// this poke, Slack / VS Code / Discord / Notion / Figma and every other
    /// Electron app look like a single empty `AXWindow` with no children, which
    /// reads exactly like a bug in *our* tree walker. Set it once per app,
    /// before the first snapshot.
    pub const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

    /// The older, broader equivalent of [`MANUAL_ACCESSIBILITY`], honored by
    /// Cocoa apps that gate rich AX output (notably anything AppKit-based that
    /// checks for VoiceOver). Harmless when unsupported.
    pub const ENHANCED_USER_INTERFACE: &str = "AXEnhancedUserInterface";
}

/// AX action names, i.e. the verbs this crate can deliver to an element.
pub mod action {
    /// The default activation. Buttons, links, menu items, checkboxes.
    pub const PRESS: &str = "AXPress";
    /// Select, for things that are selected rather than pressed (list rows,
    /// tabs, menu items in some apps, radio buttons).
    pub const PICK: &str = "AXPick";
    /// "Accept" — what Return does in a text field or a default dialog button.
    pub const CONFIRM: &str = "AXConfirm";
    /// "Dismiss" — what Escape does.
    pub const CANCEL: &str = "AXCancel";
    /// Open the element's contextual menu (the right-click equivalent).
    pub const SHOW_MENU: &str = "AXShowMenu";
    /// Raise the element's window without activating the app.
    pub const RAISE: &str = "AXRaise";
    pub const INCREMENT: &str = "AXIncrement";
    pub const DECREMENT: &str = "AXDecrement";
    pub const SCROLL_UP_BY_PAGE: &str = "AXScrollUpByPage";
    pub const SCROLL_DOWN_BY_PAGE: &str = "AXScrollDownByPage";
    pub const SCROLL_LEFT_BY_PAGE: &str = "AXScrollLeftByPage";
    pub const SCROLL_RIGHT_BY_PAGE: &str = "AXScrollRightByPage";
    /// Scroll the element into view. Cheap and side-effect-free, so it is worth
    /// calling before acting on something that may be clipped.
    pub const SCROLL_TO_VISIBLE: &str = "AXScrollToVisible";
}

// ── trust ────────────────────────────────────────────────────────────────────

/// Whether this process currently holds the Accessibility grant.
///
/// This is deliberately the *non*-prompting variant. An MCP server usually
/// runs headless under a supervisor, where the system prompt would appear
/// detached from any UI the user is looking at (or not appear at all), so we
/// report the state and let the caller surface actionable instructions instead.
pub fn is_trusted() -> bool {
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }
    unsafe { AXIsProcessTrusted() != 0 }
}

/// [`is_trusted`] as a `Result`, for `?` at the top of a tool call.
pub fn require_trusted() -> Result<()> {
    if is_trusted() {
        Ok(())
    } else {
        Err(AxError::NotTrusted)
    }
}

mod element;
pub use element::Element;
// ── limits ───────────────────────────────────────────────────────────────────

/// Caps for one [`Element::snapshot_tree`] walk.
///
/// Comparable because two walks under different caps are not comparable *trees*:
/// a 40-node walk of a 300-node window is missing 260 nodes that a later default
/// walk will report as new. Whoever diffs two snapshots has to be able to see
/// that before subtracting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Hard ceiling on nodes returned. The real constraint is the agent's
    /// context window, not memory.
    pub max_nodes: usize,
    /// Hard ceiling on nesting. Anything deeper is virtually always layout
    /// scaffolding, not content.
    pub max_depth: u32,
    /// Per-parent child cap, so one 50k-row table cannot consume the whole
    /// `max_nodes` budget by itself.
    pub max_children: usize,
    /// Do not walk into a subtree whose root is entirely off-screen. A window's
    /// off-screen halves and collapsed drawers are the bulk of a naive tree and
    /// none of it is actionable.
    pub skip_offscreen: bool,
    /// Wall-clock ceiling on one walk.
    ///
    /// The other caps bound how much is *returned*; this bounds how long the
    /// caller waits. They are not the same limit, because the cost of a node is
    /// set by the target app, not by us — see the note at the loop.
    pub budget: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 1500,
            max_depth: 40,
            max_children: 200,
            skip_offscreen: true,
            // Long enough that an ordinary app finishes well inside it, short
            // enough that a pathological one still returns something useful
            // within one turn.
            budget: std::time::Duration::from_secs(10),
        }
    }
}

/// Default AX messaging timeout, in seconds.
///
/// Long enough for a busy Electron app to answer, short enough that a wedged
/// app fails the tool call instead of the whole server.
pub const DEFAULT_TIMEOUT_SECS: f32 = 2.0;

// ── AxNode ───────────────────────────────────────────────────────────────────

/// One element, read once, in a form that can be serialized for an LLM.
///
/// Every field is captured in a single pass while the element is in hand. AX
/// reads are synchronous IPC, so re-reading a title later would be another
/// round-trip *and* could observe a different UI than the screenshot did.
#[derive(Debug, Clone)]
pub struct AxNode {
    /// Position in the flat snapshot. This is the handle the agent quotes back.
    pub index: usize,
    pub depth: u32,
    pub parent: Option<usize>,
    pub role: String,
    pub subrole: Option<String>,
    /// Best available human-readable label, resolved from several attributes.
    pub label: Option<String>,
    /// Current contents, for elements that hold a value.
    pub value: Option<String>,
    pub help: Option<String>,
    pub frame: Option<CGRect>,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
    /// Actions this element advertises, minus the ones no agent should call.
    pub actions: Vec<String>,
    /// Whether the value is writable, i.e. whether `set_value` can work here.
    pub settable: bool,
    /// Retained handle, so acting on `index` needs no second lookup.
    pub element: Element,
}

impl AxNode {
    fn read(el: &Element, index: usize, depth: u32, parent: Option<usize>) -> Self {
        let role = el.role().unwrap_or_else(|| "AXUnknown".to_string());

        // Resolve a label from the attributes apps actually populate, in
        // descending order of intent. AXTitle is the label a developer chose;
        // AXDescription is what VoiceOver reads; AXPlaceholderValue is the only
        // hint an empty search field ever gives. Falling all the way through to
        // AXRoleDescription ("button") is still better than an unlabeled node.
        let label = el.label();

        let value = el
            .value_string(attr::VALUE)
            .filter(|s| !s.trim().is_empty());

        let mut actions = el.actions();
        // AXShowMenu on a container opens a context menu, which changes the UI
        // out from under the snapshot the agent is holding. Keep it out of the
        // advertised surface; a dedicated tool can still ask for it explicitly.
        actions.retain(|a| a != action::SCROLL_TO_VISIBLE);

        Self {
            index,
            depth,
            parent,
            subrole: el.string(attr::SUBROLE),
            label,
            value,
            help: el.string(attr::HELP),
            frame: el.frame(),
            enabled: el.bool(attr::ENABLED).unwrap_or(true),
            focused: el.bool(attr::FOCUSED).unwrap_or(false),
            selected: el.bool(attr::SELECTED).unwrap_or(false),
            settable: el.is_settable(attr::VALUE),
            actions,
            role,
            element: el.clone(),
        }
    }

    /// Whether this node can be acted on, i.e. whether it is worth showing to
    /// the agent as a target.
    ///
    /// A frame makes an otherwise actionless control addressable by index:
    /// cua-core can bypass background-app AX hit-testing and route a SkyLight
    /// click to this retained element's frame instead. Restrict that fallback
    /// to control roles so framed layout containers do not flood the tree with
    /// misleading handles.
    pub fn is_actionable(&self) -> bool {
        const FRAME_CLICK_ROLES: &[&str] = &[
            "AXButton",
            "AXCheckBox",
            "AXDisclosureTriangle",
            "AXLink",
            "AXMenuBarItem",
            "AXMenuButton",
            "AXMenuItem",
            "AXPopUpButton",
            "AXRadioButton",
            "AXRow",
            "AXCell",
            "AXTab",
        ];

        self.enabled
            && (!self.actions.is_empty()
                || (self.frame.is_some() && FRAME_CLICK_ROLES.contains(&self.role.as_str())))
    }

    /// Whether the walk should continue into this node's children.
    fn should_descend(&self, limits: Limits) -> bool {
        if !limits.skip_offscreen {
            return true;
        }
        // A zero-area or negatively-positioned frame means collapsed, hidden,
        // or scrolled fully out of view. Note we still *keep* the node (its
        // existence is information); we just do not pay to walk into it.
        match self.frame {
            Some(f) => f.size.width > 0.5 && f.size.height > 0.5,
            // No frame at all is normal for menu bars and non-visual groups,
            // which do have useful children — so descend.
            None => true,
        }
    }
}

/// What [`Element::enable_rich_accessibility`] actually achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enablement {
    /// The write was accepted. Says nothing about whether it had any effect.
    pub requested: bool,
    /// `AXManualAccessibility` reads back as `true`.
    pub manual: bool,
    /// `AXEnhancedUserInterface` reads back as `true`.
    pub enhanced: bool,
}

impl Enablement {
    /// Whether either attribute reads back as enabled.
    ///
    /// **Do not use this to decide that an app refuses enablement.** Measured on
    /// macOS 26, Slack reads `AXManualAccessibility` back as `false` forever and
    /// still ends up exposing a 367-element tree with an `AXWebArea`. The
    /// attribute is effectively write-only: the read is not a mirror of the
    /// state, so a `false` here means nothing.
    ///
    /// It is kept because a `true` is still informative when debugging, and
    /// because a diagnostic that prints both the write result and the read-back
    /// is how the discrepancy was found in the first place.
    pub fn reads_back_enabled(&self) -> bool {
        self.manual || self.enhanced
    }
}

mod text;
pub use text::{find_text_range, utf16_offset, TextRange, TextWrite};
// ── helpers ──────────────────────────────────────────────────────────────────

fn check(err: AXError, ctx: Ctx<'_>) -> Result<()> {
    if err == AXError::Success {
        Ok(())
    } else {
        Err(AxError::from_ax(err, ctx))
    }
}

/// Render a number the way a UI would, not the way `f64` prints.
///
/// AX hands back `1.0` for a checked checkbox and `0.6000000000000001` for a
/// slider; both are noise in a prompt. Integers print without a decimal point
/// and fractions get clamped to three places.
fn fmt_number(n: f64) -> String {
    if n.fract().abs() < f64::EPSILON && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_render_like_a_ui_not_like_a_float() {
        assert_eq!(fmt_number(1.0), "1");
        assert_eq!(fmt_number(0.0), "0");
        assert_eq!(fmt_number(-3.0), "-3");
        assert_eq!(fmt_number(0.6000000000000001), "0.6");
        assert_eq!(fmt_number(0.5), "0.5");
        assert_eq!(fmt_number(1.2345), "1.234");
    }

    #[test]
    fn default_limits_are_bounded() {
        let l = Limits::default();
        assert!(l.max_nodes > 0 && l.max_depth > 0 && l.max_children > 0);
    }

    #[test]
    fn find_text_range_returns_char_offsets() {
        let r = find_text_range("hello world", "world", None, None).unwrap();
        assert_eq!(
            r,
            TextRange {
                offset: 6,
                length: 5
            }
        );
    }

    #[test]
    fn find_text_range_takes_the_first_match_without_anchors() {
        let r = find_text_range("ab ab ab", "ab", None, None).unwrap();
        assert_eq!(r.offset, 0);
    }

    #[test]
    fn prefix_and_suffix_disambiguate_repeats_and_are_excluded() {
        // Select the "ab" that follows "2:" -- and only the "ab".
        let r = find_text_range("1:ab 2:ab 3:ab", "ab", Some("2:"), None).unwrap();
        assert_eq!(
            r,
            TextRange {
                offset: 7,
                length: 2
            }
        );

        let r = find_text_range("ab) ab] ab}", "ab", None, Some("]")).unwrap();
        assert_eq!(
            r,
            TextRange {
                offset: 4,
                length: 2
            }
        );
    }

    #[test]
    fn find_text_range_offsets_are_chars_not_bytes() {
        // Every Korean syllable is 3 bytes; a byte-based offset would report 9.
        let r = find_text_range("가나다hi", "hi", None, None).unwrap();
        assert_eq!(r.offset, 3, "offset must be in chars");
    }

    #[test]
    fn find_text_range_rejects_an_empty_needle() {
        assert!(find_text_range("anything", "", None, None).is_none());
    }

    #[test]
    fn find_text_range_misses_are_none() {
        assert!(find_text_range("hello", "world", None, None).is_none());
        // Anchored search must fail when the anchor does not match, even though
        // the needle alone is present.
        assert!(find_text_range("1:ab", "ab", Some("9:"), None).is_none());
    }

    #[test]
    fn utf16_offset_matches_chars_for_ascii_and_diverges_for_astral() {
        assert_eq!(utf16_offset("hello", 5), 5);
        // CJK is one UTF-16 unit each, so still equal.
        assert_eq!(utf16_offset("가나다", 3), 3);
        // An emoji is one char but two UTF-16 units -- the case that breaks
        // naive selection math.
        assert_eq!(utf16_offset("a\u{1F600}b", 3), 4);
        assert_eq!(utf16_offset("", 0), 0);
        // Overshooting must saturate, not panic.
        assert_eq!(utf16_offset("ab", 99), 2);
    }

    #[test]
    fn text_write_labels_are_stable() {
        assert_eq!(TextWrite::Inserted.as_str(), "inserted");
        assert_eq!(TextWrite::Replaced.as_str(), "replaced");
    }

    /// The system-wide element always exists, so this exercises the FFI
    /// round-trip without needing the Accessibility grant or any running app.
    #[test]
    fn system_wide_element_is_constructible() {
        let el = Element::system_wide();
        // Reading an unsupported attribute must be `Ok(None)`, not an error:
        // the whole crate's ergonomics depend on that distinction.
        assert!(el.attribute("AXDefinitelyNotARealAttribute").is_ok());
    }
}
