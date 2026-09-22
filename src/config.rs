//! TOML config: parse, filter by OS, compile to engine rules.

use crate::action::Action;
use crate::engine::Rule;
use crate::keys::Chord;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub rule: Vec<RuleSpec>,
    // Each of these is read only by its own backend, so the other targets see
    // an unread field.
    #[serde(default)]
    #[allow(dead_code)]
    pub macos: MacosConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub linux: LinuxConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// Trigger chord, optionally prefixed with `~` (passthrough) and/or
    /// `*` (allow extra modifiers), mirroring AutoHotkey.
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    /// Restrict to these platforms (`macos`, `windows`, `linux`).
    /// Empty means every platform.
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub desc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MacImeMethod {
    /// Post the JIS 英数 / かな keycodes. Works with every Japanese IME and
    /// keeps the selected input method; the macOS analogue of
    /// `ImmSetOpenStatus`.
    Eisu,
    /// Switch input source outright via Text Input Services.
    Source,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MacosConfig {
    pub ime: MacImeMethod,
    /// Input source selected by `ime:off` when `ime = "source"`.
    pub ascii_source: String,
    /// Input source selected by `ime:on` when `ime = "source"`.
    pub japanese_source: String,
}

impl Default for MacosConfig {
    fn default() -> Self {
        MacosConfig {
            ime: MacImeMethod::Eisu,
            ascii_source: "com.apple.keylayout.ABC".into(),
            japanese_source: "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LinuxConfig {
    /// Device selectors: `/dev/input/...` paths or case-insensitive
    /// substrings of the device name. Empty means every keyboard.
    pub devices: Vec<String>,
    /// Override IME commands. Unset falls back to fcitx5 then ibus.
    pub ime_on: Option<String>,
    pub ime_off: Option<String>,
    pub ime_toggle: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// Compile the rules that apply to the running platform.
    pub fn rules_for(&self, os: &str) -> Result<Vec<Rule>> {
        let mut out = Vec::new();
        for (i, spec) in self.rule.iter().enumerate() {
            if !spec.os.is_empty() && !spec.os.iter().any(|o| o.eq_ignore_ascii_case(os)) {
                continue;
            }
            out.push(
                spec.compile()
                    .with_context(|| format!("rule #{} (`{}`)", i + 1, spec.from))?,
            );
        }
        Ok(out)
    }
}

impl RuleSpec {
    fn compile(&self) -> Result<Rule> {
        let mut rest = self.from.trim();
        let mut passthrough = false;
        let mut wildcard_mods = false;
        loop {
            match rest.as_bytes().first() {
                Some(b'~') if !passthrough => {
                    passthrough = true;
                    rest = &rest[1..];
                }
                Some(b'*') if !wildcard_mods => {
                    wildcard_mods = true;
                    rest = &rest[1..];
                }
                Some(b'~') | Some(b'*') => bail!("duplicated `~`/`*` prefix in `{}`", self.from),
                _ => break,
            }
        }
        let trigger = Chord::parse(rest)?;
        let actions = self
            .to
            .iter()
            .map(|a| Action::parse(a))
            .collect::<Result<Vec<_>>>()?;
        Ok(Rule {
            trigger,
            wildcard_mods,
            passthrough,
            actions,
            description: self.desc.clone(),
        })
    }
}

/// `$KAGI_CONFIG`, else `~/.config/kagi/kagi.toml`, else (Windows)
/// `%APPDATA%\kagi\kagi.toml`.
pub fn default_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("KAGI_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    for candidate in candidate_paths() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    match candidate_paths().into_iter().next() {
        Some(p) => Ok(p),
        None => bail!("cannot locate a home directory; set KAGI_CONFIG"),
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(xdg).join("kagi").join("kagi.toml"));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        out.push(PathBuf::from(home).join(".config").join("kagi").join("kagi.toml"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(PathBuf::from(appdata).join("kagi").join("kagi.toml"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ImeState;
    use crate::keys::{Key, Mods};

    const SAMPLE: &str = r#"
[[rule]]
from = "ctrl-["
to = ["esc", "ime:off"]

[[rule]]
from = "~esc"
to = ["ime:off"]

[[rule]]
from = "~henkan"
to = ["ime:on"]
os = ["windows", "linux"]
"#;

    #[test]
    fn compiles_ahk_ruleset() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let rules = cfg.rules_for("windows").unwrap();
        assert_eq!(rules.len(), 3);

        assert_eq!(rules[0].trigger, Chord { key: Key::LeftBracket, mods: Mods::CTRL });
        assert!(!rules[0].passthrough);
        assert_eq!(rules[0].actions[1], Action::Ime(ImeState::Off));

        assert!(rules[1].passthrough);
        assert_eq!(rules[1].trigger.key, Key::Escape);
    }

    #[test]
    fn os_filter_drops_foreign_rules() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.rules_for("macos").unwrap().len(), 2);
        assert_eq!(cfg.rules_for("linux").unwrap().len(), 3);
    }

    #[test]
    fn prefixes_combine_in_either_order() {
        let cfg = Config::parse(
            r#"
[[rule]]
from = "*~ctrl-["
[[rule]]
from = "~*ctrl-["
"#,
        )
        .unwrap();
        for r in cfg.rules_for("macos").unwrap() {
            assert!(r.passthrough && r.wildcard_mods);
        }
    }

    #[test]
    fn bad_rule_names_its_index() {
        let cfg = Config::parse(
            r#"
[[rule]]
from = "esc"
[[rule]]
from = "ctrl-nope"
"#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.rules_for("macos").unwrap_err());
        assert!(err.contains("rule #2"), "{err}");
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(Config::parse("[[rule]]\nfrom = \"esc\"\ntypo = 1\n").is_err());
        assert!(Config::parse("[macos]\nime = \"nope\"\n").is_err());
    }

    #[test]
    fn macos_defaults_to_eisu() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.macos.ime, MacImeMethod::Eisu);
        assert_eq!(cfg.macos.ascii_source, "com.apple.keylayout.ABC");
    }
}
