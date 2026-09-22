//! Linux backend: evdev capture, uinput synthesis, fcitx5/ibus for the IME.
//!
//! Working at the evdev layer means the same code path serves X11 and Wayland:
//! we take an exclusive grab (`EVIOCGRAB`) on the real keyboards so the
//! compositor never sees their raw stream, and re-publish everything the rules
//! let through on a single uinput device. Nothing here talks to a display
//! server, so there is no Wayland protocol to be refused by.
//!
//! Because we own the grab, our own synthetic events are written to the uinput
//! device and can never re-enter our readers — the other backends need an
//! injection marker to avoid feedback, this one does not.

use crate::action::ImeState;
use crate::config::{Config, LinuxConfig};
use crate::engine::{Decision, Engine, ModTracker};
use crate::keys::{Chord, Key, Mods};
use crate::platform::{Emitter, dispatch, run_blocking};
use anyhow::{Context, Result, anyhow, bail};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, EventType, InputEvent, Key as EvKey, Synchronization};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Directory the kernel exposes evdev character devices in.
const INPUT_DIR: &str = "/dev/input";
/// Name of the uinput device everything is re-published on. Also used to skip
/// a stale device left behind by another kagi instance.
const VIRTUAL_NAME: &str = "kagi virtual keyboard";

/// evdev key values.
const VALUE_UP: i32 = 0;
const VALUE_DOWN: i32 = 1;
const VALUE_REPEAT: i32 = 2;

