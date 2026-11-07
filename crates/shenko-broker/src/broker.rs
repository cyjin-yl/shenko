//! Inference broker — fair asynchronous dispatch over one physical resource
//! domain. SPEC §13, §22.1, §41.8–41.10, §47.
//!
//! Every controlled call — the subject, a child, memory work, compaction,
//! diagnostics — enters this broker. The provider is resolved from the open
//! registry against the *sealed* binding, so a request keeps the transport,
//! model and credential identity it was sent with. A provider that changed
//! after the seal cannot silently reinterpret the request.
//!
//! Compute permits and context residency are separate resources: the compute
//! budget belongs to the physical domain, the residency pool is described in
//! [`crate::kv`]. A request whose remote outcome is unknown keeps its
//! capacity and is quarantined as debt until it is reconciled.
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use shenko_types::ids::EntityId;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::admission::{AdmissionController, AdmissionGuard, CapacityPool, CapacitySnapshot};
use crate::capability::BackendCapabilitySnapshot;
use crate::error_sink::{ErrorSinkCell, RuntimeErrorSink, new_error_sink_cell, report_for};
use crate::execution::{execute_request, now_ms};
use crate::kv::{
    AllocationPlan, AutomaticKvPolicy, KvAllocator, KvCapabilities, KvError, KvPoolState,
    ResidencyIdentity,
};
use crate::registry::{ProviderRegistry, ProviderRuntime, TokenCount};
use crate::request::{
    BrokeredCall, InferencePriority, InferenceRequest, InferenceResponse, InferenceResult,
    InferenceStatus, ProviderBinding, ProviderCapabilities, ProviderVisionCaps, RequestPurpose,
    TerminationEvidence, TerminationKind,
};
use crate::resource::{CapacitySnapshot as DomainCapacity, ResourceDomain, ResourceDomainIdentity, ResourceDomainRegistry};
use crate::stream::{BoundedStreamPublisher, PublicStreamSink, StreamIdentity, StreamJournalSink};

#[derive(Deserialize)]
struct ModelMetadata {
    id: String,
    max_model_len: Option<u32>,
    max_context_tokens: Option<u32>,
    max_input_tokens: Option<u32>,
    context_length: Option<u32>,
    max_image_context: Option<u32>,
    patch_px: Option<u32>,
}

#[derive(Deserialize)]
struct ModelMetadataList {
    data: Vec<ModelMetadata>,
    has_more: Option<bool>,
}

impl ProviderCapabilities {
    fn from_metadata(binding: ProviderBinding, metadata: ModelMetadata) -> Result<Self> {
        ensure!(metadata.id == binding.model(), "metadata model id differs from bound model");
        // `max_tokens` is deliberately absent: Anthropic defines that as the
        // output-parameter ceiling, not the input context window.
        let limits = [metadata.max_model_len, metadata.max_context_tokens,
            metadata.max_input_tokens, metadata.context_length]
            .into_iter().flatten().collect::<Vec<_>>();
        ensure!(limits.iter().all(|limit| *limit > 0), "model metadata has an invalid context limit");
        ensure!(limits.iter().all(|limit| Some(*limit) == limits.first().copied()),
            "model metadata reports conflicting context limits");
        if let Some(limit) = metadata.max_image_context {
            ensure!(limit > 0, "model metadata has an invalid image context limit");
        }
        if let Some(patch) = metadata.patch_px {
            ensure!(patch > 0, "model metadata has an invalid vision patch size");
        }
        let vision = match (metadata.max_image_context, metadata.patch_px) {
            (Some(max_image_context), Some(patch_px)) => Some(ProviderVisionCaps { max_image_context, patch_px }),
            _ => None,
        };
        Ok(Self { binding, max_context_tokens: limits.first().copied(), vision })
    }
}

/// The ledger owner of a request, independent of the caller waiting for it.
/// It admits no effects itself: T1 must already exist before submission. Late
/// completion is still recorded when a managed child was stopped/cancelled.
pub trait InferenceReceiptSink: InferenceRecoverySource {
    /// One atomic decision at the actual send boundary, synchronized with
    /// durable cancellation and authority changes. `Send` means the caller
    /// irrevocably owns the single armed dispatch; `Fenced` means authority
    /// was verifiably lost before any send; `Unknown` means a dispatch may
    /// already have committed (e.g. an in-flight original or restart), so the
    /// request must stay recoverable and must never be marked never-sent.
    fn may_dispatch(&self) -> Result<DispatchFence>;
    fn record(&self, result: &InferenceResult) -> Result<()>;
}

/// Durable send-boundary verdict; distinct outcomes cannot silently swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchFence {
    /// This submission won the atomic transition and may send exactly once.
    Send,
    /// The authority check failed before any send committed.
    Fenced,
    /// A prior dispatch may already have committed; outcome is recoverable debt.
    Unknown,
}

/// Trusted runtime adapter to the original durable ledger, not model input or
/// a callback's assertion of success. A reader must validate the final E1 CAS,
/// operation/attempt identities and immutable request before returning evidence.
/// There is deliberately no raw-id, bool or caller-result release API.
pub trait InferenceRecoverySource: Send + Sync + 'static {
    fn request_id(&self) -> EntityId;
    fn binding(&self) -> &ProviderBinding;
    fn payload(&self) -> &crate::request::InferencePayload;
    fn read_terminal_result(&self) -> Result<Option<InferenceResult>>;
}

/// The streaming sinks of one request.
pub(crate) struct StreamSubmission {
    pub(crate) identity: StreamIdentity,
    pub(crate) journal: Option<Arc<dyn StreamJournalSink>>,
    pub(crate) public: Option<Arc<dyn PublicStreamSink>>,
}

struct Submission {
    request: InferenceRequest,
    receipt_sink: Option<Arc<dyn InferenceReceiptSink>>,
    purpose: RequestPurpose,
    stream: Option<StreamSubmission>,
}

/// One provider/model pair this broker can really run.
#[derive(Debug, Clone, PartialEq)]
pub struct AvailableProvider {
    /// The model this entry serves.
    pub model: String,
    /// The profile that describes the binding.
    pub profile_id: String,
    pub backend_family: crate::profile::BackendFamily,
    pub transport: crate::profile::TransportKind,
    /// The endpoint the binding reaches. It never carries credentials.
    pub endpoint_origin: String,
    /// The physical resource domain this binding belongs to.
    pub resource_domain_id: String,
    /// Whether this binding is the local OpenAI-compatible transport.
    pub local: bool,
    /// The binding itself.
    pub binding: ProviderBinding,
}

// None: queued or live, with a guard owned by the broker. Some: an uncertain
// completed/recovered request whose charge survives until original evidence.
type Accounting = Arc<Mutex<HashMap<EntityId, Option<AdmissionGuard>>>>;
type RecoverySources = Arc<Mutex<HashMap<EntityId, Arc<dyn InferenceRecoverySource>>>>;

#[derive(Clone)]
pub struct BrokerHandle {
    tx: mpsc::UnboundedSender<Submission>,
    binding: ProviderBinding,
    /// The compute budget of this handle's physical domain.
    pub admission: AdmissionController,
    domain: Arc<ResourceDomain>,
    domains: Arc<ResourceDomainRegistry>,
    registry: Arc<ProviderRegistry>,
    accounting: Accounting,
    recovery_sources: RecoverySources,
    accounting_changed: Arc<Notify>,
    kv: Arc<KvAllocator>,
    error_sink: ErrorSinkCell,
    handoff_closed: Arc<std::sync::atomic::AtomicBool>,
}

