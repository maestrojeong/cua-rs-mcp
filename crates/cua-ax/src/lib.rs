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
//! This crate takes the other road. Actions are delivered *directly to the
//! target UI element* through the Accessibility API — `AXUIElementPerformAction`
//! for presses, `AXUIElementSetAttributeValue` for text — which never touches
//! the cursor, never changes focus, and never activates an app. The agent and
//! the human can work at the same time, on different windows, without fighting.
//!
//! That property is not a tuning detail; it falls straight out of never calling
//! `CGEventPost`. Note that this crate links `CoreGraphics` only to *read*
//! modifier state and enumerate windows. HID synthesis, if ever added, belongs
//! behind an explicit opt-in fallback in a higher layer — never here.
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

// ── Element ──────────────────────────────────────────────────────────────────

/// A retained handle on one accessibility object.
///
/// Not `Send`/`Sync`: confine it to the thread that created it.
#[derive(Clone)]
pub struct Element(CFRetained<AXUIElement>);

impl fmt::Debug for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Element")
            .field("role", &self.role().unwrap_or_default())
            .field("title", &self.string(attr::TITLE).unwrap_or_default())
            .finish()
    }
}

impl Element {
    /// The system-wide element. Its `AXFocusedApplication` is how you find out
    /// what the human is actually looking at.
    pub fn system_wide() -> Self {
        Self(unsafe { AXUIElement::new_system_wide() })
    }

    /// The application element for a pid.
    ///
    /// A timeout is applied immediately, before the caller can make any other
    /// call: without it a wedged or modal app blocks our thread on the very
    /// first attribute read, and a "computer use" server that hangs forever on
    /// one bad app is worse than one that reports a timeout.
    pub fn for_pid(pid: libc::pid_t) -> Self {
        let el = Self(unsafe { AXUIElement::new_application(pid) });
        let _ = el.set_timeout(DEFAULT_TIMEOUT_SECS);
        el
    }

    /// Wrap an already-retained raw element.
    ///
    /// # Safety
    /// `raw` must be a valid, owned (+1 retain count) `AXUIElementRef`.
    pub unsafe fn from_retained(raw: CFRetained<AXUIElement>) -> Self {
        Self(raw)
    }

    pub fn as_raw(&self) -> &AXUIElement {
        &self.0
    }

    /// Per-element ceiling on how long AX will wait for the target app.
    pub fn set_timeout(&self, secs: f32) -> Result<()> {
        check(unsafe { self.0.set_messaging_timeout(secs) }, Ctx::None)
    }

    /// The pid that owns this element.
    pub fn pid(&self) -> Result<libc::pid_t> {
        let mut pid: libc::pid_t = 0;
        check(unsafe { self.0.pid(NonNull::from(&mut pid)) }, Ctx::None)?;
        Ok(pid)
    }

    // ── attribute reads ──────────────────────────────────────────────────

    /// Raw attribute read. `Ok(None)` means "asked, nothing there" — an absent
    /// title is normal, not an error, and collapsing both AX spellings of
    /// absence (`AttributeUnsupported`, `NoValue`) here keeps every caller from
    /// having to.
    pub fn attribute(&self, name: &str) -> Result<Option<CFRetained<CFType>>> {
        let key = CFString::from_str(name);
        let mut out: *const CFType = std::ptr::null();
        let err = unsafe { self.0.copy_attribute_value(&key, NonNull::from(&mut out)) };
        match err {
            AXError::Success => {}
            AXError::AttributeUnsupported | AXError::NoValue => return Ok(None),
            other => return Err(AxError::from_ax(other, Ctx::Attr(name))),
        }
        let Some(ptr) = NonNull::new(out.cast_mut()) else {
            return Ok(None);
        };
        Ok(Some(unsafe { CFRetained::from_raw(ptr) }))
    }

    pub fn string(&self, name: &str) -> Option<String> {
        let v = self.attribute(name).ok()??;
        v.downcast_ref::<CFString>().map(|s| s.to_string())
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        let v = self.attribute(name).ok()??;
        if let Some(b) = v.downcast_ref::<CFBoolean>() {
            return Some(b.as_bool());
        }
        // Several apps hand back 0/1 as CFNumber where the spec says CFBoolean.
        v.downcast_ref::<CFNumber>()
            .and_then(|n| n.as_i64())
            .map(|n| n != 0)
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        let v = self.attribute(name).ok()??;
        v.downcast_ref::<CFNumber>().and_then(|n| n.as_f64())
    }

