use ratatui::layout::Rect;
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let prefix = match app.mode {
        InputMode::Normal => "> ",
        InputMode::Command => ": ",
        InputMode::Confirm => "? ",
    };
    let text = format!("{prefix}{}", app.input.value());
    frame.render_widget(Paragraph::new(text), area);

    let cursor_x = area.x + prefix.len() as u16 + app.input.visual_cursor() as u16;
    let cursor_y = area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}