impl BrokerHandle {
    pub fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    /// Same physical scheduling domain and configured transport, distinct
    /// exact model binding. Existence/availability is decided by the provider,
    /// not by this selector or an invented model-capability table.
    pub fn for_model(&self, model: &str) -> Result<Self> {
        Ok(Self { binding: self.binding.with_model(model)?, ..self.clone() })
    }

    /// The physical domain this handle dispatches into.
    pub fn domain(&self) -> &Arc<ResourceDomain> {
        &self.domain
    }

    /// The domain registry: the deployment's binding of endpoints, aliases and
    /// engines to physical resource domains.
    pub fn domains(&self) -> &Arc<ResourceDomainRegistry> {
        &self.domains
    }

    /// The provider bindings this broker can really run. Availability is
    /// decided by what is installed, never by a catalog of model names.
    pub fn installed_bindings(&self) -> Vec<ProviderBinding> {
        self.registry.installed_bindings()
    }

    /// The installed provider profiles.
    pub fn profiles(&self) -> Vec<crate::profile::ProviderProfile> {
        self.registry.profiles()
    }

    /// The provider/model pairs this broker can really run right now.
    ///
    /// This is the live view a routing or quota decision uses. It is decided
    /// by what is installed and reachable, never by a catalog of model names:
    /// a model that is not installed here simply does not appear.
    pub fn available_providers(&self) -> Vec<AvailableProvider> {
        self.registry
            .profiles()
            .into_iter()
            .map(|profile| {
                let binding = self.registry.primary_binding_for(&profile).unwrap_or_else(|| profile.binding());
                AvailableProvider {
                    model: profile.model_id.clone(),
                    profile_id: profile.profile_id.clone(),
                    backend_family: profile.backend_family.clone(),
                    transport: profile.transport,
                    endpoint_origin: profile.endpoint_origin.clone(),
                    resource_domain_id: profile.resource_domain_id.clone(),
                    local: matches!(binding, ProviderBinding::Local { .. }),
                    binding,
                }
            })
            .collect()
    }

    /// Whether this handle can really serve one model on its own transport.
    ///
    /// A routing decision resolves the model before the request is sealed, so
    /// the sealed binding is one the broker can execute. The sealed
    /// comparison itself stays exact.
    pub fn serves_model(&self, model: &str) -> bool {
        self.binding
            .with_model(model)
            .ok()
            .is_some_and(|binding| self.registry.runtime_for(&binding).is_some())
    }

    /// Restore an original armed request before admitting any new inference.
    /// Repeated recovery and live requests cannot count the same identity twice.
    pub fn recover_unknown(&self, request_id: EntityId) {
        self.accounting.lock().expect("broker accounting poisoned")
            .entry(request_id).or_insert_with(|| Some(self.admission.recover_unknown()));
        self.domain
            .quarantine(request_id, now_ms(), "recovered without terminal evidence");
    }

    /// Register the original ledger reader while restoring a request. A known
    /// request keeps the reader installed by its original submit/restore; later
    /// callers cannot replace it with a bool or a fabricated result source.
    pub fn register_recovered(&self, source: Arc<dyn InferenceRecoverySource>) -> Result<()> {
        let id = source.request_id();
        {
            let accounting = self.accounting.lock().expect("broker accounting poisoned");
            let mut sources = self.recovery_sources.lock().expect("broker recovery sources poisoned");
            if !sources.contains_key(&id) {
                ensure!(!accounting.contains_key(&id), "known request has no original recovery reader; preserve unknown charge");
                sources.insert(id, source);
            }
        }
        self.reconcile_recorded(id)
    }

    /// A request ID only wakes its already-pinned reader. It is never release
    /// evidence by itself, and this API accepts no caller-supplied result.
    pub fn reconcile_recorded(&self, request_id: EntityId) -> Result<()> {
        let source = self.recovery_sources.lock().expect("broker recovery sources poisoned")
            .get(&request_id).cloned().ok_or_else(|| anyhow!("request has no original durable recovery reader"))?;
        let result = source.read_terminal_result()?;
        if let Some(result) = &result {
            ensure!(result.request_id == source.request_id()
                && result.effective.matches_seal(source.binding(), source.payload())
                && result.termination.kind != TerminationKind::Unknown,
                "recovered inference lacks matching original terminal evidence");
        }
        let mut accounting = self.accounting.lock().expect("broker accounting poisoned");
        let mut sources = self.recovery_sources.lock().expect("broker recovery sources poisoned");
        // Reading the ledger must not hold the broker lock. A real terminal
        // completion may retire this reader while that read is in progress:
        // its stale None must not recreate debt after release, nor may an old
        // reader release a later registration using the same request key.
        if !sources.get(&request_id).is_some_and(|current| Arc::ptr_eq(current, &source)) {
            return Ok(());
        }
        let Some(result) = result else {
            accounting.entry(request_id).or_insert_with(|| Some(self.admission.recover_unknown()));
            return Ok(());
        };
        if accounting.get(&source.request_id()).is_some_and(Option::is_some) {
            let guard = accounting.remove(&source.request_id()).flatten().expect("checked charge");
            match result.termination.kind {
                TerminationKind::NeverDispatched => guard.release_never_dispatched(),
                TerminationKind::ProviderTerminal | TerminationKind::TransportRejected => guard.release_terminal(),
                TerminationKind::Unknown => unreachable!("terminal evidence checked above"),
            }
            self.domain.reconcile_debt(request_id);
            // The original terminal evidence ends the context read hold the
            // unknown outcome kept charged; the context may now be reclaimed
            // at the next safe boundary (§41.10).
            self.kv
                .release_unknown_read(&mut self.domain.kv_mut(), &request_id.to_string());
            self.accounting_changed.notify_one();
        }
        if !accounting.contains_key(&request_id) {
            sources.remove(&request_id);
        }
        Ok(())
    }

    pub fn active_count(&self) -> u64 {
        self.admission.active_count()
    }