    /// Read a text-ish attribute. `AXValue` is `CFString` on a text field but
    /// `CFNumber` on a slider and `CFBoolean` on a checkbox, and an agent wants
    /// to read all three the same way, so normalize to a display string.
    pub fn value_string(&self, name: &str) -> Option<String> {
        let v = self.attribute(name).ok()??;
        if let Some(s) = v.downcast_ref::<CFString>() {
            return Some(s.to_string());
        }
        if let Some(b) = v.downcast_ref::<CFBoolean>() {
            return Some(b.as_bool().to_string());
        }
        if let Some(n) = v.downcast_ref::<CFNumber>() {
            return n.as_f64().map(fmt_number);
        }
        None
    }

    pub fn element(&self, name: &str) -> Option<Element> {
        let v = self.attribute(name).ok()??;
        v.downcast::<AXUIElement>().ok().map(Element)
    }

    /// Child elements under `name`.
    ///
    /// Yields an empty `Vec` rather than an error when the attribute is missing:
    /// leaf elements are the common case, not an exceptional one.
    pub fn elements(&self, name: &str) -> Vec<Element> {
        let Ok(Some(v)) = self.attribute(name) else {
            return Vec::new();
        };
        let Some(arr) = v.downcast_ref::<CFArray>() else {
            return Vec::new();
        };
        let n = arr.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let raw = unsafe { arr.value_at_index(i as isize) };
            if raw.is_null() {
                continue;
            }
            // SAFETY: an AX array under AXChildren/AXWindows holds
            // AXUIElementRefs; we retain before the array can be mutated.
            let el = unsafe { &*(raw as *const AXUIElement) };
            out.push(Element(el.retain()));
        }
        out
    }

    pub fn children(&self) -> Vec<Element> {
        self.elements(attr::CHILDREN)
    }

    pub fn role(&self) -> Option<String> {
        self.string(attr::ROLE)
    }

    /// Screen position of the element's top-left corner, in points, in the
    /// global (top-left origin) coordinate space AX reports.
    pub fn position(&self) -> Option<CGPoint> {
        let v = self.attribute(attr::POSITION).ok()??;
        let ax = v.downcast_ref::<AXValue>()?;
        let mut p = CGPoint { x: 0.0, y: 0.0 };
        let ok = unsafe {
            ax.value(
                AXValueType::CGPoint,
                NonNull::new((&mut p as *mut CGPoint).cast::<c_void>())?,
            )
        };
        ok.then_some(p)
    }

    /// [`attr::ACTIVATION_POINT`], if the app publishes one.
    ///
    /// Same `AXValue`-wrapped `CGPoint` shape as [`Element::position`]; kept
    /// separate rather than folded into it because the two mean different
    /// things and only one of them is optional.
    pub fn activation_point(&self) -> Option<CGPoint> {
        let v = self.attribute(attr::ACTIVATION_POINT).ok()??;
        let ax = v.downcast_ref::<AXValue>()?;
        let mut p = CGPoint { x: 0.0, y: 0.0 };
        let ok = unsafe {
            ax.value(
                AXValueType::CGPoint,
                NonNull::new((&mut p as *mut CGPoint).cast::<c_void>())?,
            )
        };
        ok.then_some(p)
    }

    pub fn size(&self) -> Option<CGSize> {
        let v = self.attribute(attr::SIZE).ok()??;
        let ax = v.downcast_ref::<AXValue>()?;
        let mut s = CGSize {
            width: 0.0,
            height: 0.0,
        };
        let ok = unsafe {
            ax.value(
                AXValueType::CGSize,
                NonNull::new((&mut s as *mut CGSize).cast::<c_void>())?,
            )
        };
        ok.then_some(s)
    }

    pub fn frame(&self) -> Option<CGRect> {
        Some(CGRect {
            origin: self.position()?,
            size: self.size()?,
        })
    }

    // ── attribute writes ─────────────────────────────────────────────────

    /// Replace a text attribute's contents.
    ///
    /// This is the no-focus-stealing way to type: it writes the field's value
    /// directly instead of synthesizing keystrokes, so it does not depend on
    /// the app being frontmost or the field being focused. The trade-off is
    /// that it *replaces* rather than appends, and apps that only react to real
    /// key events (canvas editors, terminals, games) will ignore it. cua-rs
    /// does not escalate those failures to shared keyboard input.
    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        let key = CFString::from_str(name);
        let val = CFString::from_str(value);
        check(
            unsafe { self.0.set_attribute_value(&key, val.as_ref()) },
            Ctx::Attr(name),
        )
    }

    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        let key = CFString::from_str(name);
        let val = CFBoolean::new(value);
        check(
            unsafe { self.0.set_attribute_value(&key, val.as_ref()) },
            Ctx::Attr(name),
        )
    }

    /// Replace an attribute that holds an array of elements, e.g.
    /// `AXSelectedRows` on a table or outline.
    ///
    /// Exists for the same reason [`Element::set_string`] does: some
    /// controls have no activation verb at all (a custom-drawn table row is
    /// the common case) but do let a caller drive selection by writing the
    /// container's selection attribute directly, which several apps treat as
    /// equivalent to the user clicking that row.
    pub fn set_element_array(&self, name: &str, elements: &[Element]) -> Result<()> {
        let key = CFString::from_str(name);
        let refs: Vec<CFRetained<AXUIElement>> = elements.iter().map(|e| e.0.clone()).collect();
        let arr = CFArray::from_retained_objects(&refs);
        check(
            unsafe { self.0.set_attribute_value(&key, arr.as_ref()) },
            Ctx::Attr(name),
        )
    }

    pub fn is_settable(&self, name: &str) -> bool {
        let key = CFString::from_str(name);
        let mut settable: u8 = 0;
        let err = unsafe {
            self.0
                .is_attribute_settable(&key, NonNull::from(&mut settable))
        };
        err == AXError::Success && settable != 0
    }

    /// Ask an app to build a full accessibility tree, and report whether it
    /// listened.
    ///
    /// Call this on an *application* element before the first snapshot. For
    /// Chromium and Electron apps it is what turns a single empty `AXWindow` into
    /// a real tree (see [`attr::MANUAL_ACCESSIBILITY`]).
    ///
    /// Two things measured on macOS 26 that the obvious implementation gets
    /// wrong:
    ///
    /// - **The read-back lies.** Slack accepts `AXManualAccessibility = true`,
    ///   reports success, and then reads the attribute back as `false` — forever,
    ///   even once it is demonstrably exposing a 367-element tree with an
    ///   `AXWebArea`. So the returned [`Enablement`] must not be used to conclude
    ///   that an app refused; see [`Enablement::reads_back_enabled`].
    /// - **`AXEnhancedUserInterface` advertises itself and is not implemented.**
    ///   `is_settable` says `true`, the write fails with `NotImplemented`. Kept
    ///   anyway because it costs one call and older AppKit apps still honor it.
    ///
    /// And the tree does not appear promptly. Slack showed 13 elements for at
    /// least 3.2 seconds after the poke and 367 a minute later, so a caller that
    /// sleeps briefly and then declares the window empty will be wrong. The
    /// honest response to a small tree right after a first poke is "ask again",
    /// not "this app has no content".
    pub fn enable_rich_accessibility(&self) -> Enablement {
        // Read back rather than trusting the write. `Ok(())` here means "the app
        // accepted the message", which is a weaker claim than "the app changed
        // its behavior".
        let manual_write = self.set_bool(attr::MANUAL_ACCESSIBILITY, true).is_ok();
        let manual_took = self.bool(attr::MANUAL_ACCESSIBILITY).unwrap_or(false);
        // Legacy fallback, kept because it costs one call and still works on some
        // older AppKit apps that gate rich output on VoiceOver. Every app measured
        // so far fails this write with NotImplemented while still reporting the
        // attribute as settable.
        let enhanced_took = if self.set_bool(attr::ENHANCED_USER_INTERFACE, true).is_ok() {
            self.bool(attr::ENHANCED_USER_INTERFACE).unwrap_or(false)
        } else {
            false
        };

        Enablement {
            requested: manual_write,
            manual: manual_took,
            enhanced: enhanced_took,
        }
    }

    // ── text ─────────────────────────────────────────────────────────────

    /// Current selection as `(offset, length)` in characters.
    ///
    /// A zero length means a collapsed caret at `offset`, which is how "where
    /// would typing go" is expressed.
    pub fn selected_range(&self) -> Option<TextRange> {
        let v = self.attribute(attr::SELECTED_TEXT_RANGE).ok()??;
        let ax = v.downcast_ref::<AXValue>()?;
        let mut r = CFRange {
            location: 0,
            length: 0,
        };
        let ok = unsafe {
            ax.value(
                AXValueType::CFRange,
                NonNull::new((&mut r as *mut CFRange).cast::<c_void>())?,
            )
        };
        ok.then_some(TextRange {
            offset: r.location.max(0) as usize,
            length: r.length.max(0) as usize,
        })
    }

    /// Move or extend the selection.
    pub fn set_selected_range(&self, range: TextRange) -> Result<()> {
        let mut r = CFRange {
            location: range.offset as isize,
            length: range.length as isize,
        };
        // SAFETY: the pointer is a live `CFRange`, matching `AXValueType::CFRange`.
        let value = unsafe {
            AXValue::new(
                AXValueType::CFRange,
                NonNull::new((&mut r as *mut CFRange).cast::<c_void>())
                    .ok_or(AxError::NoValue("range".into()))?,
            )
        }
        .ok_or(AxError::NoValue("AXValueCreate(CFRange)".into()))?;

        let key = CFString::from_str(attr::SELECTED_TEXT_RANGE);
        check(
            unsafe { self.0.set_attribute_value(&key, value.as_ref()) },
            Ctx::Attr(attr::SELECTED_TEXT_RANGE),
        )
    }

    /// Number of characters this element holds, when it says.
    pub fn text_length(&self) -> Option<usize> {
        self.number(attr::NUMBER_OF_CHARACTERS)
            .map(|n| n.max(0.0) as usize)
            .or_else(|| self.string(attr::VALUE).map(|s| s.chars().count()))
    }

    /// Replace the current selection with `text`.
    ///
    /// This is the *insert* primitive: with a collapsed caret it inserts, with a
    /// selection it overwrites. `AXSelectedText` is the only AX attribute that
    /// edits text without replacing the whole field, so it is what makes
    /// appending possible at all.
    pub fn set_selected_text(&self, text: &str) -> Result<()> {
        self.set_string(attr::SELECTED_TEXT, text)
    }

    /// Append `text`, preferring the least destructive mechanism available.
    ///
    /// Two paths, and the difference is visible to the caller through
    /// [`TextWrite`] because it changes what the app's undo stack and change
    /// notifications see:
    ///
    /// - [`TextWrite::Inserted`] — move the caret to the end, then write through
    ///   `AXSelectedText`. The field keeps its existing contents and the app
    ///   observes a normal edit.
    /// - [`TextWrite::Replaced`] — read `AXValue`, concatenate, write it back.
    ///   The fallback for fields that do not expose a settable selection. It is
    ///   a whole-value replacement, so an app watching for incremental edits may
    ///   see one bulk change instead.
    ///
    /// Neither path synthesizes keystrokes, so neither requires focus — and
    /// neither will satisfy an app that only reacts to real key events.
    pub fn append_text(&self, text: &str) -> Result<TextWrite> {
        if self.is_settable(attr::SELECTED_TEXT) {
            let end = self.text_length().unwrap_or(0);
            // Collapse the caret at the end first. Skipping this would overwrite
            // whatever the user happens to have selected.
            self.set_selected_range(TextRange {
                offset: end,
                length: 0,
            })?;
            self.set_selected_text(text)?;
            return Ok(TextWrite::Inserted);
        }

        let existing = self.string(attr::VALUE).unwrap_or_default();
        self.set_string(attr::VALUE, &format!("{existing}{text}"))?;
        Ok(TextWrite::Replaced)
    }

    /// Select a literal substring of this element's text.
    ///
    /// `prefix` and `suffix` disambiguate repeated matches: the search finds an
    /// occurrence of `prefix + needle + suffix` and selects only the `needle`
    /// part. Without them the first occurrence wins.
    ///
    /// Offsets are computed in `char`s and converted for AX, which counts UTF-16
    /// units — see [`utf16_offset`]. Getting that wrong silently misplaces the
    /// selection in any text containing emoji or CJK.
    pub fn select_text(
        &self,
        needle: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> Result<TextRange> {
        let haystack = self
            .string(attr::VALUE)
            .or_else(|| self.string(attr::TITLE))
            .ok_or(AxError::NoValue(attr::VALUE.into()))?;

        let range = find_text_range(&haystack, needle, prefix, suffix).ok_or_else(|| {
            AxError::NoValue(format!("text {needle:?} was not found in this element"))
        })?;

        let ax_range = TextRange {
            offset: utf16_offset(&haystack, range.offset),
            length: utf16_offset(&haystack, range.offset + range.length)
                - utf16_offset(&haystack, range.offset),
        };
        self.set_selected_range(ax_range)?;
        Ok(range)
    }

    // ── actions ──────────────────────────────────────────────────────────

    /// Action names this element advertises.
    pub fn actions(&self) -> Vec<String> {
        let mut out: *const CFArray = std::ptr::null();
        let err = unsafe { self.0.copy_action_names(NonNull::from(&mut out)) };
        if err != AXError::Success {
            return Vec::new();
        }
        let Some(ptr) = NonNull::new(out.cast_mut()) else {
            return Vec::new();
        };
        let arr = unsafe { CFRetained::from_raw(ptr) };
        let n = arr.len();
        let mut names = Vec::with_capacity(n);
        for i in 0..n {
            let raw = unsafe { arr.value_at_index(i as isize) };
            if raw.is_null() {
                continue;
            }
            let s = unsafe { &*(raw as *const CFString) };
            names.push(s.to_string());
        }
        names
    }

    /// Deliver one action to this element.
    pub fn perform(&self, name: &str) -> Result<()> {
        let key = CFString::from_str(name);
        check(unsafe { self.0.perform_action(&key) }, Ctx::Action(name))
    }

    /// Activate the element the way a click would, picking whichever verb it
    /// actually supports.
    ///
    /// AX has no single "click": buttons take `AXPress`, list rows and tabs take
    /// `AXPick`, and a default dialog button may only take `AXConfirm`. Rather
    /// than make the agent guess (and get `ActionUnsupported` back), try the
    /// plausible verbs in order of specificity and report which one landed.
    pub fn activate(&self) -> Result<&'static str> {
        const CANDIDATES: [&str; 3] = [action::PRESS, action::PICK, action::CONFIRM];
        let available = self.actions();
        let mut last = None;
        for verb in CANDIDATES {
            if !available.iter().any(|a| a == verb) {
                continue;
            }
            match self.perform(verb) {
                Ok(()) => return Ok(verb),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(AxError::Unsupported {
            what: "action",
            name: format!("any of {CANDIDATES:?} (element advertises {available:?})"),
        }))
    }

    /// Hit-test a point, in AX global coordinates.
    ///
    /// **Not usable for targeting.** On a background app — which is every app
    /// cua-rs drives — this was measured to answer `AXMenuBar` for every point,
    /// including points inside the app's own window, so it cannot be trusted to
    /// name what a coordinate covers. Resolve coordinates against a snapshot's
    /// element frames instead; see `hit_test` in `cua-core`. Kept for the
    /// `point_probe` example, which exists to demonstrate exactly this.
    pub fn element_at(&self, x: f32, y: f32) -> Result<Element> {
        let mut out: *const AXUIElement = std::ptr::null();
        check(
            unsafe {
                self.0
                    .copy_element_at_position(x, y, NonNull::from(&mut out))
            },
            Ctx::None,
        )?;
        let ptr = NonNull::new(out.cast_mut()).ok_or(AxError::NoValue("element_at".into()))?;
        Ok(Element(unsafe { CFRetained::from_raw(ptr) }))
    }

    // ── tree walk ────────────────────────────────────────────────────────

    /// Flatten this subtree, breadth-first, under explicit caps.
    ///
    /// Breadth-first, not depth-first, and that choice matters: real UIs nest
    /// wrappers dozens of levels deep, so a depth-first walk that hits
    /// `max_nodes` burns the whole budget inside the first sidebar and never
    /// reaches the main content. BFS spends the budget on the shallow elements
    /// an agent is most likely to want.
    ///
    /// The caps are not defensive padding. An AX tree can be effectively
    /// unbounded (virtualized 100k-row tables) and is not guaranteed acyclic,
    /// so an uncapped walk is a hang, not a slow path.
    pub fn snapshot_tree(&self, limits: Limits) -> Vec<AxNode> {
        self.snapshot_tree_reporting(limits).0
    }

    /// [`Element::snapshot_tree`], plus whether the walk finished.
    ///
    /// `false` means the walk stopped early and the tree is incomplete. That
    /// has to be reportable: a caller that cannot tell truncation from absence
    /// will conclude an element does not exist when it was simply never
    /// reached, and go looking for a different way to do something it could
    /// have done.
    pub fn snapshot_tree_reporting(&self, limits: Limits) -> (Vec<AxNode>, bool) {
        let deadline = std::time::Instant::now() + limits.budget;
        let mut nodes: Vec<AxNode> = Vec::new();
        let mut complete = true;
        // (element, depth, parent index in `nodes`)
        let mut queue: std::collections::VecDeque<(Element, u32, Option<usize>)> =
            std::collections::VecDeque::new();
        queue.push_back((self.clone(), 0, None));

        while let Some((el, depth, parent)) = queue.pop_front() {
            if nodes.len() >= limits.max_nodes {
                complete = false;
                break;
            }
            // A node cap is not a time cap. Every node here is a synchronous
            // IPC round-trip into another process, and a slow app makes each
            // one cost far more than the usual fraction of a millisecond:
            // KakaoTalk with ten windows open took 171 s to return 2000 nodes,
            // which from the caller's side is indistinguishable from a hang.
            // Stop on the clock and return what was reached.
            if std::time::Instant::now() >= deadline {
                complete = false;
                break;
            }

            let index = nodes.len();
            let info = AxNode::read(&el, index, depth, parent);
            let descend = depth < limits.max_depth && info.should_descend(limits);
            nodes.push(info);

            if descend {
                for child in el.children().into_iter().take(limits.max_children) {
                    queue.push_back((child, depth + 1, Some(index)));
                }
            }
        }
        (nodes, complete)
    }
}

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
        let label = el
            .string(attr::TITLE)
            .or_else(|| el.string(attr::DESCRIPTION))
            .or_else(|| el.string(attr::PLACEHOLDER))
            .or_else(|| el.string(attr::IDENTIFIER))
            .or_else(|| {
                // Some apps put the label in a *separate* element and only
                // cross-reference it. Follow that one hop.
                el.element(attr::TITLE_UI_ELEMENT)
                    .and_then(|t| t.string(attr::VALUE).or_else(|| t.string(attr::TITLE)))
            })
            .filter(|s| !s.trim().is_empty());

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

