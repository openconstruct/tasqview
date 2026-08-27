# tview

A Win95-style terminal multiplexer for the raw Linux console (VT). Written in
Rust. No X, no Wayland, no systemd-terminal-dependencies: run it on a kernel
VT (Ctrl+Alt+F1..F6) at your own risk enjoyment.

## Features

- One full-screen terminal session per "window", drawn on a single canvas
  with a 1-row Win95-style taskbar along the bottom.
- Sessions keep running in the background when unfocused.
- Clickable taskbar session buttons (left click focuses, click the red `X` to
  close). Works on a real VT via gpm.
- Mouse drag selects text; `Ctrl+Shift+C` copies, `Ctrl+Shift+V` pastes.
- Native scrollback (2000 lines): `PgUp`/`PgDn` scroll a page, mouse wheel
  scrolls 3 lines, `Esc` returns to the live view.
- `Alt+Tab` cycles sessions.
- System tray: clock + battery percentage (reads `/sys/class/power_supply/BAT*`).
- Proper resize handling via `SIGWINCH` (SGR pixel mouse, bell, bracketed
  paste, alternate screen are all handled through the vt100 emulator).

## Building on Debian 13 (trixie)

Debian 13 ships Rust 1.85, which is all you need:

```sh
sudo apt update
sudo apt install rustc cargo libc-dev        # Rust toolchain
sudo apt install gpm                         # console mouse daemon
```

Optional (smaller static binary):

```sh
sudo apt install musl-tools
rustup toolchain install stable --profile minimal   # rustup for musl target
rustup target add x86_64-unknown-linux-musl
```

Build:

```sh
cargo build --release
# or static:
# cargo build --release --target x86_64-unknown-linux-musl
```

Install:

```sh
sudo install -m 755 target/release/tview /usr/local/bin/tview
```

## Running

Run it only from a kernel console VT (not inside X, not over ssh):

```sh
sudo systemctl enable --now gpm          # if not already running
tview                                   # on a VT (Ctrl+Alt+F1..F6)
```

Keymap cheat sheet:

| Keys                     | Action                        |
|--------------------------|-------------------------------|
| `Ctrl+Shift+T`           | new session                   |
| `Alt+Tab`                | switch session                |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | copy / paste          |
| `Ctrl+Shift+W`           | close session                 |
| `Ctrl+Shift+X`           | quit                          |
| `PgUp` / `PgDn`, wheel   | scroll back / forward         |
| `Esc`                    | back to live view             |
| mouse left-click / drag  | focus button / select text    |

On a raw VT, `Ctrl+Shift+<key>` arrives as a plain control character; tview
disambiguates it with the kernel's `TIOCLINUX` shift-state ioctl, so plain
`Ctrl+C`, `Ctrl+V`, etc. still go straight to the focused program. If you run
tview on a non-VT tty (e.g. inside tmux, for testing only), the shortcuts fall
back gracefully: `Ctrl+C` copies when a selection exists, `Ctrl+V` pastes when
the clipboard is non-empty, and otherwise keys are forwarded unmodified.

## Notes

- No images, no revivable detach. The clipboard is internal only: the Linux
  console kernel exposes no API to write arbitrary text into its selection
  buffer.
- Sessions are children of tview and die with it. `Ctrl+Shift+X` (or closing
  the last session) sends `SIGHUP` to every child and restores the terminal.
- Dev/testing can be done under a pty (tmux/`script`); only the shift-state
  shortcut detection and mouse-on-console paths genuinely need a real VT.