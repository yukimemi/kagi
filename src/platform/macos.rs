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
use crate::platform::{Emitter, dispatch};
use anyhow::{Context as _, Result, anyhow, bail};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
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
use std::path::{Path, PathBuf};
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
type TapCallback =
    unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;
type TISInputSourceRef = CFTypeRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
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
unsafe extern "C" {
    /// Read-only: does not prompt, does not register kagi in any list.
    fn AXIsProcessTrusted() -> Boolean;
    /// The prompting variant. Beyond showing the dialog, it is what makes the
    /// caller *appear* in the Accessibility list — an entry you otherwise have
    /// to add by hand with the file picker.
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

/// `kIOHIDRequestTypeListenEvent` — reading other processes' key events, which
/// is what an event tap does.
const KIOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
/// `kIOHIDAccessTypeGranted`.
const KIOHID_ACCESS_TYPE_GRANTED: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
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
        0x00 => Key::A,
        0x01 => Key::S,
        0x02 => Key::D,
        0x03 => Key::F,
        0x04 => Key::H,
        0x05 => Key::G,
        0x06 => Key::Z,
        0x07 => Key::X,
        0x08 => Key::C,
        0x09 => Key::V,
        0x0B => Key::B,
        0x0C => Key::Q,
        0x0D => Key::W,
        0x0E => Key::E,
        0x0F => Key::R,
        0x10 => Key::Y,
        0x11 => Key::T,

        0x12 => Key::Num1,
        0x13 => Key::Num2,
        0x14 => Key::Num3,
        0x15 => Key::Num4,
        0x16 => Key::Num6,
        0x17 => Key::Num5,
        0x19 => Key::Num9,
        0x1A => Key::Num7,
        0x1C => Key::Num8,
        0x1D => Key::Num0,

        0x18 => Key::Equal,
        0x1B => Key::Minus,
        0x1E => Key::RightBracket,
        0x21 => Key::LeftBracket,
        0x1F => Key::O,
        0x20 => Key::U,
        0x22 => Key::I,
        0x23 => Key::P,
        0x24 => Key::Enter,
        0x25 => Key::L,
        0x26 => Key::J,
        0x27 => Key::Quote,
        0x28 => Key::K,
        0x29 => Key::Semicolon,
        0x2A => Key::Backslash,
        0x2B => Key::Comma,
        0x2C => Key::Slash,
        0x2D => Key::N,
        0x2E => Key::M,
        0x2F => Key::Period,

        0x30 => Key::Tab,
        0x31 => Key::Space,
        0x32 => Key::Grave,
        0x33 => Key::Backspace,
        0x35 => Key::Escape,

        0x36 => Key::RightMeta,
        0x37 => Key::LeftMeta,
        0x38 => Key::LeftShift,
        0x39 => Key::CapsLock,
        0x3A => Key::LeftAlt,
        0x3B => Key::LeftCtrl,
        0x3C => Key::RightShift,
        0x3D => Key::RightAlt,
        0x3E => Key::RightCtrl,

        0x40 => Key::F17,
        0x4F => Key::F18,
        0x50 => Key::F19,
        0x5A => Key::F20,
        0x60 => Key::F5,
        0x61 => Key::F6,
        0x62 => Key::F7,
        0x63 => Key::F3,
        0x64 => Key::F8,
        0x65 => Key::F9,
        0x67 => Key::F11,
        0x69 => Key::F13,
        0x6A => Key::F16,
        0x6B => Key::F14,
        0x6D => Key::F10,
        0x6F => Key::F12,
        0x71 => Key::F15,
        0x76 => Key::F4,
        0x78 => Key::F2,
        0x7A => Key::F1,

        0x41 => Key::NumpadDot,
        0x43 => Key::NumpadMultiply,
        0x45 => Key::NumpadPlus,
        0x47 => Key::NumLock,
        0x4B => Key::NumpadDivide,
        0x4C => Key::NumpadEnter,
        0x4E => Key::NumpadMinus,
        0x51 => Key::NumpadEqual,
        0x52 => Key::Numpad0,
        0x53 => Key::Numpad1,
        0x54 => Key::Numpad2,
        0x55 => Key::Numpad3,
        0x56 => Key::Numpad4,
        0x57 => Key::Numpad5,
        0x58 => Key::Numpad6,
        0x59 => Key::Numpad7,
        0x5B => Key::Numpad8,
        0x5C => Key::Numpad9,

        0x5D => Key::IntlYen,
        0x5E => Key::IntlRo,
        0x66 => Key::Eisu,
        0x68 => Key::Kana,

        0x72 => Key::Insert,
        0x73 => Key::Home,
        0x74 => Key::PageUp,
        0x75 => Key::Delete,
        0x77 => Key::End,
        0x79 => Key::PageDown,
        0x7B => Key::Left,
        0x7C => Key::Right,
        0x7D => Key::Down,
        0x7E => Key::Up,

        _ => return None,
    })
}

