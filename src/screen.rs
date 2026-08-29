//! Off-screen cell canvas for the whole terminal and a minimal diff renderer.

use std::fmt::Write as _;
use std::io::{self, Write};

pub const DEFAULT: u8 = 255; // "use terminal default color"

#[derive(Clone)]
pub struct Cell {
    pub ch: String,
    pub fg: u8,
    pub bg: u8,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
    pub italic: bool,
}

impl Cell {
    fn blank() -> Cell {
        Cell {
            ch: String::new(),
            fg: DEFAULT,
            bg: DEFAULT,
            bold: false,
            underline: false,
            inverse: false,
            italic: false,
        }
    }
    fn key(&self) -> (String, u8, u8, bool, bool, bool, bool) {
        (
            self.ch.clone(),
            self.fg,
            self.bg,
            self.bold,
            self.underline,
            self.inverse,
            self.italic,
        )
    }
}

impl PartialEq for Cell {
    fn eq(&self, o: &Cell) -> bool {
        self.fg == o.fg
            && self.bg == o.bg
            && self.bold == o.bold
            && self.underline == o.underline
            && self.inverse == o.inverse
            && self.italic == o.italic
            && self.ch == o.ch
    }
}

pub struct Canvas {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<Cell>,
    prev: Vec<Cell>,
}

impl Canvas {
    pub fn new(rows: usize, cols: usize) -> Canvas {
        let cells = (0..rows * cols).map(|_| Cell::blank()).collect();
        let prev = (0..rows * cols).map(|_| Cell::blank()).collect();
        Canvas {
            rows,
            cols,
            cells,
            prev,
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        let n = rows * cols;
        self.cells = (0..n).map(|_| Cell::blank()).collect();
        self.prev = (0..n).map(|_| Cell::blank()).collect();
    }

    /// Reset every cell to the default style.
    pub fn clear(&mut self) {
        for c in self.cells.iter_mut() {
            *c = Cell::blank();
        }
    }

    /// Set a cell when its content/style actually differs (avoids churn).
    pub fn set_cell(
        &mut self,
        r: usize,
        c: usize,
        ch: &str,
        fg: u8,
        bg: u8,
        bold: bool,
        underline: bool,
        inverse: bool,
        italic: bool,
    ) {
        let idx = r * self.cols + c;
        let cell = &mut self.cells[idx];
        let ch_owned = if ch.is_empty() { " ".to_string() } else { ch.to_string() };
        if cell.bg != bg
            || cell.fg != fg
            || cell.bold != bold
            || cell.underline != underline
            || cell.inverse != inverse
            || cell.italic != italic
            || cell.ch != ch_owned
        {
            *cell = Cell {
                ch: ch_owned,
                fg,
                bg,
                bold,
                underline,
                inverse,
                italic,
            };
        }
    }

    /// Draw the canvas to stdout, only writing rows that changed.
    pub fn flush(&mut self) {
        let mut outb: Vec<u8> = Vec::with_capacity(self.rows * self.cols * 4);
        let mut cur = (usize::MAX, usize::MAX);
        let mut cur_attrs = Cell::blank();
        let mut opened = false;

        for r in 0..self.rows {
            let base = r * self.cols;
            let changed = self.prev[base..base + self.cols] != self.cells[base..base + self.cols];
            if !changed {
                continue;
            }
            if !opened {
                // The previous flush left the terminal's active SGR as whatever
                // its last drawn cell was, and never reset it. Our per-cell diff
                // below assumes the terminal starts this flush in the default
                // style (cur_attrs == blank), so a run of default-styled cells
                // emits no SGR at all - and would inherit the stale colour,
                // painting "cleared" regions in the last app's background. Force
                // a known-clean state before the first write.
                outb.extend_from_slice(b"\x1b[0m");
                opened = true;
            }
            if cur != (r, 0) {
                let _ = write!(outb, "\x1b[{};1H", r + 1);
                cur = (r, 0);
            }
            for c in 0..self.cols {
                let cell = &self.cells[base + c];
                // Emit SGR only when the active style changes.
                let nc = (
                    cell.fg,
                    cell.bg,
                    cell.bold,
                    cell.underline,
                    cell.inverse,
                    cell.italic,
                );
                let pc = (
                    cur_attrs.fg,
                    cur_attrs.bg,
                    cur_attrs.bold,
                    cur_attrs.underline,
                    cur_attrs.inverse,
                    cur_attrs.italic,
                );
                if nc != pc {
                    outb.extend_from_slice(sgr(cell.fg, cell.bg, cell.bold, cell.underline, cell.inverse, cell.italic).as_bytes());
                    cur_attrs = Cell {
                        ch: String::new(),
                        fg: cell.fg,
                        bg: cell.bg,
                        bold: cell.bold,
                        underline: cell.underline,
                        inverse: cell.inverse,
                        italic: cell.italic,
                    };
                }
                let ch = if cell.ch.is_empty() { " " } else { &cell.ch };
                outb.extend_from_slice(ch.as_bytes());
                cur.1 += 1;
            }
        }

        if opened {
            // Leave the terminal in the default style so the parked cursor and
            // anything drawn before the next flush start clean too.
            outb.extend_from_slice(b"\x1b[0m");
        }

        let stdout = io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(&outb);
        let _ = lock.flush();

        std::mem::swap(&mut self.prev, &mut self.cells);
    }

    /// Place (and optionally show) the terminal cursor at a cell.
    pub fn park_cursor(&self, r: usize, c: usize, show: bool) {
        let s = format!(
            "{}\x1b[{};{}H",
            if show { "\x1b[?25h" } else { "\x1b[?25l" },
            r + 1,
            c + 1
        );
        out(&s.as_bytes());
    }
}

/// Build an SGR sequence for the given style. 255 means default color.
pub fn sgr(fg: u8, bg: u8, bold: bool, underline: bool, inverse: bool, italic: bool) -> String {
    let mut v = String::from("\x1b[0");
    if fg != DEFAULT {
        let _ = write!(v, ";38;5;{}", fg);
    }
    if bg != DEFAULT {
        let _ = write!(v, ";48;5;{}", bg);
    }
    if bold {
        v.push_str(";1");
    }
    if italic {
        v.push_str(";3");
    }
    if underline {
        v.push_str(";4");
    }
    if inverse {
        v.push_str(";7");
    }
    v.push('m');
    v
}

fn out(bytes: &[u8]) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}

