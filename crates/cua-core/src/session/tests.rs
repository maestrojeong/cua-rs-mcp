use super::*;
use cua_ax::Element;
use objc2_core_foundation::{CGPoint, CGSize};

fn win(id: u32, pid: libc::pid_t, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
    WindowInfo {
        id,
        title: None,
        pid,
        bundle_id: None,
        app_name: None,
        frame: CGRect {
            origin: CGPoint { x, y },
            size: CGSize {
                width: w,
                height: h,
            },
        },
        on_screen: true,
        layer: 0,
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect {
        origin: CGPoint { x, y },
        size: CGSize {
            width: w,
            height: h,
        },
    }
}

fn tnode(index: usize, role: &str, label: Option<&str>, value: Option<&str>, act: bool) -> AxNode {
    AxNode {
        index,
        depth: 0,
        parent: None,
        role: role.to_string(),
        subrole: None,
        label: label.map(str::to_string),
        value: value.map(str::to_string),
        help: None,
        frame: None,
        enabled: true,
        focused: false,
        selected: false,
        actions: if act {
            vec!["AXPress".to_string()]
        } else {
            vec![]
        },
        settable: false,
        element: Element::system_wide(),
    }
}

fn placed(index: usize, role: &str, act: bool, f: CGRect) -> AxNode {
    let mut n = tnode(index, role, None, None, act);
    n.frame = Some(f);
    n
}

#[test]
fn a_capture_failure_blames_an_open_menu_only_when_one_is_open() {
    let bare = "screencapture exited with status 1: could not create image from window";
    let no_menu = vec![tnode(0, "AXWindow", Some("Chat"), None, false)];
    assert_eq!(
        capture_failure_warning(bare, &no_menu),
        bare,
        "with no menu open the cause is unknown, and guessing would mislead"
    );

    let with_menu = vec![
        tnode(0, "AXWindow", Some("Chat"), None, false),
        tnode(1, "AXMenu", None, None, true),
    ];
    let explained = capture_failure_warning(bare, &with_menu);
    assert!(explained.starts_with(bare), "the OS text has to survive");
    assert!(explained.contains("menu open"), "got {explained}");
}

/// A snapshot with nothing in it but the properties `diff_basis` judges.
fn basis(scoped: bool, limits: Limits, complete: bool, acted_on: bool) -> Snapshot {
    Snapshot {
        id: 1,
        nodes: Vec::new(),
        window: None,
        taken_at: Instant::now(),
        process_key: ProcessKey::for_pid(std::process::id() as libc::pid_t),
        scoped,
        limits,
        complete,
        acted_on,
        popups: Vec::new(),
    }
}

#[test]
fn a_default_whole_window_snapshot_is_a_fair_diff_basis() {
    assert!(diff_basis(&basis(false, post_action_limits(), true, false)).is_ok());
}

#[test]
fn a_scoped_or_capped_snapshot_is_refused_as_a_diff_basis() {
    assert!(
        diff_basis(&basis(true, post_action_limits(), true, false)).is_err(),
        "a subtree cannot be subtracted from a whole window"
    );
    let capped = Limits {
        max_nodes: 40,
        ..post_action_limits()
    };
    assert!(
        diff_basis(&basis(false, capped, true, false)).is_err(),
        "a 40-node walk of a 300-node window would report 260 nodes as new"
    );
}

#[test]
fn an_unfinished_walk_is_refused_as_a_diff_basis() {
    // Equal caps are not enough: the time budget depends on how fast the app
    // answers, so one walk can stop at 300 nodes and the next reach 500.
    assert!(
        diff_basis(&basis(false, post_action_limits(), false, false)).is_err(),
        "nodes the first walk never reached would read as newly appeared"
    );
}

#[test]
fn an_already_acted_on_snapshot_is_refused_as_a_diff_basis() {
    assert!(
        diff_basis(&basis(false, post_action_limits(), true, true)).is_err(),
        "a diff would blame this action for the previous action's changes too"
    );
}

#[test]
fn hit_test_breaks_an_equal_frame_tie_toward_the_deeper_element() {
    // A row and its only cell normally occupy the same rectangle, so area
    // cannot separate them. Without a depth tie-break the breadth-first walk
    // order decides, which always favours the ancestor.
    let mut row = placed(0, "AXRow", true, rect(0.0, 0.0, 500.0, 40.0));
    row.depth = 3;
    let mut cell = placed(1, "AXCell", true, rect(0.0, 0.0, 500.0, 40.0));
    cell.depth = 4;
    let nodes = vec![row, cell];
    assert_eq!(
        hit_test(&nodes, 10.0, 10.0).map(|n| n.index),
        Some(1),
        "the deeper element is the more specific answer"
    );
}

#[test]
fn frame_contains_is_half_open_on_the_far_edges() {
    let f = rect(10.0, 20.0, 100.0, 50.0);
    assert!(frame_contains(&f, 10.0, 20.0), "the near corner is inside");
    assert!(
        !frame_contains(&f, 110.0, 40.0),
        "the far x edge is outside"
    );
    assert!(!frame_contains(&f, 50.0, 70.0), "the far y edge is outside");
    assert!(!frame_contains(&f, 9.0, 40.0));
}

#[test]
fn a_capture_failure_is_only_blamed_on_a_menu_for_the_window_server_refusal() {
    let with_menu = vec![
        tnode(0, "AXWindow", Some("Chat"), None, false),
        tnode(1, "AXMenu", None, None, true),
    ];
    let unrelated = "screencapture worker timed out after 5s";
    assert_eq!(
        capture_failure_warning(unrelated, &with_menu),
        unrelated,
        "a timeout is not evidence about menus, even with a menu on screen"
    );

    let refusal = "screencapture exited with status 1: could not create image from window";
    assert!(capture_failure_warning(refusal, &with_menu).contains("may be why"));
}

#[test]
fn hit_test_prefers_the_actionable_element_over_the_label_drawn_on_it() {
    let nodes = vec![
        placed(0, "AXWindow", false, rect(0.0, 0.0, 500.0, 400.0)),
        placed(1, "AXButton", true, rect(100.0, 100.0, 80.0, 30.0)),
        placed(2, "AXStaticText", false, rect(110.0, 105.0, 40.0, 20.0)),
    ];
    let hit = hit_test(&nodes, 120.0, 110.0).expect("point is inside all three");
    assert_eq!(hit.index, 1, "a static label is not a thing you can click");
}

#[test]
fn hit_test_prefers_the_smallest_of_nested_actionable_frames() {
    let nodes = vec![
        placed(0, "AXRow", true, rect(0.0, 0.0, 500.0, 50.0)),
        placed(1, "AXButton", true, rect(400.0, 10.0, 40.0, 30.0)),
    ];
    assert_eq!(hit_test(&nodes, 410.0, 20.0).map(|n| n.index), Some(1));
    assert_eq!(hit_test(&nodes, 10.0, 20.0).map(|n| n.index), Some(0));
}

#[test]
fn hit_test_answers_nothing_outside_every_frame() {
    let nodes = vec![placed(0, "AXWindow", false, rect(0.0, 0.0, 100.0, 100.0))];
    assert!(
        hit_test(&nodes, 500.0, 500.0).is_none(),
        "a miss has to be reportable, not silently retargeted at the menu bar"
    );
    assert!(
        hit_test(&nodes, 100.0, 50.0).is_none(),
        "the far edge is exclusive, so adjacent frames cannot both claim a point"
    );
}

#[test]
fn pid_click_revalidation_uses_the_live_moved_window_frame() {
    let snapshot = win(7, 42, 100.0, 100.0, 800.0, 600.0);
    let moved = win(7, 42, -400.0, 50.0, 800.0, 600.0);
    let live = current_window_for_pid_click(&[moved], &snapshot, 42, -200.0, 200.0)
        .expect("same id and pid should survive a move");
    assert_eq!(live.frame.origin.x, -400.0);
    assert_eq!(
        (-200.0 - live.frame.origin.x, 200.0 - live.frame.origin.y),
        (200.0, 150.0),
        "window-local input must use the live frame, not the snapshot frame"
    );
}

#[test]
fn pid_click_revalidation_rejects_a_recycled_window_id() {
    let snapshot = win(7, 42, 0.0, 0.0, 800.0, 600.0);
    let recycled = win(7, 99, 0.0, 0.0, 800.0, 600.0);
    let err = current_window_for_pid_click(&[recycled], &snapshot, 42, 100.0, 100.0)
        .expect_err("same window id owned by another pid must fail closed");
    assert!(
        err.contains("does not currently belong to pid 42"),
        "got {err}"
    );
}

#[test]
fn pid_click_revalidation_rejects_ax_window_drift() {
    let snapshot = win(7, 42, 0.0, 0.0, 800.0, 600.0);
    let live = snapshot.clone();
    let err = current_window_for_pid_click(&[live], &snapshot, 42, 900.0, 100.0)
        .expect_err("a point outside the validated window must not be posted");
    assert!(err.contains("outside the current frame"), "got {err}");
}

/// Every key this maps to a verb must be one `press_key` can deliver
/// without focusing anything.
#[test]
fn the_keys_offered_as_the_focus_free_alternative_really_are() {
    for key in ["return", "enter", "escape", "esc", "up", "down"] {
        assert!(
            ax_verb_for_key(key).is_some(),
            "{key} is advertised as needing no focus but has no AX verb"
        );
    }
}

/// The whole point of putting the role in the token is that a caller can
/// be told *what changed*, not just that something did.
#[test]
fn a_token_role_mismatch_names_both_roles() {
    let err = CoreError::TokenRoleMismatch {
        index: 233,
        expected: "AXCell".into(),
        found: "AXButton".into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("233"), "got {msg}");
    assert!(
        msg.contains("AXCell") && msg.contains("AXButton"),
        "got {msg}"
    );
    assert!(
        msg.contains("get_app_state"),
        "the remedy must be in the message: {msg}"
    );
}

#[test]
fn observed_labels_are_stable_and_three_valued() {
    assert_eq!(Observed::Changed.as_str(), "yes");
    assert_eq!(Observed::Unchanged.as_str(), "no");
    assert_eq!(Observed::Unknown.as_str(), "unknown");
}

#[test]
fn recycled_rows_fail_the_exact_target_check() {
    let expected = HashSet::from(["Alice".to_string(), "Profile".to_string()]);
    let same_row = HashSet::from([
        "Alice".to_string(),
        "Profile".to_string(),
        "New preview".to_string(),
    ]);
    let other_row = HashSet::from(["Bob".to_string(), "Profile".to_string()]);

    assert!(tokens_still_present(&expected, &same_row));
    assert!(!tokens_still_present(&expected, &other_row));
}

#[test]
fn appkit_placeholder_identifiers_are_not_identity() {
    let mut out = HashSet::new();
    push_token(Some("_NS:87".to_string()), &mut out);
    push_token(Some("   ".to_string()), &mut out);
    push_token(None, &mut out);
    push_token(Some("Alice".to_string()), &mut out);
    assert_eq!(sorted(&out), vec!["Alice"]);
}

#[test]
fn find_matches_case_insensitively_across_label_value_and_role() {
    let nodes = vec![
        tnode(0, "AXButton", Some("Send"), None, true),
        tnode(1, "AXTextArea", None, Some("please SEND it"), true),
        tnode(2, "AXSendButton", None, None, true),
        tnode(3, "AXButton", Some("Cancel"), None, true),
    ];
    let hits = match_nodes(&nodes, "send", 10);
    assert_eq!(hits.len(), 3, "got {hits:?}");
    // Label match ranks above value match, which ranks above role match.
    assert!(hits[0].contains("\"Send\""), "got {:?}", hits[0]);
    assert!(hits[1].contains("please SEND it"), "got {:?}", hits[1]);
    assert!(hits[2].contains("AXSendButton"), "got {:?}", hits[2]);
}

#[test]
fn find_puts_actionable_matches_before_context() {
    let nodes = vec![
        tnode(0, "AXStaticText", Some("Save now"), None, false),
        tnode(1, "AXButton", Some("Save"), None, true),
    ];
    let hits = match_nodes(&nodes, "save", 10);
    assert!(
        hits[0].starts_with("[1] "),
        "actionable first, got {hits:?}"
    );
    assert!(hits[1].starts_with("(not actionable)"), "got {hits:?}");
}

#[test]
fn find_respects_the_limit_and_rejects_an_empty_needle() {
    let nodes: Vec<AxNode> = (0..10)
        .map(|i| tnode(i, "AXButton", Some("item"), None, true))
        .collect();
    assert_eq!(match_nodes(&nodes, "item", 3).len(), 3);
    // A zero limit must still return something rather than silently nothing.
    assert_eq!(match_nodes(&nodes, "item", 0).len(), 1);
    assert!(match_nodes(&nodes, "", 5).is_empty());
}

#[test]
fn find_does_not_print_a_value_that_repeats_the_label() {
    let nodes = vec![tnode(0, "AXStaticText", Some("dup"), Some("dup"), true)];
    let hits = match_nodes(&nodes, "dup", 5);
    assert_eq!(hits[0].matches("dup").count(), 1, "got {:?}", hits[0]);
}

#[test]
fn only_intent_like_keys_have_an_ax_verb() {
    assert_eq!(ax_verb_for_key("return"), Some("AXConfirm"));
    assert_eq!(ax_verb_for_key("Enter"), Some("AXConfirm"));
    assert_eq!(ax_verb_for_key(" ESC "), Some("AXCancel"));
    assert_eq!(ax_verb_for_key("up"), Some("AXIncrement"));
    assert_eq!(ax_verb_for_key("down"), Some("AXDecrement"));
    // cua-rs deliberately refuses keys AX cannot address to an element.
    assert_eq!(ax_verb_for_key("cmd+shift+p"), None);
    assert_eq!(ax_verb_for_key("a"), None);
    assert_eq!(ax_verb_for_key("f5"), None);
}

#[test]
fn a_page_request_prefers_accessibility_and_falls_through_when_there_is_none() {
    // The whole point of keeping both tiers: the AX verb is better where it
    // exists, and where it does not there used to be nothing at all.
    assert_eq!(
        scroll_tier(ScrollAmount::Pages(1), true),
        ScrollTier::Ax,
        "an element that advertises AXScroll*ByPage should be paged through it"
    );
    assert_eq!(
        scroll_tier(ScrollAmount::Pages(1), false),
        ScrollTier::Wheel,
        "an Electron list advertises nothing, and used to be unscrollable"
    );
}

#[test]
fn a_distance_request_is_always_an_event() {
    // Accessibility has no vocabulary for "scroll 120 points" — only whole
    // pages — so asking in points cannot be served by the AX tier even
    // where the AX tier is available.
    assert_eq!(
        scroll_tier(ScrollAmount::Points(120), true),
        ScrollTier::Wheel
    );
    assert_eq!(
        scroll_tier(ScrollAmount::Points(120), false),
        ScrollTier::Wheel
    );
}

#[test]
fn a_page_on_the_wheel_tier_is_sized_from_the_element() {
    // 90% of the element's own height, so a page of a tall list and a page
    // of a short sidebar are different distances, as they should be.
    assert_eq!(page_points(Some(1000.0)), 900);
    assert_eq!(page_points(Some(200.0)), 180);
    // ...bounded at both ends, and with a usable answer for an element that
    // publishes no frame at all.
    assert_eq!(page_points(Some(1.0)), 60);
    assert_eq!(page_points(Some(100_000.0)), 4000);
    assert_eq!(page_points(None), 360);
    assert_eq!(page_points(Some(f64::NAN)), 360);
    assert_eq!(page_points(Some(-5.0)), 360);
}

#[test]
fn wheel_deltas_point_the_way_the_direction_says() {
    // Positive vertical is up and positive horizontal is left, per
    // CGEventCreateScrollWheelEvent2. Inverting either is the easiest
    // possible mistake and is invisible without an app to watch.
    assert_eq!(ScrollDir::Up.wheel_delta(120), (120, 0));
    assert_eq!(ScrollDir::Down.wheel_delta(120), (-120, 0));
    assert_eq!(ScrollDir::Left.wheel_delta(120), (0, 120));
    assert_eq!(ScrollDir::Right.wheel_delta(120), (0, -120));
}

#[test]
fn mouse_options_parse_the_same_modifier_vocabulary_as_press_key() {
    let m = MouseOptions::parse("right", "cmd+shift").unwrap();
    assert_eq!(m.button, MouseButton::Right);
    assert!(m.modifiers.contains(Modifiers::MaskCommand));
    assert!(m.modifiers.contains(Modifiers::MaskShift));
    assert_eq!(m.count, 1, "a click count is not part of parsing");

    // Both fields empty is the ordinary click, not an error: an MCP caller
    // forwards optional strings and should not have to special-case them.
    let d = MouseOptions::parse("", "").unwrap();
    assert_eq!(d.button, MouseButton::Left);
    assert!(d.modifiers.is_empty());

    assert!(MouseOptions::parse("mouse3", "").is_err());
    assert!(MouseOptions::parse("left", "cmd+clik").is_err());
}

#[test]
fn a_mouse_option_set_describes_itself_in_the_words_it_was_given() {
    // The result line has to be quotable back into the next call.
    assert_eq!(MouseOptions::default().describe(), "left");
    assert_eq!(
        MouseOptions::parse("right", "cmd+shift")
            .unwrap()
            .describe(),
        "cmd+shift right"
    );
    // Canonical order regardless of how the caller wrote it, so two
    // equivalent calls do not produce two different-looking results.
    assert_eq!(
        MouseOptions::parse("left", "shift+cmd").unwrap().describe(),
        MouseOptions::parse("left", "cmd+shift").unwrap().describe()
    );
}

#[test]
fn a_drag_end_names_itself_before_anything_is_resolved() {
    // A drag error has to name both ends, and one of them may be the end
    // that failed to resolve at all.
    assert_eq!(
        describe_location(&PointerLocation::Element(Target::Index {
            index: 12,
            snapshot_id: None,
            expected_role: None,
        })),
        "element 12"
    );
    assert_eq!(
        describe_location(&PointerLocation::WindowPoint { x: 40.4, y: 12.0 }),
        "window-local (40, 12)"
    );
}

#[test]
fn the_coordinate_guard_passes_when_the_generation_matches_and_fails_when_it_does_not() {
    let mut inner = Inner::default();
    let info = AppInfo {
        name: "Test".into(),
        bundle_id: None,
        pid: 4242,
        active: false,
        regular: true,
    };
    inner.snapshots.insert(
        info.pid,
        Snapshot {
            id: 7,
            nodes: Vec::new(),
            window: None,
            process_key: ProcessKey::for_pid(info.pid),
            limits: Limits::default(),
            complete: true,
            scoped: false,
            acted_on: false,
            taken_at: Instant::now(),
            popups: Vec::new(),
        },
    );

    // Not citing a generation is allowed: the common flow reads and acts in
    // one turn, and requiring the id would add a failure mode where there
    // is no risk.
    assert!(inner
        .check_coordinate_generation(&info, None, (10.0, 10.0))
        .is_ok());
    assert!(inner
        .check_coordinate_generation(&info, Some(7), (10.0, 10.0))
        .is_ok());

    let err = inner
        .check_coordinate_generation(&info, Some(3), (10.0, 10.0))
        .unwrap_err();
    match err {
        CoreError::StaleCoordinate { given, current, .. } => {
            assert_eq!((given, current), (3, 7));
        }
        other => panic!("expected StaleCoordinate, got {other}"),
    }
    // The message has to say what to do, not just that something is wrong.
    let text = inner
        .check_coordinate_generation(&info, Some(3), (10.0, 10.0))
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("get_app_state") && text.contains("nothing about a stale point looks wrong"),
        "must explain why a coordinate needs this guard at all: {text}"
    );
}