/// `Key` -> macOS virtual keycode. `None` for keys the platform has no code
/// for (Windows/Linux-only names such as 変換 / 無変換).
fn from_key(key: Key) -> Option<u16> {
    Some(match key {
        Key::A => 0x00,
        Key::S => 0x01,
        Key::D => 0x02,
        Key::F => 0x03,
        Key::H => 0x04,
        Key::G => 0x05,
        Key::Z => 0x06,
        Key::X => 0x07,
        Key::C => 0x08,
        Key::V => 0x09,
        Key::B => 0x0B,
        Key::Q => 0x0C,
        Key::W => 0x0D,
        Key::E => 0x0E,
        Key::R => 0x0F,
        Key::Y => 0x10,
        Key::T => 0x11,

        Key::Num1 => 0x12,
        Key::Num2 => 0x13,
        Key::Num3 => 0x14,
        Key::Num4 => 0x15,
        Key::Num6 => 0x16,
        Key::Num5 => 0x17,
        Key::Num9 => 0x19,
        Key::Num7 => 0x1A,
        Key::Num8 => 0x1C,
        Key::Num0 => 0x1D,

        Key::Equal => 0x18,
        Key::Minus => 0x1B,
        Key::RightBracket => 0x1E,
        Key::LeftBracket => 0x21,
        Key::O => 0x1F,
        Key::U => 0x20,
        Key::I => 0x22,
        Key::P => 0x23,
        Key::Enter => 0x24,
        Key::L => 0x25,
        Key::J => 0x26,
        Key::Quote => 0x27,
        Key::K => 0x28,
        Key::Semicolon => 0x29,
        Key::Backslash => 0x2A,
        Key::Comma => 0x2B,
        Key::Slash => 0x2C,
        Key::N => 0x2D,
        Key::M => 0x2E,
        Key::Period => 0x2F,

        Key::Tab => 0x30,
        Key::Space => 0x31,
        Key::Grave => 0x32,
        Key::Backspace => 0x33,
        Key::Escape => 0x35,

        Key::RightMeta => 0x36,
        Key::LeftMeta => 0x37,
        Key::LeftShift => 0x38,
        Key::CapsLock => 0x39,
        Key::LeftAlt => 0x3A,
        Key::LeftCtrl => 0x3B,
        Key::RightShift => 0x3C,
        Key::RightAlt => 0x3D,
        Key::RightCtrl => 0x3E,

        Key::F17 => 0x40,
        Key::F18 => 0x4F,
        Key::F19 => 0x50,
        Key::F20 => 0x5A,
        Key::F5 => 0x60,
        Key::F6 => 0x61,
        Key::F7 => 0x62,
        Key::F3 => 0x63,
        Key::F8 => 0x64,
        Key::F9 => 0x65,
        Key::F11 => 0x67,
        Key::F13 => 0x69,
        Key::F16 => 0x6A,
        Key::F14 => 0x6B,
        Key::F10 => 0x6D,
        Key::F12 => 0x6F,
        Key::F15 => 0x71,
        Key::F4 => 0x76,
        Key::F2 => 0x78,
        Key::F1 => 0x7A,

        Key::NumpadDot => 0x41,
        Key::NumpadMultiply => 0x43,
        Key::NumpadPlus => 0x45,
        Key::NumLock => 0x47,
        Key::NumpadDivide => 0x4B,
        Key::NumpadEnter => 0x4C,
        Key::NumpadMinus => 0x4E,
        Key::NumpadEqual => 0x51,
        Key::Numpad0 => 0x52,
        Key::Numpad1 => 0x53,
        Key::Numpad2 => 0x54,
        Key::Numpad3 => 0x55,
        Key::Numpad4 => 0x56,
        Key::Numpad5 => 0x57,
        Key::Numpad6 => 0x58,
        Key::Numpad7 => 0x59,
        Key::Numpad8 => 0x5B,
        Key::Numpad9 => 0x5C,

        Key::IntlYen => 0x5D,
        Key::IntlRo => 0x5E,
        Key::Eisu => 0x66,
        Key::Kana => 0x68,

        Key::Insert => 0x72,
        Key::Home => 0x73,
        Key::PageUp => 0x74,
        Key::Delete => 0x75,
        Key::End => 0x77,
        Key::PageDown => 0x79,
        Key::Left => 0x7B,
        Key::Right => 0x7C,
        Key::Down => 0x7D,
        Key::Up => 0x7E,

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
    let props =
        CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFString::new(id).as_CFType())]);

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
            anyhow!(
                "`{}` has no macOS keycode; it is a Windows/Linux-only key",
                chord.key
            )
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
                let id = if on {
                    &self.cfg.japanese_source
                } else {
                    &self.cfg.ascii_source
                };
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
                    (
                        if passthrough {
                            Verdict::Keep
                        } else {
                            Verdict::Delete
                        },
                        actions,
                    )
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
    // SAFETY: `user_info` is the leaked `Context`; CoreGraphics serialises
    // callbacks on the installing thread, so this `&mut` is unique.
    let ctx = unsafe { &mut *(user_info as *mut Context) };
    // Borrowed, not owned: CoreGraphics still holds a reference, so the
    // wrapper must not run CFRelease on drop.
    // SAFETY: `event_ref` is the live event CoreGraphics passed in.
    let event = ManuallyDrop::new(unsafe { CGEvent::from_ptr(event_ref) });
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
        // Registering in the two Privacy lists is the whole difficulty, and
        // this is the moment we know it is needed — so ask instead of only
        // printing instructions. `Once` because a KeepAlive agent respawns on
        // every failure and must not pile dialogs on top of the settings
        // window the user is working in.
        let ok = request_permissions(Prompt::Once)?;
        bail!(
            "CGEventTapCreate failed.\n\
             {}\n\
             kagi is registered under System Settings > Privacy & Security >\n\
             Accessibility and > Input Monitoring. Tick it in both, then run\n\
             `kagi service start`. `kagi permissions` re-opens the panes and\n\
             asks again.",
            if ok {
                "Both permissions report as granted, which normally means they were\n\
                 granted to a previous build of this binary: the grant is keyed to\n\
                 the binary's contents, so toggle each entry off and on again."
            } else {
                "Neither permission is granted yet."
            }
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

/// Whether a permission check may interact with the user.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    /// Explicit request: always prompt and open the settings panes.
    Always,
    /// Automatic request from a failing start. Interacts only if the last
    /// automatic interaction was more than [`THROTTLE`] ago, because a
    /// launchd agent with `KeepAlive` respawns every few seconds on failure —
    /// interacting unconditionally would bury the screen in dialogs and
    /// re-open the settings pane out from under someone mid-click.
    Once,
}

