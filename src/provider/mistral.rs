use crate::config::ProviderConfig;

use super::{openai_compatible::OpenAiCompatible, ProviderQuirks};

pub fn build(cfg: &ProviderConfig) -> OpenAiCompatible {
    let base_url = cfg.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.mistral.ai/v1".to_string());
    // Mistral supports parallel tool calls but doesn't guarantee array order
    // reflects intended execution order — we execute in array order anyway,
    // there's no better signal available.
    let quirks = ProviderQuirks { supports_tool_choice_required: false, supports_parallel_tool_calls: true, ..Default::default() };
    OpenAiCompatible::new("mistral", base_url, cfg.api_key.clone().filter(|s| !s.is_empty()), quirks)
}
