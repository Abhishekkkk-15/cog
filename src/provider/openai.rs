use crate::config::ProviderConfig;

use super::{openai_compatible::OpenAiCompatible, ProviderQuirks};

pub fn build(cfg: &ProviderConfig) -> OpenAiCompatible {
    let base_url = cfg.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let quirks = ProviderQuirks { supports_tool_choice_required: true, supports_parallel_tool_calls: true, ..Default::default() };
    OpenAiCompatible::new("openai", base_url, cfg.api_key.clone().filter(|s| !s.is_empty()), quirks)
}
