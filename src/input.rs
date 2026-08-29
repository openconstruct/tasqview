//! Incremental input parser: decodes key/mouse escape sequences from the raw
//! byte stream of the controlling terminal.

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A raw byte to forward to the focused session (includes control
    /// characters and UTF-8 bytes).
    Byte(u8),
    /// Alt + a printable byte (ESC prefix followed by the byte).
    AltByte(u8),
    /// A lone Escape press.
    Esc,
    /// Alt+Tab: switch sessions.
    AltTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    BackTab,
    F(u8),
    MousePress { col: u16, row: u16 },
    MouseDrag { col: u16, row: u16 },
    MouseRelease { col: u16, row: u16 },
    WheelUp { col: u16, row: u16 },
    WheelDown { col: u16, row: u16 },
    /// A control sequence that is well-formed but which we do not care about;
    /// consume it so the stream does not stall.
    Drop,
}

pub struct Parser {
    pub pending: Vec<u8>,
    /// When we've seen a lone ESC and are waiting to see if more bytes follow
    /// (the user may have typed Escape as a key).
    pub lone_esc: bool,
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            pending: Vec::with_capacity(64),
            lone_esc: false,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.lone_esc {
            self.lone_esc = false;
        }
        self.pending.extend_from_slice(data);
    }

    /// Pull the events that can be decoded from the current buffer.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        loop {
            if self.pending.is_empty() {
                break;
            }
            let b = self.pending[0];
            match b {
                0x1b => {
                    if self.pending.len() == 1 {
                        if self.lone_esc {
                            self.pending.clear();
                            self.lone_esc = false;
                            ev.push(Event::Esc);
                        } else {
                            self.lone_esc = true;
                        }
                        break;
                    }
                    let next = self.pending[1];
                    match next {
                        b'[' => {
                            if self.pending.len() < 2 {
                                break;
                            }
                            if let Some((e, rest)) = self.parse_csi() {
                                ev.push(e);
                                self.pending = rest;
                            } else {
                                break;
                            }
                        }
                        b'O' => {
                            if let Some((e, rest)) = self.parse_ss3() {
                                ev.push(e);
                                self.pending = rest;
                            } else {
                                break;
                            }
                        }
                        0x09 => {
                            self.pending.drain(..2);
                            ev.push(Event::AltTab);
                        }
                        0x0d => {
                            // Alt+Enter.
                            self.pending.drain(..2);
                            ev.push(Event::AltByte(0x0d));
                        }
                        _ => {
                            let nb = next;
                            self.pending.drain(..2);
                            ev.push(Event::AltByte(nb));
                        }
                    }
                }
                0x00..=0x1f => {
                    self.pending.drain(..1);
                    ev.push(Event::Byte(b));
                }
                0x7f => {
                    self.pending.drain(..1);
                    ev.push(Event::Byte(0x7f));
                }
                _ => {
                    let w = utf8_len(b);
                    if self.pending.len() < w {
                        break;
                    }
                    let bytes: Vec<u8> = self.pending.drain(..w).collect();
                    for x in bytes {
                        ev.push(Event::Byte(x));
                    }
                }
            }
        }
        ev
    }

    /// Parse `ESC [ ... final`, returns (event, remaining buffer).
    fn parse_csi(&mut self) -> Option<(Event, Vec<u8>)> {
        let buf = &self.pending;
        // Linux console function keys: F1-F5 arrive as ESC [ [ A .. ESC [ [ E.
        // The generic final-byte scan below treats the second '[' as the final
        // byte and drops the sequence, so pressing F1 on a raw VT just types
        // a bare 'A'. Handle the form explicitly before the scanner sees it.
        if buf.get(2) == Some(&b'[') {
            match buf.get(3) {
                None => return None, // sequence not fully arrived yet
                Some(&c) if (b'A'..=b'E').contains(&c) => {
                    let rest = buf[4..].to_vec();
                    return Some((Event::F(1 + (c - b'A')), rest));
                }
                _ => {}
            }
        }
        // Legacy mouse report: ESC [ M <btn> <col> <row> - three raw bytes,
        // each offset by 32. gpm's xterm emulation and older terminals send
        // this instead of SGR (ESC [ < ... M/m). Without handling it here the
        // ESC[M is treated as a final byte and dropped, and the three data
        // bytes leak to the child as garbage - which also trips the
        // "a keystroke clears the pending selection" guard in on_event, so a
        // mouse drag appears to deselect itself the moment you let go.
        if buf.get(2) == Some(&b'M') {
            if buf.len() < 6 {
                return None; // data bytes not all here yet
            }
            let b = (buf[3] as i64) - 32;
            let col = ((buf[4] as i64) - 33).max(0) as u16;
            let row = ((buf[5] as i64) - 33).max(0) as u16;
            let rest = buf[6..].to_vec();
            return Some((classify_mouse(b, col, row, false), rest));
        }

        // find final byte
        let mut idx = 2;
        while idx < buf.len() && buf[idx] >= 0x20 && buf[idx] < 0x40 {
            idx += 1;
        }
        if idx >= buf.len() || buf[idx] < 0x40 {
            return None; // incomplete
        }
        let params = &buf[2..idx];
        let finalb = buf[idx];
        let rest = buf[idx + 1..].to_vec();

        // SGR mouse: ESC [ < btn ; col ; row M/m
        if params.first() == Some(&b'<') {
            if let Some(e) = parse_mouse(&params[1..], finalb) {
                return Some((e, rest.clone()));
            }
            return Some((Event::Drop, rest));
        }

        let mut pnums: Vec<i64> = params
            .split(|&x| x == b';')
            .map(|s| std::str::from_utf8(s).unwrap_or("").trim().parse().unwrap_or(0))
            .collect();
        if pnums.is_empty() {
            pnums.push(0);
        }

        let ev = match finalb {
            b'A' => Event::Up,
            b'B' => Event::Down,
            b'C' => Event::Right,
            b'D' => Event::Left,
            b'H' => Event::Home,
            b'F' => Event::End,
            b'Z' => Event::BackTab,
            b'~' => match pnums[0] {
                1 | 7 => Event::Home,
                4 | 8 => Event::End,
                5 => Event::PageUp,
                6 => Event::PageDown,
                2 => Event::Insert,
                3 => Event::Delete,
                11 => Event::F(1),
                12 => Event::F(2),
                13 => Event::F(3),
                14 => Event::F(4),
                15 => Event::F(5),
                17 => Event::F(6),
                18 => Event::F(7),
                19 => Event::F(8),
                20 => Event::F(9),
                21 => Event::F(10),
                23 => Event::F(11),
                24 => Event::F(12),
                _ => Event::Drop,
            },
            _ => Event::Drop,
        };
        Some((ev, rest))
    }

    /// Parse `ESC O <final>` (SS3, used by app-cursor mode and F1-F4).
    fn parse_ss3(&mut self) -> Option<(Event, Vec<u8>)> {
        let buf = &self.pending;
        if buf.len() < 3 {
            return None;
        }
        let f = buf[2];
        let rest = buf[3..].to_vec();
        let ev = match f {
            b'A' => Event::Up,
            b'B' => Event::Down,
            b'C' => Event::Right,
            b'D' => Event::Left,
            b'H' => Event::Home,
            b'F' => Event::End,
            b'P' => Event::F(1),
            b'Q' => Event::F(2),
            b'R' => Event::F(3),
            b'S' => Event::F(4),
            _ => return None,
        };
        Some((ev, rest))
    }
}

