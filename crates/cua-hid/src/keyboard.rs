use super::*;

/// The modifier flag a token names, or `None` if the token is not a modifier.
///
/// The single source of the modifier vocabulary. [`parse_chord`] and
/// [`parse_modifiers`] both go through it, so `cmd+shift` means the same thing
/// in `press_key` as it does on a click and neither can grow an alias the other
/// does not have.
fn modifier_flag(token: &str) -> Option<CGEventFlags> {
    match token {
        "cmd" | "command" | "meta" | "super" => Some(CGEventFlags::MaskCommand),
        "shift" => Some(CGEventFlags::MaskShift),
        "alt" | "opt" | "option" => Some(CGEventFlags::MaskAlternate),
        "ctrl" | "control" => Some(CGEventFlags::MaskControl),
        "fn" | "function" => Some(CGEventFlags::MaskSecondaryFn),
        _ => None,
    }
}

/// Split a chord-shaped string into its tokens.
///
/// `-` is only a separator when there is no `+`, for the reason [`parse_chord`]
/// explains: `-` is also a key name.
fn chord_tokens(s: &str) -> impl Iterator<Item = &str> {
    let separators: &[char] = if s.contains('+') { &['+'] } else { &['+', '-'] };
    s.split(separators).map(str::trim).filter(|t| !t.is_empty())
}

/// Parse a modifier list like `cmd`, `cmd+shift`, `alt-shift` — the same
/// vocabulary and the same separators [`parse_chord`] accepts, minus the key.
///
/// An empty or whitespace-only string is no modifiers at all, which is what a
/// caller that always forwards an optional field wants. Anything that is not a
/// modifier is an error rather than being ignored: a caller who wrote
/// `cmd+click` meant something, and silently dropping `click` would deliver a
/// plain ⌘-click that looks like it worked.
///
/// Pure and unit-tested; posts nothing.
pub fn parse_modifiers(modifiers: &str) -> Result<CGEventFlags> {
    let mut flags = CGEventFlags::empty();
    for raw in chord_tokens(modifiers) {
        match modifier_flag(&raw.to_lowercase()) {
            Some(flag) => flags |= flag,
            None => {
                return Err(HidError::UnknownModifier {
                    modifiers: modifiers.to_string(),
                    token: raw.to_string(),
                })
            }
        }
    }
    Ok(flags)
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
    let mut literal: Option<char> = None;

    for raw in chord_tokens(chord) {
        let token = raw.to_lowercase();
        if let Some(flag) = modifier_flag(&token) {
            flags |= flag;
            continue;
        }
        match table.get(token.as_str()) {
            Some(&code) => {
                key = Some(code);
                // Remembered only for a single-character token: `"x"` names a
                // character, `"escape"` and `"f5"` name a key that has none.
                let mut chars = token.chars();
                literal = match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(c),
                    _ => None,
                };
            }
            None => {
                return Err(HidError::UnknownToken {
                    chord: chord.to_string(),
                    token: raw.to_string(),
                })
            }
        }
    }

    match key {
        Some(key) => Ok(Chord {
            key,
            flags,
            // A modifier changes what the keystroke *means*, so the literal is
            // dropped: `cmd+x` is Cut, not the letter x, and forcing a character
            // onto it would be a different event.
            literal: literal.filter(|_| !flags.intersects(modifier_mask())),
        }),
        None => Err(HidError::NoKey(chord.to_string())),
    }
}

/// Every flag [`modifier_flag`] can produce, as one mask.
///
/// Used to ask "did the caller name a modifier at all", which is a different
/// question from "is this flag set" — a synthesized event can carry incidental
/// bits, and only the ones a caller asked for should change the recipe.
fn modifier_mask() -> CGEventFlags {
    CGEventFlags::MaskCommand
        | CGEventFlags::MaskShift
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskControl
        | CGEventFlags::MaskSecondaryFn
}

/// Virtual key codes, keyed by the names a model is likely to produce.
///
/// These are the ANSI positions from `Carbon/Events.h`. They are *positional*:
/// `kVK_ANSI_A` is 0 regardless of the user's layout, so this table is correct
/// on a Dvorak or Korean keyboard as long as the caller means "the key where A
/// is on a US layout" — which is what a shortcut like `⌘A` actually means.
pub(super) fn key_table() -> HashMap<&'static str, u16> {
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
