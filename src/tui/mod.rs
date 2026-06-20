mod app;
pub mod event;
mod widgets;

pub use app::{AgentToUi, ConnectionStatus, MemorySnapshot, SlashCommand, UiToAgent};

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use notify::{RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::memory::MemoryManager;
use crate::provider::Provider;
use crate::tools::ToolRegistry;
use app::App;

const TICK_RATE: Duration = Duration::from_millis(250);
const TOKEN_BUDGET: usize = 128_000;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn init_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// Restores the terminal even if a panic unwinds through the render loop —
/// otherwise the user's shell is left in raw mode / the alternate screen.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original(info);
    }));
}

pub async fn run_tui(
    initial_prompt: Option<String>,
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    model: String,
    provider_name: String,
    system_prompt: &str,
    memory: Option<Arc<tokio::sync::Mutex<MemoryManager>>>,
) -> io::Result<()> {
    install_panic_hook();
    let mut terminal = init_terminal()?;

    let agent = Agent::new(provider, tools, model.clone()).with_system_prompt(system_prompt);
    let mut agent = if let Some(mem) = memory {
        agent.with_memory(mem)
    } else {
        agent
    };
    
    agent.spawn_nodes().await;
    let bus = agent.bus.clone();
    let state = agent.state.clone();
    let agent_task = tokio::spawn(async move { 
        // Legacy run_interactive is removed. Keep task alive.
        tokio::time::sleep(tokio::time::Duration::from_secs(99999)).await;
    });

    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut app = App::new(root.clone(), model, provider_name, TOKEN_BUDGET, bus.clone(), state.clone());

    if let Some(prompt) = initial_prompt {
        app.push_user_line(prompt.clone());
        let _ = bus.publish(crate::state::Event::GoalReceived(prompt));
    }

    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            for path in event.paths {
                let path_str = path.to_string_lossy();
                if !path_str.contains("target") && !path_str.contains(".git") && !path_str.contains("node_modules") {
                    let _ = fs_tx.send(path);
                }
            }
        }
    })
    .ok();
    if let Some(w) = watcher.as_mut() {
        let _ = w.watch(&root, RecursiveMode::Recursive);
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK_RATE);
    
    // Subscribe to EventBus for UI updates
    let mut bus_rx = bus.subscribe();

    let mut needs_refresh = false;
    let mut needs_draw = true;

    loop {
        if needs_draw {
            terminal.draw(|frame| widgets::render(frame, &app))?;
            needs_draw = false;
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        needs_draw = true;
                        if let Some(action) = event::handle_key(&mut app, key) {
                            if matches!(action, UiToAgent::Quit) {
                                app.should_quit = true;
                            }
                            match action {
                                UiToAgent::UserPrompt(prompt) => { let _ = bus.publish(crate::state::Event::GoalReceived(prompt)); },
                                UiToAgent::SlashCommand(cmd) => {
                                    match cmd {
                                        SlashCommand::Auth { provider, key } => {
                                            if let Ok(path) = crate::config::Config::default_path() {
                                                if let Ok((mut config, _)) = crate::config::Config::load(Some(path.clone())) {
                                                    config.providers.entry(provider.clone()).or_default().api_key = Some(key);
                                                    if config.save(&path).is_ok() {
                                                        app.lines.push(crate::tui::app::ChatLine::SystemNote(format!("Saved API key for provider '{}'", provider)));
                                                    } else {
                                                        app.lines.push(crate::tui::app::ChatLine::SystemNote("Failed to save config".into()));
                                                    }
                                                }
                                            }
                                        }
                                        SlashCommand::SwitchModel(new_model) => {
                                            app.status.model = new_model.clone();
                                            app.lines.push(crate::tui::app::ChatLine::SystemNote(format!("Switched model to '{}'. Note: This requires restart in the current architecture.", new_model)));
                                        }
                                        _ => {
                                            app.lines.push(crate::tui::app::ChatLine::SystemNote("Command not yet supported in Phase 4".into()));
                                        }
                                    }
                                }
                                UiToAgent::RequestMemorySnapshot => {},
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        needs_draw = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => app.should_quit = true,
                }
            }
            Ok(agent_event) = bus_rx.recv() => {
                app.handle_agent_event(agent_event);
                needs_draw = true;
                
                // Batch process any remaining events in the queue to prevent drawing after every single token
                while let Ok(evt) = bus_rx.try_recv() {
                    app.handle_agent_event(evt);
                }
            }
            Some(path) = fs_rx.recv() => {
                let mut paths = vec![path];
                while let Ok(p) = fs_rx.try_recv() {
                    paths.push(p);
                }
                
                for path in paths {
                    app.file_tree.modified.insert(path);
                    needs_refresh = true;
                }
            }
            _ = tick.tick() => {
                if needs_refresh {
                    app.file_tree.refresh();
                    needs_refresh = false;
                    needs_draw = true;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    let _ = agent_task.abort();
    restore_terminal()?;
    Ok(())
}
