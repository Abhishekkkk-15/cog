use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{App, ChatLine};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    for line in &app.lines {
        match line {
            ChatLine::User(text) => lines.push(Line::from(vec![Span::styled("● You: ", Style::new().fg(Color::Green).bold()), Span::raw(text.clone())])),
            ChatLine::Assistant(text) => {
                lines.push(Line::from(vec![Span::styled("○ Cog: ", Style::new().fg(Color::Blue).bold())]));
                let md_text = tui_markdown::from_str(text);
                for md_line in md_text.lines {
                    lines.push(md_line);
                }
            }
            ChatLine::ToolCall { name, args_preview, result_preview, success, .. } => {
                let (icon, color) = match success {
                    Some(true) => ("✔", Color::DarkGray),
                    Some(false) => ("✖", Color::Red),
                    None => ("⠋", Color::Yellow),
                };
                lines.push(Line::from(Span::styled(format!("  {icon} {name} {args_preview}"), Style::new().fg(color))));
                if let Some(result) = result_preview {
                    lines.push(Line::from(Span::styled(format!("    {result}"), Style::new().fg(Color::DarkGray))));
                }
            }
            ChatLine::SystemNote(text) => lines.push(Line::from(Span::styled(text.clone(), Style::new().fg(Color::Red)))),
        }
    }

    let total = lines.len() as u16;
    let height = area.height;
    let max_scroll = total.saturating_sub(height);
    let scroll = max_scroll.saturating_sub(app.scroll_offset as u16);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}
