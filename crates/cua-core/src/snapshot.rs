//! Turning an accessibility tree into something worth spending tokens on.

use cua_ax::AxNode;

/// Roles that carry no information for an agent.
///
/// These are pure layout scaffolding. A real window has hundreds of them and
/// they never have a label, a value, or an action — they exist to hold other
/// elements. Their *children* are kept; only the wrapper line is dropped.
const STRUCTURAL_ROLES: &[&str] = &[
    "AXGroup",
    "AXSplitGroup",
    "AXScrollArea",
    "AXLayoutArea",
    "AXLayoutItem",
    "AXUnknown",
    "AXSplitter",
];

/// Render a flat node list as an indented outline for an LLM.
///
/// # Format
///
/// ```text
/// AXWindow "Inbox"
///   AXToolbar
///     [3] AXButton "Compose"
///     [4] AXTextField "Search" = "" (placeholder)
///   [9] AXTable "Messages"
///     [10] AXRow "Anna — Lunch?" (selected)
/// ```
///
/// The bracketed number is the node's `index`, and it is the *only* handle the
/// agent ever needs: `click {"element_index": "3"}`. Nodes without a bracket
/// are context — present so the agent can see structure, but not targetable,
/// which keeps it from trying to click a scroll area.
///
/// Indentation is by tree depth rather than by JSON nesting because it costs
/// roughly a third of the tokens for the same information, and because a
/// truncated outline is still readable while truncated JSON is not.
pub fn render_tree(nodes: &[AxNode], opts: RenderOptions) -> String {
    let mut out = String::with_capacity(nodes.len() * 48);
    let mut skipped = 0usize;

    // Depth is remapped so that dropping a wrapper does not leave a visual gap:
    // a child of a hidden group is drawn at the group's own indent level.
    let mut visible_depth: Vec<usize> = vec![0; nodes.len()];

    for node in nodes {
        let indent = node.parent.map(|p| visible_depth[p]).unwrap_or(0);

        if !should_render(node, opts) {
            skipped += 1;
            visible_depth[node.index] = indent;
            continue;
        }
        visible_depth[node.index] = indent + 1;

        for _ in 0..indent {
            out.push_str("  ");
        }
        write_node(&mut out, node, opts);
        out.push('\n');
    }

    if skipped > 0 && opts.note_omissions {
        out.push_str(&format!(
            "\n({skipped} structural or empty elements omitted)\n"
        ));
    }
    out
}

/// Knobs for [`render_tree`].
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Include elements that have no action, no label and no value.
    pub include_noise: bool,
    /// Append frame geometry to every line. Off by default: an AX-driven agent
    /// targets by index, so coordinates are dead weight unless the caller
    /// specifically wants to reason about layout.
    pub include_frames: bool,
    /// Print the trailing "N elements omitted" note.
    pub note_omissions: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            include_noise: false,
            include_frames: false,
            note_omissions: true,
        }
    }
}

fn should_render(node: &AxNode, opts: RenderOptions) -> bool {
    if opts.include_noise {
        return true;
    }
    // Anything actionable is always worth a line.
    if node.is_actionable() {
        return true;
    }
    // As is anything carrying text the agent might be looking for.
    if node.label.is_some() || node.value.is_some() {
        return true;
    }
    // Windows anchor the outline even when unlabeled.
    if node.role == "AXWindow" {
        return true;
    }
    !STRUCTURAL_ROLES.contains(&node.role.as_str())
}

