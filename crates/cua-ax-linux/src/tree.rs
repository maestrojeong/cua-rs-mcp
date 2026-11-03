//! Walking an application's AT-SPI tree into the same numbered, AX-vocabulary
//! node list the macOS backend produces.
//!
//! # Why AX role names on Linux
//!
//! The tree outline, `find`, `element_token` (`<snapshot>-<index>-<role>`), the
//! structural-role filter in `cua_core::snapshot` and the safety heuristics in
//! `cua_core::safety::destructive` all read roles, and all of them were written
//! against the macOS vocabulary (`AXButton`, `AXSheet`, `AXStaticText`). An
//! agent that learned that vocabulary should not have to learn a second one,
//! and a policy keyed on `AXButton` must not silently stop matching because the
//! same control is spelled `push button` here. So AT-SPI roles are mapped onto
//! AX role names at the one place they are read, [`ax_role`], and nothing
//! downstream knows which platform produced a node.
//!
//! The mapping is lossy in a known direction: AT-SPI has roles AX does not
//! (`Terminal`, `LevelBar`, `InfoBar`), and those land on the closest AX role.
//! The original role is not lost — it is what [`ax_role`] was given — but it is
//! not printed, because printing two role vocabularies per line would cost
//! tokens on every line to serve almost no decision.

use std::time::Instant;

use atspi::{CoordType, Interface, InterfaceSet, Role, State, StateSet};
use cua_core::snapshot::TreeNode;
use cua_core::{CGPoint, CGRect, CGSize, Limits};

use crate::bus::{self, timed, BusFail, BusResult, Handle};

/// One walked element: what the tree renders, plus the handle to act on it.
#[derive(Debug, Clone)]
pub(crate) struct LinuxNode {
    pub(crate) index: usize,
    pub(crate) parent: Option<usize>,
    pub(crate) role: String,
    pub(crate) subrole: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) help: Option<String>,
    pub(crate) frame: Option<CGRect>,
    pub(crate) enabled: bool,
    pub(crate) focused: bool,
    pub(crate) selected: bool,
    pub(crate) settable: bool,
    /// AT-SPI action names, in `DoAction` index order.
    pub(crate) actions: Vec<String>,
    pub(crate) states: StateSet,
    pub(crate) interfaces: InterfaceSet,
    pub(crate) atspi_role: Role,
    pub(crate) handle: Handle,
    /// Whether the opt-in XTEST tier is live, which is what makes a bare frame
    /// clickable at all (see [`LinuxNode::is_actionable`]).
    pub(crate) frame_clickable: bool,
}

/// The roles a frame alone makes clickable, when there is a way to click a
/// frame — the same list `cua_ax::AxNode::is_actionable` uses.
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

impl TreeNode for LinuxNode {
    fn index(&self) -> usize {
        self.index
    }
    fn parent(&self) -> Option<usize> {
        self.parent
    }
    fn role(&self) -> &str {
        &self.role
    }
    fn subrole(&self) -> Option<&str> {
        self.subrole.as_deref()
    }
    fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
    fn frame(&self) -> Option<CGRect> {
        self.frame
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn focused(&self) -> bool {
        self.focused
    }
    fn selected(&self) -> bool {
        self.selected
    }
    fn settable(&self) -> bool {
        self.settable
    }
    /// Same rule as macOS: an enabled element with something to deliver to
    /// it. Here that is an AT-SPI action, a writable value, or — only when
    /// XTEST is enabled, since nothing else can click a bare frame — a frame
    /// on a clickable role.
    fn is_actionable(&self) -> bool {
        self.enabled
            && (!self.actions.is_empty()
                || self.settable
                || (self.frame_clickable
                    && self.frame.is_some()
                    && FRAME_CLICK_ROLES.contains(&self.role.as_str())))
    }
}

impl LinuxNode {
    /// `[N] AXButton "OK"`, the same shape `describe_node` gives on macOS.
    pub(crate) fn describe(&self) -> String {
        match (&self.label, &self.value) {
            (Some(l), _) => format!("[{}] {} {l:?}", self.index, self.role),
            (None, Some(v)) => format!("[{}] {} = {v:?}", self.index, self.role),
            (None, None) => format!("[{}] {}", self.index, self.role),
        }
    }

