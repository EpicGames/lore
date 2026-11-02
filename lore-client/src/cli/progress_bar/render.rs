// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Pure rendering primitives for the progress indicator. Stateless and
//! side-effect-free; the render thread composes these to produce the line.

/// Fields needed to render one determinate progress bar line. Borrows the
/// string slices so taking a snapshot under the data lock is allocation-free.
#[derive(Clone)]
pub struct ProgressBarSnapshot<'a> {
    pub progress: u64,
    pub max: u64,
    pub is_growing: bool,
    pub message: &'a str,
    pub units: Option<&'a str>,
    pub fg_color: anstyle::Style,
    pub bg_color: anstyle::Style,
}

/// Renders `[<cells>] <counter>  <message>`. Returns an empty string when
/// `bar_width == 0` or `snap.max == 0`.
pub fn format_bar_line(snap: &ProgressBarSnapshot<'_>, bar_width: u16) -> String {
    if bar_width == 0 || snap.max == 0 {
        return String::new();
    }

    let cells = fractional_cells(snap.progress, snap.max, bar_width);
    let fill_cells = (snap.progress * bar_width as u64 / snap.max) as u16;

    let counter = format_counter(snap.progress, snap.max, snap.units, snap.is_growing);
    let reset = anstyle::Reset;

    // Paint fg over the filled prefix and bg over the rest.
    let filled_end = (fill_cells as usize).min(cells.len());
    let fg_str: String = cells[..filled_end].iter().collect();
    let bg_str: String = cells[filled_end..].iter().collect();

    let fg_color = snap.fg_color;
    let bg_color = snap.bg_color;

    if snap.message.is_empty() {
        format!("[{fg_color}{fg_str}{reset}{bg_color}{bg_str}{reset}] {counter}")
    } else {
        format!(
            "[{fg_color}{fg_str}{reset}{bg_color}{bg_str}{reset}] {counter}  {message}",
            message = snap.message
        )
    }
}

/// The progress as a line of plain text, for output that is not a terminal: the counter and the
/// message, or the message alone for a spinner.
pub fn format_plain_line(
    progress: u64,
    max: u64,
    units: Option<&str>,
    growing: bool,
    message: &str,
) -> String {
    if max == 0 {
        return message.to_string();
    }
    let counter = format_counter(progress, max, units, growing);
    if message.is_empty() {
        counter
    } else {
        format!("{counter}  {message}")
    }
}

pub fn format_spinner_line(frame: char, message: &str) -> String {
    if message.is_empty() {
        frame.to_string()
    } else {
        format!("{frame} {message}")
    }
}

pub fn spinner_frame(tick: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[tick as usize % 10]
}

pub fn format_counter(progress: u64, max: u64, units: Option<&str>, growing: bool) -> String {
    let growing_marker = if growing { "+" } else { "" };
    match units {
        Some(u) => format!("{progress}/{max}{growing_marker} {u}"),
        None => format!("{progress}/{max}{growing_marker}"),
    }
}

/// Returns `bar_width` cells representing `progress/max`. Fully-filled cells
/// use `█`; the first partial cell uses `░ ▒ ▓` at 1/4, 1/2, 3/4 thresholds;
/// unfilled cells are `' '`. Empty vec when `max == 0` or `bar_width == 0`.
pub fn fractional_cells(progress: u64, max: u64, bar_width: u16) -> Vec<char> {
    if max == 0 || bar_width == 0 {
        return Vec::new();
    }

    let width = bar_width as usize;
    let fill = progress as f64 / max as f64 * width as f64;
    let full_cells = fill.floor() as usize;
    let remainder = fill - fill.floor();

    let partial = if full_cells >= width {
        None
    } else {
        let ch = if remainder >= 0.75 {
            '▓'
        } else if remainder >= 0.50 {
            '▒'
        } else if remainder >= 0.25 {
            '░'
        } else {
            ' '
        };
        Some(ch)
    };

    let full_count = full_cells.min(width);
    let mut cells = vec!['█'; full_count];
    if let Some(ch) = partial {
        cells.push(ch);
    }
    cells.resize(width, ' ');
    cells
}