/// `Key` <-> evdev `KEY_*`.
///
/// `Key::Eisu` and `Key::Kana` are deliberately absent: they name the macOS
/// 英数 / かな keys, which have no evdev equivalent (`KEY_HANGEUL` and
/// `KEY_KATAKANA` are different keys with different meanings, and mapping to
/// them would silently do the wrong thing). `from_key` returns `None` for
/// them and `tap` turns that into an explicit error.
const KEY_MAP: &[(Key, EvKey)] = &[
    (Key::A, EvKey::KEY_A),
    (Key::B, EvKey::KEY_B),
    (Key::C, EvKey::KEY_C),
    (Key::D, EvKey::KEY_D),
    (Key::E, EvKey::KEY_E),
    (Key::F, EvKey::KEY_F),
    (Key::G, EvKey::KEY_G),
    (Key::H, EvKey::KEY_H),
    (Key::I, EvKey::KEY_I),
    (Key::J, EvKey::KEY_J),
    (Key::K, EvKey::KEY_K),
    (Key::L, EvKey::KEY_L),
    (Key::M, EvKey::KEY_M),
    (Key::N, EvKey::KEY_N),
    (Key::O, EvKey::KEY_O),
    (Key::P, EvKey::KEY_P),
    (Key::Q, EvKey::KEY_Q),
    (Key::R, EvKey::KEY_R),
    (Key::S, EvKey::KEY_S),
    (Key::T, EvKey::KEY_T),
    (Key::U, EvKey::KEY_U),
    (Key::V, EvKey::KEY_V),
    (Key::W, EvKey::KEY_W),
    (Key::X, EvKey::KEY_X),
    (Key::Y, EvKey::KEY_Y),
    (Key::Z, EvKey::KEY_Z),
    (Key::Num0, EvKey::KEY_0),
    (Key::Num1, EvKey::KEY_1),
    (Key::Num2, EvKey::KEY_2),
    (Key::Num3, EvKey::KEY_3),
    (Key::Num4, EvKey::KEY_4),
    (Key::Num5, EvKey::KEY_5),
    (Key::Num6, EvKey::KEY_6),
    (Key::Num7, EvKey::KEY_7),
    (Key::Num8, EvKey::KEY_8),
    (Key::Num9, EvKey::KEY_9),
    (Key::F1, EvKey::KEY_F1),
    (Key::F2, EvKey::KEY_F2),
    (Key::F3, EvKey::KEY_F3),
    (Key::F4, EvKey::KEY_F4),
    (Key::F5, EvKey::KEY_F5),
    (Key::F6, EvKey::KEY_F6),
    (Key::F7, EvKey::KEY_F7),
    (Key::F8, EvKey::KEY_F8),
    (Key::F9, EvKey::KEY_F9),
    (Key::F10, EvKey::KEY_F10),
    (Key::F11, EvKey::KEY_F11),
    (Key::F12, EvKey::KEY_F12),
    (Key::F13, EvKey::KEY_F13),
    (Key::F14, EvKey::KEY_F14),
    (Key::F15, EvKey::KEY_F15),
    (Key::F16, EvKey::KEY_F16),
    (Key::F17, EvKey::KEY_F17),
    (Key::F18, EvKey::KEY_F18),
    (Key::F19, EvKey::KEY_F19),
    (Key::F20, EvKey::KEY_F20),
    (Key::Grave, EvKey::KEY_GRAVE),
    (Key::Minus, EvKey::KEY_MINUS),
    (Key::Equal, EvKey::KEY_EQUAL),
    (Key::LeftBracket, EvKey::KEY_LEFTBRACE),
    (Key::RightBracket, EvKey::KEY_RIGHTBRACE),
    (Key::Backslash, EvKey::KEY_BACKSLASH),
    (Key::Semicolon, EvKey::KEY_SEMICOLON),
    (Key::Quote, EvKey::KEY_APOSTROPHE),
    (Key::Comma, EvKey::KEY_COMMA),
    (Key::Period, EvKey::KEY_DOT),
    (Key::Slash, EvKey::KEY_SLASH),
    (Key::Escape, EvKey::KEY_ESC),
    (Key::Tab, EvKey::KEY_TAB),
    (Key::CapsLock, EvKey::KEY_CAPSLOCK),
    (Key::Space, EvKey::KEY_SPACE),
    (Key::Backspace, EvKey::KEY_BACKSPACE),
    (Key::Enter, EvKey::KEY_ENTER),
    (Key::Insert, EvKey::KEY_INSERT),
    (Key::Delete, EvKey::KEY_DELETE),
    (Key::Home, EvKey::KEY_HOME),
    (Key::End, EvKey::KEY_END),
    (Key::PageUp, EvKey::KEY_PAGEUP),
    (Key::PageDown, EvKey::KEY_PAGEDOWN),
    (Key::Left, EvKey::KEY_LEFT),
    (Key::Right, EvKey::KEY_RIGHT),
    (Key::Up, EvKey::KEY_UP),
    (Key::Down, EvKey::KEY_DOWN),
    (Key::PrintScreen, EvKey::KEY_SYSRQ),
    (Key::ScrollLock, EvKey::KEY_SCROLLLOCK),
    (Key::Pause, EvKey::KEY_PAUSE),
    (Key::Menu, EvKey::KEY_COMPOSE),
    (Key::NumLock, EvKey::KEY_NUMLOCK),
    (Key::LeftCtrl, EvKey::KEY_LEFTCTRL),
    (Key::RightCtrl, EvKey::KEY_RIGHTCTRL),
    (Key::LeftShift, EvKey::KEY_LEFTSHIFT),
    (Key::RightShift, EvKey::KEY_RIGHTSHIFT),
    (Key::LeftAlt, EvKey::KEY_LEFTALT),
    (Key::RightAlt, EvKey::KEY_RIGHTALT),
    (Key::LeftMeta, EvKey::KEY_LEFTMETA),
    (Key::RightMeta, EvKey::KEY_RIGHTMETA),
    (Key::Henkan, EvKey::KEY_HENKAN),
    (Key::Muhenkan, EvKey::KEY_MUHENKAN),
    (Key::KatakanaHiragana, EvKey::KEY_KATAKANAHIRAGANA),
    (Key::Zenkaku, EvKey::KEY_ZENKAKUHANKAKU),
    (Key::IntlRo, EvKey::KEY_RO),
    (Key::IntlYen, EvKey::KEY_YEN),
    (Key::Numpad0, EvKey::KEY_KP0),
    (Key::Numpad1, EvKey::KEY_KP1),
    (Key::Numpad2, EvKey::KEY_KP2),
    (Key::Numpad3, EvKey::KEY_KP3),
    (Key::Numpad4, EvKey::KEY_KP4),
    (Key::Numpad5, EvKey::KEY_KP5),
    (Key::Numpad6, EvKey::KEY_KP6),
    (Key::Numpad7, EvKey::KEY_KP7),
    (Key::Numpad8, EvKey::KEY_KP8),
    (Key::Numpad9, EvKey::KEY_KP9),
    (Key::NumpadEnter, EvKey::KEY_KPENTER),
    (Key::NumpadPlus, EvKey::KEY_KPPLUS),
    (Key::NumpadMinus, EvKey::KEY_KPMINUS),
    (Key::NumpadMultiply, EvKey::KEY_KPASTERISK),
    (Key::NumpadDivide, EvKey::KEY_KPSLASH),
    (Key::NumpadDot, EvKey::KEY_KPDOT),
    (Key::NumpadEqual, EvKey::KEY_KPEQUAL),
];

