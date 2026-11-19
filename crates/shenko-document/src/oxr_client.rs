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
//! terminal (`outcome_is_unknown() == false`: upstream understood and refused) or
//! unknown (`true`: the request may still have been processed), because those
//! are different facts for the caller.

use crate::types::{PageBlock, PageSize};
use serde::{Deserialize, Serialize};
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
/// request was rejected before parsing. This provider still never re-arms
/// the original attempt; any deliberate retry is a new host operation.
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

    /// The secret, for the request form and response redaction/echo checks.
    /// Crate-visible on purpose: hosts retain receipts, not credentials.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
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
    /// An armed upload has no durable terminal receipt. It must not be sent again.
    #[error("OXR attempt outcome unknown: {0}")]
    OutcomeUnknown(String),
    /// The host could not durably record the attempt or its receipt.
    #[error("OXR attempt receipt unavailable: {0}")]
    Receipt(String),
    /// The upload could not be built.
    #[error("OXR request could not be built: {0}")]
    Request(String),
}

impl OxrError {
    /// `true` when the request may still have been processed upstream: the
    /// caller must record an unknown outcome instead of a page failure.
    /// This classification never authorizes another upload.
    pub fn outcome_is_unknown(&self) -> bool {
        match self {
            // A transport failure or a polling timeout means we never learned
            // what upstream did with the document.
            Self::Transport(_)
            | Self::PollTimeout(_)
            | Self::MalformedResponse(_)
            | Self::OutcomeUnknown(_)
            | Self::Receipt(_) => true,
            Self::HttpStatus { status } => {
                // 408/425/429 and 5xx leave the outcome unknown; any other
                // 4xx is a refusal this identical request will always get.
                matches!(*status, 408 | 425 | 429) || (500..600).contains(status)
            }
            Self::Upstream { code, .. } => {
                RATE_LIMIT_CODES.contains(code) || UNKNOWN_OUTCOME_CODES.contains(code)
            }
            // Definite refusal or failure; no automatic replay is authorized.
            Self::InvalidEndpoint(_) | Self::TaskFailed(_) | Self::Request(_) => false,
        }
    }
}

/// One page of an upstream parse, mapped into the boundary's vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// The original asynchronous acceptance, retained by the host before polling.
/// A waiting deadline does not terminate the remote task or authorize another upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxrTaskHandle {
    pub task_id: String,
    pub accepted_at: chrono::DateTime<chrono::Utc>,
    pub poll_deadline: chrono::DateTime<chrono::Utc>,
}

/// One upload's response. The provider durably records an accepted task before
/// continuing with read-only polling.
#[derive(Debug, Clone)]
pub enum OxrSubmission {
    Completed(OxrDocument),
    Accepted(OxrTaskHandle),
}

/// Whether the retained bytes cover the complete HTTP entity body. These are
/// response bytes only: request forms and authorization headers are not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OxrBodyState {
    Complete,
    Incomplete,
    /// A recognizable credential echo is deliberately not archived. This is
    /// an explicit evidence gap, never a claim that redacted bytes are original.
    CredentialEchoWithheld,
}

/// One bounded received HTTP response, before its JSON fields are interpreted.
/// Debug output intentionally omits the body, which can contain document data.
pub struct OxrHttpResponse {
    pub path: String,
    pub status: u16,
    pub declared_length: Option<u64>,
    pub body_state: OxrBodyState,
    body: Vec<u8>,
}

impl OxrHttpResponse {
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl std::fmt::Debug for OxrHttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OxrHttpResponse")
            .field("path", &self.path)
            .field("status", &self.status)
            .field("declared_length", &self.declared_length)
            .field("body_state", &self.body_state)
            .field("received_bytes", &self.body.len())
            .finish()
    }
}

/// The mapped fact and the original bounded response that produced it. A
/// transport interruption before a complete read may have no response receipt.
#[derive(Debug)]
pub struct OxrObservation<T> {
    pub outcome: Result<T, OxrError>,
    pub response: Option<OxrHttpResponse>,
}

impl<T> OxrObservation<T> {
    fn without_response(outcome: Result<T, OxrError>) -> Self {
        Self {
            outcome,
            response: None,
        }
    }

