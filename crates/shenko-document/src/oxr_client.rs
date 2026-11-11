//! The real SoMarkAI/OXR HTTP client.
//!
//! OXR is a *service*, not a library: a self-hosted deployment exposes the
//! SoMark-compatible parsing API at `http://<host>:8000/v1`, the hosted free
//! API at `https://free-oxr.somark.ai/api/v1`, and both speak the same
//! contract (docs.somark.ai/en/api-reference, "API alignment"):
//!
//! * `POST /parse/sync` — multipart upload, whole result in one response.
//! * `POST /parse/async` + `POST /parse/async_check` — submit, then poll by
//!   `task_id` every 3–5 s until `status` is `success` or `failed`. OXR keeps
//!   async tasks in server memory, so a server restart loses the task.
//! * Every answer is an envelope `{ code, message, warnings, data }` with
//!   `code == 0` on success. The per-page parse lives at
//!   `data.result.outputs.json.pages[]`: each page has `page_num`,
//!   `blocks[]` and `page_size { w, h }`; each block has `idx`, `type`,
//!   `bbox` (`[x1, y1, x2, y2]`, integers), `content`, `format` and
//!   `captions`.
//!
//! Upstream has no per-page endpoint: one upload always returns the whole
//! document, and the hosted product is limited to QPS 4 per account (free
//! API: 10MB / 20 pages; self-hosted: deployment-dependent). This module
//! therefore parses a whole document once and never re-uploads per page.
//!
//! The client never invents a field: an absent `bbox`, `type`, `format` or
//! `page_size` stays `None`, so a reader can tell "the engine did not say"
//! apart from "the engine said zero". A failure is classified as either
//! terminal (`is_retryable() == false`: upstream understood and refused) or
//! unknown (`true`: the request may still have been processed), because those
//! are different facts for the caller.

use crate::types::{PageBlock, PageSize};
use serde::Deserialize;
use std::time::Duration;

/// Upstream's own guidance for the async poll interval ("every 3~5 seconds").
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Async polling budget. Upstream documents no deadline; a task that outlives
/// this is an unknown outcome, never a page result.
pub const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(600);
/// Per-request HTTP deadline (upload + server-side parsing for the sync path).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Deadline for the availability probe: reachability only, no parsing.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on an accepted response body. Upstream caps a document at
/// 200MB / 300 pages, so its JSON output is far below this; a larger body
/// means something other than the documented answer came back.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Upstream rate-limit codes (docs.somark.ai/en/api-reference/errors). The
/// request was rejected before any parsing, so a retry is legitimate.
const RATE_LIMIT_CODES: [i64; 2] = [1101, 1124];
/// Upstream task-timeout / task-record codes. The task may still have run
/// (or died with the service), so the outcome is unknown, not a failure.
const UNKNOWN_OUTCOME_CODES: [i64; 2] = [1102, 1120];

/// An upstream credential held as an opaque handle. The secret exists only to
/// be sent as the `api_key` form field; it never appears in `Debug`, in
/// `Display`, in an error message, or in provenance.
#[derive(Clone)]
pub struct CredentialHandle {
    secret: String,
}

impl CredentialHandle {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
        }
    }

    /// The secret, for the request builder only. Crate-visible on purpose:
    /// nothing outside this crate has any business reading the credential.
    pub(crate) fn expose(&self) -> &str {
        &self.secret
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialHandle(***)")
    }
}

impl std::fmt::Display for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("***")
    }
}

/// Which upstream transport the provider uses. Both are real upstream
/// endpoints that OXR implements; the difference is only how the result
/// comes back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OxrTransport {
    /// `POST /parse/async`, then `POST /parse/async_check` until the task
    /// finishes. Upstream recommends this for backend pipelines.
    #[default]
    Async,
    /// `POST /parse/sync`: upload and wait for the whole result.
    Sync,
}

