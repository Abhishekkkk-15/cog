use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const DEFAULT_MAX_CHARS: usize = 8000;

#[derive(Deserialize)]
struct WebFetchParams {
    url: String,
    max_chars: Option<usize>,
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over HTTP(S) and return its text content (HTML is converted to plain text)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch."},
                "max_chars": {"type": "integer", "description": "Optional cap on returned characters (default 8000)."}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let params: WebFetchParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| ToolError::Execution(e.to_string()))?;
        let resp = client.get(&params.url).send().await.map_err(|e| ToolError::Execution(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ToolError::Execution(format!("http {}", resp.status())));
        }

        let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = resp.text().await.map_err(|e| ToolError::Execution(e.to_string()))?;

        let text = if content_type.contains("html") { html2text::from_read(body.as_bytes(), 100).unwrap_or(body) } else { body };

        let max_chars = params.max_chars.unwrap_or(DEFAULT_MAX_CHARS);
        if text.chars().count() > max_chars {
            Ok(format!("{}\n... [truncated]", text.chars().take(max_chars).collect::<String>()))
        } else {
            Ok(text)
        }
    }
}
