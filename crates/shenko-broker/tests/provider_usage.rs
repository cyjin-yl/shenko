//! Scripted HTTP providers; no real model or account is involved.
use std::collections::BTreeMap;

use serde_json::{Value, json};
use shenko_broker::{AnthropicProvider, LocalModelProvider, InferenceMessage, InferencePayload,
    InferencePriority, InferenceRequest, InferenceResponse};
use shenko_types::ids::{ControllerEpoch, EntityId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn provider_response(body: Value, anthropic: bool) -> Value {
    let response = provider_reply(body, anthropic).await;
    serde_json::to_value(response.usage).unwrap()
}

/// One scripted reply, kept whole so a test can read the answer the product
/// would see, not only the usage envelope.
async fn provider_reply(body: Value, anthropic: bool) -> InferenceResponse {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let size = stream.read(&mut chunk).await.unwrap();
            assert_ne!(size, 0);
            received.extend_from_slice(&chunk[..size]);
            if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&received[..end]).unwrap();
                let length: usize = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                }).unwrap();
                if received.len() >= end + 4 + length { break; }
            }
        }
        let body = serde_json::to_string(&body).unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let local = LocalModelProvider::new(base_url.clone(), "fixture".into());
    let anthropic_provider = AnthropicProvider::with_endpoint("fixture".into(), base_url, "fixture");
    let binding = if anthropic { anthropic_provider.binding() } else { local.binding() };
    let (result_tx, _) = tokio::sync::oneshot::channel();
    let request = InferenceRequest { request_id: EntityId::new(), agent_id: EntityId::new(),
        conversation_id: EntityId::new(), provider_binding: binding, priority: InferencePriority::Subject,
        epoch: ControllerEpoch(1), payload: InferencePayload { system_prompt: String::new(),
            messages: vec![InferenceMessage::text("user", "fixture")], max_tokens: Some(10),
            tools: vec![], native_options: BTreeMap::new() }, result_tx };
    let response = if anthropic { anthropic_provider.complete(&request).await.unwrap() }
        else { local.complete(&request).await.unwrap() };
    server.await.unwrap();
    response
}

#[tokio::test]
async fn a_reasoning_only_reply_is_kept_and_is_the_answer() {
    // A reasoning backend can end its turn with the visible content empty and
    // the whole reply in its native reasoning field. That reply is not empty:
    // the answer the product sees must be the text the model produced.
    let response = provider_reply(
        json!({
            "choices":[{"message":{"role":"assistant","content":null,
                "reasoning":"此刻没有待办，我在等待下一个信号。"},"finish_reason":"stop"}]
        }),
        false,
    )
    .await;
    assert_eq!(response.content, "");
    assert_eq!(
        response.reasoning.as_deref(),
        Some("此刻没有待办，我在等待下一个信号。")
    );
    assert_eq!(response.visible_answer(), "此刻没有待办，我在等待下一个信号。");
}

#[tokio::test]
async fn a_visible_answer_wins_over_the_reasoning() {
    let response = provider_reply(
        json!({
            "choices":[{"message":{"role":"assistant","content":"答案",
                "reasoning":"先想一遍。"},"finish_reason":"stop"}]
        }),
        false,
    )
    .await;
    assert_eq!(response.visible_answer(), "答案");
    assert_eq!(response.reasoning.as_deref(), Some("先想一遍。"));
}

#[tokio::test]
async fn partial_invalid_and_explicit_zero_usage_remain_distinct() {
    for anthropic in [false, true] {
        let (input, output) = if anthropic { ("input_tokens", "output_tokens") }
            else { ("prompt_tokens", "completion_tokens") };
        let envelope = if anthropic {
            json!({"content":[{"type":"text","text":"fixture"}],"stop_reason":"end_turn"})
        } else {
            json!({"choices":[{"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}]})
        };
        for (raw_input, raw_output, expected_input, expected_output) in [
            (Some(json!("invalid")), None, Value::Null, Value::Null),
            (Some(json!(0)), Some(json!(0)), json!(0), json!(0)),
        ] {
            let mut response = envelope.clone();
            let mut usage = serde_json::Map::new();
            if let Some(value) = raw_input { usage.insert(input.into(), value); }
            if let Some(value) = raw_output { usage.insert(output.into(), value); }
            response["usage"] = Value::Object(usage);
            let observed = provider_response(response, anthropic).await;
            assert_eq!(observed["input_tokens"], expected_input, "input usage, anthropic={anthropic}");
            assert_eq!(observed["output_tokens"], expected_output, "output usage, anthropic={anthropic}");
        }
    }
}

#[tokio::test]
async fn local_prefix_cache_hits_pass_through_or_stay_unknown() {
    // Current OpenAI-compatible shape: prompt_tokens_details.cached_tokens.
    let observed = provider_response(
        json!({
            "choices":[{"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":120,"completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":96}}
        }),
        false,
    )
    .await;
    assert_eq!(observed["cache_read_tokens"], json!(96));
    assert_eq!(observed["input_tokens"], json!(120));

    // Older vLLM-family flat field also maps, without double counting.
    let observed = provider_response(
        json!({
            "choices":[{"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":120,"completion_tokens":5,
                "prompt_cache_hit_tokens":64}
        }),
        false,
    )
    .await;
    assert_eq!(observed["cache_read_tokens"], json!(64));

    // A backend that reports no cache evidence stays unknown — never 0 and
    // never a residency guarantee.
    let observed = provider_response(
        json!({
            "choices":[{"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":120,"completion_tokens":5}
        }),
        false,
    )
    .await;
    assert_eq!(observed["cache_read_tokens"], Value::Null);
}