    /// Close local submission at the accounting lock, then await every
    /// enqueued/running provider reader and its durable receipt write.
    /// Unknown remote outcomes stay charged; this is not remote termination.
    pub async fn quiesce_for_handoff(&self) -> Result<serde_json::Value> {
        {
            let _accounting = self.accounting.lock().expect("broker accounting poisoned");
            self.handoff_closed.store(true, std::sync::atomic::Ordering::Release);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let snapshot = {
                let accounting = self.accounting.lock().expect("broker accounting poisoned");
                if accounting.values().any(Option::is_none) { None } else {
                    let sources = self.recovery_sources.lock().expect("broker recovery sources poisoned");
                    let requests = accounting.iter().map(|(id, charge)| {
                        let source = sources.get(id);
                        serde_json::json!({
                            "request_id": id, "state": "unknown", "charged": charge.is_some(),
                            "binding": source.map(|source| source.binding()),
                            "sealed_payload": source.map(|source| source.payload()),
                            "reader_available": source.is_some(),
                        })
                    }).collect::<Vec<_>>();
                    Some(serde_json::json!({
                        "schema_version": 1, "local_admission_closed": true,
                        "local_provider_readers_drained": true,
                        "remote_termination_claimed": false,
                        "requests": requests,
                        "domains": self.domains.domains().iter()
                            .map(|domain| domain.handoff_snapshot()).collect::<Vec<_>>(),
                    }))
                }
            };
            if let Some(snapshot) = snapshot { return Ok(snapshot); }
            ensure!(tokio::time::Instant::now() < deadline,
                "BROKER_HANDOFF_INCOMPLETE: local provider/receipt readers remain; remote debt not released");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    pub fn resume_after_handoff_abort(&self) {
        let _accounting = self.accounting.lock().expect("broker accounting poisoned");
        self.handoff_closed.store(false, std::sync::atomic::Ordering::Release);
        self.accounting_changed.notify_one();
    }

    pub fn capacity(&self) -> CapacitySnapshot {
        self.admission.capacity()
    }

    /// The capacity that is actually available now: the configured budget
    /// minus the quarantined unknown debt and the enforceable external
    /// reservations of this domain.
    pub fn effective_capacity(&self) -> DomainCapacity {
        self.domain.effective_compute_capacity()
    }

    /// The residency pool of this domain.
    pub fn kv_snapshot(&self) -> KvPoolState {
        self.domain.kv().clone()
    }

    /// The residency controls the pool currently relies on. An option that is
    /// false was never demonstrated by this backend; a sealed request naming it
    /// carries no residency claim.
    pub fn kv_capabilities(&self) -> KvCapabilities {
        self.kv.capabilities()
    }

    /// Recompute the main/children share of the residency pool.
    pub fn rebalance_kv(&self) -> Result<AllocationPlan, KvError> {
        let mut pool = self.domain.kv_mut();
        self.kv.rebalance(&mut pool)
    }

    pub fn submit(&self, req: InferenceRequest) -> Result<()> {
        let purpose = RequestPurpose::from_priority(req.priority);
        self.submit_inner(req, None, purpose, None)
    }

    /// Install the durable runtime error journal of the owning subject.
    ///
    /// The broker journals the errors it cannot continue from, or that force a
    /// degradation, against the agent and conversation of the actual request.
    /// The write is fail-soft and never replaces the error it reports.
    pub fn set_runtime_error_sink(&self, sink: Arc<dyn RuntimeErrorSink>) {
        if let Ok(mut guard) = self.error_sink.lock() {
            *guard = Some(sink);
        }
    }

    pub fn submit_recorded(&self, req: InferenceRequest, sink: Arc<dyn InferenceReceiptSink>) -> Result<()> {
        let purpose = RequestPurpose::from_priority(req.priority);
        self.submit_inner(req, Some(sink), purpose, None)
    }

    /// Submit a controlled call with its declared purpose.
    pub fn submit_purpose(&self, call: BrokeredCall) -> Result<()> {
        self.submit_inner(call.request, None, call.purpose, None)
    }

    /// Submit a controlled call with its declared purpose and a durable
    /// receipt owner.
    pub fn submit_recorded_purpose(
        &self,
        call: BrokeredCall,
        sink: Arc<dyn InferenceReceiptSink>,
    ) -> Result<()> {
        self.submit_inner(call.request, Some(sink), call.purpose, None)
    }

    /// Submit a streamed controlled call.
    ///
    /// The stream starts at the provider. Without a durable journal the call
    /// is refused instead of dropping content silently; the public sink is
    /// optional, the durable one is not.
    pub fn submit_stream(
        &self,
        call: BrokeredCall,
        identity: StreamIdentity,
        journal: Option<Arc<dyn StreamJournalSink>>,
        public: Option<Arc<dyn PublicStreamSink>>,
    ) -> Result<()> {
        ensure!(
            journal.is_some(),
            "streamed inference needs a durable journal sink; content is never dropped silently"
        );
        self.submit_inner(
            call.request,
            None,
            call.purpose,
            Some(StreamSubmission { identity, journal, public }),
        )
    }

    fn submit_inner(
        &self,
        req: InferenceRequest,
        receipt_sink: Option<Arc<dyn InferenceReceiptSink>>,
        purpose: RequestPurpose,
        stream: Option<StreamSubmission>,
    ) -> Result<()> {
        anyhow::ensure!(
            req.provider_binding == self.binding,
            "MODEL_PROVIDER_BINDING_MISMATCH: request not admitted; seal a new revision"
        );
        ensure!(
            self.registry.runtime_for(&req.provider_binding).is_some(),
            "no installed provider runtime can serve this binding"
        );
        let mut accounting = self.accounting.lock().expect("broker accounting poisoned");
        ensure!(!self.handoff_closed.load(std::sync::atomic::Ordering::Acquire),
            "BROKER_HANDOFF_CLOSED: request rejected before enqueue/send");
        ensure!(!accounting.contains_key(&req.request_id), "broker request already submitted or uncertain; reconcile original identity");
        let id = req.request_id;
        if let Some(sink) = &receipt_sink {
            ensure!(sink.request_id() == id && sink.binding() == &req.provider_binding
                && sink.payload() == &req.payload, "receipt owner differs from submitted request seal");
            let source: Arc<dyn InferenceRecoverySource> = sink.clone();
            let mut sources = self.recovery_sources.lock().expect("broker recovery sources poisoned");
            ensure!(!sources.contains_key(&id), "request already has an original receipt owner");
            sources.insert(id, source);
        }
        accounting.insert(id, None);
        if self.tx.send(Submission { request: req, receipt_sink, purpose, stream }).is_err() {
            accounting.remove(&id);
            self.recovery_sources.lock().expect("broker recovery sources poisoned").remove(&id);
            return Err(anyhow!("broker admission closed; request was not enqueued"));
        }
        Ok(())
    }
}

/// FIFO lanes with explicit round-robin across urgency, main/child and child
/// priorities. Urgent work can go first but cannot indefinitely starve ordinary
/// work; ordinary main/child turns alternate whenever both sides are ready.
struct FairQueue {
    lanes: [VecDeque<Submission>; 4],
    main_next: bool,
    normal_child_next: bool,
    last_was_urgent: bool,
}

impl Default for FairQueue {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| VecDeque::new()),
            main_next: true,
            normal_child_next: true,
            last_was_urgent: false,
        }
    }
}

impl FairQueue {
    fn push(&mut self, req: Submission) {
        self.lanes[req.request.priority as usize].push_back(req);
    }

    fn is_empty(&self) -> bool {
        self.lanes.iter().all(VecDeque::is_empty)
    }

    fn pop(&mut self) -> Option<Submission> {
        let urgent_ready = !self.lanes[3].is_empty();
        let main_ready = !self.lanes[2].is_empty();
        let normal_ready = !self.lanes[1].is_empty();
        let low_ready = !self.lanes[0].is_empty();
        let ordinary_ready = main_ready || normal_ready || low_ready;

        if urgent_ready && (!self.last_was_urgent || !ordinary_ready) {
            self.last_was_urgent = true;
            return self.lanes[3].pop_front();
        }
        self.last_was_urgent = false;
        if !ordinary_ready {
            return None;
        }

        let use_main = match (main_ready, normal_ready || low_ready) {
            (true, true) => self.main_next,
            (true, false) => true,
            (false, true) => false,
            (false, false) => return None,
        };
        self.main_next = !use_main;
        if use_main {
            return self.lanes[2].pop_front();
        }

        let use_normal = match (normal_ready, low_ready) {
            (true, true) => self.normal_child_next,
            (true, false) => true,
            (false, true) => false,
            (false, false) => return None,
        };
        self.normal_child_next = !use_normal;
        self.lanes[usize::from(use_normal)].pop_front()
    }
}

struct CompletedRequest {
    request: InferenceRequest,
    receipt_sink: Option<Arc<dyn InferenceReceiptSink>>,
    result: InferenceResult,
    guard: AdmissionGuard,
}

pub struct Broker {
    rx: mpsc::UnboundedReceiver<Submission>,
    domain: Arc<ResourceDomain>,
    domains: Arc<ResourceDomainRegistry>,
    registry: Arc<ProviderRegistry>,
    accounting: Accounting,
    recovery_sources: RecoverySources,
    accounting_changed: Arc<Notify>,
    kv: Arc<KvAllocator>,
    error_sink: ErrorSinkCell,
}

