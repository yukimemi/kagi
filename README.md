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
cargo install --path .
```

## Configure

kagi reads `$KAGI_CONFIG`, else `~/.config/kagi/kagi.toml`, else
`%APPDATA%\kagi\kagi.toml`. See [`config/kagi.toml`](config/kagi.toml).

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
kagi run      # capture and remap (default)
kagi check    # parse the config and print the rules that apply here
kagi watch    # print key events as they arrive, to discover key names
```

`kagi watch` is the way to find the name of a key your keyboard actually sends:

```
down  ctrl-[             (keycode=0x21 flags=0x20040000)
```

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