fn write_node(out: &mut String, node: &AxNode, opts: RenderOptions) {
    // Only actionable nodes get a handle. Handing the agent an index it cannot
    // act on invites a call that can only fail.
    if node.is_actionable() {
        out.push_str(&format!("[{}] ", node.index));
    }

    out.push_str(&node.role);
    if let Some(sub) = &node.subrole {
        // Subrole is what distinguishes a close button from a plain button, and
        // a search field from a text field. Worth the tokens.
        out.push_str(&format!(":{}", sub.trim_start_matches("AX")));
    }

    if let Some(label) = &node.label {
        out.push_str(&format!(" {:?}", truncate(label, 120)));
    }

    if let Some(value) = &node.value {
        // Suppress a value that merely repeats the label, which is the common
        // case for buttons and static text and pure duplication in a prompt.
        if node.label.as_deref() != Some(value.as_str()) {
            out.push_str(&format!(" = {:?}", truncate(value, 200)));
        }
    }

    let mut flags: Vec<&str> = Vec::new();
    if !node.enabled {
        flags.push("disabled");
    }
    if node.focused {
        flags.push("focused");
    }
    if node.selected {
        flags.push("selected");
    }
    if node.settable {
        flags.push("editable");
    }
    if !flags.is_empty() {
        out.push_str(&format!(" ({})", flags.join(", ")));
    }

    if opts.include_frames {
        if let Some(f) = node.frame {
            out.push_str(&format!(
                " @{},{} {}x{}",
                f.origin.x as i64, f.origin.y as i64, f.size.width as i64, f.size.height as i64
            ));
        }
    }
}

/// Cut a string to `max` characters without splitting a UTF-8 boundary.
///
/// Guards against a single pathological element — a whole document's text in one
/// `AXValue`, which is normal for a text view — blowing the prompt budget.
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', "\\n");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_ax::Element;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn node(index: usize, parent: Option<usize>, role: &str, label: Option<&str>) -> AxNode {
        AxNode {
            index,
            depth: 0,
            parent,
            role: role.to_string(),
            subrole: None,
            label: label.map(str::to_string),
            value: None,
            help: None,
            frame: Some(CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 10.0,
                    height: 10.0,
                },
            }),
            enabled: true,
            focused: false,
            selected: false,
            actions: vec![],
            settable: false,
            element: Element::system_wide(),
        }
    }

    fn actionable(mut n: AxNode) -> AxNode {
        n.actions = vec!["AXPress".to_string()];
        n
    }

    #[test]
    fn only_actionable_nodes_get_a_handle() {
        let nodes = vec![
            node(0, None, "AXWindow", Some("Inbox")),
            actionable(node(1, Some(0), "AXButton", Some("Compose"))),
        ];
        let out = render_tree(&nodes, RenderOptions::default());
        assert!(out.contains("AXWindow \"Inbox\""));
        assert!(!out.contains("[0]"), "a window is context, not a target");
        assert!(out.contains("[1] AXButton \"Compose\""));
    }

    #[test]
    fn structural_wrappers_are_dropped_but_children_keep_their_place() {
        let nodes = vec![
            node(0, None, "AXWindow", Some("W")),
            node(1, Some(0), "AXGroup", None),
            actionable(node(2, Some(1), "AXButton", Some("OK"))),
        ];
        let out = render_tree(&nodes, RenderOptions::default());
        assert!(!out.contains("AXGroup"), "empty groups are noise");
        // The button sits one level under the window, not two: dropping the
        // group must not leave a phantom indent step.
        let line = out.lines().find(|l| l.contains("AXButton")).unwrap();
        assert_eq!(line, "  [2] AXButton \"OK\"", "got {line:?}");
    }

    #[test]
    fn a_value_that_merely_repeats_the_label_is_not_printed_twice() {
        let mut n = actionable(node(0, None, "AXStaticText", Some("Hello")));
        n.value = Some("Hello".to_string());
        let out = render_tree(&[n], RenderOptions::default());
        assert_eq!(out.matches("Hello").count(), 1, "got {out:?}");
    }

    #[test]
    fn long_values_are_truncated_on_char_boundaries() {
        let mut n = actionable(node(0, None, "AXTextArea", None));
        n.value = Some("한글".repeat(500));
        let out = render_tree(&[n], RenderOptions::default());
        assert!(out.contains('…'));
        // Must not panic and must stay bounded.
        assert!(out.chars().count() < 400, "len was {}", out.chars().count());
    }

    #[test]
    fn newlines_in_a_value_never_break_the_outline() {
        let mut n = actionable(node(0, None, "AXTextArea", None));
        n.value = Some("line one\nline two".to_string());
        let out = render_tree(&[n], RenderOptions::default());
        assert_eq!(out.trim().lines().count(), 1, "got {out:?}");
        assert!(out.contains("\\n"));
    }
}
