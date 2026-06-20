use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use cog::message::Message;
use cog::provider::{collect_stream, ChatRequest, OpenAiCompatible, Provider, ProviderQuirks};

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message::user("list files")],
        tools: None,
        tool_choice: None,
        stream: false,
        temperature: None,
        max_tokens: None,
    }
}

#[tokio::test]
async fn chat_parses_tool_call_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "call_abc", "type": "function", "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .mount(&server)
        .await;

    let provider = OpenAiCompatible::new("test", server.uri(), Some("key".into()), ProviderQuirks::default());
    let resp = provider.chat(&request("test-model")).await.expect("chat should succeed");

    assert!(resp.message.content.is_none());
    assert_eq!(resp.message.tool_calls.len(), 1);
    assert_eq!(resp.message.tool_calls[0].id, "call_abc");
    assert_eq!(resp.message.tool_calls[0].name, "list_dir");
    assert_eq!(resp.message.tool_calls[0].arguments, "{\"path\":\".\"}");
    assert_eq!(resp.usage.unwrap().total_tokens, 15);
}

#[tokio::test]
async fn chat_stream_accumulates_tool_call_deltas_with_missing_index() {
    let server = MockServer::start().await;

    // Simulates a provider (like Groq) that omits `index` on later chunks of
    // the same tool call, and splits id/name into the first chunk only.
    let chunk1 = json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "id": "call_1", "function": {"name": "read_file", "arguments": ""}}
    ]}}]}).to_string();
    let chunk2 = json!({"choices": [{"delta": {"tool_calls": [
        {"function": {"arguments": "{\"path\":\"a.txt\"}"}}
    ]}}]}).to_string();
    let chunk3 = json!({
        "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }).to_string();
    let sse_body = format!("data: {chunk1}\n\ndata: {chunk2}\n\ndata: {chunk3}\n\ndata: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let quirks = ProviderQuirks { streaming_omits_index: true, ..Default::default() };
    let provider = OpenAiCompatible::new("test", server.uri(), None, quirks);
    let mut req = request("test-model");
    req.stream = true;

    let resp = collect_stream(&provider, &req).await.expect("stream should collect");

    assert_eq!(resp.message.tool_calls.len(), 1);
    assert_eq!(resp.message.tool_calls[0].id, "call_1");
    assert_eq!(resp.message.tool_calls[0].name, "read_file");
    assert_eq!(resp.message.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
    assert_eq!(resp.usage.unwrap().total_tokens, 3);
}
