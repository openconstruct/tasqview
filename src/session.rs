//! One session = a child process (usually a shell) running in its own pty,
//! whose output is parsed into a `vt100::Screen`. Sessions keep running and
//! parsing even when unfocused; scrollback is provided natively by vt100.

use std::ffi::CString;
use std::os::fd::AsRawFd;

use nix::unistd::ForkResult;

pub const SCROLLBACK_LEN: usize = 2000;

pub struct Session {
    pub id: usize,
    pub name: String,
    pub pid: nix::unistd::Pid,
    pub master: i32,
    pub parser: vt100::Parser,
    /// Current view offset into the scrollback (0 = live screen).
    pub scroll: usize,
    /// Last recorded total scrollback size, used to keep the view anchored
    /// when new output arrives while we are scrolled back.
    pub last_scroll_total: usize,
    pub alive: bool,
}

impl Session {
    /// Spawn `shell` inside a fresh pty of `rows` x `cols`.
    pub fn spawn(id: usize, rows: u16, cols: u16, shell: &str) -> Option<Session> {
        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let openpty = nix::pty::openpty(Some(&ws), None).ok()?;
        let master = openpty.master;
        let slave = openpty.slave;
        let master_fd = master.as_raw_fd();
        let slave_fd = slave.as_raw_fd();
        let shell_c = CString::new(shell).ok()?;

        match unsafe { nix::unistd::fork() } {
            Ok(ForkResult::Child) => {
                // Leave the slave's termios at its default (cooked, echo on).
                // The shell/readline will switch modes itself as needed, same
                // as a real terminal emulator.
                let _ = nix::unistd::setsid();
                unsafe {
                    libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }
                    libc::close(master_fd);
                    libc::setenv(b"TERM\0".as_ptr().cast(), b"xterm-256color\0".as_ptr().cast(), 1);
                    libc::execl(
                        shell_c.as_ptr(),
                        shell_c.as_ptr(),
                        std::ptr::null::<libc::c_char>(),
                    );
                    libc::_exit(127);
                }
            }
            Ok(ForkResult::Parent { child }) => {
                // Take raw fd ownership; prevent the OwnedFd wrappers from
                // closing the descriptors out from under us.
                std::mem::forget(master);
                std::mem::forget(slave);
                let _ = unsafe { libc::close(slave_fd) };
                set_nonblock(master_fd);
                let parser = vt100::Parser::new(rows, cols, SCROLLBACK_LEN);
                Some(Session {
                    id,
                    name: format!("{id}"),
                    pid: child,
                    master: master_fd,
                    parser,
                    scroll: 0,
                    last_scroll_total: 0,
                    alive: true,
                })
            }
            Err(_) => None,
        }
    }

    /// Write input bytes to the session's stdin. If the pty's output buffer
    /// is full (EAGAIN) the bytes are dropped rather than blocking.
    pub fn write_input(&self, buf: &[u8]) {
        let mut off = 0usize;
        while off < buf.len() {
            let n = unsafe {
                libc::write(
                    self.master,
                    buf[off..].as_ptr() as *const libc::c_void,
                    buf.len() - off,
                )
            };
            if n <= 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EINTR) {
                    return;
                }
                return;
            }
            off += n as usize;
        }
    }

    /// Resize the emulator and the pty.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master, libc::TIOCSWINSZ as _, &ws);
        }
    }

    /// Total lines currently held in the scrollback.
    pub fn total_scroll(&mut self) -> usize {
        let s = self.parser.screen_mut();
        s.set_scrollback(usize::MAX);
        let t = s.scrollback();
        s.set_scrollback(self.scroll.min(t));
        t
    }

    /// Move the view into the scrollback (clamped to what is available).
    pub fn set_scroll(&mut self, target: usize) {
        let s = self.parser.screen_mut();
        s.set_scrollback(target);
        self.scroll = s.scrollback();
    }

    pub fn reset_scroll(&mut self) {
        self.set_scroll(0);
        self.last_scroll_total = 0;
    }

    pub fn signal(&self, sig: libc::c_int) {
        unsafe {
            libc::kill(self.pid.as_raw(), sig);
        }
    }

    /// Reap the child if it has exited.
    pub fn reap(&self) {
        let mut status: libc::c_int = 0;
        unsafe {
            libc::waitpid(self.pid.as_raw(), &mut status, libc::WNOHANG);
        }
    }
}

fn set_nonblock(fd: i32) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}