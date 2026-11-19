//! Mechanism tests for the real OXR HTTP client and provider.
//!
//! These exercise the client against a local mock speaking the upstream
//! SoMark/OXR response shape, and the boundary against in-process providers.
//! They prove the provider shape and the honest failure paths — they do NOT
//! prove real OXR recognition quality or establish current engine/model or
//! hardware availability. Those require separate deployment observations
//! (see docs/evidence/document-parse-provider-20261111.md).

use super::*;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn request(pages: &[u32]) -> ParseRequest {
    ParseRequest {
        schema_version: DOCUMENT_PARSE_REQUEST_SCHEMA_VERSION,
        source_digest: "blake3:deadbeef".into(),
        source_name: Some("doc.pdf".into()),
        pages: pages.iter().map(|i| PageRef { page_index: *i }).collect(),
        closure_id: "oxr/1.0".into(),
        cache_scope: Some("subject-a:scope-a".into()),
        deployment_scope: Some("scope-a".into()),
    }
}

fn bound_request(provider: &dyn DocumentProvider, pages: &[u32]) -> ParseRequest {
    let mut req = request(pages);
    req.closure_id = provider.closure_id().to_owned();
    req.deployment_scope = provider.scope().map(str::to_owned);
    req
}

// ----- a local mock speaking the upstream response shape ------------------

#[derive(Default)]
struct MockState {
    /// Recorded `(request-target, raw body)` pairs.
    requests: Vec<(String, Vec<u8>)>,
    response_delays: HashMap<String, std::time::Duration>,
    response_content_type: Option<String>,
    chunked: bool,
    response_bytes: usize,
    response_finished: bool,
}

/// Shared route table: request-target suffix → queued answers.
type Routes = std::sync::Arc<Mutex<HashMap<&'static str, VecDeque<(u16, String)>>>>;

/// One scripted endpoint: `routes` maps a request-target suffix to a queue
/// of (status, body) answers consumed in request order for that path; the
/// last queued answer repeats once the queue is down to one.
fn spawn_mock(
    routes: HashMap<&'static str, VecDeque<(u16, String)>>,
) -> (
    String,
    std::sync::Arc<Mutex<MockState>>,
    tokio::task::JoinHandle<()>,
) {
    let routes: Routes = std::sync::Arc::new(Mutex::new(routes));
    let state = std::sync::Arc::new(Mutex::new(MockState::default()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = TcpListener::from_std(listener).unwrap();
    let state_loop = std::sync::Arc::clone(&state);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let routes = std::sync::Arc::clone(&routes);
            let state = std::sync::Arc::clone(&state_loop);
            tokio::spawn(async move { serve(stream, routes, state).await });
        }
    });
    (format!("http://{addr}"), state, handle)
}

/// Read an HTTP/1.1 request (head + Content-Length bytes) and answer with the
/// next scripted response for its path, or 404. Any HTTP answer proves
/// reachability, which is all the probe needs.
async fn serve(
    stream: tokio::net::TcpStream,
    routes: Routes,
    state: std::sync::Arc<Mutex<MockState>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let Ok(1) = reader.read_exact(&mut byte).await else {
            return;
        };
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return;
        }
    }
    let head_text = String::from_utf8_lossy(&head).into_owned();
    let request_line = head_text.lines().next().unwrap_or_default().to_owned();
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_owned();
    let content_length: usize = head_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
        })
        .flatten()
        .unwrap_or(0);
    let mut body = vec![0u8; content_length.min(16 * 1024 * 1024)];
    let _ = reader.read_exact(&mut body).await;
    state.lock().unwrap().requests.push((target.clone(), body));

    let answer = {
        let mut routes = routes.lock().unwrap();
        routes
            .iter_mut()
            .find(|(path, _)| target.ends_with(*path))
            .and_then(|(_, answers)| {
                if answers.len() == 1 {
                    answers.front().cloned()
                } else {
                    answers.pop_front()
                }
            })
            .unwrap_or((404, "{}".to_owned()))
    };
    let reason = match answer.0 {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    let (delay, chunked, content_type) = {
        let state = state.lock().unwrap();
        (state.response_delays.get(&target).copied().unwrap_or_default(), state.chunked,
            state.response_content_type.clone())
    };
    let content_type = content_type.as_deref().unwrap_or("application/json");
    tokio::time::sleep(delay).await;
    if chunked {
        let header = format!("HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n", answer.0, reason);
        let mut connected = writer.write_all(header.as_bytes()).await.is_ok();
        for chunk in answer.1.as_bytes().chunks(64 * 1024) {
            if !connected { break; }
            connected = writer.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await.is_ok()
                && writer.write_all(chunk).await.is_ok() && writer.write_all(b"\r\n").await.is_ok();
            if connected { state.lock().unwrap().response_bytes += chunk.len(); }
            tokio::task::yield_now().await;
        }
        if connected { let _ = writer.write_all(b"0\r\n\r\n").await; }
        let _ = writer.shutdown().await;
        state.lock().unwrap().response_finished = true;
        return;
    }
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        answer.0,
        reason,
        answer.1.len(),
        answer.1
    );
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.shutdown().await;
}

#[tokio::test]
async fn accepted_unknown_is_shared_with_pages_and_whole_document_discovery() {
    let routes = HashMap::from([
        ("/parse/async", VecDeque::from([(200, json!({"code":0,"data":{"task_id":"original-task"}}).to_string())])),
        ("/async_check", VecDeque::from([(503, "{}".into())])),
    ]);
    let (base, state, server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Async);
    let req = bound_request(provider.as_ref(), &[0, 1]);
    let run = run_parse(&req, &provider, b"owned protocol fixture", ParseOptions::default()).await.unwrap();
    assert_eq!(run.state, ParseState::Running);
    assert_eq!(run.checkpoints.len(), 2);
    assert!(run.checkpoints.iter().all(|checkpoint| checkpoint.unknown_outcome.as_deref()
        .is_some_and(|reason| reason.contains("outcome unknown"))));
    let whole = bound_request(provider.as_ref(), &[]);
    let discovery = run_parse(&whole, &provider, b"owned protocol fixture", ParseOptions::default()).await.unwrap();
    assert_eq!(discovery.state, ParseState::Running);
    assert!(discovery.checkpoints.is_empty());
    let checkpoint = discovery.request_checkpoint.unwrap();
    assert_eq!(checkpoint.request_id, parse_request_id(&whole));
    assert!(checkpoint.reason.contains("outcome unknown"));
    let requests = &state.lock().unwrap().requests;
    assert_eq!(requests.iter().filter(|(path, _)| path.ends_with("/parse/async")).count(), 1);
    assert_eq!(requests.iter().filter(|(path, _)| path.ends_with("/async_check")).count(), 1);
    let original_check = requests.iter().find(|(path, _)| path.ends_with("/async_check")).unwrap();
    assert!(String::from_utf8_lossy(&original_check.1).contains("original-task"),
        "the original handle remains on the wire, outside diagnostic rendering");
    server.abort();
}

