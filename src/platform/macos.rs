//! macOS backend: CGEventTap for capture, Text Input Services + the JIS
//! 英数/かな keycodes for IME control.
//!
//! No kernel extension and no DriverKit driver: an event tap at
//! `kCGHIDEventTap` only needs Accessibility (and, on recent releases, Input
//! Monitoring) permission. The trade-off versus a HID-level driver is that
//! taps are bypassed while a secure input field has focus, and are not active
//! at the login window.
//!
//! `core_graphics::event::CGEventTap` cannot be used: its callback shim maps a
//! `None` return back to the *original* event, so a rule could never swallow a
//! key. The documented contract is "return NULL to delete the event from the
//! stream", which needs the raw callback — hence the FFI below.

use crate::action::ImeState;
use crate::config::{Config, MacImeMethod, MacosConfig};
use crate::engine::{Decision, Engine};
use crate::keys::{Chord, Key, Mods};
use crate::platform::{dispatch, Emitter};
use anyhow::{anyhow, bail, Result};
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource};
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{Boolean, CFRelease, CFTypeRef, OSStatus};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::mach_port::CFMachPortCreateRunLoopSource;
use core_foundation_sys::string::CFStringRef;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::sys::CGEventRef;
use foreign_types::ForeignType;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;

/// Stamped into `kCGEventSourceUserData` on every event we synthesize so the
/// tap can recognise its own output instead of reprocessing it.
const KAGI_MARK: i64 = 0x6b_61_67_69; // "kagi"

const KCG_EVENT_KEY_DOWN: u32 = 10;
const KCG_EVENT_KEY_UP: u32 = 11;
const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const KCG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

const KCG_HID_EVENT_TAP: u32 = 0;
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KCG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

type CGEventTapProxy = *const c_void;
type CGEventMask = u64;
type TapCallback = unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void)
    -> CGEventRef;
type TISInputSourceRef = CFTypeRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: CGEventMask,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> Boolean;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all: Boolean) -> CFArrayRef;
    fn TISSelectInputSource(source: TISInputSourceRef) -> OSStatus;
    fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *mut c_void;
    static kTISPropertyInputSourceID: CFStringRef;
}

// ---------------------------------------------------------------------------
// Key codes
// ---------------------------------------------------------------------------