    pub(crate) fn has(&self, iface: Interface) -> bool {
        self.interfaces.contains(iface)
    }

    pub(crate) fn centre(&self) -> Option<(f64, f64)> {
        self.frame.map(|f| {
            (
                f.origin.x + f.size.width / 2.0,
                f.origin.y + f.size.height / 2.0,
            )
        })
    }
}

/// The AX role (and subrole) an AT-SPI role reads as. See the module docs.
pub(crate) fn ax_role(role: Role, states: StateSet) -> (&'static str, Option<&'static str>) {
    use Role as R;
    match role {
        R::Button | R::PushButtonMenu => ("AXButton", None),
        R::ToggleButton => ("AXCheckBox", Some("AXToggle")),
        R::CheckBox => ("AXCheckBox", None),
        R::RadioButton => ("AXRadioButton", None),
        R::ComboBox => ("AXComboBox", None),
        R::PasswordText => ("AXTextField", Some("AXSecureTextField")),
        R::Entry | R::Autocomplete | R::Editbar => ("AXTextField", None),
        R::Text | R::Paragraph if states.contains(State::Editable) => {
            if states.contains(State::MultiLine) {
                ("AXTextArea", None)
            } else {
                ("AXTextField", None)
            }
        }
        R::Terminal => ("AXTextArea", None),
        R::Text | R::Paragraph | R::Label | R::Static | R::Caption | R::AcceleratorLabel => {
            ("AXStaticText", None)
        }
        R::Heading => ("AXHeading", None),
        R::Link => ("AXLink", None),
        R::MenuBar => ("AXMenuBar", None),
        R::Menu | R::PopupMenu => ("AXMenu", None),
        R::MenuItem | R::CheckMenuItem | R::RadioMenuItem | R::TearoffMenuItem => {
            ("AXMenuItem", None)
        }
        R::PageTab => ("AXTab", None),
        R::PageTabList => ("AXTabGroup", None),
        R::Slider | R::Dial => ("AXSlider", None),
        R::SpinButton => ("AXIncrementor", None),
        R::ScrollBar => ("AXScrollBar", None),
        R::ScrollPane | R::Viewport => ("AXScrollArea", None),
        R::Table | R::TreeTable | R::DocumentSpreadsheet => ("AXTable", None),
        R::Tree => ("AXOutline", None),
        R::List | R::ListBox | R::DescriptionList => ("AXList", None),
        R::ListItem | R::TableRow | R::TreeItem => ("AXRow", None),
        R::TableCell => ("AXCell", None),
        R::ColumnHeader | R::TableColumnHeader | R::RowHeader | R::TableRowHeader => {
            ("AXCell", Some("AXHeaderCell"))
        }
        R::Frame | R::Window | R::InternalFrame => ("AXWindow", Some("AXStandardWindow")),
        R::Dialog | R::Alert | R::FileChooser | R::ColorChooser | R::FontChooser => {
            ("AXWindow", Some("AXDialog"))
        }
        R::ToolBar => ("AXToolbar", None),
        R::Image | R::Icon | R::DesktopIcon => ("AXImage", None),
        R::ProgressBar => ("AXProgressIndicator", None),
        R::LevelBar | R::Rating => ("AXLevelIndicator", None),
        R::DocumentWeb => ("AXWebArea", None),
        R::SplitPane => ("AXSplitGroup", None),
        R::Separator => ("AXSplitter", None),
        R::ToolTip => ("AXHelpTag", None),
        R::Application => ("AXApplication", None),
        R::Invalid | R::Unknown | R::Extended | R::RedundantObject => ("AXUnknown", None),
        // Containers, landmarks and everything else that groups rather than
        // does: structural on macOS too, so the renderer skips the unlabeled
        // ones exactly as it would there.
        _ => ("AXGroup", None),
    }
}

/// Roles whose text is worth reading into `value`. Not every role with a Text
/// interface: GTK puts one on buttons and labels, whose text is already the
/// name, and reading a whole document view on every walk is the AXValue cost
/// problem the macOS walk already bounds.
fn reads_text(role: Role) -> bool {
    matches!(
        role,
        Role::Entry | Role::Text | Role::Terminal | Role::Paragraph | Role::Autocomplete
    )
}

/// Characters of a text value read per element. Same budget the renderer
/// truncates to anyway, with room for `find` to match a little further in.
const MAX_TEXT_CHARS: i32 = 2_000;

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Read one element. A failure to read an optional facet (extents, actions,
/// text) leaves it empty rather than failing the node: toolkits implement
/// interfaces partially, and one missing `GetExtents` must not hide a button.
/// Only the element's own `Accessible` failing is an error.
pub(crate) async fn read_node(
    conn: &zbus::Connection,
    handle: Handle,
    index: usize,
    parent: Option<usize>,
    frame_clickable: bool,
) -> BusResult<(LinuxNode, Vec<Handle>)> {
    let acc = bus::accessible(conn, &handle).await?;
    let atspi_role = timed(acc.get_role()).await?;
    let states = timed(acc.get_state()).await.unwrap_or_default();
    let interfaces = timed(acc.get_interfaces()).await.unwrap_or_default();
    let name = timed(acc.name()).await.unwrap_or_default();
    let description = timed(acc.description()).await.unwrap_or_default();

    let (role, subrole) = ax_role(atspi_role, states);

    let frame = if interfaces.contains(Interface::Component) {
        match bus::component(conn, &handle).await {
            Ok(c) => timed(c.get_extents(CoordType::Screen))
                .await
                .ok()
                .and_then(|(x, y, w, h)| frame_of(x, y, w, h)),
            Err(_) => None,
        }
    } else {
        None
    };

    let actions = if interfaces.contains(Interface::Action) {
        match bus::action(conn, &handle).await {
            Ok(a) => timed(a.get_actions())
                .await
                .map(|v| v.into_iter().map(|a| a.name).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let checkable = matches!(
        atspi_role,
        Role::CheckBox
            | Role::ToggleButton
            | Role::RadioButton
            | Role::CheckMenuItem
            | Role::RadioMenuItem
    );
    // Never read a password field's text: the tree is sent to a model.
    let secret = atspi_role == Role::PasswordText;
    let value = if secret {
        None
    } else if checkable {
        Some(
            if states.contains(State::Checked) {
                "1"
            } else {
                "0"
            }
            .to_string(),
        )
    } else if interfaces.contains(Interface::Text) && reads_text(atspi_role) {
        match bus::text(conn, &handle).await {
            Ok(t) => {
                let count = timed(t.character_count()).await.unwrap_or(0);
                timed(t.get_text(0, count.min(MAX_TEXT_CHARS)))
                    .await
                    .ok()
                    .and_then(non_empty)
            }
            Err(_) => None,
        }
    } else if interfaces.contains(Interface::Value) {
        match bus::value(conn, &handle).await {
            Ok(v) => timed(v.current_value()).await.ok().map(|n| format!("{n}")),
            Err(_) => None,
        }
    } else {
        None
    };

    let editable = states.contains(State::Editable) && interfaces.contains(Interface::EditableText);
    let value_settable = interfaces.contains(Interface::Value)
        && matches!(atspi_role, Role::Slider | Role::SpinButton);

    let (label, help) = match (non_empty(name), non_empty(description)) {
        (Some(n), d) => (Some(n), d),
        (None, d) => (d, None),
    };

    let node = LinuxNode {
        index,
        parent,
        role: role.to_string(),
        subrole: subrole.map(str::to_string),
        label,
        value,
        help,
        frame,
        enabled: states.contains(State::Enabled) || states.contains(State::Sensitive),
        focused: states.contains(State::Focused),
        selected: states.contains(State::Selected),
        settable: editable || value_settable,
        actions,
        states,
        interfaces,
        atspi_role,
        handle,
        frame_clickable,
    };

    let children = match timed(acc.get_children()).await {
        Ok(refs) => refs.iter().filter_map(Handle::from_ref).collect(),
        Err(_) => Vec::new(),
    };
    Ok((node, children))
}

/// AT-SPI reports "no geometry" as a zero or negative size, and some toolkits
/// as `i32::MIN` coordinates. Neither is a frame anything should aim at.
fn frame_of(x: i32, y: i32, w: i32, h: i32) -> Option<CGRect> {
    if w <= 0 || h <= 0 || x == i32::MIN || y == i32::MIN {
        return None;
    }
    Some(CGRect::new(
        CGPoint::new(f64::from(x), f64::from(y)),
        CGSize::new(f64::from(w), f64::from(h)),
    ))
}

/// What one walk produced.
pub(crate) struct Walk {
    pub(crate) nodes: Vec<LinuxNode>,
    /// Whether the walk visited everything it meant to, rather than stopping at
    /// a cap or the time budget. Same meaning as the macOS snapshot's
    /// `complete`, and used the same way: a post-action diff against an
    /// incomplete walk is refused rather than reported.
    pub(crate) complete: bool,
}

/// Depth-first, pre-order walk from `root` under `limits`, numbering nodes in
/// visit order — the order the outline renders and indices are issued in.
pub(crate) async fn walk(
    conn: &zbus::Connection,
    root: Handle,
    limits: Limits,
    frame_clickable: bool,
) -> BusResult<Walk> {
    let started = Instant::now();
    let mut nodes: Vec<LinuxNode> = Vec::new();
    let mut complete = true;
    // (handle, depth, parent)
    let mut stack: Vec<(Handle, u32, Option<usize>)> = vec![(root, 0, None)];

    while let Some((handle, depth, parent)) = stack.pop() {
        if nodes.len() >= limits.max_nodes || started.elapsed() > limits.budget {
            complete = false;
            break;
        }
        let index = nodes.len();
        let (node, children) = match read_node(conn, handle, index, parent, frame_clickable).await {
            Ok(r) => r,
            // The root failing is the caller's error; a child that vanished
            // mid-walk is an ordinary race and simply is not in this walk.
            Err(e) if parent.is_none() => return Err(e),
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let descend = depth < limits.max_depth
            && !(limits.skip_offscreen && depth > 0 && !node.states.contains(State::Showing));
        if !descend && !children.is_empty() && depth >= limits.max_depth {
            complete = false;
        }
        nodes.push(node);
        if descend {
            if children.len() > limits.max_children {
                complete = false;
            }
            for child in children.into_iter().take(limits.max_children).rev() {
                stack.push((child, depth + 1, Some(index)));
            }
        }
    }
    Ok(Walk { nodes, complete })
}

/// One application on the AT-SPI bus.
#[derive(Debug, Clone)]
pub(crate) struct BusApp {
    pub(crate) root: Handle,
    pub(crate) info: cua_core::AppInfo,
}

/// The executable's file name: the closest thing Linux has to a bundle
/// identifier, and what the safety policy matches on. Read from the kernel
/// (`/proc/<pid>/exe`), not from the app, so an app cannot rename itself out
/// of the forbidden list; `comm` is the fallback for a process this user may
/// not inspect.
pub(crate) fn exe_name(pid: libc::pid_t) -> Option<String> {
    let from_exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    from_exe.or_else(|| {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .and_then(non_empty)
    })
}

/// Kernel start time of `pid` in clock ticks since boot (`/proc/<pid>/stat`
/// field 22), or `0` when unreadable. The Linux counterpart of the macOS
/// `ProcessKey::start_time`: pids are recycled here too.
pub(crate) fn process_start(pid: libc::pid_t) -> u64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return 0;
    };
    // `comm` is parenthesised and may itself contain spaces or `)`, so split
    // after the last `)`; starttime is then the 20th field.
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0;
    };
    rest.split_whitespace()
        .nth(19)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Whether `handle`'s app has a top-level window in the Active state.
async fn has_active_frame(conn: &zbus::Connection, frames: &[Handle]) -> bool {
    for f in frames {
        if let Ok(acc) = bus::accessible(conn, f).await {
            if let Ok(states) = timed(acc.get_state()).await {
                if states.contains(State::Active) {
                    return true;
                }
            }
        }
    }
    false
}

/// Every application the registry knows, frontmost first.
pub(crate) async fn list_apps(conn: &zbus::Connection) -> BusResult<Vec<BusApp>> {
    let registry = bus::accessible(conn, &Handle::new(bus::REGISTRY_BUS, bus::ROOT_PATH)).await?;
    let refs = timed(registry.get_children()).await?;
    let mut out = Vec::new();
    for r in refs.iter() {
        let Some(root) = Handle::from_ref(r) else {
            continue;
        };
        let Ok(acc) = bus::accessible(conn, &root).await else {
            continue;
        };
        let name = timed(acc.name()).await.unwrap_or_default();
        let Ok(pid) = bus::pid_of(conn, &root.bus).await else {
            continue;
        };
        let frames: Vec<Handle> = timed(acc.get_children())
            .await
            .map(|v| v.iter().filter_map(Handle::from_ref).collect())
            .unwrap_or_default();
        let active = has_active_frame(conn, &frames).await;
        out.push(BusApp {
            info: cua_core::AppInfo {
                name: non_empty(name)
                    .or_else(|| exe_name(pid))
                    .unwrap_or_else(|| "(unnamed)".to_string()),
                bundle_id: exe_name(pid),
                pid,
                active,
                // "Has a window" is the Linux reading of "regular": an app with
                // no top-level accessible is a background service as far as
                // driving it goes.
                regular: !frames.is_empty(),
            },
            root,
        });
    }
    // The same order `cua_core::apps::sort_apps` gives on macOS: frontmost
    // first, then apps with windows, then by name.
    out.sort_by_key(|a| (!a.info.active, !a.info.regular, a.info.name.to_lowercase()));
    Ok(out)
}

/// An app's top-level windows (its root's children), in registry order.
pub(crate) async fn frames(conn: &zbus::Connection, app: &BusApp) -> BusResult<Vec<Handle>> {
    let acc = bus::accessible(conn, &app.root).await?;
    Ok(timed(acc.get_children())
        .await?
        .iter()
        .filter_map(Handle::from_ref)
        .collect())
}

/// The window a snapshot should describe: the active one, else the first one
/// that is showing, else the first one at all.
pub(crate) async fn front_frame(
    conn: &zbus::Connection,
    app: &BusApp,
) -> BusResult<Option<Handle>> {
    let frames = frames(conn, app).await?;
    let mut showing: Option<Handle> = None;
    for f in &frames {
        let Ok(acc) = bus::accessible(conn, f).await else {
            continue;
        };
        let Ok(states) = timed(acc.get_state()).await else {
            continue;
        };
        if states.contains(State::Active) {
            return Ok(Some(f.clone()));
        }
        if showing.is_none() && states.contains(State::Showing) {
            showing = Some(f.clone());
        }
    }
    Ok(showing.or_else(|| frames.first().cloned()))
}

/// Re-read an element's role and label, for the drift check before acting.
pub(crate) async fn live_identity(
    conn: &zbus::Connection,
    h: &Handle,
) -> BusResult<(Role, Option<String>)> {
    let acc = bus::accessible(conn, h).await?;
    let role = timed(acc.get_role()).await?;
    let name = timed(acc.name()).await.unwrap_or_default();
    Ok((role, non_empty(name)))
}

/// `Gone` as the snapshot-level error the macOS backend gives a recycled or
/// destroyed element.
pub(crate) fn gone_or(e: BusFail, index: usize, expected: &str) -> cua_core::CoreError {
    match e {
        BusFail::Gone(_) => cua_core::CoreError::TargetChanged {
            index,
            expected: expected.to_string(),
            found: "no element (it has been destroyed since the snapshot)".to_string(),
        },
        other => other.into(),
    }
}
