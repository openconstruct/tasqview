# tview

A Win95-style terminal multiplexer for the raw Linux console (VT). Written in
Rust. No X, no Wayland - run it on a kernel VT (Ctrl+Alt+F1..F6).

## Features

- Full-screen terminal sessions on one canvas, with a Win95-style taskbar.
- Sessions keep running in the background when unfocused.
- Clickable taskbar tabs: focus on left click, red `x` closes. Works on a real
  VT via a console mouse daemon (`consolation` or `gpm`).
- Mouse drag selects text; `Alt+Shift+C` copies, `Alt+Shift+V` pastes, with a
  toast confirmation.
- **Start** button (bottom-left): launchers from a menu file / New tab / Exit.
- 2000-line scrollback: `PgUp`/`PgDn` page, wheel scrolls 3 lines, `Esc` returns.
- `Alt+Tab` cycles sessions. Tray shows clock + battery.
- Resize handled via SIGWINCH; SGR pixel mouse, bell, bracketed paste and the
  alternate screen run through the vt100 emulator.

## Build

```sh
cargo build --release            # static: --target x86_64-unknown-linux-musl
sudo install -m 755 target/release/tview /usr/local/bin/tview
```

Needs `rustc`/`cargo`; for a real VT also enable a console mouse daemon:
`sudo systemctl enable --now consolation` (or `gpm`).

## Run

Run only from a kernel console VT (not inside X, not over ssh):

```sh
tview
```

Note: the Linux kernel reports only mouse press/release positions on a VT (no
motion), so selection appears on button-up; tview owns all mouse input and does
not forward it to child programs.

## Keys

| Keys                        | Action                                |
|-----------------------------|---------------------------------------|
| `Ctrl+B` prefix             | `c` copy · `v` paste · `s` select · `t` new · `w` close · `x` quit · `n` next |
| `Alt+Shift+T`               | new session                           |
| `Alt+Tab`                   | switch session                        |
| `Alt+Shift+C` / `Alt+Shift+V` | copy / paste                        |
| `Alt+Shift+S`               | keyboard select (arrows extend, Enter copies, Space re-anchors, Esc cancels) |
| `Alt+Shift+W` / `Alt+Shift+X` | close session / quit                 |
| `PgUp` / `PgDn`, wheel      | scroll back / forward                 |
| `Esc`                       | back to live view                     |
| mouse                       | click tab to focus · drag to select · `x` closes |

`Alt+<key>` and its `Alt+Shift+<key>` forms are both accepted, so a keymap that
drops Shift still works (children lose `Alt+c/v/t/w/s`). `Ctrl+Shift+<key>` is a
VT-only best-effort fallback; `Ctrl+C` and `Ctrl+V` always reach the child.

## Start menu

The menu always lists launchers (from a text file, re-read on every open)
followed by **New tab** and **Exit**. File lookup: `$TASQVIEW_MENU`, else
`$XDG_CONFIG_HOME/tasqview/menu`, else `~/.config/tasqview/menu`.

Format: `key: command` per line, one accelerator key (blank for none), run as
`sh -c '<command>'` in a new tab:

```
h: htop
m: mc
e: nvim /etc/fstab
# blank lines and lines starting with # are ignored
```

## Notes

- No detach/revive; the clipboard is internal only (the console kernel exposes
  no API to write the selection buffer). Sessions are children of tview and die
  with it; exiting sends SIGHUP and restores the terminal.
- `TVIEW_DEBUG=1` logs input decisions to `/tmp/tview-debug.log`.
- Dev/testing works under a pty (`script`, tmux); only the shift-state
  shortcuts and console mouse need a real VT. `Ctrl+B` chords are the canonical
  shortcuts and work everywhere.