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
//!   `captions` and the transient image resource `img_url`.
//!
//! Upstream has no per-page endpoint: one upload always returns the whole
//! document, and the hosted product is limited to QPS 4 per account (free
//! API: 10MB / 20 pages; self-hosted: deployment-dependent). This module
//! therefore parses a whole document once and never re-uploads per page.
//!
//! The client never invents a field: an absent `bbox`, `type`, `format` or
//! `page_size` stays `None`, so a reader can tell "the engine did not say"
//! apart from "the engine said zero". A failure is classified as either
//! terminal (`outcome_is_unknown() == false`: a conclusive fresh-submit refusal or
//! original-task failure) or
//! unknown (`true`: the request may still have been processed), because those
//! are different facts for the caller.

use crate::types::{PageBlock, PageImageResource, PageSize};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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
/// This client's response-body admission bound, enforced while streaming.
/// Self-hosted OXR has no documented fixed output cap; input size/page limits
/// do not establish an upper bound for its JSON response.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Upstream rate-limit codes (docs.somark.ai/en/api-reference/errors). Their
/// outcome stays Unknown; these codes alone never authorize another upload.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Which original observation produced retained response bytes. This contains
/// no configured URL, credential or provider-controlled diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OxrResponseSource {
    Sync,
    AsyncSubmit,
    AsyncCheck,
    SavedResponse,
}

/// Bounded original response evidence, separate from its interpretation.
/// `None` means no body bytes were captured; a complete empty body is
/// `Some(Vec::new())`. Partial capture never claims the remote task stopped.
#[derive(Clone)]
pub struct OxrResponseObservation {
    pub body: Option<Vec<u8>>,
    /// Exact declared header bytes, when present; not a trusted JSON MIME.
    pub declared_content_type: Option<Vec<u8>>,
    pub capture_complete: bool,
    pub source: OxrResponseSource,
    pub http_status: Option<u16>,
}

impl std::fmt::Debug for OxrResponseObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OxrResponseObservation")
            .field("captured_bytes", &self.body.as_ref().map(Vec::len))
            .field("capture_complete", &self.capture_complete)
            .field("source", &self.source)
            .field("http_status", &self.http_status)
            .finish_non_exhaustive()
    }
}

impl OxrResponseObservation {
    fn saved(body: Vec<u8>, capture_complete: bool) -> Self {
        Self { body: Some(body), declared_content_type: None, capture_complete,
            source: OxrResponseSource::SavedResponse, http_status: None }
    }
}

/// A failure to obtain a page result from upstream. Raw observations are
/// deliberately excluded from Display and Debug.
#[derive(Clone, thiserror::Error)]
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
    #[error("OXR upstream error {code} (provider message retained in response evidence)")]
    Upstream { code: i64, message: String },
    /// The async task reached `failed`.
    #[error("OXR task failed (provider reason retained in response evidence)")]
    TaskFailed(String),
    /// The answer did not have the documented shape.
    #[error("OXR response is missing {0}")]
    MalformedResponse(String),
    /// The async task did not reach a terminal status inside the budget.
    #[error("OXR task did not finish within {0:?}")]
    PollTimeout(Duration),
    /// A known upstream task remains unresolved. Query this same handle;
    /// uploading the document again would create a different effect.
    #[error("OXR accepted task outcome unknown: {reason}")]
    TaskUnknown { task_id: String, reason: String, observation: Option<Arc<OxrResponseObservation>> },
    /// The original response or captured prefix accompanies its typed cause.
    #[error("{cause}")]
    Observed { cause: Box<OxrError>, observation: Arc<OxrResponseObservation> },
    /// The original response contained a recognizable credential echo.
    /// Its bytes are withheld explicitly; the typed cause keeps its outcome.
    #[error("{cause}; credential echo response bytes withheld")]
    CredentialEchoWithheld { cause: Box<OxrError> },
    /// The upload could not be built.
    #[error("OXR request could not be built: {0}")]
    Request(String),
}

impl std::fmt::Debug for OxrError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Provider text, task handles, header values and raw bytes remain
        // structured evidence, never Debug output.
        std::fmt::Display::fmt(self, formatter)
    }
}

impl OxrError {
    fn observed(self, observation: OxrResponseObservation) -> Self {
        Self::Observed { cause: Box::new(self), observation: Arc::new(observation) }
    }

    /// Borrow actual captured bytes without rendering or copying them.
    pub fn response_observation(&self) -> Option<&OxrResponseObservation> {
        match self {
            Self::Observed { observation, .. } => Some(observation.as_ref()),
            Self::TaskUnknown { observation, .. } => observation.as_deref(),
            _ => None,
        }
    }

