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
    clip: clipboard::Clipboard,
    batt: Option<battery::Battery>,
    on_console: bool,
    quit: bool,
    layout: taskbar::Layout,
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
            clip: clipboard::Clipboard::default(),
            batt: battery::Battery::new(),
            on_console,
            quit: false,
            layout: taskbar::Layout::default(),
        }
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

    fn loop_run(&mut self) -> i32 {
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

            if stdin_closed {
                self.quit = true;
            }

            let events = self.inptr.drain();
            for ev in &events {
                if self.quit {
                    break;
                }
                self.on_event(ev.clone());
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
        // Bytes destined for the focused session.
        let mut wbuf: Vec<u8> = Vec::new();

        match ev {
            input::Event::Byte(b) => {
                if let Some(ac) = self.ctrl_action(b) {
                    match ac {
                        CtrlAction::Copy => self.copy_selection(),
                        CtrlAction::Paste => self.paste(),
                        CtrlAction::NewSession => self.new_session(),
                        CtrlAction::CloseSession => self.close_active(),
                        CtrlAction::Quit => self.quit = true,
                    }
                    return;
                }
                wbuf.push(b);
            }
            input::Event::AltByte(a) => {
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
                    let scr = self.sessions[self.active].parser.screen().clone();
                    let kb = key_bytes(&scr, &ev);
                    wbuf.extend_from_slice(&kb);
                }
            }
        }

        if !wbuf.is_empty() && !self.sessions.is_empty() {
            self.sessions[self.active].write_input(&wbuf);
        }
    }

    /// Decide whether a control byte is one of our shortcuts and whether it
    /// applies on this platform.
    fn ctrl_action(&mut self, b: u8) -> Option<CtrlAction> {
        match b {
            0x03 => {
                if self.on_console {
                    if console::shift_held(0) {
                        Some(CtrlAction::Copy)
                    } else {
                        None
                    }
                } else if self.sel.have {
                    Some(CtrlAction::Copy)
                } else {
                    None
                }
            }
            0x16 => {
                let want = if self.on_console {
                    console::shift_held(0)
                } else {
                    !self.clip.is_empty() && !self.sessions.is_empty()
                };
                if want {
                    Some(CtrlAction::Paste)
                } else {
                    None
                }
            }
            0x14 => {
                if self.on_console && console::shift_held(0) {
                    Some(CtrlAction::NewSession)
                } else {
                    None
                }
            }
            0x17 => {
                if self.on_console && console::shift_held(0) {
                    Some(CtrlAction::CloseSession)
                } else {
                    None
                }
            }
            0x18 => {
                if self.on_console && console::shift_held(0) {
                    Some(CtrlAction::Quit)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn active_session_scrolled(&self) -> bool {
        self.sessions.get(self.active).map(|s| s.scroll > 0).unwrap_or(false)
    }

    fn mouse_press(&mut self, col: u16, row: u16) {
        let (row, col) = (row as usize, col as usize);
        if row == self.view_rows {
            // Taskbar hit test.
            for b in &self.layout.buttons {
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

    fn mouse_release(&mut self, _col: u16, _row: u16) {
        self.sel.release();
    }

    fn copy_selection(&mut self) {
        if !self.sel.have || self.sessions.is_empty() {
            return;
        }
        let s = &self.sessions[self.active];
        let text = self.sel.extract(s.parser.screen(), self.view_rows as u16, self.view_cols as u16);
        if !text.is_empty() {
            self.clip.set(text);
        }
    }

    fn paste(&mut self) {
        if self.sessions.is_empty() || self.clip.is_empty() {
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
            .map(|(i, s)| (i, s.name.clone()))
            .collect();
        let clock = clock_str();
        let batt = self.batt.as_ref().and_then(|b| b.read());
        self.layout = taskbar::draw(&mut self.canvas, &labels, self.active, &clock, batt);
        self.canvas.flush();

        if !self.sessions.is_empty() {
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