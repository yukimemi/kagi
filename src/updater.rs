//! Self-update, via [`kaishin`].
//!
//! kagi's `run` blocks forever inside a platform event loop, so there is never
//! a good moment to print an "update available" banner. Instead the update is
//! applied silently in the background and takes effect on the next launch,
//! which is also how a long-running key mapper should behave.

use anyhow::Result;
use std::thread;

/// Environment variable that opts out of the silent background update.
pub const NO_AUTOUPDATE: &str = "KAGI_NO_AUTOUPDATE";

fn options() -> kaishin::KaishinOptions {
    // The GitHub repo and the binary are both `kagi`; the crates.io package is
    // `kagikey`, because `kagi` was already taken. The `cargo install`
    // fallback path needs the package name, not the binary name.
    kaishin::KaishinOptions::new("yukimemi", "kagi", "kagi", env!("CARGO_PKG_VERSION"))
        .crate_name(env!("CARGO_PKG_NAME"))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// `kagi update`: check for, and optionally install, a newer release.
pub fn run_self_update(yes: bool, check_only: bool, non_interactive: bool) -> Result<()> {
    let upd_opts = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);
    runtime()?.block_on(kaishin::run_self_update(&options(), upd_opts))
}

/// Fire-and-forget silent update on a detached thread.
///
/// kaishin self-throttles (24 h), skips dev builds under `target/`, and
/// serialises across processes with an advisory lock, so calling this on every
/// start is safe. It gets its own current-thread runtime because kagi has no
/// ambient one: the main thread is about to be taken over by the platform
/// event loop.
pub fn spawn_auto_update() {
    if std::env::var_os(NO_AUTOUPDATE).is_some() {
        return;
    }
    thread::spawn(|| {
        let Ok(rt) = runtime() else { return };
        let checker = kaishin::Checker::new("kagi", options());
        let _ = rt.block_on(checker.auto_update());
    });
}
