use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolError};

const MAX_DOC_CHARS: usize = 6000;

#[derive(Deserialize)]
struct FetchCrateDocsParams {
    name: String,
    version: Option<String>,
}

pub struct FetchCrateDocsTool;

#[async_trait]
impl Tool for FetchCrateDocsTool {
    fn name(&self) -> &str {
        "fetch_crate_docs"
    }

    fn description(&self) -> &str {
        "Fetch metadata and documentation for a Rust crate from crates.io and docs.rs. \
         Returns the crate description, latest version, repository link, and a rendered \
         overview from docs.rs (converted from HTML to plain text)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The crate name (e.g. 'serde', 'tokio')."},
                "version": {"type": "string", "description": "Optional specific version. Defaults to the latest."}
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let params: FetchCrateDocsParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("cog-coding-agent/0.1")
            .build()
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        // 1. Fetch crate metadata from crates.io JSON API
        let api_url = format!("https://crates.io/api/v1/crates/{}", params.name);
        let api_resp = client
            .get(&api_url)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("crates.io request failed: {e}")))?;

        if !api_resp.status().is_success() {
            return Err(ToolError::Execution(format!(
                "crates.io returned {}",
                api_resp.status()
            )));
        }

        let api_body: Value = api_resp
            .json()
            .await
            .map_err(|e| ToolError::Execution(format!("failed to parse crates.io response: {e}")))?;

        let krate = &api_body["crate"];
        let description = krate["description"].as_str().unwrap_or("(no description)");
        let max_version = krate["max_version"].as_str().unwrap_or("unknown");
        let repository = krate["repository"].as_str().unwrap_or("(none)");

        let version = params.version.as_deref().unwrap_or(max_version);

        let mut output = format!(
            "# {} v{}\n\n{}\n\nRepository: {}\nLatest version: {}\n",
            params.name, version, description, repository, max_version
        );

        // 2. Fetch docs.rs overview — note: docs.rs paths use underscores, not hyphens
        let name_underscored = params.name.replace('-', "_");
        let docs_url = format!(
            "https://docs.rs/{}/{}/{}/",
            params.name, version, name_underscored
        );

        match client.get(&docs_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                let text = html2text::from_read(body.as_bytes(), 100).unwrap_or(body);
                let truncated = if text.chars().count() > MAX_DOC_CHARS {
                    format!(
                        "{}... [truncated]",
                        text.chars().take(MAX_DOC_CHARS).collect::<String>()
                    )
                } else {
                    text
                };
                output.push_str(&format!("\n## Documentation (docs.rs)\n\n{truncated}"));
            }
            Ok(resp) => {
                output.push_str(&format!(
                    "\n(docs.rs returned {} — documentation may not be available for this version)",
                    resp.status()
                ));
            }
            Err(e) => {
                output.push_str(&format!("\n(could not fetch docs.rs: {e})"));
            }
        }

        Ok(output)
    }
}
