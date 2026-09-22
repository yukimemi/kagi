//! `examples/smoke.rs` — release-time smoke target.
//!
//! `release.yml` runs `cargo run --release --target <T> --example smoke` on
//! every matrix entry, to catch regressions that `cargo test` misses: the
//! tests pass on every runner, but the shipped binary dies on real startup.
//!
//! kagi's startup path that can regress without any test noticing is config
//! loading: teravars renders the TOML, serde deserialises it, and the rules
//! compile against the per-platform key tables. A missing crate feature or a
//! key name that exists on one OS and not another shows up here and nowhere
//! else — `cargo test` runs on the host, this runs on every release target.
//!
//! Deliberately headless: no event tap, no evdev, no hook. Those need
//! permissions and a session that CI does not have.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config")
        .join("kagi.toml");

    // Examples get no `CARGO_BIN_EXE_*`; the binary sits one directory up
    // from `target/<profile>/examples/smoke`.
    let bin = match std::env::current_exe().ok().and_then(|exe| {
        let dir = exe.parent()?.parent()?.to_path_buf();
        Some(dir.join(format!("kagi{}", std::env::consts::EXE_SUFFIX)))
    }) {
        Some(b) => b,
        None => {
            eprintln!("smoke: could not locate the kagi binary");
            return ExitCode::FAILURE;
        }
    };

    let output = std::process::Command::new(&bin)
        .args(["--config", &path.to_string_lossy(), "check"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("smoke: could not run the kagi binary: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !output.status.success() {
        eprintln!(
            "smoke: `kagi check` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return ExitCode::FAILURE;
    }

    // The shipped config must yield at least one rule on every platform; an
    // empty result means the `os` filter or the key tables lost something.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains(": 0 rule(s) for ") {
        eprintln!("smoke: the shipped config compiled to no rules on this platform:\n{stdout}");
        return ExitCode::FAILURE;
    }

    print!("smoke: {stdout}");
    ExitCode::SUCCESS
}
