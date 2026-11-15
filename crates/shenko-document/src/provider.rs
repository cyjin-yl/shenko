//! Document-parse providers: a replaceable boundary plus the first named
//! provider, SoMarkAI/OXR. A provider is only ever *selected* — never
//! substituted: when the named provider cannot actually run, the boundary
//! reports `ProviderUnavailable` with the honest reason, never a different
//! engine's output.

use crate::attempt::{OxrAttemptBinding, OxrAttemptClaim, OxrAttemptContext, OxrAttemptReceipt};
use crate::oxr_client::{
    CredentialHandle, DEFAULT_POLL_INTERVAL, DEFAULT_POLL_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    OxrClient, OxrClientConfig, OxrDocument, OxrError, OxrSubmission, OxrTransport,
};
use crate::types::{
    DOCUMENT_PARSE_DEFINITION_VERSION, PageProvenance, PageRef, PageResult, ParseRequest,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

/// What a provider can do for one page of one document.
#[derive(Debug)]
pub enum PageOutcome {
    /// The provider produced a real page result.
    Parsed(Box<PageResult>),
    /// The provider definitively could not parse this page (not transient:
    /// upstream understood and refused, or the page does not exist).
    PageFailed(String),
    /// The page's outcome is UNKNOWN: the request may or may not have been
    /// processed upstream (transport failure, rate limit, lost task). This is
    /// deliberately not a terminal failure. The caller must reconcile the
    /// original attempt; this does not authorize another upload.
    Unknown(String),
}

/// Whole-document discovery has the same outcome distinctions as a page.
/// An unknown external effect must survive before any page count is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageCountOutcome {
    Known(u32),
    Undetermined,
    Failed(String),
    Unknown(String),
}

/// A parse provider: bound to a closure, reports its real capability.
#[async_trait::async_trait]
pub trait DocumentProvider: Send + Sync {
    /// Stable provider identity, recorded as page provenance.
    fn provider_id(&self) -> &str;
    /// The closure identity this provider instance was bound under.
    fn closure_id(&self) -> &str;
    /// Whether this provider can actually run in this environment RIGHT NOW
    /// (endpoint bound and reachable, model identity bound, credential bound,
    /// engine prerequisites attested). The honest answer; absence is not an
    /// error to hide.
    async fn availability(&self) -> ProviderAvailability;
    /// Request-scoped readiness may serve committed results while the engine
    /// is offline. Providers without durable results use live availability.
    async fn availability_for(
        &self,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> ProviderAvailability {
        let _ = (source_bytes, req);
        self.availability().await
    }
    /// The number of pages the provider can serve for this document, when it
    /// can determine the count without guessing. Undetermined means no count
    /// is available; Unknown means an external parse may have executed.
    async fn page_count(
        &self,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<PageCountOutcome> {
        let _ = (source_bytes, req);
        Ok(PageCountOutcome::Undetermined)
    }
    /// Parse one page. Implementations must not be called unless
    /// `availability()` is `Ready`; the boundary checks first.
    async fn parse_page(
        &self,
        source_bytes: &[u8],
        page: PageRef,
        req: &ParseRequest,
    ) -> Result<PageOutcome>;
    /// The provider's own page-level concurrency ceiling, used when the
    /// caller does not set an explicit bound (the hosted OXR API is limited
    /// to QPS 4 per account).
    fn concurrency_limit(&self) -> usize {
        crate::DEFAULT_PARSE_CONCURRENCY
    }
    /// The provider/model/config evidence a produced page records.
    fn provenance(&self) -> PageProvenance {
        PageProvenance {
            provider_id: self.provider_id().into(),
            code_version: self.closure_id().into(),
            model_version: None,
            params_digest: None,
            raw_output_ref: None,
        }
    }
}

/// The honest availability a provider reports.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAvailability {
    /// Endpoint, model, credential and engine prerequisites are all bound and
    /// the endpoint answered the reachability probe.
    Ready,
    /// Cannot run; the reason names the missing prerequisite precisely.
    Unavailable { reason: String },
}

/// Where the OXR engine that serves an endpoint runs. This client never
/// probes hardware: it cannot see a remote deployment's GPU and does not
/// pretend to. For a local deployment the deployer's attestation is the
/// evidence, because an NVIDIA GPU with BF16 tensor cores (SM80+) plus a
/// CUDA-13 driver is a hard OXR prerequisite that cannot be assumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OxrEngineBinding {
    /// The endpoint is a remote service (hosted or another machine); the
    /// engine's runtime prerequisites are that deployment's own concern.
    #[default]
    Remote,
    /// The endpoint is served by an OXR engine on this host. The deployer
    /// must bind an attestation of the engine's runtime prerequisites —
    /// without one the provider reports Unavailable instead of assuming the
    /// GPU and driver are there.
    Local { attestation: Option<String> },
}

