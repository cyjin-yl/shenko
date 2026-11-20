//! Owner-requested observation-only attachment. HTTP authenticates the owner;
//! the existing native dispatcher alone executes the durably admitted request.
use super::*;
use crate::seat::runtime::local_handle_for_location;
use shenko_store::SeatLocation;
use shenko_types::{commit::CommitRef, task::*};

const KIND: &str = "seat.attach.local";
const PLUGIN: &str = "64b9cf86-40fb-4e93-82a5-c26052e8453c";
const CONTRACT: &[u8] = b"shenko.seat.attach.local/v1: authenticated installation owner selects one exact local physical pane and one existing task; atomic original seat/lease binding; observation only, no terminal input or process launch; recovery reads original binding, never creates or replaces it";

/// The owner selects an exact previously observed locator. Task and principal
/// authority are resolved by the server, never accepted as actor body fields.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalSeatAttachRequest {
    pub request_id: String,
    pub task_id: EntityId,
    pub location: SeatLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    schema_version: u32,
    installation_id: EntityId,
    principal_ref: ObjectRef,
    owning_task_revision: TaskRevision,
    request: LocalSeatAttachRequest,
}

fn request_task_id(installation_id: EntityId, principal: &ObjectRef, request_id: &str) -> EntityId {
    EntityId::derived("owner-local-seat-request/v1",
        format!("{installation_id}:{}:{request_id}", principal.digest.0).as_bytes())
}

fn implementation() -> Result<ImplementationRef> {
    Ok(ImplementationRef { plugin_id: plugin_id(PLUGIN),
        artifact_digest: shenko_kernel::native_artifact::native_artifact_digest()?, registry_revision: 0 })
}

fn task_frozen(store: &Store, task: &TaskRecord) -> Result<Frozen> {
    let bytes = store.cas.read_object(&task.input_ref.digest)?;
    ensure!(bytes.len() as u64 == task.input_ref.bytes && Digest::blake3(&bytes) == task.input_ref.digest,
        "owner attachment input closure mismatch");
    let frozen: Frozen = serde_json::from_slice(&bytes)?;
    ensure!(frozen.schema_version == 1 && task.kind == KIND && task.definition_version == 1
        && task.created_by_step_id.is_none()
        && task.memos.get("owner_local_seat_request") == Some(&serde_json::json!(true))
        && task.memos.get(shenko_store::child::NATIVE_INPUT).and_then(|v| v.as_str()) == Some("object")
        && task.task_id == request_task_id(frozen.installation_id, &frozen.principal_ref, &frozen.request.request_id)
        && task.owner == (TaskOwner::Task { id: frozen.request.task_id }),
        "attachment is not an admitted authenticated owner request");
    local_handle_for_location(&frozen.request.location)?;
    Ok(frozen)
}

fn receipt(store: &Store, task: &TaskRecord, replayed: bool) -> Result<serde_json::Value> {
    let operation_id = EntityId::derived("native-operation-v1", task.task_id.to_string().as_bytes());
    let operation = store.find_external_operation(operation_id)?;
    let seat_id = EntityId::derived("local-seat-attach-seat/v1", operation_id.to_string().as_bytes());
    Ok(serde_json::json!({"accepted": true, "replayed": replayed, "request_task_id": task.task_id,
        "owning_task_id": task_frozen(store, task)?.request.task_id,
        "operation_id": operation_id, "operation": operation,
        "seat": store.seat(seat_id)?, "accepted_commit": task.created_commit,
        "note": "Admission and durable binding do not attest current transport liveness; read the original operation evidence and seat.fleet observation."}))
}