/// Every modifier side, in the order `Backend::held` indexes them. The bit a
/// side contributes comes from `Key::mod_bit`, so this table cannot drift from
/// the core's idea of what counts as a modifier.
const MOD_SIDES: [(EvKey, Key); 8] = [
    (EvKey::KEY_LEFTCTRL, Key::LeftCtrl),
    (EvKey::KEY_RIGHTCTRL, Key::RightCtrl),
    (EvKey::KEY_LEFTSHIFT, Key::LeftShift),
    (EvKey::KEY_RIGHTSHIFT, Key::RightShift),
    (EvKey::KEY_LEFTALT, Key::LeftAlt),
    (EvKey::KEY_RIGHTALT, Key::RightAlt),
    (EvKey::KEY_LEFTMETA, Key::LeftMeta),
    (EvKey::KEY_RIGHTMETA, Key::RightMeta),
];

/// `(index into MOD_SIDES, evdev code, modifier bit)` for every side.
fn mod_sides() -> impl Iterator<Item = (usize, u16, Mods)> {
    MOD_SIDES
        .iter()
        .enumerate()
        .filter_map(|(i, (ev, key))| key.mod_bit().map(|bit| (i, ev.code(), bit)))
}

/// Side synthesized when a chord wants a modifier nothing is holding.
const MOD_PREFERRED: [(Mods, EvKey); 4] = [
    (Mods::CTRL, EvKey::KEY_LEFTCTRL),
    (Mods::SHIFT, EvKey::KEY_LEFTSHIFT),
    (Mods::ALT, EvKey::KEY_LEFTALT),
    (Mods::META, EvKey::KEY_LEFTMETA),
];

/// evdev code -> `Key`.
fn to_key(code: u16) -> Option<Key> {
    KEY_MAP
        .iter()
        .find(|(_, ev)| ev.code() == code)
        .map(|(k, _)| *k)
}

/// `Key` -> evdev code. `None` for the two macOS-only names.
fn from_key(key: Key) -> Option<u16> {
    KEY_MAP
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, ev)| ev.code())
}

fn key_event(code: u16, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY, code, value)
}

// ---------------------------------------------------------------- devices

struct Selected {
    path: PathBuf,
    name: String,
    device: Device,
}

/// A keyboard advertises the whole alphabet; mice, lid switches and power
/// buttons publish `EV_KEY` too, so a plain "has keys" test is not enough.
fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(EvKey::KEY_A) && keys.contains(EvKey::KEY_Z))
}

/// `/dev/input/event12` sorts after `event2`, unlike the lexical order.
fn event_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("event"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Devices that opened, paired with their path.
type Opened = Vec<(PathBuf, Device)>;

/// Open every `/dev/input/event*`. Returns the devices we could open plus the
/// paths that were refused, so the caller can explain a permission problem.
fn open_input_devices() -> Result<(Opened, Vec<PathBuf>)> {
    let dir = Path::new(INPUT_DIR);
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if is_event {
            paths.push(path);
        }
    }
    paths.sort_by_key(|p| event_index(p));

    let mut opened = Vec::new();
    let mut denied = Vec::new();
    for path in paths {
        match Device::open(&path) {
            Ok(device) => opened.push((path, device)),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => denied.push(path),
            // Anything else (device vanished mid-enumeration, not an evdev
            // node) is not actionable; the "nothing matched" error lists what
            // we did manage to open.
            Err(_) => {}
        }
    }
    Ok((opened, denied))
}

fn selector_matches(selector: &str, path: &Path, name: &str) -> bool {
    let selector = selector.trim();
    if selector.starts_with('/') {
        path.to_str() == Some(selector)
    } else {
        name.to_lowercase().contains(&selector.to_lowercase())
    }
}

