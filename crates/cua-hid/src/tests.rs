use super::*;

#[test]
fn a_scroll_recipe_name_round_trips_and_a_typo_is_the_shipped_one() {
    for recipe in [
        ScrollRecipe::Plain,
        ScrollRecipe::NsEventRoundTrip,
        ScrollRecipe::Phased,
        ScrollRecipe::PhasedGesture,
    ] {
        assert_eq!(
            ScrollRecipe::parse(recipe.as_str()),
            Some(recipe),
            "{} must survive being printed and read back, or the probe's own \
                 output stops naming the arm it ran",
            recipe.as_str()
        );
    }
    // Case and stray whitespace come from a shell variable, not from code.
    assert_eq!(
        ScrollRecipe::parse(" NsEvent "),
        Some(ScrollRecipe::NsEventRoundTrip)
    );
    // An unset variable is the shipped recipe, and so is an empty one.
    assert_eq!(ScrollRecipe::parse(""), Some(ScrollRecipe::Plain));
    assert_eq!(ScrollRecipe::default(), ScrollRecipe::Plain);
    // A misspelling has to be distinguishable from a real arm here, so that
    // `from_env` can fall back to the shipped recipe rather than silently
    // running a different experiment than the one that was asked for.
    assert_eq!(ScrollRecipe::parse("phased-gestrue"), None);
}

#[test]
fn a_modifier_list_shares_the_chord_vocabulary() {
    // The point of routing both parsers through one table: whatever spells
    // a modifier in `press_key` must spell it on a click too.
    for alias in ["cmd", "command", "meta", "super"] {
        assert_eq!(
            parse_modifiers(alias).unwrap(),
            CGEventFlags::MaskCommand,
            "{alias} must mean command on a click as well as in a chord"
        );
    }
    assert_eq!(
        parse_modifiers("cmd+shift").unwrap(),
        parse_chord("cmd+shift+a").unwrap().flags,
        "a modifier list and the modifier half of a chord must agree"
    );
    // Same separator rules, so a model that writes dashes in one place and
    // pluses in the other gets the same answer.
    assert_eq!(
        parse_modifiers("alt-ctrl").unwrap(),
        parse_modifiers("ctrl+alt").unwrap()
    );
}

#[test]
fn an_empty_modifier_list_is_no_modifiers_not_an_error() {
    // A caller forwarding an optional field should not have to special-case
    // the absent case.
    assert!(parse_modifiers("").unwrap().is_empty());
    assert!(parse_modifiers("   ").unwrap().is_empty());
}

#[test]
fn a_key_name_in_a_modifier_list_is_refused_not_ignored() {
    // `cmd+click` is a thing a model will write. Dropping the `click`
    // silently would deliver a plain command-click that looks correct.
    let err = parse_modifiers("cmd+click").unwrap_err();
    match err {
        HidError::UnknownModifier { token, .. } => assert_eq!(token, "click"),
        other => panic!("expected UnknownModifier, got {other:?}"),
    }
    // Even a token that IS a valid key: a modifier list has no key in it.
    assert!(matches!(
        parse_modifiers("shift+p").unwrap_err(),
        HidError::UnknownModifier { .. }
    ));
}

#[test]
fn buttons_parse_by_name_and_default_to_left() {
    assert_eq!(MouseButton::parse("").unwrap(), MouseButton::Left);
    assert_eq!(MouseButton::parse("  Right ").unwrap(), MouseButton::Right);
    assert_eq!(MouseButton::parse("MIDDLE").unwrap(), MouseButton::Middle);
    assert!(matches!(
        MouseButton::parse("mouse2").unwrap_err(),
        HidError::UnknownButton(_)
    ));
}

#[test]
fn each_button_gets_its_own_event_type_family() {
    // A view implementing `rightMouseDown:` never sees a `leftMouseDown`,
    // whatever button number is stamped on it, so the type is what selects
    // the handler and the three families must not be mixed up.
    let (down, dragged, up) = MouseButton::Right.types();
    assert_eq!(down, NSEventType::RightMouseDown);
    assert_eq!(dragged, NSEventType::RightMouseDragged);
    assert_eq!(up, NSEventType::RightMouseUp);

    let (down, dragged, up) = MouseButton::Middle.types();
    assert_eq!(down, NSEventType::OtherMouseDown);
    assert_eq!(dragged, NSEventType::OtherMouseDragged);
    assert_eq!(up, NSEventType::OtherMouseUp);

    assert_eq!(MouseButton::Left.number(), 0);
    assert_eq!(MouseButton::Right.number(), 1);
    assert_eq!(MouseButton::Middle.number(), 2);
}

