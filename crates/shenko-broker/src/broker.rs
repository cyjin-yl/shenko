//! Inference broker — fair asynchronous dispatch over explicitly bound physical
//! resource domains.
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
use std::sync::{Arc, Mutex, MutexGuard};
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy};
use shenko_types::ids::EntityId;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use crate::admission::{AdmissionController, AdmissionGuard, CapacityPool, CapacitySnapshot};
use crate::capability::BackendCapabilitySnapshot;
use crate::error_sink::{ErrorSinkCell, RuntimeErrorSink, new_error_sink_cell, report_for};
use crate::execution::{ExecutionContext, execute_request, now_ms};
use crate::kv::{
    AllocationPlan, AutomaticKvPolicy, KvAllocator, KvCapabilities, KvError, KvPoolState,
    ResidencyIdentity,
};
use crate::registry::{ProviderRegistry, ProviderRuntime, TokenCount};
use crate::request::{
    BrokeredCall, InferenceRequest, InferenceResponse, InferenceResult,
    ProviderBinding, ProviderCapabilities, ProviderVisionCaps, RequestPurpose, TerminationKind,
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
    meta: Option<ServedModelMetadata>,
}

#[derive(Deserialize)]
struct ServedModelMetadata {
    /// llama.cpp reports the actual per-sequence serving limit here.
    /// `n_ctx_train` is deliberately not a fallback: training capacity does
    /// not establish how much context this running endpoint serves.
    n_ctx: Option<u32>,
}

#[derive(Deserialize)]
struct ModelMetadataList {
    data: Vec<ModelMetadata>,
    has_more: Option<bool>,
}

/// An advertised metadata response is structurally invalid, or the bound
/// model's limits are invalid or contradictory. Callers may continue without
/// an observation, but must not hide a rejected response as an optional probe
/// failure and admit the model as unknown/unbounded.
#[derive(Debug, thiserror::Error)]
#[error("invalid model metadata: {0}")]
pub struct InvalidModelMetadata(String);

impl ProviderCapabilities {
    fn from_metadata(binding: ProviderBinding, metadata: ModelMetadata) -> Result<Self> {
        ensure!(metadata.id == binding.model(), "metadata model id differs from bound model");
        // `max_tokens` is deliberately absent: Anthropic defines that as the
        // output-parameter ceiling, not the input context window.
        let limits = [metadata.max_model_len, metadata.max_context_tokens,
            metadata.max_input_tokens, metadata.context_length,
            metadata.meta.and_then(|meta| meta.n_ctx)]
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
    resources: RequestResources,
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

#[derive(Clone)]
struct RequestResources {
    domain: Arc<ResourceDomain>,
    kv: Arc<KvAllocator>,
}

struct AccountedRequest {
    /// The original physical owner survives routing changes and reconciliation
    /// through a different caller's handle.
    resources: RequestResources,
    // None: queued/live with a worker-owned guard. Some: retained Unknown debt.
    charge: Option<AdmissionGuard>,
}

type Accounting = Arc<Mutex<HashMap<EntityId, AccountedRequest>>>;
type RecoverySources = Arc<Mutex<HashMap<EntityId, Arc<dyn InferenceRecoverySource>>>>;
type DomainKvAllocators = Arc<Mutex<HashMap<String, Arc<KvAllocator>>>>;

fn lock_broker_state<'a, T>(state: &'a Mutex<T>, name: &str) -> Result<MutexGuard<'a, T>> {
    state.lock().map_err(|_| {
        ProtocolError::new("BROKER_STATE_POISONED", ErrorScope::Request, RetryStrategy::NeverAutomatically)
            .with_details(serde_json::json!({ "state": name, "remote_debt_released": false }))
            .into()
    })
}

#[derive(Clone)]
pub struct BrokerHandle {
    tx: mpsc::UnboundedSender<Submission>,
    binding: ProviderBinding,
    /// The compute budget of this handle's physical domain.
    /// Kept for reading charge counters; dispatch never decides through this
    /// handle — the domain's own `try_admit` is the single budget owner.
    pub(crate) admission: AdmissionController,
    domain: Arc<ResourceDomain>,
    domains: Arc<ResourceDomainRegistry>,
    registry: Arc<ProviderRegistry>,
    accounting: Accounting,
    recovery_sources: RecoverySources,
    accounting_changed: Arc<Notify>,
    kv: Arc<KvAllocator>,
    domain_kv: DomainKvAllocators,
    error_sink: ErrorSinkCell,
    health_observer: crate::health::HealthObserverCell,
    handoff_closed: Arc<std::sync::atomic::AtomicBool>,
    stop: tokio_util::sync::CancellationToken,
}

impl BrokerHandle {
    /// Close local admission and ask owned provider observations to end.
    /// The runner must still be joined. Unknown remote occupancy is retained,
    /// and a provider with an uninterruptible worker may delay that join.
    pub fn request_stop(&self) {
        self.handoff_closed.store(true, std::sync::atomic::Ordering::Release);
        self.stop.cancel();
    }

    pub fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    pub fn set_health_observer(&self, observer: Arc<dyn crate::health::InferenceHealthObserver>) {
        *self.health_observer.write() = Some(observer);
    }

    /// Probe this handle's exact binding, independently of the dispatch loop.
    /// It issues only provider metadata reads and admits no model generation.
    pub async fn probe_binding(&self) -> Result<crate::health::BindingProbe> {
        let Some(runtime) = self.registry.runtime_for(&self.binding) else {
            return Ok(crate::health::BindingProbe {
                binding: self.binding.clone(), observed_at: chrono::Utc::now(),
                usability: crate::health::BindingUsability::Unavailable,
                reason: "no installed runtime can serve this exact configured binding".into(),
                capabilities: None, snapshot: None,
            });
        };
        let snapshot = runtime.probe().await?;
        Ok(crate::health::classify_probe(self.binding.clone(), snapshot))
    }

    /// Same physical scheduling domain and configured transport, distinct
    /// exact model binding. Existence/availability is decided by the provider,
    /// not by this selector or an invented model-capability table.
    pub fn for_model(&self, model: &str) -> Result<Self> {
        Ok(Self { binding: self.binding.with_model(model)?, ..self.clone() })
    }