// ── text types ───────────────────────────────────────────────────────────────

/// A character range inside an element's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub offset: usize,
    pub length: usize,
}

/// Which mechanism [`Element::append_text`] ended up using.
///
/// Surfaced rather than hidden because the two are not equivalent from the app's
/// point of view, and a caller debugging "my text went in but the app did not
/// notice" needs to know which one happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextWrite {
    /// Written through `AXSelectedText` at a collapsed caret. Existing contents
    /// preserved.
    Inserted,
    /// Whole `AXValue` replaced with old + new.
    Replaced,
}

impl TextWrite {
    pub fn as_str(self) -> &'static str {
        match self {
            TextWrite::Inserted => "inserted",
            TextWrite::Replaced => "replaced",
        }
    }
}

/// Locate `needle` in `haystack`, optionally anchored by `prefix`/`suffix`.
///
/// Returns a **char**-based range covering only `needle`. Pure and total: no AX
/// involved, which is what makes the disambiguation logic testable.
pub fn find_text_range(
    haystack: &str,
    needle: &str,
    prefix: Option<&str>,
    suffix: Option<&str>,
) -> Option<TextRange> {
    if needle.is_empty() {
        return None;
    }
    let pre = prefix.unwrap_or("");
    let suf = suffix.unwrap_or("");
    let pattern = format!("{pre}{needle}{suf}");

    let byte_at = haystack.find(&pattern)?;
    // Skip past the prefix so the returned range covers the needle alone.
    let needle_byte = byte_at + pre.len();

    Some(TextRange {
        offset: haystack[..needle_byte].chars().count(),
        length: needle.chars().count(),
    })
}

/// Convert a char offset to a UTF-16 code-unit offset.
///
/// AX text ranges are counted in UTF-16 units because the API predates any
/// notion of scalar-based indexing. For ASCII the two agree, which is exactly
/// why this bug survives testing: it only appears once the text contains CJK,
/// emoji, or anything else outside the BMP. An emoji is one `char` and two UTF-16
/// units, so a selection past one is off by one per emoji.
pub fn utf16_offset(s: &str, char_offset: usize) -> usize {
    s.chars().take(char_offset).map(char::len_utf16).sum()
}

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
