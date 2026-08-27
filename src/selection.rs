//! Mouse drag selection overlay over the focused session view.

#[derive(Clone, Copy, Default)]
pub struct Sel {
    /// A completed selection exists (can be copied).
    pub have: bool,
    /// A drag is currently in progress.
    pub active: bool,
    start: (u16, u16),
    end: (u16, u16),
}

impl Sel {
    pub fn press(&mut self, r: u16, c: u16) {
        self.have = true;
        self.active = true;
        self.start = (r, c);
        self.end = (r, c);
    }

    pub fn drag(&mut self, r: u16, c: u16) {
        if self.active {
            self.end = (r, c);
        }
    }

    pub fn release(&mut self) {
        self.active = false;
    }

    pub fn clear(&mut self) {
        self.have = false;
        self.active = false;
    }

    fn stream_pos(&self) -> (usize, usize) {
        let a = self.start.0 as usize * 65536 + self.start.1 as usize;
        let b = self.end.0 as usize * 65536 + self.end.1 as usize;
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Whether the (row, col) cell inside the selection, in stream order.
    pub fn inside(&self, r: usize, c: usize) -> bool {
        if !self.have {
            return false;
        }
        let (a, b) = self.stream_pos();
        let p = r * 65536 + c;
        a <= p && p <= b
    }

    /// Extract plain text of the current selection from a vt100 screen
    /// (respects the current scrollback view offset applied to the screen).
    pub fn extract(&self, screen: &vt100::Screen, rows: u16, cols: u16) -> String {
        if !self.have {
            return String::new();
        }
        let (a, b) = self.stream_pos();
        let mut text = String::new();
        for r in 0..rows {
            let start_row = r as usize * 65536;
            if start_row > b {
                break;
            }
            let mut line = String::new();
            for c in 0..cols {
                let p = start_row + c as usize;
                if p < a || p > b {
                    continue;
                }
                if let Some(cell) = screen.cell(r, c) {
                    if !cell.is_wide_continuation() {
                        line.push_str(cell.contents());
                    }
                }
            }
            if !line.is_empty() {
                text.push_str(&line);
                if r + 1 < rows && !screen.row_wrapped(r) {
                    text.push('\n');
                }
            }
        }
        text
    }
}