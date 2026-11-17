//! Configurable finite-child workspace tools. Their lifecycle is the existing
//! NativeTaskHandler/Cordis lifecycle; no registry or kernel policy is added.
mod edit;
mod io;

pub use edit::EditorMode;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_store::Store;
use shenko_types::{
    commit::CommitRef,
    effect::{
        CancelContract, CheckpointContract, EffectActionKind, EffectContract,
        EffectContractBuilder, ExternalEffect, GuaranteeBasis, IdempotencyGuarantee,
        LifecycleContract, ResourceAccess,
    },
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskRecord},
};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceToolsConfig {
    pub root: PathBuf,
    pub editor: EditorMode,
}

/// Source identity for installation manifests. The running descriptor also
/// binds the canonical root and editor, because these change the read contract.
pub fn implementation_digest() -> Digest {
    let mut bytes = Vec::new();
    for source in [
        include_bytes!("mod.rs").as_slice(),
        include_bytes!("io.rs").as_slice(),
        include_bytes!("edit.rs").as_slice(),
    ] {
        bytes.extend_from_slice(&(source.len() as u64).to_le_bytes());
        bytes.extend_from_slice(source);
    }
    Digest::blake3(&bytes)
}

pub struct WorkspaceTools {
    shared: Arc<Shared>,
}

struct Shared {
    store: Arc<Store>,
    fs: io::WorkspaceFs,
    config: WorkspaceToolsConfig,
    identity: Digest,
}

impl WorkspaceTools {
    pub fn new(store: Arc<Store>, mut config: WorkspaceToolsConfig) -> Result<Self> {
        let fs = io::WorkspaceFs::new(config.root.clone())?;
        config.root = fs.root.clone();
        let identity = Digest::blake3(&serde_json::to_vec(
            &json!({"source":implementation_digest(),"config":config}),
        )?);
        Ok(Self {
            shared: Arc::new(Shared {
                store,
                fs,
                config,
                identity,
            }),
        })
    }

