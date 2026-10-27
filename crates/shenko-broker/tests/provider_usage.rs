//! Scripted HTTP providers; no real model or account is involved.
use std::collections::BTreeMap;

use serde_json::{Value, json};
use shenko_broker::{AnthropicProvider, LocalModelProvider, InferenceMessage, InferencePayload,
    InferencePriority, InferenceRequest, ProviderBinding};
use shenko_types::ids::{ControllerEpoch, EntityId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn provider_response(body: Value, anthropic: bool) -> Value {
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
    let anthropic_provider = AnthropicProvider { api_key: "fixture".into(), base_url: base_url.clone(), model: "fixture".into() };
    let binding = if anthropic {
        ProviderBinding::Anthropic { base_url, model: "fixture".into(), api_version: "2026-10-21".into(),
            serializer_revision: 3, default_max_tokens: 8192 }
    } else { local.binding() };
    let (result_tx, _) = tokio::sync::oneshot::channel();
    let request = InferenceRequest { request_id: EntityId::new(), agent_id: EntityId::new(),
        conversation_id: EntityId::new(), provider_binding: binding, priority: InferencePriority::Subject,
        epoch: ControllerEpoch(1), payload: InferencePayload { system_prompt: String::new(),
            messages: vec![InferenceMessage::text("user", "fixture")], max_tokens: Some(10),
            tools: vec![], native_options: BTreeMap::new() }, result_tx };
    let response = if anthropic { anthropic_provider.complete(&request).await.unwrap() }
        else { local.complete(&request).await.unwrap() };
    server.await.unwrap();
    serde_json::to_value(response.usage).unwrap()
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
            (None, Some(json!(7)), Value::Null, json!(7)),
            (Some(json!(123)), None, json!(123), Value::Null),
            (Some(json!(0)), Some(json!(0)), json!(0), json!(0)),
            (Some(json!("invalid")), Some(json!(-5)), Value::Null, Value::Null),
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