/// Minimum gap between automatic (`Prompt::Once`) interactions.
///
/// Short enough that a grant revoked outside of a click — an OS update, or
/// (observed) a reboot invalidating an ad-hoc-signed binary's Accessibility
/// and Input Monitoring grants — is retried again soon after the next
/// restart, without turning a `KeepAlive` respawn loop into a dialog storm.
const THROTTLE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Path recording the last time an automatic permission check interacted
/// with the user (prompted, or opened a settings pane).
fn throttle_marker() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("kagi")
            .join("permission-check"),
    )
}

/// Whether this call may interact with the user, and record that it did.
fn may_interact(prompt: Prompt) -> bool {
    if prompt == Prompt::Always {
        return true;
    }
    let Some(marker) = throttle_marker() else {
        return true;
    };
    let stale = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age >= THROTTLE);
    let due = stale || !marker.exists();
    if due {
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&marker, b"");
    }
    due
}

/// Signing identifier kagi pins itself to.
///
/// Not `com.yukimemi.kagi`: that string was requested from this machine
/// several times during development *before* the signing pipeline below was
/// fully correct (the `-r=` flag-syntax bug, in particular, left many of
/// those requests running against a mismatched/invalid signature). TCC
/// records a decision (denied, in this case) the first time an identifier is
/// ever asked about and never reconsiders it — `IOHIDRequestAccess` on an
/// already-decided identity shows no dialog *and never creates a Privacy
/// pane row*, so there was no way for the Input Monitoring list to ever show
/// `com.yukimemi.kagi ` for the user to grant. `tccutil` cannot target an
/// unbundled identifier for a surgical reset either. A never-before-seen
/// identifier is the only way out that does not `tccutil reset` (and so
/// revoke) every other application's grants.
const SIGN_ID: &str = "com.yukimemi.kagi.agent";

