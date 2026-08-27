//! Linux console (VT) specific ioctls via TIOCLINUX.
//!
//! Only meaningful when running on a kernel virtual terminal. Over a pty
//! (ssh, script, etc.) these ioctls fail with EINVAL/ENODEV and every call
//! returns None, letting the caller fall back gracefully.

use libc::{c_int, c_ulong};

/// TIOCLINUX ioctl request (0x541C on all mainstream Linux architectures).
const TIOCLINUX: c_ulong = 0x541C;
/// subcode: write kernel shift_state into the byte pointed to by arg.
const TIOCL_GETSHIFTSTATE: u8 = 6;

/// Shift-state bitmask returned by TIOCL_GETSHIFTSTATE.
mod shift {
    pub const SHIFT: u8 = 0x01;
    pub const ALTGR: u8 = 0x02;
    pub const CTRL: u8 = 0x04;
    pub const ALT: u8 = 0x08;
}

/// Query the current keyboard modifier state on the console.
/// Returns None when the fd is not a Linux VT (e.g. a pty).
pub fn shift_state(fd: c_int) -> Option<u8> {
    let mut state: u8 = TIOCL_GETSHIFTSTATE;
    let r = unsafe { libc::ioctl(fd, TIOCLINUX, &mut state as *mut u8) };
    if r < 0 {
        None
    } else {
        Some(state)
    }
}

/// True when we are running on a real Linux console and can disambiguate
/// Ctrl+Shift combinations from plain Ctrl combinations.
pub fn is_console(fd: c_int) -> bool {
    shift_state(fd).is_some()
}

/// Whether Shift is currently held.
pub fn shift_held(fd: c_int) -> bool {
    shift_state(fd).map(|s| s & shift::SHIFT != 0).unwrap_or(false)
}