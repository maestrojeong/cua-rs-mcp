//! `press_key` chords as X keysyms.
//!
//! Same vocabulary and separators as `cua_hid::parse_chord` — modifier names,
//! key names, `+`/`-` splitting — so a chord means the same thing on both
//! platforms *as a set of keys*. It does not translate meaning: `cmd` is the
//! Super key here, because that is the key named, and Linux applications put
//! their shortcuts on Ctrl. An agent that wants Copy on Linux sends `ctrl+c`;
//! `menu_bar` reports each row's shortcut already spelled that way.
//!
//! Pure and compiled on every target, so its tests also run in the macOS CI
//! job.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use cua_core::platform::portable::chord_tokens;

pub(crate) const SHIFT_L: u32 = 0xffe1;
pub(crate) const CONTROL_L: u32 = 0xffe3;
pub(crate) const ALT_L: u32 = 0xffe9;
pub(crate) const SUPER_L: u32 = 0xffeb;

/// A parsed chord: modifier keysyms to hold, then the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XChord {
    pub(crate) modifiers: Vec<u32>,
    pub(crate) key: u32,
}

fn modifier_keysym(token: &str) -> Option<Result<u32, String>> {
    match token {
        "cmd" | "command" | "meta" | "super" => Some(Ok(SUPER_L)),
        "shift" => Some(Ok(SHIFT_L)),
        "alt" | "opt" | "option" => Some(Ok(ALT_L)),
        "ctrl" | "control" => Some(Ok(CONTROL_L)),
        "fn" | "function" => Some(Err(
            "`fn` is handled inside the keyboard's firmware and has no X11 keysym".to_string(),
        )),
        _ => None,
    }
}

/// The keysym for one named key, from the same table of names `cua-hid` uses.
pub(crate) fn key_keysym(token: &str) -> Option<u32> {
    let named = match token {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "space" => 0x20,
        "delete" | "backspace" => 0xff08,
        "escape" | "esc" => 0xff1b,
        "forwarddelete" => 0xffff,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "help" => 0xff6a,
        "clear" => 0xff0b,
        "numpadenter" => 0xff8d,
        "minus" | "hyphen" => 0x2d,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    if let Some(n) = token.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=20).contains(&n) {
            return Some(0xffbe + n - 1);
        }
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if "abcdefghijklmnopqrstuvwxyz0123456789=-][';\\,/.`".contains(c) => {
            Some(c as u32)
        }
        _ => None,
    }
}

/// The keysym that types `c`: Latin-1 directly, everything else through the
/// Unicode keysym range X11 reserves for it.
pub(crate) fn char_keysym(c: char) -> u32 {
    match c {
        '\n' => 0xff0d,
        '\t' => 0xff09,
        c if (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

/// Parse a chord like `ctrl+shift+p`, `escape`, `f5`.
pub(crate) fn parse_chord(chord: &str) -> Result<XChord, String> {
    let mut modifiers = Vec::new();
    let mut key = None;
    for raw in chord_tokens(chord) {
        let token = raw.to_lowercase();
        if let Some(m) = modifier_keysym(&token) {
            let m = m?;
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
            continue;
        }
        match key_keysym(&token) {
            Some(k) => key = Some(k),
            None => {
                return Err(format!(
                    "unknown key or modifier `{raw}` in {chord:?}. Modifiers: cmd (Super), shift, alt, ctrl. Keys: a-z, 0-9, f1-f20, return, tab, space, escape, delete, arrows, home, end, pageup, pagedown"
                ))
            }
        }
    }
    key.map(|key| XChord { modifiers, key })
        .ok_or_else(|| format!("chord {chord:?} has modifiers but no key"))
}

/// GTK/Qt's accelerator spelling (`<Control><Shift>s`, `<Primary>q`) in
/// `press_key`'s vocabulary (`ctrl+shift+s`), for `menu_bar`. `None` for an
/// empty or unparseable binding: reporting no shortcut beats reporting a wrong
/// one.
pub(crate) fn accelerator_to_chord(accel: &str) -> Option<String> {
    // AT-SPI's key binding string is `mnemonic;sequence;accelerator`; GTK3
    // fills the last field. A bare accelerator has no semicolons.
    let accel = accel.rsplit(';').next().unwrap_or(accel).trim();
    if accel.is_empty() {
        return None;
    }
    let mut mods: Vec<&str> = Vec::new();
    let mut rest = accel;
    while let Some(stripped) = rest.strip_prefix('<') {
        let (name, after) = stripped.split_once('>')?;
        let m = match name.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "ctrl",
            "shift" => "shift",
            "alt" | "mod1" => "alt",
            "super" | "meta" | "mod4" => "cmd",
            _ => return None,
        };
        if !mods.contains(&m) {
            mods.push(m);
        }
        rest = after;
    }
    let key = rest.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    let key = match key.as_str() {
        "escape" => "escape".to_string(),
        "delete" => "forwarddelete".to_string(),
        "backspace" => "delete".to_string(),
        "page_up" => "pageup".to_string(),
        "page_down" => "pagedown".to_string(),
        k => k.to_string(),
    };
    key_keysym(&key)?;
    let mut out: Vec<&str> = ["ctrl", "alt", "shift", "cmd"]
        .into_iter()
        .filter(|m| mods.contains(m))
        .collect();
    out.push(&key);
    Some(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_to_modifiers_and_one_key() {
        let c = parse_chord("Ctrl+Shift+P").unwrap();
        assert_eq!(c.modifiers, vec![CONTROL_L, SHIFT_L]);
        assert_eq!(c.key, 'p' as u32);
        assert_eq!(parse_chord("escape").unwrap().key, 0xff1b);
        assert_eq!(parse_chord("f5").unwrap().key, 0xffc2);
        assert_eq!(parse_chord("cmd-c").unwrap().modifiers, vec![SUPER_L]);
    }

    #[test]
    fn a_minus_key_survives_plus_separated_chords() {
        assert_eq!(parse_chord("ctrl+-").unwrap().key, '-' as u32);
    }

    #[test]
    fn unknown_tokens_and_fn_are_errors_not_guesses() {
        assert!(parse_chord("ctrl+shft+p").is_err());
        assert!(parse_chord("fn+f1").is_err());
        assert!(parse_chord("ctrl+shift").is_err());
    }

    #[test]
    fn delete_means_backspace_as_it_does_on_macos() {
        assert_eq!(parse_chord("delete").unwrap().key, 0xff08);
        assert_eq!(parse_chord("forwarddelete").unwrap().key, 0xffff);
    }

    #[test]
    fn gtk_accelerators_become_press_key_chords() {
        assert_eq!(
            accelerator_to_chord("<Control><Shift>s").as_deref(),
            Some("ctrl+shift+s")
        );
        assert_eq!(
            accelerator_to_chord("<Primary>q").as_deref(),
            Some("ctrl+q")
        );
        assert_eq!(
            accelerator_to_chord("_File;;<Control>o").as_deref(),
            Some("ctrl+o")
        );
        assert_eq!(accelerator_to_chord("").as_deref(), None);
        assert_eq!(accelerator_to_chord("<Hyper>x").as_deref(), None);
    }

    #[test]
    fn characters_map_to_latin1_or_unicode_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('한'), 0x0100_0000 + '한' as u32);
        assert_eq!(char_keysym('\n'), 0xff0d);
    }
}
