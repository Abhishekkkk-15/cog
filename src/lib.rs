pub mod agent;
pub mod bus;
pub mod cli;
pub mod config;
pub mod error;
pub mod memory;
pub mod message;
pub mod nodes;
pub mod provider;
pub mod state;
pub mod tools;
pub mod tui;
pub mod utils;

use std::sync::Arc;

use clap::Parser;

use agent::Agent;
use cli::{Cli, Commands, ConfigAction};
use config::Config;
use error::{CogError, Result};
use memory::{MemoryManager, MistralEmbedder};
use message::Role;
use provider::{DummyProvider, Provider};
use state::Event;
use tools::ToolRegistry;

const SYSTEM_PROMPT: &str = "You are cog, a terminal-based coding assistant with tools to read, search, write, and run code. \
When asked to build, create, fix, or modify something, actually do it using the available tools — do not just describe, outline, \
or propose steps in prose. A reply with no tool calls ends the task immediately, so writing a plan instead of executing it means \
nothing actually gets built. Take action by default. If you genuinely need to ask the user something before proceeding, use the \
ask_user tool rather than ending your reply with a question — a plain-text question will not be answered, since the task ends as \
soon as you stop calling tools.";

fn build_provider(config: &Config) -> Result<Box<dyn Provider>> {
    let name = config.defaults.provider.as_str();
    // Dummy is for tests/scripting — its exact response sequencing matters,
    // so it skips the retry wrapper entirely. Every real backend gets it,
    // since none of them retry transient errors (429/5xx) on their own.
    if name == "dummy" {
        return Ok(Box::new(DummyProvider::echo()));
    }
    let provider: Box<dyn Provider> = match name {
        "mistral" => Box::new(provider::mistral::build(&config.resolve_provider("mistral")?)),
        "groq" => Box::new(provider::groq::build(&config.resolve_provider("groq")?)),
        "nvidia" => Box::new(provider::nvidia::build(&config.resolve_provider("nvidia")?)),
        "openai" => Box::new(provider::openai::build(&config.resolve_provider("openai")?)),
        "custom" => Box::new(provider::custom::build(&config.resolve_provider("custom")?)?),
        other => return Err(CogError::Config(format!("unknown provider '{other}' (expected one of: dummy, mistral, groq, nvidia, openai, custom)"))),
    };
    Ok(Box::new(provider::RetryingProvider::new(provider)))
}

/// Builds a `MemoryManager` backed by `MistralEmbedder` if the mistral
/// provider config exists. Returns `None` gracefully (no crash) if
/// either the config or DB path can't be resolved — memory features
/// just stay disabled.
fn build_memory(config: &Config) -> Option<Arc<tokio::sync::Mutex<MemoryManager>>> {
    let mistral_cfg = config.resolve_provider("mistral").ok()?;
    let embedder = Arc::new(MistralEmbedder::from_config(&mistral_cfg));
    let db_path = MemoryManager::default_path().ok()?;
    let manager = MemoryManager::open(&db_path, embedder).ok()?;
    Some(Arc::new(tokio::sync::Mutex::new(manager)))
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let (config, config_path) = Config::load(cli.config.clone())?;
    let config = config.merge_cli(&cli);

    let command = cli.command.clone().unwrap_or(Commands::Chat { prompt: None });

    match command {
        Commands::Config { action } => match action {
            ConfigAction::Show => {
                let text = toml::to_string_pretty(&config).map_err(|e| error::CogError::Config(e.to_string()))?;
                println!("{text}");
            }
            ConfigAction::Path => {
                println!("{}", config_path.display());
            }
        },
        Commands::Run { prompt, yes } => {
            let provider = build_provider(&config)?;
            let memory = build_memory(&config);
            let agent = Agent::new(provider, ToolRegistry::new(), config.defaults.model.clone()).with_system_prompt(SYSTEM_PROMPT).with_auto_approve(yes);
            let agent = if let Some(mem) = memory { agent.with_memory(mem) } else { agent };

            agent.spawn_nodes().await;
            let bus = agent.bus.clone();
            let state = agent.state.clone();
            let mut bus_rx = bus.subscribe();
            let _ = bus.publish(Event::GoalReceived(prompt));

            let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), async {
                loop {
                    match bus_rx.recv().await {
                        Ok(Event::RunFinished(success)) => break Some(success),
                        Ok(_) => continue,
                        Err(_) => break None,
                    }
                }
            })
            .await;

            let success = match outcome {
                Ok(Some(success)) => {
                    if !success {
                        eprintln!("cog: run failed verification after exhausting retries");
                    }
                    success
                }
                Ok(None) => {
                    eprintln!("error: agent event bus closed unexpectedly");
                    false
                }
                Err(_) => {
                    eprintln!("error: run timed out after 300s");
                    false
                }
            };

            let st = state.read().await;
            if let Some(last) = st.conversation.messages.iter().rev().find(|m| m.role == Role::Assistant) {
                println!("{}", last.content.as_deref().unwrap_or(""));
            }
            drop(st);

            if !success {
                std::process::exit(1);
            }
        }
        Commands::Chat { prompt } => {
            let provider = build_provider(&config)?;
            let memory = build_memory(&config);
            tui::run_tui(prompt.clone(), provider, ToolRegistry::new(), config.defaults.model.clone(), config.defaults.provider.clone(), SYSTEM_PROMPT, memory)
                .await?;
        }
    }

    Ok(())
}
