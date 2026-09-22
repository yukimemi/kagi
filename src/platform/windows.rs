//! Windows backend: a `WH_KEYBOARD_LL` hook for capture, `SendInput` for
//! synthesis, IMM32 for IME control.
//!
//! The hook is registered globally (thread id 0), but the system always calls
//! the hook procedure on the thread that installed it, while that thread is
//! retrieving messages. All mutable state therefore lives in a `thread_local!`
//! and needs no locking.

use crate::action::ImeState;
use crate::config::Config;
use crate::engine::{Decision, Engine, ModTracker};
use crate::keys::{Chord, Key, Mods};
use crate::platform::{dispatch, Emitter};
use anyhow::{anyhow, bail, Result};
use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Foundation::{
    GetLastError, BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
// The tables below reference some seventy `VK_*` constants, so this module is
// imported wholesale rather than name by name. It also provides `SendInput`,
// `INPUT`, `KEYBDINPUT`, the `KEYEVENTF_*` flags and `MapVirtualKeyW`.
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, PostThreadMessageW,
    SendMessageTimeoutW, SetWindowsHookExW, UnhookWindowsHookEx, GUITHREADINFO, HC_ACTION, HHOOK,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, SMTO_ABORTIFHUNG, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// Stamped into `dwExtraInfo` of every event we inject, so the hook can
/// recognise its own output and forward it without consulting the engine.
/// `LLKHF_INJECTED` would be the obvious filter but it also matches other
/// legitimate injectors (AutoHotkey, remote desktop, on-screen keyboards),
/// whose events must still be remappable.
const KAGI_MARK: usize = 0x6B_61_67_69;

// WM_IME_CONTROL and its sub-commands are not in the windows-sys metadata.
const WM_IME_CONTROL: u32 = 0x0283;
const IMC_GETOPENSTATUS: WPARAM = 0x0005;
const IMC_SETOPENSTATUS: WPARAM = 0x0006;
/// Upper bound on how long an IME round-trip may block the hook thread.
const IME_TIMEOUT_MS: u32 = 200;

// =============================================================================
// Key translation
// =============================================================================

/// Japanese keys, resolved by scan code: their virtual-key code depends on the
/// active keyboard layout and on whether the IME is running, the scan code
/// does not. `(scan code, virtual key, key)`.
const JIS_TABLE: &[(u16, u16, Key)] = &[
    (0x29, VK_KANJI, Key::Zenkaku),
    (0x70, VK_OEM_COPY, Key::KatakanaHiragana),
    (0x73, VK_OEM_102, Key::IntlRo),
    (0x79, VK_CONVERT, Key::Henkan),
    (0x7B, VK_NONCONVERT, Key::Muhenkan),
    (0x7D, VK_OEM_5, Key::IntlYen),
];

/// Virtual-key codes reported for 半角/全角. `VK_OEM_AUTO` and `VK_OEM_ENLW`
/// are the values the SDK also calls `VK_DBE_SBCSCHAR` / `VK_DBE_DBCSCHAR`,
/// and the IME swallows the key into `VK_PROCESSKEY` while it is running.
const ZENKAKU_VKS: &[u16] = &[VK_KANJI, VK_OEM_AUTO, VK_OEM_ENLW, VK_PROCESSKEY];

/// Virtual-key code <-> `Key`. Only codes that are the same on every layout
/// and independent of the extended-key flag; the Japanese keys come from
/// [`JIS_TABLE`] and the sided/generic modifiers plus `VK_RETURN` are handled
/// directly in [`to_key`] and [`from_key`].
const VK_TABLE: &[(u16, Key)] = &[
    (VK_A, Key::A), (VK_B, Key::B), (VK_C, Key::C), (VK_D, Key::D),
    (VK_E, Key::E), (VK_F, Key::F), (VK_G, Key::G), (VK_H, Key::H),
    (VK_I, Key::I), (VK_J, Key::J), (VK_K, Key::K), (VK_L, Key::L),
    (VK_M, Key::M), (VK_N, Key::N), (VK_O, Key::O), (VK_P, Key::P),
    (VK_Q, Key::Q), (VK_R, Key::R), (VK_S, Key::S), (VK_T, Key::T),
    (VK_U, Key::U), (VK_V, Key::V), (VK_W, Key::W), (VK_X, Key::X),
    (VK_Y, Key::Y), (VK_Z, Key::Z),

    (VK_0, Key::Num0), (VK_1, Key::Num1), (VK_2, Key::Num2),
    (VK_3, Key::Num3), (VK_4, Key::Num4), (VK_5, Key::Num5),
    (VK_6, Key::Num6), (VK_7, Key::Num7), (VK_8, Key::Num8),
    (VK_9, Key::Num9),

    (VK_F1, Key::F1), (VK_F2, Key::F2), (VK_F3, Key::F3), (VK_F4, Key::F4),
    (VK_F5, Key::F5), (VK_F6, Key::F6), (VK_F7, Key::F7), (VK_F8, Key::F8),
    (VK_F9, Key::F9), (VK_F10, Key::F10), (VK_F11, Key::F11),
    (VK_F12, Key::F12), (VK_F13, Key::F13), (VK_F14, Key::F14),
    (VK_F15, Key::F15), (VK_F16, Key::F16), (VK_F17, Key::F17),
    (VK_F18, Key::F18), (VK_F19, Key::F19), (VK_F20, Key::F20),

    (VK_OEM_3, Key::Grave), (VK_OEM_MINUS, Key::Minus),
    (VK_OEM_PLUS, Key::Equal), (VK_OEM_4, Key::LeftBracket),
    (VK_OEM_6, Key::RightBracket), (VK_OEM_5, Key::Backslash),
    (VK_OEM_1, Key::Semicolon), (VK_OEM_7, Key::Quote),
    (VK_OEM_COMMA, Key::Comma), (VK_OEM_PERIOD, Key::Period),
    (VK_OEM_2, Key::Slash),

    (VK_ESCAPE, Key::Escape), (VK_TAB, Key::Tab), (VK_CAPITAL, Key::CapsLock),
    (VK_SPACE, Key::Space), (VK_BACK, Key::Backspace),
    (VK_INSERT, Key::Insert), (VK_DELETE, Key::Delete),
    (VK_HOME, Key::Home), (VK_END, Key::End),
    (VK_PRIOR, Key::PageUp), (VK_NEXT, Key::PageDown),
    (VK_LEFT, Key::Left), (VK_RIGHT, Key::Right),
    (VK_UP, Key::Up), (VK_DOWN, Key::Down),
    (VK_SNAPSHOT, Key::PrintScreen), (VK_SCROLL, Key::ScrollLock),
    (VK_PAUSE, Key::Pause), (VK_APPS, Key::Menu), (VK_NUMLOCK, Key::NumLock),

    (VK_LCONTROL, Key::LeftCtrl), (VK_RCONTROL, Key::RightCtrl),
    (VK_LSHIFT, Key::LeftShift), (VK_RSHIFT, Key::RightShift),
    (VK_LMENU, Key::LeftAlt), (VK_RMENU, Key::RightAlt),
    (VK_LWIN, Key::LeftMeta), (VK_RWIN, Key::RightMeta),

    (VK_NUMPAD0, Key::Numpad0), (VK_NUMPAD1, Key::Numpad1),
    (VK_NUMPAD2, Key::Numpad2), (VK_NUMPAD3, Key::Numpad3),
    (VK_NUMPAD4, Key::Numpad4), (VK_NUMPAD5, Key::Numpad5),
    (VK_NUMPAD6, Key::Numpad6), (VK_NUMPAD7, Key::Numpad7),
    (VK_NUMPAD8, Key::Numpad8), (VK_NUMPAD9, Key::Numpad9),
    (VK_ADD, Key::NumpadPlus), (VK_SUBTRACT, Key::NumpadMinus),
    (VK_MULTIPLY, Key::NumpadMultiply), (VK_DIVIDE, Key::NumpadDivide),
    (VK_DECIMAL, Key::NumpadDot), (VK_OEM_NEC_EQUAL, Key::NumpadEqual),
];

/// Keys the system prefixes with `E0` in the scan-code stream, i.e. the ones
/// whose synthetic events need `KEYEVENTF_EXTENDEDKEY`.
const EXTENDED_KEYS: &[Key] = &[
    Key::RightCtrl,
    Key::RightAlt,
    Key::LeftMeta,
    Key::RightMeta,
    Key::Menu,
    Key::Insert,
    Key::Delete,
    Key::Home,
    Key::End,
    Key::PageUp,
    Key::PageDown,
    Key::Left,
    Key::Right,
    Key::Up,
    Key::Down,
    Key::NumLock,
    Key::NumpadDivide,
    Key::NumpadEnter,
];

/// The `Mods` bit each side key contributes. Order fixes the order in which
/// modifiers are released and pressed during neutralisation.
const SIDED_MODS: &[(Key, Mods)] = &[
    (Key::LeftCtrl, Mods::CTRL),
    (Key::RightCtrl, Mods::CTRL),
    (Key::LeftShift, Mods::SHIFT),
    (Key::RightShift, Mods::SHIFT),
    (Key::LeftAlt, Mods::ALT),
    (Key::RightAlt, Mods::ALT),
    (Key::LeftMeta, Mods::META),
    (Key::RightMeta, Mods::META),
];

/// Side used when a chord asks for a modifier the user is not holding.
const MOD_KEYS: &[(Mods, Key)] = &[
    (Mods::CTRL, Key::LeftCtrl),
    (Mods::SHIFT, Key::LeftShift),
    (Mods::ALT, Key::LeftAlt),
    (Mods::META, Key::LeftMeta),
];

/// Native event -> `Key`. `None` means kagi has no name for this code; the
/// caller forwards such events untouched.
fn to_key(vk: u32, sc: u32, extended: bool) -> Option<Key> {
    let vk = vk as u16;
    let sc = sc as u16;

    if let Some(key) = jis_key(vk, sc) {
        return Some(key);
    }
    match vk {
        // The low-level hook normally reports the sided codes, but a
        // synthesized or remapped event can still carry the generic one.
        // Right Ctrl and right Alt are `E0`-prefixed; right Shift is not, so
        // it has to be told apart by scan code.
        VK_CONTROL => Some(if extended { Key::RightCtrl } else { Key::LeftCtrl }),
        VK_MENU => Some(if extended { Key::RightAlt } else { Key::LeftAlt }),
        VK_SHIFT => Some(if sc == 0x36 { Key::RightShift } else { Key::LeftShift }),
        // Numpad Enter shares VK_RETURN with the main Enter key.
        VK_RETURN => Some(if extended { Key::NumpadEnter } else { Key::Enter }),
        _ => VK_TABLE.iter().find(|(v, _)| *v == vk).map(|(_, k)| *k),
    }
}

/// Reverse of [`to_key`]: the codes needed to synthesize `key`, as
/// `(virtual key, scan code, extended)`. A scan code of 0 means "no usable
/// scan code, send the virtual key instead". `None` means Windows has no
/// equivalent for the key.
fn from_key(key: Key) -> Option<(u16, u16, bool)> {
    if let Some((sc, vk, _)) = JIS_TABLE.iter().find(|(_, _, k)| *k == key) {
        return Some((*vk, *sc, false));
    }
    let vk = match key {
        Key::Enter => VK_RETURN,
        Key::NumpadEnter => VK_RETURN,
        _ => *VK_TABLE.iter().find(|(_, k)| *k == key).map(|(v, _)| v)?,
    };
    // Pause is `E1 1D 45` and PrintScreen `E0 37`, but MapVirtualKeyW reports
    // the legacy 0x45 / 0x54 codes for them, which no app would decode as the
    // key we meant. Both are synthesized by virtual key instead.
    if matches!(key, Key::Pause | Key::PrintScreen) {
        return Some((vk, 0, false));
    }
    // SAFETY: MapVirtualKeyW only reads the calling thread's keyboard layout;
    // it takes no pointers and cannot fail destructively (0 means "no scan
    // code for this key on this layout", which the caller handles).
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    Some((vk, sc, EXTENDED_KEYS.contains(&key)))
}

/// Japanese keys by scan code.
fn jis_key(vk: u16, sc: u16) -> Option<Key> {
    let (_, _, key) = JIS_TABLE.iter().find(|(s, _, _)| *s == sc)?;
    // Scan code 0x29 is 半角/全角 on a JIS keyboard but Grave on ANSI; only
    // the virtual-key code tells the two apart.
    if *key == Key::Zenkaku && !ZENKAKU_VKS.contains(&vk) {
        return None;
    }
    Some(*key)
}

// =============================================================================
// Modifier bookkeeping
// =============================================================================

/// Which physical modifier keys are down, by side. `ModTracker` collapses the
/// sides away, but neutralisation has to release exactly the side the user is
/// holding: releasing the other one would leave the held side pressed and add
/// a phantom release that no physical key-up will ever balance.
#[derive(Debug, Default, Clone, Copy)]
struct HeldSides(u8);

impl HeldSides {
    fn update(&mut self, key: Key, down: bool) {
        if let Some(i) = SIDED_MODS.iter().position(|(k, _)| *k == key) {
            if down {
                self.0 |= 1 << i;
            } else {
                self.0 &= !(1 << i);
            }
        }
    }

    fn mods(self) -> Mods {
        let mut mods = Mods::empty();
        for (i, (_, m)) in SIDED_MODS.iter().enumerate() {
            if self.0 & (1 << i) != 0 {
                mods |= *m;
            }
        }
        mods
    }

    /// The held side keys that contribute one of `mods`.
    fn keys_in(self, mods: Mods) -> Vec<Key> {
        let mut out = Vec::new();
        for (i, (key, m)) in SIDED_MODS.iter().enumerate() {
            if self.0 & (1 << i) != 0 && mods.contains(*m) {
                out.push(*key);
            }
        }
        out
    }
}

/// Feed one transition into `tracker` and return the modifier state at the
/// moment of the event, excluding the event's own key — which is what
/// `Engine::on_key` expects.
fn track(tracker: &mut ModTracker, key: Key, down: bool) -> Mods {
    if down {
        let before = tracker.current();
        tracker.update(key, true);
        before
    } else {
        tracker.update(key, false);
        tracker.current()
    }
}

// =============================================================================
// Synthesis
// =============================================================================

#[derive(Debug, Default)]
struct WinEmitter {
    held: HeldSides,
}

impl Emitter for WinEmitter {
    fn tap(&mut self, chord: Chord) -> Result<()> {
        let held = self.held.mods();
        // Physically held modifiers that the chord does not want. Without this
        // a `ctrl-[ -> esc` rule would deliver Ctrl+Esc — the Start menu —
        // because the user is still holding Ctrl when the tap is injected.
        let release = self.held.keys_in(held & !chord.mods);
        let press: Vec<Key> = MOD_KEYS
            .iter()
            .filter(|(m, _)| (chord.mods & !held).contains(*m))
            .map(|(_, k)| *k)
            .collect();

        let mut inputs = Vec::with_capacity(2 * (release.len() + press.len()) + 2);
        for key in &release {
            inputs.push(key_event(*key, true)?);
        }
        for key in &press {
            inputs.push(key_event(*key, false)?);
        }
        inputs.push(key_event(chord.key, false)?);
        inputs.push(key_event(chord.key, true)?);
        // Restore the physical state exactly: drop what we pressed, re-press
        // what we released. The physical keys are still down, so their real
        // key-up events will balance these.
        for key in press.iter().rev() {
            inputs.push(key_event(*key, true)?);
        }
        for key in release.iter().rev() {
            inputs.push(key_event(*key, false)?);
        }

        // One call: the system injects the batch as a unit, so no physical or
        // foreign event can interleave and see a half-neutralised state.
        send(&inputs)
    }

    fn ime(&mut self, state: ImeState) -> Result<()> {
        let open = match state {
            ImeState::On => true,
            ImeState::Off => false,
            ImeState::Toggle => ime_control(IMC_GETOPENSTATUS, 0)? == 0,
        };
        ime_control(IMC_SETOPENSTATUS, open as LPARAM)?;
        Ok(())
    }

    fn input_source(&mut self, id: &str) -> Result<()> {
        bail!(
            "`source:{id}` is macOS/Linux-only. Windows has no Text Input \
             Services equivalent: an IME is bound to the focused thread's \
             keyboard layout, and switching it for another process is not \
             something an outside program can do reliably. Use `ime:on` / \
             `ime:off` / `ime:toggle`, which drive the focused window's IME \
             open status through IMM32 and are what the AutoHotkey \
             `IME_SET()` recipe does."
        )
    }
}

/// One synthetic key transition, stamped with [`KAGI_MARK`].
fn key_event(key: Key, up: bool) -> Result<INPUT> {
    let (vk, sc, extended) = from_key(key).ok_or_else(|| {
        anyhow!(
            "`{}` has no Windows key code; `eisu` and `kana` are macOS keys — \
             on Windows use `henkan`, `muhenkan` or `katakanahiragana`",
            key.name()
        )
    })?;
    let mut flags = 0;
    // Prefer scan codes: the receiving app maps them through its own layout,
    // so a tap lands on the same physical key the user asked for.
    if sc != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    Ok(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: if sc != 0 { 0 } else { vk },
                wScan: sc,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: KAGI_MARK,
            },
        },
    })
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a live slice of `INPUT`, the count matches its
    // length and the size argument matches the struct SendInput expects.
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        bail!(
            "SendInput delivered {sent} of {} events (error 0x{err:08X}); \
             Windows blocks injection into windows of higher integrity, so \
             kagi has to run elevated to type into elevated windows",
            inputs.len()
        );
    }
    Ok(())
}

