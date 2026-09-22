//! `kagi service` — register kagi as a per-user background service.
//!
//! Each platform gets the mechanism that actually fits a key-mapping daemon,
//! rather than one lowest-common-denominator autostart entry:
//!
//! * macOS — a launchd **LaunchAgent**. `ProcessType = Interactive` keeps
//!   launchd from throttling keyboard handling behind background QoS.
//! * Linux — a **systemd user unit** bound to the graphical session. kagi
//!   grabs evdev devices, so it must come up with the session and go down
//!   with it; a `.desktop` autostart entry gives neither restart-on-failure
//!   nor an ordering guarantee.
//! * Windows — a **logon scheduled task** driving a `wscript` shim, because a
//!   console binary started from the Startup folder leaves a window on screen
//!   for as long as the daemon runs.
//!
//! Everything is per-user. Nothing here needs root or an installer.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

fn exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running kagi binary")?;
    // `current_exe` can hand back a symlink in the target directory; resolve
    // it so the service keeps pointing at the same binary after a rebuild.
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot locate a home directory")
}

/// Run `program` and turn a non-zero exit into an error carrying stderr.
fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program}`"))?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Same, but a failure is expected and uninteresting (tearing down something
/// that may not be registered).
fn run_quiet(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).output();
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// The `run` arguments the service is registered with.
fn run_args(config: Option<&Path>) -> Vec<String> {
    match config {
        Some(c) => vec!["--config".into(), c.display().to_string(), "run".into()],
        None => vec!["run".into()],
    }
}

// ===========================================================================
// macOS
// ===========================================================================

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// launchd job label, and the plist filename.
    const LABEL: &str = "com.yukimemi.kagi";

    fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// Where the agent actually runs from: inside a real `.app` bundle at
    /// `~/Applications/kagi.app`, never `current_exe()` (`~/.cargo/bin/kagi`)
    /// directly.
    ///
    /// Two things pushed this here, both load-bearing:
    ///
    /// 1. **TCC keys an Accessibility/Input Monitoring grant to the
    ///    executable path, not just its code signature**, and appears to
    ///    cache that per-path association *permanently* — exhaustively
    ///    confirmed on `~/.cargo/bin/kagi` during development: fixing the
    ///    signature (stable identifier, stable `LC_UUID`, valid Designated
    ///    Requirement — see `platform::macos::ensure_stable_identity`),
    ///    switching to a never-before-seen identifier, and the user manually
    ///    removing the Settings row with `−` and re-adding it with `+`, ALL
    ///    still failed with the identical stale `SecStaticCodeCheckValidity`
    ///    error (`errSecCSReqFailed`/`-67050` against a specific old
    ///    `cdhash`), while an *identical* binary at a fresh path granted
    ///    normally every time. `tccd` restart is SIP-blocked
    ///    (`launchctl kickstart` on `com.apple.tccd` answers "Operation not
    ///    permitted while System Integrity Protection is engaged"), and
    ///    `tccutil reset <service> <path-or-identifier>` refuses anything
    ///    that is not a real, LaunchServices-registered bundle identifier
    ///    ("No such bundle identifier"). There is no user-space fix for a
    ///    poisoned path; only moving off it works.
    /// 2. A **real bundle**, not a bare copied executable, makes that
    ///    `tccutil reset <service> <bundle-id>` command actually work if this
    ///    path *ever* gets poisoned too — a targeted reset that, unlike
    ///    `kagi permissions --reset`, does not revoke every other
    ///    application's grants for the same service. Matches
    ///    [paneru](https://github.com/karinushka/paneru)'s own
    ///    `install-app`.
    fn app_bundle() -> Result<PathBuf> {
        Ok(home()?.join("Applications").join("kagi.app"))
    }

    fn app_executable() -> Result<PathBuf> {
        Ok(app_bundle()?.join("Contents").join("MacOS").join("kagi"))
    }

    /// The bundle's `CFBundleIdentifier` — see `platform::macos::BUNDLE_ID`
    /// for the full story on why it is its own constant, not [`LABEL`] and
    /// not `crate::platform::SIGN_ID`.
    use crate::platform::BUNDLE_ID;

    fn info_plist() -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>{BUNDLE_ID}</string>
  <key>CFBundleExecutable</key>
  <string>kagi</string>
  <key>CFBundleName</key>
  <string>kagi</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
  <!-- No Dock icon, no menu bar: kagi has no UI of its own. -->
  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
"#,
            version = env!("CARGO_PKG_VERSION"),
        )
    }

    /// Copy the current binary into a freshly (re)written `~/Applications/
    /// kagi.app`, so the registered service always runs the latest build
    /// from its own dedicated, bundle-identified path.
    fn deploy() -> Result<PathBuf> {
        let src = exe()?;
        let dst = app_executable()?;
        write_file(
            &app_bundle()?.join("Contents").join("Info.plist"),
            &info_plist(),
        )?;
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::copy(&src, &dst)
            .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        // Two overrides on top of plain ad-hoc `--sign -`, both load-bearing
        // across a rebuild:
        //
        // * `--deep` covers the bundle as a whole; a bare `--sign` on just
        //   the executable leaves Info.plist unsealed and `codesign
        //   --verify` on the bundle fails with "bundle format unrecognized,
        //   invalid, or unsuitable".
        // * `-r=<expr>` pins the designated requirement to the identifier
        //   alone. Ad-hoc signing's *default* designated requirement pins
        //   the content hash instead (`designated => cdhash H"..."`),
        //   confirmed with `codesign -d -r-` on a bundle signed without
        //   this override — which changes on every `cargo install`/rebuild.
        //   TCC's grant did not survive a rebuild without this override:
        //   observed directly, a fresh `kagi service install` right after
        //   one that had just been granted came back to `CGEventTapCreate
        //   failed` / "NOT granted" again. Same fix, same reason, as
        //   `ensure_stable_identity`'s `-r=` on the raw-binary path — see
        //   its doc comment for the argv-joining gotcha (`-r=<expr>` must be
        //   one argument, not two).
        run(
            "codesign",
            &[
                "--force",
                "--deep",
                "--sign",
                "-",
                "--identifier",
                BUNDLE_ID,
                &format!("-r=designated => identifier \"{BUNDLE_ID}\""),
                &app_bundle()?.display().to_string(),
            ],
        )?;
        Ok(dst)
    }

    /// launchd addresses a per-user domain by uid. `id -u` avoids pulling in
    /// libc just for `getuid`.
    fn domain() -> Result<String> {
        let uid = run("id", &["-u"])?;
        Ok(format!("gui/{}", uid.trim()))
    }

    fn target() -> Result<String> {
        Ok(format!("{}/{LABEL}", domain()?))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn plist(exe: &Path, config: Option<&Path>) -> String {
        let mut args = String::new();
        args.push_str(&format!(
            "    <string>{}</string>\n",
            xml_escape(&exe.display().to_string())
        ));
        for a in run_args(config) {
            args.push_str(&format!("    <string>{}</string>\n", xml_escape(&a)));
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Generated by `kagi service install`. Re-run it to regenerate. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>

  <key>ProgramArguments</key>
  <array>
{args}  </array>

  <key>EnvironmentVariables</key>
  <dict>
    <!-- macOS keys Accessibility and Input Monitoring to this binary. A
         background self-update swaps it out, and the agent then runs unable
         to see any key until the grants are renewed, with no prompt because
         nothing is in the foreground. Update deliberately: `kagi update`. -->
    <key>KAGI_NO_AUTOUPDATE</key>
    <string>1</string>
  </dict>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <!-- No ThrottleInterval override on purpose: `launchctl kickstart` blocks
       until the job actually spawns, so raising it makes `kagi service start`
       hang for that long. The repeated-prompt problem it would paper over is
       already solved by asking for a permission only once. -->

  <!-- Keyboard handling must not be throttled behind background QoS. -->
  <key>ProcessType</key>
  <string>Interactive</string>

  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
            log = xml_escape(&log_path().display().to_string()),
        )
    }

    /// `~/Library/Logs` is where a user agent's log belongs, and unlike
    /// `std::env::temp_dir()` (a per-boot `/var/folders/...` path on macOS) it
    /// stays quotable and survives reboots.
    pub fn log_path() -> PathBuf {
        match home() {
            Ok(h) => h.join("Library").join("Logs").join("kagi.log"),
            Err(_) => PathBuf::from("/tmp/kagi.log"),
        }
    }

    pub fn install(config: Option<&Path>) -> Result<()> {
        // A concurrently respawning `KeepAlive` daemon calls
        // `platform::macos::request_permissions(Prompt::Once)` on every
        // failed start (a lightweight, non-blocking check+open) — running
        // that at the same time as this function's own interactive,
        // dialog-driven flow can pop two overlapping system prompts. Stop it
        // first; `start()` at the end brings it back once permissions are in
        // place.
        run_quiet("launchctl", &["bootout", &target()?]);

        let agent = deploy()?;
        let path = plist_path()?;
        write_file(&path, &plist(&agent, config))?;
        println!("wrote {}", path.display());

        run(
            "launchctl",
            &["bootstrap", &domain()?, &path.display().to_string()],
        )?;
        println!("loaded {}", target()?);
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        run_quiet("launchctl", &["bootout", &target()?]);
        let path = plist_path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
        }
        let bundle = app_bundle()?;
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle)
                .with_context(|| format!("removing {}", bundle.display()))?;
            println!("removed {}", bundle.display());
        }
        println!("unloaded {}", target()?);
        Ok(())
    }

    pub fn start() -> Result<()> {
        run("launchctl", &["kickstart", "-k", &target()?])?;
        println!("started {}", target()?);
        Ok(())
    }

    pub fn stop() -> Result<()> {
        run("launchctl", &["kill", "SIGTERM", &target()?])?;
        println!("signalled {}", target()?);
        Ok(())
    }

    pub fn status() -> Result<()> {
        let path = plist_path()?;
        println!("plist: {} ({})", path.display(), exists(&path));
        match Command::new("launchctl")
            .args(["print", &target()?])
            .output()
        {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    let t = line.trim();
                    if t.starts_with("state =")
                        || t.starts_with("pid =")
                        || t.starts_with("last exit code =")
                    {
                        println!("{t}");
                    }
                }
            }
            _ => println!("state = not loaded"),
        }
        println!("log: {}", log_path().display());
        Ok(())
    }

    /// Permission note printed after a successful install.
    pub fn after_install() -> Result<()> {
        println!(
            "\nmacOS grants Accessibility and Input Monitoring per binary, and the\n\
             agent is not the terminal you just used. The first run therefore fails\n\
             with `CGEventTapCreate failed` until you add\n  {}\n\
             under System Settings > Privacy & Security > Accessibility, and again\n\
             under Input Monitoring. Then:\n  launchctl kickstart -k {}\n\
             Check progress with `kagi service status` and {}.",
            app_executable()?.display(),
            target()?,
            log_path().display()
        );
        Ok(())
    }
}