#[test]
fn absent_pages_are_not_an_explicit_empty_or_real_document() {
    let missing = json!({"code":0,"data":{"result":{"outputs":{"json":{}}}}});
    assert!(OxrDocument::from_response(serde_json::to_vec(&missing).unwrap()).is_err());
    let empty = json!({"code":0,"data":{"metadata":{"page_num":0},"result":{"outputs":{"json":{"pages":[]}}}}});
    let document = OxrDocument::from_response(serde_json::to_vec(&empty).unwrap()).unwrap();
    assert_eq!(document.page_count, Some(0));
    assert!(document.pages.is_empty());
    assert_eq!(OxrDocument::from_response(upstream_document_json().into_bytes()).unwrap().pages.len(), 2);
}

async fn assert_original_inventory_unknown(mut response: serde_json::Value) {
    response["data"]["task_id"] = json!("original-inventory");
    response["data"]["status"] = json!("success");
    let raw = response.to_string();
    let saved_error = OxrDocument::from_response(raw.as_bytes().to_vec()).unwrap_err();
    assert!(saved_error.outcome_is_unknown());
    let saved = saved_error.response_observation().unwrap();
    assert!(saved.capture_complete);
    assert_eq!(saved.body.as_deref(), Some(raw.as_bytes()));

    let (base, state, server) = spawn_mock(HashMap::from([
        ("/parse/async", VecDeque::from([(200, json!({"code":0,"data":{"task_id":"original-inventory"}}).to_string())])),
        ("/async_check", VecDeque::from([(200, raw)])),
    ]));
    let provider = provider_with(&base, OxrTransport::Async);
    let requested = bound_request(provider.as_ref(), &[0]);
    let run = run_parse(&requested, &provider, b"owned inventory fixture", ParseOptions::default()).await.unwrap();
    assert_eq!(run.state, ParseState::Running);
    assert_eq!(run.checkpoints.len(), 1);
    assert!(run.checkpoints[0].result.is_none());
    assert!(run.checkpoints[0].failure.is_none(), "an incomplete original observation is no absent-page proof");
    assert!(run.checkpoints[0].unknown_outcome.is_some());
    let whole = bound_request(provider.as_ref(), &[]);
    let run = run_parse(&whole, &provider, b"owned inventory fixture", ParseOptions::default()).await.unwrap();
    assert_eq!(run.state, ParseState::Running);
    assert!(run.checkpoints.is_empty(), "no page may publish from a rejected original inventory");
    assert!(run.request_checkpoint.is_some());
    let requests = state.lock().unwrap().requests.clone();
    assert_eq!(requests.iter().filter(|(path, _)| path.ends_with("/parse/async")).count(), 1);
    let checks: Vec<_> = requests.iter().filter(|(path, _)| path.ends_with("/async_check")).collect();
    assert_eq!(checks.len(), 1, "cached uncertainty never resets the original attempt");
    assert!(String::from_utf8_lossy(&checks[0].1).contains("original-inventory"));
    server.abort();
}

#[tokio::test]
async fn contradictory_zero_page_count_keeps_original_task_unknown() {
    assert_original_inventory_unknown(json!({"code":0,"data":{"metadata":{"page_num":0},
        "result":{"outputs":{"json":{"pages":[{"page_num":0,"blocks":[{"content":"not an empty document"}]}]}}}}})).await;
}

#[tokio::test]
async fn missing_original_page_is_not_a_terminal_absent_page() {
    assert_original_inventory_unknown(json!({"code":0,"data":{"metadata":{"page_num":2},
        "result":{"outputs":{"json":{"pages":[{"page_num":0,"blocks":[{"content":"only one original page arrived"}]}]}}}}})).await;
}

#[tokio::test]
async fn duplicate_original_page_identity_never_publishes() {
    assert_original_inventory_unknown(json!({"code":0,"data":{"metadata":{"page_num":2},
        "result":{"outputs":{"json":{"pages":[{"page_num":0},{"page_num":0}]}}}}})).await;
}