    fn and_then<U>(self, f: impl FnOnce(T) -> Result<U, OxrError>) -> OxrObservation<U> {
        OxrObservation {
            outcome: self.outcome.and_then(f),
            response: self.response,
        }
    }
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
        if url.query().is_some() || url.fragment().is_some() {
            return Err(OxrError::InvalidEndpoint(
                "the endpoint must not contain a query or fragment".into(),
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
        match self.submit_document(file_name, bytes).await? {
            OxrSubmission::Completed(document) => Ok(document),
            OxrSubmission::Accepted(task) => self.poll_document(&task).await,
        }
    }

    /// Send exactly one upload. Durable callers must first claim their original
    /// operation/attempt in the host ledger, and retain this response there.
    pub async fn submit_document(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<OxrSubmission, OxrError> {
        self.submit_document_observed(file_name, bytes)
            .await
            .outcome
    }

    /// The durable provider uses this path to retain the original response
    /// alongside the mapped acceptance/result before doing further work.
    pub async fn submit_document_observed(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> OxrObservation<OxrSubmission> {
        let form = match self.upload_form(file_name, bytes) {
            Ok(form) => form,
            Err(error) => return OxrObservation::without_response(Err(error)),
        };
        if self.transport == OxrTransport::Sync {
            return self
                .post("/parse/sync", form)
                .await
                .and_then(|envelope| map_document(envelope).map(OxrSubmission::Completed));
        }
        self.post("/parse/async", form).await.and_then(|envelope| {
            let data = envelope.data.ok_or_else(|| {
                OxrError::MalformedResponse("`data` from the async submit".into())
            })?;
            let task_id = data
                .task_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| OxrError::MalformedResponse("a nonempty `data.task_id`".into()))?;
            let accepted_at = chrono::Utc::now();
            let budget = chrono::Duration::from_std(self.poll_timeout).map_err(|_| {
                OxrError::OutcomeUnknown("poll deadline overflow after acceptance".into())
            })?;
            let poll_deadline = accepted_at.checked_add_signed(budget).ok_or_else(|| {
                OxrError::OutcomeUnknown("poll deadline overflow after acceptance".into())
            })?;
            Ok(OxrSubmission::Accepted(OxrTaskHandle {
                task_id,
                accepted_at,
                poll_deadline,
            }))
        })
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
    ) -> OxrObservation<OxrEnvelope> {
        let mut response = match self
            .http
            .post(format!("{}{path}", self.base))
            .multipart(form)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return OxrObservation::without_response(Err(OxrError::Transport(
                    error.to_string(),
                )));
            }
        };
        let status = response.status();
        let mut raw = OxrHttpResponse {
            path: path.to_owned(),
            status: status.as_u16(),
            declared_length: response.content_length(),
            body_state: OxrBodyState::Incomplete,
            body: Vec::new(),
        };
        if raw
            .declared_length
            .is_some_and(|length| length > MAX_RESPONSE_BYTES)
        {
            let error = OxrError::MalformedResponse(format!(
                "an answer below {MAX_RESPONSE_BYTES} bytes (Content-Length was {:?})",
                raw.declared_length
            ));
            return self.observed(raw, Err(error));
        }
        // Content-Length is only an early rejection. Chunked or dishonest
        // responses must be bounded while reading, before allocation of a full body.
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    return self.observed(raw, Err(OxrError::Transport(error.to_string())));
                }
            };
            let remaining = MAX_RESPONSE_BYTES as usize - raw.body.len();
            if chunk.len() > remaining {
                raw.body.extend_from_slice(&chunk[..remaining]);
                return self.observed(
                    raw,
                    Err(OxrError::MalformedResponse(format!(
                        "an answer no larger than {MAX_RESPONSE_BYTES} bytes"
                    ))),
                );
            }
            raw.body.extend_from_slice(&chunk);
        }
        raw.body_state = OxrBodyState::Complete;
        let outcome = self.decode(status, &raw.body);
        self.observed(raw, outcome)
    }

    fn observed<T>(
        &self,
        mut raw: OxrHttpResponse,
        mut outcome: Result<T, OxrError>,
    ) -> OxrObservation<T> {
        let secret = self.credential.expose();
        let encoded = serde_json::to_string(secret).ok();
        let contains = |needle: &[u8]| {
            !needle.is_empty() && raw.body.windows(needle.len()).any(|part| part == needle)
        };
        if !secret.is_empty()
            && (contains(secret.as_bytes())
                || encoded
                    .as_ref()
                    .and_then(|text| text.strip_prefix('"'))
                    .and_then(|text| text.strip_suffix('"'))
                    .is_some_and(|text| contains(text.as_bytes())))
        {
            raw.body.clear();
            raw.body_state = OxrBodyState::CredentialEchoWithheld;
            // A recognized document field can echo the same credential as
            // the wire body. Do not archive that decoded success through the
            // normalized receipt or page result after withholding its bytes.
            // Decoded refusals already pass through `redact` in `decode`.
            if outcome.is_ok() {
                outcome = Err(OxrError::OutcomeUnknown(
                    "response contained a credential echo; original and decoded content withheld"
                        .into(),
                ));
            }
        }
        OxrObservation {
            outcome,
            response: Some(raw),
        }
    }

    fn decode(&self, status: reqwest::StatusCode, body: &[u8]) -> Result<OxrEnvelope, OxrError> {
        // The body is never echoed into an error: an upstream error page can
        // quote the request, and with it the credential. `code`/`message` are
        // read out of it when they are there.
        if !status.is_success() {
            if let Ok(envelope) = serde_json::from_slice::<OxrEnvelope>(body)
                && let Some(code) = envelope.code.filter(|code| *code != 0)
            {
                return Err(OxrError::Upstream {
                    code,
                    message: self.redact(envelope.message.unwrap_or_default()),
                });
            }
            return Err(OxrError::HttpStatus {
                status: status.as_u16(),
            });
        }
        let envelope: OxrEnvelope = serde_json::from_slice(body)
            .map_err(|_| OxrError::MalformedResponse("a JSON envelope".into()))?;
        let code = envelope
            .code
            .ok_or_else(|| OxrError::MalformedResponse("the envelope `code`".into()))?;
        if code != 0 {
            return Err(OxrError::Upstream {
                code,
                message: self.redact(envelope.message.unwrap_or_default()),
            });
        }
        Ok(envelope)
    }

    fn redact(&self, text: String) -> String {
        let secret = self.credential.expose();
        if secret.is_empty() {
            text
        } else {
            text.replace(secret, "***")
        }
    }

    /// Reconcile only the original accepted task. Every HTTP request, body read
    /// and sleep shares one deadline; neither a slow response nor a restart
    /// resets the budget. Expiry remains unknown, never remote cancellation.
    pub async fn poll_document(&self, task: &OxrTaskHandle) -> Result<OxrDocument, OxrError> {
        self.poll_document_with_observer(task, |_| Ok(())).await
    }

    /// Persist every received poll observation synchronously before another
    /// network request or sleep. The same absolute waiting deadline still
    /// covers all requests; recording evidence never grants upload authority.
    pub async fn poll_document_with_observer(
        &self,
        task: &OxrTaskHandle,
        mut observe: impl FnMut(&OxrObservation<Option<OxrDocument>>) -> Result<(), OxrError>,
    ) -> Result<OxrDocument, OxrError> {
        let now = chrono::Utc::now();
        if now < task.accepted_at || task.poll_deadline < task.accepted_at {
            return Err(OxrError::OutcomeUnknown(
                "the original polling clock cannot be established".into(),
            ));
        }
        let remaining = (task.poll_deadline - now)
            .to_std()
            .map_err(|_| OxrError::PollTimeout(self.poll_timeout))?;
        let deadline = tokio::time::Instant::now()
            .checked_add(remaining.min(self.poll_timeout))
            .ok_or_else(|| OxrError::Request("poll deadline overflow".into()))?;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(OxrError::PollTimeout(self.poll_timeout));
            }
            let observed = tokio::time::timeout_at(deadline, self.check_document_observed(task))
                .await
                .map_err(|_| OxrError::PollTimeout(self.poll_timeout))?;
            observe(&observed)?;
            let document = observed.outcome?;
            if tokio::time::Instant::now() >= deadline {
                return Err(OxrError::PollTimeout(self.poll_timeout));
            }
            if let Some(document) = document {
                return Ok(document);
            }
            let next = tokio::time::Instant::now()
                .checked_add(self.poll_interval)
                .unwrap_or(deadline)
                .min(deadline);
            tokio::time::sleep_until(next).await;
        }
    }

    /// One read-only observation of the original task. This remains available
    /// after the original wait expired: a waiting deadline is not evidence of
    /// remote termination. The client's request timeout still bounds this call.
    pub async fn check_document(
        &self,
        task: &OxrTaskHandle,
    ) -> Result<Option<OxrDocument>, OxrError> {
        self.check_document_observed(task).await.outcome
    }

    /// The read-only original-task observation with its bounded response.
    pub async fn check_document_observed(
        &self,
        task: &OxrTaskHandle,
    ) -> OxrObservation<Option<OxrDocument>> {
        if task.task_id.is_empty() || task.poll_deadline < task.accepted_at {
            return OxrObservation::without_response(Err(OxrError::OutcomeUnknown(
                "invalid original async task receipt".into(),
            )));
        }
        let form = reqwest::multipart::Form::new()
            .text("task_id", task.task_id.clone())
            .text("api_key", self.credential.expose().to_owned());
        let mut observed = self.post("/parse/async_check", form).await;
        observed.outcome = observed.outcome.map_err(|error| {
            if error.outcome_is_unknown() {
                error
            } else {
                OxrError::OutcomeUnknown(format!("task observation was refused: {error}"))
            }
        });
        observed.and_then(|envelope| {
            let data = envelope
                .data
                .as_ref()
                .ok_or_else(|| OxrError::MalformedResponse("`data` from the async check".into()))?;
            if data.task_id.as_deref() != Some(task.task_id.as_str()) {
                return Err(OxrError::MalformedResponse(
                    "the original `data.task_id`".into(),
                ));
            }
            match data.status.as_deref() {
                Some("success") => map_document(envelope).map(Some),
                Some("failed") => {
                    let reason = data
                        .error
                        .as_deref()
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or("upstream reported the task as failed")
                        .to_owned();
                    Err(OxrError::TaskFailed(self.redact(reason)))
                }
                Some("queuing") | Some("processing") => Ok(None),
                Some(other) => Err(OxrError::MalformedResponse(format!(
                    "a known async task status (got `{}`)",
                    self.redact(other.to_owned())
                ))),
                None => Err(OxrError::MalformedResponse(
                    "`data.status` from the async check".into(),
                )),
            }
        })
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
    pages: Option<Vec<OxrPageDto>>,
}