#[test]
fn delivery_labels_are_stable() {
    assert_eq!(Delivery::Ax.as_str(), "ax");
    assert_eq!(Delivery::Pid.as_str(), "pid");
    // The parenthetical is the load-bearing part of this label, not
    // decoration: it is the only place a caller learns that this result
    // confirms delivery and not that anything was hit.
    assert_eq!(Delivery::PidNoElement.as_str(), "pid (no element)");
    assert_eq!(Delivery::PidKey.as_str(), "pid (keyboard)");
}

#[test]
fn focus_is_classified_from_the_read_back_not_from_the_write() {
    // The app naming the addressed element is the only positive evidence
    // there is, and it is enough on its own.
    assert_eq!(classify_focus(Some(true)), FocusState::Verified);
    // A different element of the same process. Not "failed" — the keys
    // were still delivered — but the caller has to be told.
    assert_eq!(classify_focus(Some(false)), FocusState::Mismatched);
    // Silence. Deliberately its own answer rather than being folded into
    // `Mismatched`: an app that publishes no `AXFocusedUIElement` is not
    // an app that published the wrong one, and refusing on it would refuse
    // almost everything.
    assert_eq!(classify_focus(None), FocusState::Unverified);
}

#[test]
fn focus_labels_are_stable() {
    assert_eq!(FocusState::Verified.as_str(), "verified");
    assert_eq!(FocusState::Unverified.as_str(), "unverified");
    assert_eq!(FocusState::Mismatched.as_str(), "mismatched");
}