#[test]
fn a_drag_path_ends_exactly_where_it_was_aimed() {
    // Not "within a rounding error": a drop a fraction of a point short is
    // a drop into the neighbouring row.
    let path = drag_path((100.0, 100.0), (300.0, 250.0));
    assert_eq!(*path.last().unwrap(), (300.0, 250.0));
    assert!(
        !path.contains(&(100.0, 100.0)),
        "the origin belongs to the mouse-down, not to the move run"
    );
}

#[test]
fn a_drag_path_is_monotone_along_both_axes() {
    let path = drag_path((0.0, 0.0), (100.0, -50.0));
    for pair in path.windows(2) {
        assert!(pair[1].0 > pair[0].0, "x must advance: {path:?}");
        assert!(pair[1].1 < pair[0].1, "y must advance: {path:?}");
    }
}

#[test]
fn step_count_holds_the_step_length_constant_between_its_bounds() {
    // 24 points per step in the middle of the range...
    assert_eq!(drag_step_count((0.0, 0.0), (0.0, 240.0)), 10);
    // ...the floor for anything short, so a 5-point drag is still a run of
    // moves and not one jump...
    assert_eq!(drag_step_count((0.0, 0.0), (5.0, 0.0)), DRAG_MIN_STEPS);
    assert_eq!(drag_step_count((7.0, 7.0), (7.0, 7.0)), DRAG_MIN_STEPS);
    // ...and the ceiling for anything long, so one gesture cannot run for
    // seconds.
    assert_eq!(drag_step_count((0.0, 0.0), (4000.0, 0.0)), DRAG_MAX_STEPS);
}

#[test]
fn a_zero_length_drag_still_produces_a_usable_path() {
    // Origin == destination is a caller error the tiers above catch, but it
    // must not produce an empty path here: an empty path would mean a
    // mouse-down with no moves, which is the shape a stuck drag has.
    let path = drag_path((10.0, 10.0), (10.0, 10.0));
    assert_eq!(path.len(), DRAG_MIN_STEPS);
    assert!(path.iter().all(|p| *p == (10.0, 10.0)));
}

#[test]
fn a_non_finite_endpoint_does_not_blow_up_the_step_count() {
    // `f64 as usize` saturates rather than wrapping, but NaN converts to 0,
    // and 0 steps is a drag with no moves in it.
    assert_eq!(
        drag_step_count((0.0, 0.0), (f64::NAN, f64::NAN)),
        DRAG_MIN_STEPS
    );
    assert_eq!(
        drag_step_count((0.0, 0.0), (f64::INFINITY, 0.0)),
        DRAG_MAX_STEPS
    );
}

#[test]
fn parses_a_plain_key() {
    let c = parse_chord("escape").unwrap();
    assert_eq!(c.key, 53);
    assert!(c.flags.is_empty(), "a bare key must carry no modifiers");
}

#[test]
fn parses_modifiers_in_any_order_and_any_case() {
    let a = parse_chord("cmd+shift+p").unwrap();
    let b = parse_chord("Shift+Command+P").unwrap();
    let c = parse_chord("SHIFT-CMD-p").unwrap();
    assert_eq!(a, b, "order must not matter");
    assert_eq!(a, c, "case and separator must not matter");
    assert_eq!(a.key, 35);
    assert!(a.flags.contains(CGEventFlags::MaskCommand));
    assert!(a.flags.contains(CGEventFlags::MaskShift));
}

#[test]
fn accepts_every_alias_for_the_same_modifier() {
    for alias in ["alt", "opt", "option"] {
        let c = parse_chord(&format!("{alias}+a")).unwrap();
        assert!(
            c.flags.contains(CGEventFlags::MaskAlternate),
            "{alias} failed"
        );
    }
    for alias in ["cmd", "command", "meta", "super"] {
        let c = parse_chord(&format!("{alias}+a")).unwrap();
        assert!(
            c.flags.contains(CGEventFlags::MaskCommand),
            "{alias} failed"
        );
    }
}

#[test]
fn function_keys_are_not_off_by_one() {
    // F1-F4 are non-contiguous and descending in Carbon; a naive
    // `112 + n` table gets every one of them wrong.
    assert_eq!(parse_chord("f1").unwrap().key, 122);
    assert_eq!(parse_chord("f2").unwrap().key, 120);
    assert_eq!(parse_chord("f3").unwrap().key, 99);
    assert_eq!(parse_chord("f5").unwrap().key, 96);
    assert_eq!(parse_chord("f12").unwrap().key, 111);
}