// ===========================================================================
// Linux
// ===========================================================================

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    const UNIT: &str = "kagi.service";

    fn unit_path() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) => PathBuf::from(x),
            None => home()?.join(".config"),
        };
        Ok(base.join("systemd").join("user").join(UNIT))
    }

    fn unit(exe: &Path, config: Option<&Path>) -> String {
        let args = run_args(config).join(" ");
        format!(
            "# Generated by `kagi service install`. Re-run it to regenerate.\n\
             [Unit]\n\
             Description=kagi — cross-platform key mapper with first-class IME control\n\
             Documentation=https://github.com/yukimemi/kagi\n\
             After=graphical-session.target\n\
             PartOf=graphical-session.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} {args}\n\
             Restart=on-failure\n\
             RestartSec=2\n\
             # A key mapper that loses to the scheduler feels like broken hardware.\n\
             Nice=-5\n\
             \n\
             [Install]\n\
             WantedBy=graphical-session.target\n",
            exe = exe.display(),
        )
    }

    pub fn install(config: Option<&Path>) -> Result<()> {
        let path = unit_path()?;
        write_file(&path, &unit(&exe()?, config))?;
        println!("wrote {}", path.display());

        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", UNIT])?;
        println!("enabled {UNIT}");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        run_quiet("systemctl", &["--user", "disable", "--now", UNIT]);
        let path = unit_path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
        }
        run_quiet("systemctl", &["--user", "daemon-reload"]);
        println!("disabled {UNIT}");
        Ok(())
    }

    pub fn start() -> Result<()> {
        run("systemctl", &["--user", "restart", UNIT])?;
        println!("started {UNIT}");
        Ok(())
    }

    pub fn stop() -> Result<()> {
        run("systemctl", &["--user", "stop", UNIT])?;
        println!("stopped {UNIT}");
        Ok(())
    }

    pub fn status() -> Result<()> {
        let path = unit_path()?;
        println!("unit: {} ({})", path.display(), exists(&path));
        let out = Command::new("systemctl")
            .args(["--user", "--no-pager", "status", UNIT])
            .output();
        match out {
            Ok(o) => print!("{}", String::from_utf8_lossy(&o.stdout)),
            Err(e) => println!("systemctl unavailable: {e}"),
        }
        Ok(())
    }

    pub fn after_install() -> Result<()> {
        println!(
            "\nkagi reads /dev/input/event* and writes /dev/uinput. If the service\n\
             fails with a permission error:\n  \
             sudo usermod -aG input $USER    # then log out and back in\n  \
             echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"' \\\n    \
             | sudo tee /etc/udev/rules.d/99-kagi-uinput.rules\n  \
             sudo modprobe uinput && sudo udevadm control --reload-rules\n\
             Logs: journalctl --user -u kagi -f"
        );
        Ok(())
    }
}

