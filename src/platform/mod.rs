//! Backend contract. Each platform owns capture, synthesis and IME control;
//! everything above this line is platform-neutral.

use crate::action::{Action, ImeState};
use crate::config::Config;
use crate::engine::Engine;
use crate::keys::Chord;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// Synthesis side of a backend: what a matched rule can drive.
pub trait Emitter {
    /// Press and release `chord`, with only `chord.mods` applied — the
    /// physically held modifiers must not leak into the synthetic event.
    fn tap(&mut self, chord: Chord) -> Result<()>;
    /// Open/close/toggle the IME of the focused window.
    fn ime(&mut self, state: ImeState) -> Result<()>;
    /// Select an input source by platform-native id.
    fn input_source(&mut self, id: &str) -> Result<()>;
}

/// Run a matched rule's actions in order.
pub fn dispatch(emitter: &mut dyn Emitter, actions: &[Action]) -> Result<()> {
    for action in actions {
        match action {
            Action::Tap(chord) => emitter.tap(*chord)?,
            Action::Ime(state) => emitter.ime(*state)?,
            Action::InputSource(id) => emitter.input_source(id)?,
            Action::Cmd(cmd) => spawn_detached(cmd)?,
        }
    }
    Ok(())
}

/// Fire-and-forget shell command. Reaped on a helper thread so a long-lived
/// daemon does not accumulate zombies.
pub fn spawn_detached(cmd: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning `{cmd}`"))?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Run `cmd` and report whether it exited successfully. Used by the Linux
/// backend to probe IME helpers (fcitx5, ibus) in order.
#[allow(dead_code)]
pub fn run_blocking(cmd: &str) -> bool {
    let mut command = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
compile_error!("kagi supports macOS, Windows and Linux only");

/// Capture keys and apply `engine` until interrupted.
pub fn run(engine: Engine, config: &Config) -> Result<()> {
    imp::run(engine, config)
}

/// Print key events as they arrive, without remapping. Used to discover the
/// names to put in a config.
pub fn watch(config: &Config) -> Result<()> {
    imp::watch(config)
}

/// Ask the OS for whatever kagi needs to capture keys, and report whether it
/// has it. Safe to call repeatedly.
#[cfg(target_os = "macos")]
pub fn request_permissions() -> Result<bool> {
    imp::request_permissions(imp::Prompt::Always)
}

/// Drop existing capture grants so they can be granted afresh.
#[cfg(target_os = "macos")]
pub fn reset_permissions() -> Result<()> {
    imp::reset_permissions()
}

#[cfg(not(target_os = "macos"))]
pub fn reset_permissions() -> Result<()> {
    anyhow::bail!("--reset only applies to macOS Privacy & Security grants")
}

#[cfg(not(target_os = "macos"))]
pub fn request_permissions() -> Result<bool> {
    // Only macOS gates capture behind a per-binary grant that has to be
    // requested before the binary even appears in the settings list. Linux
    // needs device permissions, which `kagi run` already reports with the
    // exact remedy, and a Windows hook needs nothing below its own integrity
    // level.
    println!(
        "no per-binary permission grant is required on {}",
        std::env::consts::OS
    );
    Ok(true)
}