/// The `.app` bundle's `CFBundleIdentifier` — `service::imp::deploy` stamps
/// this into `~/Applications/kagi.app/Contents/Info.plist` and signs the
/// bundle with it as the designated requirement (see `deploy`'s own
/// comment). Kept apart from [`SIGN_ID`] on purpose, even though both name
/// the same logical agent: TCC tracks a *Bundle ID* (this one, resolved
/// through the bundle's Info.plist) and a bare executable's *path identity*
/// (`SIGN_ID`, pinned by [`ensure_stable_identity`]) as two entirely
/// separate rows, confirmed independently `Update Access Record`-ing in
/// `log show --predicate 'subsystem == "com.apple.TCC"'`. A `tccutil reset
/// <service> <bundle-id>` in [`reset_permissions`] can only ever target this
/// one — `tccutil` resolves its argument through LaunchServices, which only
/// a real installed bundle answers to.
pub(crate) const BUNDLE_ID: &str = "com.yukimemi.kagi.app";

/// `LC_UUID`, patched to this fixed value by [`pin_macho_uuid`].
///
/// Any 16 bytes work; this is `sha256(b"com.yukimemi.kagi.agent")[..16]`,
/// computed once and hard-coded rather than at build time, so the value is
/// visible here rather than buried in a build script.
const PINNED_UUID: [u8; 16] = [
    0x10, 0x04, 0x83, 0xfb, 0x42, 0x22, 0xd1, 0xf6, 0xd4, 0x79, 0x75, 0x98, 0xc8, 0xce, 0xc7, 0xf4,
];

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_UUID: u32 = 0x1b;