/// Pick the devices to read. An explicit selector wins over the keyboard
/// heuristic: naming a device in the config means "use this one".
fn select_devices(selectors: &[String]) -> Result<Vec<Selected>> {
    let (opened, denied) = open_input_devices()?;

    let mut selected = Vec::new();
    let mut seen = Vec::new();
    for (path, device) in opened {
        let name = device.name().unwrap_or("<unnamed>").to_string();
        // Never read our own (or a previous instance's) virtual device.
        if name == VIRTUAL_NAME {
            continue;
        }
        let keyboard = is_keyboard(&device);
        let wanted = if selectors.is_empty() {
            keyboard
        } else {
            selectors.iter().any(|s| selector_matches(s, &path, &name))
        };
        seen.push((path.clone(), name.clone(), keyboard));
        if wanted {
            selected.push(Selected { path, name, device });
        }
    }

    if !selected.is_empty() {
        return Ok(selected);
    }

    let mut msg = if selectors.is_empty() {
        "no keyboard found among the input devices".to_string()
    } else {
        format!(
            "no input device matched devices = [{}] under [linux]",
            selectors
                .iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if seen.is_empty() {
        msg.push_str("\n  no input device could be opened at all");
    } else {
        msg.push_str("\n  devices found:");
        for (path, name, keyboard) in &seen {
            let mark = if *keyboard { "  [keyboard]" } else { "" };
            msg.push_str(&format!("\n    {:<20} {name}{mark}", path.display()));
        }
    }
    if !denied.is_empty() {
        msg.push_str(&format!(
            "\n  permission denied on {} device node(s), e.g. {}\
             \n  remedy: sudo usermod -aG input $USER (log out and back in), or run kagi as root",
            denied.len(),
            denied[0].display()
        ));
    }
    bail!(msg)
}

fn grab_error(selected: &Selected, e: io::Error) -> anyhow::Error {
    let where_ = format!("{} ({})", selected.path.display(), selected.name);
    if e.kind() == io::ErrorKind::PermissionDenied {
        anyhow!(
            "EVIOCGRAB denied on {where_}: {e}\
             \n  remedy: sudo usermod -aG input $USER (log out and back in), or run kagi as root"
        )
    } else {
        anyhow!(
            "could not grab {where_}: {e}\
             \n  another process (a second kagi, or another remapper) may already hold an \
             exclusive grab on this device"
        )
    }
}

fn uinput_error(e: io::Error) -> anyhow::Error {
    match e.kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound => anyhow!(
            "cannot use /dev/uinput: {e}\
             \n  remedy: load the module (sudo modprobe uinput, and echo uinput | sudo tee \
             /etc/modules-load.d/uinput.conf to make it stick),\
             \n  then grant access with a udev rule in /etc/udev/rules.d/99-kagi.rules:\
             \n    KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\", OPTIONS+=\"static_node=uinput\"\
             \n  and add yourself to the input group (sudo usermod -aG input $USER) - or run kagi as root"
        ),
        _ => anyhow!("creating the kagi virtual keyboard failed: {e}"),
    }
}

// ---------------------------------------------------------------- reading

/// One `SYN_REPORT`-terminated packet of key events.
type Packet = Vec<InputEvent>;

/// What a reader thread hands to the main loop.
enum Report {
    Keys(Packet),
    /// The device stopped producing events (unplugged, or the node vanished).
    Lost(String),
}

/// Blocking reader for one device. Batches each packet so the main loop can
/// re-emit it as a single report, and exits when the device goes away or the
/// main loop drops the receiver.
fn reader(mut device: Device, label: String, tx: Sender<Report>) {
    let mut packet: Packet = Vec::new();
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                eprintln!("kagi: {label}: reading events failed: {e}");
                let _ = tx.send(Report::Lost(label));
                return;
            }
        };
        for event in events {
            let kind = event.event_type();
            if kind == EventType::KEY {
                packet.push(event);
            } else if kind == EventType::SYNCHRONIZATION
                && event.code() == Synchronization::SYN_REPORT.0
                && !packet.is_empty()
            {
                // Only EV_KEY is remapped, so EV_MSC/EV_LED traffic is dropped
                // rather than forwarded; the SYN_REPORT is what closes the
                // packet and is re-created by `VirtualDevice::emit`.
                if tx.send(Report::Keys(std::mem::take(&mut packet))).is_err() {
                    return;
                }
            }
        }
    }
}

