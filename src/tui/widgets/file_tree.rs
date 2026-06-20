use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, List, ListItem, ListState};
use ratatui::Frame;

use crate::tui::app::{App, Focus};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .file_tree
        .entries
        .iter()
        .map(|entry| {
            let name = entry.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let indent = "  ".repeat(entry.depth);
            let marker = if entry.is_dir {
                if app.file_tree.expanded.contains(&entry.path) { "v" } else { ">" }
            } else {
                " "
            };
            let modified = if app.file_tree.modified.contains(&entry.path) { " *" } else { "" };
            let style = if entry.is_dir { Style::new().fg(Color::Blue) } else { Style::default() };
            ListItem::new(format!("{indent}{marker} {name}{modified}")).style(style)
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(app.file_tree.selected));
    let title = if app.focus == Focus::FileTree { "Files [Tab to switch]" } else { "Files" };
    let list = List::new(items).block(Block::bordered().title(title)).highlight_style(Style::new().bg(Color::DarkGray));
    frame.render_stateful_widget(list, area, &mut state);
}
