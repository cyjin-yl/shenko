//! `document.parse:1` — parse a source document's pages through the bound
//! document provider.
//!
//! The subject delegates a parse of one CAS-bound source document. The
//! boundary (shenko-document) is replaceable: the request names the exact
//! provider closure it wants, the source digest pins the input bytes, and a
//! scope/closure change never reuses another run's result. A provider that
//! is not provisioned reports an honest `ProviderUnavailable` — the result
//! carries the reason and zero fabricated page claims. There is no silent
//! substitution to a different engine, ever.
//!
//! A parse of a document that is not yet a real artifact (no bound engine or
//! model) is not a failure to hide: the run reports `provider_unavailable`
//! so the subject learns the capability is absent, not that the document was
//! empty. Pages that do run record real per-page checkpoints as evidence.

use std::sync::Arc;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_document::{
    DocumentProvider, PageRef, ParseRequest, run_parse, select_provider,
    DOCUMENT_PARSE_DEFINITION_VERSION,
};
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

const PARSE_CONTRACT: &[u8] = b"shenko.document.parse/v1: parse a CAS-bound source document's pages through the bound provider; honest unavailable, no substitution, no effects";
const PARSE_KIND: &str = "document.parse";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

/// One request to parse a source document.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DocumentParseInput {
    /// CAS digest of the source document bytes (the exact input).
    source_digest: String,
    /// Optional original filename for provider-side logging.
    #[serde(default)]
    source_name: Option<String>,
    /// Pages to parse; empty = whole document.
    #[serde(default)]
    pages: Vec<PageRef>,
    /// The provider closure identity to run under (e.g. "oxr/1.0"). A
    /// result is only valid under this exact closure.
    closure_id: String,
    /// Permission/scope binding.
    #[serde(default)]
    scope: Option<String>,
}

/// Parses a CAS-bound document through the bound provider.
pub struct DocumentParseHandler {
    store: Arc<Store>,
    providers: Arc<Vec<Arc<dyn DocumentProvider>>>,
}

impl DocumentParseHandler {
    /// Bind the handler to a provider registry. An empty registry is honest:
    /// every request reports `provider_unavailable` until a real engine is
    /// provisioned and its provider added.
    pub fn new(store: Arc<Store>, providers: Arc<Vec<Arc<dyn DocumentProvider>>>) -> Self {
        Self { store, providers }
    }