/// macOS virtual keycode -> `Key`.
fn to_key(code: u16) -> Option<Key> {
    Some(match code {
        0x00 => Key::A, 0x01 => Key::S, 0x02 => Key::D, 0x03 => Key::F,
        0x04 => Key::H, 0x05 => Key::G, 0x06 => Key::Z, 0x07 => Key::X,
        0x08 => Key::C, 0x09 => Key::V, 0x0B => Key::B, 0x0C => Key::Q,
        0x0D => Key::W, 0x0E => Key::E, 0x0F => Key::R, 0x10 => Key::Y,
        0x11 => Key::T,

        0x12 => Key::Num1, 0x13 => Key::Num2, 0x14 => Key::Num3,
        0x15 => Key::Num4, 0x16 => Key::Num6, 0x17 => Key::Num5,
        0x19 => Key::Num9, 0x1A => Key::Num7, 0x1C => Key::Num8,
        0x1D => Key::Num0,

        0x18 => Key::Equal, 0x1B => Key::Minus,
        0x1E => Key::RightBracket, 0x21 => Key::LeftBracket,
        0x1F => Key::O, 0x20 => Key::U, 0x22 => Key::I, 0x23 => Key::P,
        0x24 => Key::Enter, 0x25 => Key::L, 0x26 => Key::J,
        0x27 => Key::Quote, 0x28 => Key::K, 0x29 => Key::Semicolon,
        0x2A => Key::Backslash, 0x2B => Key::Comma, 0x2C => Key::Slash,
        0x2D => Key::N, 0x2E => Key::M, 0x2F => Key::Period,

        0x30 => Key::Tab, 0x31 => Key::Space, 0x32 => Key::Grave,
        0x33 => Key::Backspace, 0x35 => Key::Escape,

        0x36 => Key::RightMeta, 0x37 => Key::LeftMeta,
        0x38 => Key::LeftShift, 0x39 => Key::CapsLock,
        0x3A => Key::LeftAlt, 0x3B => Key::LeftCtrl,
        0x3C => Key::RightShift, 0x3D => Key::RightAlt, 0x3E => Key::RightCtrl,

        0x40 => Key::F17, 0x4F => Key::F18, 0x50 => Key::F19, 0x5A => Key::F20,
        0x60 => Key::F5, 0x61 => Key::F6, 0x62 => Key::F7, 0x63 => Key::F3,
        0x64 => Key::F8, 0x65 => Key::F9, 0x67 => Key::F11, 0x69 => Key::F13,
        0x6A => Key::F16, 0x6B => Key::F14, 0x6D => Key::F10, 0x6F => Key::F12,
        0x71 => Key::F15, 0x76 => Key::F4, 0x78 => Key::F2, 0x7A => Key::F1,

        0x41 => Key::NumpadDot, 0x43 => Key::NumpadMultiply,
        0x45 => Key::NumpadPlus, 0x47 => Key::NumLock,
        0x4B => Key::NumpadDivide, 0x4C => Key::NumpadEnter,
        0x4E => Key::NumpadMinus, 0x51 => Key::NumpadEqual,
        0x52 => Key::Numpad0, 0x53 => Key::Numpad1, 0x54 => Key::Numpad2,
        0x55 => Key::Numpad3, 0x56 => Key::Numpad4, 0x57 => Key::Numpad5,
        0x58 => Key::Numpad6, 0x59 => Key::Numpad7, 0x5B => Key::Numpad8,
        0x5C => Key::Numpad9,

        0x5D => Key::IntlYen, 0x5E => Key::IntlRo,
        0x66 => Key::Eisu, 0x68 => Key::Kana,

        0x72 => Key::Insert, 0x73 => Key::Home, 0x74 => Key::PageUp,
        0x75 => Key::Delete, 0x77 => Key::End, 0x79 => Key::PageDown,
        0x7B => Key::Left, 0x7C => Key::Right, 0x7D => Key::Down, 0x7E => Key::Up,

        _ => return None,
    })
}