/// Overwrite `LC_UUID` in a Mach-O file, via an external `dd` process.
///
/// This is the actual fix, and the surprising part of the whole
/// investigation (see [`ensure_stable_identity`]): TCC does not use
/// `codesign`'s `--identifier` to identify a bundle-less Mach-O executable at
/// all. It resolves identity from `LC_UUID`, which `rustc`/`ld64` mint fresh
/// on every build — confirmed by comparing `otool -l | grep -A2 LC_UUID`
/// output across two builds (different every time) against `log show
/// --predicate 'subsystem == "com.apple.TCC"'` output, which showed the
/// *responsible process identifier* tccd recorded as a linker-style
/// `kagi-<hash>` string even immediately after `codesign --identifier
/// com.yukimemi.kagi` — and, after this patch, as `com.yukimemi.kagi`
/// instead.
///
/// `ld64 -no_uuid` (omit the load command) was tried first and rejected:
/// dyld refuses to execute some binaries — observed on a build-script
/// dependency — with "missing LC_UUID load command".
///
/// `file` must NOT be a binary that is currently executing: the kernel's
/// code-signing enforcement SIGKILLs a process the moment *any* writer —
/// itself, or an external tool like `dd` used here — changes bytes in its
/// mapped, signed backing file. Confirmed by direct reproduction (exit 137)
/// from `kagi permissions` patching its own live executable. That is why
/// [`ensure_stable_identity`] never calls this on `current_exe()` directly;
/// it always operates on an inert copy first.
fn pin_macho_uuid(file: &Path) -> bool {
    use std::io::Write;

    let Ok(data) = std::fs::read(file) else {
        return false;
    };
    if data.len() < 32 || u32::from_le_bytes(data[0..4].try_into().unwrap()) != MH_MAGIC_64 {
        return false;
    }
    let ncmds = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let sizeofcmds = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;
    let Some(end) = 32usize.checked_add(sizeofcmds).map(|e| e.min(data.len())) else {
        return false;
    };

    let mut off = 32usize;
    for _ in 0..ncmds {
        if off + 8 > end {
            return false;
        }
        let cmd = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        let cmdsize = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
        if cmdsize < 8 || off + cmdsize > end {
            return false;
        }
        if cmd == LC_UUID && cmdsize == 24 {
            let uuid_off = off + 8;
            if data[uuid_off..uuid_off + 16] == PINNED_UUID {
                return true; // already pinned
            }
            let Ok(mut dd) = std::process::Command::new("dd")
                .args([
                    &format!("of={}", file.display()),
                    "bs=1",
                    &format!("seek={uuid_off}"),
                    "count=16",
                    "conv=notrunc",
                    "status=none",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return false;
            };
            if let Some(mut stdin) = dd.stdin.take() {
                let _ = stdin.write_all(&PINNED_UUID);
            }
            return dd.wait().is_ok_and(|s| s.success());
        }
        off += cmdsize;
    }
    false
}

/// Whether `file`'s on-disk signature is valid *and* carries [`SIGN_ID`] as
/// its designated requirement. Read-only — safe to call on a live,
/// executing binary, unlike [`pin_macho_uuid`] and the `codesign --force`
/// step in [`ensure_stable_identity`].
fn has_valid_stable_signature(file: &Path) -> bool {
    let verified = std::process::Command::new("codesign")
        .args(["--verify", &file.display().to_string()])
        .output()
        .is_ok_and(|o| o.status.success());
    if !verified {
        return false;
    }
    std::process::Command::new("codesign")
        .args(["-d", "-r", "-", &file.display().to_string()])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout).contains(&format!("identifier \"{SIGN_ID}\""))
        })
}