/// Everything needed to bind one OXR deployment.
#[derive(Debug, Clone, Default)]
pub struct OxrProviderConfig {
    /// The upstream base URL (`http://host:8000/v1` or
    /// `https://…/api/v1`). `None` = the provider is not provisioned.
    pub endpoint: Option<String>,
    /// The model identity the deployment serves (e.g. `OXR-1.0`). A page's
    /// provenance names exactly this; without it the provider cannot state
    /// which model produced its pages, so it reports Unavailable.
    pub model_identity: Option<String>,
    /// The execution scope the results are scoped to.
    pub scope: Option<String>,
    /// The upstream API key. Upstream requires the `api_key` field even when
    /// it is not validated (self-hosted accepts any value), so a missing
    /// credential is a real missing prerequisite.
    pub credential: Option<CredentialHandle>,
    /// Where the engine that serves the endpoint runs.
    pub engine: OxrEngineBinding,
    /// Which upstream transport to use.
    pub transport: OxrTransport,
    pub request_timeout: Option<Duration>,
    pub poll_interval: Option<Duration>,
    pub poll_timeout: Option<Duration>,
    /// The host's original durable operation/attempt and receipt adapter.
    /// Without this binding an upload cannot be made safely across restart.
    pub attempt: Option<OxrAttemptContext>,
}

/// The SoMarkAI/OXR provider (Optical Everything Recognition). OXR is an
/// HTTP service: it turns PDF/document images into Markdown/JSON with layout
/// coordinates through `POST /parse/sync` or `/parse/async` +
/// `/parse/async_check`. Its serving side requires an NVIDIA GPU with BF16
/// tensor cores and a CUDA-13-compatible driver.
///
/// The provider binds the real upstream contract — endpoint, model identity,
/// scope, credential — and reports an honest Unavailable listing exactly the
/// missing prerequisite when it cannot run, per issue #89: an absent
/// capability is reported, never auto-substituted.
type SharedDocument = Arc<OnceCell<Result<Arc<OxrDocument>, OxrError>>>;

pub struct OxrProvider {
    endpoint: Option<String>,
    model_identity: Option<String>,
    scope: Option<String>,
    credential: Option<CredentialHandle>,
    engine: OxrEngineBinding,
    /// The real HTTP client when the endpoint and credential bind; `Err`
    /// keeps the rejection reason so `availability()` can name it.
    client: Result<OxrClient, String>,
    params_digest: String,
    attempt: Option<OxrAttemptContext>,
    /// Whole-document parses shared across one request's pages. Upstream has
    /// no per-page endpoint and the hosted API is QPS-limited, so one upload
    /// per document is the only honest pattern: concurrent `parse_page`
    /// calls on the same source share the same in-flight parse.
    documents: Mutex<HashMap<String, SharedDocument>>,
}

impl OxrProvider {
    /// Bind an OXR provider to an endpoint + model identity + scope +
    /// credential. A `None` in any of them means the provider is not
    /// provisioned: it reports Unavailable naming what is missing rather
    /// than pretending to be ready.
    pub fn new(config: OxrProviderConfig) -> Self {
        let client = match (&config.endpoint, &config.credential) {
            (Some(endpoint), Some(credential)) => OxrClient::new(OxrClientConfig {
                endpoint: endpoint.clone(),
                credential: credential.clone(),
                transport: config.transport,
                request_timeout: config.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
                poll_interval: config.poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
                poll_timeout: config.poll_timeout.unwrap_or(DEFAULT_POLL_TIMEOUT),
            })
            .map_err(|error| error.to_string()),
            _ => Err(String::new()),
        };
        let params_digest = params_digest_of(&config);
        Self {
            endpoint: config.endpoint,
            model_identity: config.model_identity,
            scope: config.scope,
            credential: config.credential,
            engine: config.engine,
            client,
            params_digest,
            attempt: config.attempt,
            documents: Mutex::new(HashMap::new()),
        }
    }