/// Called only after OwnerState::actor(headers) authenticated this principal.
/// This publisher performs no pane probe and never executes the handler in HTTP.
pub fn admit_owner_local_seat_attachment(
    store: &Store,
    installation_id: EntityId,
    principal_ref: ObjectRef,
    request: LocalSeatAttachRequest,
    handler_available: bool,
) -> Result<serde_json::Value> {
    let grant = store.owner_grant(installation_id)?.ok_or_else(|| anyhow::anyhow!("owner not claimed"))?;
    ensure!(grant.human_principal_ref == principal_ref, "owner principal mismatch");
    ensure!(!request.request_id.is_empty() && request.request_id.len() <= 64
        && request.request_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "request_id must be 1..64 ASCII letters, digits, '-' or '_'");
    local_handle_for_location(&request.location)?;
    let task_id = request_task_id(installation_id, &principal_ref, &request.request_id);
    if let Some(task) = store.get_task(task_id)? {
        let original = task_frozen(store, &task)?;
        ensure!(original.request == request && original.principal_ref == principal_ref,
            "owner attachment request identity conflict");
        return receipt(store, &task, true);
    }
    ensure!(handler_available, "current seat attachment generation is unavailable");
    // A body UUID selects a server record; it never supplies delegation or a
    // lease holder. Only this installation's live subject task can be selected.
    let selected = store.get_task(request.task_id)?.ok_or_else(|| anyhow::anyhow!("selected task missing"))?;
    ensure!(store.get_existing_agent_id()? == Some(selected.agent_id)
        && !selected.is_terminal() && selected.abort_request.is_none()
        && matches!(selected.dispatch_eligibility, DispatchEligibility::Ready), "selected task unavailable");
    let frozen = Frozen { schema_version: 1, installation_id, principal_ref,
        owning_task_revision: selected.task_revision, request };
    let input_ref = support::seal(store, &frozen, "owner-local-seat-attachment")?;
    let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1, task_id, task_revision: TaskRevision(0), record_revision: RecordRevision(0),
        agent_id: selected.agent_id, conversation_id: selected.conversation_id,
        owner: TaskOwner::Task { id: selected.task_id }, background: false,
        kind: KIND.into(), definition_version: 1, implementation_ref: implementation()?, input_ref,
        state: TaskState::Pending { checkpoint_ref: None }, abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), serde_json::json!(task_id)),
            (shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object")),
            ("owner_local_seat_request".into(), serde_json::json!(true)),
        ]),
        residual_case_ids: vec![], created_by_step_id: None, dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0, first_run_at: None, terminal_at: None, created_commit: zero, updated_commit: zero,
    };
    let event = shenko_types::inbox::InboxEvent {
        event_id: EntityId::derived("owner-local-seat-request-event/v1", task_id.to_string().as_bytes()),
        kind: shenko_types::inbox::InboxEventKind::SystemEvent, created_at: chrono::Utc::now(),
        source: "owner.local-seat".into(), payload: serde_json::json!({"kind": KIND,
            "request_task_id": task_id, "owning_task_id": selected.task_id, "input_ref": task.input_ref,
            "principal_ref": frozen.principal_ref, "read_only": true}),
    };
    let input_event = shenko_types::store::RawInboxEvent {
        event_id: event.event_id, agent_id: selected.agent_id, source: event.source.clone(),
        kind: "system_event".into(), payload_json: serde_json::to_string(&event)?,
        received_at_ms: event.created_at.timestamp_millis(),
    };
    // Same canonical admission/authority/quota path as other requested native
    // operations. Its idempotent result is the durable original, never a new
    // dispatch future started by an HTTP callback.
    match store.admit_native_task(&task, &input_event) {
        Ok(Some(existing)) => receipt(store, &existing, true),
        Ok(None) => receipt(store, &store.external_task(task_id)?, false),
        Err(error) => {
            if let Some(original) = store.get_task(task_id)? {
                let original_frozen = task_frozen(store, &original)?;
                ensure!(original_frozen.request == frozen.request, "owner attachment request identity conflict");
                receipt(store, &original, true)
            } else { Err(error) }
        }
    }
}

/// Generation-owned installed entry. It has no model prompt: arbitrary model
/// input cannot impersonate an owner-admitted top-level native task.
pub struct LocalSeatAttachHandler { store: Arc<Store>, runtime: Arc<SeatRuntime> }

impl LocalSeatAttachHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self { Self { store, runtime } }

    async fn attach_original(&self, operation: &OperationRecord, frozen: &Frozen) -> Result<serde_json::Value> {
        let seat_id = EntityId::derived("local-seat-attach-seat/v1", operation.operation_id.to_string().as_bytes());
        let lease_id = EntityId::derived("local-seat-attach-lease/v1", operation.operation_id.to_string().as_bytes());
        let seat = self.store.seat(seat_id)?;
        let Some(seat) = seat else {
            return Ok(serde_json::json!({"status": "not_bound", "operation_id": operation.operation_id,
                "note": "Original operation has no durable binding; recovery does not create one."}));
        };
        ensure!(seat.read_only_operation == Some(operation.operation_id)
            && seat.task_id == frozen.request.task_id && seat.location == frozen.request.location,
            "original attachment binding conflict");
        let observation = self.runtime.attach_existing_local_read_only(operation.operation_id, seat_id, lease_id).await;
        Ok(match observation {
            Ok(handle) => serde_json::json!({"status":"attached", "seat_id":seat_id,"lease_id":lease_id,
                "owning_task_id":seat.task_id,"operation_id":operation.operation_id,"handle":handle,"read_only":true}),
            Err(error) => serde_json::json!({"status":"durable_only", "seat_id":seat_id,"lease_id":lease_id,
                "operation_id":operation.operation_id,"error":format!("{error:#}"),"read_only":true}),
        })
    }
}

