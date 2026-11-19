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
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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
    /// Exact execution scope bound to this provider. `None` is unscoped,
    /// not a wildcard or permission grant; it only matches request `None`.
    /// An empty scope string is distinct from an absent scope.
    fn scope(&self) -> Option<&str>;
    /// Binding/reachability preflight. OXR checks configured model/credential
    /// and local attestation presence, then HTTP reachability. Ready does not
    /// prove credential acceptance, model identity/readiness or recognition.
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
    /// The provider's local page-future ceiling when the caller does not
    /// set an explicit bound. This does not enforce requests per second.
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
/// pretend to. A local binding records the deployer's attestation of its
/// actual runtime. Upstream's documented CUDA route calls for BF16 support
/// and a compatible driver; this string does not verify either prerequisite
/// or rule out other upstream-supported configurations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OxrEngineBinding {
    /// The endpoint is a remote service (hosted or another machine); the
    /// engine's runtime prerequisites are that deployment's own concern.
    Remote {},
    /// The endpoint is served by an OXR engine on this host. The deployer
    /// must bind an attestation of the engine's runtime prerequisites —
    /// without one the provider reports Unavailable instead of assuming the
    /// GPU and driver are there.
    Local { attestation: Option<String> },
}

impl Default for OxrEngineBinding {
    fn default() -> Self { Self::Remote {} }
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

/// Non-secret deployment identity retained with the original attempt. A new
/// endpoint, model, effective configuration or scope is a different closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OxrProviderBinding {
    pub endpoint: Option<String>,
    pub model_identity: Option<String>,
    pub scope: Option<String>,
    pub params_digest: String,
    pub code_digest: String,
    pub transport: OxrTransport,
}

type DocumentResult = Result<Arc<OxrDocument>, OxrError>;
type DocumentSlot = Arc<Mutex<Option<DocumentResult>>>;

/// The SoMarkAI/OXR provider (Optical Everything Recognition). OXR is an
/// HTTP service: it turns PDF/document images into Markdown/JSON with layout
/// coordinates through `POST /parse/sync` or `/parse/async` +
/// `/parse/async_check`. Binding and HTTP reachability do not prove a genuine
/// OXR installation, a ready model or successful recognition.
///
/// This generic adapter shares one in-memory attempt across page futures.
/// It exposes neither the accepted-handle persistence boundary nor restart
/// recovery. The native document task owner uses the split client API and
/// the Store instead; dropping/recreating this adapter is not durable resume.
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
    binding: OxrProviderBinding,
    closure_identity: String,
    /// Whole-document parses shared across one request's pages. Upstream has
    /// no per-page endpoint and the hosted API is QPS-limited, so one upload
    /// per document is the only honest pattern: concurrent `parse_page`
    /// calls on the same source share the same in-flight parse.
    documents: Mutex<HashMap<String, DocumentSlot>>,
}