// ===========================================================================
// Windows
// ===========================================================================

#[cfg(target_os = "windows")]
mod imp {
    use super::*;

    const TASK: &str = "kagi";

    fn shim_path() -> Result<PathBuf> {
        let base = match std::env::var_os("LOCALAPPDATA") {
            Some(x) => PathBuf::from(x),
            None => home()?.join("AppData").join("Local"),
        };
        Ok(base.join("kagi").join("kagi-hidden.vbs"))
    }

    /// `wscript` with window style 0 is the one way to start a console binary
    /// on Windows without leaving a console window on screen for the life of
    /// the process, while still running inside the interactive session — which
    /// kagi needs, because its IME calls target the foreground window.
    fn shim(exe: &Path, config: Option<&Path>) -> String {
        let mut cmd = format!("\"\"{}\"\"", exe.display());
        for a in run_args(config) {
            cmd.push_str(&format!(" \"\"{a}\"\""));
        }
        format!(
            "' Generated by `kagi service install`. Re-run it to regenerate.\r\n\
             Set sh = CreateObject(\"WScript.Shell\")\r\n\
             sh.Run \"{cmd}\", 0, False\r\n"
        )
    }

    pub fn install(config: Option<&Path>) -> Result<()> {
        let shim_file = shim_path()?;
        write_file(&shim_file, &shim(&exe()?, config))?;
        println!("wrote {}", shim_file.display());

        let action = format!("wscript.exe \"{}\"", shim_file.display());
        run(
            "schtasks",
            &[
                "/Create", "/TN", TASK, "/TR", &action, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
            ],
        )?;
        println!("registered logon task `{TASK}`");
        start()
    }

