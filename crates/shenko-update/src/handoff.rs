//! The activation coordinator — SPEC §9.5, §35.7, §41.12.
//!
//! One `UpdateService` owns the hand-over state machine of one subject:
//!
//! ```text
//! prepared → copying → catch_up → smoke → handoff → activated → observing → retired
//! ```
//!
//! Every step is a real, inspectable change:
//!
//! * **prepared** — the request is validated and the activation record is
//!   created in the live ledger.
//! * **copying** — the candidate gets its own state copy, taken with the
//!   SQLite backup API.
//! * **catch_up** — the candidate imports the facts committed after the
//!   copy.
//! * **smoke** — the candidate program actually starts against its own copy
//!   and reports readiness. A failed or absent report is an error that stops
//!   the preparation: no activation record is created for it. The swap still
//!   re-reads the sealed report before quiescing, so a record whose saved
//!   evidence failed or is missing can never replace the working runtime.
//! * **handoff** — the subject pauses at a boundary, the participants release
//!   their resources, the candidate imports the final watermark and the
//!   selector swaps in one ledger transaction.
//! * **observing / retired** — the old closure is watched, then retired while
//!   the new facts it may still produce stay importable.
//!
//! A rollback is a new activation back to the previous generation, never a
//! phase: the new generation's facts are imported into the old copy before
//! the selector swaps back, so no fact is lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use shenko_store::Store;
use shenko_store::runtime_boundary::{SubjectQuiesce, SubjectQuiesceState};
use shenko_types::activation::{
    ActivationOrigin, ActivationPhase, ActivationRecord, ActivationSelector, PrimaryClaim,
    RouteAuthority,
};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration};
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionRef};
use shenko_types::subject::{SubjectBranchHead, SubjectMode};

use crate::build::{self, BuildOutcome, BuildProvider, CandidateArtifact};
use crate::migration::{self, ImportReport};
use crate::slots::{self, AbSlot, SlotLayout, SLOT_DB_FILENAME};
use crate::smoke::{self, SmokeReport};

macro_rules! bail {
    ($($arg:tt)*) => { return Err(anyhow::anyhow!($($arg)*).into()) };
}
macro_rules! ensure {
    ($condition:expr, $($arg:tt)*) => {
        if !$condition { bail!($($arg)*); }
    };
}

/// What one participant actually did while quiescing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffParticipantReceipt {
    pub participant: String,
    /// The concrete resources that were stopped, e.g. process identities.
    /// An empty list is an honest receipt: it means nothing was running.
    pub stopped: Vec<String>,
    pub details: serde_json::Value,
}

/// A component whose resources must not survive a generation switch.
#[async_trait::async_trait]
pub trait HandoffParticipant: Send + Sync {
    fn name(&self) -> &str;
    /// Stop admitting new work and release the resources bound to the old
    /// generation. The returned receipt names what was actually stopped.
    async fn quiesce(&self) -> anyhow::Result<HandoffParticipantReceipt>;
    /// The activation was abandoned before the selector swap: the old
    /// generation continues and may take the resources back.
    async fn resume(&self) -> anyhow::Result<()>;
}

/// Where the committed primary claim comes from. Wired by the container
/// (the node domain) — never from model input. When no source is
/// configured, the hand-over records the single-node local profile
/// explicitly (`expected_primary_claim: None`).
pub trait PrimaryClaimSource: Send + Sync {
    fn committed_primary_claim(&self) -> Result<Option<PrimaryClaim>>;
    /// Explicit authority for an isolated local branch, never a global
    /// quorum claim. Absence of both bindings is not authority.
    fn local_branch_authority(&self) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

/// The participants of a hand-over, in registration order.
pub struct HandoffParticipantRegistry {
    participants: parking_lot::Mutex<Vec<Arc<dyn HandoffParticipant>>>,
    inactive_owner_proven: bool,
    subject_driver_running: std::sync::atomic::AtomicBool,
}

impl Default for HandoffParticipantRegistry {
    fn default() -> Self {
        Self { participants: parking_lot::Mutex::new(Vec::new()), inactive_owner_proven: false,
            subject_driver_running: std::sync::atomic::AtomicBool::new(true) }
    }
}

impl HandoffParticipantRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The caller must hold the installation's exclusive RuntimeOwner guard
    /// throughout this registry's use. Connection failure is not proof.
    pub fn for_exclusively_owned_stopped_installation() -> Self {
        Self { participants: parking_lot::Mutex::new(Vec::new()), inactive_owner_proven: true,
            subject_driver_running: std::sync::atomic::AtomicBool::new(false) }
    }

    /// The composition root knows whether it actually spawned the subject
    /// loop. Lack of a model is not evidence of a fictional running loop.
    pub fn bind_subject_driver(&self, running: bool) {
        self.subject_driver_running.store(running, std::sync::atomic::Ordering::Release);
    }

    /// Whether the caller really holds the installation's exclusive runtime
    /// owner. Connection failure is not proof, and neither is a name.
    pub fn exclusive_owner_proven(&self) -> bool {
        self.inactive_owner_proven
    }

    pub fn register(&self, participant: Arc<dyn HandoffParticipant>) {
        self.participants.lock().push(participant);
    }

    pub fn register_admission_barrier(&self, participant: Arc<dyn HandoffParticipant>) {
        self.participants.lock().insert(0, participant);
    }

    pub fn unregister(&self, name: &str) {
        self.participants.lock().retain(|participant| participant.name() != name);
    }

    pub async fn quiesce_all(&self) -> Result<Vec<HandoffParticipantReceipt>> {
        let participants: Vec<_> = self.participants.lock().clone();
        ensure!(!participants.is_empty() || self.inactive_owner_proven,
            "HANDOFF_UNBOUND: active runtime resource handles unavailable");
        let mut receipts = Vec::with_capacity(participants.len());
        for (index, participant) in participants.iter().enumerate() {
            match participant.quiesce().await {
                Ok(receipt) => receipts.push(receipt),
                Err(error) => {
                    let mut failures = Vec::new();
                    // The failing participant may already have released some
                    // resources: include it in compensating resume.
                    for released in participants[..=index].iter().rev() {
                        if let Err(error) = released.resume().await {
                            failures.push(format!("{}: {error:#}", released.name()));
                        }
                    }
                    bail!("participant {} failed to quiesce: {error:#}; resume failures: {failures:?}", participant.name());
                }
            }
        }
        Ok(receipts)
    }

    pub async fn resume_all(&self) -> Result<()> {
        let participants: Vec<_> = self.participants.lock().clone();
        let mut failures = Vec::new();
        for participant in participants.iter().rev() {
            if let Err(error) = participant.resume().await {
                failures.push(format!("{}: {error:#}", participant.name()));
            }
        }
        ensure!(failures.is_empty(), "handoff resume incomplete: {failures:?}");
        Ok(())
    }
}

/// Where a candidate artifact comes from.
#[derive(Debug, Clone)]
pub enum CandidateSource {
    /// Build it with the configured build provider.
    Build {
        source_dir: PathBuf,
        profile: crate::build::BuildProfile,
        target: Option<String>,
        extra_args: Vec<String>,
    },
    /// Accept an artifact that was built elsewhere.
    Prebuilt { path: PathBuf },
}

/// One prepare request.
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    pub agent_id: EntityId,
    /// The generation the candidate becomes.
    pub generation: RuntimeGeneration,
    pub source: CandidateSource,
    /// The committed primary claim this hand-over is prepared under. It is
    /// re-read before the swap; a changed claim refuses the swap.
    pub primary_claim: Option<PrimaryClaim>,
}

/// The current activation state of a subject — read-only, deterministic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationStatus {
    pub selector: Option<ActivationSelector>,
    pub launch_receipt: Option<serde_json::Value>,
    pub open_activation: Option<ActivationRecord>,
    pub history: Vec<ActivationRecord>,
    pub live_slot: Option<String>,
    pub generations: Vec<String>,
}

/// What a recovery read found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub selector: Option<ActivationSelector>,
    pub open_activation: Option<ActivationRecord>,
    pub generations: Vec<String>,
    /// What a recovery cannot explain yet. Each entry names the exact
    /// missing prerequisite.
    pub missing: Vec<String>,
}

/// What a cold-start reconciliation of an interrupted hand-over found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffReconciliationState {
    /// The subject has no paused boundary, or the pause is not an open
    /// hand-over. Nothing was changed.
    NothingToDo,
    /// The candidate selector already committed. The retired writer's
    /// mirror/fence publication was completed; it must never run again.
    SelectionCommitted,
    /// The selection never committed. This process is the same controller
    /// generation and resumed its own boundary: no new epoch, generation or
    /// second primary was created.
    ResumedSameController,
}

/// The durable answer to "was this pause an interrupted hand-over, and which
/// side owns it now?".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffReconciliation {
    pub state: HandoffReconciliationState,
    pub agent_id: EntityId,
    /// The concrete observation behind the decision.
    pub reason: String,
    pub selector: Option<ActivationSelector>,
    pub activation_id: Option<EntityId>,
    pub activation_phase: Option<ActivationPhase>,
    pub request_id: Option<EntityId>,
    pub mode_before: SubjectMode,
    pub mode_after: Option<SubjectMode>,
    pub epoch: Option<ControllerEpoch>,
    pub generation: Option<RuntimeGeneration>,
    /// Whether the same controller really took its boundary back.
    pub resumed: bool,
    /// The sealed copy of this answer in the shared CAS.
    pub receipt_ref: Option<ObjectRef>,
}

