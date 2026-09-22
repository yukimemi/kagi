//! Platform-neutral key identity and modifier set.
//!
//! Backends translate between this enum and their native code space
//! (macOS virtual keycodes, Windows scan codes, Linux evdev codes).

use anyhow::{bail, Result};
use bitflags::bitflags;
use std::fmt;

bitflags! {
    /// Modifier state, collapsed to sides-agnostic bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Mods: u8 {
        const CTRL  = 1 << 0;
        const SHIFT = 1 << 1;
        /// Option on macOS, Alt elsewhere.
        const ALT   = 1 << 2;
        /// Command on macOS, Win/Super elsewhere.
        const META  = 1 << 3;
    }
}

impl fmt::Display for Mods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (bit, name) in [
            (Mods::CTRL, "ctrl"),
            (Mods::SHIFT, "shift"),
            (Mods::ALT, "alt"),
            (Mods::META, "meta"),
        ] {
            if self.contains(bit) {
                if !first {
                    f.write_str("-")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// A physical key, identified by position rather than produced character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::upper_case_acronyms)]
pub enum Key {
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z,

    Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,

    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    F13, F14, F15, F16, F17, F18, F19, F20,

    Grave, Minus, Equal, LeftBracket, RightBracket, Backslash,
    Semicolon, Quote, Comma, Period, Slash,

    Escape, Tab, CapsLock, Space, Backspace, Enter,
    Insert, Delete, Home, End, PageUp, PageDown,
    Left, Right, Up, Down,
    PrintScreen, ScrollLock, Pause, Menu, NumLock,

    LeftCtrl, RightCtrl, LeftShift, RightShift,
    LeftAlt, RightAlt, LeftMeta, RightMeta,

    /// JIS 変換
    Henkan,
    /// JIS 無変換
    Muhenkan,
    /// JIS カタカナ/ひらがな
    KatakanaHiragana,
    /// JIS 半角/全角
    Zenkaku,
    /// JIS `\` `_` (ろ)
    IntlRo,
    /// JIS `¥` `|`
    IntlYen,
    /// macOS 英数
    Eisu,
    /// macOS かな
    Kana,

    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4,
    Numpad5, Numpad6, Numpad7, Numpad8, Numpad9,
    NumpadEnter, NumpadPlus, NumpadMinus, NumpadMultiply,
    NumpadDivide, NumpadDot, NumpadEqual,
}

/// Canonical name first, then accepted aliases.
const TABLE: &[(Key, &[&str])] = &[
    (Key::A, &["a"]), (Key::B, &["b"]), (Key::C, &["c"]), (Key::D, &["d"]),
    (Key::E, &["e"]), (Key::F, &["f"]), (Key::G, &["g"]), (Key::H, &["h"]),
    (Key::I, &["i"]), (Key::J, &["j"]), (Key::K, &["k"]), (Key::L, &["l"]),
    (Key::M, &["m"]), (Key::N, &["n"]), (Key::O, &["o"]), (Key::P, &["p"]),
    (Key::Q, &["q"]), (Key::R, &["r"]), (Key::S, &["s"]), (Key::T, &["t"]),
    (Key::U, &["u"]), (Key::V, &["v"]), (Key::W, &["w"]), (Key::X, &["x"]),
    (Key::Y, &["y"]), (Key::Z, &["z"]),

    (Key::Num0, &["0"]), (Key::Num1, &["1"]), (Key::Num2, &["2"]),
    (Key::Num3, &["3"]), (Key::Num4, &["4"]), (Key::Num5, &["5"]),
    (Key::Num6, &["6"]), (Key::Num7, &["7"]), (Key::Num8, &["8"]),
    (Key::Num9, &["9"]),

    (Key::F1, &["f1"]), (Key::F2, &["f2"]), (Key::F3, &["f3"]),
    (Key::F4, &["f4"]), (Key::F5, &["f5"]), (Key::F6, &["f6"]),
    (Key::F7, &["f7"]), (Key::F8, &["f8"]), (Key::F9, &["f9"]),
    (Key::F10, &["f10"]), (Key::F11, &["f11"]), (Key::F12, &["f12"]),
    (Key::F13, &["f13"]), (Key::F14, &["f14"]), (Key::F15, &["f15"]),
    (Key::F16, &["f16"]), (Key::F17, &["f17"]), (Key::F18, &["f18"]),
    (Key::F19, &["f19"]), (Key::F20, &["f20"]),

    (Key::Grave, &["grave", "`"]),
    (Key::Minus, &["minus", "-"]),
    (Key::Equal, &["equal", "="]),
    (Key::LeftBracket, &["[", "leftbracket", "lbracket"]),
    (Key::RightBracket, &["]", "rightbracket", "rbracket"]),
    (Key::Backslash, &["backslash", "\\"]),
    (Key::Semicolon, &["semicolon", ";"]),
    (Key::Quote, &["quote", "'"]),
    (Key::Comma, &["comma", ","]),
    (Key::Period, &["period", "."]),
    (Key::Slash, &["slash", "/"]),

    (Key::Escape, &["esc", "escape"]),
    (Key::Tab, &["tab"]),
    (Key::CapsLock, &["capslock", "caps"]),
    (Key::Space, &["space", "spc"]),
    (Key::Backspace, &["backspace", "bs"]),
    (Key::Enter, &["enter", "return", "cr"]),
    (Key::Insert, &["insert", "ins"]),
    (Key::Delete, &["delete", "del"]),
    (Key::Home, &["home"]),
    (Key::End, &["end"]),
    (Key::PageUp, &["pageup", "pgup"]),
    (Key::PageDown, &["pagedown", "pgdn"]),
    (Key::Left, &["left"]),
    (Key::Right, &["right"]),
    (Key::Up, &["up"]),
    (Key::Down, &["down"]),
    (Key::PrintScreen, &["printscreen", "prtsc"]),
    (Key::ScrollLock, &["scrolllock"]),
    (Key::Pause, &["pause"]),
    (Key::Menu, &["menu", "apps"]),
    (Key::NumLock, &["numlock"]),

    (Key::LeftCtrl, &["lctrl"]), (Key::RightCtrl, &["rctrl"]),
    (Key::LeftShift, &["lshift"]), (Key::RightShift, &["rshift"]),
    (Key::LeftAlt, &["lalt", "lopt"]), (Key::RightAlt, &["ralt", "ropt"]),
    (Key::LeftMeta, &["lmeta", "lcmd", "lwin", "lsuper"]),
    (Key::RightMeta, &["rmeta", "rcmd", "rwin", "rsuper"]),

    (Key::Henkan, &["henkan", "convert"]),
    (Key::Muhenkan, &["muhenkan", "nonconvert"]),
    (Key::KatakanaHiragana, &["katakanahiragana", "kanamode"]),
    (Key::Zenkaku, &["zenkaku", "hankaku", "zenkakuhankaku"]),
    (Key::IntlRo, &["ro", "intlro"]),
    (Key::IntlYen, &["yen", "intlyen"]),
    (Key::Eisu, &["eisu", "eisuu"]),
    (Key::Kana, &["kana"]),

    (Key::Numpad0, &["kp0"]), (Key::Numpad1, &["kp1"]), (Key::Numpad2, &["kp2"]),
    (Key::Numpad3, &["kp3"]), (Key::Numpad4, &["kp4"]), (Key::Numpad5, &["kp5"]),
    (Key::Numpad6, &["kp6"]), (Key::Numpad7, &["kp7"]), (Key::Numpad8, &["kp8"]),
    (Key::Numpad9, &["kp9"]),
    (Key::NumpadEnter, &["kpenter"]), (Key::NumpadPlus, &["kpplus"]),
    (Key::NumpadMinus, &["kpminus"]), (Key::NumpadMultiply, &["kpmultiply"]),
    (Key::NumpadDivide, &["kpdivide"]), (Key::NumpadDot, &["kpdot"]),
    (Key::NumpadEqual, &["kpequal"]),
];

impl Key {
    /// Canonical, round-trippable name.
    pub fn name(self) -> &'static str {
        TABLE
            .iter()
            .find(|(k, _)| *k == self)
            .map(|(_, names)| names[0])
            .unwrap_or("unknown")
    }

    /// Parse a single key name; case-insensitive.
    pub fn parse(s: &str) -> Option<Key> {
        let lower = s.to_ascii_lowercase();
        TABLE
            .iter()
            .find(|(_, names)| names.iter().any(|n| *n == lower))
            .map(|(k, _)| *k)
    }

    /// The modifier bit this key contributes while held, if any.
    ///
    /// The Windows backend resolves sides through its own scan-code table, so
    /// this is unused there.
    #[allow(dead_code)]
    pub fn mod_bit(self) -> Option<Mods> {
        Some(match self {
            Key::LeftCtrl | Key::RightCtrl => Mods::CTRL,
            Key::LeftShift | Key::RightShift => Mods::SHIFT,
            Key::LeftAlt | Key::RightAlt => Mods::ALT,
            Key::LeftMeta | Key::RightMeta => Mods::META,
            _ => return None,
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A key plus the modifiers that must accompany it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    pub mods: Mods,
}

impl Chord {
    /// Parse `ctrl-[`, `ctrl-shift-a`, `esc`.
    ///
    /// `-` is the separator, so a literal `-` key must be spelled `minus`.
    pub fn parse(spec: &str) -> Result<Chord> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("empty key spec");
        }
        let mut mods = Mods::empty();
        let parts: Vec<&str> = spec.split('-').collect();
        // Trailing empty segment means the spec ended with the separator,
        // e.g. `ctrl--`; treat the final `-` as the Minus key.
        let (mod_parts, key_part) = match parts.split_last() {
            Some((last, rest)) if last.is_empty() && !rest.is_empty() => {
                (&rest[..rest.len() - 1], "minus")
            }
            Some((last, rest)) => (rest, *last),
            None => bail!("empty key spec"),
        };
        for p in mod_parts {
            let bit = match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => Mods::CTRL,
                "shift" | "s" => Mods::SHIFT,
                "alt" | "opt" | "option" | "a" | "m" => Mods::ALT,
                "meta" | "cmd" | "command" | "win" | "super" | "d" => Mods::META,
                other => bail!("unknown modifier `{other}` in `{spec}`"),
            };
            mods |= bit;
        }
        let key = Key::parse(key_part)
            .ok_or_else(|| anyhow::anyhow!("unknown key `{key_part}` in `{spec}`"))?;
        Ok(Chord { key, mods })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.mods.is_empty() {
            write!(f, "{}-", self.mods)?;
        }
        f.write_str(self.key.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ahk_equivalent_chords() {
        assert_eq!(
            Chord::parse("ctrl-[").unwrap(),
            Chord { key: Key::LeftBracket, mods: Mods::CTRL }
        );
        assert_eq!(
            Chord::parse("esc").unwrap(),
            Chord { key: Key::Escape, mods: Mods::empty() }
        );
        assert_eq!(
            Chord::parse("CTRL-Shift-A").unwrap(),
            Chord { key: Key::A, mods: Mods::CTRL | Mods::SHIFT }
        );
    }

    #[test]
    fn hyphen_key_needs_no_escape_hatch() {
        assert_eq!(Chord::parse("ctrl--").unwrap().key, Key::Minus);
        assert_eq!(Chord::parse("minus").unwrap().key, Key::Minus);
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(Chord::parse("hyper-a").is_err());
        assert!(Chord::parse("ctrl-nope").is_err());
        assert!(Chord::parse("").is_err());
    }

    #[test]
    fn names_round_trip() {
        for (key, names) in TABLE {
            assert_eq!(Key::parse(names[0]), Some(*key), "alias {:?}", names[0]);
            assert_eq!(key.name(), names[0]);
        }
    }
}
