use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::tui::AgentToUi;

use super::{Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct AskUserParams {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the human user a clarifying question and wait for their answer."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question to ask the user."},
                "options": {"type": "array", "items": {"type": "string"}, "description": "Optional list of suggested answers."}
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let params: AskUserParams = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if let Some(ui_tx) = &ctx.ui_tx {
            let (tx, rx) = oneshot::channel();
            ui_tx
                .send(AgentToUi::AskUser { question: params.question, options: params.options, respond_to: tx })
                .map_err(|_| ToolError::Execution("UI channel closed".into()))?;
            return rx.await.map_err(|_| ToolError::Execution("no answer received".into()));
        }

        println!("{}", params.question);
        if !params.options.is_empty() {
            println!("options: {}", params.options.join(", "));
        }
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }
}