impl Broker {
    pub fn new() -> Result<(Self, BrokerHandle)> {
        Self::from_env_with_admission(AdmissionController::nominal())
    }

    /// Configure a compute-only concurrency budget. This does not assert a
    /// measured physical stream count or any KV residency capacity.
    pub fn with_capacity(max: u64) -> Result<(Self, BrokerHandle)> {
        Self::from_env_with_admission(AdmissionController::new(max))
    }

    pub fn with_local_provider(provider: crate::local_provider::LocalModelProvider, max: u64) -> Result<(Self, BrokerHandle)> {
        provider.binding().validate_endpoint()?;
        let domain = default_domain(AdmissionController::new(max))?;
        Self::with_runtime(Arc::new(provider), domain)
    }

    /// Bind an explicitly configured real Anthropic provider without reading
    /// ambient credentials. Credentials stay separate from the validated
    /// endpoint; dispatch shares the ordinary broker queue and compute budget.
    pub fn with_anthropic_provider(
        provider: crate::provider::AnthropicProvider,
        max: u64,
    ) -> Result<(Self, BrokerHandle)> {
        provider.binding().validate_endpoint()?;
        let domain = default_domain(AdmissionController::new(max))?;
        Self::with_runtime(Arc::new(provider), domain)
    }

    /// Bind one installed provider runtime to one physical resource domain.
    ///
    /// This is the open path: the runtime may be any installed provider, and
    /// the domain is the deployment's explicit binding of that endpoint to the
    /// engine it really reaches.
    pub fn with_runtime(
        runtime: Arc<dyn ProviderRuntime>,
        domain: Arc<ResourceDomain>,
    ) -> Result<(Self, BrokerHandle)> {
        runtime.binding().validate_endpoint()?;
        let registry = Arc::new(ProviderRegistry::new());
        registry.install(Arc::clone(&runtime));
        Ok(Self::with_registry(registry, domain))
    }

    /// Bind a whole registry of installed provider runtimes to one physical
    /// resource domain. The broker serves the bindings the registry can run.
    pub fn with_registry(
        registry: Arc<ProviderRegistry>,
        domain: Arc<ResourceDomain>,
    ) -> (Self, BrokerHandle) {
        let domains = Arc::new(ResourceDomainRegistry::new());
        let _ = domains.register(Arc::clone(&domain));
        Self::assemble(registry, domain, domains)
    }

    fn from_env_with_admission(admission: AdmissionController) -> Result<(Self, BrokerHandle)> {
        let runtime = env_runtime()?;
        let domain = default_domain(admission.clone())?;
        let registry = Arc::new(ProviderRegistry::new());
        registry.install(Arc::clone(&runtime));
        Ok(Self::assemble(registry, domain, Arc::new(ResourceDomainRegistry::new())))
    }

