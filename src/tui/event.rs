use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_input::backend::crossterm::EventHandler;

use super::app::{App, ConfirmDecision, Focus, InputMode, PendingPrompt, RightPanel};
use super::{SlashCommand, UiToAgent};

/// Translates a raw key event into an action for the agent, mutating `app`'s
/// input/mode state along the way. Mode-aware: a plain chat prompt in Normal
/// mode, a vim-style `:` command line, or a confirmation/ask-user response.
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<UiToAgent> {
    // On at least some Windows terminals, a single physical keypress can
    // surface as both a Press and a Release `KeyEvent` even without the
    // opt-in keyboard-enhancement protocol enabled. Without this guard,
    // every binding below fires twice per press — harmless for one-shot
    // actions, but a toggle (F2) flips on then immediately back off,
    // looking like the panel "appeared for a millisecond then vanished".
    if key.kind != KeyEventKind::Press {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(UiToAgent::Quit);
    }

    match app.mode {
        InputMode::Confirm => handle_confirm_key(app, key),
        InputMode::Normal | InputMode::Command => {
            // F2 toggles the memory inspector panel. Deliberately not a
            // Ctrl+letter combo: in raw terminal input, Ctrl+M sends the
            // exact same byte (\r) as plain Enter, so crossterm reports it
            // as KeyCode::Enter, not Char('m') with CONTROL — the binding
            // would silently never fire. Several other Ctrl combos have
            // the same kind of legacy ASCII collision (Ctrl+I = Tab,
            // Ctrl+H = Backspace, Ctrl+[ = Esc); a function key avoids the
            // whole class of bug.
            if key.code == KeyCode::F(2) {
                match app.right_panel {
                    RightPanel::FileTree => {
                        app.right_panel = RightPanel::MemoryInspector;
                        app.focus = Focus::MemoryInspector;
                        return Some(UiToAgent::RequestMemorySnapshot);
                    }
                    RightPanel::MemoryInspector => {
                        app.right_panel = RightPanel::FileTree;
                        app.focus = Focus::Input;
                    }
                }
                return None;
            }

            if key.code == KeyCode::Tab {
                app.focus = match app.focus {
                    Focus::Input => match app.right_panel {
                        RightPanel::FileTree => Focus::FileTree,
                        RightPanel::MemoryInspector => Focus::MemoryInspector,
                    },
                    Focus::FileTree | Focus::MemoryInspector => Focus::Input,
                };
                return None;
            }
            match app.focus {
                Focus::FileTree => {
                    handle_file_tree_key(app, key);
                    None
                }
                Focus::MemoryInspector => None, // read-only panel, no key handling
                Focus::Input => handle_text_key(app, key),
            }
        }
    }
}

fn handle_file_tree_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => app.file_tree.move_selection(-1),
        KeyCode::Down => app.file_tree.move_selection(1),
        KeyCode::Enter | KeyCode::Char(' ') => app.file_tree.toggle_selected(),
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) -> Option<UiToAgent> {
    let pending = app.pending_prompt.take()?;
    match pending {
        PendingPrompt::Confirm { tool_name, description, respond_to } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let _ = respond_to.send(ConfirmDecision::Once);
                app.mode = InputMode::Normal;
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let _ = respond_to.send(ConfirmDecision::Always);
                app.mode = InputMode::Normal;
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let _ = respond_to.send(ConfirmDecision::Deny);
                app.mode = InputMode::Normal;
                None
            }
            _ => {
                app.pending_prompt = Some(PendingPrompt::Confirm { tool_name, description, respond_to });
                None
            }
        },
        PendingPrompt::Ask { question, options, respond_to } => match key.code {
            KeyCode::Enter => {
                let answer = app.input.value().to_string();
                app.input.reset();
                let _ = respond_to.send(answer);
                app.mode = InputMode::Normal;
                None
            }
            KeyCode::Esc => {
                app.input.reset();
                let _ = respond_to.send(String::new());
                app.mode = InputMode::Normal;
                None
            }
            _ => {
                app.input.handle_event(&Event::Key(key));
                app.pending_prompt = Some(PendingPrompt::Ask { question, options, respond_to });
                None
            }
        },
    }
}

/// Lines scrolled per PageUp/PageDown press. `App` doesn't track the chat
/// panel's actual rendered height (key handling only sees `app`, not frame
/// size), so this is a fixed approximation rather than a true page jump.
const PAGE_SCROLL_LINES: usize = 10;

fn handle_text_key(app: &mut App, key: KeyEvent) -> Option<UiToAgent> {
    match key.code {
        KeyCode::Up => {
            app.scroll_up(1);
            None
        }
        KeyCode::Down => {
            app.scroll_down(1);
            None
        }
        KeyCode::PageUp => {
            app.scroll_up(PAGE_SCROLL_LINES);
            None
        }
        KeyCode::PageDown => {
            app.scroll_down(PAGE_SCROLL_LINES);
            None
        }
        KeyCode::Enter => {
            let text = app.input.value().to_string();
            app.input.reset();
            if text.is_empty() {
                return None;
            }
            if let Some(rest) = text.strip_prefix('/') {
                app.mode = InputMode::Normal;
                return parse_command(rest);
            }
            app.mode = InputMode::Normal;
            app.push_user_line(text.clone());
            Some(UiToAgent::UserPrompt(text))
        }
        KeyCode::Esc => {
            app.input.reset();
            app.mode = InputMode::Normal;
            None
        }
        KeyCode::Char('/') if app.input.value().is_empty() => {
            app.mode = InputMode::Command;
            app.input.handle_event(&Event::Key(key)); // insert the '/' character
            None
        }
        _ => {
            app.input.handle_event(&Event::Key(key));
            None
        }
    }
}

pub fn parse_command(rest: &str) -> Option<UiToAgent> {
    let rest = rest.trim();
    let mut parts = rest.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "q" | "quit" => Some(UiToAgent::Quit),
        "model" => Some(UiToAgent::SlashCommand(SlashCommand::SwitchModel(arg))),
        "provider" => Some(UiToAgent::SlashCommand(SlashCommand::SwitchProvider(arg))),
        "config" => Some(UiToAgent::SlashCommand(SlashCommand::OpenConfig)),
        "search" => Some(UiToAgent::SlashCommand(SlashCommand::Search(arg))),
        "memory" => Some(UiToAgent::SlashCommand(SlashCommand::MemoryStats)),
        "forget" if !arg.is_empty() => Some(UiToAgent::SlashCommand(SlashCommand::Forget(arg))),
        "auth" => {
            let mut sub_parts = arg.splitn(2, ' ');
            let provider = sub_parts.next().unwrap_or("").trim().to_string();
            let key = sub_parts.next().unwrap_or("").trim().to_string();
            if !provider.is_empty() && !key.is_empty() {
                Some(UiToAgent::SlashCommand(SlashCommand::Auth { provider, key }))
            } else {
                None
            }
        }
        "session" => {
            let mut sub_parts = arg.splitn(2, ' ');
            let sub_cmd = sub_parts.next().unwrap_or("");
            let sub_arg = sub_parts.next().unwrap_or("").trim().to_string();
            match sub_cmd {
                "resume" if !sub_arg.is_empty() => Some(UiToAgent::SlashCommand(SlashCommand::SessionResume(sub_arg))),
                _ => None,
            }
        }
        _ => None,
    }
}
