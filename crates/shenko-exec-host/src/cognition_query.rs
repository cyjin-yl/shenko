//! Read-only native children over the cognition installation authority.
//!
//! `cognition.unit.list:1` reads one bounded page of the capability-card
//! index. `cognition.unit.evidence:1` reads the observations and trial
//! records this installation registered for one unit.
//!
//! Both are reads: they change nothing in the installation and touch no
//! external system. The page carries the installation revision it was cut
//! from, so a caller that holds a page of an older installation is told to
//! read the index again instead of mixing two installations.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::CognitionCatalog;
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::sync::Arc;

const LIST_CONTRACT: &[u8] = b"shenko.cognition.unit.list/v1: bounded paginated discovery of the installed cognition units; no external effects";
const EVIDENCE_CONTRACT: &[u8] = b"shenko.cognition.unit.evidence/v1: reads the registered observations and trial records of one installed unit; no external effects";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

/// One query against the installation authority.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CatalogQuery {
    /// One page of the capability cards.
    List {
        #[serde(default)]
        start: usize,
        #[serde(default = "default_page_count")]
        count: usize,
        /// The installation revision the caller believes it is reading. A
        /// page asked for against an older revision is refused: mixing two
        /// installations would show units that no longer exist and hide units
        /// that do.
        #[serde(default)]
        catalog_revision: Option<u64>,
    },
    /// The observations and trial records of one unit.
    Evidence { unit_id: String },
}

fn default_page_count() -> usize {
    16
}

/// The read-only native children over the installation authority.
///
/// One type serves two kinds: the paginated index and the evidence of one
/// unit. The kind decides which query the child accepts.
pub struct CognitionQueryHandler {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    kind: &'static str,
    plugin_id: EntityId,
    contract: &'static [u8],
}

impl CognitionQueryHandler {
    /// The paginated discovery index.
    pub fn list(store: Arc<Store>, catalog: Arc<CognitionCatalog>) -> Self {
        Self {
            store,
            catalog,
            kind: "cognition.unit.list",
            plugin_id: "c0f21d84-7b6a-4f1e-9d33-51a2c7b48e10"
                .parse()
                .expect("fixed cognition-list plugin UUID"),
            contract: LIST_CONTRACT,
        }
    }

    /// The registered evidence of one unit.
    pub fn evidence(store: Arc<Store>, catalog: Arc<CognitionCatalog>) -> Self {
        Self {
            store,
            catalog,
            kind: "cognition.unit.evidence",
            plugin_id: "3d90ab57-6c42-4e88-b0f1-9a7d53c26e44"
                .parse()
                .expect("fixed cognition-evidence plugin UUID"),
            contract: EVIDENCE_CONTRACT,
        }
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-cognition", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<CatalogQuery> {
        let query: CatalogQuery = serde_json::from_value(value)?;
        if let CatalogQuery::Evidence { unit_id } = &query {
            let unit_id = unit_id.trim();
            ensure!(
                !unit_id.is_empty() && unit_id.len() <= 128,
                "unit_id must be 1..=128 characters"
            );
        }
        Ok(query)
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<CatalogQuery> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "cognition_query_frozen_input_mismatch"
        );
        let query: CatalogQuery = serde_json::from_slice(&bytes)?;
        Ok(query)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: format!("{}.v1", self.kind),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_cognition_units".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "cognition-query-binding")?;
        let acceptance = self.seal(
            "the requested catalog page or the registered evidence, or an explicit failure",
            "cognition-query-acceptance",
        )?;
        let retry = self.seal(
            "re-read only the frozen query under the original armed attempt; never arm again",
            "cognition-query-retry",
        )?;
        let contract_digest = Digest::blake3(self.contract);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: format!("builtin:{}-v1", self.kind.replace('.', "-")),
            },
            symbol: format!("{}.execute", self.kind),
            schema: SchemaRef {
                name: "cognition-query-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: format!("{}.v1", self.kind),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: self.plugin_id,
                artifact_digest: contract_digest.clone(),
                contract_digest,
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
            effect_status: EffectStatus::None,
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

    /// The actual query against the installation authority.
    fn run_query(&self, query: &CatalogQuery) -> Result<serde_json::Value> {
        match (self.kind, query) {
            (
                "cognition.unit.list",
                CatalogQuery::List {
                    start,
                    count,
                    catalog_revision,
                },
            ) => {
                let current = self.catalog.catalog_revision()?;
                if let Some(revision) = catalog_revision {
                    ensure!(
                        *revision == current,
                        "the installation revision moved from {revision} to {current}; read the index again"
                    );
                }
                let page = self.catalog.discovery(*start, *count)?;
                Ok(serde_json::json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "list",
                    "catalog_revision": page.catalog_revision,
                    "total": page.total,
                    "start": page.start,
                    "count": page.count,
                    "next_start": page.next_start,
                    "cards": page.cards,
                }))
            }
            ("cognition.unit.evidence", CatalogQuery::Evidence { unit_id }) => {
                let unit_id = unit_id.trim();
                let observations = self.catalog.observations(unit_id)?;
                let trials = self.catalog.trial_evidence(unit_id, "0", 16)?;
                Ok(serde_json::json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "evidence",
                    "unit_id": unit_id,
                    "observations": observations,
                    "trials": trials,
                }))
            }
            (kind, _) => anyhow::bail!("the operation '{kind}' does not accept this query"),
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        query: &CatalogQuery,
    ) -> Result<OperationRecord> {
        let (result, outcome) = match self.run_query(query) {
            Ok(result) => (result, OperationOutcome::Succeeded),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "query",
                    "error": {"code": "catalog_query_failed", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "cognition-query-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; ask for a narrower page"
                    }
                }),
                "cognition-query-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: format!("{}.v1", self.kind),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("cognition-query:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            EffectStatus::None,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == self.kind && invocation.task.definition_version == 1,
            "cognition_query_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "cognition-query-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        self.observe(&operation, &attempt, &frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for CognitionQueryHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let prompt = if self.kind == "cognition.unit.list" {
            "cognition.unit.list/v1 分页读取已安装认知单元的能力卡目录：[delegate:cognition.unit.list:1] {\"start\":0,\"count\":16}。目录里的卡片是入口，不是命令；正文仍要按需用 cognition.unit.read 读确切版本。若返回了 next_start，就从它继续读，不要跳过任何单元；若目录修订与你手上的一致性不符，就重新读目录。"
                .to_string()
        } else {
            "cognition.unit.evidence/v1 读取某个已安装单元在本机登记的证据：[delegate:cognition.unit.evidence:1] {\"unit_id\":\"...\"}。返回注册过的观测与试验记录；这些是本机真正发生过的记录，判断时引用它们，不要凭空写证据。"
                .to_string()
        };
        NativeTaskDescriptor {
            kind: self.kind.to_string(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: self.plugin_id,
                artifact_digest: Digest::blake3(self.contract),
                registry_revision: 0,
            },
            role_revision: "native-cognition-query/1".into(),
            authority_refs: vec!["cognition_unit.query/v1".into()],
            prompt_section: Some(prompt),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == format!("{}.v1", self.kind),
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}