/// The part of the live control transport's frozen request that a cold
/// reconciliation can verify. The exact command and helper digest belong to
/// the helper that was going to execute it.
#[derive(Debug, Deserialize)]
struct FrozenProcessHandoff {
    schema_version: u32,
    request_id: EntityId,
    agent_id: EntityId,
    installation_id: EntityId,
    source_selector: ActivationSelector,
    source_slot: PathBuf,
}

/// The outcome of an activate() or rollback() call.
#[derive(Debug, Clone)]
pub struct ActivationOutcome {
    pub record: ActivationRecord,
    pub selector: ActivationSelector,
    pub commit: CommitRef,
    pub fenced_head: SubjectBranchHead,
    pub participants: Vec<HandoffParticipantReceipt>,
}

/// The one update service of an installation.
pub struct UpdateService {
    /// The installation's data directory.
    data_dir: PathBuf,
    /// The live slot's directory. Its database is the live store's database.
    live_slot: PathBuf,
    /// The live store of the current generation.
    store: Arc<Store>,
    /// The shared content store of the installation.
    cas_dir: PathBuf,
    /// Components that must release their resources before the switch.
    participants: Arc<HandoffParticipantRegistry>,
    /// Where candidate artifacts come from.
    builder: Arc<dyn BuildProvider>,
    /// The node-committed primary claim, re-read before the swap.
    claim_source: Option<Arc<dyn PrimaryClaimSource>>,
    /// The subject's owned versioned-root chain (SPEC §51.4). The controller
    /// owns its manifest and storage; an installation without the controller
    /// contract cannot roll the state root back and says so instead of
    /// pretending a file copy is one.
    root_chain: Option<Arc<dyn crate::rollback::RootChain>>,
    /// The mechanism that owns the external-effect ledger. Rollback reads it
    /// once so the appended node's receipt can say what was not undone.
    external_effects: Option<Arc<dyn crate::rollback::ExternalEffects>>,
}

