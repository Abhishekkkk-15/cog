mod chat_panel;
mod confirm_popup;
mod file_tree;
mod input_bar;
mod memory_inspector;
mod status_bar;

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use super::app::{App, RightPanel};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    
    // Bottom input bar is 1 line. Status bar is 1 line. Rest is chat.
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).split(area);
    
    chat_panel::render(frame, outer[0], app);

    if let Some((current, total)) = app.indexing {
        let status_layout = Layout::horizontal([Constraint::Percentage(80), Constraint::Percentage(20)]).split(outer[1]);
        status_bar::render(frame, status_layout[0], app);
        
        let percent = (current as f64 / total as f64 * 100.0) as u16;
        let progress = ratatui::widgets::Gauge::default()
            .gauge_style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan))
            .percent(percent.min(100))
            .label(format!("Indexing {}/{}", current, total));
        frame.render_widget(progress, status_layout[1]);
    } else {
        status_bar::render(frame, outer[1], app);
    }

    input_bar::render(frame, outer[2], app);

    // Render popups over the chat
    if app.focus != crate::tui::app::Focus::Input {
        let popup_area = centered_rect(60, 60, area);
        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(ratatui::widgets::Block::bordered().title("Overlay"), popup_area);
        
        let inner_popup = popup_area.inner(ratatui::layout::Margin { vertical: 1, horizontal: 1 });
        match app.right_panel {
            RightPanel::FileTree => file_tree::render(frame, inner_popup, app),
            RightPanel::MemoryInspector => memory_inspector::render(frame, inner_popup, app),
        }
    }

    if app.pending_prompt.is_some() {
        confirm_popup::render(frame, area, app);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}