/// Grab (or merely open, for `watch`) the devices and merge them into one
/// channel, one thread per device.
fn spawn_readers(devices: Vec<Selected>, grab: bool) -> Result<Receiver<Report>> {
    let (tx, rx) = mpsc::channel();
    for mut selected in devices {
        if grab {
            if let Err(e) = selected.device.grab() {
                return Err(grab_error(&selected, e));
            }
        }
        eprintln!(
            "kagi: {} {} ({})",
            if grab { "grabbed" } else { "watching" },
            selected.name,
            selected.path.display()
        );
        let tx = tx.clone();
        let label = selected.name;
        let device = selected.device;
        thread::spawn(move || reader(device, label, tx));
    }
    Ok(rx)
}

// ---------------------------------------------------------------- emitting

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImeHelper {
    Fcitx5,
    Ibus,
}

/// ibus engine that means "plain ASCII"; `ibus engine` prints the active one.
const IBUS_ASCII: &str = "xkb:us::eng";
/// Japanese ibus engines, tried in order the first time `ime:on` runs.
const IBUS_JAPANESE: &[&str] = &["mozc-jp", "anthy", "kkc", "skk"];

struct Backend<'a> {
    virt: VirtualDevice,
    /// Modifier sides the *downstream* consumer believes are held, i.e. the
    /// ones we actually forwarded. A swallowed modifier never reached it, so
    /// tracking the physical state instead would neutralise the wrong keys.
    held: [bool; MOD_SIDES.len()],
    config: &'a LinuxConfig,
    /// Probed once, not per keystroke.
    helper: Option<ImeHelper>,
    ibus_engine: Option<String>,
}

impl<'a> Backend<'a> {
    fn new(virt: VirtualDevice, config: &'a LinuxConfig) -> Backend<'a> {
        Backend {
            virt,
            held: [false; MOD_SIDES.len()],
            config,
            helper: None,
            ibus_engine: None,
        }
    }

    /// Write a batch to the virtual device. `emit` appends the SYN_REPORT that
    /// makes the kernel deliver it.
    fn emit(&mut self, events: &[InputEvent]) -> Result<()> {
        self.virt
            .emit(events)
            .context("writing to the kagi virtual keyboard")
    }

    /// Re-publish the events the rules let through, and remember what that
    /// does to the downstream modifier state.
    fn forward(&mut self, batch: &mut Vec<InputEvent>) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        for event in batch.iter() {
            if let Some(i) = MOD_SIDES
                .iter()
                .position(|(ev, _)| ev.code() == event.code())
            {
                // Autorepeat (value 2) leaves the key held.
                self.held[i] = event.value() != VALUE_UP;
            }
        }
        let events = std::mem::take(batch);
        let result = self.emit(&events);
        *batch = events;
        batch.clear();
        result
    }

    fn mod_held(&self, bit: Mods) -> bool {
        mod_sides().any(|(i, _, side)| self.held[i] && side == bit)
    }

    /// Release every modifier the consumer still believes is held. Called when
    /// a device disappears mid-chord: without this the desktop is left with a
    /// phantom Ctrl that no physical key can clear.
    fn release_modifiers(&mut self) -> Result<()> {
        let mut events = Vec::new();
        for (i, code, _) in mod_sides() {
            if self.held[i] {
                self.held[i] = false;
                events.push(key_event(code, VALUE_UP));
            }
        }
        if events.is_empty() {
            return Ok(());
        }
        self.emit(&events)
    }

    fn ime_helper(&mut self) -> Result<ImeHelper> {
        if let Some(helper) = self.helper {
            return Ok(helper);
        }
        let helper = if has_command("fcitx5-remote") {
            ImeHelper::Fcitx5
        } else if has_command("ibus") {
            ImeHelper::Ibus
        } else {
            bail!(
                "no IME helper found: neither `fcitx5-remote` nor `ibus` is on PATH\
                 \n  install fcitx5 or ibus, or set ime_on/ime_off/ime_toggle under [linux] \
                 in your config"
            );
        };
        self.helper = Some(helper);
        Ok(helper)
    }