/// The errors of the update service.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no activation selector; the launch must create the first one")]
    SelectorMissing,
    #[error("an activation is already open for this subject: {0}")]
    ActivationOpen(EntityId),
    #[error("activation {0} not found")]
    ActivationNotFound(EntityId),
    #[error("activation {0} is in phase {1:?}, expected {2:?}")]
    PhaseMismatch(EntityId, ActivationPhase, ActivationPhase),
    #[error("the subject did not pause before the hand-over deadline")]
    SubjectNotPaused,
    #[error("the migration left blocked units: {0:?}")]
    MigrationBlocked(Vec<String>),
    #[error("the primary claim changed between preparation and the swap")]
    PrimaryClaimChanged,
    #[error("the smoke launch failed the candidate: {0}")]
    SmokeFailed(String),
    #[error("the candidate store rejected the state: {0}")]
    CandidateStore(#[from] ShenkoError),
    #[error(transparent)]
    RootRollback(#[from] crate::rollback::RollbackError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type UpdateResult<T, E = UpdateError> = std::result::Result<T, E>;

impl UpdateService {
    pub fn new(
        data_dir: PathBuf,
        live_slot: PathBuf,
        store: Arc<Store>,
        participants: Arc<HandoffParticipantRegistry>,
        builder: Arc<dyn BuildProvider>,
    ) -> Self {
        let cas_dir = data_dir.join("cas");
        Self {
            data_dir,
            live_slot,
            store,
            cas_dir,
            participants,
            builder,
            claim_source: None,
            root_chain: None,
            external_effects: None,
        }
    }

    pub fn with_claim_source(mut self, source: Arc<dyn PrimaryClaimSource>) -> Self {
        self.claim_source = Some(source);
        self
    }

    /// Bind the subject's versioned-root chain (SPEC §51.4).
    ///
    /// The chain is the controller's, not this crate's: nothing here defines a
    /// root manifest, selector or node layout. Wiring is additive, so an
    /// installation that has not adopted the controller contract keeps
    /// working and its root rollback refuses with the missing contract named.
    pub fn with_root_chain(mut self, chain: Arc<dyn crate::rollback::RootChain>) -> Self {
        self.root_chain = Some(chain);
        self
    }

    /// Bind the external-effect ledger read by root rollback.
    pub fn with_external_effects(mut self, effects: Arc<dyn crate::rollback::ExternalEffects>) -> Self {
        self.external_effects = Some(effects);
        self
    }

    /// The bound root chain, if the controller contract has been wired.
    pub fn root_chain(&self) -> Option<Arc<dyn crate::rollback::RootChain>> {
        self.root_chain.clone()
    }

    /// The bound external-effect ledger, if one is installed.
    pub fn external_effects(&self) -> Option<Arc<dyn crate::rollback::ExternalEffects>> {
        self.external_effects.clone()
    }

    /// The installation's shared Store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// SPEC §51.4 append-node rollback of the owned versioned root.
    ///
    /// This appends a new node whose content equals the selected historical
    /// node; it never rewinds, truncates or deletes the chain or the ledger
    /// log, and `agent_id` keeps its pre-switch value. External effects are
    /// reconciled and recorded as not undone.
    ///
    /// The selected *software generation* is untouched: that is
    /// [`UpdateService::rollback`], a different operation with its own record.
    pub fn rollback_root(
        &self,
        agent_id: EntityId,
        request: &crate::rollback::RollbackRequest,
    ) -> UpdateResult<crate::rollback::RollbackOutcome> {
        let chain = self.root_chain.as_ref().ok_or_else(|| {
            anyhow!(
                "ROOT_CHAIN_UNAVAILABLE: §51.4 state-root rollback needs the owned versioned-root \
                 controller contract (node select + append-node over one manifest covering home \
                 files, software slots, cognition selectors, WASM linear memory, execution stack \
                 and interpreter object graph); no chain is bound to this installation"
            )
        })?;
        let subject = self.store.get_existing_agent_id()?;
        ensure!(
            subject == Some(agent_id),
            "rollback subject {agent_id} is not this installation's subject"
        );
        let external = self.external_effects_reconciliation(agent_id);
        Ok(crate::rollback::append_rollback(
            &self.store,
            chain.as_ref(),
            request,
            external,
        )?)
    }

    /// What the effect ledger shows at rollback time.
    ///
    /// Without a ledger source the record says `Unobserved`: that is a gap in
    /// the evidence, never a claim that nothing was sent.
    fn external_effects_reconciliation(
        &self,
        agent_id: EntityId,
    ) -> crate::rollback::ExternalEffectReconciliation {
        use crate::rollback::ExternalEffectReconciliation as Reconciliation;
        match self.external_effects.as_ref() {
            Some(source) => match source.unreconciled(agent_id) {
                Ok(items) => Reconciliation::Observed { items },
                Err(error) => Reconciliation::Unobserved {
                    detail: format!(
                        "the external-effect ledger could not be read, so no effect is claimed \
                         undone and none is claimed absent: {error:#}"
                    ),
                },
            },
            None => Reconciliation::Unobserved {
                detail: "no external-effect ledger source is installed; this receipt neither \
                         claims that nothing left the process nor marks any effect undone"
                    .to_owned(),
            },
        }
    }

    pub fn current_primary_claim(&self) -> UpdateResult<Option<PrimaryClaim>> {
        let source = self.claim_source.as_ref().ok_or_else(|| anyhow!("AUTHORITY_UNBOUND"))?;
        Ok(source.committed_primary_claim()?)
    }

    /// Live phase ends here. The final watermark and selection are executed
    /// by a separate helper only AFTER it acquires the installation owner
    /// released by this runtime's actual process teardown.
    pub async fn quiesce_for_process_handoff(&self, activation_id: EntityId, request_id: EntityId,
        expected_selector_revision: u64) -> UpdateResult<()> {
        let mut record = self.store.activation_record(activation_id)?
            .ok_or(UpdateError::ActivationNotFound(activation_id))?;
        let selected = self.store.activation_selector(record.agent_id)?.ok_or(UpdateError::SelectorMissing)?;
        ensure!(selected.selector_revision == expected_selector_revision, "selector revision changed before process handoff");
        self.authority_snapshot()?;
        self.quiesce_subject(record.agent_id, request_id).await?;
        let receipts = match self.participants.quiesce_all().await {
            Ok(receipts) => receipts,
            Err(error) => {
                let resume = self.store.resume_subject_quiesce(record.agent_id, request_id);
                bail!("resource quiesce failed: {error:#}; subject resume: {resume:?}");
            }
        };
        record.pending_operations_ref = Some(self.seal_json(&receipts, "activation", "process-handoff-resources")?);
        record.inference_allocations_ref = record.pending_operations_ref.clone();
        self.store.update_activation_record(&record, RecordRevision(record.record_revision))?;
        Ok(())
    }

    pub async fn resume_process_handoff(&self, agent: EntityId, request_id: EntityId) -> UpdateResult<()> {
        let resources = self.participants.resume_all().await;
        let subject = self.store.resume_subject_quiesce(agent, request_id);
        ensure!(resources.is_ok() && subject.is_ok(), "process handoff abort incomplete: resources={resources:?},subject={subject:?}");
        Ok(())
    }

    /// Whether this process really owns the installation's exclusive runtime
    /// lease. Only that proves no other local writer exists.
    pub fn exclusive_owner_proven(&self) -> bool {
        self.participants.exclusive_owner_proven()
    }

    /// Reconcile a subject boundary left behind by an interrupted hand-over.
    ///
    /// A live activate/rollback freezes its request, quiesces the subject and
    /// hands the final catch-up and the selector CAS to a finite helper. If
    /// that helper dies before the CAS — killed, crashed, or refused at spawn —
    /// the installation is left with a paused head, its open quiesce request
    /// and an open activation record, and the old side cannot take the boundary
    /// back on its own: the private request id only ever existed in the dead
    /// process and in its frozen file. The next owner of the installation
    /// therefore reconciles deterministically:
    ///
    /// * the caller must hold the exclusive `RuntimeOwner`, so no second local
    ///   writer and no still-waiting helper exists;
    /// * a committed selection is never undone: the retired writer is mirrored
    ///   and fenced instead, and the new generation owns the subject;
    /// * a paused head without its own durable boundary record is refused, not
    ///   guessed;
    /// * a pause that is not an open hand-over (an operator's `runtime pause`)
    ///   is left exactly as it is;
    /// * the resumed boundary keeps the same controller epoch and generation,
    ///   so no second primary and no duplicated dispatch appears;
    /// * the abandoned activation keeps its identity, its stopped-resource
    ///   receipts and the sealed reason it was abandoned.
    pub fn reconcile_interrupted_handoff(&self, agent_id: EntityId) -> UpdateResult<HandoffReconciliation> {
        ensure!(self.participants.exclusive_owner_proven(),
            "HANDOFF_OWNER_UNPROVEN: reconciling an interrupted hand-over needs the installation's exclusive runtime owner");
        let (head, _) = self.store.load_subject(agent_id).map_err(UpdateError::CandidateStore)?;
        let selector = self.store.activation_selector(agent_id).map_err(UpdateError::CandidateStore)?;
        let mut answer = HandoffReconciliation {
            state: HandoffReconciliationState::NothingToDo,
            agent_id,
            reason: String::new(),
            selector: selector.clone(),
            activation_id: None,
            activation_phase: None,
            request_id: None,
            mode_before: head.mode,
            mode_after: None,
            epoch: Some(head.controller_epoch),
            generation: Some(head.software_generation.clone()),
            resumed: false,
            receipt_ref: None,
        };
        if head.mode == SubjectMode::Active {
            answer.reason = "the subject has no paused boundary".to_string();
            return Ok(answer);
        }
        if head.mode != SubjectMode::Paused {
            // A `handoff` head is the retired, already-fenced writer of a
            // committed selection: reviving it would create a second primary.
            bail!("HANDOFF_NOT_RESUMABLE: the subject mode is {:?}; this copy is not the resumable controller", head.mode);
        }
        let request = self.store.open_subject_quiesce(agent_id)
            .map_err(UpdateError::CandidateStore)?
            .ok_or_else(|| anyhow!("PAUSED_BOUNDARY_UNBOUND: the paused head has no local quiesce record; explicit reconciliation required"))?;
        answer.request_id = Some(request.request_id);
        let Some(activation) = self.store.open_activation(agent_id).map_err(UpdateError::CandidateStore)? else {
            // An explicit operator pause is a control state, not an
            // interrupted hand-over: only an explicit resume may end it.
            answer.reason = "the paused boundary is not an open hand-over".to_string();
            return Ok(answer);
        };
        answer.activation_id = Some(activation.activation_id);
        answer.activation_phase = Some(activation.phase);
        let selected = selector.clone().ok_or(UpdateError::SelectorMissing)?;
        ensure!(request.generation == head.software_generation && request.epoch == head.controller_epoch,
            "the paused boundary does not bind this head's generation/epoch");
        ensure!(activation.agent_id == agent_id && activation.from_generation == head.software_generation,
            "the open hand-over does not start from the paused generation");
        // The recorded source slot must be this installation's live slot. A
        // pre-slot installation is its own slot, so the data directory is the
        // same answer; a foreign slot is refused rather than resumed.
        let recorded_slot = self.state_slot(&activation.from_state_ref)?;
        ensure!(same_slot_path(&recorded_slot, &self.live_slot)
                || same_slot_path(&recorded_slot, &self.data_dir),
            "the open hand-over names another source slot; explicit reconciliation required");
        if selected.selector_revision > activation.expected_selector_revision {
            // The candidate CAS committed. Completion (mirror, fence, pointer)
            // is deterministic; the old writer is never resumed.
            self.finish_committed_handoff(agent_id)?;
            let (head, _) = self.store.load_subject(agent_id).map_err(UpdateError::CandidateStore)?;
            ensure!(head.mode != SubjectMode::Paused,
                "the committed selection was completed but the retired writer is still paused");
            answer.state = HandoffReconciliationState::SelectionCommitted;
            answer.reason = "the selector committed before retirement; the retired writer is fenced and must not run".to_string();
            answer.mode_after = Some(head.mode);
            answer.epoch = Some(head.controller_epoch);
            answer.generation = Some(head.software_generation.clone());
            answer.activation_phase = self.store
                .activation_record(activation.activation_id)
                .map_err(UpdateError::CandidateStore)?
                .map(|record| record.phase);
            answer.selector = self.store.activation_selector(agent_id)
                .map_err(UpdateError::CandidateStore)?;
            let sealed = self.seal_json(&answer, "activation", "handoff-reconciliation")?;
            answer.receipt_ref = Some(sealed);
            return Ok(answer);
        }
        ensure!(selected.selector_revision == activation.expected_selector_revision,
            "the selector moved without committing this activation; explicit reconciliation required");
        ensure!(selected.generation == head.software_generation,
            "the selected generation does not own the paused head");
        self.verify_frozen_process_handoff(&activation, &selected, &request)?;

        // The same controller takes its own boundary back. `resume_subject_quiesce`
        // re-reads the head, the epoch, the generation and the planned successor
        // inside one transaction, so a boundary that moved in the meantime is
        // refused instead of resumed.
        let resumed = self.store.resume_subject_quiesce(agent_id, request.request_id)
            .map_err(UpdateError::CandidateStore)?;
        ensure!(resumed.state == SubjectQuiesceState::Resumed,
            "the boundary resume did not commit; the paused subject must not be treated as active");
        let (head, _) = self.store.load_subject(agent_id).map_err(UpdateError::CandidateStore)?;
        ensure!(head.mode == SubjectMode::Active && head.controller_epoch == request.epoch
            && head.software_generation == request.generation,
            "the resumed controller is not the same generation/epoch");
        let abandoned = self.abandon_interrupted_handoff(&activation, &request)?;
        answer.state = HandoffReconciliationState::ResumedSameController;
        answer.reason = "the selection never committed; the same controller generation resumed its boundary".to_string();
        answer.mode_after = Some(head.mode);
        answer.epoch = Some(head.controller_epoch);
        answer.generation = Some(head.software_generation.clone());
        answer.activation_phase = Some(abandoned.phase);
        answer.resumed = true;
        let sealed = self.seal_json(&answer, "activation", "handoff-reconciliation")?;
        answer.receipt_ref = Some(sealed);
        tracing::warn!(
            activation_id = %activation.activation_id,
            request_id = %request.request_id,
            generation = %request.generation.0,
            "interrupted hand-over boundary resumed by the same controller generation"
        );
        Ok(answer)
    }

    /// Reconcile only when this process really owns the installation. A live
    /// runtime that still holds its participants must not decide this.
    pub fn reconcile_if_exclusive(&self, agent_id: EntityId) -> UpdateResult<Option<HandoffReconciliation>> {
        if !self.exclusive_owner_proven() {
            return Ok(None);
        }
        Ok(Some(self.reconcile_interrupted_handoff(agent_id)?))
    }

    /// Verify the frozen live-control request of an interrupted hand-over.
    ///
    /// The request is the only proof that this pause was this installation's
    /// own update and not some other control state. An offline activation has
    /// no frozen live request; its activation record, selector revision and
    /// boundary commit are the whole binding.
    fn verify_frozen_process_handoff(
        &self,
        activation: &ActivationRecord,
        selected: &ActivationSelector,
        request: &SubjectQuiesce,
    ) -> UpdateResult<()> {
        let closure_bytes = self.store.cas.read_object(&activation.recovery_closure_ref.digest)
            .map_err(anyhow::Error::from)?;
        ensure!(Digest::blake3(&closure_bytes) == activation.recovery_closure_ref.digest
                && closure_bytes.len() as u64 == activation.recovery_closure_ref.bytes,
            "the activation recovery closure does not match its reference");
        let closure: serde_json::Value = serde_json::from_slice(&closure_bytes)
            .map_err(anyhow::Error::from)?;
        let Some(reference) = closure.get("process_handoff_request_ref") else {
            return Ok(());
        };
        let object: ObjectRef = serde_json::from_value(reference.clone())
            .map_err(anyhow::Error::from)?;
        let bytes = self.store.cas.read_object(&object.digest).map_err(anyhow::Error::from)?;
        ensure!(Digest::blake3(&bytes) == object.digest && bytes.len() as u64 == object.bytes,
            "the frozen process hand-off request does not match its reference");
        let frozen: FrozenProcessHandoff = serde_json::from_slice(&bytes)
            .map_err(anyhow::Error::from)?;
        ensure!(frozen.schema_version == 1, "unsupported frozen process hand-off request");
        ensure!(frozen.request_id == request.request_id && frozen.agent_id == activation.agent_id,
            "the frozen process hand-off request is not this boundary");
        ensure!(frozen.installation_id
                == self.store.installation_id_for_subject(activation.agent_id)
                    .map_err(UpdateError::CandidateStore)?,
            "the frozen process hand-off request belongs to another installation");
        ensure!(&frozen.source_selector == selected,
            "the frozen process hand-off source selector is not the current selection");
        let live = slots::resolve_live_slot(&self.data_dir).ok().flatten()
            .unwrap_or_else(|| self.data_dir.clone());
        ensure!(same_slot_path(&frozen.source_slot, &live),
            "the frozen process hand-off source slot is not the live slot");
        Ok(())
    }

    /// Abandon an interrupted activation without destroying its history: the
    /// stopped-resource receipts, the cursors and the original identity stay.
    fn abandon_interrupted_handoff(
        &self,
        activation: &ActivationRecord,
        request: &SubjectQuiesce,
    ) -> UpdateResult<ActivationRecord> {
        ensure!(!activation.phase.is_terminal(),
            "the interrupted hand-over is already terminal in phase {:?}", activation.phase);
        let mut abandoned = activation.clone();
        abandoned.last_error_ref = Some(self.seal_json(
            &serde_json::json!({
                "state": "abandoned_before_selection",
                "reason": "the process hand-over never committed its selection; the same controller resumed its boundary",
                "activation_id": activation.activation_id,
                "request_id": request.request_id,
                "epoch": request.epoch,
                "generation": request.generation.0,
                "observed_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "retained": {
                    "pending_operations_ref": activation.pending_operations_ref,
                    "inference_allocations_ref": activation.inference_allocations_ref,
                    "ingress_cursors_ref": activation.ingress_cursors_ref,
                    "evidence_cursors_ref": activation.evidence_cursors_ref,
                    "imported_through": activation.imported_through,
                },
            }),
            "activation",
            "handoff-reconciliation-failure",
        )?);
        abandoned.phase = ActivationPhase::Abandoned;
        self.store
            .update_activation_record(&abandoned, RecordRevision(abandoned.record_revision))
            .map_err(UpdateError::CandidateStore)
    }

    /// The candidate program this activation will select, re-verified against
    /// its prepared artifact digest.
    ///
    /// It is checked before anything is quiesced and again immediately before
    /// the selector CAS: a candidate whose bytes changed or vanished between
    /// its smoke launch and its selection must never become the committed
    /// authority while the working runtime has already stopped.
    fn verified_candidate_program(&self, record: &ActivationRecord) -> UpdateResult<(PathBuf, Digest)> {
        let slot_dir = self.layout().slot_dir(&record.to_generation);
        let manifest = slots::read_slot_manifest(&slot_dir).map_err(anyhow::Error::from)?;
        ensure!(manifest.generation == record.to_generation.0,
            "the candidate slot manifest names generation {}, not this activation's target {}",
            manifest.generation, record.to_generation.0);
        let executable = manifest.artifact_path.context("candidate artifact path missing")?;
        let digest = manifest.artifact_digest.context("candidate artifact digest missing")?;
        let bytes = std::fs::read(&executable)
            .with_context(|| format!("candidate executable {executable} is unreadable"))?;
        ensure!(Digest::blake3(&bytes) == digest,
            "candidate executable {executable} does not match its prepared artifact digest");
        Ok((PathBuf::from(executable), digest))
    }

    fn authority_snapshot(&self) -> UpdateResult<serde_json::Value> {
        let source = self.claim_source.as_ref()
            .ok_or_else(|| anyhow!("AUTHORITY_UNBOUND: runtime must bind its node or explicit local branch"))?;
        let claim = source.committed_primary_claim()?;
        let branch = source.local_branch_authority()?;
        ensure!(claim.is_some() || branch.is_some(), "AUTHORITY_UNAVAILABLE: neither committed primary nor local branch authority");
        Ok(serde_json::json!({"primary_claim": claim, "local_branch": branch}))
    }

    /// Re-read the committed primary claim before the swap. A changed claim
    /// refuses the hand-over: participants and subject are resumed and the
    /// activation is recorded abandoned.
    async fn check_primary_claim(&self, record: &ActivationRecord, request_id: EntityId) -> UpdateResult<()> {
        let now = self.authority_snapshot()?;
        let closure_bytes = self.store.cas.read_object(&record.recovery_closure_ref.digest)
            .map_err(anyhow::Error::from)?;
        let closure: serde_json::Value = serde_json::from_slice(&closure_bytes)
            .map_err(anyhow::Error::from)?;
        if closure.get("authority") == Some(&now) {
            return Ok(());
        }
        self.participants
            .resume_all()
            .await
            .map_err(UpdateError::Other)?;
        if request_id != EntityId::nil() {
            let _ = self
                .store
                .resume_subject_quiesce(record.agent_id, request_id)
                .map_err(UpdateError::CandidateStore)?;
        }
        tracing::warn!(
            activation_id = %record.activation_id,
            "the committed primary claim changed; the hand-over is refused"
        );
        let _ = self.fail_open(
            record.clone(),
            "the committed primary claim changed between preparation and the swap".to_string(),
        );
        Err(UpdateError::PrimaryClaimChanged)
    }

    fn layout(&self) -> SlotLayout {
        SlotLayout::new(self.data_dir.clone())
    }

    fn live_slot_handle(&self, generation: &RuntimeGeneration) -> AbSlot {
        AbSlot {
            generation: generation.clone(),
            slot_dir: self.live_slot.clone(),
            db_path: self.live_slot.join(SLOT_DB_FILENAME),
            cas_dir: self.cas_dir.clone(),
            artifact_path: None,
            artifact_digest: None,
            is_active: true,
        }
    }

    fn seal_json(
        &self,
        value: &impl Serialize,
        namespace: &str,
        codec_name: &str,
    ) -> UpdateResult<ObjectRef> {
        let bytes = serde_json::to_vec(value).map_err(anyhow::Error::from)?;
        Ok(self
            .store
            .cas
            .seal_object(&bytes, "application/json", namespace, codec_name, 1)
            .map_err(anyhow::Error::from)?
            .object)
    }

    /// The deterministic, read-only view of one subject's activation state.
    pub fn status(&self, agent_id: EntityId) -> UpdateResult<ActivationStatus> {
        let selector = self.store.activation_selector(agent_id)?;
        let history = self.store.activation_records(agent_id)?;
        let open_activation = history
            .iter()
            .rev()
            .find(|record| !record.phase.is_terminal())
            .cloned();
        let live_slot = slots::resolve_live_slot(&self.data_dir)
            .ok()
            .flatten()
            .map(|path| path.to_string_lossy().to_string());
        let generations = list_generations(&self.data_dir)?;
        let launch_receipt = match &selector {
            Some(selector) => self.store.activation_launch_for_selector(agent_id, selector.selector_revision)?,
            None => None,
        };
        Ok(ActivationStatus {
            selector,
            launch_receipt,
            open_activation,
            history,
            live_slot,
            generations,
        })
    }

    /// A deterministic recovery read. It never invents a successor and never
    /// infers the live side from the last error: it reads the selector, the
    /// open activation and the slot directories.
    pub fn recover(&self, agent_id: EntityId) -> UpdateResult<RecoveryReport> {
        let selector = self.store.activation_selector(agent_id)?;
        let open_activation = self.store.open_activation(agent_id)?;
        let mut generations = Vec::new();
        let versions = self.data_dir.join("versions");
        if versions.is_dir() {
            for entry in std::fs::read_dir(&versions)? {
                let entry = entry?;
                if entry.file_type()?.is_dir()
                    && !entry.file_name().to_string_lossy().starts_with(".shenko-stage-")
                {
                    generations.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        let mut missing = Vec::new();
        match (selector.clone(), open_activation.clone()) {
            (Some(selector), Some(record)) => {
                let slot = self.layout().slot_dir(&selector.generation);
                if slot != self.live_slot && !slot.is_dir() {
                    missing.push(format!(
                        "the selector names slot {} which is not on disk",
                        selector.generation.0
                    ));
                }
                if record.phase != ActivationPhase::Activated && record.imported_through.is_none()
                {
                    missing.push(
                        "the open activation has not recorded its imported watermark; run the phase again"
                            .to_string(),
                    );
                }
            }
            (Some(_), None) => {}
            (None, _) => {
                missing.push(
                    "no activation selector; the installation has no committed control authority"
                        .to_string(),
                );
            }
        }
        Ok(RecoveryReport {
            selector,
            open_activation,
            generations,
            missing,
        })
    }

    /// Complete only an already committed candidate selection. Never infer
    /// success/abandonment from the last error or from the launcher pointer.
    /// The caller holds the runtime's control lane or exclusive offline owner.
    pub fn finish_committed_handoff(&self, agent_id: EntityId) -> UpdateResult<()> {
        for source_record in self.store.activation_records(agent_id)? {
            let target_slot = self.state_slot(&source_record.to_state_ref)?;
            if target_slot == self.live_slot || !target_slot.join(SLOT_DB_FILENAME).is_file() { continue; }
            let candidate = Store::open(target_slot.join(SLOT_DB_FILENAME), self.cas_dir.clone())?;
            let Some(selected) = candidate.activation_selector(agent_id)? else { continue; };
            if selected.origin != (ActivationOrigin::Activation { activation_id: source_record.activation_id })
                || selected.selector_revision != source_record.expected_selector_revision + 1 { continue; }
            let committed = candidate.activation_record(source_record.activation_id)?
                .ok_or(UpdateError::ActivationNotFound(source_record.activation_id))?;
            ensure!(matches!(committed.phase, ActivationPhase::Activated | ActivationPhase::Observing | ActivationPhase::Retired),
                "candidate selector does not have its atomic activated record");
            let current = self.store.activation_selector(agent_id)?.ok_or(UpdateError::SelectorMissing)?;
            if current.selector_revision > selected.selector_revision { continue; }
            if current.selector_revision == selected.selector_revision {
                ensure!(current == selected, "equal selector revisions disagree; explicit reconciliation required");
            } else {
                ensure!(current.selector_revision == source_record.expected_selector_revision,
                    "retired selector cannot mirror this candidate commit");
                let mut mirror = committed.clone();
                mirror.record_revision = source_record.record_revision;
                self.store.swap_activation_selector(current.selector_revision, &selected, &mirror)?;
            }
            let (head, _) = self.store.load_subject(agent_id)?;
            ensure!(head.software_generation == source_record.from_generation,
                "retired head no longer belongs to interrupted activation");
            if head.mode != SubjectMode::Handoff {
                ensure!(head.mode == SubjectMode::Paused, "retired writer is not quiesced; refusing route publication");
                self.store.fence_head_for_handoff(&head, head.record_revision, &selected.route_authority_ref)?;
            }
            slots::publish_current_slot(&self.data_dir, &target_slot).map_err(anyhow::Error::from)?;
            return Ok(());
        }
        Ok(())
    }

    /// Build, copy, migrate and smoke a candidate.
    pub async fn prepare(&self, request: PrepareRequest) -> UpdateResult<ActivationRecord> {
        let authority = self.authority_snapshot()?;
        ensure!(authority.get("primary_claim") == Some(&serde_json::to_value(&request.primary_claim).map_err(anyhow::Error::from)?),
            "request primary claim differs from runtime authority");
        let current = self
            .store
            .activation_selector(request.agent_id)?
            .ok_or(UpdateError::SelectorMissing)?;
        if let Some(open) = self.store.open_activation(request.agent_id)? {
            return Err(UpdateError::ActivationOpen(open.activation_id));
        }
        if request.generation == current.generation {
            bail!(
                "the candidate generation {} is already the current one",
                request.generation.0
            );
        }
        let route_bytes = self.store.cas.read_object(&current.route_authority_ref.digest).map_err(anyhow::Error::from)?;
        let previous_route: RouteAuthority = serde_json::from_slice(&route_bytes).map_err(anyhow::Error::from)?;
        let previous_bytes = std::fs::read(&previous_route.artifact_path)?;
        ensure!(Digest::blake3(&previous_bytes) == previous_route.artifact_digest,
            "previous runnable artifact differs from its committed route");
        let recovery_dir = self.data_dir.join("recovery-programs").join(previous_route.artifact_digest.to_string().replace(':', "-"));
        std::fs::create_dir_all(&recovery_dir)?;
        let retained_program = recovery_dir.join("shenko");
        if !retained_program.exists() {
            use std::io::Write;
            let mut retained = std::fs::OpenOptions::new().write(true).create_new(true).open(&retained_program)?;
            retained.write_all(&previous_bytes)?;
            retained.set_permissions(std::fs::metadata(&previous_route.artifact_path)?.permissions())?;
            retained.sync_all()?;
            std::fs::File::open(&recovery_dir)?.sync_all()?;
        }
        ensure!(Digest::blake3(&std::fs::read(&retained_program)?) == previous_route.artifact_digest,
            "retained previous executable is incomplete or corrupt");
        slots::write_slot_manifest(&self.live_slot, &slots::SlotManifest {
            schema_version: slots::SlotManifest::SCHEMA_VERSION,
            generation: current.generation.0.clone(), cas_dir: self.cas_dir.to_string_lossy().into_owned(),
            artifact_path: Some(retained_program.to_string_lossy().into_owned()),
            artifact_digest: Some(previous_route.artifact_digest), copied_through_seq: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        }).map_err(anyhow::Error::from)?;

        // 1. The candidate artifact, with a real content identity.
        let outcome = match &request.source {
            CandidateSource::Prebuilt { path } => BuildOutcome {
                artifact: Some(
                    build::describe_artifact(path, &request.generation).map_err(anyhow::Error::from)?,
                ),
                exit_code: 0,
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                command: vec![path.to_string_lossy().to_string()],
            },
            CandidateSource::Build {
                source_dir,
                profile,
                target,
                extra_args,
            } => {
                let build_request = crate::build::BuildRequest {
                    source_dir: source_dir.clone(),
                    generation: request.generation.clone(),
                    profile: *profile,
                    target: target.clone(),
                    extra_args: extra_args.clone(),
                    binary_name: "shenko".to_string(),
                };
                self.builder
                    .build(&build_request)
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?
            }
        };
        let artifact = outcome
            .artifact
            .ok_or_else(|| anyhow!("the build produced no artifact"))?;

        // 2. Copy the live slot into the candidate slot.
        let live = self.live_slot_handle(&current.generation);
        let slot = slots::copy_slot(&live, &self.layout(), &request.generation)
            .map_err(anyhow::Error::from)?;
        // Place the candidate program into its slot.
        let artifact_dir = slot.slot_dir.join("artifact");
        std::fs::create_dir_all(&artifact_dir)?;
        let artifact_path = artifact_dir.join("shenko");
        std::fs::copy(&artifact.path, &artifact_path)?;
        ensure!(Digest::blake3(&std::fs::read(&artifact_path)?) == artifact.digest,
            "candidate artifact changed while copied");
        std::fs::File::open(&artifact_path)?.sync_all()?;
        std::fs::File::open(&artifact_dir)?.sync_all()?;
        let slot = AbSlot {
            generation: request.generation.clone(),
            slot_dir: slot.slot_dir.clone(),
            db_path: slot.db_path.clone(),
            cas_dir: self.cas_dir.clone(),
            artifact_path: Some(artifact_path),
            artifact_digest: Some(artifact.digest.clone()),
            is_active: false,
        };
        let mut descriptor = slots::read_slot_manifest(&slot.slot_dir).map_err(anyhow::Error::from)?;
        descriptor.artifact_path = slot.artifact_path.as_ref().map(|path| path.to_string_lossy().into_owned());
        descriptor.artifact_digest = slot.artifact_digest.clone();
        slots::write_slot_manifest(&slot.slot_dir, &descriptor).map_err(anyhow::Error::from)?;

        // 3. Migrate the candidate copy by opening it: the store runs its own
        //    schema migration on the copy, never on the live database.
        let candidate_store =
            Store::open(slot.db_path.clone(), self.cas_dir.clone()).map_err(UpdateError::Other)?;
        drop(candidate_store);

        // 4. Import the facts committed after the copy.
        let import = migration::import_delta(&live.db_path, &slot.db_path, &self.cas_dir)
            .map_err(anyhow::Error::from)?;
        if !import.blocked_units.is_empty() {
            return Err(UpdateError::MigrationBlocked(import.blocked_units));
        }

        // 5. Smoke the candidate against its own copy.
        let artifact_path = slot
            .artifact_path
            .clone()
            .ok_or_else(|| anyhow!("the candidate slot has no artifact"))?;
        let report = smoke::launch_smoke(
            &artifact_path,
            &slot.slot_dir,
            &request.generation,
        )
        .await
        .map_err(|error| UpdateError::SmokeFailed(format!("{error}")))?;

        // 6. Persist the activation record with its sealed references.
        let record = self.build_record(&current, &request, &live, &slot, &import, &report)?;
        let record = self.store.create_activation_record(&record)?;
        tracing::info!(
            activation_id = %record.activation_id,
            generation = %request.generation.0,
            imported = ?record.imported_through,
            "activation prepared"
        );
        Ok(record)
    }

    fn build_record(
        &self,
        current: &ActivationSelector,
        request: &PrepareRequest,
        live: &AbSlot,
        slot: &AbSlot,
        import: &ImportReport,
        report: &SmokeReport,
    ) -> UpdateResult<ActivationRecord> {
        let copied = slots::read_slot_manifest(&slot.slot_dir).map_err(anyhow::Error::from)?;
        let live_watermark = copied.copied_through_seq.map(|seq|
            CommitRef::new(self.store.ledger.ledger_id(), shenko_types::ids::LedgerSeq(seq)));
        let from_state = self.seal_json(
            &serde_json::json!({
                "schema_version": 1,
                "generation": current.generation.0,
                "slot_dir": live.slot_dir.to_string_lossy(),
                "db_path": live.db_path.to_string_lossy(),
                "watermark": live_watermark,
            }),
            "activation",
            "state-copy",
        )?;
        let to_state = self.seal_json(
            &serde_json::json!({
                "schema_version": 1,
                "generation": request.generation.0,
                "slot_dir": slot.slot_dir.to_string_lossy(),
                "db_path": slot.db_path.to_string_lossy(),
                "watermark": import.watermark,
                "copied_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }),
            "activation",
            "state-copy",
        )?;
        let smoke_evidence = self.seal_json(report, "activation", "smoke-report")?;

        let rollback_plan = self.seal_json(
            &serde_json::json!({
                "schema_version": 1,
                "steps": [
                    "create a new activation record back to the previous generation",
                    "import the current generation's new facts into the previous slot",
                    "swap the selector back with the next epoch",
                    "fence the current generation's head",
                    "publish the route back to the previous slot"
                ],
                "retains_new_facts": true,
            }),
            "activation",
            "rollback-plan",
        )?;

        let recovery_closure = self.seal_json(
            &serde_json::json!({
                "schema_version": 1,
                "from_slot": live.slot_dir.to_string_lossy(),
                "to_slot": slot.slot_dir.to_string_lossy(),
                "from_artifact": current.route_authority_ref,
                "to_artifact": {
                    "path": slot.artifact_path,
                    "digest": slot.artifact_digest,
                },
                "shared_cas_dir": self.cas_dir.to_string_lossy(),
                "authority": self.authority_snapshot()?,
                "importer": delta_migrator_ref(),
                "object_handles": "content-addressed by digest; the shared CAS is the authority",
            }),
            "activation",
            "recovery-closure",
        )?;

        Ok(ActivationRecord {
            activation_id: EntityId::new(),
            agent_id: request.agent_id,
            from_generation: current.generation.clone(),
            to_generation: request.generation.clone(),
            from_state_ref: from_state,
            to_state_ref: to_state,
            migrator_ref: delta_migrator_ref(),
            recovery_closure_ref: recovery_closure,
            contract_revision: "shenko.update.activation/v1".to_string(),
            phase: ActivationPhase::Smoke,
            w0: live_watermark,
            w1: None,
            imported_through: import.watermark,
            ingress_cursors_ref: None,
            evidence_cursors_ref: None,
            pending_operations_ref: None,
            inference_allocations_ref: None,
            expected_selector_revision: current.selector_revision,
            target_epoch: current.active_epoch + 1,
            smoke_evidence_ref: Some(smoke_evidence),
            rollback_plan_ref: rollback_plan,
            last_error_ref: None,
            record_revision: 0,
            expected_primary_claim: request.primary_claim.clone(),
        })
    }

    /// Quiesce, catch up, swap the selector and fence the retired writer.
    pub async fn activate(
        &self,
        activation_id: EntityId,
        expected_selector_revision: u64,
    ) -> UpdateResult<ActivationOutcome> {
        ensure!(self.participants.inactive_owner_proven,
            "final selection requires the retired runtime's exclusive owner; use process handoff control");
        let result = self.activate_inner(activation_id, expected_selector_revision).await;
        if result.is_err() {
            if let Some(record) = self.store.activation_record(activation_id)? {
                if let Err(cleanup) = self.abort_before_selection(&record, activation_id, expected_selector_revision, &self.layout().slot_dir(&record.to_generation)).await {
                    bail!("handoff failed: {}; compensation incomplete: {cleanup}", result.as_ref().unwrap_err());
                }
            }
        }
        result
    }

    async fn activate_inner(&self, activation_id: EntityId, expected_selector_revision: u64) -> UpdateResult<ActivationOutcome> {
        let mut record = self
            .store
            .activation_record(activation_id)?
            .ok_or(UpdateError::ActivationNotFound(activation_id))?;
        if record.phase != ActivationPhase::Smoke {
            return Err(UpdateError::PhaseMismatch(
                activation_id,
                record.phase,
                ActivationPhase::Smoke,
            ));
        }
        if record.expected_selector_revision != expected_selector_revision {
            bail!(
                "SELECTOR_REVISION_CONFLICT: the activation expects revision {}, the caller asked for {}",
                record.expected_selector_revision,
                expected_selector_revision
            );
        }
        let current = self
            .store
            .activation_selector(record.agent_id)?
            .ok_or(UpdateError::SelectorMissing)?;
        if current.selector_revision != record.expected_selector_revision {
            return Err(UpdateError::PhaseMismatch(
                activation_id,
                record.phase,
                ActivationPhase::Smoke,
            ));
        }

        // 0b. The sealed smoke evidence must actually say the candidate
        //     passed, and it must name this activation's target generation.
        //     A failed, absent or foreign report refuses the swap before
        //     anything is quiesced: the working runtime is never replaced
        //     by a candidate its own smoke rejected (docs/specs/self-update.md:
        //     "a missing/malformed manifest, failed command or failed report
        //     is an error").
        self.verify_smoke_evidence(&record)?;

        // 0c. The program that would become the selection must still be the
        //     one its smoke ran against. Refusing here — before any resource
        //     is quiesced — keeps the working runtime untouched.
        self.verified_candidate_program(&record)?;

        // 1. The subject pauses at a boundary; a running primary is fenced
        //    here, a stopped one has already stopped admitting decisions.
        let quiesce_request = self.quiesce_subject(record.agent_id, activation_id).await?;

        // 2. Every participant releases its generation-bound resources.
        let participant_receipts = self
            .participants
            .quiesce_all()
            .await
            .map_err(|error| self.fail_open(record.clone(), format!("{error}")))?;
        if !participant_receipts.is_empty() || record.pending_operations_ref.is_none() {
            record.pending_operations_ref = Some(self.seal_json(&participant_receipts, "activation", "resource-handoff-receipts")?);
            record.inference_allocations_ref = record.pending_operations_ref.clone();
        }
        record.phase = ActivationPhase::Handoff;
        record = self.store.update_activation_record(&record, RecordRevision(record.record_revision))?;

        // 3b. The claim this hand-over was prepared under must still be the
        //     committed one; a changed claim refuses the swap.
        self.check_primary_claim(&record, quiesce_request).await?;

        // 3. The final import up to the live watermark.
        let slot = self
            .layout()
            .slot_dir(&record.to_generation)
            .join(SLOT_DB_FILENAME);
        let import = migration::import_delta(&self.live_slot.join(SLOT_DB_FILENAME), &slot, &self.cas_dir)
            .map_err(anyhow::Error::from)?;
        if !import.blocked_units.is_empty() {
            return Err(self.fail_open(record, format!("{:?}", import.blocked_units)));
        }
        let w1 = import
            .watermark
            .ok_or_else(|| anyhow!("the live watermark could not be read"))?;
        self.check_primary_claim(&record, quiesce_request).await?;

        // 4. The selector swap in the candidate copy — the linearization
        //    point. Before this commit the candidate has no authority.
        let candidate_store =
            Store::open(slot.parent().unwrap().join(SLOT_DB_FILENAME), self.cas_dir.clone())
                .map_err(UpdateError::Other)?;
        let mut candidate_record = self
            .store
            .activation_record(record.activation_id)?
            .ok_or(UpdateError::ActivationNotFound(activation_id))?;
        candidate_record.w1 = Some(w1);
        candidate_record.imported_through = Some(w1);
        let next = ActivationSelector {
            agent_id: record.agent_id,
            selector_revision: current.selector_revision + 1,
            active_epoch: record.target_epoch,
            generation: record.to_generation.clone(),
            state_ref: record.to_state_ref.clone(),
            origin: ActivationOrigin::Activation {
                activation_id: record.activation_id,
            },
            route_authority_ref: record.to_state_ref.clone(),
        };
        // The program verified before quiescing is the one this CAS selects.
        // Re-reading it here refuses a swap that happened in between instead
        // of committing authority to bytes nobody smoked.
        let (executable, artifact_digest) = self.verified_candidate_program(&record)?;
        let executable = executable.to_string_lossy().into_owned();
        let route = RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id: record.agent_id,
            generation: record.to_generation.clone(),
            active_epoch: next.active_epoch,
            selector_revision: next.selector_revision,
            data_dir: self.data_dir.to_string_lossy().to_string(),
            slot_dir: slot.parent().unwrap().to_string_lossy().to_string(),
            artifact_path: executable.clone(),
            artifact_digest,
            launch_command: vec![
                executable,
                "--data-dir".to_string(),
                self.data_dir.to_string_lossy().to_string(),
                "start".to_string(),
            ],
            primary_claim: record.expected_primary_claim.clone(),
            activation_id: Some(record.activation_id),
        };
        // The route authority is a content-addressed fact of the shared CAS.
        let route_bytes = serde_json::to_vec(&route).map_err(anyhow::Error::from)?;
        let route_ref = candidate_store
            .cas
            .seal_object(&route_bytes, "application/json", "activation", "route-authority", 1)
            .map_err(anyhow::Error::from)?
            .object;
        let next = ActivationSelector {
            route_authority_ref: route_ref,
            ..next
        };
        let (swapped_selector, activated_record, commit) = candidate_store
            .activate_successor(expected_selector_revision, &next, &candidate_record)
            .map_err(UpdateError::CandidateStore)?;
        drop(candidate_store);

        // 5. The retired copy mirrors the swap and its writer is fenced.
        //    Its steps are bound to the old epoch, so it can no longer admit
        //    a decision even while its files are still readable.
        let (head, head_revision) = {
            let (head, _) = self
                .store
                .load_subject(record.agent_id)
                .map_err(UpdateError::CandidateStore)?;
            (head.clone(), head.record_revision)
        };
        if head.software_generation != record.from_generation {
            bail!(
                "the live head's generation {} does not match the activation's from-generation {}",
                head.software_generation.0,
                record.from_generation.0
            );
        }
        let reason = self.seal_json(
            &serde_json::json!({
                "activation_id": record.activation_id,
                "new_epoch": swapped_selector.active_epoch,
                "new_generation": swapped_selector.generation.0,
                "new_slot": route.slot_dir,
            }),
            "activation",
            "handoff-reason",
        )?;
        let (_, mirrored_record, _) = self
            .store
            .swap_activation_selector(expected_selector_revision, &swapped_selector, &record)
            .map_err(UpdateError::CandidateStore)?;
        let fenced = self
            .store
            .fence_head_for_handoff(&head, head_revision, &reason)
            .map_err(UpdateError::CandidateStore)?;
        let _ = mirrored_record;

        // 6. The route publication: the launcher pointer. It follows the
        //    swap; it never creates authority.
        slots::publish_current_slot(&self.data_dir, slot.parent().unwrap())
            .map_err(anyhow::Error::from)?;

        let mut observed = activated_record.clone();
        if observed.phase == ActivationPhase::Activated {
            observed.phase = ActivationPhase::Observing;
            observed = self
                .store
                .update_activation_record(&observed, RecordRevision(activated_record.record_revision))
                .map_err(UpdateError::CandidateStore)?;
            let selected_store = Store::open(slot.clone(), self.cas_dir.clone())?;
            selected_store.update_activation_record(&observed, RecordRevision(activated_record.record_revision))?;
        }

        tracing::info!(
            activation_id = %record.activation_id,
            commit = %commit,
            generation = %swapped_selector.generation.0,
            "activation swapped"
        );
        Ok(ActivationOutcome {
            record: observed,
            selector: swapped_selector,
            commit,
            fenced_head: fenced,
            participants: participant_receipts,
        })
    }

    /// Roll the control authority back to the previous generation.
    ///
    /// This is a new activation, not a phase. The current generation's new
    /// facts are imported into the previous slot first; then the selector
    /// swaps back with the next epoch.
    pub async fn rollback(
        &self,
        activation_id: EntityId,
        expected_selector_revision: u64,
    ) -> UpdateResult<ActivationOutcome> {
        ensure!(self.participants.inactive_owner_proven,
            "final rollback requires the retired runtime's exclusive owner; use process handoff control");
        let request_id = EntityId::new();
        let result = self.rollback_inner(activation_id, expected_selector_revision, request_id).await;
        if result.is_err() {
            if let Some(record) = self.store.activation_record(activation_id)? {
                let target = self.state_slot(&record.from_state_ref)?;
                if let Err(cleanup) = self.abort_before_selection(&record, request_id, expected_selector_revision, &target).await {
                    bail!("rollback failed: {}; compensation incomplete: {cleanup}", result.as_ref().unwrap_err());
                }
            }
        }
        result
    }

    async fn rollback_inner(&self, activation_id: EntityId, expected_selector_revision: u64, request_id: EntityId) -> UpdateResult<ActivationOutcome> {
        let rollback_authority = self.authority_snapshot()?;
        let record = self
            .store
            .activation_record(activation_id)?
            .ok_or(UpdateError::ActivationNotFound(activation_id))?;
        if record.phase == ActivationPhase::Retired || record.phase == ActivationPhase::Abandoned {
            return Err(UpdateError::PhaseMismatch(
                activation_id,
                record.phase,
                ActivationPhase::Activated,
            ));
        }
        let current = self
            .store
            .activation_selector(record.agent_id)?
            .ok_or(UpdateError::SelectorMissing)?;
        if current.selector_revision != expected_selector_revision {
            bail!(
                "SELECTOR_REVISION_CONFLICT: the selector is at revision {}, the caller asked for {}",
                current.selector_revision,
                expected_selector_revision
            );
        }

        // 1. Fence the newer writer: it must not produce more facts while we
        //    take its state.
        let quiesce_request = self.quiesce_subject(record.agent_id, request_id).await?;
        let participants = self
            .participants
            .quiesce_all()
            .await
            .map_err(UpdateError::Other)?;

        // 2. Use the declared typed recovery/conflict units. Never overwrite
        // a conflicting task/effect closure as a generic row merge.
        ensure!(current.generation == record.to_generation, "rollback source is not the currently selected generation");
        let from_slot = self.live_slot.join(SLOT_DB_FILENAME);
        let previous_slot = self.state_slot(&record.from_state_ref)?;
        let to_slot = previous_slot.join(SLOT_DB_FILENAME);
        // Rollback is the one direction where the destination becomes the
        // selected generation immediately after the import, so a source commit
        // that the export did not cover is unrecoverable. The strict importer
        // refuses that instead of reporting a caught-up copy. Prepare and
        // activate keep the ordinary importer: the live subject is still
        // committing while they catch up.
        let import = migration::import_rollback_delta(&from_slot, &to_slot, &self.cas_dir)
            .map_err(anyhow::Error::from)?;
        if !import.blocked_units.is_empty() {
            return Err(UpdateError::MigrationBlocked(import.blocked_units));
        }
        ensure!(self.authority_snapshot()? == rollback_authority,
            "rollback authority changed during catchup");
        let recovery_bytes = self.store.cas.read_object(&record.recovery_closure_ref.digest).map_err(anyhow::Error::from)?;
        let mut recovery: serde_json::Value = serde_json::from_slice(&recovery_bytes).map_err(anyhow::Error::from)?;
        recovery["authority"] = rollback_authority;
        recovery["rollback_of"] = serde_json::to_value(record.activation_id).map_err(anyhow::Error::from)?;
        let rollback_recovery = self.seal_json(&recovery, "activation", "recovery-closure")?;

        // 3. A new activation record back to the old generation.
        let previous_manifest = slots::read_slot_manifest(&previous_slot).map_err(anyhow::Error::from)?;
        // The retained executable must pass its smoke against the older
        // copy exactly like a forward candidate: a failed or absent report
        // is an error and no rollback record is created for it. The working
        // runtime stays selected and its quiesced writers are resumed.
        let rollback_smoke = smoke::launch_smoke(
            Path::new(previous_manifest.artifact_path.as_ref().context("retained rollback executable missing")?),
            &previous_slot, &record.from_generation,
        ).await.map_err(|error| UpdateError::SmokeFailed(error.to_string()))?;
        let rollback_record = ActivationRecord {
            activation_id: EntityId::new(),
            agent_id: record.agent_id,
            from_generation: current.generation.clone(),
            to_generation: record.from_generation.clone(),
            from_state_ref: record.to_state_ref.clone(),
            to_state_ref: record.from_state_ref.clone(),
            migrator_ref: delta_migrator_ref(),
            recovery_closure_ref: rollback_recovery,
            contract_revision: record.contract_revision.clone(),
            phase: ActivationPhase::Handoff,
            // The forward activation's watermark is not where this rollback
            // starts: the live generation kept committing after it.
            w0: import.source_watermark_before.or(record.w1),
            w1: import.watermark,
            imported_through: import.watermark,
            ingress_cursors_ref: record.ingress_cursors_ref.clone(),
            evidence_cursors_ref: record.evidence_cursors_ref.clone(),
            pending_operations_ref: record.pending_operations_ref.clone(),
            inference_allocations_ref: record.inference_allocations_ref.clone(),
            expected_selector_revision: current.selector_revision,
            target_epoch: current.active_epoch + 1,
            smoke_evidence_ref: Some(self.seal_json(&rollback_smoke, "activation", "rollback-smoke")?),
            rollback_plan_ref: record.rollback_plan_ref.clone(),
            last_error_ref: None,
            record_revision: 0,
            expected_primary_claim: self.current_primary_claim()?,
        };
        let mut rollback_record = self.store.create_activation_record(&rollback_record)?;

        // 4. Swap the selector back in the older copy — its authority.
        let old_store = Store::open(
            previous_slot.join(SLOT_DB_FILENAME),
            self.cas_dir.clone(),
        )
        .map_err(UpdateError::Other)?;
        let rollback_route = RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id: record.agent_id, generation: record.from_generation.clone(),
            active_epoch: rollback_record.target_epoch, selector_revision: current.selector_revision + 1,
            data_dir: self.data_dir.to_string_lossy().into_owned(),
            slot_dir: previous_slot.to_string_lossy().into_owned(),
            artifact_path: previous_manifest.artifact_path.context("retained rollback program missing")?,
            artifact_digest: previous_manifest.artifact_digest.context("retained rollback digest missing")?,
            launch_command: Vec::new(), primary_claim: record.expected_primary_claim.clone(),
            activation_id: Some(rollback_record.activation_id),
        };
        let rollback_route = RouteAuthority {
            launch_command: vec![rollback_route.artifact_path.clone(), "--data-dir".into(),
                self.data_dir.to_string_lossy().into_owned(), "start".into()], ..rollback_route
        };
        let rollback_route_ref = self.seal_json(&rollback_route, "activation", "route-authority")?;
        old_store.create_activation_record(&rollback_record)?;
        let next = ActivationSelector {
            agent_id: record.agent_id,
            selector_revision: current.selector_revision + 1,
            active_epoch: rollback_record.target_epoch,
            generation: record.from_generation.clone(),
            state_ref: record.from_state_ref.clone(),
            origin: ActivationOrigin::Activation {
                activation_id: rollback_record.activation_id,
            },
            route_authority_ref: rollback_route_ref,
        };
        let (swapped, mut activated, commit) = old_store
            .activate_successor(current.selector_revision, &next, &rollback_record)
            .map_err(UpdateError::CandidateStore)?;
        drop(old_store);
        let _ = &mut activated;

        // 5. The newer copy mirrors the swap back and is fenced.
        let (head, head_revision) = {
            let (head, _) = self
                .store
                .load_subject(record.agent_id)
                .map_err(UpdateError::CandidateStore)?;
            (head.clone(), head.record_revision)
        };
        let reason = self.seal_json(
            &serde_json::json!({
                "activation_id": rollback_record.activation_id,
                "rollback_of": record.activation_id,
                "new_epoch": swapped.active_epoch,
                "new_generation": swapped.generation.0,
            }),
            "activation",
            "handoff-reason",
        )?;
        let (_, mirrored, _) = self.store
            .swap_activation_selector(current.selector_revision, &swapped, &rollback_record)
            .map_err(UpdateError::CandidateStore)?;
        rollback_record = mirrored;
        let fenced = self
            .store
            .fence_head_for_handoff(&head, head_revision, &reason)
            .map_err(UpdateError::CandidateStore)?;

        // 6. The route publication back to the old slot.
        slots::publish_current_slot(&self.data_dir, &previous_slot)
            .map_err(anyhow::Error::from)?;

        // 7. Mark both records.
        rollback_record.phase = ActivationPhase::Activated;
        rollback_record = self
            .store
            .update_activation_record(&rollback_record, RecordRevision(rollback_record.record_revision))
            .map_err(UpdateError::CandidateStore)?;

        tracing::info!(
            activation_id = %rollback_record.activation_id,
            commit = %commit,
            generation = %swapped.generation.0,
            "activation rolled back"
        );
        Ok(ActivationOutcome {
            record: rollback_record,
            selector: swapped,
            commit,
            fenced_head: fenced,
            participants,
        })
    }

    /// Record a failure on an activation and stop it, without destroying its
    /// history. The returned error carries what happened.
    fn fail_open(&self, record: ActivationRecord, detail: String) -> UpdateError {
        let mut failed = record;
        failed.phase = ActivationPhase::Abandoned;
        match self.seal_json(&serde_json::json!({"error": &detail, "observed_at": chrono::Utc::now()}),
            "activation", "activation-failure") {
            Ok(reference) => failed.last_error_ref = Some(reference),
            Err(error) => return UpdateError::Other(anyhow!("activation failed: {detail}; failure evidence could not be sealed: {error}")),
        }
        if let Err(error) = self.store.update_activation_record(
            &failed,
            RecordRevision(failed.record_revision),
        ) {
            return UpdateError::Other(anyhow!("activation failed: {detail}; failure state could not be committed: {error}"));
        }
        UpdateError::Other(anyhow::anyhow!("activation abandoned: {detail}"))
    }

    /// Re-read the sealed smoke evidence of a Smoke-phase activation and
    /// refuse the swap when it is missing, reports a failure, or names a
    /// generation other than this record's target. The refusal abandons the
    /// activation with the outcome named in `last_error_ref`; integrity or
    /// decoding failures stay loud errors instead.
    fn verify_smoke_evidence(&self, record: &ActivationRecord) -> UpdateResult<()> {
        let evidence = match record.smoke_evidence_ref.as_ref() {
            Some(evidence) => evidence,
            None => {
                return Err(self.refuse_smoke(
                    record,
                    "the activation has no saved smoke report; a missing report is not a pass",
                ));
            }
        };
        let bytes = self
            .store
            .cas
            .read_object(&evidence.digest)
            .map_err(anyhow::Error::from)?;
        ensure!(
            Digest::blake3(&bytes) == evidence.digest && bytes.len() as u64 == evidence.bytes,
            "smoke evidence reference integrity mismatch"
        );
        let report: SmokeReport = serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
        if !report.passed {
            return Err(self.refuse_smoke(
                record,
                &format!("the saved smoke report did not pass: {}", smoke::describe_failure(&report)),
            ));
        }
        if report.generation != record.to_generation.0 {
            return Err(self.refuse_smoke(
                record,
                &format!(
                    "the saved smoke report names generation {}, not this activation's target {}",
                    report.generation, record.to_generation.0
                ),
            ));
        }
        Ok(())
    }

    /// Abandon an activation whose smoke evidence does not qualify it, then
    /// return the refusal. The saved failure becomes the record's
    /// `last_error_ref`, so the decision stays inspectable afterwards.
    fn refuse_smoke(&self, record: &ActivationRecord, detail: &str) -> UpdateError {
        let _ = self.fail_open(record.clone(), detail.to_string());
        UpdateError::SmokeFailed(detail.to_string())
    }

    fn state_slot(&self, state_ref: &ObjectRef) -> UpdateResult<PathBuf> {
        let bytes = self.store.cas.read_object(&state_ref.digest).map_err(anyhow::Error::from)?;
        ensure!(Digest::blake3(&bytes) == state_ref.digest && bytes.len() as u64 == state_ref.bytes,
            "state-copy reference integrity mismatch");
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
        Ok(PathBuf::from(value.get("slot_dir").and_then(|value| value.as_str())
            .context("retained state copy has no slot directory")?))
    }

    async fn abort_before_selection(&self, record: &ActivationRecord, request_id: EntityId,
        expected: u64, target_slot: &Path) -> UpdateResult<()> {
        if self.store.subject_quiesce(request_id)?.is_none() { return Ok(()); }
        let conn = rusqlite::Connection::open_with_flags(target_slot.join(SLOT_DB_FILENAME),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(anyhow::Error::from)?;
        let selected = shenko_store::activation::read_selector(&conn, record.agent_id)?;
        if selected.as_ref().is_some_and(|selector| selector.selector_revision > expected) {
            // The CAS already committed: old admissions MUST stay closed.
            // Deterministic completion reconciles mirror/fence/pointer.
            return Ok(());
        }
        let resources = self.participants.resume_all().await;
        let subject = self.store.resume_subject_quiesce(record.agent_id, request_id);
        match (resources, subject) {
            (Ok(()), Ok(_)) => Ok(()),
            (resources, subject) => bail!("resource resume={resources:?}; subject resume={subject:?}"),
        }
    }

    /// Ask the running subject to pause at a boundary and wait for the
    /// durable `Paused` state. Returns the quiesce request id (nil when the
    /// subject was already paused) so an abandoned hand-over can resume it.
    async fn quiesce_subject(&self, agent_id: EntityId, request_id: EntityId) -> UpdateResult<EntityId> {
        let (head, _) = self.store.load_subject(agent_id).map_err(UpdateError::CandidateStore)?;
        if head.mode == SubjectMode::Paused {
            return Ok(EntityId::nil());
        }
        if head.mode != SubjectMode::Active {
            bail!(
                "the subject's mode is {:?}; the hand-over needs an active or paused subject",
                head.mode
            );
        }
        self.store
            .request_subject_quiesce(agent_id, request_id)
            .map_err(UpdateError::CandidateStore)?;
        if self.participants.inactive_owner_proven
            || !self.participants.subject_driver_running.load(std::sync::atomic::Ordering::Acquire) {
            self.store.pause_subject_at_boundary(&head)?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            match self.store.subject_quiesce(request_id).map_err(UpdateError::CandidateStore)? {
                Some(record)
                    if record.state
                        == shenko_store::runtime_boundary::SubjectQuiesceState::Paused =>
                {
                    return Ok(request_id);
                }
                Some(_) => {}
                None => {
                    return Err(UpdateError::SubjectNotPaused);
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(UpdateError::SubjectNotPaused);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

/// Whether two slot paths name the same directory. Canonicalization tolerates
/// symlinked data directories; an unreadable path is compared as written.
fn same_slot_path(left: &Path, right: &Path) -> bool {
    let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

/// The generation directories of an installation, oldest first.
fn list_generations(data_dir: &Path) -> UpdateResult<Vec<String>> {
    let mut generations = Vec::new();
    let versions = data_dir.join("versions");
    if versions.is_dir() {
        for entry in std::fs::read_dir(&versions)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && !entry.file_name().to_string_lossy().starts_with(".shenko-stage-")
            {
                generations.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    generations.sort();
    Ok(generations)
}

/// The identity of the same-schema delta importer.
pub fn delta_migrator_ref() -> VersionRef {
    const DELTA_MIGRATOR: &[u8] =
        b"shenko.update.delta_import/v1: compare rows by key, write the differing ones, copy the commits they reference; blocked units are reported with a name, never silently migrated";
    let digest = Digest::blake3(DELTA_MIGRATOR);
    VersionRef {
        artifact: ArtifactRef {
            digest: digest.clone(),
            locator: "builtin:update-delta-import-v1".to_string(),
        },
        symbol: "shenko.update.delta_import".to_string(),
        schema: SchemaRef {
            name: "update-delta-import/v1".to_string(),
            major: 1,
            digest: digest.clone(),
        },
    }
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod handoff_tests;