/// `Key` -> macOS virtual keycode. `None` for keys the platform has no code
/// for (Windows/Linux-only names such as 変換 / 無変換).
fn from_key(key: Key) -> Option<u16> {
    Some(match key {
        Key::A => 0x00, Key::S => 0x01, Key::D => 0x02, Key::F => 0x03,
        Key::H => 0x04, Key::G => 0x05, Key::Z => 0x06, Key::X => 0x07,
        Key::C => 0x08, Key::V => 0x09, Key::B => 0x0B, Key::Q => 0x0C,
        Key::W => 0x0D, Key::E => 0x0E, Key::R => 0x0F, Key::Y => 0x10,
        Key::T => 0x11,

        Key::Num1 => 0x12, Key::Num2 => 0x13, Key::Num3 => 0x14,
        Key::Num4 => 0x15, Key::Num6 => 0x16, Key::Num5 => 0x17,
        Key::Num9 => 0x19, Key::Num7 => 0x1A, Key::Num8 => 0x1C,
        Key::Num0 => 0x1D,

        Key::Equal => 0x18, Key::Minus => 0x1B,
        Key::RightBracket => 0x1E, Key::LeftBracket => 0x21,
        Key::O => 0x1F, Key::U => 0x20, Key::I => 0x22, Key::P => 0x23,
        Key::Enter => 0x24, Key::L => 0x25, Key::J => 0x26,
        Key::Quote => 0x27, Key::K => 0x28, Key::Semicolon => 0x29,
        Key::Backslash => 0x2A, Key::Comma => 0x2B, Key::Slash => 0x2C,
        Key::N => 0x2D, Key::M => 0x2E, Key::Period => 0x2F,

        Key::Tab => 0x30, Key::Space => 0x31, Key::Grave => 0x32,
        Key::Backspace => 0x33, Key::Escape => 0x35,

        Key::RightMeta => 0x36, Key::LeftMeta => 0x37,
        Key::LeftShift => 0x38, Key::CapsLock => 0x39,
        Key::LeftAlt => 0x3A, Key::LeftCtrl => 0x3B,
        Key::RightShift => 0x3C, Key::RightAlt => 0x3D, Key::RightCtrl => 0x3E,

        Key::F17 => 0x40, Key::F18 => 0x4F, Key::F19 => 0x50, Key::F20 => 0x5A,
        Key::F5 => 0x60, Key::F6 => 0x61, Key::F7 => 0x62, Key::F3 => 0x63,
        Key::F8 => 0x64, Key::F9 => 0x65, Key::F11 => 0x67, Key::F13 => 0x69,
        Key::F16 => 0x6A, Key::F14 => 0x6B, Key::F10 => 0x6D, Key::F12 => 0x6F,
        Key::F15 => 0x71, Key::F4 => 0x76, Key::F2 => 0x78, Key::F1 => 0x7A,

        Key::NumpadDot => 0x41, Key::NumpadMultiply => 0x43,
        Key::NumpadPlus => 0x45, Key::NumLock => 0x47,
        Key::NumpadDivide => 0x4B, Key::NumpadEnter => 0x4C,
        Key::NumpadMinus => 0x4E, Key::NumpadEqual => 0x51,
        Key::Numpad0 => 0x52, Key::Numpad1 => 0x53, Key::Numpad2 => 0x54,
        Key::Numpad3 => 0x55, Key::Numpad4 => 0x56, Key::Numpad5 => 0x57,
        Key::Numpad6 => 0x58, Key::Numpad7 => 0x59, Key::Numpad8 => 0x5B,
        Key::Numpad9 => 0x5C,

        Key::IntlYen => 0x5D, Key::IntlRo => 0x5E,
        Key::Eisu => 0x66, Key::Kana => 0x68,

        Key::Insert => 0x72, Key::Home => 0x73, Key::PageUp => 0x74,
        Key::Delete => 0x75, Key::End => 0x77, Key::PageDown => 0x79,
        Key::Left => 0x7B, Key::Right => 0x7C, Key::Down => 0x7D, Key::Up => 0x7E,

        _ => return None,
    })
}

fn mods_from_flags(flags: CGEventFlags) -> Mods {
    let mut mods = Mods::empty();
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        mods |= Mods::CTRL;
    }
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        mods |= Mods::SHIFT;
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        mods |= Mods::ALT;
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        mods |= Mods::META;
    }
    mods
}

fn flags_from_mods(mods: Mods) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if mods.contains(Mods::CTRL) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if mods.contains(Mods::SHIFT) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if mods.contains(Mods::ALT) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if mods.contains(Mods::META) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

// ---------------------------------------------------------------------------
// Input sources
// ---------------------------------------------------------------------------

fn current_input_source_id() -> Option<String> {
    // SAFETY: TISCopyCurrentKeyboardInputSource follows the Copy rule, so the
    // result is owned here and released below; the property is a Get-rule
    // CFStringRef borrowed from it and only read while the owner is alive.
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return None;
        }
        let id_ref = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) as CFStringRef;
        let id = if id_ref.is_null() {
            None
        } else {
            Some(CFString::wrap_under_get_rule(id_ref).to_string())
        };
        CFRelease(source);
        id
    }
}

/// Whether the current input source composes Japanese text.
///
/// Plain keyboard layouts (`com.apple.keylayout.*`) and the Roman modes of
/// Japanese input methods are the "IME off" states; everything else composes.
fn ime_is_on() -> bool {
    match current_input_source_id() {
        Some(id) => !(id.starts_with("com.apple.keylayout.") || id.ends_with(".Roman")),
        None => false,
    }
}

