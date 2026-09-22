<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kagi/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/kagi/main/assets/logo.svg" alt="kagi — cross-platform key mapper with first-class IME control" width="540">
  </picture>
</p>

# kagi

Cross-platform key mapper with **IME on/off as a first-class action**.

`Ctrl+[` → `Esc` *and close the IME*, with one config, on macOS, Windows and
Linux.

## Why another remapper

kanata, kmonad, Karabiner-Elements and xremap all remap keys well. None of them
treats "turn the IME off" as an action — you end up shelling out, or writing
per-OS glue like an AutoHotkey `ImmSetOpenStatus` helper next to a Karabiner
JSON rule next to an xremap YAML. kagi makes that one line:

```toml
[[rule]]
from = "ctrl-["
to = ["esc", "ime:off"]
```

kagi also needs no kernel extension and no DriverKit driver on macOS — an event
tap plus Accessibility permission is enough.

## Install

```sh
cargo install kagikey    # the crate is `kagikey`; the binary is `kagi`
cargo install --path .   # or from a checkout
```

kagi keeps itself up to date in the background and exposes `kagi update`.
Packagers who own updates themselves can drop that (and with it the
reqwest/rustls tree) entirely:

```sh
cargo install kagikey --no-default-features
```

## Configure

kagi reads `$KAGI_CONFIG`, else `~/.config/kagi/kagi.toml`, else
`%APPDATA%\kagi\kagi.toml`. See [`config/kagi.toml`](config/kagi.toml).

