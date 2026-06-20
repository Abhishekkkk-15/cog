use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" Memory Inspector (Ctrl+M) ")
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    match &app.memory_snapshot {
        Some(snapshot) => {
            lines.push(Line::from(Span::styled(
                "─── Facts ───",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));

            if snapshot.facts.is_empty() {
                lines.push(Line::from(Span::styled("  (none)", Style::new().fg(Color::DarkGray))));
            } else {
                for (key, value) in snapshot.facts.iter().take(10) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {key}"), Style::new().fg(Color::Green)),
                        Span::raw(": "),
                        Span::styled(truncate(value, 40), Style::new().fg(Color::White)),
                    ]));
                }
                if snapshot.facts.len() > 10 {
                    lines.push(Line::from(Span::styled(
                        format!("  ... and {} more", snapshot.facts.len() - 10),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }

            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "─── Sessions ───",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));

            if snapshot.sessions.is_empty() {
                lines.push(Line::from(Span::styled("  (none)", Style::new().fg(Color::DarkGray))));
            } else {
                for (id, project_root, _created_at) in snapshot.sessions.iter().take(5) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {id}"), Style::new().fg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(truncate(project_root, 30), Style::new().fg(Color::DarkGray)),
                    ]));
                }
            }

            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("Code chunks: ", Style::new().fg(Color::Yellow)),
                Span::raw(format!("{}", snapshot.code_chunks_count)),
            ]));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "Loading...",
                Style::new().fg(Color::DarkGray),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
