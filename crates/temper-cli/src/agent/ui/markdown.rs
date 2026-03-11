//! Markdown rendering to terminal using `termimad`.
//!
//! Wraps termimad with terminal-width–aware rendering and
//! optional no-color fallback for piped output.

use termimad::MadSkin;

/// Render markdown text to a styled terminal string.
///
/// Respects terminal width. Returns plain text if `use_color` is false.
#[allow(dead_code)]
pub fn render(text: &str, width: usize, use_color: bool) -> String {
    if !use_color {
        return text.to_string();
    }

    let mut skin = MadSkin::default();
    // Limit table/paragraph width to terminal width.
    skin.set_headers_fg(termimad::crossterm::style::Color::Cyan);
    skin.bold.set_fg(termimad::crossterm::style::Color::White);
    skin.italic.set_fg(termimad::crossterm::style::Color::Grey);
    skin.inline_code
        .set_bg(termimad::crossterm::style::Color::DarkGrey);

    let area = termimad::Area::new(0, 0, width as u16, u16::MAX);
    let text = skin.area_text(text, &area);
    text.to_string()
}
