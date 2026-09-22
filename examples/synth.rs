//! Throwaway smoke-test harness: posts a Ctrl+[ chord so the tap can be
//! exercised without a human at the keyboard.
//!
//! Usage: `cargo run --example synth`

#[cfg(target_os = "macos")]
fn main() {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }

    // SAFETY: plain FFI call, no arguments.
    println!("AXIsProcessTrusted = {}", unsafe { AXIsProcessTrusted() });

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .expect("CGEventSourceCreate failed");

    const LEFT_CTRL: u16 = 0x3B;
    const LEFT_BRACKET: u16 = 0x21;
    let ctrl = CGEventFlags::CGEventFlagControl;

    for (code, down, flags) in [
        (LEFT_CTRL, true, ctrl),
        (LEFT_BRACKET, true, ctrl),
        (LEFT_BRACKET, false, ctrl),
        (LEFT_CTRL, false, CGEventFlags::CGEventFlagNull),
    ] {
        let event = CGEvent::new_keyboard_event(source.clone(), code, down)
            .expect("CGEventCreateKeyboardEvent failed");
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    println!("posted ctrl+[");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("synth is a macOS-only harness");
}
