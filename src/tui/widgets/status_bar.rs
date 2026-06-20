use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{App, ConnectionStatus};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let status = &app.status;
    let connection = match &status.connection {
        ConnectionStatus::Idle => "idle".to_string(),
        ConnectionStatus::Connecting => "connecting...".to_string(),
        ConnectionStatus::Streaming => "streaming".to_string(),
        ConnectionStatus::Error(e) => format!("error: {e}"),
    };
    let mem_stats = if status.facts_count > 0 || status.vectors_count > 0 {
        format!("  |  mem: {} facts, {} chunks", status.facts_count, status.vectors_count)
    } else {
        String::new()
    };

    let text =
        format!(" {} / {}  |  tokens {}/{}{mem_stats}  |  {}", status.provider, status.model, status.total_tokens, status.token_budget, connection);
    frame.render_widget(Paragraph::new(text).style(Style::new().bg(Color::Black).fg(Color::White)), area);
}
