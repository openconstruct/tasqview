#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

mod battery;
mod clipboard;
mod console;
mod input;
mod screen;
mod selection;
mod session;
mod taskbar;
mod term;

use std::io;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, Signal, SigSet};

static WINCH: AtomicBool = AtomicBool::new(false);
extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

struct App {
    rows: usize,
    cols: usize,
    view_rows: usize,
    view_cols: usize,
    canvas: screen::Canvas,
    sessions: Vec<session::Session>,
    active: usize,
    inptr: input::Parser,
    sel: selection::Sel,
    keysel: Option<KeySel>,
    clip: clipboard::Clipboard,
    batt: Option<battery::Battery>,
    on_console: bool,
    quit: bool,
    layout: taskbar::Layout,
    /// Transient status message (text, shown-at); cleared after ~1.6s.
    toast: Option<(String, std::time::Instant)>,
    /// Start-menu state: Some(selected_index) when open.
    menu: Option<usize>,
    /// Entries for the currently open menu (rebuilt from disk each open).
    menu_cache: Vec<MenuEntry>,
    /// True after the Ctrl+B prefix key, waiting for the command key.
    prefix: bool,
    /// True after a quit request, waiting for y/n confirmation.
    quit_confirm: bool,
}

/// Minimum start-menu width; grows to fit the longest entry.
const MENU_MIN_WIDTH: usize = 14;

#[derive(Clone)]
enum MenuAction {
    NewTab,
    Exit,
    Run(String),
}

#[derive(Clone)]
struct MenuEntry {
    /// Optional accelerator key (typed while the menu is open).
    key: Option<char>,
    label: String,
    action: MenuAction,
}

/// Path to the user's start-menu file. `$TASQVIEW_MENU` wins, else
/// `$XDG_CONFIG_HOME/tasqview/menu`, else `~/.config/tasqview/menu`.
fn menu_file_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("TASQVIEW_MENU") {
        if !p.is_empty() {
            return Some(p.into());
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!("{}/.config", std::env::var("HOME").unwrap_or_default())
        });
    Some(std::path::PathBuf::from(base).join("tasqview").join("menu"))
}

/// Display name for a menu command: basename of the first word.
fn menu_cmd_name(cmd: &str) -> String {
    let first = cmd.split_whitespace().next().unwrap_or(cmd);
    first.rsplit('/').next().unwrap_or(first).to_string()
}

/// Parse the launcher section of a menu file (`key: command` per line).
fn parse_menu_text(txt: &str) -> Vec<MenuEntry> {
    let mut v = Vec::new();
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, cmd)) = line.split_once(':') else {
            continue;
        };
        let (k, cmd) = (k.trim(), cmd.trim());
        if cmd.is_empty() {
            continue;
        }
        let key = if k.chars().count() == 1 {
            k.chars().next()
        } else {
            None
        };
        let label = if k.is_empty() {
            menu_cmd_name(cmd)
        } else {
            format!("{k}  {}", menu_cmd_name(cmd))
        };
        v.push(MenuEntry {
            key,
            label,
            action: MenuAction::Run(cmd.to_string()),
        });
    }
    v
}

/// Build the start-menu entries: user file lines first (`key: command`),
/// then the always-present "New tab" and "Exit" built-ins.
fn load_menu_entries() -> Vec<MenuEntry> {
    let mut v = menu_file_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|txt| parse_menu_text(&txt))
        .unwrap_or_default();
    v.push(MenuEntry {
        key: Some('n'),
        label: "New tab".into(),
        action: MenuAction::NewTab,
    });
    v.push(MenuEntry {
        key: Some('q'),
        label: "Exit".into(),
        action: MenuAction::Exit,
    });
    v
}

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    if unsafe { libc::isatty(0) } != 1 {
        eprintln!("tview: stdin is not a tty (run this at a console)");
        return 1;
    }
    let mut raw = match term::RawGuard::enter(0) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("tview: cannot enter raw mode: {e}");
            return 1;
        }
    };
    set_nonblock(0);
    install_signal_handlers();
    term::out(term::ENTER_ALT);

    let (rows, cols) = win_rows_cols();
    let on_console = console::is_console(0);
    dlog(format_args!("startup on_console={on_console} rows={rows} cols={cols}"));
    let mut app = App::new(rows, cols, on_console);

    if !app.on_console {
        eprintln!("\x1b[2Ktview: not on a kernel VT - Ctrl+Shift shortcuts disabled");
    }

    app.new_session();
    if app.sessions.is_empty() {
        term::out(term::LEAVE_ALT);
        raw.restore(0);
        eprintln!("tview: failed to spawn a session");
        return 1;
    }

    let code = app.loop_run();
    app.cleanup(&mut raw);
    code
}