    /// The execution scope this provider is bound to (permission boundary
    /// the results are scoped to; `None` = unscoped). Results from a
    /// different scope never mix — this is part of the closure identity.
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// The real capability, naming the FIRST missing prerequisite.
    /// Reachability is probed last, only after every binding check passed,
    /// so a probe never fires without a complete configuration.
    async fn unavailable_reason(&self) -> Option<String> {
        if self.endpoint.is_none() {
            return Some("no OXR endpoint bound (endpoint is None)".into());
        }
        if self.model_identity.is_none() {
            return Some("no OXR model identity bound (model_identity is None)".into());
        }
        if self.credential.is_none() {
            return Some("no OXR credential bound (credential is None)".into());
        }
        // Endpoint, model and credential are all bound at this point, so a
        // client error is a genuine binding rejection, never an empty shadow
        // of a missing field.
        if let Err(reason) = &self.client {
            return Some(format!("OXR endpoint rejected at binding: {reason}"));
        }
        if let OxrEngineBinding::Local { attestation: None } = &self.engine {
            return Some(
                "local OXR endpoint bound without an engine attestation (OXR requires \
                 an NVIDIA GPU with BF16 tensor cores and a CUDA-13 driver)"
                    .into(),
            );
        }
        match &self.attempt {
            None => return Some("no OXR durable operation/attempt receipt store bound".into()),
            Some(attempt) if attempt.account_identity.trim().is_empty() => {
                return Some("no OXR account identity bound".into());
            }
            Some(_) => {}
        }
        let client = self
            .client
            .as_ref()
            .expect("client bound when endpoint and credential are");
        match client.probe().await {
            Ok(()) => None,
            Err(error) => Some(format!(
                "OXR endpoint {} is unreachable: {error}",
                client.endpoint()
            )),
        }
    }