#[test]
fn only_mismatched_focus_is_strict_mode_worthy() {
    // Strict mode's rule, stated as a test so that widening it later is a
    // deliberate edit: `Unverified` delivers. It has to, or `press_key`
    // would start failing on every app that answers nothing.
    let refusable = |state| state == FocusState::Mismatched;
    assert!(refusable(FocusState::Mismatched));
    assert!(!refusable(FocusState::Unverified));
    assert!(!refusable(FocusState::Verified));
}

#[test]
fn mechanism_defaults_to_the_accessibility_write() {
    // The default is the decision, not an accident of ordering: a bulk
    // text write is the one operation AX expresses better than events.
    assert_eq!(Mechanism::default(), Mechanism::Ax);
    assert_eq!(Mechanism::parse("ax"), Ok(Mechanism::Ax));
    assert_eq!(Mechanism::parse("keystrokes"), Ok(Mechanism::Keystrokes));
    // Tolerant about shape, not about spelling.
    assert_eq!(Mechanism::parse("  KeyStrokes "), Ok(Mechanism::Keystrokes));
}

#[test]
fn an_unknown_mechanism_is_an_error_rather_than_the_default() {
    // Falling back to `ax` here would write `AXValue` into a terminal that
    // ignores it and report success, which is the exact failure the
    // explicit mechanism exists to prevent.
    let err = Mechanism::parse("keystroke").expect_err("a near-miss must not be accepted");
    assert!(
        err.contains("keystrokes"),
        "the error names the two valid values: {err}"
    );
    assert!(Mechanism::parse("hid").is_err());
    assert!(Mechanism::parse("").is_err());
}