impl App {
    fn new(rows: usize, cols: usize, on_console: bool) -> App {
        let view_rows = rows.saturating_sub(1).max(1);
        App {
            rows,
            cols,
            view_rows,
            view_cols: cols,
            canvas: screen::Canvas::new(rows, cols),
            sessions: Vec::new(),
            active: 0,
            inptr: input::Parser::new(),
            sel: selection::Sel::default(),
            keysel: None,
            clip: clipboard::Clipboard::default(),
            batt: battery::Battery::new(),
            on_console,
            quit: false,
            layout: taskbar::Layout::default(),
            toast: None,
            menu: None,
            menu_cache: Vec::new(),
            prefix: false,
            quit_confirm: false,
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), std::time::Instant::now()));
    }

    /// Ask before tearing down every session. Confirmed in on_event by y/Y.
    fn request_quit(&mut self) {
        self.menu = None;
        self.prefix = false;
        self.quit_confirm = true;
        let n = self.sessions.len();
        self.set_toast(format!("quit tasqview? {n} session(s) will close  [y/n]"));
    }

    fn shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }

    fn new_session(&mut self) {
        let id = self.sessions.len() + 1;
        if let Some(s) = session::Session::spawn(
            id,
            self.view_rows as u16,
            self.view_cols as u16,
            &Self::shell(),
        ) {
            self.sessions.push(s);
            self.active = self.sessions.len() - 1;
            self.sel.clear();
        }
    }

    /// Open a new tab running `cmd` under the shell (`sh -c cmd`). The tab
    /// closes when the command exits.
    fn new_session_cmd(&mut self, cmd: &str) {
        let id = self.sessions.len() + 1;
        if let Some(s) = session::Session::spawn_cmd(
            id,
            self.view_rows as u16,
            self.view_cols as u16,
            &Self::shell(),
            cmd,
        ) {
            self.sessions.push(s);
            self.active = self.sessions.len() - 1;
            self.sel.clear();
        } else {
            self.set_toast("failed to launch");
        }
    }

    fn loop_run(&mut self) -> i32 {
        // Paint once up front so the taskbar layout (hit-test rectangles) is
        // populated before the first input event is handled.
        self.render();
        while !self.quit {
            if WINCH.swap(false, Ordering::Relaxed) {
                self.resize_all();
            }

            // Build poll set: stdin + every session pty master.
            let mut pfds = vec![PollFd::new(
                unsafe { BorrowedFd::borrow_raw(0) },
                PollFlags::POLLIN,
            )];
            for s in &self.sessions {
                pfds.push(PollFd::new(
                    unsafe { BorrowedFd::borrow_raw(s.master) },
                    PollFlags::POLLIN,
                ));
            }
            let timeout: nix::poll::PollTimeout = if self.inptr.lone_esc {
                nix::poll::PollTimeout::from(25u16)
            } else {
                nix::poll::PollTimeout::from(100u16)
            };

            let mut stdin_ready = false;
            let mut stdin_closed = false;
            match poll(&mut pfds, timeout) {
                Ok(n) if n > 0 => {
                    stdin_ready = pfds[0]
                        .revents()
                        .unwrap_or(PollFlags::empty())
                        .intersects(PollFlags::POLLIN)
                }
                Ok(_) => {}
                Err(_) => continue,
            }

            // Resolve a lone ESC that got no follow-up bytes.
            if self.inptr.lone_esc && !stdin_ready {
                self.inptr.lone_esc = false;
                self.inptr.pending.clear();
                self.on_event(input::Event::Esc);
            }

            if stdin_ready {
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut _, buf.len()) };
                    if n > 0 {
                        self.inptr.push(&buf[..n as usize]);
                    } else if n == 0 {
                        stdin_closed = true;
                        break;
                    } else {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::EAGAIN)
                            || e.raw_os_error() == Some(libc::EINTR)
                        {
                            break;
                        }
                        stdin_closed = true;
                        break;
                    }
                    if (buf.len() as isize) < 4096 {
                        break;
                    }
                }
            }

            // Process buffered input before honoring an stdin EOF, so the last
            // keystrokes before a close are not dropped.
            let events = self.inptr.drain();
            for ev in &events {
                if self.quit {
                    break;
                }
                self.on_event(ev.clone());
            }

            if stdin_closed {
                self.quit = true;
            }

            // Pump pty output into each session's emulator.
            for (i, s) in self.sessions.iter_mut().enumerate() {
                let rev = pfds
                    .get(i + 1)
                    .and_then(|p| p.revents())
                    .unwrap_or(PollFlags::empty());
                let _ = &s.master;
                if rev.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR) {
                    let mut buf = [0u8; 8192];
                    let n = unsafe { libc::read(s.master, buf.as_mut_ptr() as *mut _, buf.len()) };
                    if n > 0 {
                        s.parser.process(&buf[..n as usize]);
                    } else if n == 0 {
                        s.alive = false;
                    } else {
                        let e = io::Error::last_os_error();
                        if !matches!(
                            e.raw_os_error(),
                            Some(libc::EAGAIN) | Some(libc::EINTR)
                        ) {
                            s.alive = false;
                        }
                    }
                }
            }

            // Refresh taskbar labels from each pty's foreground process
            // (self-throttled inside refresh_title).
            for s in self.sessions.iter_mut() {
                s.refresh_title();
            }

            // Keep scrolled views anchored as scrollback grows.
            for s in self.sessions.iter_mut() {
                if s.scroll > 0 {
                    let cur = s.total_scroll();
                    if cur > s.last_scroll_total {
                        let d = cur - s.last_scroll_total;
                        s.set_scroll(s.scroll + d);
                    }
                    s.last_scroll_total = cur;
                }
            }

            // Drop exited sessions, keeping self.active valid.
            let mut i = 0usize;
            while i < self.sessions.len() {
                if !self.sessions[i].alive {
                    self.sessions[i].reap();
                    self.sessions.remove(i);
                    continue;
                }
                i += 1;
            }
            if self.sessions.is_empty() {
                self.quit = true;
            } else if self.active >= self.sessions.len() {
                self.active = self.sessions.len() - 1;
            }

            self.render();
        }
        0
    }

    fn on_event(&mut self, ev: input::Event) {
        if self.quit_confirm {
            self.quit_confirm = false;
            match ev {
                input::Event::Byte(b'y') | input::Event::Byte(b'Y') => self.quit = true,
                _ => self.set_toast("quit cancelled"),
            }
            return;
        }

        if self.keysel.is_some() {
            dlog(format_args!("keysel ev={ev:?}"));
            match ev {
                input::Event::Up => self.keysel_move(-1, 0),
                input::Event::Down => self.keysel_move(1, 0),
                input::Event::Left => self.keysel_move(0, -1),
                input::Event::Right => self.keysel_move(0, 1),
                input::Event::Home => self.keysel_move(0, -(self.view_cols as i32)),
                input::Event::End => self.keysel_move(0, self.view_cols as i32),
                input::Event::PageUp => self.keysel_move(-(self.view_rows as i32), 0),
                input::Event::PageDown => self.keysel_move(self.view_rows as i32, 0),
                input::Event::Byte(b' ') => self.keysel_reanchor(),
                input::Event::Byte(0x0d)
                | input::Event::Byte(0x0a)
                | input::Event::Byte(b'y')
                | input::Event::Byte(0x03) => self.keysel_copy(),
                input::Event::Esc | input::Event::Byte(0x1b) => self.keysel_cancel(),
                _ => {}
            }
            return;
        }

        // Ctrl+B prefix: the reliable console path for shortcuts. After the
        // prefix, a single command key runs the action; anything else cancels.
        if self.prefix {
            self.prefix = false;
            dlog(format_args!("prefix ev={ev:?}"));
            match ev {
                input::Event::Byte(b) => match b {
                    b'c' | 0x03 => self.copy_selection(),
                    b'v' | 0x16 => self.paste(),
                    b's' | 0x13 => self.toggle_select_mode(),
                    b't' | 0x14 => self.new_session(),
                    b'w' | 0x17 => self.close_active(),
                    b'x' | 0x18 => self.request_quit(),
                    b'n' => {
                        let n = self.sessions.len();
                        if n > 1 {
                            self.active = (self.active + 1) % n;
                        }
                    }
                    0x02 => {
                        // Ctrl+B Ctrl+B: send a literal Ctrl+B to the session.
                        if !self.sessions.is_empty() {
                            self.sessions[self.active].write_input(&[0x02]);
                        }
                    }
                    _ => self.set_toast("prefix cancelled"),
                },
                _ => self.set_toast("prefix cancelled"),
            }
            return;
        }

        if self.menu.is_some() {
            match ev {
                input::Event::Up => self.menu_move(-1),
                input::Event::Down => self.menu_move(1),
                input::Event::Byte(0x0d) => self.menu_activate(),
                input::Event::Esc => self.menu = None,
                input::Event::Byte(b) if (0x20..0x7f).contains(&b) => {
                    let ch = b as char;
                    if let Some(i) =
                        self.menu_cache.iter().position(|e| e.key == Some(ch))
                    {
                        self.menu = Some(i);
                        self.menu_activate();
                    }
                }
                input::Event::MousePress { col, row } => self.menu_click(col, row),
                _ => {}
            }
            return;
        }

        // Bytes destined for the focused session.
        let mut wbuf: Vec<u8> = Vec::new();

        match ev {
            input::Event::Byte(b) => {
                if b == 0x02 {
                    self.prefix = true;
                    self.set_toast("prefix: c/v s(elect) t/w x n");
                    return;
                }
                if let Some(ac) = self.ctrl_action(b) {
                    match ac {
                        CtrlAction::Copy => self.copy_selection(),
                        CtrlAction::Paste => self.paste(),
                        CtrlAction::NewSession => self.new_session(),
                        CtrlAction::CloseSession => self.close_active(),
                        CtrlAction::Quit => self.request_quit(),
                        CtrlAction::ToggleSelect => self.toggle_select_mode(),
                    }
                    return;
                }
                wbuf.push(b);
            }
            input::Event::AltByte(a) => {
                if let Some(ac) = self.alt_action(a) {
                    match ac {
                        CtrlAction::Copy => self.copy_selection(),
                        CtrlAction::Paste => self.paste(),
                        CtrlAction::NewSession => self.new_session(),
                        CtrlAction::CloseSession => self.close_active(),
                        CtrlAction::Quit => self.request_quit(),
                        CtrlAction::ToggleSelect => self.toggle_select_mode(),
                    }
                    return;
                }
                wbuf.push(0x1b);
                wbuf.push(a);
            }
            input::Event::Esc => {
                if self.active_session_scrolled() {
                    let s = &mut self.sessions[self.active];
                    s.reset_scroll();
                } else {
                    wbuf.extend_from_slice(b"\x1b");
                }
            }
            input::Event::AltTab => {
                let n = self.sessions.len();
                if n > 1 {
                    self.active = (self.active + 1) % n;
                }
            }
            input::Event::PageUp => {
                if !self.sessions.is_empty() {
                    let s = &mut self.sessions[self.active];
                    s.set_scroll(s.scroll + self.view_rows);
                    s.last_scroll_total = s.total_scroll();
                }
            }
            input::Event::PageDown => {
                if !self.sessions.is_empty() {
                    let s = &mut self.sessions[self.active];
                    if s.scroll > 0 {
                        let t = s.scroll.saturating_sub(self.view_rows);
                        s.set_scroll(t);
                        s.last_scroll_total = s.total_scroll();
                    }
                }
            }
            input::Event::MousePress { col, row } => self.mouse_press(col, row),
            input::Event::MouseDrag { col, row } => self.mouse_drag(col, row),
            input::Event::MouseRelease { col, row } => self.mouse_release(col, row),
            input::Event::WheelUp { col: _, row } => {
                if (row as usize) < self.view_rows && !self.sessions.is_empty() {
                    let s = &mut self.sessions[self.active];
                    s.set_scroll(s.scroll + 3);
                    s.last_scroll_total = s.total_scroll();
                }
            }
            input::Event::WheelDown { col: _, row } => {
                if (row as usize) < self.view_rows && !self.sessions.is_empty() {
                    let s = &mut self.sessions[self.active];
                    if s.scroll > 0 {
                        let t = s.scroll.saturating_sub(3);
                        s.set_scroll(t);
                        s.last_scroll_total = s.total_scroll();
                    }
                }
            }
            _ => {
                if !self.sessions.is_empty() {
                    let kb = key_bytes(self.sessions[self.active].parser.screen(), &ev);
                    wbuf.extend_from_slice(&kb);
                }
            }
        }

        if !wbuf.is_empty() && !self.sessions.is_empty() {
            // Any keystroke to the child consumes a pending (mouse) selection,
            // so a stale highlight can't turn the next Ctrl+C into a copy.
            if self.sel.have && !self.sel.active {
                self.sel.clear();
            }
            self.sessions[self.active].write_input(&wbuf);
        }
    }

    /// Decide whether a control byte is one of our shortcuts and whether it
    /// applies on this platform. On a real VT the Ctrl+Shift+<key> chord
    /// arrives as a bare control byte, so we lean on the kernel shift-state
    /// ioctl to disambiguate it - which is inherently racy (the key may be
    /// released by the time we sample). The Alt+Shift chords in `alt_action`
    /// are the reliable path; this stays as a best-effort fallback.
    fn ctrl_action(&mut self, b: u8) -> Option<CtrlAction> {
        let shift = self.on_console && console::shift_held(0);
        dlog(format_args!(
            "ctrl b={b:#04x} on_console={} shift={shift}",
            self.on_console
        ));
        match b {
            // Ctrl+C and Ctrl+V are deliberately NOT intercepted: on a tty
            // Ctrl+C must always reach the child as SIGINT, and the old
            // "copy when a selection is pending" behaviour made Ctrl+C
            // unpredictable. Copy/paste live on the Ctrl+B prefix
            // (Ctrl+B c / Ctrl+B v / Ctrl+B s) and on Alt+C / Alt+V / Alt+S.
            0x14 => (self.on_console && shift).then_some(CtrlAction::NewSession),
            0x17 => (self.on_console && shift).then_some(CtrlAction::CloseSession),
            0x18 => (self.on_console && shift).then_some(CtrlAction::Quit),
            0x13 => (self.on_console && shift).then_some(CtrlAction::ToggleSelect),
            _ => None,
        }
    }

    /// Alt+Shift chords: multiplexer shortcuts decoded straight from the byte
    /// stream (ESC-prefixed), with no dependence on the console shift-state
    /// ioctl. Works on a VT and over a pty alike. `a` is the byte after ESC.
    /// Both cases are accepted so a keymap that drops the Shift still works;
    /// the cost is that child programs lose Alt+c/v/t/w.
    fn alt_action(&mut self, a: u8) -> Option<CtrlAction> {
        let act = match a {
            b'C' | b'c' => CtrlAction::Copy,
            b'V' | b'v' => CtrlAction::Paste,
            b'T' | b't' => CtrlAction::NewSession,
            b'W' | b'w' => CtrlAction::CloseSession,
            b'S' | b's' => CtrlAction::ToggleSelect,
            b'X' | b'x' => CtrlAction::Quit,
            _ => return None,
        };
        dlog(format_args!("alt b={a:#04x} -> shortcut"));
        Some(act)
    }

    fn menu_toggle(&mut self) {
        if self.menu.is_some() {
            self.menu = None;
        } else {
            self.menu_cache = load_menu_entries();
            self.menu = Some(0);
        }
        dlog(format_args!("menu_toggle -> {:?}", self.menu));
    }

    fn menu_len(&self) -> usize {
        self.menu_cache.len().max(1)
    }

    /// Pixel width of the menu popup: longest label + padding, min floor.
    fn menu_width(&self) -> usize {
        self.menu_cache
            .iter()
            .map(|e| e.label.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(2)
            .max(MENU_MIN_WIDTH)
            .min(self.view_cols.max(1))
    }

    fn menu_move(&mut self, d: i32) {
        let n = self.menu_len() as i32;
        if let Some(sel) = &mut self.menu {
            *sel = (((*sel as i32 + d) % n + n) % n) as usize;
        }
    }

    fn menu_activate(&mut self) {
        let Some(sel) = self.menu.take() else { return };
        let Some(entry) = self.menu_cache.get(sel).cloned() else {
            return;
        };
        match entry.action {
            MenuAction::NewTab => self.new_session(),
            MenuAction::Exit => self.request_quit(),
            MenuAction::Run(cmd) => self.new_session_cmd(&cmd),
        }
    }

    fn menu_click(&mut self, col: u16, row: u16) {
        let (col, row) = (col as usize, row as usize);
        let n = self.menu_len();
        let w = self.menu_width();
        let r0 = self.view_rows.saturating_sub(n);
        if row >= r0 && row < self.view_rows && col < w {
            self.menu = Some(row - r0);
            self.menu_activate();
        } else {
            self.menu = None;
        }
    }

    fn active_session_scrolled(&self) -> bool {
        self.sessions.get(self.active).map(|s| s.scroll > 0).unwrap_or(false)
    }

    fn mouse_press(&mut self, col: u16, row: u16) {
        let (row, col) = (row as usize, col as usize);
        dlog(format_args!(
            "mouse_press col={col} row={row} view_rows={} start=[{},{}]",
            self.view_rows, self.layout.start_c0, self.layout.start_c1
        ));
        if row == self.view_rows {
            if col >= self.layout.start_c0 && col <= self.layout.start_c1 {
                self.menu_toggle();
                return;
            }
            // Taskbar hit test. `layout.buttons` is rebuilt at render time;
            // if a session closed since the last frame an idx can point at a
            // removed session - skip it rather than index out of bounds.
            for b in &self.layout.buttons {
                if b.idx >= self.sessions.len() {
                    continue;
                }
                if b.close == Some(col) {
                    self.active = b.idx;
                    self.close_active();
                    return;
                }
                if col >= b.c0 && col <= b.c1 {
                    self.active = b.idx;
                    return;
                }
            }
        } else if row < self.view_rows && col < self.view_cols {
            self.sel.press(row as u16, col as u16);
        }
    }

    fn mouse_drag(&mut self, col: u16, row: u16) {
        if row < self.view_rows as u16 && col < self.view_cols as u16 {
            self.sel.drag(row, col);
        }
    }

    fn mouse_release(&mut self, col: u16, row: u16) {
        // The Linux console (via gpm/consolation) never sends motion reports -
        // only press and, in ?1000 mode, a release carrying the final cursor
        // position. Treat that release position as the end of the drag so a
        // click-move-release still produces a selection even with zero drag
        // events in between.
        if row < self.view_rows as u16 && col < self.view_cols as u16 {
            self.sel.drag(row, col);
        }
        self.sel.release();
    }

    fn copy_selection(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if !self.sel.have {
            self.set_toast("nothing selected");
            return;
        }
        let s = &self.sessions[self.active];
        let text = self.sel.extract(s.parser.screen(), self.view_rows as u16, self.view_cols as u16);
        if text.is_empty() {
            self.set_toast("nothing selected");
            return;
        }
        let n = text.chars().count();
        self.clip.set(text);
        // Consume the selection: with it cleared, a bare Ctrl+C goes back to
        // being SIGINT for the child instead of re-copying.
        self.sel.clear();
        self.set_toast(format!("copied {n} chars"));
    }

    fn toggle_select_mode(&mut self) {
        if self.keysel.is_some() {
            self.keysel = None;
            self.sel.clear();
            self.set_toast("select cancelled");
        } else {
            self.enter_select_mode();
        }
        dlog(format_args!("toggle_select_mode -> keysel={:?}", self.keysel.is_some()));
    }

    fn enter_select_mode(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let (r, c) = self.sessions[self.active].parser.screen().cursor_position();
        let r = (r as usize).min(self.view_rows.saturating_sub(1)) as u16;
        let c = (c as usize).min(self.view_cols.saturating_sub(1)) as u16;
        self.sel.clear();
        self.sel.press(r, c);
        self.keysel = Some(KeySel { r, c, anchored: true });
        self.set_toast("SELECT: arrows extend · Space re-anchors · Enter/y copy · Esc cancel");
    }

    fn keysel_move(&mut self, dr: i32, dc: i32) {
        let Some(ks) = &mut self.keysel else { return };
        let nr = (ks.r as i32 + dr).clamp(0, self.view_rows as i32 - 1) as u16;
        let nc = (ks.c as i32 + dc).clamp(0, self.view_cols as i32 - 1) as u16;
        ks.r = nr;
        ks.c = nc;
        if ks.anchored {
            self.sel.drag(nr, nc);
        }
    }

    /// Space in select mode: drop a fresh anchor at the caret, so the user can
    /// start the selection somewhere other than where the cursor began.
    fn keysel_reanchor(&mut self) {
        let Some(ks) = &self.keysel else { return };
        let (r, c) = (ks.r, ks.c);
        self.sel.press(r, c);
        self.set_toast("anchor set · arrows extend");
    }

    fn keysel_copy(&mut self) {
        if self.keysel.is_none() {
            return;
        }
        self.sel.release();
        self.copy_selection();
        self.keysel = None;
    }

    fn keysel_cancel(&mut self) {
        self.keysel = None;
        self.sel.clear();
    }

    fn paste(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.clip.is_empty() {
            self.set_toast("clipboard empty");
            return;
        }
        let text = self.clip.text.clone();
        let bracketed = self.sessions[self.active].parser.screen().bracketed_paste();
        let mut buf = Vec::with_capacity(text.len() + 16);
        if bracketed {
            buf.extend_from_slice(b"\x1b[200~");
        }
        buf.extend_from_slice(text.as_bytes());
        if bracketed {
            buf.extend_from_slice(b"\x1b[201~");
        }
        let s = &self.sessions[self.active];
        s.write_input(&buf);
        self.set_toast(format!("pasted {} chars", text.chars().count()));
    }

    fn close_active(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.sessions[self.active].signal(libc::SIGHUP);
        self.sessions.remove(self.active);
        self.sel.clear();
        if self.sessions.is_empty() {
            self.quit = true;
        } else {
            self.active = self.active.min(self.sessions.len() - 1);
        }
    }

    fn resize_all(&mut self) {
        let (rows, cols) = win_rows_cols();
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.view_rows = rows.saturating_sub(1).max(1);
        self.view_cols = cols;
        self.canvas.resize(rows, cols);
        let vr = self.view_rows as u16;
        let vc = self.view_cols as u16;
        for s in self.sessions.iter_mut() {
            s.resize(vr, vc);
        }
    }

    fn cleanup(&mut self, raw: &mut term::RawGuard) {
        for s in &self.sessions {
            s.signal(libc::SIGHUP);
            s.reap();
        }
        term::out(term::LEAVE_ALT);
        raw.restore(0);
    }

    fn render(&mut self) {
        self.canvas.clear();

        if !self.sessions.is_empty() {
            let active = self.active;
            let (vr, vc) = (self.view_rows, self.view_cols);
            {
                let scr = self.sessions[active].parser.screen();
                for r in 0..vr {
                    for c in 0..vc {
                        let cell = scr.cell(r as u16, c as u16);
                        let (ch, fg, bg, bdd, und, inv, ita) = blit_cell(&self.sel, r, c, cell);
                        self.canvas.set_cell(r, c, &ch, fg, bg, bdd, und, inv, ita);
                    }
                }
            }
        }

        let labels: Vec<(usize, String)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.title()))
            .collect();
        let clock = clock_str();
        let batt = self.batt.as_ref().and_then(|b| b.read());
        self.layout = taskbar::draw(&mut self.canvas, &labels, self.active, &clock, batt);

        // Transient toast: bottom row, right-aligned, black-on-red.
        let toast_dead = self
            .toast
            .as_ref()
            .map(|(_, t)| t.elapsed() >= std::time::Duration::from_millis(1600))
            .unwrap_or(false);
        if toast_dead {
            self.toast = None;
        }
        if let Some((msg, _)) = &self.toast {
            let msg = format!(" {msg} ");
            let w = msg.chars().count();
            let start = self.view_cols.saturating_sub(w + 1);
            let row = self.view_rows;
            for (j, ch) in msg.chars().enumerate() {
                if start + j < self.view_cols {
                    self.canvas.set_cell(
                        row, start + j, &ch.to_string(),
                        taskbar::WHITE, taskbar::RED, true, false, false, false,
                    );
                }
            }
        }

        // Start menu: pops up bottom-left, above the taskbar row.
        if let Some(sel) = self.menu {
            let w = self.menu_width();
            let labels: Vec<String> =
                self.menu_cache.iter().map(|e| e.label.clone()).collect();
            let r0 = self.view_rows.saturating_sub(labels.len());
            for (i, item) in labels.iter().enumerate() {
                let r = r0 + i;
                if r >= self.view_rows {
                    continue;
                }
                let selected = i == sel;
                let (fg, bg) = if selected {
                    (taskbar::WHITE, taskbar::GRAY_DARK)
                } else {
                    (taskbar::BLACK, taskbar::GRAY)
                };
                let mut label = format!(" {item}");
                while label.chars().count() < w {
                    label.push(' ');
                }
                for (j, ch) in label.chars().enumerate() {
                    if j < w {
                        self.canvas.set_cell(
                            r, j, &ch.to_string(), fg, bg, selected, false, false, false,
                        );
                    }
                }
            }
        }

        self.canvas.flush();

        if let Some(ks) = &self.keysel {
            self.canvas.park_cursor(ks.r as usize, ks.c as usize, true);
        } else if !self.sessions.is_empty() {
            let s = &self.sessions[self.active];
            if s.scroll > 0 {
                self.canvas.park_cursor(0, 0, false);
            } else {
                let (r, c) = s.parser.screen().cursor_position();
                let hide = s.parser.screen().hide_cursor();
                let r = (r as usize).min(self.view_rows - 1);
                let c = (c as usize).min(self.view_cols - 1);
                self.canvas.park_cursor(r, c, !hide);
            }
        } else {
            self.canvas.park_cursor(0, 0, true);
        }
    }
}