/// Give the binary a stable identity: a fixed `LC_UUID` (what TCC actually
/// keys a bundle-less executable's grant to), plus a `codesign` identifier
/// and identifier-only Designated Requirement for good measure.
///
/// `cargo` leaves a linker-signed ad-hoc signature whose identifier embeds a
/// hash of the binary — `kagi-bef9cabe50a08b72` — and whose default
/// Designated Requirement pins the cdhash rather than that identifier. Fixing
/// only those two (via `--identifier` and `-r`) turned out not to be enough;
/// see [`pin_macho_uuid`] for the part that actually mattered.
///
/// Works on a **copy**, never on `current_exe()` directly, then swaps it in
/// with [`std::fs::rename`]. This is the same reason every self-updater
/// (including kaishin's own) replaces a running binary via a temp-file +
/// rename rather than an in-place write: `rename` only repoints the
/// directory entry, so the process currently executing off the *old* inode
/// is untouched. Modifying that inode's bytes directly, even in a spawned
/// child, gets the calling process SIGKILLed instead — see
/// [`pin_macho_uuid`]'s doc comment for the reproduction.
///
/// Best-effort throughout: a machine without the command line tools, an
/// unexpected Mach-O layout, or a cross-device temp dir simply leaves
/// whatever `cargo` produced in place.
fn ensure_stable_identity() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Inside a real `.app` bundle (`kagi service install`'s deployment
    // target since — see `service::imp::deploy`), TCC resolves identity via
    // the bundle's own `CFBundleIdentifier`/Info.plist, not `LC_UUID` or a
    // codesign `--identifier`/DR pinned on the bare executable. Patching the
    // *executable's* signature here would actively break that: it un-seals
    // the bundle (`codesign --verify` on the bundle then fails with "code
    // has no resources but signature indicates they must be present"),
    // observed directly when this ran unconditionally. `deploy`'s own
    // `codesign --force --deep --sign -` on the whole bundle is complete on
    // its own; nothing here is needed for it.
    if exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return;
    }
    if has_valid_stable_signature(&exe) {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let tmp = dir.join(format!(".kagi-identity-{}", std::process::id()));
    if std::fs::copy(&exe, &tmp).is_err() {
        return;
    }

    pin_macho_uuid(&tmp);
    // `-r=<expr>` is one argv entry: codesign's requirement flag takes its
    // value joined with `=`, not as a separate following argument — passing
    // `["-r", expr]` makes codesign treat `expr` as a requirements *file*
    // path instead of inline text, and fail with "No such file or
    // directory" / "invalid requirement specification".
    let signed = std::process::Command::new("codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            SIGN_ID,
            &format!("-r=designated => identifier \"{SIGN_ID}\""),
            &tmp.display().to_string(),
        ])
        .output()
        .is_ok_and(|o| o.status.success());

    if signed {
        if let Ok(meta) = std::fs::metadata(&exe) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        // Same directory as `exe`, so this is an atomic same-filesystem
        // rename: the process currently running from `exe`'s old inode is
        // unaffected, and any *new* launch of the path gets the patched copy.
        let _ = std::fs::rename(&tmp, &exe);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drop kagi's own Accessibility and Input Monitoring rows — the scripted
/// equivalent of pressing `−` on just kagi's rows, not every application's.
///
/// Targets [`BUNDLE_ID`] specifically: `tccutil reset <service>
/// <bundle-id>` resolves its target through LaunchServices, which only
/// answers for a real, currently-installed bundle — confirmed directly, both
/// that this now succeeds for `BUNDLE_ID` once `kagi service install` has
/// deployed `~/Applications/kagi.app`, and that it still refuses a bare path
/// or an identifier with no matching installed bundle ("No such bundle
/// identifier", `OSStatus -10814`). This is the fix for a real, once painful
/// problem: earlier in development, a **service-wide** `tccutil reset
/// Accessibility` (no target — the only form that worked before the bundle
/// existed) silently revoked *every other application's* grant for that
/// service too, including an unrelated window manager's. `kagi service
/// install` must have run first — this is a targeted fix for kagi's own
/// stuck grant, not a way to get kagi *into* the list.
pub fn reset_permissions() -> Result<()> {
    for service in ["Accessibility", "ListenEvent"] {
        let out = std::process::Command::new("tccutil")
            .args(["reset", service, BUNDLE_ID])
            .output()
            .with_context(|| format!("running tccutil reset {service} {BUNDLE_ID}"))?;
        if !out.status.success() {
            bail!(
                "tccutil reset {service} {BUNDLE_ID} failed: {}\n\
                 (run `kagi service install` first if it has not deployed \
                 ~/Applications/kagi.app yet)",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("reset {service} for {BUNDLE_ID}");
    }
    // The throttle describes an interaction that no longer reflects reality.
    if let Some(m) = throttle_marker() {
        let _ = std::fs::remove_file(m);
    }
    Ok(())
}

/// Read-only: whether Accessibility is currently trusted, no prompt.
fn ax_trusted() -> bool {
    // SAFETY: plain FFI, no arguments.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Read-only: whether Input Monitoring is currently granted, no prompt.
fn listen_event_granted() -> bool {
    // SAFETY: plain FFI.
    unsafe { IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) == KIOHID_ACCESS_TYPE_GRANTED }
}

pub fn request_permissions(prompt: Prompt) -> Result<bool> {
    // Do this before asking: the identifier under which the grant is recorded
    // is the one the binary carries at the moment of the request.
    ensure_stable_identity();

    let interact = may_interact(prompt);

    // SAFETY: plain FFI. `IOHIDCheckAccess` reports granted / denied /
    // never-asked. `IOHIDRequestAccess` shows a system dialog only for
    // never-asked; for an already-decided (denied) identity it shows no UI —
    // but it still creates/refreshes the row in the Input Monitoring pane,
    // which a denied identity otherwise never gets. Skipping the call for
    // "already denied" (the previous behaviour) left Input Monitoring with
    // **zero entries** — nothing for the user to even click — while
    // Accessibility, whose `AXIsProcessTrustedWithOptions` call below is
    // unconditional, did show a row. Call both the same way.
    let input_monitoring = if listen_event_granted() {
        true
    } else if interact {
        // SAFETY: plain FFI.
        unsafe { IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) }
    } else {
        false
    };

    // SAFETY: the global is a constant CFStringRef (Get rule); the dictionary
    // outlives the call.
    let accessibility = unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::from(interact).as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0
    };

    println!(
        "accessibility:   {}\ninput monitoring: {}",
        granted(accessibility),
        granted(input_monitoring)
    );

    // `AXIsProcessTrustedWithOptions(prompt: true)` does not just show a
    // dialog when not yet granted — it also navigates System Settings to
    // the Accessibility pane itself, **asynchronously**, arriving *after*
    // this call returns. Confirmed by direct reproduction: manually opening
    // the Input Monitoring pane, then making only this AX call, flips the
    // frontmost pane back to Accessibility a beat later regardless. Fighting
    // that timing with our own `open_privacy_pane("Privacy_Accessibility")`
    // is redundant (the OS already does it) and racy (two `open` calls back
    // to back land on the first pane; the second is a no-op). So: never open
    // Accessibility ourselves, and open Input Monitoring — the pane with no
    // OS-driven navigation of its own — only after giving the OS's async
    // Accessibility navigation time to land, so it is always what is left on
    // screen.
    if interact && !input_monitoring {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        open_privacy_pane("Privacy_ListenEvent");
    }
    Ok(accessibility && input_monitoring)
}

fn granted(ok: bool) -> &'static str {
    if ok { "granted" } else { "NOT granted" }
}