The file is rendered by [teravars](https://github.com/yukimemi/teravars)
before it is parsed, so one config can cover several machines: `[vars]` for
your own values, `{{ system.os }}` / `{{ system.host }}` / `{{ system.user }}`
for the machine, and `include` to pull in a shared fragment.

```toml
[[rule]]
desc = "Ctrl+[ -> Esc, and close the IME"
from = "ctrl-["
to = ["esc", "ime:off"]

[[rule]]
desc = "Esc also closes the IME"
from = "~esc"
to = ["ime:off"]

[[rule]]
from = "~henkan"
to = ["ime:on"]
os = ["windows", "linux"]

[[rule]]
from = "~muhenkan"
to = ["ime:off"]
os = ["windows", "linux"]
```

### `from`

A chord such as `ctrl-[`, `ctrl-shift-a`, `esc`. `-` separates modifiers, so a
literal hyphen is `minus` (or the trailing `-` in `ctrl--`).

Modifiers: `ctrl`, `shift`, `alt` (Option on macOS), `meta` (Command / Win).
Modifiers must match **exactly** unless the rule is wildcarded.

Two prefixes, borrowed from AutoHotkey:

| prefix | meaning |
|---|---|
| `~` | the original key still reaches the focused app |
| `*` | extra modifiers beyond the listed ones are tolerated |

### `to`

| entry | effect |
|---|---|
| `esc`, `ctrl-a`, … | synthesize that key |
| `ime:on` / `ime:off` / `ime:toggle` | drive the platform IME |
| `source:<id>` | select an input source (macOS TIS id, Linux IME engine) |
| `cmd:<shell command>` | run a command, detached |

An empty `to` swallows the key.

### `os`

`os = ["windows", "linux"]` restricts a rule to those platforms. Omit it for
every platform.

## Commands

```sh
kagi run         # capture and remap (default)
kagi check       # parse the config and print the rules that apply here
kagi watch       # print key events as they arrive, to discover key names
kagi service     # install | uninstall | status | start | stop
kagi permissions # request the OS grants kagi needs, and report what is missing
kagi update      # install the latest release (--check to only look)
```

`kagi watch` is the way to find the name of a key your keyboard actually sends:

```
down  ctrl-[             (keycode=0x21 flags=0x20040000)
```

## Run it at login

```sh
kagi service install
```

kagi is a daemon, so this registers the mechanism each platform actually
wants rather than one generic autostart entry. Everything is per-user; none
of it needs root.

| | mechanism | why not the obvious one |
|---|---|---|
| macOS | launchd LaunchAgent | `ProcessType = Interactive` keeps launchd from throttling keyboard handling behind background QoS |
| Linux | systemd user unit, `PartOf=graphical-session.target` | kagi *grabs* evdev devices, so it has to come up and go down with the session; a `.desktop` autostart entry gives no ordering and no restart-on-failure |
| Windows | logon scheduled task driving a `wscript` shim | a console binary launched from the Startup folder leaves a window on screen for as long as the daemon runs |

`install` compiles your config first and refuses to register a service that
would die on startup. Pass `--config` to bake a non-default path into the
registration.

`kagi service status` reports registration and run state; `uninstall`,
`start` and `stop` do what they say.

### macOS permissions

An event tap needs **Accessibility** *and* **Input Monitoring**, and macOS
grants both per binary — the agent is not the terminal you installed from, so
its first run fails regardless of what your terminal is allowed to do.

A binary that has never *asked* does not even appear in those lists; the only
way in would be the `+` button and a file picker aimed at `~/.cargo/bin`. So
kagi asks, through `IOHIDRequestAccess` and `AXIsProcessTrustedWithOptions`,
which is what registers the entry. `kagi service install` does it for you,
`kagi permissions` repeats it on demand, and a failing start does it once.

Tick both entries, then `kagi service start`. Logs go to
`~/Library/Logs/kagi.log`.

macOS keys the grant to the binary's **signing identifier**, not its path.
`cargo build` leaves a linker ad-hoc signature whose identifier embeds a hash
of the binary (`kagi-bef9cabe50a08b72`), so every rebuild looks like a
different application to TCC — the old grant goes stale and the Privacy list
accumulates a dead `kagi` row per build. `kagi permissions` re-signs the
running binary with a fixed identifier (`com.yukimemi.kagi`) before asking,
which keeps future rebuilds landing on the one row.

A binary that predates this fix already has a stale row that no amount of
toggling helps, because it isn't the one being checked anymore. `tccutil`
resolves through LaunchServices and only accepts a real bundle identifier, so
an unbundled CLI can't be singled out for a targeted reset — the only
scripted fix is the whole service:

```sh
kagi permissions --reset   # tccutil reset Accessibility + ListenEvent —
                            # clears the grant for *every* application
```

That is why the generated agent sets `KAGI_NO_AUTOUPDATE=1`: a silent
self-update would swap the binary out and leave the agent running blind, with
nothing in the foreground to prompt you. Update deliberately with
`kagi update`, then re-tick if `kagi service status` shows the agent failing.

## How each platform does it

| | capture | IME |
|---|---|---|
| macOS | `CGEventTap` at `kCGHIDEventTap` | posts the JIS 英数/かな keycodes, or `TISSelectInputSource` |
| Windows | `WH_KEYBOARD_LL` hook + `SendInput` | `ImmGetDefaultIMEWnd` + `WM_IME_CONTROL`/`IMC_SETOPENSTATUS` |
| Linux | evdev + uinput (works under X11 *and* Wayland) | `fcitx5-remote`, else `ibus` |

### Permissions

* **macOS** — System Settings ▸ Privacy & Security ▸ **Accessibility** and
  **Input Monitoring**, for the kagi binary (or the terminal launching it).
  Granting them requires restarting the granted app. Event taps are bypassed
  while a secure input field has focus, and at the login window.
* **Windows** — an elevated foreground window only receives hooked input if
  kagi runs elevated too.
* **Linux** — read access to `/dev/input/event*` (`sudo usermod -aG input
  $USER`) and write access to `/dev/uinput` (udev rule, or run as root).

## macOS IME method

```toml
[macos]
ime = "eisu"   # or "source"
```

* `eisu` (default) posts the 英数 / かな keycodes a JIS keyboard sends. Every
  Japanese IME honours them and the selected input method is preserved — the
  closest analogue to AutoHotkey's `ImmSetOpenStatus`.
* `source` switches the input source outright via Text Input Services, using
  `ascii_source` / `japanese_source`.

## Development

```sh
cargo test
cargo check --target x86_64-pc-windows-msvc
cargo check --target x86_64-unknown-linux-gnu
```

`cargo run --example synth` posts a synthetic `Ctrl+[` so the macOS tap can be
exercised without a human at the keyboard. Run `kagi watch` first, then
`kagi run`, then `synth`: the watcher should report `esc` and `eisu` rather
than `[`.

## License

MIT