    /// Use one complete already-frozen provider binding on the same queue.
    /// This selects neither a model by name nor replacement credentials. The
    /// installed runtime must still match, and its physical domain owns both
    /// compute admission and residency accounting.
    pub fn for_binding(&self, binding: &ProviderBinding) -> Result<Self> {
        binding.validate_endpoint()?;
        ensure!(self.registry.runtime_for(binding).is_some(),
            "MODEL_PROVIDER_BINDING_UNAVAILABLE: no installed runtime matches the frozen binding");
        let resources = self.resources_for_binding(binding)?;
        Ok(Self {
            binding: binding.clone(), admission: resources.domain.admission().clone(),
            domain: resources.domain, kv: resources.kv, ..self.clone()
        })
    }

    fn resources_for_binding(&self, binding: &ProviderBinding) -> Result<RequestResources> {
        let domain = if self.binding.same_transport(binding) {
            Arc::clone(&self.domain)
        } else if let Some(profile) = binding.profile() {
            self.domains.resolve(profile)?
        } else {
            let runtime = self.registry.runtime_for(binding)
                .ok_or_else(|| anyhow!("original legacy binding has no installed resource-domain mapping"))?;
            self.domains.resolve(runtime.profile())?
        };
        let mut allocators = lock_broker_state(&self.domain_kv, "domain_kv")?;
        let kv = Arc::clone(allocators.entry(domain.identity().domain_id).or_insert_with(||
            Arc::new(KvAllocator::new(Box::new(AutomaticKvPolicy::default()), KvCapabilities::default()))));
        Ok(RequestResources { domain, kv })
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

    /// Installed profile inventory, without runtime availability or reachability
    /// checks. Profile enumeration and binding lookup are separate reads, so
    /// replacement can also change the inventory during collection. Use
    /// `serves_model` for fresh eligibility on this handle's exact transport.
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

    /// Fresh eligibility on this handle's exact installed profile and transport:
    /// the runtime must declare the model and currently report availability.
    /// This is not a probe or a reservation, and must not gate reconciliation
    /// of an original request. `for_model` and `runtime_for` retain their
    /// transport-compatible recovery semantics.
    pub fn serves_model(&self, model: &str) -> bool {
        self.registry.serves_model(&self.binding, model)
    }

    /// Restore an original armed request before admitting any new inference.
    /// Repeated recovery and live requests cannot count the same identity twice.
    pub fn recover_unknown(&self, request_id: EntityId) -> Result<()> {
        let mut accounting = lock_broker_state(&self.accounting, "accounting")?;
        let entry = accounting.entry(request_id).or_insert_with(|| AccountedRequest {
            resources: RequestResources { domain: Arc::clone(&self.domain), kv: Arc::clone(&self.kv) },
            charge: Some(self.domain.recover_unknown()),
        });
        entry.resources.domain
            .quarantine(request_id, now_ms(), "recovered without terminal evidence");
        Ok(())
    }

    /// Register the original ledger reader while restoring a request. A known
    /// request keeps the reader installed by its original submit/restore; later
    /// callers cannot replace it with a bool or a fabricated result source.
    pub fn register_recovered(&self, source: Arc<dyn InferenceRecoverySource>) -> Result<()> {
        let id = source.request_id();
        {
            let accounting = lock_broker_state(&self.accounting, "accounting")?;
            let mut sources = lock_broker_state(&self.recovery_sources, "recovery_sources")?;
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
        let source = lock_broker_state(&self.recovery_sources, "recovery_sources")?
            .get(&request_id).cloned().ok_or_else(|| anyhow!("request has no original durable recovery reader"))?;
        let result = source.read_terminal_result()?;
        if let Some(result) = &result {
            ensure!(result.request_id == source.request_id()
                && result.effective.matches_seal(source.binding(), source.payload())
                && result.termination.kind != TerminationKind::Unknown,
                "recovered inference lacks matching original terminal evidence");
        }
        let recovered_resources = if result.is_none() {
            let pinned = lock_broker_state(&self.accounting, "accounting")?
                .get(&request_id).map(|entry| entry.resources.clone());
            Some(match pinned {
                Some(resources) => resources,
                None => self.resources_for_binding(source.binding())?,
            })
        } else { None };
        let mut accounting = lock_broker_state(&self.accounting, "accounting")?;
        let mut sources = lock_broker_state(&self.recovery_sources, "recovery_sources")?;
        // Reading the ledger must not hold the broker lock. A real terminal
        // completion may retire this reader while that read is in progress:
        // its stale None must not recreate debt after release, nor may an old
        // reader release a later registration using the same request key.
        if !sources.get(&request_id).is_some_and(|current| Arc::ptr_eq(current, &source)) {
            return Ok(());
        }
        let Some(result) = result else {
            let resources = recovered_resources
                .ok_or_else(|| anyhow!("unknown recovery has no resolved original domain"))?;
            accounting.entry(request_id).or_insert_with(|| {
                let charge = resources.domain.recover_unknown();
                resources.domain.quarantine(request_id, now_ms(), "recovered without terminal evidence");
                AccountedRequest { resources, charge: Some(charge) }
            });
            return Ok(());
        };
        if let Some((guard, resources)) = accounting.get_mut(&request_id)
            .and_then(|entry| entry.charge.take().map(|guard| (guard, entry.resources.clone())))
        {
            accounting.remove(&request_id);
            if result.termination.kind == TerminationKind::NeverDispatched {
                guard.release_never_dispatched();
            } else {
                // Matching terminal evidence was validated before taking the
                // charge; Unknown never reaches a capacity-release path.
                guard.release_terminal();
            }
            resources.domain.reconcile_debt(request_id);
            // The original terminal evidence ends the context read hold the
            // unknown outcome kept charged; the context may now be reclaimed
            // at the next safe boundary.
            resources.kv
                .release_unknown_read(&mut resources.domain.kv_mut(), &request_id.to_string());
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
            let accounting = lock_broker_state(&self.accounting, "accounting")?;
            self.handoff_closed.store(true, std::sync::atomic::Ordering::Release);
            drop(accounting);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let snapshot = {
                let accounting = lock_broker_state(&self.accounting, "accounting")?;
                if accounting.values().any(|entry| entry.charge.is_none()) { None } else {
                    let sources = lock_broker_state(&self.recovery_sources, "recovery_sources")?;
                    let requests = accounting.iter().map(|(id, entry)| {
                        let source = sources.get(id);
                        serde_json::json!({
                            "request_id": id, "state": "unknown", "charged": entry.charge.is_some(),
                            "resource_domain": entry.resources.domain.identity(),
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

    pub fn resume_after_handoff_abort(&self) -> Result<()> {
        let accounting = lock_broker_state(&self.accounting, "accounting")?;
        self.handoff_closed.store(false, std::sync::atomic::Ordering::Release);
        drop(accounting);
        self.accounting_changed.notify_one();
        Ok(())
    }

    pub fn capacity(&self) -> CapacitySnapshot {
        self.admission.capacity()
    }

    /// The capacity that is actually available now: the same figure the
    /// dispatch path enforces — configured budget minus held charges and
    /// enforceable external reservations of this domain.
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
        let previous = match self.error_sink.lock() {
            Ok(mut guard) => guard.replace(sink),
            Err(error) => {
                warn!(%error, "runtime error sink could not be installed");
                return;
            }
        };
        // A replaced plugin may own destructors; retire it after unlocking.
        drop(previous);
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

    /// Submit a streamed controlled call whose result the caller awaits.
    ///
    /// Same contract as [`BrokerHandle::submit_stream`], plus the receipt
    /// owner: the caller receives the terminal result exactly like a
    /// non-streamed call, so the sealed request, the committed decision and
    /// the streamed surface stay one dispatch. The stream is an additional
    /// ordered surface, never a replacement for the result.
    pub fn submit_stream_recorded(
        &self,
        req: InferenceRequest,
        identity: StreamIdentity,
        journal: Option<Arc<dyn StreamJournalSink>>,
        public: Option<Arc<dyn PublicStreamSink>>,
        sink: Arc<dyn InferenceReceiptSink>,
    ) -> Result<()> {
        ensure!(
            journal.is_some(),
            "streamed inference needs a durable journal sink; content is never dropped silently"
        );
        let purpose = RequestPurpose::from_priority(req.priority);
        self.submit_inner(
            req,
            Some(sink),
            purpose,
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
        let id = req.request_id;
        if let Some(sink) = &receipt_sink {
            ensure!(sink.request_id() == id && sink.binding() == &req.provider_binding
                && sink.payload() == &req.payload, "receipt owner differs from submitted request seal");
        }
        let mut accounting = lock_broker_state(&self.accounting, "accounting")?;
        ensure!(!self.handoff_closed.load(std::sync::atomic::Ordering::Acquire) && !self.stop.is_cancelled(),
            "BROKER_HANDOFF_CLOSED: request rejected before enqueue/send");
        ensure!(!accounting.contains_key(&req.request_id), "broker request already submitted or uncertain; reconcile original identity");
        // Keep both maps locked through enqueue and its rollback: a failed
        // send must not need another fallible lock to undo local admission.
        let mut sources = lock_broker_state(&self.recovery_sources, "recovery_sources")?;
        if let Some(sink) = &receipt_sink {
            let source: Arc<dyn InferenceRecoverySource> = sink.clone();
            ensure!(!sources.contains_key(&id), "request already has an original receipt owner");
            sources.insert(id, source);
        }
        let resources = RequestResources { domain: Arc::clone(&self.domain), kv: Arc::clone(&self.kv) };
        accounting.insert(id, AccountedRequest { resources: resources.clone(), charge: None });
        if let Err(failed) = self.tx.send(Submission { request: req, receipt_sink, purpose, stream, resources }) {
            accounting.remove(&id);
            let source = sources.remove(&id);
            drop(sources);
            drop(accounting);
            drop(source);
            drop(failed);
            return Err(ProtocolError::new("BROKER_ADMISSION_CLOSED", ErrorScope::Request, RetryStrategy::AfterReconcile)
                .with_details(serde_json::json!({ "request_id": id, "enqueued": false }))
                .into());
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

    fn len(&self) -> usize {
        self.lanes.iter().map(VecDeque::len).sum()
    }

    /// Each distinct blocked domain can wake the one shared dispatcher when
    /// its external reservation changes. No polling interval or second queue.
    async fn capacity_changed(&self) {
        let mut domains = HashMap::new();
        for submission in self.lanes.iter().flatten() {
            domains.entry(submission.resources.domain.identity().domain_id)
                .or_insert_with(|| Arc::clone(&submission.resources.domain));
        }
        let domains = domains.into_values().collect::<Vec<_>>();
        let mut notifications = domains.iter()
            .map(|domain| Box::pin(domain.budget_changed().notified())).collect::<Vec<_>>();
        std::future::poll_fn(|context| {
            for notification in &mut notifications {
                if std::future::Future::poll(notification.as_mut(), context).is_ready() {
                    return std::task::Poll::Ready(());
                }
            }
            std::task::Poll::Pending
        }).await
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
    guard: Option<AdmissionGuard>,
    resources: RequestResources,
    /// Whether execution submitted a specific cause to the fail-soft reporter.
    /// This does not acknowledge a durable journal write.
    cause_report_attempted: bool,
}

pub struct Broker {
    rx: mpsc::UnboundedReceiver<Submission>,
    domain: Arc<ResourceDomain>,
    registry: Arc<ProviderRegistry>,
    accounting: Accounting,
    recovery_sources: RecoverySources,
    accounting_changed: Arc<Notify>,
    kv: Arc<KvAllocator>,
    error_sink: ErrorSinkCell,
    health_observer: crate::health::HealthObserverCell,
    stop: tokio_util::sync::CancellationToken,
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
        let domain = default_domain(AdmissionController::new(max));
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
        let domain = default_domain(AdmissionController::new(max));
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
        Self::with_registry(registry, domain)
    }

    /// Bind the registry's primary runtime to the supplied physical domain.
    /// Additional runtime profiles use explicitly registered domains/aliases;
    /// they share this broker's queue, not an invented second budget.
    /// Invalid primary profile aliases fail construction before a queue is published.
    pub fn with_registry(
        registry: Arc<ProviderRegistry>,
        domain: Arc<ResourceDomain>,
    ) -> Result<(Self, BrokerHandle)> {
        Self::assemble(registry, domain)
    }

    fn from_env_with_admission(admission: AdmissionController) -> Result<(Self, BrokerHandle)> {
        let runtime = env_runtime()?;
        let domain = default_domain(admission);
        let registry = Arc::new(ProviderRegistry::new());
        registry.install(Arc::clone(&runtime));
        Self::assemble(registry, domain)
    }

    fn assemble(
        registry: Arc<ProviderRegistry>,
        domain: Arc<ResourceDomain>,
    ) -> Result<(Self, BrokerHandle)> {
        let domains = Arc::new(ResourceDomainRegistry::new());
        domains.register(Arc::clone(&domain))?;
        let (tx, rx) = mpsc::unbounded_channel();
        // The binding comes from the installed runtime, not from a profile
        // table: the runtime is what actually executes the request.
        let binding = registry
            .primary_binding()
            .unwrap_or(ProviderBinding::Stub { codec: 2 });
        if let Some(runtime) = registry.runtime_for(&binding) {
            let domain_id = domain.identity().domain_id;
            for alias in [&runtime.profile().endpoint_origin, &runtime.profile().profile_id] {
                domains.bind_alias(alias, &domain_id)?;
            }
        }
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
        let domain_kv = Arc::new(Mutex::new(HashMap::from([
            (domain.identity().domain_id, Arc::clone(&kv)),
        ])));
        let error_sink = new_error_sink_cell();
        let health_observer: crate::health::HealthObserverCell = Arc::new(parking_lot::RwLock::new(None));
        let stop = tokio_util::sync::CancellationToken::new();
        Ok((
            Self {
                rx,
                domain: Arc::clone(&domain),
                registry: Arc::clone(&registry),
                accounting: Arc::clone(&accounting),
                recovery_sources: Arc::clone(&recovery_sources),
                accounting_changed: Arc::clone(&accounting_changed),
                kv: Arc::clone(&kv),
                error_sink: Arc::clone(&error_sink),
                health_observer: Arc::clone(&health_observer),
                stop: stop.clone(),
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
                domain_kv,
                error_sink,
                health_observer,
                handoff_closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                stop,
            },
        ))
    }

    /// Explicit offline construction; does not inspect ambient credentials and
    /// returns typed Unavailable rather than a fabricated successful completion.
    pub fn offline() -> Result<(Self, BrokerHandle)> {
        let registry = Arc::new(ProviderRegistry::new());
        registry.install(Arc::new(OfflineRuntime));
        let domain = default_domain(AdmissionController::nominal());
        Self::assemble(registry, domain)
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
        if let Err(error) = self.kv.rebalance(&mut pool) {
            warn!(%error, "observed KV capabilities adopted but residency rebalance failed");
        }
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
        capabilities_from_snapshot(&binding, &snapshot)
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

    fn finish(
        completed: CompletedRequest,
        accounting: &Accounting,
        recovery_sources: &RecoverySources,
        error_sink: &ErrorSinkCell,
        health_observer: &crate::health::HealthObserverCell,
    ) -> Result<()> {
        let CompletedRequest { request, receipt_sink, result, guard, resources, cause_report_attempted } = completed;
        let domain = &resources.domain;
        let kv = &resources.kv;
        let request_id = request.request_id;
        let settlement = (|| -> Result<()> {
            if let Some(sink) = receipt_sink {
                if let Err(error) = sink.record(&result) {
                    domain.quarantine(request_id, now_ms(), "model receipt not durably recorded");
                    match lock_broker_state(accounting, "accounting") {
                        Ok(mut accounting) => { accounting.insert(request_id,
                            AccountedRequest { resources: resources.clone(), charge: guard }); }
                        Err(retention_error) => {
                            // Dropping an unreleased guard keeps occupancy even
                            // when its bookkeeping lock is unavailable.
                            return Err(error.context(format!(
                                "model receipt not durably recorded; retaining charge also failed: {retention_error:#}"
                            )));
                        }
                    }
                    return Err(error.context("model receipt not durably recorded; original charge retained"));
                }
            }
            // Dropping the receiver does not cancel the already executing
            // provider future or establish its remote termination.
            let mut accounting = lock_broker_state(accounting, "accounting")?;
            let mut sources = lock_broker_state(recovery_sources, "recovery_sources")?;
            let retired_source = match result.termination.kind {
                TerminationKind::ProviderTerminal | TerminationKind::TransportRejected | TerminationKind::NeverDispatched => {
                    accounting.remove(&request_id);
                    let source = sources.remove(&request_id);
                    domain.reconcile_debt(request_id);
                    kv.release_unknown_read(&mut domain.kv_mut(), &request_id.to_string());
                    if let Some(guard) = guard {
                        if result.termination.kind == TerminationKind::NeverDispatched {
                            guard.release_never_dispatched();
                        } else {
                            guard.release_terminal();
                        }
                    }
                    source
                }
                TerminationKind::Unknown => {
                    accounting.insert(request_id,
                        AccountedRequest { resources: resources.clone(), charge: guard });
                    domain.quarantine(request_id, now_ms(), "the provider outcome could not be determined");
                    None
                }
            };
            drop(sources);
            drop(accounting);
            drop(retired_source);
            Ok(())
        })();
        if let Err(error) = settlement {
            domain.quarantine(request_id, now_ms(), "inference completion could not be durably settled");
            crate::error_sink::journal_runtime_error(error_sink, &report_for(
                &request, result.purpose, "BROKER_COMPLETION_FAILED",
                "inference completion could not be durably settled; original capacity retained",
            ));
            return Err(error.context(format!("inference completion settlement failed for {request_id}")));
        }
        // A cause report is fail-soft: it does not establish persistence or
        // release Unknown occupancy. The retained original receipt and guard
        // own recovery; only an outcome with no specific cause gets this report.
        if result.termination.kind == TerminationKind::Unknown && !cause_report_attempted {
            crate::error_sink::journal_runtime_error(
                error_sink,
                &report_for(&request, result.purpose, "INFERENCE_OUTCOME_UNKNOWN", "provider outcome unknown"),
            );
        }
        crate::health::notify(health_observer, &request.provider_binding, &result);
        // The caller may have disappeared; the durable receipt above does not
        // depend on it.
        let _ = request.result_tx.send(result);
        Ok(())
    }

    pub async fn run(mut self) {
        info!("inference broker running");
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<CompletedRequest>();
        let mut queue = FairQueue::default();
        let mut input_closed = false;
        let mut inflight = 0_u64;
        let mut prefer_input = false;
        let workers = tokio_util::task::TaskTracker::new();
        let mut stopping = false;
        enum Event {
            Completed(Option<CompletedRequest>),
            Incoming(Option<Submission>),
            AccountingChanged,
            Stop,
        }

        loop {
            if self.stop.is_cancelled() && !stopping {
                stopping = true;
                self.rx.close();
            }
            // Move the submissions already waiting at this dispatch boundary
            // into the priority lanes before spending a newly free slot.
            // Otherwise a background queue can dispatch ahead of a fresh IM
            // merely because the completion arm won the previous select.
            // Snapshot the count: a producer flood cannot starve settlement.
            let ready = self.rx.len();
            for _ in 0..ready {
                match self.rx.try_recv() {
                    Ok(submission) => queue.push(submission),
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        input_closed = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            // Visit each ready submission at most once. A blocked physical
            // domain must not prevent another domain from running; put its
            // requests back in their original lane order after this pass.
            let ready = queue.len();
            let mut blocked = Vec::new();
            for _ in 0..ready {
                let rotation = (queue.main_next, queue.normal_child_next, queue.last_was_urgent);
                let submission = queue.pop().expect(
                    "invariant: FairQueue::is_empty and pop inspect the same lanes without intervening mutation"
                );
                if stopping {
                    // This submission never acquired a physical slot or
                    // crossed this broker's send boundary. Publish its exact
                    // queued cancellation without inventing a compute charge.
                    let result = crate::execution::stopped_before_dispatch(&submission.request, submission.purpose);
                    let completed = CompletedRequest {
                        request: submission.request, receipt_sink: submission.receipt_sink,
                        result, guard: None, resources: submission.resources,
                        cause_report_attempted: false,
                    };
                    if let Err(error) = Self::finish(completed, &self.accounting,
                        &self.recovery_sources, &self.error_sink, &self.health_observer)
                    {
                        warn!(error=%format!("{error:#}"), "queued inference stop could not be durably settled");
                    }
                    continue;
                }
                let Some(guard) = submission.resources.domain.try_admit() else {
                    // A failed admission does not consume a fairness turn.
                    (queue.main_next, queue.normal_child_next, queue.last_was_urgent) = rotation;
                    blocked.push(submission);
                    continue;
                };
                let registry = Arc::clone(&self.registry);
                let error_sink = Arc::clone(&self.error_sink);
                let health_observer = Arc::clone(&self.health_observer);
                let accounting = Arc::clone(&self.accounting);
                let recovery_sources = Arc::clone(&self.recovery_sources);
                let completed_tx = completed_tx.clone();
                let stop = self.stop.clone();
                workers.spawn(async move {
                    let Submission { request, receipt_sink, purpose, stream, resources } = submission;
                    let executed = execute_request(
                        &request,
                        receipt_sink.as_ref(),
                        purpose,
                        stream,
                        &ExecutionContext {
                            registry: &registry,
                            domain: &resources.domain,
                            kv: &resources.kv,
                            error_sink: &error_sink,
                            stop: &stop,
                        },
                    )
                    .await;
                    let completed = CompletedRequest {
                        request,
                        receipt_sink,
                        result: executed.result,
                        cause_report_attempted: executed.cause_report_attempted,
                        guard: Some(guard),
                        resources,
                    };
                    if let Err(failed) = completed_tx.send(completed) {
                        // Losing the dispatch loop does not revoke the
                        // original durable receipt owner's completion.
                        let completed = failed.0;
                        if let Err(error) = Self::finish(completed, &accounting, &recovery_sources, &error_sink, &health_observer) {
                            warn!(error=%format!("{error:#}"), "model completion settlement failed");
                        }
                    }
                });
                inflight += 1;
            }
            for submission in blocked.into_iter().rev() {
                queue.lanes[submission.request.priority as usize].push_front(submission);
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
                    _ = self.stop.cancelled(), if !stopping => Event::Stop,
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    _ = self.accounting_changed.notified() => Event::AccountingChanged,
                    _ = queue.capacity_changed() => Event::AccountingChanged,
                    else => std::future::pending::<Event>().await,
                }
            } else {
                tokio::select! {
                    biased;
                    _ = self.stop.cancelled(), if !stopping => Event::Stop,
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    _ = self.accounting_changed.notified() => Event::AccountingChanged,
                    _ = queue.capacity_changed() => Event::AccountingChanged,
                    else => std::future::pending::<Event>().await,
                }
            };
            match event {
                Event::Completed(completed) => {
                    if let Some(completed) = completed {
                        inflight -= 1;
                        if let Err(error) = Self::finish(completed, &self.accounting, &self.recovery_sources, &self.error_sink, &self.health_observer) {
                            warn!(error=%format!("{error:#}"), "model completion settlement failed");
                        }
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
                Event::Stop => {}
            }
        }
        // Completion delivery and the worker future returning are different
        // boundaries. Join every owned local worker before the runner ends.
        workers.close();
        workers.wait().await;
        info!("inference broker stopped");
    }
}

pub(crate) fn capabilities_from_snapshot(
    binding: &ProviderBinding, snapshot: &BackendCapabilitySnapshot,
) -> Result<ProviderCapabilities> {
    let mut has_more = false;
    let mut capabilities = None;
    for document in &snapshot.raw_evidence {
        // Metrics and diagnostics are separate evidence, not failed model
        // lists. A document presenting a data member must parse as one.
        if document.get("data").is_none() {
            continue;
        }
        let list = ModelMetadataList::deserialize(document)
            .map_err(|error| InvalidModelMetadata(error.to_string()))?;
        has_more |= list.has_more == Some(true);
        for metadata in list.data {
            if metadata.id != binding.model() {
                continue;
            }
            let observed = ProviderCapabilities::from_metadata(binding.clone(), metadata)
                .map_err(|error| InvalidModelMetadata(error.to_string()))?;
            if capabilities.as_ref().is_some_and(|prior| prior != &observed) {
                return Err(InvalidModelMetadata("conflicting observations for the bound model".into()).into());
            }
            capabilities = Some(observed);
        }
    }
    if let Some(capabilities) = capabilities {
        return Ok(capabilities);
    }
    Err(ProtocolError::new(
        if has_more { "PROVIDER_MODEL_METADATA_INCOMPLETE" } else { "PROVIDER_MODEL_METADATA_MISSING" },
        ErrorScope::Request,
        RetryStrategy::NewRevision,
    ).with_details(serde_json::json!({ "model": binding.model(), "has_more": has_more })).into())
}

/// The provider runtime selected from the environment.
fn env_runtime() -> Result<Arc<dyn ProviderRuntime>> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() == Ok("1") {
        let provider = crate::local_provider::LocalModelProvider::from_env()?;
        info!(model=%provider.model, "local model provider selected (docs/specs/nodes.md#spec-22-1)");
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
fn default_domain(admission: AdmissionController) -> Arc<ResourceDomain> {
    let capacity = admission.capacity();
    ResourceDomain::new(
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
    )
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

    async fn complete(&self, _request: &InferenceRequest) -> Result<InferenceResponse> {
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
        InferenceMessage, InferencePayload, InferencePriority, InferenceStatus,
        TerminationEvidence, TokenUsage, WireErrorCode,
    };
    use crate::stream::{
        SseDecoder, SseFrame, StreamBatch, StreamEvent, StreamSinkError,
        parse_anthropic_frame, parse_openai_frame,
    };
    use shenko_types::ids::{ControllerEpoch, EntityId};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Semaphore, mpsc, oneshot};

    #[derive(Default)]
    struct RecordingHealth(Mutex<Vec<(EntityId, ProviderBinding, crate::health::BindingUsability)>>);
    impl crate::health::InferenceHealthObserver for RecordingHealth {
        fn observe(&self, binding: &ProviderBinding, result: &InferenceResult) -> Result<()> {
            self.0.lock().unwrap().push((result.request_id, binding.clone(), crate::health::inference_usability(result)));
            Ok(())
        }
    }

    struct ControlledProvider {
        binding: ProviderBinding,
        starts: mpsc::UnboundedSender<(EntityId, InferencePriority)>,
        gate: Arc<Semaphore>,
        active: AtomicU64,
        max_active: AtomicU64,
        panic_next: AtomicBool,
    }

    struct ActiveCall<'a>(&'a AtomicU64);
    impl Drop for ActiveCall<'_> {
        fn drop(&mut self) { self.0.fetch_sub(1, Ordering::SeqCst); }
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
                        request_timeout_ms: None,
                    },
                    starts,
                    gate: Arc::new(Semaphore::new(0)),
                    active: AtomicU64::new(0),
                    max_active: AtomicU64::new(0),
                    panic_next: AtomicBool::new(false),
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
            let _active = ActiveCall(&self.active);
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.starts.send((req.request_id, req.priority)).unwrap();
            let _permit = self
                .gate
                .clone()
                .acquire_owned()
                .await
                .expect("fixture gate remains open");
            // Each request needs an explicit test release. Returning the
            // semaphore permit here would let completed work release its
            // successor and make this gate useless.
            std::mem::forget(_permit);
            assert!(!self.panic_next.swap(false, Ordering::SeqCst),
                "controlled provider panic after original dispatch");
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
            request: &InferenceRequest,
            publisher: &BoundedStreamPublisher,
        ) -> Result<crate::stream::StreamOutcome> {
            // The fixture really streams: the public text is published in two
            // deltas, the second one carrying a control line that must never
            // reach the reader surface.
            self.starts.send((request.request_id, request.priority)).unwrap();
            publisher.push(
                crate::stream::StreamEvent::TextDelta { text: "第一段。".into() },
                1,
            )?;
            publisher.push(
                crate::stream::StreamEvent::TextDelta {
                    text: "第二段。\n[reply:private:12345] 不该出现在公开流里\n".into(),
                },
                2,
            )?;
            publisher.push(
                crate::stream::StreamEvent::MessageStop { stop_reason: "end_turn".into() },
                3,
            )?;
            Ok(crate::stream::StreamOutcome {
                response: InferenceResponse {
                    content: "第一段。\n第二段。\n[reply:private:12345] 不该出现在公开流里\n".into(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: Some(TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(4),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                    }),
                    ratelimit_headers: Vec::new(),
                    citations: Vec::new(),
                },
                events: 3,
                batches: 1,
            })
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
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(1))).unwrap();
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

        // A fresh IM queued while one background generation runs wins its
        // next finite boundary, even when ordinary submissions arrived first.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(1))).unwrap();
        let (running, running_rx) = submit(&handle, InferencePriority::Low);
        let run = tokio::spawn(broker.run());
        assert_eq!(ControlledProvider::next_start(&mut starts).await.0, running);
        let (background, background_rx) = submit(&handle, InferencePriority::Low);
        tokio::task::yield_now().await;
        let (interactive, interactive_rx) = submit(&handle, InferencePriority::Urgent);
        provider.gate.add_permits(1);
        assert_eq!(ControlledProvider::next_start(&mut starts).await.0, interactive);
        provider.gate.add_permits(1);
        assert_eq!(ControlledProvider::next_start(&mut starts).await.0, background,
            "interactive priority preserves background progress");
        provider.gate.add_permits(1);
        for rx in [running_rx, interactive_rx, background_rx] {
            assert_eq!(rx.await.unwrap().status, InferenceStatus::Success);
        }
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run).await.unwrap().unwrap();

        // C=2: two same-tier futures really overlap and start in FIFO order.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(2))).unwrap();
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
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(0))).unwrap();
        let (_, mut pending_rx) = submit(&handle, InferencePriority::Subject);
        let run = tokio::spawn(broker.run());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut pending_rx)
                .await
                .is_err()
        );
        assert!(starts.try_recv().is_err());
        assert_eq!(handle.active_count(), 0);
        handle.request_stop();
        let cancelled = tokio::time::timeout(Duration::from_secs(2), pending_rx).await.unwrap().unwrap();
        assert_eq!(cancelled.status, InferenceStatus::Cancelled);
        assert_eq!(cancelled.termination.kind, TerminationKind::NeverDispatched);
        tokio::time::timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert!(starts.try_recv().is_err());
        assert_eq!(handle.active_count(), 0);

        // An owned provider future really panics after dispatch. Its original
        // receipt must still reach Unknown, keep C charged and let the runner
        // join, without admitting the pending request behind it.
        let (provider, mut starts) = ControlledProvider::new();
        provider.panic_next.store(true, Ordering::SeqCst);
        let (broker, handle) = Broker::with_runtime(provider.clone(), default_domain(AdmissionController::new(1))).unwrap();
        let request_id = EntityId::new();
        let payload = InferencePayload {
            system_prompt: String::new(),
            messages: vec![InferenceMessage::text("user", "owned panic boundary")],
            max_tokens: Some(16), tools: vec![], native_options: BTreeMap::new(),
        };
        let receipt = Arc::new(RecordingReceipt {
            request_id, binding: handle.binding().clone(), payload: payload.clone(),
            results: Mutex::new(Vec::new()),
        });
        let (result_tx, result_rx) = oneshot::channel();
        handle.submit_recorded(InferenceRequest {
            request_id, agent_id: EntityId::new(), conversation_id: EntityId::new(),
            provider_binding: handle.binding().clone(), priority: InferencePriority::Subject,
            epoch: ControllerEpoch(1), payload: payload.clone(), result_tx,
        }, receipt.clone()).unwrap();
        let (_, mut pending_rx) = submit(&handle, InferencePriority::Low);
        let run = tokio::spawn(broker.run());
        assert_eq!(ControlledProvider::next_start(&mut starts).await.0, request_id);
        provider.gate.add_permits(1);
        let unknown = tokio::time::timeout(Duration::from_secs(2), result_rx).await.unwrap().unwrap();
        assert_eq!(unknown.request_id, request_id);
        assert_eq!(unknown.status, InferenceStatus::Failed);
        assert_eq!(unknown.termination.kind, TerminationKind::Unknown);
        assert!(unknown.effective.matches_seal(handle.binding(), &payload));
        assert_eq!(provider.active.load(Ordering::SeqCst), 0, "provider future must unwind its real local owner");
        assert_eq!(handle.active_count(), 1, "panic does not release remote debt");
        assert!(matches!(pending_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        handle.request_stop();
        let cancelled = tokio::time::timeout(Duration::from_secs(2), pending_rx).await.unwrap().unwrap();
        assert_eq!(cancelled.status, InferenceStatus::Cancelled);
        assert_eq!(cancelled.termination.kind, TerminationKind::NeverDispatched);
        tokio::time::timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        assert_eq!(provider.active.load(Ordering::SeqCst), 0);
        assert_eq!(handle.active_count(), 1);
        assert!(starts.try_recv().is_err(), "no queued send or replay may follow the panic");
        let recorded = receipt.results.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].request_id, request_id);
        assert_eq!(recorded[0].termination.kind, TerminationKind::Unknown);
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
        let health = Arc::new(RecordingHealth::default());
        handle.set_health_observer(health.clone());
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
        assert_eq!(health.0.lock().unwrap()[0], (request_id, binding.clone(), crate::health::BindingUsability::Available));

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
        assert_eq!(health.0.lock().unwrap().last().unwrap().2, crate::health::BindingUsability::Unknown);
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
    /// The reader surface a test watches.
    #[derive(Default)]
    struct RecordingPublic {
        chunks: Mutex<Vec<crate::stream::PublicTextChunk>>,
        terminals: Mutex<usize>,
    }

    impl crate::stream::PublicStreamSink for RecordingPublic {
        fn publish_text(
            &self,
            chunk: &crate::stream::PublicTextChunk,
        ) -> Result<(), crate::stream::StreamSinkError> {
            self.chunks.lock().expect("public chunks poisoned").push(chunk.clone());
            Ok(())
        }

        fn publish_terminal(
            &self,
            _request_id: EntityId,
            _terminal: &TerminationEvidence,
        ) -> Result<(), crate::stream::StreamSinkError> {
            *self.terminals.lock().expect("public terminals poisoned") += 1;
            Ok(())
        }
    }

    /// A receipt owner that only remembers what it was handed.
    struct RecordingReceipt {
        request_id: EntityId,
        binding: ProviderBinding,
        payload: InferencePayload,
        results: Mutex<Vec<InferenceResult>>,
    }

    impl InferenceRecoverySource for RecordingReceipt {
        fn request_id(&self) -> EntityId {
            self.request_id
        }

        fn binding(&self) -> &ProviderBinding {
            &self.binding
        }

        fn payload(&self) -> &InferencePayload {
            &self.payload
        }

        fn read_terminal_result(&self) -> anyhow::Result<Option<InferenceResult>> {
            Ok(self.results.lock().expect("receipt results poisoned").last().cloned())
        }
    }

    impl InferenceReceiptSink for RecordingReceipt {
        fn may_dispatch(&self) -> anyhow::Result<DispatchFence> {
            Ok(DispatchFence::Send)
        }

        fn record(&self, result: &InferenceResult) -> anyhow::Result<()> {
            self.results.lock().expect("receipt results poisoned").push(result.clone());
            Ok(())
        }
    }

    /// A streamed call reaches the reader surface AND the receipt owner —
    /// The stream is an additional ordered surface, never a
    /// replacement for the recorded result, and a control line never crosses
    /// into the reader's text.
    #[tokio::test(flavor = "current_thread")]
    async fn a_streamed_call_reaches_the_reader_surface_and_the_receipt_owner() {
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_runtime(
            provider.clone(),
            default_domain(AdmissionController::new(1)),
        )
        .expect("the fixture binds");
        let binding = handle.binding().clone();
        let public = Arc::new(RecordingPublic::default());
        let journal = RecordingJournal::new();
        let request_id = EntityId::new();
        let payload = InferencePayload {
            system_prompt: "fixture system".into(),
            messages: vec![InferenceMessage::text("user", "fixture input")],
            max_tokens: Some(16),
            tools: vec![],
            native_options: BTreeMap::new(),
        };
        let receipt = Arc::new(RecordingReceipt {
            request_id,
            binding: binding.clone(),
            payload: payload.clone(),
            results: Mutex::new(Vec::new()),
        });
        let (result_tx, result_rx) = oneshot::channel();
        handle
            .submit_stream_recorded(
                InferenceRequest {
                    request_id,
                    agent_id: EntityId::new(),
                    conversation_id: EntityId::new(),
                    provider_binding: binding.clone(),
                    priority: InferencePriority::Subject,
                    epoch: ControllerEpoch(1),
                    payload,
                    result_tx,
                },
                StreamIdentity {
                    message_id: EntityId::new().to_string(),
                    part_id: "text/0".into(),
                    stream_id: request_id,
                    attempt_id: EntityId::new(),
                },
                Some(journal.clone()),
                Some(public.clone()),
                receipt.clone(),
            )
            .expect("the streamed submission is admitted");
        let run = tokio::spawn(broker.run());
        let started = ControlledProvider::next_start(&mut starts).await;
        assert_eq!(started.0, request_id);

        let result = tokio::time::timeout(Duration::from_secs(3), result_rx)
            .await
            .expect("the streamed call answers")
            .expect("the streamed call delivered its result");
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.status, InferenceStatus::Success);

        // The reader surface saw the public text, in order, under the identity
        // the committed projection also uses.
        let chunks = public.chunks.lock().expect("public chunks poisoned").clone();
        assert!(!chunks.is_empty(), "the reader surface received nothing");
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.part_id == "text/0" && chunk.stream_id == request_id),
            "every chunk carries its part and stream identity"
        );
        let text: String = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        assert!(text.contains("第一段。"), "public text: {text}");
        assert!(
            !text.contains("[reply:private:12345]"),
            "a control line never reaches the reader surface: {text}"
        );

        // The durable journal saw the stream: ordered batches and a terminal.
        let journaled = journal.journaled_events();
        assert!(
            journaled.iter().any(|event| matches!(
                event,
                crate::stream::StreamEvent::TextDelta { text } if text.contains("第一段。")
            )),
            "the journal kept the public text"
        );
        assert_eq!(journal.closed.load(Ordering::SeqCst), 1, "the stream was closed");

        // The receipt owner received the terminal result: a streamed call is
        // still a recorded call.
        let recorded = receipt.results.lock().expect("receipt results poisoned").clone();
        assert_eq!(recorded.len(), 1, "the receipt owner recorded the result");
        assert_eq!(recorded[0].request_id, request_id);

        drop(handle);
        run.abort();
        let _ = run.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_enforceable_external_reservation_limits_real_dispatch() {
        use crate::resource::ExternalClientUsage;

        let submit = |handle: &BrokerHandle| {
            let request_id = EntityId::new();
            let (result_tx, result_rx) = oneshot::channel();
            handle
                .submit(InferenceRequest {
                    request_id,
                    agent_id: EntityId::new(),
                    conversation_id: EntityId::new(),
                    provider_binding: handle.binding().clone(),
                    priority: InferencePriority::Normal,
                    epoch: ControllerEpoch(1),
                    payload: InferencePayload {
                        system_prompt: String::new(),
                        messages: vec![InferenceMessage::text("user", "external reservation fixture")],
                        max_tokens: Some(16),
                        tools: vec![],
                        native_options: BTreeMap::new(),
                    },
                    result_tx,
                })
                .unwrap();
            (request_id, result_rx)
        };

        // C=2 plus one enforceable external unit: only one local request may
        // enter the provider, and the diagnostic figure must equal what the
        // dispatch path enforces.
        let (provider, mut starts) = ControlledProvider::new();
        let domain = default_domain(AdmissionController::new(2));
        let (broker, handle) = Broker::with_runtime(provider.clone(), Arc::clone(&domain)).unwrap();
        domain.register_external_client(ExternalClientUsage {
            client_ref: "ext-1".into(),
            units: 1,
            enforceable: true,
            evidence: None,
        });
        assert_eq!(
            domain.effective_compute_capacity().value,
            Some(1),
            "the diagnostic capacity shows the one remaining local slot"
        );
        let (first, first_rx) = submit(&handle);
        let (second, second_rx) = submit(&handle);
        let run = tokio::spawn(broker.run());
        let observed = ControlledProvider::next_start(&mut starts).await;
        assert_eq!(observed.0, first);
        assert_eq!(handle.active_count(), 1);
        assert_eq!(
            domain.effective_compute_capacity().value,
            Some(0),
            "while the local slot is occupied nothing else may enter"
        );
        // The second local request stays queued: the provider is not entered
        // a second time even though its configured capacity is 2.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), starts.recv())
                .await
                .is_err(),
            "the external reservation kept the second request out of the provider"
        );
        // Releasing the first request lets the queued one through: the
        // reservation bound concurrency, not request identity.
        provider.gate.add_permits(1);
        assert_eq!(first_rx.await.unwrap().status, InferenceStatus::Success);
        let observed = ControlledProvider::next_start(&mut starts).await;
        assert_eq!(observed.0, second);
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
        provider.gate.add_permits(1);
        assert_eq!(second_rx.await.unwrap().status, InferenceStatus::Success);
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_external_reservation_release_resumes_queued_dispatch() {
        use crate::resource::ExternalClientUsage;

        // C=1 plus one enforceable external unit: nothing local may dispatch;
        // releasing the reservation must wake the queued request.
        let (provider, mut starts) = ControlledProvider::new();
        let domain = default_domain(AdmissionController::new(1));
        let (broker, handle) = Broker::with_runtime(provider.clone(), Arc::clone(&domain)).unwrap();
        domain.register_external_client(ExternalClientUsage {
            client_ref: "ext-1".into(),
            units: 1,
            enforceable: true,
            evidence: None,
        });
        assert_eq!(domain.effective_compute_capacity().value, Some(0));
        let request_id = EntityId::new();
        let (result_tx, result_rx) = oneshot::channel();
        handle
            .submit(InferenceRequest {
                request_id,
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: handle.binding().clone(),
                priority: InferencePriority::Normal,
                epoch: ControllerEpoch(1),
                payload: InferencePayload {
                    system_prompt: String::new(),
                    messages: vec![InferenceMessage::text("user", "resume after release fixture")],
                    max_tokens: Some(16),
                    tools: vec![],
                    native_options: BTreeMap::new(),
                },
                result_tx,
            })
            .unwrap();
        let run = tokio::spawn(broker.run());
        assert!(
            tokio::time::timeout(Duration::from_millis(200), starts.recv())
                .await
                .is_err(),
            "the full reservation blocks every local request"
        );
        // The reservation is released; the queued request must be dispatched
        // without a second submission.
        assert!(domain.release_external_client("ext-1"));
        assert_eq!(domain.effective_compute_capacity().value, Some(1));
        let observed = ControlledProvider::next_start(&mut starts).await;
        assert_eq!(observed.0, request_id);
        provider.gate.add_permits(1);
        assert_eq!(result_rx.await.unwrap().status, InferenceStatus::Success);
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
    }
}
