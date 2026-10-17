//! Pure matching core: key event in, decision out. No platform calls.

use crate::action::Action;
use crate::keys::{Chord, Key, Mods};
use std::collections::{HashMap, HashSet};

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

/// One-to-one key replacement applied to every event before rules or modifier
/// tracking see it, so a remapped key is indistinguishable from the real one
/// downstream. This is what lets `capslock -> leftctrl` make `ctrl-…` rules
/// fire: modifier state is derived from the remapped stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    pub from: Key,
    pub to: Key,
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
    remaps: HashMap<Key, Key>,
    /// Physical sources still holding each logical key down. Several sources
    /// may share a target, including the target's own physical key.
    held_sources: HashMap<Key, Key>,
    /// Key-downs we swallowed, so the matching key-up is swallowed too and
    /// the focused app never sees a dangling release.
    swallowed: HashSet<Key>,
}

impl Engine {
    pub fn new(rules: Vec<Rule>) -> Engine {
        Engine {
            rules,
            remaps: HashMap::new(),
            held_sources: HashMap::new(),
            swallowed: HashSet::new(),
        }
    }

    /// Install key replacements. `Config::remaps_for` guarantees each source
    /// key appears once and no target is itself a source, so a replacement is
    /// a single lookup, never a chain.
    pub fn with_remaps(mut self, remaps: &[Remap]) -> Engine {
        self.remaps = remaps.iter().map(|r| (r.from, r.to)).collect();
        self
    }

