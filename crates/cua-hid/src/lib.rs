//! Opt-in HID event synthesis.
//!
//! # Read this before using this crate
//!
//! This is the **only** crate in the workspace that can move the cursor or take
//! keyboard focus. Everything else drives macOS through the Accessibility API,
//! which addresses a UI element directly and therefore coexists with the human
//! at the keyboard. This crate does the opposite: it writes into the session's
//! single, shared HID event stream.
//!
//! It exists because the Accessibility API has no general keyboard verb. There
//! is `AXConfirm` for Return and `AXCancel` for Escape, and after that nothing —
//! no way to express `⌘⇧P`, no way to drive a terminal, no way to reach a canvas
//! app that only listens for real key events. Refusing to implement that at all
//! (which is effectively what OpenAI's implementation does) leaves a large hole;
//! implementing it silently would destroy the property that makes the rest of
//! this project worth using.
//!
//! So it is isolated here, and the isolation is enforced by the dependency
//! graph rather than by a comment: `cua-ax` and `cua-capture` do not depend on
//! this crate and cannot reach it. `grep -rl cua_hid crates/` enumerates every
//! call site that can touch the user's pointer.
//!
//! Reachable only when the server was started with `--allow-hid`, and every
//! result that came through here is tagged `delivery: hid` so an agent can never
//! mistake it for a background action.

use std::collections::HashMap;

use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum HidError {
    /// The chord did not parse. Carries the offending token rather than the
    /// whole string, because a model that wrote `cmd+shft+p` needs to see which
    /// word was wrong.
    #[error("unknown key or modifier `{token}` in {chord:?}. Modifiers: cmd, shift, alt/option, ctrl, fn. Keys: a-z, 0-9, f1-f20, return, tab, space, escape, delete, arrows, home, end, pageup, pagedown")]
    UnknownToken { chord: String, token: String },

    #[error("chord {0:?} has modifiers but no key")]
    NoKey(String),

    #[error("could not create a HID event source; the Accessibility grant may have been revoked")]
    NoSource,
}

pub type Result<T> = std::result::Result<T, HidError>;

/// A parsed key chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: u16,
    pub flags: CGEventFlags,
}

/// Parse a chord like `cmd+shift+p`, `escape`, `f5`, `ctrl+alt+delete`.
///
/// Accepts `+` or `-` as the separator and is case-insensitive, because models
/// produce all of `Cmd+Shift+P`, `cmd-shift-p` and `COMMAND+SHIFT+P`. The last
/// non-modifier token is the key; order does not matter otherwise.
///
/// `-` is only treated as a separator when the chord contains no `+`, because
/// `-` is also a key name: splitting `cmd+-` on both characters would throw the
/// key away and report "no key". Write `cmd+-` (or `cmd+minus`) for that one.
///
/// Pure and unit-tested: no events are posted, nothing is touched. This is the
/// half of the crate that can be verified without a display.
pub fn parse_chord(chord: &str) -> Result<Chord> {
    let table = key_table();
    let mut flags = CGEventFlags::empty();
    let mut key: Option<u16> = None;

    let separators: &[char] = if chord.contains('+') {
        &['+']
    } else {
        &['+', '-']
    };

    for raw in chord
        .split(separators)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let token = raw.to_lowercase();
        match token.as_str() {
            "cmd" | "command" | "meta" | "super" => flags |= CGEventFlags::MaskCommand,
            "shift" => flags |= CGEventFlags::MaskShift,
            "alt" | "opt" | "option" => flags |= CGEventFlags::MaskAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::MaskControl,
            "fn" | "function" => flags |= CGEventFlags::MaskSecondaryFn,
            other => match table.get(other) {
                Some(&code) => key = Some(code),
                None => {
                    return Err(HidError::UnknownToken {
                        chord: chord.to_string(),
                        token: raw.to_string(),
                    })
                }
            },
        }
    }

    match key {
        Some(key) => Ok(Chord { key, flags }),
        None => Err(HidError::NoKey(chord.to_string())),
    }
}