#[tokio::test]
async fn invalid_explicit_page_identity_never_becomes_array_position() {
    for invalid in [json!(-1), json!(4_294_967_296u64), json!(1), json!(0.5), serde_json::Value::Null] {
        assert_original_inventory_unknown(json!({"code":0,"data":{"metadata":{"page_num":1},
            "result":{"outputs":{"json":{"pages":[{"page_num":invalid}]}}}}})).await;
    }
    let positional = json!({"code":0,"data":{"metadata":{"page_num":2},
        "result":{"outputs":{"json":{"pages":[{"blocks":[{"content":"zero"}]},{"blocks":[{"content":"one"}]}]}}}}});
    let parsed = OxrDocument::from_response(serde_json::to_vec(&positional).unwrap()).unwrap();
    assert_eq!(parsed.pages.iter().map(|page| page.page_num).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(parsed.pages[1].blocks[0].text, "one");
}

#[tokio::test]
async fn invalid_nonnull_page_count_keeps_original_observation_unknown() {
    for invalid in [json!(-1), json!(4_294_967_296u64), json!(1.5)] {
        assert_original_inventory_unknown(json!({"code":0,"data":{"metadata":{"page_num":invalid},
            "result":{"outputs":{"json":{"pages":[{"page_num":0}]}}}}})).await;
    }
}

#[tokio::test]
async fn exact_deployment_scope_is_required_before_upload_and_cached_reuse() {
    for scope in [Some("scope-a".to_owned()), None, Some(String::new())] {
        let (base, state, server) = spawn_mock(HashMap::from([
            ("/parse/sync", VecDeque::from([(200, upstream_document_json())])),
        ]));
        let provider: std::sync::Arc<dyn DocumentProvider> = std::sync::Arc::new(OxrProvider::new(OxrProviderConfig {
            endpoint: Some(base), model_identity: Some("owned-scope-fixture".into()), scope: scope.clone(),
            credential: Some(CredentialHandle::new("owned-scope-fixture")), transport: OxrTransport::Sync,
            ..OxrProviderConfig::default()
        }));
        let req = bound_request(provider.as_ref(), &[]);
        let mut refused = req.clone();
        refused.deployment_scope = Some("other-scope".into());
        assert!(provider.page_count(b"owned scope fixture", &refused).await.is_err());
        assert!(provider.parse_page(b"owned scope fixture", PageRef { page_index:0 }, &refused).await.is_err());
        assert!(run_parse(&refused, &provider, b"owned scope fixture", ParseOptions::default()).await.is_err());
        assert!(state.lock().unwrap().requests.is_empty(), "scope refusal precedes even a probe");
        let run = run_parse(&req, &provider, b"owned scope fixture", ParseOptions::default()).await.unwrap();
        assert_eq!(run.checkpoints.len(), 2);
        let before = state.lock().unwrap().requests.len();
        refused.deployment_scope = if scope.is_some() { None } else { Some(String::new()) };
        assert!(provider.page_count(b"owned scope fixture", &refused).await.is_err());
        assert!(provider.parse_page(b"owned scope fixture", PageRef { page_index:0 }, &refused).await.is_err());
        assert!(run_parse(&refused, &provider, b"owned scope fixture", ParseOptions::default()).await.is_err());
        assert_eq!(state.lock().unwrap().requests.len(), before, "cached bytes grant no different scope");
        let mut other_partition = req.clone();
        other_partition.cache_scope = Some("subject-b:independent-cache".into());
        assert_eq!(provider.page_count(b"owned scope fixture", &other_partition).await.unwrap(), Some(2));
        assert_eq!(state.lock().unwrap().requests.iter().filter(|(path, _)| path.ends_with("/parse/sync")).count(), 2,
            "cache partition is separate from exact deployment permission");
        server.abort();
    }
}

#[tokio::test]
async fn conclusive_remote_failure_retains_body_without_becoming_unknown() {
    let raw = json!({"code":0,"data":{"task_id":"original-failed","status":"failed",
        "error":"private provider reason https://private.invalid/resource?key=owned-secret",
        "result":{"outputs":{"json":{"pages":[{"page_num":null},{"page_num":-1}]}}}}}).to_string();
    let refusal = json!({"code":1133,"message":"private refusal https://private.invalid/check?key=owned-secret"}).to_string();
    let (base, state, server) = spawn_mock(HashMap::from([
        ("/parse/async", VecDeque::from([
            (401, "{}".into()), (200, refusal.clone()),
            (200, json!({"code":0,"data":{"task_id":"original-failed"}}).to_string()),
        ])),
        ("/async_check", VecDeque::from([
            (401, "{}".into()), (403, "{}".into()), (404, "{}".into()),
            (200, refusal),
            // A terminal-looking check that does not positively name the
            // queried handle is malformed, never a settlement of it.
            (200, json!({"code":0,"data":{"status":"failed","error":"upstream failed"}}).to_string()),
            (200, json!({"code":0,"data":{"task_id":"unrelated-task","status":"failed"}}).to_string()),
            (200, json!({"code":0,"data":{"status":"success","result":{"outputs":{"json":{"pages":[]}}}}}).to_string()),
            (200, json!({"code":0,"data":{"task_id":"unrelated-task","status":"success","result":{"outputs":{"json":{"pages":[]}}}}}).to_string()),
            (200, raw.clone()),
        ])),
    ]));
    // The provider's private diagnostic token is not this connection's API
    // credential. Actual credential echoes are covered by the withholding
    // consumer; this case must retain a noncredential original body.
    let client = OxrClient::new(OxrClientConfig::new(base, CredentialHandle::new("owned-submit-credential"))).unwrap();
    let refused = client.submit_document("fresh-http-refusal.pdf", b"owned fresh refusal fixture").await.unwrap_err();
    assert!(!refused.outcome_is_unknown(), "a conclusive fresh-submit refusal remains terminal");
    let refused = client.submit_document("fresh-envelope-refusal.pdf", b"independent owned fresh refusal fixture").await.unwrap_err();
    assert!(!refused.outcome_is_unknown());
    let OxrSubmission::Accepted { task_id, response } = client.submit_document("accepted.pdf", b"owned accepted fixture").await.unwrap() else {
        panic!("the original task must actually be accepted before its checks");
    };
    assert!(response.capture_complete);
    assert_eq!(response.source, crate::OxrResponseSource::AsyncSubmit);
    assert_eq!(response.http_status, Some(200));
    let accepted: serde_json::Value = serde_json::from_slice(response.body.as_deref().unwrap()).unwrap();
    assert_eq!(accepted["data"]["task_id"], task_id);
    assert_eq!(task_id, "original-failed");
    for status in [401, 403, 404, 200, 200, 200, 200, 200] {
        let error = client.check_document(&task_id).await.unwrap_err();
        assert!(matches!(&error, OxrError::TaskUnknown { task_id: original, .. } if original == &task_id));
        assert!(error.outcome_is_unknown(), "a refused accepted-handle check cannot settle the computation");
        assert!(!error.is_terminal_task_failure());
        let observation = error.response_observation().unwrap();
        assert_eq!(observation.http_status, Some(status));
        assert!(observation.capture_complete);
        assert!(observation.body.is_some());
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("private.invalid"));
            assert!(!rendered.contains("owned-secret"));
            assert!(!rendered.contains("owned-submit-credential"));
        }
    }
    let error = client.check_document(&task_id).await.unwrap_err();
    assert!(!error.outcome_is_unknown(), "an explicit failed status is conclusive");
    assert!(error.is_terminal_task_failure());
    let observation = error.response_observation().unwrap();
    assert!(observation.capture_complete);
    assert_eq!(observation.source, OxrResponseSource::AsyncCheck);
    assert_eq!(observation.body.as_deref(), Some(raw.as_bytes()));
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains("private.invalid"));
        assert!(!rendered.contains("owned-secret"));
        assert!(!rendered.contains("owned-submit-credential"));
        assert!(!rendered.contains("private provider reason"));
    }
    let requests = state.lock().unwrap().requests.clone();
    assert_eq!(requests.iter().filter(|(target, _)| target.ends_with("/parse/async")).count(), 3);
    let checks: Vec<_> = requests.iter().filter(|(target, _)| target.ends_with("/async_check")).collect();
    assert_eq!(checks.len(), 9);
    assert!(checks.iter().all(|(_, body)| String::from_utf8_lossy(body).contains("original-failed")));
    server.abort();
}