/// Everything needed to talk to one OXR deployment.
#[derive(Debug, Clone)]
pub struct OxrClientConfig {
    /// Upstream base URL, e.g. `http://127.0.0.1:8000/v1` (self-hosted) or
    /// `https://free-oxr.somark.ai/api/v1` (hosted free API). A base without a
    /// path is read as `/v1`, the version prefix both deployments serve.
    pub endpoint: String,
    /// The upstream API key. OXR requires the field to be present but does
    /// not validate it; the hosted product validates it.
    pub credential: CredentialHandle,
    pub transport: OxrTransport,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
    pub poll_timeout: Duration,
}

impl OxrClientConfig {
    pub fn new(endpoint: impl Into<String>, credential: CredentialHandle) -> Self {
        Self {
            endpoint: endpoint.into(),
            credential,
            transport: OxrTransport::default(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            poll_timeout: DEFAULT_POLL_TIMEOUT,
        }
    }
}

/// A failure to obtain a page result from upstream.
#[derive(Debug, thiserror::Error)]
pub enum OxrError {
    /// The configured endpoint is not a usable http(s) URL.
    #[error("OXR endpoint is not a usable http(s) URL: {0}")]
    InvalidEndpoint(String),
    /// The request never produced a usable answer (DNS, connect, TLS,
    /// timeout). Whether upstream parsed the document is unknown.
    #[error("OXR transport failed: {0}")]
    Transport(String),
    /// Upstream answered with a non-success HTTP status.
    #[error("OXR endpoint returned HTTP {status}")]
    HttpStatus { status: u16 },
    /// Upstream answered with a non-zero envelope `code`.
    #[error("OXR upstream error {code}: {message}")]
    Upstream { code: i64, message: String },
    /// The async task reached `failed`.
    #[error("OXR task failed: {0}")]
    TaskFailed(String),
    /// The answer did not have the documented shape.
    #[error("OXR response is missing {0}")]
    MalformedResponse(String),
    /// The async task did not reach a terminal status inside the budget.
    #[error("OXR task did not finish within {0:?}")]
    PollTimeout(Duration),
    /// The upload could not be built.
    #[error("OXR request could not be built: {0}")]
    Request(String),
}

impl OxrError {
    /// `true` when the request may still have been processed upstream: the
    /// caller must record an unknown outcome instead of a page failure.
    pub fn is_retryable(&self) -> bool {
        match self {
            // A transport failure or a polling timeout means we never learned
            // what upstream did with the document.
            Self::Transport(_) | Self::PollTimeout(_) => true,
            Self::HttpStatus { status } => {
                // 408/425/429 and 5xx leave the outcome unknown; any other
                // 4xx is a refusal this identical request will always get.
                matches!(*status, 408 | 425 | 429) || (500..600).contains(status)
            }
            Self::Upstream { code, .. } => {
                RATE_LIMIT_CODES.contains(code) || UNKNOWN_OUTCOME_CODES.contains(code)
            }
            // Refused, failed or unreadable: resending the identical page
            // request changes nothing.
            Self::InvalidEndpoint(_)
            | Self::TaskFailed(_)
            | Self::MalformedResponse(_)
            | Self::Request(_) => false,
        }
    }
}

/// One page of an upstream parse, mapped into the boundary's vocabulary.
#[derive(Debug, Clone)]
pub struct OxrParsedPage {
    /// The upstream page number (0-based), falling back to the page's index
    /// in the upstream `pages` array when the field is absent.
    pub page_num: u32,
    /// Blocks in upstream reading order.
    pub blocks: Vec<PageBlock>,
    /// The page's coordinate space, when upstream reported one.
    pub page_size: Option<PageSize>,
}

/// One upstream document parse.
#[derive(Debug, Clone)]
pub struct OxrDocument {
    pub file_name: Option<String>,
    /// `data.metadata.page_num`: the document's page count as upstream saw it.
    pub page_count: Option<u32>,
    /// OXR reports which requested parameters it ignored. Always empty for
    /// this client, which sends no optional parameter — kept so a caller can
    /// see a deployment disagreeing with the request.
    pub warnings: Vec<String>,
    pub pages: Vec<OxrParsedPage>,
}

/// The real HTTP client for one OXR deployment.
#[derive(Debug, Clone)]
pub struct OxrClient {
    http: reqwest::Client,
    /// Normalized base URL, no trailing slash, always carrying the version
    /// path upstream serves. Never contains the credential.
    base: String,
    credential: CredentialHandle,
    transport: OxrTransport,
    poll_interval: Duration,
    poll_timeout: Duration,
}

impl OxrClient {
    pub fn new(config: OxrClientConfig) -> Result<Self, OxrError> {
        let mut url = reqwest::Url::parse(config.endpoint.trim())
            .map_err(|error| OxrError::InvalidEndpoint(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(OxrError::InvalidEndpoint(format!(
                "scheme `{}` is not http(s)",
                url.scheme()
            )));
        }
        // An endpoint carrying a userinfo would leak the secret into every
        // message that quotes the URL. The credential belongs in the `api_key`
        // form field, so such an endpoint is refused outright.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OxrError::InvalidEndpoint(
                "the endpoint must not embed credentials; they go in the api_key field".into(),
            ));
        }
        if url.path().trim_end_matches('/').is_empty() {
            url.set_path("/v1");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(PROBE_TIMEOUT)
            .timeout(config.request_timeout)
            // The credential is sealed to this deployment. An automatic
            // redirect would re-send it to another host, so a redirect is a
            // terminal answer instead of a followed hop.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| OxrError::Request(error.to_string()))?;
        Ok(Self {
            http,
            base: url.as_str().trim_end_matches('/').to_owned(),
            credential: config.credential,
            transport: config.transport,
            poll_interval: config.poll_interval,
            poll_timeout: config.poll_timeout,
        })
    }