/// Post a chord as a real key press to whatever currently has focus.
///
/// This is global: it goes to the focused app, not to a chosen one. There is no
/// `app` parameter on purpose — pretending to target an app while writing to the
/// shared HID stream would be a lie, and the honest contract is "this behaves
/// exactly as if the user pressed the keys".
pub fn post_chord(chord: Chord) -> Result<()> {
    // `CombinedSessionState` rather than a private source so the window server
    // treats these as ordinary session input and modifier state composes with
    // whatever the user is physically holding.
    let source =
        CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok_or(HidError::NoSource)?;

    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(Some(&source), chord.key, down)
            .ok_or(HidError::NoSource)?;
        // Flags must be set on both the down and the up event. Leaving them off
        // the key-up leaves apps that track modifier transitions believing the
        // modifier is still held.
        CGEvent::set_flags(Some(&event), chord.flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

/// Virtual key codes, keyed by the names a model is likely to produce.
///
/// These are the ANSI positions from `Carbon/Events.h`. They are *positional*:
/// `kVK_ANSI_A` is 0 regardless of the user's layout, so this table is correct
/// on a Dvorak or Korean keyboard as long as the caller means "the key where A
/// is on a US layout" — which is what a shortcut like `⌘A` actually means.
fn key_table() -> HashMap<&'static str, u16> {
    let mut m = HashMap::new();
    let letters: [(&str, u16); 26] = [
        ("a", 0),
        ("s", 1),
        ("d", 2),
        ("f", 3),
        ("h", 4),
        ("g", 5),
        ("z", 6),
        ("x", 7),
        ("c", 8),
        ("v", 9),
        ("b", 11),
        ("q", 12),
        ("w", 13),
        ("e", 14),
        ("r", 15),
        ("y", 16),
        ("t", 17),
        ("o", 31),
        ("u", 32),
        ("i", 34),
        ("p", 35),
        ("l", 37),
        ("j", 38),
        ("k", 40),
        ("n", 45),
        ("m", 46),
    ];
    m.extend(letters);

    let digits: [(&str, u16); 10] = [
        ("1", 18),
        ("2", 19),
        ("3", 20),
        ("4", 21),
        ("5", 23),
        ("6", 22),
        ("7", 26),
        ("8", 28),
        ("9", 25),
        ("0", 29),
    ];
    m.extend(digits);

    let punct: [(&str, u16); 13] = [
        ("=", 24),
        ("-", 27),
        ("minus", 27),
        ("hyphen", 27),
        ("]", 30),
        ("[", 33),
        ("'", 39),
        (";", 41),
        ("\\", 42),
        (",", 43),
        ("/", 44),
        (".", 47),
        ("`", 50),
    ];
    m.extend(punct);

    let named: [(&str, u16); 20] = [
        ("return", 36),
        ("enter", 36),
        ("tab", 48),
        ("space", 49),
        ("delete", 51),
        ("backspace", 51),
        ("escape", 53),
        ("esc", 53),
        ("forwarddelete", 117),
        ("home", 115),
        ("end", 119),
        ("pageup", 116),
        ("pagedown", 121),
        ("left", 123),
        ("right", 124),
        ("down", 125),
        ("up", 126),
        ("help", 114),
        ("clear", 71),
        ("numpadenter", 76),
    ];
    m.extend(named);

    // F1-F20. Non-contiguous in Carbon, so spelled out.
    let fkeys: [(&str, u16); 20] = [
        ("f1", 122),
        ("f2", 120),
        ("f3", 99),
        ("f4", 118),
        ("f5", 96),
        ("f6", 97),
        ("f7", 98),
        ("f8", 100),
        ("f9", 101),
        ("f10", 109),
        ("f11", 103),
        ("f12", 111),
        ("f13", 105),
        ("f14", 107),
        ("f15", 113),
        ("f16", 106),
        ("f17", 64),
        ("f18", 79),
        ("f19", 80),
        ("f20", 90),
    ];
    m.extend(fkeys);

    m
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
