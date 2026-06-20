use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{App, PendingPrompt};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(60, 30, area);
    frame.render_widget(Clear, popup_area);

    let (title, body) = match &app.pending_prompt {
        Some(PendingPrompt::Confirm { tool_name, description, .. }) => {
            ("Confirm action".to_string(), format!("Allow tool '{tool_name}' to run?\n\n{description}\n\n[y]es / [n]o"))
        }
        Some(PendingPrompt::Ask { question, options, .. }) => {
            let opts = if options.is_empty() { String::new() } else { format!("\n\noptions: {}", options.join(", ")) };
            ("Question".to_string(), format!("{question}{opts}\n\ntype your answer below and press Enter"))
        }
        None => return,
    };

    frame.render_widget(Paragraph::new(body).block(Block::bordered().title(title)).wrap(Wrap { trim: false }), popup_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical =
        Layout::vertical([Constraint::Percentage((100 - percent_y) / 2), Constraint::Percentage(percent_y), Constraint::Percentage((100 - percent_y) / 2)])
            .split(area);
    let horizontal =
        Layout::horizontal([Constraint::Percentage((100 - percent_x) / 2), Constraint::Percentage(percent_x), Constraint::Percentage((100 - percent_x) / 2)])
            .split(vertical[1]);
    horizontal[1]
}
