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

    // Subtree sizes, needed only to decide what to collapse in skeleton mode.
    // Computed bottom-up over the flat list: a BFS walk guarantees a parent's
    // position precedes its children's, so iterating in reverse visits every
    // child before its parent.
    let mut subtree: Vec<usize> = vec![1; nodes.len()];
    if opts.skeleton {
        for pos in (0..nodes.len()).rev() {
            let own: usize = children[pos].iter().map(|&c| subtree[c]).sum();
            subtree[pos] += own;
        }
    }

    // (position, indent) — an explicit stack, because a real AX tree can nest
    // deeply enough to blow a recursive renderer's stack.
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&r| (r, 0)).collect();
    let mut collapsed = 0usize;
    while let Some((pos, indent)) = stack.pop() {
        let node = &nodes[pos];

        // Dropping a wrapper must not leave a phantom indent step, so children
        // of a hidden node are drawn at the hidden node's own level.
        let rendered = should_render(node, opts);
        let child_indent = if rendered {
            for _ in 0..indent {
                out.push_str("  ");
            }
            write_node(&mut out, node, opts);
            indent + 1
        } else {
            skipped += 1;
            indent
        };

        // Skeleton mode: past a certain depth, a large subtree is summarized by
        // its size instead of expanded. The node keeps its handle, so it doubles
        // as the drill-in root for a follow-up `scope_element_id` call. This is
        // the difference between a 12k-element Slack window costing the whole
        // context window and costing forty lines.
        let descendants = subtree[pos].saturating_sub(1);
        let collapse = opts.skeleton
            && rendered
            && indent >= opts.skeleton_depth
            && descendants > opts.collapse_over;

        if collapse {
            out.push_str(&format!(
                "  (+{descendants} elements — pass scope_element_id={} to expand)",
                node.index
            ));
            collapsed += descendants;
        }
        if rendered {
            out.push('\n');
        }

        if !collapse {
            for &child in children[pos].iter().rev() {
                stack.push((child, child_indent));
            }
        }
    }

    if opts.note_omissions {
        if collapsed > 0 {
            out.push_str(&format!(
                "\n(skeleton: {collapsed} elements collapsed into their containers; \
                 pass scope_element_id=N to expand one, or skeleton=false for everything)\n"
            ));
        }
        if skipped > 0 {
            out.push_str(&format!(
                "\n({skipped} structural or empty elements omitted)\n"
            ));
        }
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
    /// Summarize large deep subtrees by their size instead of expanding them.
    pub skeleton: bool,
    /// Indent level below which nothing is collapsed.
    ///
    /// Shallow structure is where an agent orients itself — window, then toolbar
    /// / sidebar / content area — so collapsing at the top would hide the map
    /// along with the territory. `2` keeps the window and its direct children
    /// always expanded, which is the smallest useful map; a table of 40 rows
    /// hanging off a toolbar sits at exactly this depth and is precisely what
    /// should collapse.
    pub skeleton_depth: usize,
    /// Minimum descendant count before a subtree is worth collapsing.
    ///
    /// Below this, the summary line costs about as much as the elements it
    /// replaces while being strictly less useful.
    pub collapse_over: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            include_noise: false,
            include_frames: false,
            note_omissions: true,
            skeleton: false,
            skeleton_depth: 2,
            collapse_over: 8,
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

/// What changed between two rendered trees.
///
/// Line-level and deliberately not positional. A structural diff over `AxNode`
/// would need stable identity across snapshots, and there is none: `index` is a
/// position in a walk that can reorder, and the retained handles are new objects
/// each time. Comparing the rendered outline sidesteps the problem, and the
/// outline is what the agent reads anyway — a change it cannot see in the text
/// is a change it cannot act on.
///
/// Indentation is kept in the reported lines, so a caller can still tell roughly
/// where in the tree something appeared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Compare two rendered trees, reporting only lines that appeared or vanished.
///
/// Multiset semantics, so a line that occurs three times before and once after
/// is reported as two removals rather than as unchanged. Order within each list
/// follows the tree it came from.
///
/// The point is size: an app whose outline runs to hundreds of nodes produces a
/// handful of diff lines for a click, and re-sending the whole outline after
/// every action is the difference between an affordable verification step and
/// one an agent learns to skip.
pub fn diff_trees(before: &str, after: &str) -> TreeDiff {
    use std::collections::HashMap;

    let mut before_counts: HashMap<&str, i64> = HashMap::new();
    for line in before.lines() {
        *before_counts.entry(line).or_default() += 1;
    }
    let mut after_counts: HashMap<&str, i64> = HashMap::new();
    for line in after.lines() {
        *after_counts.entry(line).or_default() += 1;
    }

    let mut added = Vec::new();
    let mut seen: HashMap<&str, i64> = HashMap::new();
    for line in after.lines() {
        let n = seen.entry(line).or_default();
        *n += 1;
        if *n > before_counts.get(line).copied().unwrap_or(0) {
            added.push(line.to_string());
        }
    }

    let mut removed = Vec::new();
    let mut seen: HashMap<&str, i64> = HashMap::new();
    for line in before.lines() {
        let n = seen.entry(line).or_default();
        *n += 1;
        if *n > after_counts.get(line).copied().unwrap_or(0) {
            removed.push(line.to_string());
        }
    }

    TreeDiff { added, removed }
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
            frame: None,
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

    fn framed(mut n: AxNode) -> AxNode {
        n.frame = Some(CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 10.0,
                height: 10.0,
            },
        });
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
    fn an_actionless_framed_node_gets_a_pid_click_handle() {
        let nodes = vec![
            node(0, None, "AXWindow", Some("Chat")),
            framed(node(1, Some(0), "AXButton", None)),
        ];
        let out = render_tree(&nodes, RenderOptions::default());
        assert!(out.contains("[1] AXButton"), "got {out}");
        assert!(nodes[1].actions.is_empty());
        assert!(nodes[1].is_actionable());
    }

    #[test]
    fn framed_layout_containers_do_not_get_click_handles() {
        for role in ["AXWindow", "AXGroup", "AXToolbar", "AXTable"] {
            let node = framed(node(0, None, role, None));
            assert!(!node.is_actionable(), "{role} must remain context-only");
        }
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

    /// A window > toolbar > deep container holding `n` leaf buttons.
    fn wide_tree(n: usize) -> Vec<AxNode> {
        let mut v = vec![
            node(0, None, "AXWindow", Some("W")),
            node(1, Some(0), "AXToolbar", Some("bar")),
            node(2, Some(1), "AXTable", Some("rows")),
        ];
        for i in 0..n {
            v.push(actionable(node(3 + i, Some(2), "AXRow", Some("row"))));
        }
        v
    }

    fn skeleton_opts() -> RenderOptions {
        RenderOptions {
            skeleton: true,
            ..RenderOptions::default()
        }
    }

    #[test]
    fn skeleton_collapses_a_big_subtree_into_a_countable_summary() {
        let nodes = wide_tree(40);
        let out = render_tree(&nodes, skeleton_opts());
        assert!(
            out.contains("(+40 elements — pass scope_element_id=2 to expand)"),
            "got {out}"
        );
        // The rows themselves must be gone, not merely reordered.
        assert_eq!(out.matches("AXRow").count(), 0, "got {out}");
    }

    #[test]
    fn a_collapsed_container_names_its_index_even_without_a_handle() {
        // scope_element_id does not require the container to be actionable, so
        // the summary must state the index outright rather than relying on a
        // "[N]" prefix that non-actionable nodes never get.
        let nodes = wide_tree(40);
        assert!(
            !nodes[2].is_actionable(),
            "fixture precondition: the container has no actions"
        );
        let out = render_tree(&nodes, skeleton_opts());
        assert!(!out.contains("[2] AXTable"), "no handle expected: {out}");
        assert!(out.contains("scope_element_id=2"), "got {out}");
    }

    #[test]
    fn a_collapsed_container_that_is_actionable_keeps_its_handle() {
        let mut nodes = wide_tree(40);
        nodes[2] = actionable(nodes[2].clone());
        let out = render_tree(&nodes, skeleton_opts());
        assert!(out.contains("[2] AXTable"), "got {out}");
        assert!(out.contains("scope_element_id=2"), "got {out}");
    }

    #[test]
    fn skeleton_leaves_shallow_structure_alone() {
        // The map must survive: window/toolbar/container are how an agent
        // orients itself, so nothing at or above skeleton_depth collapses.
        let nodes = wide_tree(40);
        let out = render_tree(&nodes, skeleton_opts());
        assert!(out.contains("AXWindow \"W\""), "got {out}");
        assert!(out.contains("AXToolbar \"bar\""), "got {out}");
    }

    #[test]
    fn skeleton_does_not_collapse_a_subtree_too_small_to_be_worth_it() {
        // 4 children < collapse_over (8): a summary line would cost about what
        // the elements cost while being strictly less useful.
        let nodes = wide_tree(4);
        let out = render_tree(&nodes, skeleton_opts());
        assert!(!out.contains("scope_element_id"), "got {out}");
        assert_eq!(out.matches("AXRow").count(), 4, "got {out}");
    }

    #[test]
    fn skeleton_is_off_by_default() {
        let nodes = wide_tree(40);
        let out = render_tree(&nodes, RenderOptions::default());
        assert!(!out.contains("scope_element_id"), "got {out}");
        assert_eq!(out.matches("AXRow").count(), 40);
    }

    #[test]
    fn skeleton_counts_whole_subtrees_not_just_direct_children() {
        // window > group > container > 10 rows, each row holding one button.
        // The container's summary must say 20, not 10.
        let mut nodes = vec![
            node(0, None, "AXWindow", Some("W")),
            node(1, Some(0), "AXToolbar", Some("bar")),
            node(2, Some(1), "AXList", Some("list")),
        ];
        for i in 0..10 {
            nodes.push(actionable(node(3 + i, Some(2), "AXRow", Some("r"))));
        }
        for i in 0..10 {
            nodes.push(actionable(node(13 + i, Some(3 + i), "AXButton", Some("b"))));
        }
        let out = render_tree(&nodes, skeleton_opts());
        assert!(out.contains("(+20 elements"), "got {out}");
    }

    #[test]
    fn skeleton_summary_totals_are_reported_once_at_the_end() {
        let nodes = wide_tree(40);
        let out = render_tree(&nodes, skeleton_opts());
        assert_eq!(
            out.matches("skeleton: 40 elements collapsed").count(),
            1,
            "got {out}"
        );
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

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn identical_trees_have_no_diff() {
        let t = "[0] AXWindow\n  [1] AXButton\n";
        assert!(diff_trees(t, t).is_empty());
    }

    #[test]
    fn reports_appearing_and_vanishing_lines() {
        let before = "[0] AXWindow\n  [1] AXButton \"Save\"\n";
        let after = "[0] AXWindow\n  [1] AXButton \"Save\"\n  [2] AXMenu \"Options\"\n";
        let d = diff_trees(before, after);
        assert_eq!(d.added, vec!["  [2] AXMenu \"Options\"".to_string()]);
        assert!(d.removed.is_empty());

        let back = diff_trees(after, before);
        assert_eq!(back.removed, vec!["  [2] AXMenu \"Options\"".to_string()]);
        assert!(back.added.is_empty());
    }

    #[test]
    fn repeated_lines_are_counted_not_deduplicated() {
        // Three identical rows collapsing to one is a real change and must not
        // read as "nothing happened" just because the text still occurs.
        let before = "  row\n  row\n  row\n";
        let after = "  row\n";
        let d = diff_trees(before, after);
        assert_eq!(d.removed.len(), 2);
        assert!(d.added.is_empty());
    }

    #[test]
    fn a_changed_value_shows_as_one_removal_and_one_addition() {
        let before = "  [3] AXTextArea = \"draft\"\n";
        let after = "  [3] AXTextArea = \"sent\"\n";
        let d = diff_trees(before, after);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.added.len(), 1);
    }
}