#[test]
fn digit_five_and_six_are_not_swapped() {
    // kVK_ANSI_5 is 23 and kVK_ANSI_6 is 22 -- the one pair that is out of
    // order, and the easiest thing in this table to get wrong.
    assert_eq!(parse_chord("5").unwrap().key, 23);
    assert_eq!(parse_chord("6").unwrap().key, 22);
}

#[test]
fn aliases_resolve_to_the_same_code() {
    assert_eq!(
        parse_chord("return").unwrap().key,
        parse_chord("enter").unwrap().key
    );
    assert_eq!(
        parse_chord("delete").unwrap().key,
        parse_chord("backspace").unwrap().key
    );
    assert_eq!(
        parse_chord("escape").unwrap().key,
        parse_chord("esc").unwrap().key
    );
}

#[test]
fn a_typo_names_the_offending_token_not_the_whole_chord() {
    let err = parse_chord("cmd+shft+p").unwrap_err();
    match err {
        HidError::UnknownToken { token, .. } => assert_eq!(token, "shft"),
        other => panic!("expected UnknownToken, got {other:?}"),
    }
}

#[test]
fn modifiers_without_a_key_are_rejected() {
    assert!(matches!(
        parse_chord("cmd+shift").unwrap_err(),
        HidError::NoKey(_)
    ));
    assert!(matches!(parse_chord("").unwrap_err(), HidError::NoKey(_)));
}

#[test]
fn separators_and_whitespace_are_tolerated() {
    let c = parse_chord(" cmd + shift + p ").unwrap();
    assert_eq!(c.key, 35);
    assert!(c.flags.contains(CGEventFlags::MaskCommand));
}

#[test]
fn the_minus_key_survives_being_the_separator_character() {
    // "-" is both a separator and a key name. When a "+" is present it is
    // the separator, so "-" can be the key.
    let c = parse_chord("cmd+-").unwrap();
    assert_eq!(c.key, 27, "cmd+- must reach the minus key");
    assert!(c.flags.contains(CGEventFlags::MaskCommand));

    // The spelled-out alias always works, whichever separator is in use.
    assert_eq!(parse_chord("cmd-minus").unwrap().key, 27);
    assert_eq!(parse_chord("cmd+minus").unwrap().key, 27);
}

#[test]
fn dash_separated_chords_still_work() {
    let dashed = parse_chord("cmd-shift-p").unwrap();
    let plussed = parse_chord("cmd+shift+p").unwrap();
    assert_eq!(dashed, plussed);
}

// ── the literal character, and when it is not one ────────────────────────

#[test]
fn a_bare_character_key_remembers_its_character() {
    // The whole point: under a Korean source the keycode alone arrives as a
    // different letter, so the character has to travel with the event.
    assert_eq!(parse_chord("x").unwrap().literal, Some('x'));
    assert_eq!(parse_chord("X").unwrap().literal, Some('x'));
    assert_eq!(parse_chord("7").unwrap().literal, Some('7'));
    // A bare `-` is a separator, not a key — pre-existing, and why the
    // spelled name exists. Its keycode form still carries no literal,
    // because `minus` names a key rather than a character.
    assert!(parse_chord("-").is_err());
    assert_eq!(parse_chord("cmd+-").unwrap().literal, None);
}

#[test]
fn a_named_key_has_no_character_to_force() {
    // `escape` and `f5` produce no character at all; claiming one would put
    // a literal "e" on the event.
    assert_eq!(parse_chord("escape").unwrap().literal, None);
    assert_eq!(parse_chord("return").unwrap().literal, None);
    assert_eq!(parse_chord("f5").unwrap().literal, None);
    assert_eq!(parse_chord("tab").unwrap().literal, None);
    assert_eq!(parse_chord("minus").unwrap().literal, None);
}

#[test]
fn a_modifier_drops_the_character() {
    // `cmd+x` is Cut, not the letter x. Forcing a character onto a chord
    // would change what the keystroke means.
    assert_eq!(parse_chord("cmd+x").unwrap().literal, None);
    assert_eq!(parse_chord("shift+a").unwrap().literal, None);
    assert_eq!(parse_chord("ctrl+alt+delete").unwrap().literal, None);
    // ...but the keycode is still the one the letter names.
    assert_eq!(
        parse_chord("cmd+x").unwrap().key,
        parse_chord("x").unwrap().key
    );
}