    /// Exactly one editing grammar is visible in a runtime generation.
    pub fn handlers(&self) -> Vec<Arc<dyn NativeTaskHandler>> {
        [Action::Read, Action::Search, Action::Create, Action::Edit]
            .into_iter()
            .map(|action| {
                Arc::new(Handler {
                    shared: self.shared.clone(),
                    action,
                }) as Arc<dyn NativeTaskHandler>
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Action {
    Read,
    Search,
    Create,
    Edit,
}

#[derive(Clone, Copy)]
enum ReceiptOrigin {
    Execution,
    Recovery,
}

impl Action {
    fn writes(self) -> bool {
        matches!(self, Self::Create | Self::Edit)
    }

    fn kind(self, editor: EditorMode) -> &'static str {
        match (self, editor) {
            (Self::Read, _) => "workspace.read",
            (Self::Search, _) => "workspace.search",
            (Self::Create, _) => "workspace.create",
            (Self::Edit, EditorMode::Replace) => "workspace.edit.replace",
            (Self::Edit, EditorMode::Hashline) => "workspace.edit.hashline",
        }
    }
}

struct Handler {
    shared: Arc<Shared>,
    action: Action,
}

impl Handler {
    fn kind(&self) -> &'static str {
        self.action.kind(self.shared.config.editor)
    }

    fn plugin_id() -> EntityId {
        EntityId::derived("native-workspace-tools", b"v1")
    }

    fn help(&self) -> String {
        let specific = match (self.action, self.shared.config.editor) {
            (Action::Read, EditorMode::Replace) => {
                r#"Input {"path":"relative/file","start_line":1,"max_lines":100}. Returns exact text and the full file digest. Follow next_line while truncated. A digest identifies a snapshot; it does not mean the whole file was shown or understood."#
            }
            (Action::Read, EditorMode::Hashline) => {
                r#"Input {"path":"relative/file","start_line":1,"max_lines":100}. Returns anchors [{line,anchor,text,ending}] and the full file digest. Reconstruct bytes as text+ending. Follow next_line while truncated; use these anchors in workspace.edit.hashline."#
            }
            (Action::Search, _) => {
                r#"Input {"query":"literal","path":".","mode":"content"}. mode:path searches real file paths (empty query lists paths). Content hits include line, context and full file digest; they are not proof of full-file inspection. Results are bounded; narrow path/query if truncated. Search skips symlinks and .git."#
            }
            (Action::Create, _) => {
                r#"Input {"path":"relative/new-file","text":"exact bytes\n"}. Parent must exist. Creates only if the destination is absent; never overwrites. Returns the actual changes and new digest."#
            }
            (Action::Edit, EditorMode::Replace) => {
                r#"Input {"path":"relative/file","expected_digest":"blake3:FULL_DIGEST_FROM_READ","edits":[{"old_text":"exact existing text","new_text":"replacement"}]}. Read relevant source/context first. Every nonempty old_text must match exactly once in the same original snapshot. All edits are preflighted; ambiguity, overlap, stale digest and no-change explicitly reject the write. No fuzzy matching or whitespace normalization. An existing empty file requires an execution tool. Inspect returned changes and run the task's focused validation."#
            }
            (Action::Edit, EditorMode::Hashline) => {
                r#"Input {"path":"relative/file","expected_digest":"blake3:FULL_DIGEST_FROM_READ","edits":[{"start":"N:HASH","end":"M:HASH","new_text":"replacement\n"}]}. Read relevant source/context first. Copy anchors from workspace.read. Inclusive line ranges include their original line terminators; new_text is literal, so include all desired newlines. Empty new_text deletes the range. All edits resolve against the same original snapshot. Stale digest/anchors, overlap and no-change reject the write. Empty files have no anchors. Inspect returned changes and run the task's focused validation."#
            }
        };
        format!(
            "{}: {} Configured root: {}. UTF-8 regular files up to 1 MiB; paths are relative to this root. Read/search display is bounded to about 12 KiB; full write differences remain in the raw receipt. The shared in-process writer lock and final digest check do not exclude concurrent shell writers. Configuration is fixed for this runtime; {} editor is selected.",
            self.kind(),
            specific,
            self.shared.config.root.display(),
            match self.shared.config.editor {
                EditorMode::Replace => "replace",
                EditorMode::Hashline => "hashline",
            }
        )
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.shared.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "workspace-tools",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.shared.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let input_ref = self.seal(&invocation.input, "workspace-input")?;
        let bindings = ResourceBindings {
            provider: self.kind().into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "cooperative_local_workspace".into(),
            extra: json!({"root":self.shared.config.root,"editor":self.shared.config.editor,
                "implementation":self.shared.identity,"external_writer_exclusion":false}),
        };
        let resource = self.seal(&bindings, "workspace-binding")?;
        let acceptance = self.seal(&"read: complete bounded observation; write: all preconditions pass, target bytes equal planned result on immediate readback; does not prove semantic task correctness or power-loss durability", "workspace-acceptance")?;
        let retry = self.seal(&"never replay an armed operation; reconcile original receipts, retain unknown without conclusive evidence", "workspace-retry")?;
        let version = self.descriptor().implementation();
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: format!("{}.v1", self.kind()),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: self.shared.identity.clone(),
                contract_digest: self.shared.identity.clone(),
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        value: Value,
        outcome: Option<OperationOutcome>,
        effect: EffectStatus,
        origin: ReceiptOrigin,
    ) -> Result<OperationRecord> {
        let raw = self.seal(&value, "workspace-observation")?;
        let mut display = io::display_result(value);
        display["raw_receipt_ref"] = json!(raw);
        // Invalid-request diagnostics can contain input fragments. Preserve
        // their full bytes in raw evidence, but never let the child cut JSON.
        if serde_json::to_vec(&display)?.len() > 15 * 1024 {
            display = json!({"status":display["status"],"code":display["code"],
                "message":"display exceeds response budget; inspect the complete raw receipt", "raw_receipt_ref":raw,
                "display_truncated":true});
        }
        let normalized = self.seal(&display, "workspace-result")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: match origin {
                ReceiptOrigin::Execution => self.kind().into(),
                ReceiptOrigin::Recovery => format!("{}.recovery", self.kind()),
            },
            source_instance_id: match origin {
                ReceiptOrigin::Execution => attempt.node_id.to_string(),
                ReceiptOrigin::Recovery => self.shared.store.capabilities.incarnation.to_string(),
            },
            source_event_key: match origin {
                ReceiptOrigin::Execution => None,
                ReceiptOrigin::Recovery => {
                    Some(format!("controller-recovery:{}", attempt.attempt_id))
                }
            },
            producer_epoch: match origin {
                ReceiptOrigin::Execution => Some(attempt.epoch.0),
                ReceiptOrigin::Recovery => None,
            },
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: if outcome == Some(OperationOutcome::Succeeded) {
                Some(ObservationLevel::PostconditionVerified)
            } else {
                None
            },
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.shared.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            effect,
        )
    }
}

#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: self.kind().into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: self.shared.identity.clone(),
                registry_revision: 0,
            },
            role_revision: "native-child/1".into(),
            authority_refs: vec!["workspace.tools/v1".into()],
            prompt_section: Some(self.help()),
        }
    }

    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        let mut builder = EffectContractBuilder::new(Self::plugin_id(), descriptor.implementation(), format!("{}.v1", self.kind()), 1)
            .resource("local_workspace", self.shared.config.root.display().to_string(),
                if self.action.writes() { ResourceAccess::ReadWrite } else { ResourceAccess::Read })
            .action(EffectActionKind::Cancel).idempotency(IdempotencyGuarantee::None)
            .cancel(CancelContract { can_cancel_before_start: true, can_cancel_in_flight: false,
                cancel_confirms_stopped: false, may_have_partial_effect: self.action.writes() })
            .checkpoint(CheckpointContract { capturable: vec!["frozen input and committed receipts".into()],
                excluded: vec!["workspace filesystem and other writers".into()], coordinated: false, consistency: "receipt evidence only".into() })
            .lifecycle(LifecycleContract {
                on_upgrade: "withdraw old handlers; new startup binds root/editor/source identity; never replay old writes".into(),
                on_uninstall: "withdraw new calls; existing receipts remain queryable".into(),
                on_crash: "reconcile original receipts; an armed operation without conclusive evidence remains unknown".into(),
                receipt_readable_after_removal: true,
            }).basis(GuaranteeBasis::RuntimeConstrained);
        if self.action.writes() {
            builder = builder.external_effect(ExternalEffect { kind: "workspace_file_write".into(),
                description: "writes same-directory staging files and publishes one target; edit uses a final digest check and rename, create uses absent-only hard-link publication. External writers, ACL/xattr retention and power-loss durability are not guaranteed".into(),
                reversible: false, compensation_possible: false });
        }
        vec![builder.build()]
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: format!("native.{}", self.kind()),
            version: "1".into(),
            description: self.help(),
            usage: Some(format!(
                "delegate:{}:1 with the input object documented above",
                self.kind()
            )),
        }]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let (mut operation, mut attempt) = self.operation_parts(&invocation)?;
        if let Some(existing) = self.shared.store.arm_native_operation_for_dispatch(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        // T1 has committed. All workspace reads/writes happen after admission;
        // the bounded synchronous section has no cancellation point mid-write.
        let result = match self.action {
            Action::Read => self
                .shared
                .fs
                .read(self.shared.config.editor, invocation.input),
            Action::Search => self.shared.fs.search(invocation.input),
            Action::Create => self.shared.fs.create(invocation.input),
            Action::Edit => self
                .shared
                .fs
                .edit(self.shared.config.editor, invocation.input),
        };
        match result {
            Ok(value) => self.record(&operation, &attempt, value, Some(OperationOutcome::Succeeded),
                if self.action.writes() { EffectStatus::Complete } else { EffectStatus::None }, ReceiptOrigin::Execution),
            Err(error) => self.record(&operation, &attempt,
                json!({"status":if error.uncertain {"unknown"} else {"rejected"},"code":error.code,"message":error.message}),
                if error.uncertain { None } else { Some(OperationOutcome::Failed) },
                if error.uncertain { EffectStatus::Unknown } else { EffectStatus::None }, ReceiptOrigin::Execution),
        }
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.shared.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == format!("{}.v1", self.kind()),
            "original_workspace_operation_contract_mismatch"
        );
        if operation.state == OperationState::InFlight {
            let attempt = self.shared.store.external_operation_attempt(operation_id)?;
            if attempt.dispatch_stage != AttemptDispatchStage::Ended {
                return self.record(&operation, &attempt,
                    json!({"status":"unknown","code":"receipt_unavailable", "operation_id":operation_id,
                        "message":"controller recovered without conclusive original evidence; workspace I/O was not repeated"}),
                    None, if self.action.writes() { EffectStatus::Unknown } else { EffectStatus::None }, ReceiptOrigin::Recovery);
            }
        }
        Ok(operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cognition_trial::PreparedSandbox;
    use shenko_types::agent::CallContext;

    fn invocation(
        sandbox: &PreparedSandbox,
        handler: &Handler,
        input: Value,
    ) -> NativeTaskInvocation {
        let store = sandbox.store();
        let (head, _) = store.load_subject(sandbox.agent_id()).unwrap();
        let parent = store.external_task(head.subject_root_task_id).unwrap();
        let input_ref = handler.seal(&input, "workspace-input").unwrap();
        let claim = store
            .claim_child_tool_task(
                &parent,
                EntityId::new(),
                handler.kind(),
                1,
                input_ref,
                handler.descriptor().implementation_ref,
            )
            .unwrap()
            .unwrap();
        let mut context = CallContext::new_system(claim.software_generation.clone());
        context.task_id = Some(claim.task.task_id);
        context.invocation_id = Some(claim.invocation.invocation_id);
        context.controller_epoch = claim.invocation.epoch;
        NativeTaskInvocation {
            context,
            task_invocation: claim.invocation,
            task: claim.task,
            operation_id: claim.operation_id,
            authority: AuthorityRef {
                delegation_id: parent.task_id,
                delegation_revision: parent.record_revision,
                decision_ref: head.genesis_ref.genesis_id,
            },
            input,
            child_tools: None,
        }
    }

    #[tokio::test]
    async fn original_receipts_are_reused_and_armed_writes_are_never_replayed() {
        let root =
            std::env::temp_dir().join(format!("shenko-workspace-ledger-{}", EntityId::new()));
        std::fs::create_dir(&root).unwrap();
        let sandbox = PreparedSandbox::fresh("workspace", &root).unwrap();
        let store = sandbox.store().clone();
        store.start_subject_dispatcher().unwrap();
        let tools = WorkspaceTools::new(
            store.clone(),
            WorkspaceToolsConfig {
                root: root.clone(),
                editor: EditorMode::Replace,
            },
        )
        .unwrap();
        let handler = Handler {
            shared: tools.shared.clone(),
            action: Action::Create,
        };
        let first = invocation(
            &sandbox,
            &handler,
            json!({"path":"complete.txt","text":"original"}),
        );
        let first_task = first.task.clone();
        let result = handler.execute(first).await.unwrap();
        assert_eq!(result.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(result.effect_status, EffectStatus::Complete);
        assert!(
            !store
                .external_operation_evidence(result.operation_id)
                .unwrap()
                .is_empty()
        );
        std::fs::write(root.join("complete.txt"), "later writer").unwrap();
        let reconciled = handler
            .reconcile(&first_task, result.operation_id)
            .await
            .unwrap();
        assert_eq!(reconciled.record_revision, result.record_revision);
        assert_eq!(
            std::fs::read_to_string(root.join("complete.txt")).unwrap(),
            "later writer"
        );

        let interrupted = invocation(
            &sandbox,
            &handler,
            json!({"path":"never-created.txt","text":"must not replay"}),
        );
        let (mut operation, mut attempt) = handler.operation_parts(&interrupted).unwrap();
        assert!(
            store
                .arm_native_operation_for_dispatch(
                    &interrupted.task_invocation,
                    &mut operation,
                    &mut attempt
                )
                .unwrap()
                .is_none()
        );
        let unknown = handler
            .reconcile(&interrupted.task, operation.operation_id)
            .await
            .unwrap();
        assert_eq!(unknown.effect_status, EffectStatus::Unknown);
        assert_eq!(unknown.outcome, None);
        assert!(!root.join("never-created.txt").exists());
        assert_eq!(
            store
                .external_operation_attempts(operation.operation_id)
                .unwrap()
                .len(),
            1
        );
        drop(handler);
        drop(tools);
        drop(store);
        drop(sandbox);
        std::fs::remove_dir_all(root).unwrap();
    }
}
