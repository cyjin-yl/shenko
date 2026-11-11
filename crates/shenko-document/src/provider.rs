//! Document-parse providers: a replaceable boundary plus the first named
//! provider, SoMarkAI/OXR. A provider is only ever *selected* — never
//! substituted: when the named provider cannot actually run, the boundary
//! reports `ProviderUnavailable` with the honest reason, never a different
//! engine's output.

use crate::oxr_client::{
    CredentialHandle, DEFAULT_POLL_INTERVAL, DEFAULT_POLL_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    OxrClient, OxrClientConfig, OxrDocument, OxrError, OxrTransport,
};
use crate::types::{PageProvenance, PageRef, PageResult, ParseRequest};
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
    /// deliberately not a terminal failure — the page is retryable under the
    /// same cache key, but the caller has to reconcile the unknown first.
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
    /// The number of pages the provider can serve for this document, when it
    /// can determine the count without guessing. `None` = unknown; the
    /// boundary never fabricates a page count. The default reports unknown.
    async fn page_count(&self, source_bytes: &[u8], req: &ParseRequest) -> Result<Option<u32>> {
        let _ = (source_bytes, req);
        Ok(None)
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
pub struct OxrProvider {
    endpoint: Option<String>,
    model_identity: Option<String>,
    scope: Option<String>,
    credential: Option<CredentialHandle>,
    engine: OxrEngineBinding,
    /// The real HTTP client when the endpoint and credential bind; `Err`
    /// keeps the rejection reason so `availability()` can name it.
    client: Result<OxrClient, String>,
    params_digest: Option<String>,
    /// Whole-document parses shared across one request's pages. Upstream has
    /// no per-page endpoint and the hosted API is QPS-limited, so one upload
    /// per document is the only honest pattern: concurrent `parse_page`
    /// calls on the same source share the same in-flight parse.
    documents: Mutex<HashMap<String, Arc<OnceCell<Arc<OxrDocument>>>>>,
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
        let params_digest = Some(params_digest_of(&config));
        Self {
            endpoint: config.endpoint,
            model_identity: config.model_identity,
            scope: config.scope,
            credential: config.credential,
            engine: config.engine,
            client,
            params_digest,
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

    /// The document parse shared across this source's pages. One upload per
    /// (source digest, closure, byte length): the byte length joins the key
    /// so a caller that misbinds a digest to different bytes does not share a
    /// parse it must not share.
    async fn document_for(
        &self,
        client: &OxrClient,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<Arc<OxrDocument>, OxrError> {
        let key = format!(
            "{}\u{1f}{}\u{1f}{}",
            req.source_digest,
            self.closure_id(),
            source_bytes.len()
        );
        let cell = {
            let mut documents = self.documents.lock().await;
            documents
                .entry(key)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        // `source_name` is the upload name: upstream infers the document type
        // from it. A failure leaves the cell unset, so the next page retries
        // rather than inheriting a cached failure.
        let file_name = req
            .source_name
            .clone()
            .unwrap_or_else(|| "document".to_owned());
        let document = cell
            .get_or_try_init(|| async {
                client
                    .parse_document(&file_name, source_bytes)
                    .await
                    .map(Arc::new)
            })
            .await?;
        Ok(Arc::clone(document))
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

    async fn page_count(&self, source_bytes: &[u8], req: &ParseRequest) -> Result<Option<u32>> {
        let client = match &self.client {
            Ok(client) => client,
            Err(reason) if reason.is_empty() => {
                anyhow::bail!("OXR provider is not bound to a usable endpoint")
            }
            Err(reason) => {
                anyhow::bail!("OXR provider is not bound to a usable endpoint: {reason}")
            }
        };
        // The count comes from the same shared document parse the pages use:
        // `metadata.page_num` is upstream's own statement, and the page list
        // is the fallback when upstream did not report it.
        let document = self.document_for(client, source_bytes, req).await?;
        Ok(document
            .page_count
            .or_else(|| u32::try_from(document.pages.len()).ok()))
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
            Err(error) if error.is_retryable() => {
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
            params_digest: self.params_digest.clone(),
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
    let engine = match &config.engine {
        OxrEngineBinding::Remote => "remote".to_owned(),
        OxrEngineBinding::Local { attestation } => {
            format!("local:attested={}", attestation.is_some())
        }
    };
    let canonical = format!(
        "document.parse/oxr-params/v1;transport={:?};output_formats=json;\
         element_formats=upstream-default;feature_config=upstream-default;engine={engine}",
        config.transport,
    );
    format!("blake3:{}", blake3::hash(canonical.as_bytes()).to_hex())
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