    /// Share both results and failures between pages. Cancellation can leave
    /// the local cell empty, but the host's durable claim remains armed and
    /// prevents another upload after drop, provider recreation or restart.
    async fn document_for(
        &self,
        client: &OxrClient,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<Arc<OxrDocument>, OxrError> {
        let attempt = self
            .attempt
            .as_ref()
            .ok_or_else(|| OxrError::Receipt("no durable attempt binding".into()))?;
        let binding = self.attempt_binding(source_bytes, req)?;
        let cell = {
            let mut documents = self.documents.lock().await;
            documents
                .entry(
                    binding
                        .digest()
                        .map_err(|error| OxrError::Request(error.to_string()))?,
                )
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        cell.get_or_init(|| async {
            self.run_attempt(client, source_bytes, attempt, &binding)
                .await
                .map(Arc::new)
        })
        .await
        .clone()
    }

    /// Freeze the exact upload identity without contacting the endpoint. A
    /// native host seals this in its T1 checkpoint at the dispatch claim.
    pub fn attempt_binding(
        &self,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<OxrAttemptBinding, OxrError> {
        let client = self
            .client
            .as_ref()
            .map_err(|reason| OxrError::Request(reason.clone()))?;
        let attempt = self
            .attempt
            .as_ref()
            .ok_or_else(|| OxrError::Receipt("no durable attempt binding".into()))?;
        if req.schema_version != DOCUMENT_PARSE_DEFINITION_VERSION
            || req.closure_id != self.closure_id()
            || req.scope != self.scope
        {
            return Err(OxrError::Request(
                "request schema, closure or scope differs from the bound provider".into(),
            ));
        }
        Ok(OxrAttemptBinding {
            operation_id: attempt.operation_id,
            attempt_id: attempt.attempt_id,
            account_identity: attempt.account_identity.clone(),
            source_digest: req.source_digest.clone(),
            source_bytes_digest: format!("blake3:{}", blake3::hash(source_bytes).to_hex()),
            source_name: req.source_name.clone().unwrap_or_else(|| "document".into()),
            request_schema_version: req.schema_version,
            closure_id: self.closure_id().into(),
            scope: self.scope.clone(),
            endpoint: client.endpoint().into(),
            model_identity: self
                .model_identity
                .clone()
                .ok_or_else(|| OxrError::Request("no model identity bound".into()))?,
            params_digest: self.params_digest.clone(),
        })
    }

    async fn run_attempt(
        &self,
        client: &OxrClient,
        source_bytes: &[u8],
        attempt: &OxrAttemptContext,
        binding: &OxrAttemptBinding,
    ) -> Result<OxrDocument, OxrError> {
        let claim = attempt
            .store
            .claim(binding)
            .map_err(|error| OxrError::Receipt(error.to_string()))?;
        let task = match claim {
            OxrAttemptClaim::Dispatch => {
                match client
                    .submit_document(&binding.source_name, source_bytes)
                    .await
                {
                    Ok(OxrSubmission::Accepted(task)) => {
                        attempt
                            .store
                            .append(
                                binding,
                                &OxrAttemptReceipt {
                                    task: Some(task.clone()),
                                    outcome: None,
                                },
                            )
                            .map_err(|error| OxrError::Receipt(error.to_string()))?;
                        task
                    }
                    result => {
                        let outcome = result.map(|submission| match submission {
                            OxrSubmission::Completed(document) => document,
                            OxrSubmission::Accepted(_) => unreachable!("handled above"),
                        });
                        attempt
                            .store
                            .append(
                                binding,
                                &OxrAttemptReceipt {
                                    task: None,
                                    outcome: Some(outcome.clone()),
                                },
                            )
                            .map_err(|error| OxrError::Receipt(error.to_string()))?;
                        return outcome;
                    }
                }
            }
            OxrAttemptClaim::Replay(Some(receipt)) => match (&receipt.outcome, receipt.task) {
                (Some(Ok(document)), _) => return Ok(document.clone()),
                (Some(Err(error)), _) if !error.outcome_is_unknown() => return Err(error.clone()),
                (_, Some(task)) => task,
                (Some(Err(error)), None) => return Err(error.clone()),
                _ => {
                    return Err(OxrError::OutcomeUnknown(
                        "armed upload has no usable receipt".into(),
                    ));
                }
            },
            OxrAttemptClaim::Replay(None) => {
                return Err(OxrError::OutcomeUnknown("original upload is already armed without a receipt; reconcile it, do not resend".into()));
            }
        };
        // A later reconciliation may check an expired task once. It never
        // invents a fresh polling budget or submits the document again.
        let outcome = if chrono::Utc::now() >= task.poll_deadline {
            client.check_document(&task).await.and_then(|document| {
                document.ok_or_else(|| {
                    OxrError::OutcomeUnknown(
                        "original polling deadline expired; remote task still pending".into(),
                    )
                })
            })
        } else {
            client.poll_document(&task).await
        };
        attempt
            .store
            .append(
                binding,
                &OxrAttemptReceipt {
                    task: Some(task),
                    outcome: Some(outcome.clone()),
                },
            )
            .map_err(|error| OxrError::Receipt(error.to_string()))?;
        outcome
    }
}

#[async_trait::async_trait]
impl DocumentProvider for OxrProvider {
    fn provider_id(&self) -> &str {
        "oxr"
    }

    fn closure_id(&self) -> &str {
        "oxr/1.0"
    }

    async fn availability(&self) -> ProviderAvailability {
        match self.unavailable_reason().await {
            Some(reason) => ProviderAvailability::Unavailable { reason },
            None => ProviderAvailability::Ready,
        }
    }

    async fn availability_for(
        &self,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> ProviderAvailability {
        if let (Ok(binding), Some(attempt)) =
            (self.attempt_binding(source_bytes, req), &self.attempt)
        {
            match attempt.store.receipt(&binding) {
                Ok(Some(receipt))
                    if receipt
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| match outcome {
                            Ok(_) => true,
                            Err(error) => !error.outcome_is_unknown(),
                        }) =>
                {
                    return ProviderAvailability::Ready;
                }
                Ok(_) => {}
                Err(error) => {
                    return ProviderAvailability::Unavailable {
                        reason: format!("OXR original receipt unavailable: {error}"),
                    };
                }
            }
        }
        self.availability().await
    }

    async fn page_count(
        &self,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<PageCountOutcome> {
        let client = self.client.as_ref().map_err(|reason| {
            anyhow::anyhow!("OXR provider is not bound to a usable endpoint: {reason}")
        })?;
        match self.document_for(client, source_bytes, req).await {
            Ok(document) => Ok(
                match document
                    .page_count
                    .or_else(|| u32::try_from(document.pages.len()).ok())
                {
                    Some(count) => PageCountOutcome::Known(count),
                    None => PageCountOutcome::Undetermined,
                },
            ),
            Err(error) if error.outcome_is_unknown() => {
                Ok(PageCountOutcome::Unknown(error.to_string()))
            }
            Err(error) => Ok(PageCountOutcome::Failed(error.to_string())),
        }
    }

    async fn parse_page(
        &self,
        source_bytes: &[u8],
        page: PageRef,
        req: &ParseRequest,
    ) -> Result<PageOutcome> {
        let client = match &self.client {
            Ok(client) => client,
            Err(reason) if reason.is_empty() => {
                // The boundary checks availability() before calling: reaching
                // here unbound is a contract violation, reported as an error,
                // not a fabricated page.
                anyhow::bail!("OXR provider parse_page called while unavailable")
            }
            Err(reason) => {
                anyhow::bail!("OXR provider parse_page called while unavailable: {reason}")
            }
        };
        let document = match self.document_for(client, source_bytes, req).await {
            Ok(document) => document,
            Err(error) if error.outcome_is_unknown() => {
                return Ok(PageOutcome::Unknown(format!(
                    "page {} outcome unknown: {error}",
                    page.page_index
                )));
            }
            Err(error) => {
                return Ok(PageOutcome::PageFailed(format!(
                    "page {} failed: {error}",
                    page.page_index
                )));
            }
        };
        match document
            .pages
            .iter()
            .find(|parsed| parsed.page_num == page.page_index)
        {
            Some(parsed) => Ok(PageOutcome::Parsed(Box::new(PageResult {
                page,
                source_digest: req.source_digest.clone(),
                blocks: parsed.blocks.clone(),
                provenance: self.provenance(),
                // Bboxes only mean "restored to the original page" when the
                // page's own coordinate space is known; without a page_size
                // the coordinates have no declared space.
                coordinates_restored: parsed.page_size.is_some(),
                page_size: parsed.page_size,
            }))),
            None => {
                let reported = document
                    .page_count
                    .unwrap_or_else(|| document.pages.len() as u32);
                Ok(PageOutcome::PageFailed(format!(
                    "upstream returned no page {} (the document has {reported} page(s))",
                    page.page_index
                )))
            }
        }
    }

    fn concurrency_limit(&self) -> usize {
        // The hosted OXR API is limited to QPS 4 per account; a self-hosted
        // deployment can go higher, which the caller expresses by overriding
        // `ParseOptions::max_concurrency`.
        4
    }

    fn provenance(&self) -> PageProvenance {
        PageProvenance {
            provider_id: self.provider_id().into(),
            code_version: self.closure_id().into(),
            model_version: self.model_identity.clone(),
            params_digest: Some(self.params_digest.clone()),
            // This crate does not retain the raw upstream body in a CAS; the
            // caller seals the mapped result, so no raw-output ref is claimed.
            raw_output_ref: None,
        }
    }
}

/// A deterministic digest of the provider's effective parameters (transport,
/// requested outputs, engine binding), evidence for what produced the pages.
/// The credential and endpoint are deliberately not inputs: the digest names
/// the parameter set, not the deployment.
fn params_digest_of(config: &OxrProviderConfig) -> String {
    let canonical = serde_json::json!({
        "schema": "document.parse/oxr-params/v2",
        "transport": format!("{:?}", config.transport),
        "output_formats": "json", "element_formats": "upstream-default",
        "feature_config": "upstream-default", "engine": format!("{:?}", config.engine),
        "request_timeout": config.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
        "poll_timeout": config.poll_timeout.unwrap_or(DEFAULT_POLL_TIMEOUT),
        "poll_interval": config.poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
    });
    let bytes = canonical.to_string().into_bytes();
    format!("blake3:{}", blake3::hash(&bytes).to_hex())
}

/// Select the provider for a parse request's closure. Only the named
/// provider is returned; if it is not actually provisioned the selection
/// still yields the named provider so the caller sees its honest
/// `Unavailable` rather than a silent substitute.
pub fn select_provider(
    closure_id: &str,
    providers: &[Arc<dyn DocumentProvider>],
) -> Option<Arc<dyn DocumentProvider>> {
    providers
        .iter()
        .find(|provider| provider.closure_id() == closure_id)
        .cloned()
}

/// The provenance a real page records. Provider ids are evidence — a result
/// only ever names the provider that actually produced it.
pub fn page_provenance(provider: &dyn DocumentProvider) -> PageProvenance {
    provider.provenance()
}