#[test]
fn mechanism_labels_are_stable() {
    assert_eq!(Mechanism::Ax.as_str(), "ax");
    assert_eq!(Mechanism::Keystrokes.as_str(), "keystrokes");
}

#[test]
fn strict_focus_is_off_unless_the_flag_says_otherwise() {
    // The switch parser every env flag in this crate shares, exercised
    // without touching the process environment (`cargo test` shares it
    // across threads, which would make a `set_var` here racy).
    assert!(!flag_is_on(None), "unset means off — deliver anyway");
    assert!(!flag_is_on(Some("0")));
    assert!(!flag_is_on(Some("")));
    assert!(!flag_is_on(Some("yes")), "only 1/true, so a typo is off");
    assert!(flag_is_on(Some("1")));
    assert!(flag_is_on(Some("true")));
    assert!(flag_is_on(Some("TRUE")));
}

#[test]
fn a_window_local_click_is_re_anchored_to_the_window_that_moved() {
    // The whole reason `click_in_window` takes window-local coordinates: the
    // caller read a screenshot of a window at one place, the user dragged the
    // window, and the click must still land on the same pixel of the same
    // content rather than on whatever now occupies the old screen point.
    let live = win(7, 42, 500.0, 300.0, 800.0, 600.0);
    let resolved = live_window_for_pid_click(std::slice::from_ref(&live), 7, 42)
        .expect("the window is present and owned by this pid");
    let (x, y) = (120.0, 40.0);
    let screen = (resolved.frame.origin.x + x, resolved.frame.origin.y + y);
    assert_eq!(screen, (620.0, 340.0));
    assert!(screen_point_inside(&resolved, screen.0, screen.1).is_ok());
}

