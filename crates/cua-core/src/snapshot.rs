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

    // The walk is breadth-first (so the element budget is spent on shallow,
    // likely-relevant elements) but an indented outline is only readable in
    // depth-first order: printed in BFS order, a node's children appear far
    // below it, after every one of its cousins, and the indentation looks like
    // it is lying. So reconstruct parent→children links and emit depth-first,
    // keeping the indices the BFS walk assigned.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (pos, node) in nodes.iter().enumerate() {
        match node.parent {
            // A parent beyond the slice means the walk was truncated between
            // this node and its parent; treat it as a root rather than dropping
            // the whole subtree.
            Some(p) if p < nodes.len() && p != pos => children[p].push(pos),
            _ => roots.push(pos),
        }
    }

    // (position, indent) — an explicit stack, because a real AX tree can nest
    // deeply enough to blow a recursive renderer's stack.
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&r| (r, 0)).collect();
    while let Some((pos, indent)) = stack.pop() {
        let node = &nodes[pos];

        // Dropping a wrapper must not leave a phantom indent step, so children
        // of a hidden node are drawn at the hidden node's own level.
        let child_indent = if should_render(node, opts) {
            for _ in 0..indent {
                out.push_str("  ");
            }
            write_node(&mut out, node, opts);
            out.push('\n');
            indent + 1
        } else {
            skipped += 1;
            indent
        };

        for &child in children[pos].iter().rev() {
            stack.push((child, child_indent));
        }
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

/// Roles whose `AXValue` is actually text a caller can write.
const TEXT_ROLES: &[&str] = &[
    "AXTextField",
    "AXTextArea",
    "AXSearchField",
    "AXComboBox",
    "AXStaticText",
];

/// Whether `editable` is worth printing for this node.
///
/// `AXUIElementIsAttributeSettable(AXValue)` is not the right test on its own.
/// Chromium reports `AXValue` as settable on essentially every element it
/// exposes — buttons, groups, toolbars — so trusting it alone tags a whole
/// Chrome window `(editable)` and trains the model to try `set_value` on a
/// toolbar. Require the role to be one where writing text is meaningful.
fn is_text_editable(node: &AxNode) -> bool {
    if !node.settable {
        return false;
    }
    TEXT_ROLES.contains(&node.role.as_str())
        || node
            .subrole
            .as_deref()
            .is_some_and(|s| s.contains("Text") || s.contains("SearchField"))
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
    if is_text_editable(node) {
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
    fn output_is_depth_first_even_though_the_walk_was_breadth_first() {
        // BFS order: window, then both toolbars, then their buttons. Printed in
        // that order the indentation would be a lie, so the renderer must
        // regroup each toolbar with its own child.
        let nodes = vec![
            node(0, None, "AXWindow", Some("W")),
            node(1, Some(0), "AXToolbar", Some("top")),
            node(2, Some(0), "AXToolbar", Some("bottom")),
            actionable(node(3, Some(1), "AXButton", Some("in-top"))),
            actionable(node(4, Some(2), "AXButton", Some("in-bottom"))),
        ];
        let out = render_tree(&nodes, RenderOptions::default());
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines,
            vec![
                "AXWindow \"W\"",
                "  AXToolbar \"top\"",
                "    [3] AXButton \"in-top\"",
                "  AXToolbar \"bottom\"",
                "    [4] AXButton \"in-bottom\"",
            ],
            "got {out}"
        );
    }

    #[test]
    fn a_truncated_parent_link_does_not_drop_the_subtree() {
        // The walk hit its budget, so node 0's parent (index 9) is not present.
        // It must still be rendered, as a root.
        let mut orphan = actionable(node(0, Some(9), "AXButton", Some("orphan")));
        orphan.parent = Some(9);
        let out = render_tree(&[orphan], RenderOptions::default());
        assert!(out.contains("[0] AXButton \"orphan\""), "got {out}");
    }

    #[test]
    fn settable_alone_does_not_earn_the_editable_flag() {
        // Chromium marks AXValue settable on buttons and groups; tagging those
        // "editable" would invite set_value on a toolbar.
        let mut button = actionable(node(0, None, "AXButton", Some("Reload")));
        button.settable = true;
        let out = render_tree(&[button], RenderOptions::default());
        assert!(!out.contains("editable"), "got {out}");

        let mut field = actionable(node(1, None, "AXTextField", Some("Address")));
        field.settable = true;
        let out = render_tree(&[field], RenderOptions::default());
        assert!(out.contains("(editable)"), "got {out}");
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