    fn assemble(
        registry: Arc<ProviderRegistry>,
        domain: Arc<ResourceDomain>,
        domains: Arc<ResourceDomainRegistry>,
    ) -> (Self, BrokerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        // The binding comes from the installed runtime, not from a profile
        // table: the runtime is what actually executes the request.
        let binding = registry
            .primary_binding()
            .unwrap_or(ProviderBinding::Stub { codec: 2 });
        // The compute budget belongs to the physical domain; the handle and
        // the broker must observe the same counter.
        let admission = domain.admission().clone();
        let accounting = Arc::new(Mutex::new(HashMap::new()));
        let recovery_sources = Arc::new(Mutex::new(HashMap::new()));
        let accounting_changed = Arc::new(Notify::new());
        let kv = Arc::new(KvAllocator::new(
            Box::new(AutomaticKvPolicy::default()),
            KvCapabilities::default(),
        ));
        let error_sink = new_error_sink_cell();
        (
            Self {
                rx,
                domain: Arc::clone(&domain),
                domains: Arc::clone(&domains),
                registry: Arc::clone(&registry),
                accounting: Arc::clone(&accounting),
                recovery_sources: Arc::clone(&recovery_sources),
                accounting_changed: Arc::clone(&accounting_changed),
                kv: Arc::clone(&kv),
                error_sink: Arc::clone(&error_sink),
            },
            BrokerHandle {
                tx,
                binding,
                admission,
                domain,
                domains,
                registry,
                accounting,
                recovery_sources,
                accounting_changed,
                kv,
                error_sink,
                handoff_closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        )
    }

    /// Explicit offline construction; does not inspect ambient credentials and
    /// returns typed Unavailable rather than a fabricated successful completion.
    pub fn offline() -> (Self, BrokerHandle) {
        let registry = Arc::new(ProviderRegistry::new());
        registry.install(Arc::new(OfflineRuntime));
        let domain = default_domain(AdmissionController::nominal())
            .expect("the default domain is constructible");
        let domains = Arc::new(ResourceDomainRegistry::new());
        let _ = domains.register(Arc::clone(&domain));
        Self::assemble(registry, domain, domains)
    }

    /// Observe what the bound backend actually offers, and adopt what the
    /// residency pool may rely on.
    ///
    /// A probe whose result is only returned would leave the pool claiming
    /// nothing, so the discovered pool size and the demonstrated residency
    /// controls are applied to this domain here. An observation that reported
    /// no pool figure leaves the capacity unknown; nothing is estimated.
    pub async fn probe_capabilities(&self) -> Result<BackendCapabilitySnapshot> {
        let binding = self
            .registry
            .primary_binding()
            .ok_or_else(|| anyhow!("no provider runtime is installed"))?;
        let runtime = self.runtime_for(&binding)?;
        let snapshot = runtime.probe().await?;
        self.adopt_capabilities(&snapshot);
        Ok(snapshot)
    }

    /// Adopt one capability snapshot into the live residency pool.
    ///
    /// The pool takes the ContextKv figure the backend reported, with its own
    /// provenance, the controls the backend demonstrated and the identity the
    /// binding belongs to. Everything the snapshot left unknown stays unknown:
    /// no share, pause, spill, affinity or identity claim is invented from a
    /// backend that never showed it.
    ///
    /// The share is recomputed here, so a newly discovered pool or a newly
    /// demonstrated control reaches the existing leases instead of waiting for
    /// the next admission; a pool that shrank below its occupancy reclaims down
    /// to the new figure as part of that same step.
    pub fn adopt_capabilities(&self, snapshot: &BackendCapabilitySnapshot) -> KvCapabilities {
        let capabilities = KvCapabilities::from_snapshot(snapshot);
        if let Some(entry) = snapshot.capacity(CapacityPool::ContextKv) {
            self.domain.apply_kv_capacity(entry.value, entry.provenance);
        }
        let mut identity = ResidencyIdentity::from_snapshot(snapshot);
        identity.engine_identity = Some(self.domain.identity().engine_identity);
        let mut pool = self.domain.kv_mut();
        self.kv.adopt_identity(&mut pool, &identity);
        self.kv.apply_capabilities(&mut pool, capabilities);
        let _ = self.kv.rebalance(&mut pool);
        capabilities
    }

    /// Read one bounded native metadata response before dispatch. The model
    /// must match exactly; no first-card fallback or capability name table.
    pub async fn discover_capabilities(&self) -> Result<ProviderCapabilities> {
        let binding = self
            .registry
            .primary_binding()
            .ok_or_else(|| anyhow!("no provider runtime is installed"))?;
        let runtime = self.runtime_for(&binding)?;
        let snapshot = runtime.probe().await?;
        // Startup discovery is also the moment the residency pool learns what
        // this backend offers; the metadata read below is only the model ceiling.
        self.adopt_capabilities(&snapshot);
        let binding = runtime.binding();
        let metadata = snapshot
            .raw_evidence
            .iter()
            .filter_map(|document| serde_json::from_value::<ModelMetadataList>(document.clone()).ok())
            .flat_map(|list| list.data)
            .find(|model| model.id == binding.model())
            .ok_or_else(|| anyhow!("bound model metadata not found"))?;
        ProviderCapabilities::from_metadata(binding, metadata)
    }

    /// Count the tokens one request would send, when the binding supports it.
    pub async fn count_tokens(&self, request: &InferenceRequest) -> Result<TokenCount> {
        let runtime = self.runtime_for(&request.provider_binding)?;
        runtime.count_tokens(request).await
    }

    fn runtime_for(&self, binding: &ProviderBinding) -> Result<Arc<dyn ProviderRuntime>> {
        self.registry
            .runtime_for(binding)
            .ok_or_else(|| anyhow!("no installed provider runtime can serve this binding"))
    }

    fn finish(&self, completed: CompletedRequest) {
        let CompletedRequest { request, receipt_sink, result, guard } = completed;
        let request_id = request.request_id;
        if let Some(sink) = receipt_sink {
            if let Err(error) = sink.record(&result) {
                warn!(request_id=%request_id, %error, "model receipt not durably recorded; retaining unknown charge");
                self.accounting.lock().expect("broker accounting poisoned")
                    .insert(request_id, Some(guard));
                return;
            }
        }
        let termination = result.termination.kind;
        // Dropping the receiver does not cancel the already executing provider
        // future. Its actual completion is still accounted before this guard.
        let mut accounting = self.accounting.lock().expect("broker accounting poisoned");
        match termination {
            TerminationKind::ProviderTerminal | TerminationKind::TransportRejected => {
                accounting.remove(&request_id);
                self.recovery_sources.lock().expect("broker recovery sources poisoned").remove(&request_id);
                self.domain.reconcile_debt(request_id);
                // Defensive drain: a hold can only exist after an unknown
                // outcome, and this result is that outcome's own terminal fact.
                self.kv
                    .release_unknown_read(&mut self.domain.kv_mut(), &request_id.to_string());
                guard.release_terminal();
            }
            TerminationKind::NeverDispatched => {
                accounting.remove(&request_id);
                self.recovery_sources.lock().expect("broker recovery sources poisoned").remove(&request_id);
                self.domain.reconcile_debt(request_id);
                self.kv
                    .release_unknown_read(&mut self.domain.kv_mut(), &request_id.to_string());
                guard.release_never_dispatched();
            }
            TerminationKind::Unknown => {
                accounting.insert(request_id, Some(guard));
                crate::error_sink::journal_runtime_error(
                    &self.error_sink,
                    &report_for(&request, result.purpose, "INFERENCE_OUTCOME_UNKNOWN", "provider outcome unknown"),
                );
                self.domain.quarantine(
                    request_id,
                    now_ms(),
                    "the provider outcome could not be determined",
                );
            }
        }
        drop(accounting);
        // The caller may have disappeared; the durable receipt above does not
        // depend on it.
        let _ = request.result_tx.send(result);
    }

    pub async fn run(mut self) {
        info!("inference broker running");
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<CompletedRequest>();
        let mut queue = FairQueue::default();
        let mut input_closed = false;
        let mut inflight = 0_u64;
        let mut prefer_input = false;
        enum Event {
            Completed(Option<CompletedRequest>),
            Incoming(Option<Submission>),
            AccountingChanged,
        }

        loop {
            // Fill only currently available slots. A full budget leaves the
            // request in FairQueue instead of losing it or failing it spuriously.
            while !queue.is_empty() {
                let Some(guard) = self.domain.admission().try_admit() else {
                    break;
                };
                let submission = queue.pop().expect("queue checked non-empty");
                let registry = Arc::clone(&self.registry);
                let domain = Arc::clone(&self.domain);
                let kv = Arc::clone(&self.kv);
                let error_sink = Arc::clone(&self.error_sink);
                let completed_tx = completed_tx.clone();
                tokio::spawn(async move {
                    let Submission { request, receipt_sink, purpose, stream } = submission;
                    let result = execute_request(
                        &request,
                        receipt_sink.as_ref(),
                        purpose,
                        stream,
                        &registry,
                        &domain,
                        &kv,
                        &error_sink,
                    )
                    .await;
                    let _ = completed_tx.send(CompletedRequest {
                        request,
                        receipt_sink,
                        result,
                        guard,
                    });
                });
                inflight += 1;
            }

            if input_closed && queue.is_empty() && inflight == 0 {
                break;
            }

            // One event per round, alternating priority when both channels stay
            // ready. Neither a completion flood nor an inbound flood can prevent
            // the other side from being reaped on the next round.
            let event = if prefer_input {
                tokio::select! {
                    biased;
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    _ = self.accounting_changed.notified() => Event::AccountingChanged,
                    else => std::future::pending::<Event>().await,
                }
            } else {
                tokio::select! {
                    biased;
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    _ = self.accounting_changed.notified() => Event::AccountingChanged,
                    else => std::future::pending::<Event>().await,
                }
            };
            match event {
                Event::Completed(completed) => {
                    if let Some(completed) = completed {
                        inflight -= 1;
                        self.finish(completed);
                    }
                    prefer_input = true;
                }
                Event::Incoming(incoming) => {
                    match incoming {
                        Some(req) => queue.push(req),
                        None => input_closed = true,
                    }
                    prefer_input = false;
                }
                Event::AccountingChanged => {}
            }
        }
        info!("inference broker stopped");
    }
}

/// The provider runtime selected from the environment.
fn env_runtime() -> Result<Arc<dyn ProviderRuntime>> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() == Ok("1") {
        let provider = crate::local_provider::LocalModelProvider::from_env()?;
        info!(model=%provider.model, "local model provider selected (§22.1)");
        return Ok(Arc::new(provider));
    }
    if let Some(provider) = crate::provider::AnthropicProvider::from_env() {
        info!("Anthropic API key found — real inference enabled");
        return Ok(Arc::new(provider));
    }
    info!("no inference provider configured — typed unavailable mode");
    Ok(Arc::new(OfflineRuntime))
}

/// The default physical domain of an unclassified deployment.
///
/// The engine identity is unknown, so the domain offers no hard guarantee; it
/// only keeps one conservative budget for everything this deployment runs. A
/// deployment that knows its engines binds them explicitly.
fn default_domain(admission: AdmissionController) -> Result<Arc<ResourceDomain>> {
    let capacity = admission.capacity();
    Ok(ResourceDomain::new(
        ResourceDomainIdentity {
            domain_id: "default.unclassified".into(),
            engine_identity: "unclassified".into(),
            engine_incarnation: None,
            epoch: 1,
        },
        capacity.value.unwrap_or(1),
        capacity.provenance,
        None,
        crate::admission::CapacityProvenance::Unknown,
    ))
}

/// The typed unavailable runtime.
struct OfflineRuntime;

#[async_trait::async_trait]
impl ProviderRuntime for OfflineRuntime {
    fn profile(&self) -> &crate::profile::ProviderProfile {
        static PROFILE: std::sync::LazyLock<crate::profile::ProviderProfile> = std::sync::LazyLock::new(|| crate::profile::ProviderProfile {
            profile_id: "provider.unavailable".into(),
            revision: 2,
            backend_family: crate::profile::BackendFamily::Other("unavailable".into()),
            transport: crate::profile::TransportKind::HttpJson,
            endpoint_origin: "http://127.0.0.1:0".into(),
            model_id: "stub".into(),
            api_version: None,
            sdk: None,
            request_schema_revision: 2,
            response_schema_revision: 2,
            gateway_profile: None,
            beta_features: Vec::new(),
            sampling: crate::profile::SamplingCapabilities::default(),
            auth: crate::profile::AuthBinding::none(),
            usage: crate::profile::UsageFieldMapping::default(),
            native_namespaces: Vec::new(),
            resource_domain_id: "default.unclassified".into(),
            engine_incarnation: None,
            default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
            serializer_revision: 2,
        });
        &PROFILE
    }