#[test]
fn a_window_local_click_past_the_windows_size_is_refused() {
    let live = win(7, 42, 500.0, 300.0, 800.0, 600.0);
    // 900 points across an 800-point-wide window. Adding the origin makes
    // this a perfectly valid *screen* point that happens to be over the
    // window next door, which is precisely the mistake to refuse.
    let err = screen_point_inside(&live, 500.0 + 900.0, 300.0 + 40.0)
        .expect_err("a point past the window's width must not be posted");
    assert!(err.contains("500,300 800x600"), "got {err}");
}

#[test]
fn a_window_local_click_will_not_borrow_another_apps_window_id() {
    // A pid-addressed event stamped with a window id belonging to someone
    // else is the one outcome this tier must make impossible.
    let other_app = win(7, 99, 0.0, 0.0, 800.0, 600.0);
    let err = live_window_for_pid_click(&[other_app], 7, 42)
        .expect_err("a window owned by another pid must fail closed");
    assert!(
        err.contains("does not currently belong to pid 42"),
        "got {err}"
    );
}

#[test]
fn a_panicking_native_job_returns_an_error_without_killing_the_worker() {
    let cua = Cua::new();
    let err = cua
        .exec::<(), _>(|_| panic!("synthetic native failure"))
        .expect_err("panic must be returned to the caller");
    assert!(matches!(err, CoreError::NativePanic));

    // A follow-up request must still be serviceable; otherwise MCP sees a
    // connection close instead of the original tool error.
    assert_eq!(cua.exec(|_| 7usize).unwrap(), 7);
}

