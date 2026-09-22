//! What a matched rule does.

use crate::keys::Chord;
use anyhow::{Result, bail};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeState {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Synthesize a key press and release.
    Tap(Chord),
    /// Drive the platform IME open/closed.
    Ime(ImeState),
    /// Select a named input source (macOS TIS id, Linux IME engine name).
    InputSource(String),
    /// Run a shell command, detached.
    Cmd(String),
}

impl Action {
    /// Parse one `to` entry.
    ///
    /// * `ime:off` / `ime:on` / `ime:toggle`
    /// * `source:<id>`
    /// * `cmd:<shell command>`
    /// * anything else is a key chord (`esc`, `ctrl-a`)
    pub fn parse(spec: &str) -> Result<Action> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("ime:") {
            return Ok(Action::Ime(match rest.trim() {
                "on" => ImeState::On,
                "off" => ImeState::Off,
                "toggle" => ImeState::Toggle,
                other => bail!("unknown ime state `{other}`; expected on|off|toggle"),
            }));
        }
        if let Some(rest) = spec.strip_prefix("source:") {
            let id = rest.trim();
            if id.is_empty() {
                bail!("`source:` needs an input source id");
            }
            return Ok(Action::InputSource(id.to_string()));
        }
        if let Some(rest) = spec.strip_prefix("cmd:") {
            let cmd = rest.trim();
            if cmd.is_empty() {
                bail!("`cmd:` needs a command line");
            }
            return Ok(Action::Cmd(cmd.to_string()));
        }
        Ok(Action::Tap(Chord::parse(spec)?))
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Tap(c) => write!(f, "{c}"),
            Action::Ime(ImeState::On) => f.write_str("ime:on"),
            Action::Ime(ImeState::Off) => f.write_str("ime:off"),
            Action::Ime(ImeState::Toggle) => f.write_str("ime:toggle"),
            Action::InputSource(id) => write!(f, "source:{id}"),
            Action::Cmd(c) => write!(f, "cmd:{c}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Key, Mods};

    #[test]
    fn parses_each_action_form() {
        assert_eq!(
            Action::parse("ime:off").unwrap(),
            Action::Ime(ImeState::Off)
        );
        assert_eq!(
            Action::parse("source:com.apple.keylayout.ABC").unwrap(),
            Action::InputSource("com.apple.keylayout.ABC".into())
        );
        assert_eq!(
            Action::parse("cmd:notify-send hi").unwrap(),
            Action::Cmd("notify-send hi".into())
        );
        assert_eq!(
            Action::parse("esc").unwrap(),
            Action::Tap(Chord {
                key: Key::Escape,
                mods: Mods::empty()
            })
        );
    }

    #[test]
    fn rejects_malformed_prefixed_actions() {
        assert!(Action::parse("ime:sideways").is_err());
        assert!(Action::parse("source:").is_err());
        assert!(Action::parse("cmd:").is_err());
    }
}
