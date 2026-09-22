//! Pure matching core: key event in, decision out. No platform calls.

use crate::action::Action;
use crate::keys::{Chord, Key, Mods};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Rule {
    pub trigger: Chord,
    /// `*` prefix: extra modifiers beyond `trigger.mods` are tolerated.
    pub wildcard_mods: bool,
    /// `~` prefix: the original key still reaches the focused app.
    pub passthrough: bool,
    pub actions: Vec<Action>,
    pub description: Option<String>,
}

/// What the backend should do with the event it just reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing matched; deliver the event untouched.
    Pass,
    /// Swallow the event; emit nothing.
    Consume,
    /// Run rule `index`'s actions. `passthrough` says whether the original
    /// event is also delivered.
    Run { index: usize, passthrough: bool },
}

#[derive(Debug)]
pub struct Engine {
    rules: Vec<Rule>,
    /// Key-downs we swallowed, so the matching key-up is swallowed too and
    /// the focused app never sees a dangling release.
    swallowed: HashSet<Key>,
}

impl Engine {
    pub fn new(rules: Vec<Rule>) -> Engine {
        Engine { rules, swallowed: HashSet::new() }
    }

    pub fn actions(&self, index: usize) -> &[Action] {
        &self.rules[index].actions
    }

    /// Feed one key event. `mods` is the modifier state at the moment of the
    /// event, excluding the event's own key.
    pub fn on_key(&mut self, key: Key, mods: Mods, down: bool) -> Decision {
        if !down {
            return if self.swallowed.remove(&key) { Decision::Consume } else { Decision::Pass };
        }

        for (index, rule) in self.rules.iter().enumerate() {
            if rule.trigger.key != key {
                continue;
            }
            let matched = if rule.wildcard_mods {
                mods.contains(rule.trigger.mods)
            } else {
                mods == rule.trigger.mods
            };
            if !matched {
                continue;
            }
            if !rule.passthrough {
                self.swallowed.insert(key);
            }
            return Decision::Run { index, passthrough: rule.passthrough };
        }
        Decision::Pass
    }

    /// Drop swallow state, e.g. after the backend loses the input grab.
    // Used by the macOS backend when the system disables the event tap.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.swallowed.clear();
    }
}

/// Modifier bookkeeping for backends whose events carry no modifier state
/// (Windows low-level hook, Linux evdev). Tracks sides so releasing one
/// Shift while the other is held keeps SHIFT set.
///
/// macOS reads the modifier state off each `CGEvent` instead, so this is
/// dead code in a macOS-only build.
#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)]
pub struct ModTracker {
    held: u8,
}

#[allow(dead_code)]
const SIDES: [(Key, u8, Mods); 8] = [
    (Key::LeftCtrl, 1 << 0, Mods::CTRL),
    (Key::RightCtrl, 1 << 1, Mods::CTRL),
    (Key::LeftShift, 1 << 2, Mods::SHIFT),
    (Key::RightShift, 1 << 3, Mods::SHIFT),
    (Key::LeftAlt, 1 << 4, Mods::ALT),
    (Key::RightAlt, 1 << 5, Mods::ALT),
    (Key::LeftMeta, 1 << 6, Mods::META),
    (Key::RightMeta, 1 << 7, Mods::META),
];

#[allow(dead_code)]
impl ModTracker {
    /// Record a key transition. Returns true if `key` was a modifier.
    pub fn update(&mut self, key: Key, down: bool) -> bool {
        for (k, bit, _) in SIDES {
            if k == key {
                if down {
                    self.held |= bit;
                } else {
                    self.held &= !bit;
                }
                return true;
            }
        }
        false
    }

    pub fn current(&self) -> Mods {
        let mut mods = Mods::empty();
        for (_, bit, m) in SIDES {
            if self.held & bit != 0 {
                mods |= m;
            }
        }
        mods
    }

    /// Forget every held side, e.g. after an input grab is lost.
    pub fn clear(&mut self) {
        self.held = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ImeState;

    fn rule(spec: &str, passthrough: bool, wildcard: bool, actions: Vec<Action>) -> Rule {
        Rule {
            trigger: Chord::parse(spec).unwrap(),
            wildcard_mods: wildcard,
            passthrough,
            actions,
            description: None,
        }
    }

    fn ahk_equivalent() -> Engine {
        Engine::new(vec![
            rule(
                "ctrl-[",
                false,
                false,
                vec![Action::Tap(Chord::parse("esc").unwrap()), Action::Ime(ImeState::Off)],
            ),
            rule("esc", true, false, vec![Action::Ime(ImeState::Off)]),
            rule("henkan", true, false, vec![Action::Ime(ImeState::On)]),
            rule("muhenkan", true, false, vec![Action::Ime(ImeState::Off)]),
        ])
    }

    #[test]
    fn ctrl_bracket_runs_actions_and_swallows_the_original() {
        let mut e = ahk_equivalent();
        assert_eq!(
            e.on_key(Key::LeftBracket, Mods::CTRL, true),
            Decision::Run { index: 0, passthrough: false }
        );
        assert_eq!(
            e.actions(0),
            &[Action::Tap(Chord::parse("esc").unwrap()), Action::Ime(ImeState::Off)]
        );
    }

    #[test]
    fn swallowed_key_up_never_reaches_the_app() {
        let mut e = ahk_equivalent();
        e.on_key(Key::LeftBracket, Mods::CTRL, true);
        assert_eq!(e.on_key(Key::LeftBracket, Mods::CTRL, false), Decision::Consume);
        // Second release (already cleared) passes: no phantom swallow.
        assert_eq!(e.on_key(Key::LeftBracket, Mods::CTRL, false), Decision::Pass);
    }

    #[test]
    fn passthrough_rule_leaves_key_up_alone() {
        let mut e = ahk_equivalent();
        assert_eq!(
            e.on_key(Key::Escape, Mods::empty(), true),
            Decision::Run { index: 1, passthrough: true }
        );
        assert_eq!(e.on_key(Key::Escape, Mods::empty(), false), Decision::Pass);
    }

    #[test]
    fn exact_mods_by_default_wildcard_when_asked() {
        let mut strict = ahk_equivalent();
        assert_eq!(strict.on_key(Key::LeftBracket, Mods::CTRL | Mods::SHIFT, true), Decision::Pass);
        assert_eq!(strict.on_key(Key::LeftBracket, Mods::empty(), true), Decision::Pass);

        let mut loose = Engine::new(vec![rule("ctrl-[", false, true, vec![])]);
        assert!(matches!(
            loose.on_key(Key::LeftBracket, Mods::CTRL | Mods::SHIFT, true),
            Decision::Run { .. }
        ));
        assert_eq!(loose.on_key(Key::LeftBracket, Mods::SHIFT, true), Decision::Pass);
    }

    #[test]
    fn first_matching_rule_wins() {
        let mut e = Engine::new(vec![
            rule("esc", true, false, vec![Action::Ime(ImeState::On)]),
            rule("esc", true, false, vec![Action::Ime(ImeState::Off)]),
        ]);
        assert_eq!(
            e.on_key(Key::Escape, Mods::empty(), true),
            Decision::Run { index: 0, passthrough: true }
        );
    }

    #[test]
    fn mod_tracker_keeps_bit_while_other_side_held() {
        let mut t = ModTracker::default();
        assert!(t.update(Key::LeftShift, true));
        assert!(t.update(Key::RightShift, true));
        t.update(Key::LeftShift, false);
        assert_eq!(t.current(), Mods::SHIFT);
        t.update(Key::RightShift, false);
        assert_eq!(t.current(), Mods::empty());
        assert!(!t.update(Key::A, true));
    }
}