fn select_input_source(id: &str) -> Result<()> {
    let key = // SAFETY: the Carbon global is a constant CFStringRef, Get rule.
        unsafe { CFString::wrap_under_get_rule(kTISPropertyInputSourceID) };
    let props = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFString::new(id).as_CFType())]);

    // SAFETY: TISCreateInputSourceList follows the Create rule; the array and
    // its elements stay valid until the CFRelease below.
    unsafe {
        let list = TISCreateInputSourceList(props.as_concrete_TypeRef(), 0);
        if list.is_null() {
            bail!("no input source matches `{id}`");
        }
        let count = CFArrayGetCount(list);
        if count == 0 {
            CFRelease(list as CFTypeRef);
            bail!("no input source matches `{id}`");
        }
        let source = CFArrayGetValueAtIndex(list, 0) as TISInputSourceRef;
        let status = TISSelectInputSource(source);
        CFRelease(list as CFTypeRef);
        if status != 0 {
            bail!("TISSelectInputSource(`{id}`) failed with OSStatus {status}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

struct MacEmitter {
    source: CGEventSource,
    cfg: MacosConfig,
}

impl MacEmitter {
    fn new(cfg: MacosConfig) -> Result<MacEmitter> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow!("CGEventSourceCreate failed"))?;
        Ok(MacEmitter { source, cfg })
    }

    /// Post a key down+up with exactly `flags`, marked as ours.
    fn post(&self, code: u16, flags: CGEventFlags) -> Result<()> {
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(self.source.clone(), code, down)
                .map_err(|()| anyhow!("CGEventCreateKeyboardEvent failed"))?;
            event.set_flags(flags);
            event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, KAGI_MARK);
            event.post(CGEventTapLocation::HID);
        }
        Ok(())
    }
}

impl Emitter for MacEmitter {
    fn tap(&mut self, chord: Chord) -> Result<()> {
        let code = from_key(chord.key).ok_or_else(|| {
            anyhow!("`{}` has no macOS keycode; it is a Windows/Linux-only key", chord.key)
        })?;
        // Flags travel inside the synthetic event, so a physically held
        // modifier cannot contaminate it the way it would on Windows/Linux.
        self.post(code, flags_from_mods(chord.mods))
    }

    fn ime(&mut self, state: ImeState) -> Result<()> {
        let on = match state {
            ImeState::On => true,
            ImeState::Off => false,
            ImeState::Toggle => !ime_is_on(),
        };
        match self.cfg.ime {
            // 英数 / かな: what a JIS keyboard sends. Every Japanese IME
            // honours these, and the selected input method is preserved.
            MacImeMethod::Eisu => {
                let code = if on { 0x68 } else { 0x66 };
                self.post(code, CGEventFlags::CGEventFlagNull)
            }
            MacImeMethod::Source => {
                let id = if on { &self.cfg.japanese_source } else { &self.cfg.ascii_source };
                select_input_source(id)
            }
        }
    }

    fn input_source(&mut self, id: &str) -> Result<()> {
        select_input_source(id)
    }
}

// ---------------------------------------------------------------------------
// Tap
// ---------------------------------------------------------------------------

struct Context {
    engine: Engine,
    emitter: MacEmitter,
    tap: CFMachPortRef,
    /// Keys whose key-down was rewritten in place. The matching key-up must be
    /// rewritten the same way, or the focused app is left holding a key that
    /// never comes back up.
    rewritten: HashMap<Key, (u16, CGEventFlags)>,
}

/// What the tap callback should do with the in-flight event.
enum Verdict {
    Keep,
    Delete,
}