    pub fn uninstall() -> Result<()> {
        run_quiet("schtasks", &["/End", "/TN", TASK]);
        run_quiet("schtasks", &["/Delete", "/TN", TASK, "/F"]);
        let shim_file = shim_path()?;
        if shim_file.exists() {
            std::fs::remove_file(&shim_file)
                .with_context(|| format!("removing {}", shim_file.display()))?;
            println!("removed {}", shim_file.display());
        }
        println!("removed logon task `{TASK}`");
        Ok(())
    }

    pub fn start() -> Result<()> {
        run("schtasks", &["/Run", "/TN", TASK])?;
        println!("started `{TASK}`");
        Ok(())
    }

    pub fn stop() -> Result<()> {
        run("schtasks", &["/End", "/TN", TASK])?;
        println!("stopped `{TASK}`");
        Ok(())
    }

    pub fn status() -> Result<()> {
        let shim_file = shim_path()?;
        println!("shim: {} ({})", shim_file.display(), exists(&shim_file));
        match Command::new("schtasks")
            .args(["/Query", "/TN", TASK, "/FO", "LIST"])
            .output()
        {
            Ok(o) if o.status.success() => print!("{}", String::from_utf8_lossy(&o.stdout)),
            _ => println!("state = not registered"),
        }
        Ok(())
    }

    pub fn after_install() -> Result<()> {
        println!(
            "\nA low-level keyboard hook cannot see input destined for a window\n\
             running at a higher integrity level. If remapping stops working in an\n\
             elevated app, re-register with an elevated shell and `/RL HIGHEST`."
        );
        Ok(())
    }
}

fn exists(path: &Path) -> &'static str {
    if path.exists() { "present" } else { "absent" }
}

pub fn install(config: Option<&Path>) -> Result<()> {
    imp::install(config)?;
    imp::after_install()
}

pub fn uninstall() -> Result<()> {
    imp::uninstall()
}

pub fn start() -> Result<()> {
    imp::start()
}

pub fn stop() -> Result<()> {
    imp::stop()
}

pub fn status() -> Result<()> {
    imp::status()
}
