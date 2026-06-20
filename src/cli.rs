use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cog", about = "A terminal-based AI coding assistant")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Override the model from config.
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Override the provider from config.
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Override the API key for the selected provider.
    #[arg(long, global = true)]
    pub api_key: Option<String>,

    /// Path to a config file, overriding the platform default.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    /// Launch the interactive TUI.
    Chat {
        /// Optional initial prompt to send on startup.
        prompt: Option<String>,
    },
    /// Inspect or manage configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Run a single headless agent turn and print the final answer.
    Run {
        prompt: String,
        /// Auto-approve any confirmation-gated tool calls.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand, Clone)]
pub enum ConfigAction {
    /// Print the resolved (loaded + CLI-merged) configuration.
    Show,
    /// Print the path to the config file in use.
    Path,
}