#[tokio::test]
async fn chunked_response_is_rejected_before_the_entire_oversized_body_arrives() {
    let routes = HashMap::from([("/parse/sync", VecDeque::from([
        (200, upstream_document_json()), (200, " ".repeat(80 * 1024 * 1024)),
    ]))]);
    let (base, state, server) = spawn_mock(routes);
    state.lock().unwrap().chunked = true;
    let mut config = OxrClientConfig::new(base, CredentialHandle::new("owned-protocol-fixture"));
    config.transport = OxrTransport::Sync;
    let client = OxrClient::new(config).unwrap();
    assert_eq!(client.parse_document("small.pdf", b"small owned fixture").await.unwrap().pages.len(), 2);
    { let mut state = state.lock().unwrap(); state.response_bytes = 0; state.response_finished = false; }
    let error = client.parse_document("large.pdf", b"separate owned fixture").await.unwrap_err();
    assert!(error.outcome_is_unknown());
    assert!(error.to_string().contains("body exceeded the limit"));
    let observation = error.response_observation().unwrap();
    assert!(!observation.capture_complete);
    assert_eq!(observation.body.as_ref().unwrap().len(), 64 * 1024 * 1024);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !state.lock().unwrap().response_finished { tokio::task::yield_now().await; }
    }).await.unwrap();
    assert!(state.lock().unwrap().response_bytes < 80 * 1024 * 1024);
    server.abort();
}

#[tokio::test]
async fn poll_deadline_bounds_a_slow_check_without_claiming_remote_stop() {
    let routes = HashMap::from([("/async_check", VecDeque::from([(200, upstream_check_success_json()), (200, upstream_check_success_json())]))]);
    let (base, state, server) = spawn_mock(routes);
    state.lock().unwrap().response_delays.insert("/v1/parse/async_check".into(), std::time::Duration::from_secs(2));
    let mut config = OxrClientConfig::new(base, CredentialHandle::new("owned-protocol-fixture"));
    config.poll_timeout = std::time::Duration::from_millis(20);
    config.request_timeout = std::time::Duration::from_secs(5);
    let client = OxrClient::new(config).unwrap();
    for durable in [false, true] {
        let deadline = chrono::Utc::now() + chrono::Duration::milliseconds(20);
        let error = if durable {
            tokio::time::timeout(std::time::Duration::from_millis(500), client.poll_document_with_observer("t1", deadline, |_| Ok(())))
                .await.expect("the original deadline must also bound the durable observer").expect("no ledger failure").unwrap_err()
        } else {
            tokio::time::timeout(std::time::Duration::from_millis(500), client.poll_document("t1", deadline))
                .await.expect("the absolute poll deadline must bound a slow HTTP check").unwrap_err()
        };
        assert!(matches!(error, OxrError::TaskUnknown { ref task_id, .. } if task_id == "t1"));
        assert!(error.outcome_is_unknown());
    }
    assert_eq!(state.lock().unwrap().requests.len(), 2);
    assert!(state.lock().unwrap().requests.iter().all(|request| request.0.ends_with("/async_check")));
    server.abort();
}

/// Two upstream-shaped pages covering several element types and formats
/// verbatim, including OXR-native spellings ("Picture", "Caption") and blocks
/// with no bbox (unknown, never fabricated).
fn upstream_document_json() -> String {
    json!({
        "code": 0,
        "message": "任务成功",
        "warnings": [],
        "data": {
            "task_id": "t1",
            "error": null,
            "metadata": {"page_num": 2, "file_type": ".pdf"},
            "result": {
                "file_name": "doc.pdf",
                "catalog": [],
                "outputs": {
                    "json": {
                        "pages": [
                            {
                                "page_num": 0,
                                "blocks": [
                                    {"idx": 0, "type": "title", "bbox": [72, 50, 540, 80],
                                     "content": "第一章 引言", "format": "text", "captions": [],
                                     "img_url": "", "title_level": 1},
                                    {"idx": 1, "type": "Text", "bbox": [72, 100, 540, 200],
                                     "content": "本文档介绍了…", "format": "text", "captions": []},
                                    {"idx": 2, "type": "Formula",
                                     "content": "$$E=mc^2$$", "format": "latex", "captions": []},
                                    {"idx": 3, "type": "Table", "bbox": [10, 20, 500, 300],
                                     "content": "<table><tr><td>1</td></tr></table>",
                                     "format": "html", "captions": []},
                                    {"idx": 4, "type": "Code Block", "bbox": [10, 310, 500, 400],
                                     "content": "fn main() {}", "format": "markdown", "captions": []}
                                ],
                                "page_size": {"h": 1684, "w": 1190},
                                "merge_content_from_pre_page": false
                            },
                            {
                                "page_num": 1,
                                "blocks": [
                                    {"idx": 0, "type": "Picture", "bbox": [10, 20, 300, 400],
                                     "content": "", "format": "image", "captions": [1],
                                     "img_url": "https://example.invalid/img/1.png"},
                                    {"idx": 1, "type": "Caption", "bbox": [10, 410, 300, 430],
                                     "content": "图 1 示例", "format": "text", "captions": []},
                                    {"idx": 2, "type": "Chemical Structure", "bbox": [40, 440, 200, 520],
                                     "content": "CCO", "format": "image", "captions": []}
                                ],
                                "page_size": {"h": 1684, "w": 1190}
                            }
                        ]
                    }
                }
            }
        }
    })
    .to_string()
}