    /// A typed terminal task fact from the original check path, after its
    /// returned identity is checked. An HTTP/envelope refusal of that check
    /// cannot establish that the accepted computation ended.
    pub fn is_terminal_task_failure(&self) -> bool {
        match self {
            Self::TaskFailed(_) => true,
            Self::Observed { cause, .. } | Self::CredentialEchoWithheld { cause } => cause.is_terminal_task_failure(),
            _ => false,
        }
    }

    /// `true` when the request may still have been processed upstream: the
    /// caller must record an unknown outcome instead of a page failure.
    /// Accepted-handle checks use `poll_document`'s phase-aware normalization;
    /// a fresh-submit refusal alone says nothing about an accepted task.
    pub fn outcome_is_unknown(&self) -> bool {
        match self {
            Self::Observed { cause, .. } | Self::CredentialEchoWithheld { cause } => cause.outcome_is_unknown(),
            // A transport failure or a polling timeout means we never learned
            // what upstream did with the document.
            Self::Transport(_) | Self::PollTimeout(_) | Self::TaskUnknown { .. }
                | Self::MalformedResponse(_) => true,
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
            | Self::Request(_) => false,
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
    /// Upstream warnings, preserved verbatim even though this client sends
    /// no optional parameters. Their absence proves no recognition property.
    pub warnings: Vec<String>,
    pub pages: Vec<OxrParsedPage>,
    /// Exact bounded upstream result bytes, retained by the durable consumer
    /// in CAS before it publishes page checkpoints.
    pub raw_response: Vec<u8>,
}

impl OxrDocument {
    /// Decode saved bytes with this build's current mapper without contacting
    /// a service. The caller authenticates the CAS bytes and original binding.
    /// This is not an old-mapper/provenance migration: a historical checkpoint
    /// remains an immutable observation under its original code and closure.
    pub fn from_response(mut bytes: Vec<u8>) -> Result<Self, OxrError> {
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            bytes.truncate(MAX_RESPONSE_BYTES as usize);
            return Err(OxrError::MalformedResponse("a bounded saved response".into())
                .observed(OxrResponseObservation::saved(bytes, false)));
        }
        let mut envelope: OxrEnvelope = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => return Err(OxrError::MalformedResponse("a saved JSON envelope".into())
                .observed(OxrResponseObservation::saved(bytes, true))),
        };
        if envelope.code != Some(0) {
            return Err(OxrError::MalformedResponse("a successful saved envelope".into())
                .observed(OxrResponseObservation::saved(bytes, true)));
        }
        envelope.observation = Some(OxrResponseObservation::saved(bytes, true));
        map_document(envelope)
    }
}

/// Submission and observation are separate so an accepted task can be
/// recorded before the first poll, and reopened without another upload.
#[derive(Debug)]
pub enum OxrSubmission {
    Complete(OxrDocument),
    Accepted { task_id: String, response: OxrResponseObservation },
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
            return Err(OxrError::InvalidEndpoint("the endpoint must not contain a query or fragment".into()));
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