impl OxrProvider {
    /// Bind an OXR provider to an endpoint + model identity + scope +
    /// credential. Endpoint, model and credential must be provisioned.
    /// Scope `None` explicitly binds an unscoped provider; requests must
    /// still match that exact absence.
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
        let mut code = blake3::Hasher::new();
        code.update(include_bytes!("provider.rs"));
        code.update(include_bytes!("oxr_client.rs"));
        code.update(include_bytes!("types.rs"));
        let binding = OxrProviderBinding {
            endpoint: client.as_ref().ok().map(|client| client.endpoint().to_owned())
                .or_else(|| config.endpoint.clone()),
            model_identity: config.model_identity.clone(),
            scope: config.scope.clone(),
            params_digest: params_digest.clone().expect("effective parameters are bound"),
            code_digest: format!("blake3:{}", code.finalize().to_hex()),
            transport: config.transport,
        };
        let identity = serde_json::to_vec(&binding).expect("provider binding serializes");
        let closure_identity = format!("oxr/1.0/{}", blake3::hash(&identity).to_hex());
        Self {
            endpoint: config.endpoint,
            model_identity: config.model_identity,
            scope: config.scope,
            credential: config.credential,
            engine: config.engine,
            client,
            params_digest,
            binding,
            closure_identity,
            documents: Mutex::new(HashMap::new()),
        }
    }

    pub fn binding(&self) -> &OxrProviderBinding {
        &self.binding
    }

    /// Binding checks only. Acquiring a client never uploads, probes, installs
    /// a model or treats a configured endpoint as proven ready.
    pub fn bound_client(&self) -> Result<&OxrClient, String> {
        if self.endpoint.as_deref().is_none_or(|value| value.trim().is_empty()) {
            return Err("no OXR endpoint bound (endpoint is None)".into());
        }
        if self.model_identity.as_deref().is_none_or(|value| value.trim().is_empty()) {
            return Err("no OXR model identity bound (model_identity is None)".into());
        }
        if self.credential.is_none() {
            return Err("no OXR credential bound (credential is None)".into());
        }
        if let OxrEngineBinding::Local { attestation } = &self.engine
            && attestation.as_deref().is_none_or(|value| value.trim().is_empty())
        {
            return Err("local OXR endpoint bound without an engine attestation (the documented CUDA route uses BF16 support and a compatible CUDA-13 driver)".into());
        }
        self.client.as_ref().map_err(|reason| format!("OXR endpoint rejected at binding: {reason}"))
    }

    /// The real capability, naming the FIRST missing prerequisite.
    /// Reachability is probed last, only after every binding check passed,
    /// so a probe never fires without a complete configuration.
    async fn unavailable_reason(&self) -> Option<String> {
        let client = match self.bound_client() {
            Ok(client) => client,
            Err(reason) => return Some(reason),
        };
        match client.probe().await {
            Ok(()) => None,
            Err(error) => Some(format!("bound OXR endpoint is unreachable: {error}")),
        }
    }

    fn validate_request(&self, req: &ParseRequest) -> Result<(), OxrError> {
        if req.schema_version != crate::DOCUMENT_PARSE_REQUEST_SCHEMA_VERSION || req.closure_id != self.closure_id() {
            return Err(OxrError::Request("document request does not bind this schema/provider closure".into()));
        }
        if req.deployment_scope.as_deref() != self.scope() {
            return Err(OxrError::Request("document provider scope mismatch".into()));
        }
        Ok(())
    }

    /// The admitted document parse shared across this source's pages. Its identity
    /// binds source content, upload name, closure, both scopes and schema. The actual
    /// byte digest prevents a misbound source identity from sharing a parse.
    async fn document_for(
        &self,
        client: &OxrClient,
        source_bytes: &[u8],
        req: &ParseRequest,
    ) -> Result<Arc<OxrDocument>, OxrError> {
        let identity = serde_json::to_vec(&(
            &req.source_digest, &req.source_name, self.closure_id(), &req.cache_scope, &req.deployment_scope,
            req.schema_version, blake3::hash(source_bytes).to_hex().as_str(),
        )).expect("document identity serializes");
        let key = blake3::hash(&identity).to_hex().to_string();
        let cell = {
            let mut documents = self.documents.lock().await;
            documents
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        // Retain even an unsuccessful or abandoned upload. Once the send
        // boundary may have been crossed, another page cannot infer it is
        // safe to submit a second document.
        let file_name = req
            .source_name
            .clone()
            .unwrap_or_else(|| "document".to_owned());
        let mut result = cell.lock().await;
        if result.is_none() {
            *result = Some(Err(OxrError::Transport(
                "original upload was interrupted; reconcile before another attempt".into(),
            )));
            *result = Some(client.parse_document(&file_name, source_bytes).await.map(Arc::new));
        }
        result.as_ref().expect("original attempt retained").clone()
    }
}

#[async_trait::async_trait]
impl DocumentProvider for OxrProvider {
    fn provider_id(&self) -> &str {
        "oxr"
    }

    fn closure_id(&self) -> &str {
        &self.closure_identity
    }

    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn availability(&self) -> ProviderAvailability {
        match self.unavailable_reason().await {
            Some(reason) => ProviderAvailability::Unavailable { reason },
            None => ProviderAvailability::Ready,
        }
    }

    async fn page_count(&self, source_bytes: &[u8], req: &ParseRequest) -> Result<Option<u32>> {
        self.validate_request(req)?;
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
        self.validate_request(req)?;
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
                // Dimensions and bboxes describe the provider's output page.
                // No transform to original source coordinates was supplied.
                coordinates_restored: false,
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
        // Local page-future capacity only, not account QPS enforcement.
        4
    }

    fn provenance(&self) -> PageProvenance {
        PageProvenance {
            provider_id: self.provider_id().into(),
            code_version: self.binding.code_digest.clone(),
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
        OxrEngineBinding::Remote {} => "remote".to_owned(),
        OxrEngineBinding::Local { attestation } => {
            format!("local:attested={}", attestation.is_some())
        }
    };
    let canonical = format!(
        "document.parse/oxr-params/v1;transport={:?};output_formats=json;\
         element_formats=upstream-default;feature_config=upstream-default;engine={engine};\
         request_schema=2;mapping=original-inventory-v2;image_resource=transient-unretained;coordinates=provider-space;\
         request_timeout={:?};poll_interval={:?};poll_timeout={:?}",
        config.transport, config.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
        config.poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
        config.poll_timeout.unwrap_or(DEFAULT_POLL_TIMEOUT),
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