fn open_privacy_pane(anchor: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
    let _ = std::process::Command::new("open").arg(&url).status();
}

/// How long [`ensure_permissions_interactive`] waits, per permission, for the
/// user to flip the checkbox after opening Settings.
const INTERACTIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// AppleScript string-literal escaping: backslash first, so the quote
/// escape's own backslash is not re-escaped.
fn osascript_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A native `display dialog` via `osascript` — the same "explain, then let
/// the user drive System Settings themselves" pattern
/// [paneru](https://github.com/karinushka/paneru) uses (`NSAlert` there;
/// `osascript` here, to avoid pulling `objc2`/AppKit into a crate that
/// otherwise only links CoreFoundation/CoreGraphics). `display dialog`
/// blocks the calling thread until the user answers, which is the point:
/// unlike [`request_permissions`]'s fire-and-forget prompts, the caller here
/// knows *when* the user has moved on, instead of firing both platform
/// prompts within milliseconds of each other and letting them race for the
/// frontmost System Settings pane.
///
/// Returns whether `action` (not "Cancel") was clicked.
fn ask(title: &str, message: &str, action: &str) -> bool {
    let script = format!(
        "display dialog \"{}\" with title \"{}\" buttons {{\"Cancel\", \"{}\"}} \
         default button \"{}\" cancel button \"Cancel\" with icon caution",
        osascript_quote(message),
        osascript_quote(title),
        osascript_quote(action),
        osascript_quote(action),
    );
    // `cancel button` makes AppleScript raise on Cancel/Esc/window-close,
    // which osascript reports as a non-zero exit — the boolean falls out of
    // the exit status alone, no stdout parsing needed.
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .status()
        .is_ok_and(|s| s.success())
}

