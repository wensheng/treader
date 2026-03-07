// Terminal setup/teardown and utility drawing functions.
// Manages raw mode, alternate screen, and window size detection without ratatui.

use std::io::{BufReader, Read as _, Write as _, stdout};

use anyhow::Context as _;
use crossterm::{
    cursor, execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, window_size,
    },
};

/// Terminal dimensions in both cells and pixels.
#[derive(Debug, Clone, Copy)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
    /// Total terminal width in pixels.
    pub width_px: u16,
    /// Width of a single cell in pixels.
    pub cell_width_px: u16,
    /// Height of a single cell in pixels.
    pub cell_height_px: u16,
}

impl WindowSize {
    /// Pixel width of the page display area (full terminal width).
    pub fn page_area_width_px(&self) -> f32 {
        self.width_px as f32
    }

    /// Pixel height of the page display area (terminal minus one row for status bar).
    pub fn page_area_height_px(&self) -> f32 {
        ((self.rows.saturating_sub(1)) as f32) * (self.cell_height_px as f32)
    }

    /// Number of rows available for page display (rows minus status bar).
    pub fn page_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }
}

/// RAII guard that enters the alternate screen and raw mode on construction,
/// and restores the terminal on drop.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> anyhow::Result<Self> {
        execute!(
            stdout(),
            EnterAlternateScreen,
            cursor::Hide,
            crossterm::event::EnableMouseCapture,
        )
        .context("failed to enter alternate screen")?;
        enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        reset_terminal();
    }
}

/// Reset terminal state unconditionally (safe to call in panic hooks).
pub fn reset_terminal() {
    // Delete all Kitty graphics images so they don't linger after exit.
    // Raw escape: APC + "Ga=d" + ST  (action=delete, which=all)
    let _ = stdout().write_all(b"\x1b_Ga=d\x1b\\");
    let _ = stdout().flush();

    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        LeaveAlternateScreen,
        cursor::Show,
        crossterm::event::DisableMouseCapture,
    );
}

/// Query the terminal for its dimensions in cells and pixels.
pub fn get_window_size() -> anyhow::Result<WindowSize> {
    let ws = window_size().context("cannot query terminal window size")?;
    let (cols, rows) = crossterm::terminal::size().context("cannot query terminal cell count")?;

    // Some terminals don't report pixel dimensions via TIOCGWINSZ; fall back to escape sequence.
    let (width_px, height_px) = if ws.width > 0 && ws.height > 0 && cols > 0 && rows > 0 {
        (ws.width, ws.height)
    } else if cols > 0 && rows > 0 {
        match get_font_size_through_escape() {
            Ok((cell_w, cell_h)) => (cell_w * cols, cell_h * rows),
            Err(_) => {
                // Best-effort fallback: assume 10×20 px cells
                (cols * 10, rows * 20)
            }
        }
    } else {
        anyhow::bail!("terminal reported zero rows/cols");
    };

    let cell_width_px = if cols > 0 { width_px / cols } else { 10 };
    let cell_height_px = if rows > 0 { height_px / rows } else { 20 };

    Ok(WindowSize {
        rows,
        cols,
        width_px,
        cell_width_px,
        cell_height_px,
    })
}

/// Query cell pixel size via the `\e[14t` escape sequence
fn get_font_size_through_escape() -> anyhow::Result<(u16, u16)> {
    print!("\x1b[14t");
    stdout().flush()?;

    enable_raw_mode()?;

    let input_vec: Vec<u8> = BufReader::new(std::io::stdin())
        .bytes()
        .filter_map(Result::ok)
        .take_while(|b| *b != b't')
        .collect();

    disable_raw_mode()?;

    let input_line = String::from_utf8(input_vec)?;
    let input_line = input_line
        .trim_start_matches("\x1b[4")
        .trim_start_matches(';');

    // Response format: `\e[4;<height>;<width>t`
    let mut splits = input_line.split([';', 't']);
    let (Some(h_str), Some(w_str)) = (splits.next(), splits.next()) else {
        anyhow::bail!("unparseable terminal size response: {input_line:?}");
    };

    Ok((w_str.parse()?, h_str.parse()?))
}

/// Draw the status bar on the last terminal row.
pub fn draw_status_bar(ws: &WindowSize, content: &str) -> anyhow::Result<()> {
    // Pad/truncate to fill the full row width.
    let padded = format!(
        " {:<width$}",
        content,
        width = (ws.cols as usize).saturating_sub(1)
    );
    execute!(
        stdout(),
        cursor::MoveTo(0, ws.rows.saturating_sub(1)),
        Print(padded),
    )?;
    Ok(())
}

/// Clear all terminal cells above the status bar.
pub fn clear_page_area(ws: &WindowSize) -> anyhow::Result<()> {
    for row in 0..ws.page_rows() {
        execute!(
            stdout(),
            cursor::MoveTo(0, row),
            Clear(ClearType::CurrentLine),
        )?;
    }
    Ok(())
}