// =============================================================================
// IME
// =============================================================================

/// The window whose IME should be driven: the focused control of the
/// foreground thread, as `IME.ahk` does, falling back to the foreground
/// window itself when `GetGUIThreadInfo` reports nothing usable.
fn focus_window() -> HWND {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        flags: 0,
        hwndActive: ptr::null_mut(),
        hwndFocus: ptr::null_mut(),
        hwndCapture: ptr::null_mut(),
        hwndMenuOwner: ptr::null_mut(),
        hwndMoveSize: ptr::null_mut(),
        hwndCaret: ptr::null_mut(),
        rcCaret: RECT { left: 0, top: 0, right: 0, bottom: 0 },
    };
    // SAFETY: `info` is a valid GUITHREADINFO with `cbSize` filled in, which
    // is the only precondition; thread id 0 asks about the foreground thread.
    let ok = unsafe { GetGUIThreadInfo(0, &mut info) } != 0;
    if ok && !info.hwndFocus.is_null() {
        return info.hwndFocus;
    }
    // SAFETY: GetForegroundWindow takes no arguments and may return null,
    // which the caller checks.
    unsafe { GetForegroundWindow() }
}

/// Send one `WM_IME_CONTROL` command to the focused window's default IME
/// window and return its result.
fn ime_control(command: WPARAM, value: LPARAM) -> Result<usize> {
    let hwnd = focus_window();
    if hwnd.is_null() {
        bail!("no focused window to drive the IME of");
    }
    // SAFETY: `hwnd` is a non-null window handle; ImmGetDefaultIMEWnd only
    // looks it up and returns null when the window has no IME window.
    let ime = unsafe { ImmGetDefaultIMEWnd(hwnd) };
    if ime.is_null() {
        bail!("focused window has no IMM32 IME window; no Japanese IME is active for it");
    }

    let mut result: usize = 0;
    // A plain SendMessageW blocks until the target thread pumps its message
    // queue, so one hung application would wedge the hook thread and with it
    // the whole keyboard. SMTO_ABORTIFHUNG plus a 200 ms cap bounds that.
    // SAFETY: `ime` is a live window handle and `result` is a valid
    // out-parameter for the duration of the call.
    let ok = unsafe {
        SendMessageTimeoutW(
            ime,
            WM_IME_CONTROL,
            command,
            value,
            SMTO_ABORTIFHUNG,
            IME_TIMEOUT_MS,
            &mut result,
        )
    };
    if ok == 0 {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        bail!(
            "WM_IME_CONTROL(0x{command:04X}) timed out or failed after \
             {IME_TIMEOUT_MS} ms (error 0x{err:08X}); the focused app is not \
             answering messages"
        );
    }
    Ok(result)
}