enum CtrlAction {
    Copy,
    Paste,
    NewSession,
    CloseSession,
    Quit,
    ToggleSelect,
}

struct KeySel {
    r: u16,
    c: u16,
    anchored: bool,
}

#[allow(clippy::type_complexity)]
fn blit_cell(
    sel: &selection::Sel,
    r: usize,
    c: usize,
    cell: Option<&vt100::Cell>,
) -> (String, u8, u8, bool, bool, bool, bool) {
    let Some(cell) = cell else {
        return (" ".to_string(), screen::DEFAULT, screen::DEFAULT, false, false, false, false);
    };
    let ch = if cell.is_wide_continuation() {
        String::new()
    } else {
        cell.contents().to_string()
    };
    let fg = screen::vt_color(cell.fgcolor());
    let bg = screen::vt_color(cell.bgcolor());
    let bold = cell.bold();
    let underline = cell.underline();
    let italic = cell.italic();
    let mut inverse = cell.inverse();
    if sel.inside(r, c) {
        inverse = !inverse;
    }
    (ch, fg, bg, bold, underline, inverse, italic)
}

/// Translate navigation/function keys into terminal input bytes, honouring
/// the focused session's application cursor/keypad modes.
fn key_bytes(screen: &vt100::Screen, ev: &input::Event) -> Vec<u8> {
    let appcur = screen.application_cursor();
    let appkey = screen.application_keypad();
    let _ = appkey;
    match ev {
        input::Event::Up => {
            if appcur {
                b"\x1bOA".to_vec()
            } else {
                b"\x1b[A".to_vec()
            }
        }
        input::Event::Down => {
            if appcur {
                b"\x1bOB".to_vec()
            } else {
                b"\x1b[B".to_vec()
            }
        }
        input::Event::Right => {
            if appcur {
                b"\x1bOC".to_vec()
            } else {
                b"\x1b[C".to_vec()
            }
        }
        input::Event::Left => {
            if appcur {
                b"\x1bOD".to_vec()
            } else {
                b"\x1b[D".to_vec()
            }
        }
        input::Event::Home => {
            if appcur {
                b"\x1bOH".to_vec()
            } else {
                b"\x1b[H".to_vec()
            }
        }
        input::Event::End => {
            if appcur {
                b"\x1bOF".to_vec()
            } else {
                b"\x1b[F".to_vec()
            }
        }
        input::Event::Insert => b"\x1b[2~".to_vec(),
        input::Event::Delete => b"\x1b[3~".to_vec(),
        input::Event::BackTab => b"\x1b[Z".to_vec(),
        input::Event::PageUp => b"\x1b[5~".to_vec(),
        input::Event::PageDown => b"\x1b[6~".to_vec(),
        input::Event::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            _ => b"\x1b[24~".to_vec(),
        },
        _ => Vec::new(),
    }
}