#[test]
fn pid_click_failure_promises_no_pointer_fallback() {
    let msg = CoreError::PidClickUnavailable {
        original: cua_ax::AxError::Unsupported {
            what: "action",
            name: "any of [\"AXPress\", \"AXPick\", \"AXConfirm\"]".into(),
        },
        reason: "SLEventPostToPid unavailable".into(),
    }
    .to_string();
    assert!(
        msg.contains("AXPress"),
        "must keep the original AX error visible: {msg}"
    );
    assert!(msg.contains("will not fall back to moving"), "got {msg}");
    assert!(
        msg.contains("AXShowMenu"),
        "must point at a background-safe alternative too: {msg}"
    );
}

#[test]
fn an_unsupported_ax_verb_does_not_contradict_itself() {
    // The bug this guards: escape *does* have an AX verb, so refusing it
    // with the generic HID message produced text that named escape as
    // something that works without HID.
    let msg = CoreError::KeyVerbUnsupported {
        key: "escape".into(),
        verb: "AXCancel",
        available: r#"["AXPress"]"#.into(),
    }
    .to_string();
    assert!(msg.contains("AXCancel"), "must name the verb: {msg}");
    assert!(
        msg.contains("[\"AXPress\"]"),
        "must list what the element does accept: {msg}"
    );
    assert!(
        !msg.contains("escape work"),
        "must not claim escape works while refusing escape: {msg}"
    );
}

