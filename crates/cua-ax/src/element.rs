use super::*;

// ── Element ──────────────────────────────────────────────────────────────────

/// A retained handle on one accessibility object.
///
/// Not `Send`/`Sync`: confine it to the thread that created it.
#[derive(Clone)]
pub struct Element(CFRetained<AXUIElement>);

/// Two handles are equal when accessibility says they name the same object.
///
/// This is `CFEqual`, not pointer identity, and the difference is the whole
/// point: an app hands out a *fresh* `AXUIElementRef` for each read, so the
/// element that came back from `AXFocusedUIElement` is never the same pointer
/// as the one a snapshot retained earlier even when it is the same text field.
/// Comparing pointers would report "different" for every pair and make any
/// focus check that used it uselessly pessimistic.
impl PartialEq for Element {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl Eq for Element {}

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
        self.element_checked(name).ok().flatten()
    }

    /// Like [`Element::element`], but a real AX failure — a timeout
    /// (`CannotComplete`), a stale element, a permission problem — comes back
    /// as `Err` instead of being folded into `None`. "Asked, nothing there"
    /// (`AttributeUnsupported`/`NoValue`, what [`Element::attribute`] already
    /// collapses to `Ok(None)`) is the only case that stays `Ok(None)` here.
    ///
    /// Use this where the two are not interchangeable: a caller deciding
    /// whether a slow-to-respond app is worth retrying needs to know which one
    /// happened, and `element`/`elements` cannot tell it.
    pub fn element_checked(&self, name: &str) -> Result<Option<Element>> {
        Ok(self
            .attribute(name)?
            .and_then(|v| v.downcast::<AXUIElement>().ok())
            .map(Element))
    }

    /// Child elements under `name`.
    ///
    /// Yields an empty `Vec` rather than an error when the attribute is missing:
    /// leaf elements are the common case, not an exceptional one.
    pub fn elements(&self, name: &str) -> Vec<Element> {
        self.elements_checked(name).unwrap_or_default()
    }

    /// Like [`Element::elements`], but see [`Element::element_checked`] for
    /// why a real failure needs to reach the caller as `Err` rather than as an
    /// indistinguishable empty `Vec`.
    pub fn elements_checked(&self, name: &str) -> Result<Vec<Element>> {
        let Some(v) = self.attribute(name)? else {
            return Ok(Vec::new());
        };
        let Some(arr) = v.downcast_ref::<CFArray>() else {
            return Ok(Vec::new());
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
        Ok(out)
    }

    pub fn children(&self) -> Vec<Element> {
        self.elements(attr::CHILDREN)
    }

    pub fn role(&self) -> Option<String> {
        self.string(attr::ROLE)
    }

    /// Best available human-readable label, resolved from the attributes apps
    /// actually populate, in descending order of intent.
    ///
    /// `AXTitle` is the label a developer chose; `AXDescription` is what
    /// VoiceOver reads; `AXPlaceholderValue` is the only hint an empty search
    /// field ever gives; `AXIdentifier` is at least stable. Falling all the way
    /// through to a linked title element follows the one hop some apps use to
    /// put a label in a separate node.
    ///
    /// Public, and used by both the tree walk and any live re-read, because the
    /// two must agree on what an element is *called*. When they did not — the
    /// walk resolving a label the re-read never looked for — a target that had
    /// not changed at all was reported as stale.
    pub fn label(&self) -> Option<String> {
        self.string(attr::TITLE)
            .or_else(|| self.string(attr::DESCRIPTION))
            .or_else(|| self.string(attr::PLACEHOLDER))
            .or_else(|| self.string(attr::IDENTIFIER))
            .or_else(|| {
                self.element(attr::TITLE_UI_ELEMENT)
                    .and_then(|t| t.string(attr::VALUE).or_else(|| t.string(attr::TITLE)))
            })
            .filter(|s| !s.trim().is_empty())
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