// =============================================================================
// Hook plumbing
// =============================================================================

struct RunState {
    engine: Engine,
    mods: ModTracker,
    emitter: WinEmitter,
}

thread_local! {
    /// Only ever touched by the hook procedure and by `run`, both on the
    /// thread that installed the hook.
    static RUN_STATE: RefCell<Option<RunState>> = const { RefCell::new(None) };
    /// `watch` needs modifier state to name chords, but no engine.
    static WATCH_MODS: Cell<ModTracker> = Cell::new(ModTracker::default());
}

/// Thread that runs the message pump, so the console control handler (which
/// runs on its own thread) can ask it to quit.
static PUMP_THREAD: AtomicU32 = AtomicU32::new(0);

extern "system" fn ctrl_handler(kind: u32) -> BOOL {
    match kind {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
            let thread = PUMP_THREAD.load(Ordering::SeqCst);
            if thread != 0 {
                // SAFETY: posting WM_QUIT to a thread id takes no pointers;
                // a stale id simply makes the call fail.
                unsafe { PostThreadMessageW(thread, WM_QUIT, 0, 0) };
            }
            1
        }
        _ => 0,
    }
}

extern "system" fn run_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && handle(wparam as u32, lparam) {
        return 1; // Non-zero swallows the event.
    }
    // SAFETY: forwarding the very arguments we were handed; a null hook handle
    // is documented as "start from the beginning of the chain".
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