#[test]
fn refusing_a_chord_explains_the_ax_alternatives() {
    let msg = CoreError::KeyNoAccessibilityEquivalent {
        key: "cmd+shift+p".into(),
    }
    .to_string();
    assert!(msg.contains("does not synthesize shared HID"), "got {msg}");
    assert!(
        msg.contains("AXShowMenu") && msg.contains("return/enter"),
        "must point at background-safe alternatives: {msg}"
    );
}

#[test]
fn presence_maps_to_the_expected_polarity() {
    assert!(Presence::Appears.wants_present());
    assert!(!Presence::Disappears.wants_present());
}

#[test]
fn window_match_prefers_the_frame_the_ax_tree_reported() {
    let windows = vec![
        win(1, 500, 0.0, 0.0, 400.0, 300.0),
        win(2, 500, 100.0, 100.0, 800.0, 600.0),
    ];
    let got = best_window_match(&windows, 500, Some(rect(102.0, 99.0, 800.0, 600.0)));
    assert_eq!(
        got.unwrap().id,
        2,
        "a few points of drift must not flip the match"
    );
}

#[test]
fn window_match_ignores_other_processes() {
    let windows = vec![win(1, 999, 0.0, 0.0, 800.0, 600.0)];
    assert!(
        best_window_match(&windows, 500, Some(rect(0.0, 0.0, 800.0, 600.0))).is_none(),
        "an identical frame in another app is never the right window"
    );
}

#[test]
fn identical_frames_prefer_the_window_that_is_on_screen() {
    let mut hidden_tab = win(1, 500, 0.0, 0.0, 800.0, 600.0);
    hidden_tab.on_screen = false;
    let visible_tab = win(2, 500, 0.0, 0.0, 800.0, 600.0);
    let windows = vec![hidden_tab, visible_tab];
    assert_eq!(
        best_window_match(&windows, 500, Some(rect(0.0, 0.0, 800.0, 600.0)))
            .unwrap()
            .id,
        2
    );
}

#[test]
fn without_an_ax_frame_the_largest_window_wins() {
    let windows = vec![
        win(1, 7, 0.0, 0.0, 100.0, 100.0),
        win(2, 7, 0.0, 0.0, 1200.0, 800.0),
    ];
    assert_eq!(best_window_match(&windows, 7, None).unwrap().id, 2);
}

#[test]
fn overlay_windows_are_never_matched() {
    let mut overlay = win(1, 7, 0.0, 0.0, 800.0, 600.0);
    overlay.layer = 25;
    assert!(best_window_match(&[overlay], 7, None).is_none());
}

// ── pulling an aim point back into the visible viewport ──────────────────

#[test]
fn a_point_already_inside_the_window_is_left_alone() {
    let window = win(1, 7, 0.0, 100.0, 1000.0, 800.0);
    let el = Some(rect(0.0, 100.0, 1000.0, 9000.0));
    assert_eq!(clamp_into_window(el, &window, 500.0, 400.0), (500.0, 400.0));
}

#[test]
fn a_tall_containers_centre_is_pulled_into_the_viewport() {
    // The measured shape: a web area whose frame is the whole document, so
    // its centre is far below the window showing it.
    let window = win(1, 7, 0.0, 100.0, 1000.0, 800.0);
    let document = rect(0.0, 100.0, 1000.0, 9000.0);
    // The element centre would be y = 100 + 4500 = 4600, off-screen.
    let (x, y) = clamp_into_window(Some(document), &window, 500.0, 4600.0);
    assert_eq!(x, 500.0, "horizontal overlap is the full width");
    assert_eq!(y, 500.0, "vertical centre of the visible 100..900 band");
    assert!(frame_contains(&window.frame, x, y));
}

#[test]
fn an_element_with_no_frame_keeps_the_point_it_was_given() {
    // Nothing better to compute from, so the caller gets the honest
    // out-of-window refusal downstream rather than an invented coordinate.
    let window = win(1, 7, 0.0, 100.0, 1000.0, 800.0);
    assert_eq!(clamp_into_window(None, &window, 5.0, 5000.0), (5.0, 5000.0));
}

#[test]
fn an_element_that_does_not_overlap_the_window_keeps_the_point() {
    // Element and window disjoint: there is no visible region to aim at, so
    // the point is left alone and the caller is refused with the real reason
    // rather than silently redirected.
    let window = win(1, 7, 0.0, 100.0, 1000.0, 800.0);
    let elsewhere = Some(rect(2000.0, 2000.0, 100.0, 100.0));
    assert_eq!(
        clamp_into_window(elsewhere, &window, 2050.0, 2050.0),
        (2050.0, 2050.0)
    );
}