/// Poll `granted` every 500 ms until it reports true or `timeout` elapses.
fn wait_until(mut granted: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if granted() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Interactive, synchronous permission setup for `kagi permissions` and
/// `kagi service install` — commands a person is looking at right now.
///
/// Walks Accessibility, then Input Monitoring, **one at a time**: show our
/// own dialog explaining what is needed, trigger the OS's own request (which
/// is what registers kagi in that Privacy & Security list) only if the user
/// agrees, then block polling the plain `AX`/`IOHID` check until it reports
/// granted or [`INTERACTIVE_TIMEOUT`] passes.
///
/// This is the fix for a real race in the old fire-both-at-once design:
/// `AXIsProcessTrustedWithOptions(prompt: true)` asynchronously navigates
/// System Settings to Accessibility on its own, arriving *after* the call
/// returns — confirmed by reproduction, opening Input Monitoring manually and
/// then making only that one AX call flipped the frontmost pane back to
/// Accessibility moments later regardless. Firing the Input Monitoring
/// request a fixed 1.5 s afterward (see [`request_permissions`], still used
/// by the non-interactive retry path) was a guess at that timing; doing the
/// two permissions strictly in sequence, gated on the user actually finishing
/// the first one, removes the guess entirely.
///
/// [`request_permissions`] (used by the daemon's own failing-start retry) is
/// deliberately left as the fire-and-forget, non-blocking path: that one
/// fires from a `KeepAlive` respawn loop that may run with nobody at the
/// keyboard, where a modal dialog blocking for up to
/// [`INTERACTIVE_TIMEOUT`] on every restart would be worse than the race it
/// would fix.
pub fn ensure_permissions_interactive() -> Result<bool> {
    ensure_stable_identity();

    if !ax_trusted() {
        if ask(
            "kagi needs Accessibility access",
            "kagi remaps keys and drives your IME, which macOS gates behind \
             Accessibility access.\n\n\
             Click \u{201c}Open System Settings\u{201d}, then turn kagi on \
             under Privacy & Security \u{2192} Accessibility.\n\n\
             If kagi is already listed there but still is not working, \
             remove it with the \u{2212} button, add it again with + (pick \
             the kagi binary), then turn it on.\n\n\
             kagi continues on its own once access is granted.",
            "Open System Settings",
        ) {
            // SAFETY: the global is a constant CFStringRef (Get rule); the
            // dictionary outlives the call. This is what registers kagi in
            // the Accessibility list and shows the OS's own prompt.
            unsafe {
                let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
                let options = CFDictionary::from_CFType_pairs(&[(
                    key.as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                )]);
                AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
            }
        }
        if !wait_until(ax_trusted, INTERACTIVE_TIMEOUT) {
            println!("accessibility:   NOT granted (timed out waiting)");
            return Ok(false);
        }
    }
    println!("accessibility:   granted");

    if !listen_event_granted() {
        if ask(
            "kagi needs Input Monitoring access",
            "kagi reads raw key events system-wide to remap them, which \
             macOS gates behind Input Monitoring access.\n\n\
             Click \u{201c}Open System Settings\u{201d}, then turn kagi on \
             under Privacy & Security \u{2192} Input Monitoring.\n\n\
             If kagi is already listed there but still is not working, \
             remove it with the \u{2212} button, add it again with + (pick \
             the kagi binary), then turn it on.\n\n\
             kagi continues on its own once access is granted.",
            "Open System Settings",
        ) {
            // SAFETY: plain FFI; registers kagi in the Input Monitoring list.
            unsafe {
                IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT);
            }
            // Unlike the Accessibility prompt, this one does not navigate
            // System Settings on its own — Accessibility already had its
            // turn and is done (we just finished waiting on it), so there is
            // no second async navigation left to race.
            open_privacy_pane("Privacy_ListenEvent");
        }
        if !wait_until(listen_event_granted, INTERACTIVE_TIMEOUT) {
            println!("input monitoring: NOT granted (timed out waiting)");
            return Ok(false);
        }
    }
    println!("input monitoring: granted");

    Ok(true)
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
    install(
        context(engine, config)?,
        KCG_EVENT_TAP_OPTION_DEFAULT,
        tap_callback,
    )
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
    // SAFETY: same contract as `tap_callback` — leaked context, one thread.
    let ctx = unsafe { &mut *(user_info as *mut Context) };
    // SAFETY: `event_ref` is the live event CoreGraphics passed in.
    let event = ManuallyDrop::new(unsafe { CGEvent::from_ptr(event_ref) });

    if etype == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT || etype == KCG_EVENT_TAP_DISABLED_BY_USER_INPUT {
        // SAFETY: `ctx.tap` is the port from CGEventTapCreate, alive for the
        // process lifetime.
        unsafe { CGEventTapEnable(ctx.tap, true) };
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
    let prefix = if mods.is_empty() {
        String::new()
    } else {
        format!("{mods}-")
    };
    println!(
        "{phase}  {prefix}{name:<18} (keycode=0x{code:02X} flags=0x{:X})",
        flags.bits()
    );
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
        for mods in [
            Mods::empty(),
            Mods::CTRL,
            Mods::CTRL | Mods::SHIFT,
            Mods::all(),
        ] {
            assert_eq!(mods_from_flags(flags_from_mods(mods)), mods);
        }
    }
}
