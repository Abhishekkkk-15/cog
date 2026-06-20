use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{App, PendingPrompt};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let popup_area = centered_rect(70, 50, area);
    frame.render_widget(Clear, popup_area);

    let (title, body, footer) = match &app.pending_prompt {
        Some(PendingPrompt::Confirm { tool_name, description, .. }) => {
            ("Confirm action".to_string(), format!("Allow tool '{tool_name}' to run?\n\n{description}"), "[y]es / [a]lways / [n]o".to_string())
        }
        Some(PendingPrompt::Ask { question, options, .. }) => {
            let opts = if options.is_empty() { String::new() } else { format!("\noptions: {}", options.join(", ")) };
            (
                "Question".to_string(),
                format!("{question}{opts}"),
                "type your answer below and press Enter".to_string(),
            )
        }
        None => return,
    };

    let block = Block::bordered().title(title);
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // The footer (the y/a/n options, or the Ask instruction) gets its own
    // fixed-height row pinned to the bottom of the popup — otherwise a long
    // multi-line description (e.g. a diff) pushes it below the visible
    // area with no scrolling, hiding it entirely.
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows[0]);
    frame.render_widget(Paragraph::new(footer).style(Style::new().fg(Color::Yellow).bold()), rows[1]);
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
