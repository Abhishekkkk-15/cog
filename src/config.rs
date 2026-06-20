use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::error::{CogError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Defaults {
    pub model: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

const DEFAULT_CONFIG_TEMPLATE: &str = r#"# cog configuration
[defaults]
model = "mistral-large-latest"
provider = "mistral"

[providers.mistral]
api_key = ""
base_url = "https://api.mistral.ai/v1"

[providers.groq]
api_key = ""
base_url = "https://api.groq.com/openai/v1"

[providers.openai]
api_key = ""
base_url = "https://api.openai.com/v1"

[providers.nvidia]
api_key = ""
base_url = "https://integrate.api.nvidia.com/v1"

[providers.custom]
api_key = ""
base_url = ""
model = ""
"#;

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::config_dir().ok_or_else(|| CogError::Config("could not determine platform config directory".into()))?;
        Ok(dir.join("cog").join("config.toml"))
    }

    /// Loads config from `path` (or the platform default), creating a
    /// commented default file on first run if none exists.
    pub fn load(path: Option<PathBuf>) -> Result<(Config, PathBuf)> {
        let path = match path {
            Some(p) => p,
            None => Self::default_path()?,
        };

        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
        }

        let text = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&text).map_err(|e| CogError::Config(format!("{}: {e}", path.display())))?;
        Ok((config, path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| CogError::Config(e.to_string()))?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Applies CLI overrides (model/provider/api_key) with CLI taking precedence.
    pub fn merge_cli(mut self, cli: &Cli) -> Config {
        if let Some(model) = &cli.model {
            self.defaults.model = model.clone();
        }
        if let Some(provider) = &cli.provider {
            self.defaults.provider = provider.clone();
        }
        if let Some(api_key) = &cli.api_key {
            let provider_name = self.defaults.provider.clone();
            self.providers.entry(provider_name).or_default().api_key = Some(api_key.clone());
        }
        self
    }

    pub fn resolve_provider(&self, name: &str) -> Result<ProviderConfig> {
        self.providers
            .get(name)
            .cloned()
            .ok_or_else(|| CogError::Config(format!("no configuration found for provider '{name}'")))
    }
}