#[derive(Debug, Deserialize)]
struct OxrPageDto {
    #[serde(default, deserialize_with = "explicit_page_index")]
    page_num: Option<u32>,
    #[serde(default)]
    blocks: Vec<OxrBlockDto>,
    #[serde(default)]
    page_size: Option<OxrPageSizeDto>,
}

/// Only omission permits the array position. A present value must be a u32;
/// null, negative, fractional and overflow values are malformed observations.
fn explicit_page_index<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
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
    let raw_pages = json
        .pages
        .ok_or_else(|| OxrError::MalformedResponse("`data.result.outputs.json.pages`".into()))?;
    let page_count = data.metadata.and_then(|metadata| metadata.page_num);
    if page_count.is_some_and(|count| count as usize != raw_pages.len()) {
        return Err(OxrError::MalformedResponse(
            "a complete pages array matching metadata.page_num".into(),
        ));
    }
    let pages: Vec<_> = raw_pages
        .iter()
        .enumerate()
        .map(|(position, page)| map_page(page, position))
        .collect();
    let mut indices: Vec<_> = pages.iter().map(|page| page.page_num).collect();
    indices.sort_unstable();
    if indices
        .iter()
        .enumerate()
        .any(|(index, value)| u32::try_from(index).ok() != Some(*value))
    {
        return Err(OxrError::MalformedResponse(
            "unique contiguous zero-based page indices".into(),
        ));
    }
    Ok(OxrDocument {
        file_name: result.file_name,
        page_count,
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
        page_num: page.page_num.unwrap_or(position as u32),
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