extern "system" fn watch_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        report(wparam as u32, lparam);
    }
    // `watch` never consumes: always fall through to the next hook.
    // SAFETY: forwarding the very arguments we were handed; a null hook handle
    // is documented as "start from the beginning of the chain".
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

/// `true` if the event must be swallowed.
fn handle(message: u32, lparam: LPARAM) -> bool {
    // SAFETY: for HC_ACTION the system passes a pointer to a KBDLLHOOKSTRUCT
    // that stays valid for the duration of the hook call.
    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    if info.dwExtraInfo == KAGI_MARK {
        return false; // Our own injection: never feed it back to the engine.
    }
    let down = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return false,
    };
    let extended = info.flags & LLKHF_EXTENDED != 0;
    // An unknown code is forwarded untouched.
    let Some(key) = to_key(info.vkCode, info.scanCode, extended) else {
        return false;
    };

    RUN_STATE.with(|cell| {
        // The hook only runs on the thread that installed it and nothing else
        // borrows this cell while the hook is executing. `try_borrow_mut`
        // guards the theoretical re-entrant case, because a panic inside an
        // `extern "system"` function aborts the process.
        let Ok(mut guard) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(state) = guard.as_mut() else {
            return false;
        };

        state.emitter.held.update(key, down);
        let mods = track(&mut state.mods, key, down);
        match state.engine.on_key(key, mods, down) {
            Decision::Pass => false,
            Decision::Consume => true,
            Decision::Run { index, passthrough } => {
                let RunState { engine, emitter, .. } = state;
                if let Err(err) = dispatch(emitter, engine.actions(index)) {
                    eprintln!("kagi: {err:#}");
                }
                !passthrough
            }
        }
    })
}