    /// The deployment this client is bound to. Never contains the credential.
    pub fn endpoint(&self) -> &str {
        &self.base
    }

    /// The upstream transport this client speaks.
    pub fn transport(&self) -> OxrTransport {
        self.transport
    }

    /// Reachability only: did an HTTP answer come back from the deployment?
    /// Any status counts — a 404 still proves the endpoint is there. The
    /// credential is not sent, so a probe neither spends quota nor proves the
    /// credential is accepted; it answers exactly one question.
    pub async fn probe(&self) -> Result<(), OxrError> {
        let response = self
            .http
            .get(self.base.clone())
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|error| OxrError::Transport(error.to_string()))?;
        let _ = response.status();
        Ok(())
    }

    /// Parse one whole document. Upstream has no per-page endpoint, so one
    /// upload always yields every page; callers needing several pages of one
    /// document must share this result instead of re-uploading per page.
    pub async fn parse_document(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<OxrDocument, OxrError> {
        let form = self.upload_form(file_name, bytes)?;
        let envelope = match self.transport {
            OxrTransport::Sync => self.post("/parse/sync", form).await?,
            OxrTransport::Async => self.submit_and_poll(form).await?,
        };
        map_document(envelope)
    }

    /// Build the multipart upload. Only the structured `json` output is
    /// requested: `markdown` is another rendering of the same parse and `zip`
    /// requires `element_formats.image = file`, which OXR rejects. No
    /// `element_formats` or `feature_config` is sent, so the deployment's own
    /// defaults apply and the same request works against OXR and the hosted
    /// SoMark API (which reject or ignore the unsupported options).
    fn upload_form(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<reqwest::multipart::Form, OxrError> {
        // reqwest needs an owned body, so the document is copied once per
        // upstream request (upstream caps a file at 200MB).
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(file_name.to_owned())
            .mime_str("application/octet-stream")
            .map_err(|error| OxrError::Request(error.to_string()))?;
        Ok(reqwest::multipart::Form::new()
            .part("file", part)
            .text("api_key", self.credential.expose().to_owned())
            .text("output_formats", "json"))
    }

    async fn post(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<OxrEnvelope, OxrError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .multipart(form)
            .send()
            .await
            .map_err(|error| OxrError::Transport(error.to_string()))?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES)
        {
            return Err(OxrError::MalformedResponse(format!(
                "an answer below {MAX_RESPONSE_BYTES} bytes (Content-Length was {:?})",
                response.content_length()
            )));
        }
        let body = response
            .text()
            .await
            .map_err(|error| OxrError::Transport(error.to_string()))?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(OxrError::MalformedResponse(format!(
                "an answer below {MAX_RESPONSE_BYTES} bytes ({} arrived)",
                body.len()
            )));
        }
        // The body is never echoed into an error: an upstream error page can
        // quote the request, and with it the credential. `code`/`message` are
        // read out of it when they are there.
        if !status.is_success() {
            if let Ok(envelope) = serde_json::from_str::<OxrEnvelope>(&body)
                && let Some(code) = envelope.code.filter(|code| *code != 0)
            {
                return Err(OxrError::Upstream {
                    code,
                    message: envelope.message.unwrap_or_default(),
                });
            }
            return Err(OxrError::HttpStatus {
                status: status.as_u16(),
            });
        }
        let envelope: OxrEnvelope = serde_json::from_str(&body)
            .map_err(|_| OxrError::MalformedResponse("a JSON envelope".into()))?;
        let code = envelope
            .code
            .ok_or_else(|| OxrError::MalformedResponse("the envelope `code`".into()))?;
        if code != 0 {
            return Err(OxrError::Upstream {
                code,
                message: envelope.message.unwrap_or_default(),
            });
        }
        Ok(envelope)
    }

    /// Submit, then poll by `task_id` until the task is terminal. Upstream
    /// asks for a 3–5 s interval; OXR keeps tasks in server memory, so a
    /// restart mid-poll surfaces as "task record not found" (unknown outcome).
    async fn submit_and_poll(
        &self,
        form: reqwest::multipart::Form,
    ) -> Result<OxrEnvelope, OxrError> {
        let submitted = self.post("/parse/async", form).await?;
        let data = submitted
            .data
            .ok_or_else(|| OxrError::MalformedResponse("`data` from the async submit".into()))?;
        let task_id = data
            .task_id
            .ok_or_else(|| OxrError::MalformedResponse("`data.task_id`".into()))?;
        let deadline = std::time::Instant::now() + self.poll_timeout;
        loop {
            let form = reqwest::multipart::Form::new()
                .text("task_id", task_id.clone())
                .text("api_key", self.credential.expose().to_owned());
            let envelope = self.post("/parse/async_check", form).await?;
            let data = envelope
                .data
                .as_ref()
                .ok_or_else(|| OxrError::MalformedResponse("`data` from the async check".into()))?;
            // `data` borrows the envelope, so the status is cloned before the
            // match to keep the successful arm free to return the envelope.
            let status = data.status.clone();
            match status.as_deref() {
                Some("success") => return Ok(envelope),
                Some("failed") => {
                    let reason = data
                        .error
                        .as_deref()
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or("upstream reported the task as failed")
                        .to_owned();
                    return Err(OxrError::TaskFailed(reason));
                }
                // `queuing` and `processing` are both non-terminal.
                Some("queuing") | Some("processing") => {}
                Some(other) => {
                    return Err(OxrError::MalformedResponse(format!(
                        "a known async task status (got `{other}`)"
                    )));
                }
                None => {
                    return Err(OxrError::MalformedResponse(
                        "`data.status` from the async check".into(),
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(OxrError::PollTimeout(self.poll_timeout));
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

/// The upstream envelope: `{ code, message, warnings, data }`.
#[derive(Debug, Deserialize)]
struct OxrEnvelope {
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    data: Option<OxrData>,
}

/// Upstream `data`. The sync answer carries `metadata` + `result`, the async
/// submit carries `task_id`, and the async check carries `status` + `result`;
/// one struct covers all three because every field is genuinely optional.
#[derive(Debug, Deserialize)]
struct OxrData {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    metadata: Option<OxrMetadata>,
    #[serde(default)]
    result: Option<OxrResult>,
}

#[derive(Debug, Deserialize)]
struct OxrMetadata {
    #[serde(default)]
    page_num: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OxrResult {
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    outputs: Option<OxrOutputs>,
}

#[derive(Debug, Deserialize)]
struct OxrOutputs {
    #[serde(default)]
    json: Option<OxrJsonOutput>,
}

#[derive(Debug, Deserialize)]
struct OxrJsonOutput {
    #[serde(default)]
    pages: Vec<OxrPageDto>,
}

#[derive(Debug, Deserialize)]
struct OxrPageDto {
    #[serde(default)]
    page_num: Option<i64>,
    #[serde(default)]
    blocks: Vec<OxrBlockDto>,
    #[serde(default)]
    page_size: Option<OxrPageSizeDto>,
}

#[derive(Debug, Deserialize)]
struct OxrPageSizeDto {
    #[serde(default)]
    w: Option<f32>,
    #[serde(default)]
    h: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct OxrBlockDto {
    #[serde(default)]
    idx: Option<u32>,
    #[serde(default, rename = "type")]
    element_type: Option<String>,
    /// `[x1, y1, x2, y2]`, integers upstream. Read as a vector so a
    /// wrong-length box becomes "unknown" instead of failing the page.
    #[serde(default)]
    bbox: Option<Vec<f64>>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    captions: Vec<i64>,
}

fn map_document(envelope: OxrEnvelope) -> Result<OxrDocument, OxrError> {
    let data = envelope
        .data
        .ok_or_else(|| OxrError::MalformedResponse("`data` from the parse answer".into()))?;
    let result = data
        .result
        .ok_or_else(|| OxrError::MalformedResponse("`data.result` (no parse result)".into()))?;
    let json = result
        .outputs
        .and_then(|outputs| outputs.json)
        .ok_or_else(|| OxrError::MalformedResponse("`data.result.outputs.json`".into()))?;
    let pages = json
        .pages
        .iter()
        .enumerate()
        .map(|(position, page)| map_page(page, position))
        .collect();
    Ok(OxrDocument {
        file_name: result.file_name,
        page_count: data.metadata.and_then(|metadata| metadata.page_num),
        warnings: envelope.warnings,
        pages,
    })
}

fn map_page(page: &OxrPageDto, position: usize) -> OxrParsedPage {
    let page_size = page
        .page_size
        .as_ref()
        .and_then(|size| match (size.w, size.h) {
            // A half-reported or degenerate size is no size at all: without both
            // dimensions the bboxes have no coordinate space.
            (Some(width), Some(height))
                if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 =>
            {
                Some(PageSize { width, height })
            }
            _ => None,
        });
    let blocks = page
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| map_block(block, index as u32))
        .collect();
    OxrParsedPage {
        page_num: page
            .page_num
            .and_then(|number| u32::try_from(number).ok())
            .unwrap_or(position as u32),
        blocks,
        page_size,
    }
}

fn map_block(block: &OxrBlockDto, position: u32) -> PageBlock {
    PageBlock {
        // Upstream numbers blocks in reading order itself; when it omits the
        // index, the array position is the reading order it documents.
        order: block.idx.unwrap_or(position),
        text: block.content.clone().unwrap_or_default(),
        bbox: block.bbox.as_deref().and_then(bbox_of),
        structure_ref: structure_ref_of(block),
        element_type: non_empty(block.element_type.as_deref()),
        content_format: non_empty(block.format.as_deref()),
    }
}

/// `[x1, y1, x2, y2]` and nothing else: a wrong-length or non-finite box is
/// unknown, never a repaired guess.
fn bbox_of(values: &[f64]) -> Option<[f32; 4]> {
    let [x1, y1, x2, y2]: [f64; 4] = values.try_into().ok()?;
    if ![x1, y1, x2, y2].iter().all(|value| value.is_finite()) {
        return None;
    }
    Some([x1 as f32, y1 as f32, x2 as f32, y2 as f32])
}

/// Upstream links a figure/table block to its caption blocks by block index.
/// That is the provider's own structural cross-reference and nothing else in
/// the boundary carries it, so it is preserved verbatim.
fn structure_ref_of(block: &OxrBlockDto) -> Option<String> {
    if block.captions.is_empty() {
        return None;
    }
    let indices: Vec<String> = block
        .captions
        .iter()
        .map(|index| index.to_string())
        .collect();
    Some(format!("oxr:captions={}", indices.join(",")))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