/// The `/parse/async_check` success answer: the whole parse under
/// `data.result`, plus `data.status` = "success" (and `record_id`).
fn upstream_check_success_json() -> String {
    let mut value: serde_json::Value = serde_json::from_str(&upstream_document_json()).unwrap();
    value["data"]["status"] = json!("success");
    value["data"]["record_id"] = json!(12345);
    value.to_string()
}

fn provider_with(endpoint: &str, transport: OxrTransport) -> std::sync::Arc<dyn DocumentProvider> {
    std::sync::Arc::new(OxrProvider::new(OxrProviderConfig {
        endpoint: Some(endpoint.to_owned()),
        model_identity: Some("OXR-1.0".into()),
        scope: Some("scope-a".into()),
        credential: Some(CredentialHandle::new("sk-test-secret")),
        transport,
        poll_interval: Some(std::time::Duration::from_millis(1)),
        ..OxrProviderConfig::default()
    }))
}

#[test]
fn checkpoint_from_old_payload_still_deserializes() {
    // A checkpoint serialized before element_type/content_format/page_size/
    // unknown_outcome/image_resource existed must still deserialize. This is
    // a historical observation, not new mapper evidence or a re-keyed result.
    let payload = r#"{
        "cache_key": "doc-parse:abc",
        "result": {
            "page": {"page_index": 0},
            "source_digest": "blake3:deadbeef",
            "blocks": [{"order": 0, "text": "hi", "bbox": [1.0, 2.0, 3.0, 4.0], "structure_ref": null}],
            "provenance": {"provider_id": "oxr", "code_version": "oxr/1.0", "model_version": null, "params_digest": null, "raw_output_ref": null},
            "coordinates_restored": true
        },
        "failure": null,
        "committed_at": "2026-11-11T00:00:00Z"
    }"#;
    let checkpoint: PageCheckpoint = serde_json::from_str(payload).unwrap();
    let result = checkpoint.result.unwrap();
    assert!(result.page_size.is_none());
    assert!(checkpoint.unknown_outcome.is_none());
    let block = &result.blocks[0];
    assert!(block.element_type.is_none());
    assert!(block.content_format.is_none());
    assert!(block.image_resource.is_none());
    assert!(result.coordinates_restored, "preserve the historical claim verbatim");
    assert_eq!(block.bbox, Some([1.0, 2.0, 3.0, 4.0]));
}

#[tokio::test]
async fn sync_transport_maps_upstream_page_exactly() {
    let mut routes = HashMap::new();
    routes.insert(
        "/parse/sync",
        VecDeque::from(vec![(200u16, upstream_document_json())]),
    );
    let (base, state, _server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Sync);

    assert_eq!(provider.availability().await, ProviderAvailability::Ready);

    let outcome = provider
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 0 }, &bound_request(provider.as_ref(), &[0]))
        .await
        .unwrap();
    let PageOutcome::Parsed(result) = outcome else {
        panic!("a real upstream-shaped answer must produce a page")
    };
    assert_eq!(result.page.page_index, 0);
    assert!(!result.coordinates_restored, "provider dimensions do not prove a source transform");
    assert_eq!(
        result.page_size,
        Some(PageSize {
            width: 1190.0,
            height: 1684.0
        })
    );
    assert_eq!(result.provenance.provider_id, "oxr");
    assert_eq!(result.provenance.model_version.as_deref(), Some("OXR-1.0"));
    assert!(result.provenance.params_digest.is_some());
    assert_eq!(
        result.provenance.raw_output_ref, None,
        "no raw-output CAS is claimed"
    );

    let blocks = &result.blocks;
    assert_eq!(blocks.len(), 5);
    // Element types carried verbatim, including OXR-native spellings.
    assert_eq!(blocks[0].element_type.as_deref(), Some("title"));
    assert_eq!(blocks[0].content_format.as_deref(), Some("text"));
    assert_eq!(blocks[0].bbox, Some([72.0, 50.0, 540.0, 80.0]));
    assert_eq!(blocks[0].order, 0);
    assert!(blocks[0].image_resource.is_none(), "an empty img_url is no retained resource");
    assert_eq!(blocks[1].element_type.as_deref(), Some("Text"));
    assert_eq!(blocks[2].element_type.as_deref(), Some("Formula"));
    assert_eq!(blocks[2].content_format.as_deref(), Some("latex"));
    // The Formula block has no bbox upstream: unknown, never fabricated.
    assert_eq!(blocks[2].bbox, None);
    assert_eq!(blocks[3].content_format.as_deref(), Some("html"));
    assert_eq!(blocks[4].element_type.as_deref(), Some("Code Block"));
    assert_eq!(blocks[4].content_format.as_deref(), Some("markdown"));

    // Second page: a caption link becomes the structure reference.
    let outcome = provider
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 1 }, &bound_request(provider.as_ref(), &[1]))
        .await
        .unwrap();
    let PageOutcome::Parsed(result) = outcome else {
        panic!("a real upstream-shaped answer must produce a page")
    };
    assert_eq!(
        result.blocks[0].structure_ref.as_deref(),
        Some("oxr:captions=1")
    );
    assert_eq!(result.blocks[0].content_format.as_deref(), Some("image"));
    assert_eq!(result.blocks[0].text, "");
    assert_eq!(result.blocks[0].image_resource, Some(PageImageResource::TransientUnretained {
        provider_url: "https://example.invalid/img/1.png".into(),
    }));
    assert!(result.blocks[2].image_resource.is_none(), "image format does not invent a resource");
    let saved = serde_json::to_vec(&*result).unwrap();
    let restored: PageResult = serde_json::from_slice(&saved).unwrap();
    assert_eq!(restored.blocks[0].image_resource, result.blocks[0].image_resource);
    let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(saved["blocks"][0]["image_resource"]["retention"], "transient_unretained");
    assert!(saved["blocks"][0]["image_resource"].get("cas_ref").is_none());
    assert_eq!(
        result.blocks[2].element_type.as_deref(),
        Some("Chemical Structure")
    );

    // A page the document does not contain is a terminal failure, not a
    // fabricated block.
    let outcome = provider
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 5 }, &bound_request(provider.as_ref(), &[5]))
        .await
        .unwrap();
    let PageOutcome::PageFailed(reason) = outcome else {
        panic!("an absent page must fail, not fabricate")
    };
    assert!(reason.contains("page 5"), "{reason}");

    // Exactly one upload served all three page answers: the shared document
    // parse, never a re-upload per page.
    let requests = state.lock().unwrap().requests.clone();
    let uploads = requests
        .iter()
        .filter(|(target, _)| target.ends_with("/parse/sync"))
        .count();
    assert_eq!(
        uploads,
        1,
        "{:?}",
        requests
            .iter()
            .map(|(target, _)| target)
            .collect::<Vec<_>>()
    );
    // The upload carried the api_key and file form fields.
    let upload = requests
        .iter()
        .find(|(target, _)| target.ends_with("/parse/sync"))
        .unwrap();
    let body = String::from_utf8_lossy(&upload.1);
    assert!(
        body.contains("name=\"api_key\""),
        "the api_key form field must be sent"
    );
    assert!(body.contains("sk-test-secret"));
    assert!(body.contains("name=\"file\""));
    // Exactly one other request: the credential-free reachability probe.
    let probes = requests
        .iter()
        .filter(|(target, _)| !target.ends_with("/parse/sync"))
        .count();
    assert_eq!(probes, 1);
}