/// Print one event for `watch`, including codes kagi has no name for.
fn report(message: u32, lparam: LPARAM) {
    // SAFETY: for HC_ACTION the system passes a pointer to a KBDLLHOOKSTRUCT
    // that stays valid for the duration of the hook call.
    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let down = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return,
    };
    let extended = info.flags & LLKHF_EXTENDED != 0;
    let name = match to_key(info.vkCode, info.scanCode, extended) {
        Some(key) => {
            let mut tracker = WATCH_MODS.get();
            let mods = track(&mut tracker, key, down);
            WATCH_MODS.set(tracker);
            Chord { key, mods }.to_string()
        }
        None => "unknown".to_string(),
    };
    let dir = if down { "down" } else { "up  " };
    println!(
        "{dir}  {name:<20} (vk=0x{:02X} sc=0x{:02X} ext={})",
        info.vkCode, info.scanCode, extended as u8
    );
}

fn install(proc: HOOKPROC) -> Result<HHOOK> {
    // SAFETY: a null module name asks for the handle of the running process
    // image, which is always loaded and must not be freed.
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    // SAFETY: `proc` has the signature WH_KEYBOARD_LL requires, `module`
    // belongs to this process and thread id 0 installs a global hook whose
    // callbacks are delivered on this thread.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, proc, module, 0) };
    if hook.is_null() {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        bail!(
            "SetWindowsHookExW(WH_KEYBOARD_LL) failed (error 0x{err:08X}); \
             a hook never sees keys typed into a window of higher integrity, \
             so kagi must run elevated if it should work in elevated \
             foreground windows"
        );
    }
    Ok(hook)
}