    fn ibus_on(&mut self) -> Result<()> {
        if let Some(engine) = self.ibus_engine.clone() {
            return run_checked(&format!("ibus engine {}", shell_quote(&engine)));
        }
        for candidate in IBUS_JAPANESE {
            if run_blocking(&format!("ibus engine {candidate}")) {
                self.ibus_engine = Some((*candidate).to_string());
                return Ok(());
            }
        }
        bail!(
            "no Japanese ibus engine is installed (tried {})\
             \n  set ime_on under [linux] in your config, e.g. ime_on = \"ibus engine mozc-jp\"",
            IBUS_JAPANESE.join(", ")
        )
    }

    fn ibus(&mut self, state: ImeState) -> Result<()> {
        let on = match state {
            ImeState::On => true,
            ImeState::Off => false,
            // ibus has no toggle: ask it what is active and flip. An engine
            // name starting with `xkb` is a plain keyboard layout, i.e. off.
            ImeState::Toggle => capture("ibus engine")
                .map(|out| out.trim().starts_with("xkb"))
                .unwrap_or(false),
        };
        if on {
            self.ibus_on()
        } else {
            run_checked(&format!("ibus engine {IBUS_ASCII}"))
        }
    }
}

impl Emitter for Backend<'_> {
    /// Synthesize `chord` with exactly its own modifiers.
    ///
    /// The consumer derives modifier state from the event stream, so a
    /// physically held Ctrl would turn a synthetic Esc into Ctrl+Esc. Release
    /// the held-but-unwanted sides, press the wanted-but-unheld ones, tap, then
    /// put the physical state back exactly as it was.
    fn tap(&mut self, chord: Chord) -> Result<()> {
        let code = from_key(chord.key).ok_or_else(|| {
            anyhow!(
                "`{}` is a macOS-only key name with no evdev equivalent; \
                 use `ime:on`/`ime:off` or a JIS key such as `henkan`/`muhenkan` on Linux",
                chord.key.name()
            )
        })?;

        let mut prelude = Vec::new();
        let mut restore = Vec::new();
        for (i, code, bit) in mod_sides() {
            if self.held[i] && !chord.mods.contains(bit) {
                prelude.push(key_event(code, VALUE_UP));
                restore.push(key_event(code, VALUE_DOWN));
            }
        }
        for (bit, ev) in MOD_PREFERRED {
            if chord.mods.contains(bit) && !self.mod_held(bit) {
                prelude.push(key_event(ev.code(), VALUE_DOWN));
                restore.push(key_event(ev.code(), VALUE_UP));
            }
        }
        restore.reverse();

        if !prelude.is_empty() {
            self.emit(&prelude)?;
        }
        self.emit(&[key_event(code, VALUE_DOWN)])?;
        self.emit(&[key_event(code, VALUE_UP)])?;
        if !restore.is_empty() {
            self.emit(&restore)?;
        }
        // `self.held` is untouched on purpose: the physical state is back to
        // what it was, so the downstream view is unchanged.
        Ok(())
    }

    fn ime(&mut self, state: ImeState) -> Result<()> {
        let explicit = match state {
            ImeState::On => self.config.ime_on.as_deref(),
            ImeState::Off => self.config.ime_off.as_deref(),
            ImeState::Toggle => self.config.ime_toggle.as_deref(),
        };
        if let Some(cmd) = explicit {
            return run_checked(cmd);
        }
        match self.ime_helper()? {
            ImeHelper::Fcitx5 => {
                let flag = match state {
                    ImeState::On => "-o",
                    ImeState::Off => "-c",
                    ImeState::Toggle => "-t",
                };
                run_checked(&format!("fcitx5-remote {flag}"))
            }
            ImeHelper::Ibus => self.ibus(state),
        }
    }

    fn input_source(&mut self, id: &str) -> Result<()> {
        let cmd = match self.ime_helper()? {
            ImeHelper::Fcitx5 => format!("fcitx5-remote -s {}", shell_quote(id)),
            ImeHelper::Ibus => format!("ibus engine {}", shell_quote(id)),
        };
        run_checked(&cmd)
    }
}

fn has_command(name: &str) -> bool {
    run_blocking(&format!("command -v {name}"))
}

