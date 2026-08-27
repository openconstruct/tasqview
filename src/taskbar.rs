//! The Win95-style taskbar: session buttons on the left, clock/battery tray
//! on the right, all on the single bottom row.

use crate::screen::Canvas;

/// Win95 silver grey and friends (256-color palette indices).
pub const GRAY: u8 = 188; // #c0c0c0
pub const GRAY_DARK: u8 = 244; // #808080
pub const BLACK: u8 = 0;
pub const WHITE: u8 = 15;
pub const RED: u8 = 9;

pub struct Button {
    /// Column range of the clickable button body.
    pub c0: usize,
    pub c1: usize,
    pub idx: usize,
    /// Column of the close ("X") marker, if any.
    pub close: Option<usize>,
}

#[derive(Default)]
pub struct Layout {
    pub buttons: Vec<Button>,
    /// First column of the system tray (not clickable).
    pub tray0: usize,
}

/// Render the taskbar. `sessions` is (index, label).
pub fn draw(
    canvas: &mut Canvas,
    sessions: &[(usize, String)],
    active: usize,
    clock: &str,
    battery: Option<(u8, bool)>,
) -> Layout {
    let mut layout = Layout::default();
    let row = canvas.rows.saturating_sub(1);
    if canvas.rows == 0 {
        return layout;
    }

    // Base grey fill.
    for c in 0..canvas.cols {
        canvas.set_cell(row, c, " ", BLACK, GRAY, false, false, false, false);
    }

    let mut c = 0usize;

    // "Start" look-alike so it feels like Win95.
    let start_txt = " Start ";
    for (j, ch) in start_txt.chars().enumerate() {
        if c + j < canvas.cols {
            canvas.set_cell(row, c + j, &ch.to_string(), BLACK, GRAY, true, false, false, false);
        }
    }
    c += start_txt.chars().count() + 1;

    // Session buttons.
    for (idx, label) in sessions {
        let prefix = " ";
        let body: String = format!("{}{}", prefix, label);
        let n = body.chars().count() + 1; // +1 for the close marker column
        if c + n >= canvas.cols.saturating_sub(12) {
            break;
        }
        let active_btn = *idx == active;
        let (bg, fg) = if active_btn {
            (GRAY_DARK, WHITE)
        } else {
            (GRAY, BLACK)
        };
        let c0 = c;
        for (j, ch) in body.chars().enumerate() {
            canvas.set_cell(row, c + j, &ch.to_string(), fg, bg, active_btn, false, false, false);
        }
        c += body.chars().count();
        let c1 = c.saturating_sub(1);
        // Close marker.
        canvas.set_cell(row, c, "X", RED, bg, false, false, false, false);
        let close_col = c;
        c += 1;
        // Spacer.
        canvas.set_cell(row, c, " ", BLACK, GRAY, false, false, false, false);
        c += 1;
        layout.buttons.push(Button {
            c0,
            c1,
            idx: *idx,
            close: Some(close_col),
        });
    }

    // Tray: clock + battery (right aligned).
    let batt_str = match battery {
        Some((pct, charging)) => {
            let filled = (pct as usize) * 5 / 100;
            let mut s = format!("{:3}% ", pct);
            for k in 0..5 {
                s.push(if k < filled { '▓' } else { '░' });
            }
            if charging {
                s.push('*');
            }
            s
        }
        None => String::new(),
    };
    let tray = format!("  {}  {}", clock, batt_str);
    let tray_chars: Vec<char> = tray.chars().collect();
    layout.tray0 = canvas.cols.saturating_sub(tray_chars.len());
    if layout.tray0 >= 4 {
        for (j, ch) in tray_chars.iter().enumerate() {
            canvas.set_cell(row, layout.tray0 + j, &ch.to_string(), BLACK, GRAY, false, false, false, false);
        }
    }

    layout
}