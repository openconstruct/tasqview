//! The Win95-style taskbar: wide labelled session buttons on the left,
//! clock/battery tray on the right, all on the single bottom row.

use crate::screen::Canvas;

/// Win95 silver grey and friends (256-color palette indices).
pub const GRAY: u8 = 250; // inactive button face (light, "raised")
pub const GRAY_DARK: u8 = 240; // active button face (dark gray, "pressed")
pub const BAR: u8 = 244; // taskbar base fill
pub const BLACK: u8 = 0;
pub const WHITE: u8 = 15;
pub const RED: u8 = 9;

/// Widest a single session button may get (incl. padding + close marker).
const BTN_MAX: usize = 22;
/// Narrowest a session button may shrink to before we stop drawing more.
const BTN_MIN: usize = 10;
/// Columns reserved on the right for the clock/battery tray.
const TRAY_RESERVE: usize = 24;

pub struct Button {
    /// Column range of the clickable button body.
    pub c0: usize,
    pub c1: usize,
    pub idx: usize,
    /// Column of the close ("x") marker, if any.
    pub close: Option<usize>,
}

#[derive(Default)]
pub struct Layout {
    pub buttons: Vec<Button>,
    /// First column of the system tray (not clickable).
    pub tray0: usize,
    /// Clickable column range of the "Start" button (inclusive).
    pub start_c0: usize,
    pub start_c1: usize,
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

    // Base fill.
    for c in 0..canvas.cols {
        canvas.set_cell(row, c, " ", BLACK, BAR, false, false, false, false);
    }

    let mut c = 0usize;

    // "Start" look-alike so it feels like Win95.
    let start_txt = " Start ";
    for (j, ch) in start_txt.chars().enumerate() {
        if c + j < canvas.cols {
            canvas.set_cell(row, c + j, &ch.to_string(), BLACK, GRAY, true, false, false, false);
        }
    }
    layout.start_c0 = 0;
    layout.start_c1 = start_txt
        .chars()
        .count()
        .saturating_sub(1)
        .min(canvas.cols.saturating_sub(1));
    c += start_txt.chars().count() + 1;

    // Session buttons: wide, labelled with the running program, active one
    // sunken (dark). Width adapts to how many tabs must fit.
    let btn_area_end = canvas.cols.saturating_sub(TRAY_RESERVE).max(c);
    let avail = btn_area_end.saturating_sub(c);
    let n = sessions.len().max(1);
    let bw = (avail / n).clamp(BTN_MIN, BTN_MAX);

    for (idx, label) in sessions {
        if c + bw > btn_area_end {
            break;
        }
        let active_btn = *idx == active;
        let (bg, fg) = if active_btn {
            (GRAY_DARK, WHITE)
        } else {
            (GRAY, BLACK)
        };

        // Body layout: leading space, label, trailing pad, then close marker.
        let text_w = bw.saturating_sub(3);
        let name: String = {
            let cn = label.chars().count();
            if cn > text_w && text_w >= 2 {
                let mut s: String = label.chars().take(text_w - 1).collect();
                s.push('…');
                s
            } else {
                label.chars().take(text_w).collect()
            }
        };
        let mut body = format!(" {name}");
        while body.chars().count() < bw - 1 {
            body.push(' ');
        }

        let c0 = c;
        for (j, ch) in body.chars().enumerate() {
            if c + j < canvas.cols {
                canvas.set_cell(row, c + j, &ch.to_string(), fg, bg, active_btn, false, false, false);
            }
        }
        c += body.chars().count();
        let c1 = c.saturating_sub(1);

        // Close marker.
        let close_col = c;
        if c < canvas.cols {
            canvas.set_cell(row, c, "x", RED, bg, false, false, false, false);
        }
        c += 1;
        // Spacer between buttons.
        if c < canvas.cols {
            canvas.set_cell(row, c, " ", BLACK, BAR, false, false, false, false);
        }
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
            canvas.set_cell(row, layout.tray0 + j, &ch.to_string(), BLACK, BAR, false, false, false, false);
        }
    }

    layout
}