#[tokio::test]
async fn async_transport_polls_by_task_id_until_success() {
    let mut routes = HashMap::new();
    routes.insert(
        "/parse/async",
        VecDeque::from(vec![(
            200u16,
            json!({"code": 0, "message": "任务已提交", "data": {"task_id": "t1", "status": "queuing"}})
                .to_string(),
        )]),
    );
    routes.insert(
        "/async_check",
        VecDeque::from(vec![
            (
                200u16,
                json!({"code": 0, "message": "查询成功", "data": {"task_id": "t1", "status": "processing"}})
                    .to_string(),
            ),
            (200u16, upstream_check_success_json()),
        ]),
    );
    let (base, state, _server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Async);

    let outcome = provider
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 1 }, &bound_request(provider.as_ref(), &[1]))
        .await
        .unwrap();
    let PageOutcome::Parsed(result) = outcome else {
        panic!("the async task must resolve to the parsed page")
    };
    assert_eq!(result.page.page_index, 1);
    assert_eq!(result.blocks[1].element_type.as_deref(), Some("Caption"));

    let targets: Vec<String> = state
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|(target, _)| target.clone())
        .collect();
    assert_eq!(
        targets,
        vec![
            "/v1/parse/async".to_owned(),
            "/v1/parse/async_check".to_owned(),
            "/v1/parse/async_check".to_owned(),
        ],
        "{targets:?}"
    );
    // The polls carried the task_id, not a new upload.
    let requests = state.lock().unwrap().requests.clone();
    let checks = requests
        .iter()
        .filter(|(target, _)| target.ends_with("async_check"))
        .collect::<Vec<_>>();
    for (_, body) in checks {
        let body = String::from_utf8_lossy(body);
        assert!(body.contains("name=\"task_id\""), "{body}");
        assert!(body.contains("t1"), "{body}");
    }
}

#[tokio::test]
async fn run_parse_produces_page_ordered_checkpoints() {
    // The retained primary schema has an optional integer count. The client
    // also treats null as unreported; both forms still validate the full array.
    for null_count in [false, true] {
    let mut response: serde_json::Value = serde_json::from_str(&upstream_document_json()).unwrap();
    if null_count {
        response["data"]["metadata"]["page_num"] = serde_json::Value::Null;
    } else {
        response["data"]["metadata"].as_object_mut().unwrap().remove("page_num");
    }
    let mut routes = HashMap::new();
    routes.insert(
        "/parse/sync",
        VecDeque::from(vec![(200u16, response.to_string())]),
    );
    let (base, state, server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Sync);

    // Whole-document request: the page count is provider-discovered, never
    // guessed, and each discovered page gets its own checkpoint.
    let whole = bound_request(provider.as_ref(), &[]);
    let run = run_parse(&whole, &provider, b"%PDF-1.7 fake", ParseOptions::default())
        .await
        .unwrap();
    assert_eq!(run.state, ParseState::Complete);
    assert_eq!(run.checkpoints.len(), 2);
    assert_eq!(
        run.checkpoints[0].result.as_ref().unwrap().page.page_index,
        0
    );
    assert_eq!(
        run.checkpoints[1].result.as_ref().unwrap().page.page_index,
        1
    );
    for checkpoint in &run.checkpoints {
        assert!(!checkpoint.cache_key.is_empty());
        assert!(checkpoint.failure.is_none());
        assert!(checkpoint.unknown_outcome.is_none());
    }
    let index = index_checkpoints(&run);
    assert_eq!(index.len(), 2);

    // The whole-document run still uploaded exactly once (the probe is the
    // only other request).
    let requests = state.lock().unwrap().requests.clone();
    let uploads = requests
        .iter()
        .filter(|(target, _)| target.ends_with("/parse/sync"))
        .count();
    assert_eq!(uploads, 1);
    server.abort();
    }
}