    fn plugin_id() -> EntityId {
        "9d2e4f81-3b7c-4a95-8e2f-1a6b4c8d0e2f"
            .parse()
            .expect("fixed document-parse plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-document-parse", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize + ?Sized>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<DocumentParseInput> {
        let input: DocumentParseInput = serde_json::from_value(value)?;
        ensure!(
            !input.source_digest.trim().is_empty() && input.source_digest.len() <= 512,
            "source_digest must be 1..=512 characters"
        );
        ensure!(
            !input.closure_id.trim().is_empty() && input.closure_id.len() <= 256,
            "closure_id must be 1..=256 characters"
        );
        Ok(input)
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<DocumentParseInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "document_parse_frozen_input_mismatch"
        );
        let input: DocumentParseInput = serde_json::from_slice(&bytes)?;
        Ok(input)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: format!("{PARSE_KIND}.v1"),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "document_parse".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "document-parse-binding")?;
        let acceptance = self.seal(
            "per-page parse checkpoints with real provenance, or an honest provider_unavailable naming the missing closure/engine/model",
            "document-parse-acceptance",
        )?;
        let retry = self.seal(
            "re-read only the frozen parse request under the original armed attempt; never arm again",
            "document-parse-retry",
        )?;
        let contract_digest = Digest::blake3(PARSE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:document-parse-v1".into(),
            },
            symbol: format!("{PARSE_KIND}.execute"),
            schema: SchemaRef {
                name: "document-parse-observation-v1".into(),
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
            operation_name: format!("{PARSE_KIND}.v1"),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
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

    /// Resolve the source bytes and run the parse. An absent provider closure
    /// or an unprovisioned engine is an honest `provider_unavailable` run with
    /// zero page claims, not an error and never a substitute.
    fn run_query(&self, input: &DocumentParseInput) -> Result<serde_json::Value> {
        let provider = match select_provider(&input.closure_id, &self.providers) {
            Some(provider) => provider,
            None => {
                return Ok(serde_json::json!({
                    "schema_version": 1,
                    "status": "provider_unavailable",
                    "op": "document.parse",
                    "state_reason": format!("no provider bound for closure '{}'", input.closure_id),
                    "checkpoints": [],
                }));
            }
        };

        let request = ParseRequest {
            schema_version: DOCUMENT_PARSE_DEFINITION_VERSION,
            source_digest: input.source_digest.clone(),
            source_name: input.source_name.clone(),
            pages: input.pages.clone(),
            closure_id: input.closure_id.clone(),
            scope: input.scope.clone(),
        };

        // An unprovisioned provider is an honest provider_unavailable run —
        // report it before touching the source, so the absence never looks
        // like a parse that happened to find nothing.
        if let shenko_document::ProviderAvailability::Unavailable { reason } =
            provider.availability()
        {
            return Ok(serde_json::json!({
                "schema_version": 1,
                "status": "provider_unavailable",
                "op": "document.parse",
                "provider": provider.provider_id(),
                "closure": provider.closure_id(),
                "state_reason": reason,
                "checkpoints": [],
            }));
        }

        // Source bytes are the CAS input the request is bound to. A missing or
        // corrupt object fails the run honestly rather than parsing a wrong
        // document fetched by path.
        let source_bytes = match self
            .store
            .cas
            .read_object(&Digest(input.source_digest.clone()))
        {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "document.parse",
                    "error": {
                        "code": "source_unavailable",
                        "message": format!("source digest {} not readable: {error:#}", input.source_digest),
                    },
                    "checkpoints": [],
                }));
            }
        };

        let run = run_parse(&request, &provider, &source_bytes)?;
        Ok(serde_json::json!({
            "schema_version": 1,
            "status": match run.state {
                shenko_document::ParseState::Complete => "ok",
                shenko_document::ParseState::ProviderUnavailable => "provider_unavailable",
                shenko_document::ParseState::Running => "running",
                shenko_document::ParseState::Cancelled => "cancelled",
            },
            "op": "document.parse",
            "provider": provider.provider_id(),
            "closure": provider.closure_id(),
            "parse_request_id": shenko_document::parse_request_id(&request).to_string(),
            "state": format!("{:?}", run.state),
            "checkpoints": run.checkpoints,
        }))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        input: &DocumentParseInput,
    ) -> Result<OperationRecord> {
        let (result, outcome) = match self.run_query(input) {
            Ok(result) => (result, OperationOutcome::Succeeded),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "document.parse",
                    "error": {"code": "document_parse_failed", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "document-parse-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; read fewer pages"
                    }
                }),
                "document-parse-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: format!("{PARSE_KIND}.v1"),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("document-parse:{}", attempt.attempt_id)),
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
            invocation.task.kind == PARSE_KIND && invocation.task.definition_version == 1,
            "document_parse_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "document-parse-frozen-input")?;
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
impl NativeTaskHandler for DocumentParseHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: PARSE_KIND.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(PARSE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-document-parse/1".into(),
            authority_refs: vec!["document.parse/v1".into()],
            prompt_section: Some(
                "document.parse/v1 解析一个 CAS 绑定源文档的若干页（[delegate:document.parse:1] {\"source_digest\":\"...\",\"closure_id\":\"oxr/1.0\",\"pages\":[]}）。结果带来源 provenance；provider 未配置时如实返回 provider_unavailable，不要当作解析到空文档。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == format!("{PARSE_KIND}.v1"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_document::OxrProvider;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-document-parse-{label}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn handler(dir: &std::path::Path, providers: Vec<Arc<dyn DocumentProvider>>) -> DocumentParseHandler {
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        DocumentParseHandler::new(store, Arc::new(providers))
    }

    /// An unbound closure honestly reports provider_unavailable — no
    /// substitute provider is ever tried, and no source is read.
    #[test]
    fn unbound_closure_reports_unavailable_not_substitute() {
        let dir = temp_dir("unbound");
        let oxr: Arc<dyn DocumentProvider> = Arc::new(OxrProvider::new(
            Some(std::env::temp_dir().join("shenko-nonexistent-engine")),
            Some("model-x".into()),
            None,
        ));
        let h = handler(&dir, vec![oxr]);
        let input = DocumentParseInput {
            source_digest: "blake3:deadbeef".into(),
            source_name: None,
            pages: vec![],
            closure_id: "no-such/1.0".into(),
            scope: None,
        };
        let result = h.run_query(&input).unwrap();
        assert_eq!(result["status"], "provider_unavailable");
        // No page claims — an unavailable closure produced nothing.
        assert_eq!(result["checkpoints"], serde_json::json!([]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A provisioned-but-not-ready OXR reports provider_unavailable BEFORE
    /// reading the source: the absent engine is named, and the source digest
    /// need not even exist in CAS.
    #[test]
    fn unprovisioned_oxr_is_unavailable_before_source_read() {
        let dir = temp_dir("oxr-absent");
        let oxr: Arc<dyn DocumentProvider> = Arc::new(OxrProvider::new(
            Some(std::env::temp_dir().join("shenko-missing-engine-xyz")),
            None,
            None,
        ));
        let h = handler(&dir, vec![oxr]);
        let input = DocumentParseInput {
            // A digest that does not exist — must NOT be read.
            source_digest: "blake3:not-in-cas".into(),
            source_name: None,
            pages: vec![PageRef { page_index: 0 }],
            // Match the provider's actual closure.
            closure_id: "oxr/1.0".into(),
            scope: Some("scope-a".into()),
        };
        let result = h.run_query(&input).unwrap();
        assert_eq!(result["status"], "provider_unavailable");
        assert_eq!(result["provider"], "oxr");
        assert_eq!(result["checkpoints"], serde_json::json!([]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
