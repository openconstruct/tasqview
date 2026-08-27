use std::io::{self, Write};
use std::os::fd::BorrowedFd;

use nix::sys::termios::{self, SetArg, Termios};

/// Enter raw mode on fd 0 and restore the original settings on drop.
pub struct RawGuard {
    orig: Termios,
    restored: bool,
}

impl RawGuard {
    pub fn enter(fd: i32) -> io::Result<Self> {
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let orig = termios::tcgetattr(bfd)?;
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        // Keep output post-processing so the terminal interprets our \n and
        // \r\n properly, but disable ONLCR since we always emit \r\n
        // ourselves.
        raw.output_flags |= termios::OutputFlags::OPOST;
        raw.output_flags.remove(termios::OutputFlags::ONLCR);
        termios::tcsetattr(bfd, SetArg::TCSANOW, &raw)?;
        Ok(RawGuard {
            orig,
            restored: false,
        })
    }

    pub fn restore(&mut self, fd: i32) {
        if !self.restored {
            self.restored = true;
            let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
            let _ = termios::tcsetattr(bfd, SetArg::TCSANOW, &self.orig);
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        self.restore(0);
    }
}

/// Terminal sequences used to take over / release the screen.
pub const ENTER_ALT: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub const LEAVE_ALT: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l";

/// Write a batch of bytes to stdout and flush.
pub fn out(bytes: &[u8]) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}