#[tokio::test]
async fn rate_limited_upstream_is_an_unknown_outcome_not_a_failure() {
    let mut routes = HashMap::new();
    routes.insert(
        "/parse/sync",
        VecDeque::from(vec![(
            200u16,
            json!({"code": 1124, "message": "QPS limit exceeded", "data": null}).to_string(),
        )]),
    );
    let (base, _state, _server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Sync);

    let run = run_parse(
        &bound_request(provider.as_ref(), &[0]),
        &provider,
        b"%PDF-1.7 fake",
        ParseOptions::default(),
    )
    .await
    .unwrap();
    // A rate limit leaves the outcome unknown: the checkpoint records it,
    // never a terminal failure and never a fabricated result, and the run
    // stays Running (not Complete).
    assert_eq!(run.state, ParseState::Running);
    assert_eq!(run.checkpoints.len(), 1);
    assert!(run.checkpoints[0].result.is_none());
    assert!(run.checkpoints[0].failure.is_none());
    assert!(
        run.checkpoints[0]
            .unknown_outcome
            .as_deref()
            .is_some_and(|reason| reason.contains("1124"))
    );
}

#[tokio::test]
async fn unreachable_endpoint_is_unavailable_naming_the_reason() {
    // Nothing listens on this port: an honest Unavailable, never a parse.
    let endpoint = "http://127.0.0.1:1/private-tenant-r70/v1?capability=private-query-r70";
    let bound = std::sync::Arc::new(OxrProvider::new(OxrProviderConfig {
        endpoint: Some(endpoint.into()), model_identity: Some("owned-availability-fixture".into()),
        scope: Some("scope-a".into()), credential: Some(CredentialHandle::new("owned-availability-fixture")),
        transport: OxrTransport::Sync, ..OxrProviderConfig::default()
    }));
    assert_eq!(bound.binding().endpoint.as_deref(), Some(endpoint), "structured deployment binding remains exact");
    let provider: std::sync::Arc<dyn DocumentProvider> = bound;
    let reason = match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("unreachable"), "{reason}");
            reason
        }
        ProviderAvailability::Ready => panic!("an unreachable endpoint is never ready"),
    };
    let run = run_parse(&bound_request(provider.as_ref(), &[]), &provider, b"owned availability fixture", ParseOptions::default()).await.unwrap();
    assert_eq!(run.state, ParseState::ProviderUnavailable);
    for reason in [reason, run.request_checkpoint.unwrap().reason] {
        assert!(!reason.contains(endpoint));
        assert!(!reason.contains("private-tenant-r70"));
        assert!(!reason.contains("private-query-r70"));
        assert!(!reason.contains("capability="));
    }
}

#[tokio::test]
async fn local_engine_without_attestation_is_unavailable() {
    let provider = OxrProvider::new(OxrProviderConfig {
        endpoint: Some("http://127.0.0.1:8000/v1".into()),
        model_identity: Some("OXR-1.0".into()),
        credential: Some(CredentialHandle::new("dummy")),
        engine: OxrEngineBinding::Local { attestation: None },
        ..OxrProviderConfig::default()
    });
    match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("attestation"), "{reason}");
            assert!(reason.contains("BF16"), "{reason}");
        }
        ProviderAvailability::Ready => panic!("an unattested local engine is never ready"),
    }
}

#[tokio::test]
async fn invalid_endpoint_shape_is_unavailable() {
    let provider = OxrProvider::new(OxrProviderConfig {
        endpoint: Some("not a url".into()),
        model_identity: Some("OXR-1.0".into()),
        credential: Some(CredentialHandle::new("dummy")),
        ..OxrProviderConfig::default()
    });
    match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("endpoint"), "{reason}");
        }
        ProviderAvailability::Ready => panic!("an invalid endpoint is never ready"),
    }
}

#[test]
fn credential_is_redacted_in_every_rendering() {
    let credential = CredentialHandle::new("sk-super-secret");
    assert!(!format!("{credential:?}").contains("sk-super-secret"));
    assert!(!format!("{credential}").contains("sk-super-secret"));
    // An endpoint embedding a secret is refused at binding, so it can never
    // leak into an error that quotes the URL.
    let provider = OxrProvider::new(OxrProviderConfig {
        endpoint: Some("http://user:sk-super-secret@127.0.0.1:8000/v1".into()),
        model_identity: Some("OXR-1.0".into()),
        credential: Some(credential),
        ..OxrProviderConfig::default()
    });
    let reason = futures::executor::block_on(async {
        match provider.availability().await {
            ProviderAvailability::Unavailable { reason } => reason,
            ProviderAvailability::Ready => String::new(),
        }
    });
    assert!(!reason.contains("sk-super-secret"), "{reason}");
}

/// Credential echoes are explicit evidence gaps across both a conclusive
/// refusal and otherwise valid success. Neither body nor typed output escapes.
#[test]
fn credential_echo_is_withheld_from_original_response_archive() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let secret = "fixture\"\ncredential";
        let mut leaked_document: serde_json::Value = serde_json::from_str(&upstream_document_json())?;
        leaked_document["warnings"] = json!([secret]);
        leaked_document["data"]["result"]["file_name"] = json!(secret);
        leaked_document["data"]["result"]["outputs"]["json"]["pages"][0]["blocks"][0]["content"] = json!(secret);
        let mut routes = HashMap::new();
        routes.insert("/parse/sync", VecDeque::from(vec![
            (403, json!({"code":1104,"message":format!("rejected {secret}"),"unmapped":secret}).to_string()),
            (200, leaked_document.to_string()),
        ]));
        let (base, state, _server) = spawn_mock(routes);
        let mut config = OxrClientConfig::new(base, CredentialHandle::new(secret));
        config.transport = OxrTransport::Sync;
        let client = OxrClient::new(config)?;
        for (status, unknown) in [(403, false), (200, true)] {
            let error = client.submit_document("fixture.pdf", b"%PDF fixture").await
                .expect_err("credential-bearing response cannot become typed success");
            assert_eq!(error.outcome_is_unknown(), unknown);
            assert!(!error.to_string().contains(secret));
            assert!(!format!("{error:?}").contains(secret));
            assert!(error.to_string().contains("credential echo response bytes withheld"));
            let response = error.response_observation().ok_or_else(|| anyhow::anyhow!("response metadata missing"))?;
            assert_eq!(response.http_status, Some(status));
            assert!(response.body.is_none());
            assert!(!response.capture_complete);
        }
        assert_eq!(state.lock().map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?.requests.len(), 2);
        Ok(())
    })
}