/// Print "Loading..." centred in the page area.
pub fn draw_loading(ws: &WindowSize) -> anyhow::Result<()> {
    let msg = "Loading...";
    let x = ws.cols.saturating_sub(msg.len() as u16) / 2;
    let y = ws.page_rows() / 2;
    execute!(stdout(), cursor::MoveTo(x, y), Print(msg))?;
    Ok(())
}

/// Draw the table of contents overlay.
pub fn draw_toc(
    ws: &WindowSize,
    toc: &[crate::renderer::TocEntry],
    scroll: usize,
    current_page: usize,
) -> anyhow::Result<()> {
    let max_rows = ws.page_rows() as usize;
    let cols = ws.cols as usize;

    for (i, entry) in toc.iter().enumerate().skip(scroll).take(max_rows) {
        let row = (i - scroll) as u16;
        let indent = "  ".repeat(entry.level);
        let marker = if entry.page == current_page { ">" } else { " " };
        let highlight = i == scroll;

        let line = format!(
            "{marker}{indent}{title}  (p.{page})",
            title = entry.title,
            page = entry.page + 1,
        );

        // Truncate to terminal width
        let display: String = line.chars().take(cols).collect();

        if highlight {
            // Highlight selected entry with reverse video
            execute!(
                stdout(),
                cursor::MoveTo(0, row),
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
                Print(format!("{:<width$}", display, width = cols)),
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
            )?;
        } else {
            execute!(stdout(), cursor::MoveTo(0, row), Print(display),)?;
        }
    }

    Ok(())
}

/// Draw the metadata overlay (full-screen text).
pub fn draw_metadata(ws: &WindowSize, metadata: &[(String, String)]) -> anyhow::Result<()> {
    let cols = ws.cols as usize;

    execute!(
        stdout(),
        cursor::MoveTo(0, 0),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
        Print(format!("{:<width$}", " Document Metadata", width = cols)),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    for (i, (key, val)) in metadata.iter().enumerate() {
        let row = (i + 2) as u16;
        if row >= ws.page_rows() {
            break;
        }
        let line = format!("  {key}: {val}");
        let display: String = line.chars().take(cols).collect();
        execute!(stdout(), cursor::MoveTo(0, row), Print(display))?;
    }

    if metadata.is_empty() {
        execute!(
            stdout(),
            cursor::MoveTo(2, 2),
            Print("No metadata available."),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(rows: u16, cols: u16, cell_w: u16, cell_h: u16) -> WindowSize {
        WindowSize {
            rows,
            cols,
            width_px: cols * cell_w,
            cell_width_px: cell_w,
            cell_height_px: cell_h,
        }
    }

    #[test]
    fn page_area_width_is_full_terminal() {
        let w = ws(24, 80, 10, 20);
        assert!((w.page_area_width_px() - 800.0).abs() < f32::EPSILON);
    }

    #[test]
    fn page_area_height_excludes_status_bar() {
        let w = ws(25, 80, 10, 20);
        // (25-1) * 20 = 480
        assert!((w.page_area_height_px() - 480.0).abs() < f32::EPSILON);
    }

    #[test]
    fn page_rows_excludes_status_bar() {
        let w = ws(25, 80, 10, 20);
        assert_eq!(w.page_rows(), 24);
    }

    #[test]
    fn page_rows_saturates_at_zero() {
        let w = ws(0, 80, 10, 20);
        assert_eq!(w.page_rows(), 0);
    }

    #[test]
    fn page_area_single_row() {
        let w = ws(1, 80, 10, 20);
        // page_rows = 0, page_area_height = 0
        assert_eq!(w.page_rows(), 0);
        assert!((w.page_area_height_px() - 0.0).abs() < f32::EPSILON);
    }
}

/// Draw the links overlay (numbered list).
pub fn draw_links(
    ws: &WindowSize,
    links: &[crate::renderer::LinkInfo],
    input: &str,
    page_num: usize,
) -> anyhow::Result<()> {
    let cols = ws.cols as usize;

    let title = format!(" Links on page {} ({} found)", page_num + 1, links.len());
    execute!(
        stdout(),
        cursor::MoveTo(0, 0),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
        Print(format!("{:<width$}", title, width = cols)),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    if links.is_empty() {
        execute!(
            stdout(),
            cursor::MoveTo(2, 2),
            Print("No links on this page."),
        )?;
    } else {
        for (i, link) in links.iter().enumerate() {
            let row = (i + 2) as u16;
            if row >= ws.page_rows().saturating_sub(1) {
                break;
            }
            let line = format!("  {}: {}", i + 1, link.text);
            let display: String = line.chars().take(cols).collect();
            execute!(stdout(), cursor::MoveTo(0, row), Print(display))?;
        }
    }

    let prompt_row = ws.page_rows().saturating_sub(1);
    let prompt = format!("Enter number to follow (Esc to cancel): {input}");
    execute!(stdout(), cursor::MoveTo(0, prompt_row), Print(prompt))?;

    Ok(())
}
