use crate::config::ProviderConfig;

use super::{openai_compatible::OpenAiCompatible, ProviderQuirks};

pub fn build(cfg: &ProviderConfig) -> OpenAiCompatible {
    let base_url = cfg.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://integrate.api.nvidia.com/v1".to_string());
    // NIM endpoints are frequently vLLM-backed and inconsistently support
    // forced tool_choice / parallel calls across hosted models — stay
    // conservative rather than assume the most permissive behavior.
    let quirks = ProviderQuirks { supports_tool_choice_required: false, supports_parallel_tool_calls: false, ..Default::default() };
    OpenAiCompatible::new("nvidia", base_url, cfg.api_key.clone().filter(|s| !s.is_empty()), quirks)
}