fn parse_mouse(params: &[u8], finalb: u8) -> Option<Event> {
    let s = std::str::from_utf8(params).ok()?;
    let mut it = s.split(';');
    let btn: i64 = it.next()?.parse().ok()?;
    let col: i64 = it.next()?.parse().ok()?;
    let row: i64 = it.next()?.parse().ok()?;
    let (col, row) = (col.saturating_sub(1).max(0) as u16, row.saturating_sub(1).max(0) as u16);
    Some(classify_mouse(btn, col, row, finalb == b'm'))
}

/// Turn a mouse button code into an event. `btn` is the xterm button field
/// (SGR: the number before the final byte; legacy: the raw byte minus 32).
/// `sgr_release` is set only on the SGR `m` final byte, where the button code
/// no longer carries the "which button went up" bits.
/// Encoding: bits 0-1 button, bit 5 (32) = motion (drag), bit 6 (64) = wheel;
/// a button field with bits 0-1 == 3 also means release.
fn classify_mouse(btn: i64, col: u16, row: u16, sgr_release: bool) -> Event {
    if btn & 64 != 0 {
        return if btn & 1 == 0 {
            Event::WheelUp { col, row }
        } else {
            Event::WheelDown { col, row }
        };
    }
    if sgr_release || btn & 3 == 3 {
        return Event::MouseRelease { col, row };
    }
    if btn & 32 != 0 {
        return Event::MouseDrag { col, row };
    }
    Event::MousePress { col, row }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        p.push(bytes);
        p.drain()
    }

    #[test]
    fn linux_console_function_keys() {
        assert_eq!(drain_all(b"\x1b[[A"), vec![Event::F(1)]);
        assert_eq!(drain_all(b"\x1b[[B"), vec![Event::F(2)]);
        assert_eq!(drain_all(b"\x1b[[C"), vec![Event::F(3)]);
        assert_eq!(drain_all(b"\x1b[[D"), vec![Event::F(4)]);
        assert_eq!(drain_all(b"\x1b[[E"), vec![Event::F(5)]);
    }

    #[test]
    fn ss3_and_csi_function_keys_still_work() {
        assert_eq!(drain_all(b"\x1bOP"), vec![Event::F(1)]);
        assert_eq!(drain_all(b"\x1bOS"), vec![Event::F(4)]);
        assert_eq!(drain_all(b"\x1b[15~"), vec![Event::F(5)]);
        assert_eq!(drain_all(b"\x1b[24~"), vec![Event::F(12)]);
    }

    #[test]
    fn console_fkey_then_trailing_byte() {
        assert_eq!(drain_all(b"\x1b[[Ax"), vec![Event::F(1), Event::Byte(b'x')]);
    }

    #[test]
    fn legacy_mouse_press_drag_release() {
        // ESC [ M  btn=' '(0)+32  col='!'(1)+32  row='!'(1)+32  -> press at 0,0
        assert_eq!(
            drain_all(b"\x1b[M \x21\x21"),
            vec![Event::MousePress { col: 0, row: 0 }]
        );
        // btn = 32 (motion) + 0 -> drag
        assert_eq!(
            drain_all(b"\x1b[M\x40\x30\x25"),
            vec![Event::MouseDrag { col: 15, row: 4 }]
        );
        // btn low bits 3 -> release
        assert_eq!(
            drain_all(b"\x1b[M\x23\x30\x25"),
            vec![Event::MouseRelease { col: 15, row: 4 }]
        );
    }

    #[test]
    fn legacy_mouse_wheel() {
        // btn = 64 + 32 (offset) = 96 = '`'
        assert_eq!(
            drain_all(b"\x1b[M\x60\x21\x21"),
            vec![Event::WheelUp { col: 0, row: 0 }]
        );
        assert_eq!(
            drain_all(b"\x1b[M\x61\x21\x21"),
            vec![Event::WheelDown { col: 0, row: 0 }]
        );
    }

    #[test]
    fn legacy_mouse_no_leak_to_child() {
        // The three data bytes must be consumed, not forwarded as Byte events.
        let evs = drain_all(b"\x1b[M \x21\x21x");
        assert_eq!(
            evs,
            vec![Event::MousePress { col: 0, row: 0 }, Event::Byte(b'x')]
        );
    }

    #[test]
    fn partial_legacy_mouse_waits() {
        let mut p = Parser::new();
        p.push(b"\x1b[M \x21");
        assert_eq!(p.drain(), vec![]);
        p.push(b"\x21");
        assert_eq!(p.drain(), vec![Event::MousePress { col: 0, row: 0 }]);
    }

    #[test]
    fn sgr_mouse_still_works() {
        assert_eq!(
            drain_all(b"\x1b[<0;12;5M"),
            vec![Event::MousePress { col: 11, row: 4 }]
        );
        assert_eq!(
            drain_all(b"\x1b[<0;12;5m"),
            vec![Event::MouseRelease { col: 11, row: 4 }]
        );
        assert_eq!(
            drain_all(b"\x1b[<32;12;5M"),
            vec![Event::MouseDrag { col: 11, row: 4 }]
        );
        assert_eq!(
            drain_all(b"\x1b[<64;1;1M"),
            vec![Event::WheelUp { col: 0, row: 0 }]
        );
    }

    #[test]
    fn partial_console_fkey_waits() {
        let mut p = Parser::new();
        p.push(b"\x1b[[");
        assert_eq!(p.drain(), vec![]);
        p.push(b"C");
        assert_eq!(p.drain(), vec![Event::F(3)]);
    }
}