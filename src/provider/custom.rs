use crate::config::ProviderConfig;
use crate::error::CogError;

use super::{openai_compatible::OpenAiCompatible, ProviderQuirks};

pub fn build(cfg: &ProviderConfig) -> Result<OpenAiCompatible, CogError> {
    let base_url = cfg
        .base_url
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CogError::Config("the 'custom' provider requires base_url to be set in config".into()))?;
    let quirks = ProviderQuirks { supports_tool_choice_required: true, supports_parallel_tool_calls: true, ..Default::default() };
    Ok(OpenAiCompatible::new("custom", base_url, cfg.api_key.clone().filter(|s| !s.is_empty()), quirks))
}