impl Context {
    fn handle(&mut self, etype: u32, event: &CGEvent) -> Verdict {
        if etype == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT
            || etype == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT
        {
            // The system disables a tap that took too long or when the user
            // forcibly re-enabled input; nothing arrives until it is re-armed.
            // SAFETY: `self.tap` is the port returned by CGEventTapCreate and
            // is kept alive for the process lifetime.
            unsafe { CGEventTapEnable(self.tap, true) };
            self.engine.reset();
            self.rewritten.clear();
            eprintln!("kagi: event tap was disabled by the system; re-enabled");
            return Verdict::Keep;
        }

        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == KAGI_MARK {
            return Verdict::Keep;
        }

        let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
        let Some(key) = to_key(code) else {
            return Verdict::Keep;
        };
        let flags = event.get_flags();
        let mut mods = mods_from_flags(flags);

        let down = match etype {
            KCG_EVENT_KEY_DOWN => true,
            KCG_EVENT_KEY_UP => false,
            KCG_EVENT_FLAGS_CHANGED => {
                // A FlagsChanged event already carries the post-transition
                // state, so the key's own bit says whether it went down.
                match key.mod_bit() {
                    Some(bit) => {
                        let pressed = mods.contains(bit);
                        mods.remove(bit);
                        pressed
                    }
                    // Caps Lock reports through FlagsChanged too.
                    None => flags.contains(CGEventFlags::CGEventFlagAlphaShift),
                }
            }
            _ => return Verdict::Keep,
        };

        if !down {
            if let Some((new_code, new_flags)) = self.rewritten.remove(&key) {
                rewrite(event, new_code, new_flags);
                self.engine.on_key(key, mods, false);
                return Verdict::Keep;
            }
        }

        match self.engine.on_key(key, mods, down) {
            Decision::Pass => Verdict::Keep,
            Decision::Consume => Verdict::Delete,
            Decision::Run { index, passthrough } => {
                let actions = self.engine.actions(index);
                // Rewriting the in-flight event beats posting a fresh one:
                // ordering with the original key-down is guaranteed and no
                // held modifier can leak into it.
                let rewrite_first = !passthrough
                    && down
                    && matches!(actions.first(), Some(crate::action::Action::Tap(_)));

                let (verdict, rest) = if rewrite_first {
                    let Some(crate::action::Action::Tap(chord)) = actions.first() else {
                        unreachable!("guarded by rewrite_first")
                    };
                    match from_key(chord.key) {
                        Some(new_code) => {
                            let new_flags = flags_from_mods(chord.mods);
                            rewrite(event, new_code, new_flags);
                            self.rewritten.insert(key, (new_code, new_flags));
                            (Verdict::Keep, &actions[1..])
                        }
                        None => {
                            eprintln!("kagi: `{}` has no macOS keycode", chord.key);
                            (Verdict::Delete, &actions[1..])
                        }
                    }
                } else {
                    (if passthrough { Verdict::Keep } else { Verdict::Delete }, actions)
                };

                if !rest.is_empty() {
                    let rest = rest.to_vec();
                    if let Err(e) = dispatch(&mut self.emitter, &rest) {
                        eprintln!("kagi: action failed: {e:#}");
                    }
                }
                verdict
            }
        }
    }
}

fn rewrite(event: &CGEvent, code: u16, flags: CGEventFlags) {
    event.set_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE, code as i64);
    event.set_flags(flags);
}

/// SAFETY: invoked by CoreGraphics on the run loop thread that installed the
/// tap. `user_info` is the leaked `Context` from `install`, so it outlives
/// every callback, and CoreGraphics serialises callbacks on that one thread,
/// making the `&mut` unique.
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event_ref: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let ctx = &mut *(user_info as *mut Context);
    // Borrowed, not owned: CoreGraphics still holds a reference, so the
    // wrapper must not run CFRelease on drop.
    let event = ManuallyDrop::new(CGEvent::from_ptr(event_ref));
    match ctx.handle(etype, &event) {
        Verdict::Keep => event_ref,
        // Returning NULL deletes the event from the stream.
        Verdict::Delete => ptr::null_mut(),
    }
}

fn event_mask() -> CGEventMask {
    (1 << KCG_EVENT_KEY_DOWN) | (1 << KCG_EVENT_KEY_UP) | (1 << KCG_EVENT_FLAGS_CHANGED)
}

