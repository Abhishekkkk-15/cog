use crate::config::ProviderConfig;

use super::{openai_compatible::OpenAiCompatible, ProviderQuirks};

pub fn build(cfg: &ProviderConfig) -> OpenAiCompatible {
    let base_url = cfg.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.groq.com/openai/v1".to_string());
    let quirks = ProviderQuirks {
        supports_tool_choice_required: false,
        supports_parallel_tool_calls: true,
        // Community reports indicate Groq's streaming tool_calls deltas
        // sometimes omit `index` — handled defensively regardless of this
        // flag (falls back to array position), kept here for documentation.
        streaming_omits_index: true,
        send_tool_message_name_field: false,
    };
    OpenAiCompatible::new("groq", base_url, cfg.api_key.clone().filter(|s| !s.is_empty()), quirks)
}