    fn binding(&self) -> ProviderBinding {
        ProviderBinding::Stub { codec: 2 }
    }

    fn available(&self) -> bool {
        false
    }

    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        Err(anyhow!("no provider is configured"))
    }

    async fn stream(
        &self,
        _request: &InferenceRequest,
        _publisher: &BoundedStreamPublisher,
    ) -> Result<crate::stream::StreamOutcome> {
        Err(anyhow!("no provider is configured"))
    }

    async fn count_tokens(&self, _request: &InferenceRequest) -> Result<TokenCount> {
        Err(anyhow!("no provider is configured"))
    }

    async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
        Err(anyhow!("no provider is configured"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{CapacityPool, CapacityProvenance};
    use crate::request::{
        InferenceMessage, InferencePayload, TokenUsage, WireErrorCode,
    };
    use crate::stream::{
        SseDecoder, SseFrame, StreamBatch, StreamEvent, StreamSinkError,
        parse_anthropic_frame, parse_openai_frame,
    };
    use shenko_types::ids::{ControllerEpoch, EntityId};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Semaphore, mpsc, oneshot};

    struct ControlledProvider {
        binding: ProviderBinding,
        starts: mpsc::UnboundedSender<(EntityId, InferencePriority)>,
        gate: Arc<Semaphore>,
        active: AtomicU64,
        max_active: AtomicU64,
    }

    impl ControlledProvider {
        fn new() -> (
            Arc<Self>,
            mpsc::UnboundedReceiver<(EntityId, InferencePriority)>,
        ) {
            let (starts, starts_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    binding: ProviderBinding::Local {
                        base_url: "http://127.0.0.1".into(),
                        model: "controlled-fixture".into(),
                        allow_invalid_certs: false,
                        serializer_revision: crate::local_provider::SERIALIZER_REVISION,
                        default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
                    },
                    starts,
                    gate: Arc::new(Semaphore::new(0)),
                    active: AtomicU64::new(0),
                    max_active: AtomicU64::new(0),
                }),
                starts_rx,
            )
        }

        async fn next_start(
            rx: &mut mpsc::UnboundedReceiver<(EntityId, InferencePriority)>,
        ) -> (EntityId, InferencePriority) {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for fixture dispatch")
                .expect("fixture start channel remains open")
        }
    }

    #[async_trait::async_trait]
    impl ProviderRuntime for ControlledProvider {
        fn profile(&self) -> &crate::profile::ProviderProfile {
            static PROFILE: std::sync::LazyLock<crate::profile::ProviderProfile> = std::sync::LazyLock::new(|| crate::profile::ProviderProfile {
                profile_id: "local.openai.chat_completions".into(),
                revision: crate::local_provider::SERIALIZER_REVISION,
                backend_family: crate::profile::BackendFamily::OpenAiCompatible,
                transport: crate::profile::TransportKind::HttpJson,
                endpoint_origin: "http://127.0.0.1".into(),
                model_id: "controlled-fixture".into(),
                api_version: None,
                sdk: None,
                request_schema_revision: crate::local_provider::SERIALIZER_REVISION,
                response_schema_revision: crate::local_provider::SERIALIZER_REVISION,
                gateway_profile: None,
                beta_features: Vec::new(),
                sampling: crate::profile::SamplingCapabilities::default(),
                auth: crate::profile::AuthBinding::none(),
                usage: crate::profile::UsageFieldMapping {
                    input_tokens_path: Some("usage.prompt_tokens".into()),
                    output_tokens_path: Some("usage.completion_tokens".into()),
                    cache_read_tokens_path: None,
                    cache_write_tokens_path: None,
                },
                native_namespaces: Vec::new(),
                resource_domain_id: "default.unclassified".into(),
                engine_incarnation: None,
                default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
                serializer_revision: crate::local_provider::SERIALIZER_REVISION,
            });
            &PROFILE
        }

        fn binding(&self) -> ProviderBinding {
            self.binding.clone()
        }

        async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.starts.send((req.request_id, req.priority)).unwrap();
            let _permit = self
                .gate
                .clone()
                .acquire_owned()
                .await
                .expect("fixture gate remains open");
            self.active.fetch_sub(1, Ordering::SeqCst);
            // Each request needs an explicit test release. Returning the
            // semaphore permit here would let completed work release its
            // successor and make this gate useless.
            std::mem::forget(_permit);
            Ok(InferenceResponse {
                content: "controlled fixture".into(),
                stop_reason: "end_turn".into(),
                raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                usage: Some(TokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }),
                ratelimit_headers: Vec::new(),
                citations: Vec::new(),
            })
        }

        async fn stream(
            &self,
            _request: &InferenceRequest,
            _publisher: &BoundedStreamPublisher,
        ) -> Result<crate::stream::StreamOutcome> {
            Err(anyhow!("the fixture does not stream"))
        }

        async fn count_tokens(&self, _request: &InferenceRequest) -> Result<TokenCount> {
            Err(anyhow!("the fixture does not count tokens"))
        }

        async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
            Err(anyhow!("the fixture does not probe"))
        }
    }

    /// A durable journal that records exactly what the publisher wrote.
    struct RecordingJournal {
        opened: Mutex<Vec<ProviderBinding>>,
        batches: Mutex<Vec<StreamBatch>>,
        terminals: Mutex<Vec<TerminationEvidence>>,
        closed: AtomicU64,
    }

    impl RecordingJournal {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                opened: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                terminals: Mutex::new(Vec::new()),
                closed: AtomicU64::new(0),
            })
        }

        /// Every journaled event across all batches, in wire order.
        fn journaled_events(&self) -> Vec<StreamEvent> {
            self.batches
                .lock()
                .expect("journal batches poisoned")
                .iter()
                .flat_map(|batch| batch.events.clone())
                .collect()
        }
    }

    impl StreamJournalSink for RecordingJournal {
        fn open_stream(
            &self,
            _request_id: EntityId,
            binding: &ProviderBinding,
        ) -> Result<(), StreamSinkError> {
            self.opened
                .lock()
                .expect("journal opens poisoned")
                .push(binding.clone());
            Ok(())
        }

        fn append_batch(
            &self,
            _request_id: EntityId,
            batch: &StreamBatch,
        ) -> Result<(), StreamSinkError> {
            self.batches
                .lock()
                .expect("journal batches poisoned")
                .push(batch.clone());
            Ok(())
        }

        fn append_terminal(
            &self,
            _request_id: EntityId,
            terminal: &TerminationEvidence,
        ) -> Result<(), StreamSinkError> {
            self.terminals
                .lock()
                .expect("journal terminals poisoned")
                .push(terminal.clone());
            Ok(())
        }

        fn close_stream(&self, _request_id: EntityId) -> Result<(), StreamSinkError> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fair_rotation_real_concurrency_fifo_and_zero_capacity_queueing() {
        let submit = |handle: &BrokerHandle, priority| {
            let request_id = EntityId::new();
            let (result_tx, result_rx) = oneshot::channel();
            handle
                .submit(InferenceRequest {
                    request_id,
                    agent_id: EntityId::new(),
                    conversation_id: EntityId::new(),
                    provider_binding: handle.binding().clone(),
                    priority,
                    epoch: ControllerEpoch(1),
                    payload: InferencePayload {
                        system_prompt: String::new(),
                        messages: vec![InferenceMessage::text(
                            "user",
                            "controlled scheduling fixture",
                        )],
                        max_tokens: Some(16),
                        tools: vec![],
                        native_options: BTreeMap::new(),
                    },
                    result_tx,
                })
                .unwrap();
            (request_id, result_rx)
        };

        // C=1: continuous Subject work cannot starve Low children.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(1)).unwrap()).unwrap();
        let (subject_one, subject_one_rx) = submit(&handle, InferencePriority::Subject);
        let (low_one, low_one_rx) = submit(&handle, InferencePriority::Low);
        let (subject_two, subject_two_rx) = submit(&handle, InferencePriority::Subject);
        let (low_two, low_two_rx) = submit(&handle, InferencePriority::Low);
        let run = tokio::spawn(broker.run());
        for (index, expected) in [subject_one, low_one, subject_two, low_two]
            .into_iter()
            .enumerate()
        {
            let observed = ControlledProvider::next_start(&mut starts).await;
            assert_eq!(observed.0, expected, "dispatch index {index}");
            assert_eq!(
                handle.active_count(),
                1,
                "active after dispatch index {index}"
            );
            provider.gate.add_permits(1);
        }
        for rx in [subject_one_rx, low_one_rx, subject_two_rx, low_two_rx] {
            assert_eq!(rx.await.unwrap().status, InferenceStatus::Success);
        }
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
        let observer = handle.admission.clone();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observer.active_count(), 0);

        // C=2: two same-tier futures really overlap and start in FIFO order.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(2)).unwrap()).unwrap();
        assert_eq!(handle.capacity().pool, CapacityPool::Compute);
        assert_eq!(handle.capacity().value, Some(2));
        assert_eq!(handle.capacity().provenance, CapacityProvenance::Configured);
        let (normal_one, normal_one_rx) = submit(&handle, InferencePriority::Normal);
        let (normal_two, normal_two_rx) = submit(&handle, InferencePriority::Normal);
        let run = tokio::spawn(broker.run());
        let observed = [
            ControlledProvider::next_start(&mut starts).await,
            ControlledProvider::next_start(&mut starts).await,
        ];
        assert_eq!(
            observed.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            vec![normal_one, normal_two]
        );
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(handle.active_count(), 2);
        provider.gate.add_permits(2);
        assert_eq!(
            normal_one_rx.await.unwrap().status,
            InferenceStatus::Success
        );
        assert_eq!(
            normal_two_rx.await.unwrap().status,
            InferenceStatus::Success
        );
        let observer = handle.admission.clone();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observer.active_count(), 0);

        // C=0: work stays pending instead of receiving synthetic capacity failure.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(0)).unwrap()).unwrap();
        let (_, mut pending_rx) = submit(&handle, InferencePriority::Subject);
        let run = tokio::spawn(broker.run());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut pending_rx)
                .await
                .is_err()
        );
        assert!(starts.try_recv().is_err());
        assert_eq!(handle.active_count(), 0);
        run.abort();
        let _ = run.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn frozen_local_wire_length_evidence_and_unknown_occupancy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (wire_tx, wire_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut body_start = None;
            let mut body_length = 0_usize;
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "request ended before complete headers/body");
                bytes.extend_from_slice(&chunk[..read]);
                if body_start.is_none() {
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let start = header_end + 4;
                        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                        body_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap();
                        body_start = Some(start);
                    }
                }
                if body_start.is_some_and(|start| bytes.len() >= start + body_length) {
                    break;
                }
            }
            let start = body_start.unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[start..start + body_length]).unwrap();
            wire_tx.send(body).unwrap();
            let raw = r#"{"id":"local-fixture","choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":3,"completion_tokens":7}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                raw.len(),
                raw
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let provider = crate::local_provider::LocalModelProvider::new(format!("http://{address}"), "fixture-local".into());
        let (broker, handle) =
            Broker::with_local_provider(provider, 1).expect("the local provider binds");
        let binding = handle.binding().clone();
        let payload = InferencePayload {
            system_prompt: "fixture system".into(),
            messages: vec![InferenceMessage::text("user", "fixture input")],
            max_tokens: Some(7),
            tools: vec![],
            native_options: BTreeMap::from([
                ("temperature".into(), serde_json::json!(0.25)),
                ("top_k".into(), serde_json::json!(40)),
            ]),
        };
        let request_id = EntityId::new();
        let (result_tx, result_rx) = oneshot::channel();
        let run = tokio::spawn(broker.run());
        handle
            .submit(InferenceRequest {
                request_id,
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: payload.clone(),
                result_tx,
            })
            .unwrap();

        let body = tokio::time::timeout(Duration::from_secs(2), wire_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body["model"], "fixture-local");
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["max_tokens"], 7);
        server.await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.status, InferenceStatus::Success);
        assert!(result.effective.matches_seal(&binding, &payload));
        assert_eq!(result.termination.kind, TerminationKind::ProviderTerminal);
        assert_eq!(result.termination.stop_reason.as_deref(), Some("length"));
        assert_eq!(result.termination.max_tokens, Some(7));
        assert_eq!(result.termination.output_tokens, Some(7));
        assert!(result.termination.length);

        // A changed model is a different seal, never an in-place provider override.
        let mut mismatched = binding.clone();
        let ProviderBinding::Local { model, .. } = &mut mismatched else {
            unreachable!()
        };
        *model = "different-revision".into();
        let (result_tx, _) = oneshot::channel();
        let error = handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: mismatched,
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: payload.clone(),
                result_tx,
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("MODEL_PROVIDER_BINDING_MISMATCH")
        );

        // Claude's dated restriction is scoped to Claude, not the local wire above.
        let sampling = BTreeMap::from([("temperature".into(), serde_json::json!(1.0))]);
        let error =
            crate::provider::merge_native_options(&mut serde_json::json!({}), &sampling, true)
                .unwrap_err();
        assert_eq!(error.code(), WireErrorCode::ForbiddenSampling);

        // The fixture is gone. Transport failure is unknown and keeps C=1 charged;
        // a later request cannot silently acquire that physical slot.
        let unknown_payload = InferencePayload {
            native_options: BTreeMap::new(),
            ..payload.clone()
        };
        let (result_tx, result_rx) = oneshot::channel();
        handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: unknown_payload.clone(),
                result_tx,
            })
            .unwrap();
        let unknown = tokio::time::timeout(Duration::from_secs(3), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unknown.status, InferenceStatus::Failed);
        assert_eq!(unknown.termination.kind, TerminationKind::Unknown);
        assert!(unknown.effective.matches_seal(&binding, &unknown_payload));
        assert_eq!(handle.active_count(), 1);

        let (result_tx, mut queued_rx) = oneshot::channel();
        handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding,
                priority: InferencePriority::Low,
                epoch: ControllerEpoch(1),
                payload: unknown_payload,
                result_tx,
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut queued_rx)
                .await
                .is_err()
        );
        let observer = handle.clone();
        drop(handle);
        run.abort();
        let _ = run.await;
        assert_eq!(observer.active_count(), 1);
    }

    /// Feed one body exactly the way the provider readers do: split on `\n`,
    /// keep the terminator on every line.
    fn feed_body(decoder: &mut SseDecoder, body: &str) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        let mut buffer: Vec<u8> = body.as_bytes().to_vec();
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            frames.extend(
                decoder.feed_line(&String::from_utf8_lossy(&line)),
            );
        }
        if !buffer.is_empty() {
            frames.extend(decoder.feed_line(&String::from_utf8_lossy(&buffer)));
        }
        frames
    }

    /// PR132 regression: the callers feed newline-retaining lines, so the
    /// frame separator arrives as `"\n"` (LF) or `"\r\n"` (CRLF). A boundary
    /// check that trimmed only `\r` never saw a blank line: no frame, no
    /// event, no stop reason — a valid stream ended as an empty unknown
    /// outcome that still held its admission charge. Both line endings must
    /// decode, and a genuinely empty stream must stay honestly empty.
    #[tokio::test(flavor = "current_thread")]
    async fn sse_frame_boundary_decodes_newline_retained_provider_lines() {
        // Decoder level: the real OpenAI wire shape, newline-retaining LF.
        let openai_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n",
            "\n",
            "data: [DONE]\n",
            "\n",
        );
        let mut decoder = SseDecoder::new();
        let frames = feed_body(&mut decoder, openai_body);
        assert_eq!(frames.len(), 5, "newline-retained separators end frames");
        assert_eq!(frames[4].data, "[DONE]");
        let events: Vec<StreamEvent> = frames
            .iter()
            .map(parse_openai_frame)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta { text: "Hel".into() },
                StreamEvent::TextDelta { text: "lo".into() },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: Some(11),
                        output_tokens: Some(7),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                    }
                },
                StreamEvent::MessageStop { stop_reason: "stop".into() },
            ],
            "events and stop reason decode from newline-retained lines"
        );

        // Decoder level: Anthropic event names over CRLF separators.
        let anthropic_body = concat!(
            "event: content_block_delta\r\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n",
            "\r\n",
            "event: message_delta\r\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":4}}\r\n",
            "\r\n",
        );
        let mut decoder = SseDecoder::new();
        let frames = feed_body(&mut decoder, anthropic_body);
        assert_eq!(frames.len(), 2, "CRLF separators end frames");
        assert_eq!(frames[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(
            frames[0].data,
            "{\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}"
        );
        let events: Vec<StreamEvent> = frames
            .iter()
            .map(parse_anthropic_frame)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta { text: "hi".into() },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: None,
                        output_tokens: Some(4),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                    }
                },
                StreamEvent::MessageStop { stop_reason: "end_turn".into() },
            ]
        );

        // A genuinely empty stream emits nothing; nothing is fabricated.
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.feed_line("\n"), None);
        assert_eq!(decoder.feed_line("\r\n"), None);
        assert_eq!(feed_body(&mut decoder, ": keepalive\n\n").len(), 0);

        // Broker level: the same SSE body over a real socket. Before the fix
        // this returned Success=false with an empty response, empty stop
        // reason and TerminationKind::Unknown while the charge stayed held.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bodies = [
            openai_body.to_string(),
            // Keepalive comments only: a valid stream with no events.
            ": keepalive\n: keepalive\n\n".to_string(),
        ];
        let server = tokio::spawn(async move {
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut body_start = None;
                let mut body_length = 0_usize;
                loop {
                    let mut chunk = [0_u8; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "request ended before complete headers/body");
                    bytes.extend_from_slice(&chunk[..read]);
                    if body_start.is_none() {
                        if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let start = header_end + 4;
                            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                            body_length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().ok())
                                        .flatten()
                                })
                                .unwrap();
                            body_start = Some(start);
                        }
                    }
                    if body_start.is_some_and(|start| bytes.len() >= start + body_length) {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        let provider = crate::local_provider::LocalModelProvider::new(
            format!("http://{address}"),
            "fixture-local".into(),
        );
        let (broker, handle) =
            Broker::with_local_provider(provider, 1).expect("the local provider binds");
        let binding = handle.binding().clone();
        let journal = RecordingJournal::new();
        let payload = InferencePayload {
            system_prompt: "fixture system".into(),
            messages: vec![InferenceMessage::text("user", "fixture input")],
            max_tokens: Some(16),
            tools: vec![],
            native_options: BTreeMap::new(),
        };
        let submit_stream = |payload: InferencePayload| {
            let request_id = EntityId::new();
            let (result_tx, result_rx) = oneshot::channel();
            handle
                .submit_stream(
                    BrokeredCall {
                        purpose: RequestPurpose::Subject,
                        request: InferenceRequest {
                            request_id,
                            agent_id: EntityId::new(),
                            conversation_id: EntityId::new(),
                            provider_binding: binding.clone(),
                            priority: InferencePriority::Subject,
                            epoch: ControllerEpoch(1),
                            payload,
                            result_tx,
                        },
                    },
                    StreamIdentity::default(),
                    Some(journal.clone()),
                    None,
                )
                .unwrap();
            (request_id, result_rx)
        };
        let run = tokio::spawn(broker.run());

        let (request_id, result_rx) = submit_stream(payload.clone());
        let result = tokio::time::timeout(Duration::from_secs(3), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.status, InferenceStatus::Success);
        assert_eq!(result.termination.kind, TerminationKind::ProviderTerminal);
        assert_eq!(result.termination.stop_reason.as_deref(), Some("stop"));
        let response = result.response.expect("decoded frames assemble a response");
        assert_eq!(response.content, "Hello");
        assert_eq!(response.stop_reason, "stop");
        assert_eq!(
            response.usage,
            Some(TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                cache_read_tokens: None,
                cache_write_tokens: None,
            })
        );
        let evidence = result.stream.expect("a streamed call keeps its evidence");
        assert_eq!(evidence.events, 4);
        assert_eq!(evidence.public_chunks, 2);
        assert!(evidence.journaled);
        assert_eq!(
            journal.journaled_events(),
            vec![
                StreamEvent::TextDelta { text: "Hel".into() },
                StreamEvent::TextDelta { text: "lo".into() },
                StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: Some(11),
                        output_tokens: Some(7),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                    }
                },
                StreamEvent::MessageStop { stop_reason: "stop".into() },
            ],
            "the durable journal records the decoded wire, nothing else"
        );
        assert_eq!(
            journal.terminals.lock().expect("terminals poisoned").as_slice(),
            &[TerminationEvidence {
                kind: TerminationKind::ProviderTerminal,
                stop_reason: Some("stop".into()),
                max_tokens: Some(16),
                output_tokens: Some(7),
                length: false,
            }]
        );

        // An empty stream is not rescued by the boundary fix: no frames, no
        // events, an honest unknown outcome that keeps its charge.
        let (_, result_rx) = submit_stream(payload.clone());
        let result = tokio::time::timeout(Duration::from_secs(3), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, InferenceStatus::Failed);
        assert_eq!(result.termination.kind, TerminationKind::Unknown);
        let response = result.response.expect("an empty stream keeps an empty response");
        assert!(response.content.is_empty(), "no fabricated content");
        assert!(response.stop_reason.is_empty(), "no fabricated stop reason");
        assert_eq!(response.usage, None);
        let evidence = result.stream.expect("a streamed call keeps its evidence");
        assert_eq!(evidence.events, 0);
        assert_eq!(evidence.public_chunks, 0);
        assert!(evidence.journaled);
        assert_eq!(
            journal.journaled_events().len(),
            4,
            "the empty stream journaled no event"
        );
        assert_eq!(journal.closed.load(Ordering::SeqCst), 2);
        assert_eq!(
            handle.active_count(),
            1,
            "the unknown outcome still holds its admission charge"
        );

        server.await.unwrap();
        drop(submit_stream);
        let observer = handle.clone();
        drop(handle);
        run.abort();
        let _ = run.await;
        assert_eq!(observer.active_count(), 1);
    }
}