/// Convert a vt100 cell color into a 256-color palette index (255 = default).
pub fn vt_color(c: vt100::Color) -> u8 {
    match c {
        vt100::Color::Default => DEFAULT,
        vt100::Color::Idx(n) => n,
        vt100::Color::Rgb(r, g, b) => rgb_to_256(r, g, b),
    }
}

/// Map an RGB triple to the nearest xterm 256-color index.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let to36 = |x: u8| -> u8 {
        let v = if x < 48 {
            0
        } else if x < 114 {
            1
        } else if x < 178 {
            2
        } else if x < 231 {
            3
        } else if x < 243 {
            4
        } else {
            5
        };
        v
    };
    let (ri, gi, bi) = (to36(r), to36(g), to36(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    // Grayscale approximation for near-neutral colors.
    let gray_lum = (r as i32 * 3 + g as i32 * 4 + b as i32 * 2) / 10;
    let gray = 232 + (gray_lum - 8).clamp(0, 23) as u8;
    let (rc, gc, bc) = (33 * ri + 16, 33 * gi + 16, 33 * bi + 16);
    let dcube = dist2(r, g, b, rc as u32, gc as u32, bc as u32);
    let ggray = 8 + gray * 10;
    let dgray = dist2(r, g, b, ggray as u32, ggray as u32, ggray as u32);
    if dgray < dcube {
        gray
    } else {
        cube
    }
}

fn dist2(r: u8, g: u8, b: u8, r2: u32, g2: u32, b2: u32) -> u32 {
    let dr = r as i32 - r2 as i32;
    let dg = g as i32 - g2 as i32;
    let db = b as i32 - b2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

#[allow(dead_code)]
pub fn cell_key(c: &Cell) -> (String, u8, u8, bool, bool, bool, bool) {
    c.key()
}