fn run_checked(cmd: &str) -> Result<()> {
    if run_blocking(cmd) {
        Ok(())
    } else {
        bail!("`{cmd}` failed")
    }
}

/// Capture stdout of a helper. `run_blocking` only reports the exit status,
/// and `ime:toggle` on ibus needs the active engine name.
fn capture(cmd: &str) -> Option<String> {
    let output = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Single-quote for `sh -c`; input source ids come from the config.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------- entry points

pub fn run(mut engine: Engine, config: &Config) -> Result<()> {
    let devices = select_devices(&config.linux.devices)?;

    // The virtual device must advertise every key it will ever emit: what kagi
    // can synthesize, plus everything the grabbed devices can produce, since
    // keys no rule mentions are forwarded verbatim. The kernel silently drops
    // events for codes a device did not declare.
    let mut keys = AttributeSet::<EvKey>::new();
    for (_, ev) in KEY_MAP {
        keys.insert(*ev);
    }
    for selected in &devices {
        if let Some(supported) = selected.device.supported_keys() {
            for key in supported.iter() {
                keys.insert(key);
            }
        }
    }
    let virt = VirtualDeviceBuilder::new()
        .map_err(uinput_error)?
        .name(VIRTUAL_NAME)
        .with_keys(&keys)
        .map_err(uinput_error)?
        .build()
        .map_err(uinput_error)?;

    let rx = spawn_readers(devices, true)?;
    eprintln!("kagi: running; Ctrl-C to stop");

    let mut backend = Backend::new(virt, &config.linux);
    pump(&mut engine, &mut backend, rx)
}

/// The decision loop: native event in, forward/swallow/act out.
///
/// Returns when every reader thread is gone (all devices unplugged); a Ctrl-C
/// tears the process down instead, and the kernel drops the grabs as our fds
/// close.
fn pump(engine: &mut Engine, backend: &mut Backend<'_>, rx: Receiver<Report>) -> Result<()> {
    let mut tracker = ModTracker::default();
    let mut pending: Packet = Vec::new();

    for report in rx {
        let packet = match report {
            Report::Keys(packet) => packet,
            Report::Lost(name) => {
                // Whatever was held on that keyboard can never be released by
                // the user again, and any half-matched rule is now stale.
                eprintln!("kagi: {name}: gone; releasing held modifiers");
                backend.release_modifiers()?;
                engine.reset();
                tracker.clear();
                continue;
            }
        };
        for event in packet {
            let down = event.value() != VALUE_UP;
            let Some(key) = to_key(event.code()) else {
                // Unknown to kagi: forward verbatim.
                pending.push(event);
                continue;
            };
            // The engine wants the state *excluding* this event's own key, so
            // sample before a press and after a release.
            let mods = if down {
                let mods = tracker.current();
                tracker.update(key, true);
                mods
            } else {
                tracker.update(key, false);
                tracker.current()
            };
            match engine.on_key(key, mods, down) {
                Decision::Pass => pending.push(event),
                Decision::Consume => {}
                Decision::Run { index, passthrough } => {
                    if passthrough {
                        pending.push(event);
                    }
                    // Flush first: the trigger must reach the app before the
                    // rule's own synthetic events.
                    backend.forward(&mut pending)?;
                    dispatch(backend, engine.actions(index))?;
                }
            }
        }
        backend.forward(&mut pending)?;
    }
    Ok(())
}

pub fn watch(config: &Config) -> Result<()> {
    let devices = select_devices(&config.linux.devices)?;
    // No grab and no virtual device: events reach the desktop normally and the
    // terminal keeps working while you hunt for a key name.
    let rx = spawn_readers(devices, false)?;
    eprintln!("kagi: press keys to see their names; Ctrl-C to stop\n");

    for report in rx {
        let packet = match report {
            Report::Keys(packet) => packet,
            Report::Lost(name) => {
                eprintln!("kagi: {name}: gone");
                continue;
            }
        };
        for event in packet {
            let kind = match event.value() {
                VALUE_UP => "up",
                VALUE_REPEAT => "rep",
                _ => "down",
            };
            let name = to_key(event.code()).map_or("unknown", |k| k.name());
            println!("{kind:<4}  {name:<18}  (code={})", event.code());
        }
    }
    Ok(())
}
