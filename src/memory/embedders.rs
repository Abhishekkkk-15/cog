use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::ProviderConfig;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("response parse error: {0}")]
    Parse(String),
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Batch-capable: both fact-recall and code-chunk indexing embed many
    /// strings per call, so this avoids one HTTP round-trip per string.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn dimensions(&self) -> usize;
}

/// Reuses the existing `[providers.mistral]` config/API key rather than a
/// separate embeddings credential — a deliberate choice made with the user,
/// independent of whichever provider is configured as the active chat model.
pub struct MistralEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

const MISTRAL_EMBED_MODEL: &str = "mistral-embed";
const MISTRAL_EMBED_DIMS: usize = 1024;

impl MistralEmbedder {
    pub fn from_config(cfg: &ProviderConfig) -> Self {
        let base_url = cfg.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.mistral.ai/v1".to_string());
        MistralEmbedder { client: reqwest::Client::new(), base_url, api_key: cfg.api_key.clone().filter(|s| !s.is_empty()) }
    }

    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl Embedder for MistralEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = json!({ "model": MISTRAL_EMBED_MODEL, "input": texts });
        let mut request = self.client.post(self.endpoint()).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let resp = request.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(EmbedError::Api { status: status.as_u16(), message });
        }

        let body: Value = resp.json().await.map_err(|e| EmbedError::Parse(e.to_string()))?;
        let data = body.get("data").and_then(Value::as_array).ok_or_else(|| EmbedError::Parse("missing data[]".into()))?;

        // Preserve request order via the response's `index` field rather
        // than assuming array order — defensive, costs nothing.
        let mut ordered: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (position, item) in data.iter().enumerate() {
            let index = item.get("index").and_then(Value::as_u64).map(|v| v as usize).unwrap_or(position);
            let embedding: Vec<f32> = item
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| EmbedError::Parse("missing embedding[]".into()))?
                .iter()
                .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                .collect();
            ordered.push((index, embedding));
        }
        ordered.sort_by_key(|(i, _)| *i);
        Ok(ordered.into_iter().map(|(_, v)| v).collect())
    }

    fn dimensions(&self) -> usize {
        MISTRAL_EMBED_DIMS
    }
}
