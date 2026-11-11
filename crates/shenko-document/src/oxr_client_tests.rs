//! Mechanism tests for the real OXR HTTP client and provider.
//!
//! These exercise the client against a local mock speaking the upstream
//! SoMark/OXR response shape, and the boundary against in-process providers.
//! They prove the provider shape and the honest failure paths — they do NOT
//! prove real OXR recognition quality: this host's V100 GPUs have no BF16
//! tensor cores, so no real OXR engine can run here (see
//! docs/evidence/document-parse-provider-20261111.md).

use super::*;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn request(pages: &[u32]) -> ParseRequest {
    ParseRequest {
        schema_version: DOCUMENT_PARSE_DEFINITION_VERSION,
        source_digest: "blake3:deadbeef".into(),
        source_name: Some("doc.pdf".into()),
        pages: pages.iter().map(|i| PageRef { page_index: *i }).collect(),
        closure_id: "oxr/1.0".into(),
        scope: Some("scope-a".into()),
    }
}

// ----- a local mock speaking the upstream response shape ------------------

#[derive(Default)]
struct MockState {
    /// Recorded `(request-target, raw body)` pairs.
    requests: Vec<(String, Vec<u8>)>,
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
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        answer.0,
        reason,
        answer.1.len(),
        answer.1
    );
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.shutdown().await;
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
    // unknown_outcome existed must still deserialize (additive fields only).
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
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 0 }, &request(&[0]))
        .await
        .unwrap();
    let PageOutcome::Parsed(result) = outcome else {
        panic!("a real upstream-shaped answer must produce a page")
    };
    assert_eq!(result.page.page_index, 0);
    assert!(result.coordinates_restored);
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
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 1 }, &request(&[1]))
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
    assert_eq!(
        result.blocks[2].element_type.as_deref(),
        Some("Chemical Structure")
    );

    // A page the document does not contain is a terminal failure, not a
    // fabricated block.
    let outcome = provider
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 5 }, &request(&[5]))
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
        .parse_page(b"%PDF-1.7 fake", PageRef { page_index: 1 }, &request(&[1]))
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
    let mut routes = HashMap::new();
    routes.insert(
        "/parse/sync",
        VecDeque::from(vec![(200u16, upstream_document_json())]),
    );
    let (base, state, _server) = spawn_mock(routes);
    let provider = provider_with(&base, OxrTransport::Sync);

    // Whole-document request: the page count is provider-discovered, never
    // guessed, and each discovered page gets its own checkpoint.
    let whole = request(&[]);
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
        &request(&[0]),
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
    let provider = provider_with("http://127.0.0.1:1/v1", OxrTransport::Sync);
    match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("unreachable"), "{reason}");
        }
        ProviderAvailability::Ready => panic!("an unreachable endpoint is never ready"),
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
        &request(&(0..8).collect::<Vec<_>>()),
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
    let run = run_parse(&request(&[0]), &provider, b"x", ParseOptions::default())
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
    let run = run_parse(&request(&[0]), &provider, b"x", ParseOptions::default())
        .await
        .unwrap();
    assert_eq!(run.state, ParseState::Running);
    assert_eq!(
        run.checkpoints[0].unknown_outcome.as_deref(),
        Some("upstream reply lost")
    );
}