#[tokio::test]
async fn credential_in_content_type_is_withheld_with_explicit_evidence_loss() {
    let credential = "owned-header-fixture-value";
    let refusal = json!({"code":1104,"message":"request denied"}).to_string();
    let document = upstream_document_json();
    let (base, state, server) = spawn_mock(HashMap::from([("/parse/sync", VecDeque::from([
        (403, refusal.clone()),
        (200, document.clone()),
    ]))]));
    state.lock().unwrap().response_content_type = Some(format!("application/json; diagnostic={credential}"));
    let mut config = OxrClientConfig::new(base, CredentialHandle::new(credential));
    config.transport = OxrTransport::Sync;
    let client = OxrClient::new(config).unwrap();
    for (status, unknown, expected_body) in [(403, false, refusal.as_bytes()),
        (200, true, document.as_bytes())] {
        let error = client.submit_document("fixture.pdf", b"%PDF fixture").await.unwrap_err();
        assert_eq!(error.outcome_is_unknown(), unknown);
        assert!(error.to_string().contains("credential echo response bytes withheld"));
        assert!(!format!("{error:?}").contains(credential));
        let observation = error.response_observation().unwrap();
        assert_eq!(observation.http_status, Some(status));
        assert_eq!(observation.declared_content_type, None);
        assert_eq!(observation.body.as_deref(), Some(expected_body));
        assert!(observation.capture_complete);
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

// ----- bounded concurrency ------------------------------------------------

struct CountingProvider {
    in_flight: AtomicUsize,
    max_seen: AtomicUsize,
}

#[async_trait::async_trait]
impl DocumentProvider for CountingProvider {
    fn provider_id(&self) -> &str {
        "counting"
    }
    fn closure_id(&self) -> &str {
        "counting/1.0"
    }
    fn scope(&self) -> Option<&str> { Some("scope-a") }
    async fn availability(&self) -> ProviderAvailability {
        ProviderAvailability::Ready
    }
    async fn parse_page(
        &self,
        _source_bytes: &[u8],
        page: PageRef,
        req: &ParseRequest,
    ) -> anyhow::Result<PageOutcome> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(in_flight, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(PageOutcome::Parsed(Box::new(PageResult {
            page,
            source_digest: req.source_digest.clone(),
            blocks: vec![],
            provenance: PageProvenance {
                provider_id: "counting".into(),
                code_version: "counting/1.0".into(),
                model_version: None,
                params_digest: None,
                raw_output_ref: None,
            },
            coordinates_restored: false,
            page_size: None,
        })))
    }
}

#[tokio::test]
async fn run_parse_respects_the_configured_concurrency_bound() {
    let counting = std::sync::Arc::new(CountingProvider {
        in_flight: AtomicUsize::new(0),
        max_seen: AtomicUsize::new(0),
    });
    let provider: std::sync::Arc<dyn DocumentProvider> = counting.clone();
    let run = run_parse(
        &bound_request(provider.as_ref(), &(0..8).collect::<Vec<_>>()),
        &provider,
        b"%PDF-1.7 fake",
        ParseOptions {
            max_concurrency: Some(2),
        },
    )
    .await
    .unwrap();
    assert_eq!(run.state, ParseState::Complete);
    assert_eq!(run.checkpoints.len(), 8);
    // The observed in-flight maximum never exceeded the bound.
    assert!(counting.max_seen.load(Ordering::SeqCst) <= 2);
    // And the bound was actually exercised (not a serial run).
    assert!(counting.max_seen.load(Ordering::SeqCst) == 2);
}

// ----- outcome classification ---------------------------------------------

#[tokio::test]
async fn terminal_failure_is_a_terminal_checkpoint() {
    struct Failing;
    #[async_trait::async_trait]
    impl DocumentProvider for Failing {
        fn provider_id(&self) -> &str {
            "failing"
        }
        fn closure_id(&self) -> &str {
            "failing/1.0"
        }
        fn scope(&self) -> Option<&str> { Some("scope-a") }
        async fn availability(&self) -> ProviderAvailability {
            ProviderAvailability::Ready
        }
        async fn parse_page(
            &self,
            _source_bytes: &[u8],
            _page: PageRef,
            _req: &ParseRequest,
        ) -> anyhow::Result<PageOutcome> {
            Ok(PageOutcome::PageFailed("unsupported file type".into()))
        }
    }
    let provider: std::sync::Arc<dyn DocumentProvider> = std::sync::Arc::new(Failing);
    let run = run_parse(&bound_request(provider.as_ref(), &[0]), &provider, b"x", ParseOptions::default())
        .await
        .unwrap();
    assert_eq!(
        run.state,
        ParseState::Complete,
        "a terminal failure is terminal"
    );
    assert_eq!(
        run.checkpoints[0].failure.as_deref(),
        Some("unsupported file type")
    );
}

#[tokio::test]
async fn unknown_outcome_keeps_the_run_running() {
    struct Unknown;
    #[async_trait::async_trait]
    impl DocumentProvider for Unknown {
        fn provider_id(&self) -> &str {
            "unknown"
        }
        fn closure_id(&self) -> &str {
            "unknown/1.0"
        }
        fn scope(&self) -> Option<&str> { Some("scope-a") }
        async fn availability(&self) -> ProviderAvailability {
            ProviderAvailability::Ready
        }
        async fn parse_page(
            &self,
            _source_bytes: &[u8],
            _page: PageRef,
            _req: &ParseRequest,
        ) -> anyhow::Result<PageOutcome> {
            Ok(PageOutcome::Unknown("upstream reply lost".into()))
        }
    }
    let provider: std::sync::Arc<dyn DocumentProvider> = std::sync::Arc::new(Unknown);
    let run = run_parse(&bound_request(provider.as_ref(), &[0]), &provider, b"x", ParseOptions::default())
        .await
        .unwrap();
    assert_eq!(run.state, ParseState::Running);
    assert_eq!(
        run.checkpoints[0].unknown_outcome.as_deref(),
        Some("upstream reply lost")
    );
}