fn clock_str() -> String {
    unsafe {
        let mut tv: libc::timeval = std::mem::zeroed();
        libc::gettimeofday(&mut tv, std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&tv.tv_sec, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn win_rows_cols() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(0, libc::TIOCGWINSZ as _, &mut ws) } == 0;
    if ok && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row as usize, ws.ws_col as usize)
    } else {
        (24, 80)
    }
}

fn set_nonblock(fd: libc::c_int) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

/// Append a line to /tmp/tview-debug.log, but only when TVIEW_DEBUG=1.
/// The env var is read once and cached.
fn dlog(args: std::fmt::Arguments) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var("TVIEW_DEBUG").as_deref() == Ok("1"));
    if !on {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/tview-debug.log")
    {
        let _ = writeln!(f, "{args}");
    }
}

fn install_signal_handlers() {
    let sa = SigAction::new(
        SigHandler::Handler(on_winch),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(Signal::SIGWINCH, &sa);
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn cmd_name_is_basename_of_first_word() {
        assert_eq!(menu_cmd_name("htop"), "htop");
        assert_eq!(menu_cmd_name("/usr/bin/htop"), "htop");
        assert_eq!(menu_cmd_name("nvim /etc/fstab"), "nvim");
        assert_eq!(menu_cmd_name("/opt/bin/thing --flag"), "thing");
    }

    #[test]
    fn parse_skips_comments_blanks_and_malformed() {
        let txt = "\n# a comment\nh: htop\n\n  m :  mc \nbroken line\n: bare\n";
        let v = parse_menu_text(txt);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].key, Some('h'));
        assert_eq!(v[0].label, "h  htop");
        assert!(matches!(&v[0].action, MenuAction::Run(c) if c == "htop"));
        assert_eq!(v[1].key, Some('m'));
        assert!(matches!(&v[1].action, MenuAction::Run(c) if c == "mc"));
        // blank key -> no accelerator, label is the command name
        assert_eq!(v[2].key, None);
        assert_eq!(v[2].label, "bare");
    }

    #[test]
    fn multichar_key_field_has_no_accelerator() {
        let v = parse_menu_text("12: something\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].key, None);
        assert_eq!(v[0].label, "12  something");
    }

    #[test]
    fn command_may_contain_colons() {
        let v = parse_menu_text("j: journalctl -u foo.service\n");
        assert!(matches!(&v[0].action,
            MenuAction::Run(c) if c == "journalctl -u foo.service"));
    }
}