    /// Reuse only the currently admitted endpoint and credential source for
    /// the original read-only mapper. The host authenticates legacy bindings.
    pub fn legacy_main_reader(&self, request_timeout: Duration, poll_timeout: Duration,
        poll_interval: Duration) -> Result<crate::legacy_main::OxrClient, crate::legacy_main::OxrError>
    {
        let mut config = crate::legacy_main::OxrClientConfig::new(self.base.clone(), self.credential.clone());
        config.request_timeout = request_timeout;
        config.poll_timeout = poll_timeout;
        config.poll_interval = poll_interval;
        crate::legacy_main::OxrClient::new(config)
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
            .map_err(|error| OxrError::Transport(error.without_url().to_string()))?;
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
            OxrSubmission::Complete(document) => Ok(document),
            OxrSubmission::Accepted { task_id, .. } => {
                let deadline = chrono::Duration::from_std(self.poll_timeout).ok()
                    .and_then(|budget| chrono::Utc::now().checked_add_signed(budget))
                    .ok_or_else(|| OxrError::TaskUnknown { task_id: task_id.clone(),
                        reason: "original polling deadline overflow".into(), observation: None })?;
                self.poll_document(&task_id, deadline).await
            },
        }
    }

    /// Issue exactly one upload. The caller must durably arm its original
    /// attempt before calling and save an accepted handle before polling.
    pub async fn submit_document(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<OxrSubmission, OxrError> {
        self.submit_document_owned(file_name, bytes.to_vec()).await
    }

    /// The CAS consumer already owns its bounded source buffer; move it into
    /// the multipart body instead of retaining a second document-sized copy.
    pub async fn submit_document_owned(
        &self,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<OxrSubmission, OxrError> {
        let form = self.upload_form(file_name, bytes)?;
        match self.transport {
            OxrTransport::Sync => map_document(self.post("/parse/sync", form).await?)
                .map(OxrSubmission::Complete),
            OxrTransport::Async => {
                let mut submitted = self.post("/parse/async", form).await?;
                let Some(task_id) = submitted.data.as_mut().and_then(|data| data.task_id.take())
                    .filter(|id| !id.trim().is_empty() && id.len() <= 4096) else {
                    return Err(observed_envelope_error(submitted, OxrError::MalformedResponse("`data.task_id`".into())));
                };
                let response = submitted.observation.take().expect("HTTP acceptance has original response evidence");
                Ok(OxrSubmission::Accepted { task_id, response })
            }
        }
    }

    /// Query only an already accepted task. No upload or new attempt occurs.
    /// A timeout ends local observation, not the remote computation.
    pub async fn poll_document(
        &self, task_id: &str, original_deadline: chrono::DateTime<chrono::Utc>,
    ) -> Result<OxrDocument, OxrError> {
        let result = self.poll(task_id, original_deadline).await.and_then(map_document);
        Self::original_task_outcome(task_id, result)
    }

    /// One bounded read of the original handle, including after its waiting
    /// budget expired. It creates neither another upload nor another budget.
    pub async fn check_document(&self, task_id: &str) -> Result<Option<OxrDocument>, OxrError> {
        let result = self.check(task_id).await.and_then(|envelope| envelope.map(map_document).transpose());
        Self::original_task_outcome(task_id, result)
    }

    /// The durable owner commits each completed pending response before the
    /// next check. Ledger failures propagate independently of provider errors.
    pub async fn check_document_with_observer(&self, task_id: &str,
        observe: &mut impl FnMut(&OxrResponseObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<Result<Option<OxrDocument>, OxrError>> {
        let envelope = match self.check_envelope(task_id).await {
            Ok(envelope) => envelope,
            Err(error) => return Ok(Self::original_task_outcome(task_id, Err(error))),
        };
        if envelope.data.as_ref().and_then(|data| data.status.as_deref()) != Some("success") {
            observe(envelope.observation.as_ref().expect("HTTP check has original response evidence"))?;
            return Ok(Ok(None));
        }
        Ok(Self::original_task_outcome(task_id, map_document(envelope).map(Some)))
    }

    pub async fn poll_document_with_observer(&self, task_id: &str, original_deadline: chrono::DateTime<chrono::Utc>,
        mut observe: impl FnMut(&OxrResponseObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<Result<OxrDocument, OxrError>> {
        let remaining = (original_deadline - chrono::Utc::now()).to_std().unwrap_or_default();
        let Some(deadline) = tokio::time::Instant::now().checked_add(remaining.min(self.poll_timeout)) else {
            return Ok(Self::original_task_outcome(task_id, Err(OxrError::PollTimeout(self.poll_timeout))));
        };
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(Self::original_task_outcome(task_id, Err(OxrError::PollTimeout(self.poll_timeout))));
            }
            let checked = match tokio::time::timeout_at(deadline,
                self.check_document_with_observer(task_id, &mut observe)).await {
                Ok(checked) => checked?,
                Err(_) => return Ok(Self::original_task_outcome(task_id, Err(OxrError::PollTimeout(self.poll_timeout)))),
            };
            match checked {
                Ok(Some(document)) => return Ok(Ok(document)),
                Err(error) => return Ok(Err(error)),
                Ok(None) => {},
            }
            let next = tokio::time::Instant::now().checked_add(self.poll_interval).unwrap_or(deadline).min(deadline);
            tokio::time::sleep_until(next).await;
        }
    }

    fn original_task_outcome<T>(task_id: &str, result: Result<T, OxrError>) -> Result<T, OxrError> {
        match result {
            Err(error) if !error.is_terminal_task_failure() => {
                let reason = error.to_string();
                let observation = match error {
                    OxrError::Observed { observation, .. } => Some(observation),
                    OxrError::TaskUnknown { observation, .. } => observation,
                    _ => None,
                };
                Err(OxrError::TaskUnknown { task_id: task_id.to_owned(), reason, observation })
            }
            result => result,
        }
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
        bytes: Vec<u8>,
    ) -> Result<reqwest::multipart::Form, OxrError> {
        let part = reqwest::multipart::Part::bytes(bytes)
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
        let mut response = self.http.post(format!("{}{path}", self.base))
            .multipart(form).send().await
            .map_err(|error| OxrError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE)
            .map(reqwest::header::HeaderValue::as_bytes);
        let withheld_content_type = content_type.is_some_and(|value| self.response_has_credential(value));
        let mut observation = OxrResponseObservation {
            body: None,
            declared_content_type: content_type.filter(|_| !withheld_content_type)
                .map(<[u8]>::to_vec),
            capture_complete: false,
            source: match path {
                "/parse/sync" => OxrResponseSource::Sync,
                "/parse/async" => OxrResponseSource::AsyncSubmit,
                "/parse/async_check" => OxrResponseSource::AsyncCheck,
                _ => unreachable!("only documented parse endpoints are used"),
            },
            http_status: Some(status.as_u16()),
        };
        let observed_error = |error, observation| self.observed_error(error, observation, withheld_content_type);
        if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BYTES) {
            return Err(observed_error(OxrError::MalformedResponse(format!(
                "an answer below {MAX_RESPONSE_BYTES} bytes (Content-Length was {:?})", response.content_length()
            )), observation));
        }
        let mut body = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    observation.body = (!body.is_empty()).then_some(body);
                    return Err(observed_error(OxrError::Transport(error.without_url().to_string()), observation));
                }
            };
            let remaining = MAX_RESPONSE_BYTES as usize - body.len();
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                observation.body = Some(body);
                return Err(observed_error(OxrError::MalformedResponse(format!(
                    "an answer below {MAX_RESPONSE_BYTES} bytes (body exceeded the limit)"
                )), observation));
            }
            body.extend_from_slice(&chunk);
        }
        observation.capture_complete = true;
        observation.body = Some(body);
        let body = observation.body.as_deref().expect("complete HTTP body captured");
        if !status.is_success() {
            let error = if let Ok(envelope) = serde_json::from_slice::<OxrEnvelope>(body)
                && let Some(code) = envelope.code.filter(|code| *code != 0)
            {
                OxrError::Upstream { code, message: self.redact(envelope.message.unwrap_or_default()) }
            } else { OxrError::HttpStatus { status: status.as_u16() } };
            return Err(observed_error(error, observation));
        }
        let mut envelope: OxrEnvelope = match serde_json::from_slice(body) {
            Ok(envelope) => envelope,
            Err(_) => return Err(observed_error(OxrError::MalformedResponse("a JSON envelope".into()), observation)),
        };
        let Some(code) = envelope.code else {
            return Err(observed_error(OxrError::MalformedResponse("the envelope `code`".into()), observation));
        };
        if code != 0 {
            return Err(observed_error(OxrError::Upstream {
                code, message: self.redact(envelope.message.unwrap_or_default()),
            }, observation));
        }
        // A success with withheld response bytes needs reconciliation. A
        // refusal keeps its original typed cause and any safe body bytes.
        if withheld_content_type || self.response_has_credential(body) {
            return Err(observed_error(OxrError::MalformedResponse(
                "credential-bearing response evidence withheld".into()), observation));
        }
        envelope.observation = Some(observation);
        Ok(envelope)
    }

    fn response_has_credential(&self, body: &[u8]) -> bool {
        let secret = self.credential.expose();
        let contains = |needle: &[u8]| !needle.is_empty()
            && body.windows(needle.len()).any(|part| part == needle);
        if secret.is_empty() { return false; }
        if contains(secret.as_bytes()) { return true; }
        if !secret.bytes().any(|byte| byte < 0x20 || matches!(byte, b'"' | b'\\')) {
            return false;
        }
        serde_json::to_string(secret).ok().as_deref()
            .and_then(|encoded| encoded.strip_prefix('"'))
            .and_then(|encoded| encoded.strip_suffix('"'))
            .is_some_and(|encoded| contains(encoded.as_bytes()))
    }

    fn observed_error(&self, error: OxrError, mut observation: OxrResponseObservation,
        withheld_content_type: bool) -> OxrError {
        let withheld_body = observation.body.as_deref().is_some_and(|body| self.response_has_credential(body));
        if withheld_body {
            observation.body = None;
            observation.capture_complete = false;
        }
        let error = if withheld_content_type || withheld_body {
            OxrError::CredentialEchoWithheld { cause: Box::new(error) }
        } else { error };
        error.observed(observation)
    }

    fn redact(&self, message: String) -> String {
        let credential = self.credential.expose();
        if credential.is_empty() { message } else { message.replace(credential, "***") }
    }

    /// Every check and sleep shares the original durable deadline. Reopen
    /// cannot create a fresh waiting interval for an accepted computation.
    async fn poll(&self, task_id: &str, original_deadline: chrono::DateTime<chrono::Utc>) -> Result<OxrEnvelope, OxrError> {
        let remaining = (original_deadline - chrono::Utc::now()).to_std()
            .map_err(|_| OxrError::PollTimeout(self.poll_timeout))?;
        let deadline = tokio::time::Instant::now().checked_add(remaining.min(self.poll_timeout))
            .ok_or_else(|| OxrError::PollTimeout(self.poll_timeout))?;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(OxrError::PollTimeout(self.poll_timeout));
            }
            let envelope = tokio::time::timeout_at(deadline, self.check(task_id)).await
                .map_err(|_| OxrError::PollTimeout(self.poll_timeout))??;
            if tokio::time::Instant::now() >= deadline {
                return Err(match envelope {
                    Some(envelope) => observed_envelope_error(envelope, OxrError::PollTimeout(self.poll_timeout)),
                    None => OxrError::PollTimeout(self.poll_timeout),
                });
            }
            if let Some(envelope) = envelope { return Ok(envelope); }
            let next = tokio::time::Instant::now().checked_add(self.poll_interval).unwrap_or(deadline).min(deadline);
            tokio::time::sleep_until(next).await;
        }
    }

    async fn check(&self, task_id: &str) -> Result<Option<OxrEnvelope>, OxrError> {
        let envelope = self.check_envelope(task_id).await?;
        Ok((envelope.data.as_ref().and_then(|data| data.status.as_deref()) == Some("success")).then_some(envelope))
    }

    async fn check_envelope(&self, task_id: &str) -> Result<OxrEnvelope, OxrError> {
        if task_id.trim().is_empty() || task_id.len() > 4096 {
            return Err(OxrError::MalformedResponse("the original async task identity".into()));
        }
        let form = reqwest::multipart::Form::new()
            .text("task_id", task_id.to_owned())
            .text("api_key", self.credential.expose().to_owned());
        let envelope = self.post("/parse/async_check", form).await?;
        let Some(data) = envelope.data.as_ref() else {
            return Err(observed_envelope_error(envelope,
                OxrError::MalformedResponse("`data` from the async check".into())));
        };
        if data.task_id.as_deref() != Some(task_id) {
            return Err(observed_envelope_error(envelope,
                OxrError::MalformedResponse("the original async task identity".into())));
        }
        match data.status.as_deref() {
            Some("success") => Ok(envelope),
            Some("failed") => {
                let reason = data.error.as_deref().filter(|reason| !reason.is_empty())
                    .unwrap_or("upstream reported the task as failed").to_owned();
                Err(observed_envelope_error(envelope, OxrError::TaskFailed(self.redact(reason))))
            }
            Some("queuing") | Some("processing") => Ok(envelope),
            Some(_) => Err(observed_envelope_error(envelope,
                OxrError::MalformedResponse("a known async task status".into()))),
            None => Err(observed_envelope_error(envelope,
                OxrError::MalformedResponse("`data.status` from the async check".into()))),
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
    #[serde(skip)]
    observation: Option<OxrResponseObservation>,
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
    // An omitted or null optional count is unreported. Non-null counts must
    // be u32 and match the full validated array; page-index null stays invalid.
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
    page_num: OxrPageIndex,
    #[serde(default)]
    blocks: Vec<OxrBlockDto>,
    #[serde(default)]
    page_size: Option<OxrPageSizeDto>,
}

#[derive(Debug, Deserialize)]
struct OxrPageSizeDto {
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
}

#[derive(Debug, Default)]
enum OxrPageIndex {
    #[default]
    Omitted,
    Explicit(u32),
    Malformed,
}

/// Keep malformed page data separate from the task status. Only a successful
/// document result consumes the page identity.
fn explicit_page_index<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<OxrPageIndex, D::Error> {
    Ok(serde_json::Value::deserialize(deserializer)?
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .map_or(OxrPageIndex::Malformed, OxrPageIndex::Explicit))
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
    #[serde(default)]
    img_url: Option<String>,
}

fn observed_envelope_error(mut envelope: OxrEnvelope, error: OxrError) -> OxrError {
    match envelope.observation.take() {
        Some(observation) => error.observed(observation),
        None => error,
    }
}

fn map_document(mut envelope: OxrEnvelope) -> Result<OxrDocument, OxrError> {
    let observation = envelope.observation.take();
    match map_document_output(envelope) {
        Ok(mut document) => {
            document.raw_response = observation.and_then(|observation| observation.body)
                .expect("a mapped HTTP or saved response carries its actual bytes");
            Ok(document)
        }
        Err(error) => Err(match observation {
            Some(observation) => error.observed(observation),
            None => error,
        }),
    }
}

fn map_document_output(envelope: OxrEnvelope) -> Result<OxrDocument, OxrError> {
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
    let reported_pages = json.pages
        .ok_or_else(|| OxrError::MalformedResponse("`data.result.outputs.json.pages`".into()))?;
    let observed_count = u32::try_from(reported_pages.len())
        .map_err(|_| OxrError::MalformedResponse("a representable original page inventory".into()))?;
    let page_count = data.metadata.and_then(|metadata| metadata.page_num);
    if page_count.is_some_and(|count| count != observed_count) {
        return Err(OxrError::MalformedResponse("a full original page inventory matching `metadata.page_num`".into()));
    }
    // Upstream returns a whole document, never a requested-page subset. A
    // missing optional page_num uses its zero-based array position. An
    // explicitly malformed index refuses the successful document result.
    // With no metadata count, the full array still defines the zero-based
    // domain. Unique in-range identities cover that domain exactly.
    let mut seen = vec![false; reported_pages.len()];
    let pages = reported_pages
        .into_iter()
        .enumerate()
        .map(|(position, page)| {
            let page_num = match page.page_num {
                OxrPageIndex::Omitted => position as u32,
                OxrPageIndex::Explicit(number) => number,
                OxrPageIndex::Malformed => {
                    return Err(OxrError::MalformedResponse("an unsigned integer `pages[].page_num`".into()));
                }
            };
            let slot = seen.get_mut(page_num as usize)
                .ok_or_else(|| OxrError::MalformedResponse("original page indices within the full zero-based inventory".into()))?;
            if *slot {
                return Err(OxrError::MalformedResponse("unique original page indices".into()));
            }
            *slot = true;
            Ok(map_page(page, page_num))
        })
        .collect::<Result<Vec<_>, OxrError>>()?;
    Ok(OxrDocument {
        file_name: result.file_name,
        page_count,
        warnings: envelope.warnings,
        pages,
        raw_response: Vec::new(),
    })
}

fn map_page(page: OxrPageDto, page_num: u32) -> OxrParsedPage {
    let page_size = page
        .page_size
        .and_then(|size| match (size.w, size.h) {
            // A half-reported or degenerate size is no size at all: without both
            // dimensions the output page rectangle is not declared. A valid
            // rectangle still does not establish any source transform.
            (Some(width), Some(height)) => {
                let (width, height) = (width as f32, height as f32);
                (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
                    .then_some(PageSize { width, height })
            }
            _ => None,
        });
    let blocks = page
        .blocks
        .into_iter()
        .enumerate()
        .map(|(index, block)| map_block(block, index as u32))
        .collect();
    OxrParsedPage {
        page_num,
        blocks,
        page_size,
    }
}

fn map_block(block: OxrBlockDto, position: u32) -> PageBlock {
    let structure_ref = structure_ref_of(&block);
    PageBlock {
        // Upstream numbers blocks in reading order itself; when it omits the
        // index, the array position is the reading order it documents.
        order: block.idx.unwrap_or(position),
        text: block.content.unwrap_or_default(),
        bbox: block.bbox.as_deref().and_then(bbox_of),
        structure_ref,
        element_type: non_empty(block.element_type.as_deref()),
        content_format: non_empty(block.format.as_deref()),
        image_resource: block.img_url.filter(|url| !url.trim().is_empty())
            .map(|provider_url| PageImageResource::TransientUnretained { provider_url }),
    }
}

/// `[x1, y1, x2, y2]` and nothing else: a wrong-length box or coordinates
/// unrepresentable as finite f32 values are unknown, never a repaired guess.
fn bbox_of(values: &[f64]) -> Option<[f32; 4]> {
    let [x1, y1, x2, y2]: [f64; 4] = values.try_into().ok()?;
    let narrowed = [x1 as f32, y1 as f32, x2 as f32, y2 as f32];
    narrowed.iter().all(|value| value.is_finite()).then_some(narrowed)
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