/// Create the tap, attach it to the current run loop and block forever.
fn install(ctx: Box<Context>, options: u32, callback: TapCallback) -> Result<()> {
    // The context must outlive every callback; the run loop below never
    // returns, so leaking it is the lifetime.
    let ctx = Box::into_raw(ctx);

    // SAFETY: `callback` matches the CGEventTapCallBack signature and `ctx`
    // is a valid, leaked pointer for the process lifetime.
    let port = unsafe {
        CGEventTapCreate(
            KCG_HID_EVENT_TAP,
            KCG_HEAD_INSERT_EVENT_TAP,
            options,
            event_mask(),
            callback,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        // SAFETY: plain FFI call, no arguments.
        let trusted = unsafe { AXIsProcessTrusted() } != 0;
        bail!(
            "CGEventTapCreate failed (accessibility trusted: {trusted}).\n\
             Grant the binary — or the terminal launching it — access under\n\
             System Settings > Privacy & Security > Accessibility, and\n\
             > Input Monitoring, then run kagi again."
        );
    }
    // SAFETY: `ctx` is the leaked pointer above; `port` is owned by us now.
    unsafe { (*ctx).tap = port };

    // SAFETY: `port` is a valid CFMachPort; the source is created (+1) and
    // handed to the run loop, which retains it.
    let source = unsafe {
        let s = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
        if s.is_null() {
            bail!("CFMachPortCreateRunLoopSource failed");
        }
        CFRunLoopSource::wrap_under_create_rule(s)
    };

    let run_loop = CFRunLoop::get_current();
    // SAFETY: `kCFRunLoopCommonModes` is a CoreFoundation constant.
    unsafe { run_loop.add_source(&source, kCFRunLoopCommonModes) };
    // SAFETY: `port` stays alive for the process lifetime.
    unsafe { CGEventTapEnable(port, true) };

    CFRunLoop::run_current();
    Ok(())
}

fn context(engine: Engine, config: &Config) -> Result<Box<Context>> {
    Ok(Box::new(Context {
        engine,
        emitter: MacEmitter::new(config.macos.clone())?,
        tap: ptr::null_mut(),
        rewritten: HashMap::new(),
    }))
}

pub fn run(engine: Engine, config: &Config) -> Result<()> {
    eprintln!("kagi: running (macOS event tap). Ctrl-C to stop.");
    install(context(engine, config)?, KCG_EVENT_TAP_OPTION_DEFAULT, tap_callback)
}

pub fn watch(config: &Config) -> Result<()> {
    // A listen-only tap cannot alter or delete events, so `watch_callback`
    // only prints; the ruleless engine in the context is never consulted.
    eprintln!("kagi: watching (listen-only tap). Ctrl-C to stop.");
    install(
        context(Engine::new(Vec::new()), config)?,
        KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
        watch_callback,
    )
}

/// SAFETY: same run-loop-thread contract as `tap_callback`.
unsafe extern "C" fn watch_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event_ref: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let ctx = &mut *(user_info as *mut Context);
    let event = ManuallyDrop::new(CGEvent::from_ptr(event_ref));

    if etype == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || etype == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        CGEventTapEnable(ctx.tap, true);
        return event_ref;
    }

    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let flags = event.get_flags();
    let mods = mods_from_flags(flags);
    let name = to_key(code).map(|k| k.name()).unwrap_or("unknown");
    let phase = match etype {
        KCG_EVENT_KEY_DOWN => "down",
        KCG_EVENT_KEY_UP => "up  ",
        KCG_EVENT_FLAGS_CHANGED => "flag",
        _ => return event_ref,
    };
    let prefix = if mods.is_empty() { String::new() } else { format!("{mods}-") };
    println!("{phase}  {prefix}{name:<18} (keycode=0x{code:02X} flags=0x{:X})", flags.bits());
    event_ref
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycodes_round_trip() {
        for code in 0u16..0x80 {
            if let Some(key) = to_key(code) {
                assert_eq!(from_key(key), Some(code), "keycode 0x{code:02X} -> {key}");
            }
        }
    }

    #[test]
    fn ahk_trigger_keys_exist_on_macos() {
        assert_eq!(from_key(Key::LeftBracket), Some(0x21));
        assert_eq!(from_key(Key::Escape), Some(0x35));
        assert_eq!(from_key(Key::Eisu), Some(0x66));
        assert_eq!(from_key(Key::Kana), Some(0x68));
    }

    #[test]
    fn windows_only_keys_report_absent() {
        assert_eq!(from_key(Key::Henkan), None);
        assert_eq!(from_key(Key::Muhenkan), None);
    }

    #[test]
    fn mods_survive_the_flag_round_trip() {
        for mods in [Mods::empty(), Mods::CTRL, Mods::CTRL | Mods::SHIFT, Mods::all()] {
            assert_eq!(mods_from_flags(flags_from_mods(mods)), mods);
        }
    }
}