fn uninstall(hook: HHOOK) {
    // SAFETY: `hook` came from SetWindowsHookExW on this thread and is
    // unhooked exactly once, at the end of the pump that installed it.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
}

fn install_ctrl_handler() -> Result<()> {
    // SAFETY: GetCurrentThreadId reads the calling thread's id.
    PUMP_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    // SAFETY: `ctrl_handler` matches PHANDLER_ROUTINE and stays valid for the
    // life of the process; 1 adds it to the handler list.
    if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) } == 0 {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        bail!("SetConsoleCtrlHandler failed (error 0x{err:08X}); cannot stop cleanly on Ctrl-C");
    }
    Ok(())
}

/// Message pump. A low-level keyboard hook is dispatched while its thread
/// retrieves messages, so this loop is what keeps the hook alive; it exits on
/// the WM_QUIT posted by [`ctrl_handler`].
fn pump() -> Result<()> {
    let mut msg = MSG {
        hwnd: ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: POINT { x: 0, y: 0 },
    };
    loop {
        // SAFETY: `msg` is a valid out-parameter; a null window handle asks
        // for every message posted to this thread.
        let got = unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) };
        match got {
            0 => return Ok(()), // WM_QUIT
            -1 => {
                // SAFETY: reads this thread's last-error value.
                let err = unsafe { GetLastError() };
                bail!("GetMessageW failed (error 0x{err:08X})");
            }
            _ => {}
        }
    }
}

// =============================================================================
// Entry points
// =============================================================================

pub fn run(engine: Engine, _config: &Config) -> Result<()> {
    RUN_STATE.with(|cell| {
        *cell.borrow_mut() = Some(RunState {
            engine,
            mods: ModTracker::default(),
            emitter: WinEmitter::default(),
        });
    });
    install_ctrl_handler()?;
    let hook = install(Some(run_proc))?;
    println!("kagi: hook installed; Ctrl-C to stop");

    let result = pump();

    uninstall(hook);
    RUN_STATE.with(|cell| *cell.borrow_mut() = None);
    result
}

pub fn watch(_config: &Config) -> Result<()> {
    WATCH_MODS.with(|cell| cell.set(ModTracker::default()));
    install_ctrl_handler()?;
    let hook = install(Some(watch_proc))?;
    println!("kagi: watching keys; Ctrl-C to stop");

    let result = pump();

    uninstall(hook);
    result
}