    /// The key an incoming event should be treated as.
    // macOS refuses remaps at startup, so only Linux and Windows call this.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn remap(&self, key: Key) -> Key {
        self.remaps.get(&key).copied().unwrap_or(key)
    }

    /// Merge physical transitions onto the logical key stream. Only the first
    /// source presses a target and only the last source releases it. Repeated
    /// downs from an already-held source remain autorepeat events.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn remap_event(&mut self, physical: Key, down: bool) -> Option<Key> {
        let key = self.remap(physical);
        if down {
            let repeat = self.held_sources.contains_key(&physical);
            let held = self.held_sources.values().any(|&target| target == key);
            self.held_sources.insert(physical, key);
            if held && !repeat {
                return None;
            }
        } else {
            self.held_sources.remove(&physical);
            if self.held_sources.values().any(|&target| target == key) {
                return None;
            }
        }
        Some(key)
    }

    /// Every `(from, to)` pair, for backends that must validate them.
    pub fn remap_pairs(&self) -> impl Iterator<Item = (Key, Key)> + '_ {
        self.remaps.iter().map(|(from, to)| (*from, *to))
    }

    pub fn actions(&self, index: usize) -> &[Action] {
        &self.rules[index].actions
    }

    /// Feed one key event. `mods` is the modifier state at the moment of the
    /// event, excluding the event's own key.
    pub fn on_key(&mut self, key: Key, mods: Mods, down: bool) -> Decision {
        if !down {
            return if self.swallowed.remove(&key) {
                Decision::Consume
            } else {
                Decision::Pass
            };
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
            return Decision::Run {
                index,
                passthrough: rule.passthrough,
            };
        }
        Decision::Pass
    }

    /// Drop swallow state, e.g. after the backend loses the input grab.
    // Used by the macOS backend when the system disables the event tap.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.swallowed.clear();
        self.held_sources.clear();
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
                vec![
                    Action::Tap(Chord::parse("esc").unwrap()),
                    Action::Ime(ImeState::Off),
                ],
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
            Decision::Run {
                index: 0,
                passthrough: false
            }
        );
        assert_eq!(
            e.actions(0),
            &[
                Action::Tap(Chord::parse("esc").unwrap()),
                Action::Ime(ImeState::Off)
            ]
        );
    }

    #[test]
    fn swallowed_key_up_never_reaches_the_app() {
        let mut e = ahk_equivalent();
        e.on_key(Key::LeftBracket, Mods::CTRL, true);
        assert_eq!(
            e.on_key(Key::LeftBracket, Mods::CTRL, false),
            Decision::Consume
        );
        // Second release (already cleared) passes: no phantom swallow.
        assert_eq!(
            e.on_key(Key::LeftBracket, Mods::CTRL, false),
            Decision::Pass
        );
    }

    #[test]
    fn passthrough_rule_leaves_key_up_alone() {
        let mut e = ahk_equivalent();
        assert_eq!(
            e.on_key(Key::Escape, Mods::empty(), true),
            Decision::Run {
                index: 1,
                passthrough: true
            }
        );
        assert_eq!(e.on_key(Key::Escape, Mods::empty(), false), Decision::Pass);
    }

    #[test]
    fn exact_mods_by_default_wildcard_when_asked() {
        let mut strict = ahk_equivalent();
        assert_eq!(
            strict.on_key(Key::LeftBracket, Mods::CTRL | Mods::SHIFT, true),
            Decision::Pass
        );
        assert_eq!(
            strict.on_key(Key::LeftBracket, Mods::empty(), true),
            Decision::Pass
        );

        let mut loose = Engine::new(vec![rule("ctrl-[", false, true, vec![])]);
        assert!(matches!(
            loose.on_key(Key::LeftBracket, Mods::CTRL | Mods::SHIFT, true),
            Decision::Run { .. }
        ));
        assert_eq!(
            loose.on_key(Key::LeftBracket, Mods::SHIFT, true),
            Decision::Pass
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let mut e = Engine::new(vec![
            rule("esc", true, false, vec![Action::Ime(ImeState::On)]),
            rule("esc", true, false, vec![Action::Ime(ImeState::Off)]),
        ]);
        assert_eq!(
            e.on_key(Key::Escape, Mods::empty(), true),
            Decision::Run {
                index: 0,
                passthrough: true
            }
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

    #[test]
    fn remap_overlap_keeps_ctrl_held_until_last_source_releases() {
        // Cover either press order and either release order, including the
        // native target being released while the remapped source stays down.
        for presses in [
            [Key::LeftCtrl, Key::CapsLock],
            [Key::CapsLock, Key::LeftCtrl],
        ] {
            for releases in [presses, [presses[1], presses[0]]] {
                let mut e = ahk_equivalent().with_remaps(&[Remap {
                    from: Key::CapsLock,
                    to: Key::LeftCtrl,
                    description: None,
                }]);
                let mut tracker = ModTracker::default();
                let mut forwarded = Vec::new();
                for (physical, down) in
                    [(presses[0], true), (presses[1], true), (releases[0], false)]
                {
                    if let Some(key) = e.remap_event(physical, down) {
                        let before = tracker.current();
                        tracker.update(key, down);
                        let mods = if down { before } else { tracker.current() };
                        assert_eq!(e.on_key(key, mods, down), Decision::Pass);
                        forwarded.push((key, down));
                    }
                }
                assert_eq!(forwarded, [(Key::LeftCtrl, true)]);
                assert_eq!(tracker.current(), Mods::CTRL);
                assert_eq!(
                    e.on_key(Key::LeftBracket, tracker.current(), true),
                    Decision::Run {
                        index: 0,
                        passthrough: false,
                    }
                );
                assert_eq!(
                    e.on_key(Key::LeftBracket, tracker.current(), false),
                    Decision::Consume
                );

                let key = e.remap_event(releases[1], false).unwrap();
                tracker.update(key, false);
                assert_eq!(e.on_key(key, tracker.current(), false), Decision::Pass);
                forwarded.push((key, false));
                assert_eq!(forwarded, [(Key::LeftCtrl, true), (Key::LeftCtrl, false)]);
                assert_eq!(tracker.current(), Mods::empty());
                assert_eq!(
                    e.on_key(Key::LeftBracket, tracker.current(), true),
                    Decision::Pass
                );
            }
        }
    }

    #[test]
    fn remap_shared_target_preserves_repeats_and_resets_sources() {
        let mut e = Engine::new(vec![rule("a", false, false, vec![])]).with_remaps(
            &[Key::CapsLock, Key::Escape].map(|from| Remap {
                from,
                to: Key::A,
                description: None,
            }),
        );
        assert_eq!(e.remap_event(Key::CapsLock, true), Some(Key::A));
        assert!(matches!(
            e.on_key(Key::A, Mods::empty(), true),
            Decision::Run { .. }
        ));
        assert_eq!(e.remap_event(Key::Escape, true), None);
        assert_eq!(e.remap_event(Key::A, true), None);
        // Repeats don't add extra sources that would leave the target stuck.
        assert_eq!(e.remap_event(Key::CapsLock, true), Some(Key::A));
        assert_eq!(e.remap_event(Key::CapsLock, false), None);
        assert_eq!(e.remap_event(Key::Escape, false), None);
        assert_eq!(e.remap_event(Key::A, false), Some(Key::A));
        assert_eq!(e.on_key(Key::A, Mods::empty(), false), Decision::Consume);

        assert_eq!(e.remap_event(Key::CapsLock, true), Some(Key::A));
        e.reset();
        assert_eq!(e.remap_event(Key::A, true), Some(Key::A));
        assert_eq!(e.remap_event(Key::A, false), Some(Key::A));
    }

    /// The reason remaps exist: Caps Lock stands in for Ctrl, and the rule
    /// written against `ctrl-]` must fire because modifier state is derived
    /// from the remapped stream, not the physical one.
    #[test]
    fn remapped_modifier_makes_chord_rules_match() {
        let remap = Remap {
            from: Key::CapsLock,
            to: Key::LeftCtrl,
            description: None,
        };
        let mut e = ahk_equivalent().with_remaps(&[remap]);
        let mut tracker = ModTracker::default();

        let caps = e.remap(Key::CapsLock);
        assert_eq!(caps, Key::LeftCtrl);
        tracker.update(caps, true);
        assert!(matches!(
            e.on_key(caps, Mods::empty(), true),
            Decision::Pass
        ));

        assert!(matches!(
            e.on_key(Key::LeftBracket, tracker.current(), true),
            Decision::Run {
                index: 0,
                passthrough: false
            }
        ));

        // Unmapped keys are untouched, and the physical Caps key is not what
        // the engine reports afterwards.
        assert_eq!(e.remap(Key::A), Key::A);
        assert_eq!(
            e.remap_pairs().collect::<Vec<_>>(),
            [(Key::CapsLock, Key::LeftCtrl)]
        );
    }
}