#[async_trait]
impl NativeTaskHandler for LocalSeatAttachHandler {
    fn descriptor(&self) -> Result<NativeTaskDescriptor> {
        Ok(NativeTaskDescriptor { kind: KIND.into(), definition_version: 1, implementation_ref: implementation()?,
            role_revision: "owner-local-seat-attachment/1".into(),
            authority_refs: vec!["owner.local-seat.observe-only/v1".into()], prompt_section: None })
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen = task_frozen(&self.store, &invocation.task)?;
        ensure!(serde_json::to_value(&frozen)? == invocation.input, "attachment invocation input changed");
        let (mut operation, mut attempt) = support::operation_parts(&self.store, &invocation,
            plugin_id(PLUGIN), Digest::blake3(CONTRACT), "seat.attach.local.v1", invocation.task.input_ref.clone(),
            serde_json::json!({"owning_task_id":frozen.request.task_id,"location":frozen.request.location}),
            "original exact local seat is durably bound and currently observed; no input or process launch",
            "query original records only; never bind a missing record or discover a replacement")?;
        if let Some(original) = self.store.arm_native_operation_for_dispatch(
            &invocation.task_invocation, &mut operation, &mut attempt)? { return Ok(original); }
        let binding = async {
            let handle = local_handle_for_location(&frozen.request.location)?;
            let adapter = crate::seat::tmux::TmuxSeatAdapter::local();
            ensure!(adapter.local_location(&handle).await? == frozen.request.location,
                "owner-selected pane changed before binding");
            self.store.bind_local_read_only_seat(&invocation.task_invocation, operation.operation_id,
                frozen.request.task_id, frozen.owning_task_revision, frozen.request.location.clone())?;
            self.attach_original(&operation, &frozen).await
        }.await;
        let (payload, outcome, effect) = match binding {
            Ok(payload) => {
                let attached = payload["status"] == "attached";
                (payload, Some(if attached { OperationOutcome::Succeeded } else { OperationOutcome::Failed }), EffectStatus::Complete)
            }
            Err(error) => {
                let seat_id = EntityId::derived("local-seat-attach-seat/v1", operation.operation_id.to_string().as_bytes());
                match self.store.seat(seat_id) {
                    Ok(None) => (serde_json::json!({"status":"refused", "error":format!("{error:#}"),
                        "operation_id":operation.operation_id,"read_only":true}), Some(OperationOutcome::Failed), EffectStatus::None),
                    Ok(Some(seat)) if seat.read_only_operation == Some(operation.operation_id) =>
                        (serde_json::json!({"status":"durable_only", "error":format!("{error:#}"),
                            "operation_id":operation.operation_id,"seat_id":seat_id,"read_only":true}),
                            Some(OperationOutcome::Failed), EffectStatus::Complete),
                    Ok(Some(_)) => (serde_json::json!({"status":"unknown", "error":format!("{error:#}"),
                        "operation_id":operation.operation_id,"binding_conflict":true}), None, EffectStatus::Unknown),
                    Err(read_error) => (serde_json::json!({"status":"unknown", "error":format!("{error:#}"),
                        "operation_id":operation.operation_id,"binding_read_error":format!("{read_error:#}")}), None, EffectStatus::Unknown),
                }
            }
        };
        support::observe_record(&self.store, &operation, &attempt, "seat.attach.local.v1", &payload, outcome, effect)
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        let frozen = task_frozen(&self.store, task)?;
        ensure!(operation.task_id == task.task_id && operation.operation_name == "seat.attach.local.v1"
            && operation.final_input_ref == task.input_ref && operation.contract_ref.plugin_id == plugin_id(PLUGIN)
            && operation.contract_ref.contract_digest == Digest::blake3(CONTRACT), "original attachment operation mismatch");
        if operation.state == shenko_types::operation::OperationState::Resolved { return Ok(operation); }
        let payload = self.attach_original(&operation, &frozen).await?;
        let bound = payload["status"] != "not_bound";
        let attached = payload["status"] == "attached";
        let attempt = self.store.external_operation_attempt(operation_id)?;
        support::observe_record(&self.store, &operation, &attempt, "seat.attach.local.recovery", &payload,
            Some(if attached { OperationOutcome::Succeeded } else { OperationOutcome::Failed }),
            if bound { EffectStatus::Complete } else { EffectStatus::None })
    }
}