#[test]
fn a_partly_offscreen_element_is_aimed_at_the_part_that_shows() {
    // A list scrolled so its top is above the window: the visible band is
    // 100..600, so the aim is its centre rather than the element's.
    let window = win(1, 7, 0.0, 100.0, 1000.0, 800.0);
    let list = Some(rect(200.0, -400.0, 400.0, 1000.0));
    let (x, y) = clamp_into_window(list, &window, 400.0, 100.0 - 300.0);
    assert_eq!((x, y), (400.0, 350.0));
    assert!(frame_contains(&window.frame, x, y));
}

/// The measured KakaoTalk arrangement: a chat window with its hamburger
/// menu open, plus the app's main window and cua-rs's own overlay.
fn kakao_windows() -> Vec<WindowInfo> {
    let chat = win(43899, 34667, 46.0, 86.0, 924.0, 770.0);
    let main = win(42510, 34667, 273.0, 33.0, 599.0, 771.0);
    let menu = WindowInfo {
        layer: 101,
        ..win(44501, 34667, 938.0, 599.0, 202.0, 318.0)
    };
    let overlay = WindowInfo {
        layer: 25,
        ..win(50000, 34667, 0.0, 0.0, 400.0, 400.0)
    };
    let other_app_menu = WindowInfo {
        layer: 101,
        ..win(60000, 999, 10.0, 10.0, 300.0, 300.0)
    };
    vec![chat, main, menu, overlay, other_app_menu]
}

#[test]
fn an_open_menu_is_reported_and_the_ordinary_windows_are_not() {
    let popups = transient_popups(&kakao_windows(), 34667, None);
    assert_eq!(popups.len(), 1, "got {popups:?}");
    assert_eq!(popups[0].id, 44501);
    assert_eq!(popups[0].layer, 101);
    assert_eq!(popups[0].frame.size.width, 202.0);
    assert_eq!(
        popups[0].appeared, None,
        "with nothing to compare against, whether it just opened is unknown, \
             not false"
    );
}

#[test]
fn the_menu_does_not_become_the_window_the_snapshot_is_of() {
    // The whole reason the widened rule is a second predicate. The chat
    // window's AX frame must still pick the chat window with a menu open.
    let matched = best_window_match(
        &kakao_windows(),
        34667,
        Some(rect(46.0, 86.0, 924.0, 770.0)),
    )
    .expect("a window");
    assert_eq!(matched.id, 43899);
}

#[test]
fn a_menu_opened_by_the_action_is_marked_as_appeared() {
    let before = [42510_u32];
    let popups = transient_popups(&kakao_windows(), 34667, Some(&before));
    assert_eq!(popups[0].appeared, Some(true));

    let before = [44501_u32];
    let popups = transient_popups(&kakao_windows(), 34667, Some(&before));
    assert_eq!(
        popups[0].appeared,
        Some(false),
        "a menu that was already up was not opened by this action"
    );
}

#[test]
fn stacked_popups_are_reported_topmost_first() {
    let mut windows = kakao_windows();
    // A submenu: same level, opened later, therefore in front.
    windows.push(WindowInfo {
        layer: 101,
        ..win(44900, 34667, 1100.0, 640.0, 180.0, 200.0)
    });
    // And a higher-level sheet above both.
    windows.push(WindowInfo {
        layer: 200,
        ..win(44100, 34667, 400.0, 400.0, 300.0, 300.0)
    });
    let ids: Vec<u32> = transient_popups(&windows, 34667, None)
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        ids,
        vec![44100, 44900, 44501],
        "level first, then window number newest-first: the head of the list is \
             the one a coordinate will reach"
    );
}

#[test]
fn a_popup_may_be_stamped_on_an_event_but_the_overlay_and_desktop_may_not() {
    let windows = kakao_windows();
    assert_eq!(
        live_window_for_pid_click(&windows, 44501, 34667)
            .expect("the menu is addressable")
            .id,
        44501
    );
    assert!(
        live_window_for_pid_click(&windows, 50000, 34667).is_err(),
        "cua-rs must never route a click into its own overlay"
    );
    assert!(
        live_window_for_pid_click(&windows, 44501, 999).is_err(),
        "another process's menu is not this app's to click"
    );

    let desktop = WindowInfo {
        layer: -2147483623,
        ..win(70000, 34667, 0.0, 0.0, 1512.0, 982.0)
    };
    assert!(live_window_for_pid_click(&[desktop], 70000, 34667).is_err());
}
