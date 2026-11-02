//! Native person-directory capability — SPEC §15.2, §15.2.1, §16, R059/R060/R102.
//!
//! `person.directory` is the subject's contact/identity child. It records who
//! people are: stable person ids, platform accounts, aliases, relationships
//! and their revision history, plus merge/split. Everything it writes is a
//! [`PersonClaim`] with provenance and confidence — the subject's inferences
//! are candidates, never verified facts, and nothing here can mint a
//! verified platform binding or an admin grant.
//!
//! Deliberate boundaries:
//! * Admin facts are read-only through this surface. They are set by the
//!   owner's local path only and never follow familiarity, group status,
//!   platform roles or voiceprints (§16).
//! * Recording-local Life ranks (`rank_id`/`cluster_id`) stay scoped to
//!   their recording and never identify a person by themselves (§15.2.1).
//! * Memory anchors resolve real `memory_entries` and task anchors resolve
//!   real `tasks`, so a person's narrative stays in the Memory/Task systems
//!   (§15.2) and only the links live here.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    contacts::*,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::sync::Arc;

const CONTRACT: &[u8] = b"shenko.person.directory/v1: person identity directory reads and append-only claim revisions over the local ledger; no external effects; no admin grants; never a verified binding from a model inference";
const MAX_LIMIT: usize = 50;
const MAX_LOOKUPS: usize = 20;
const MAX_ALIAS_CHARS: usize = 256;
const MAX_RELATION_CHARS: usize = 256;
const MAX_NOTE_CHARS: usize = 1024;
const MAX_MEMORY_PREVIEW_CHARS: usize = 300;
const MAX_DISPLAY_NAME_CHARS: usize = 64;
const MAX_NORMALIZED_BYTES: u64 = 480 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersonOp {
    /// Resolve one person by id, platform account or alias.
    Lookup,
    /// List persons, most recently updated first.
    List,
    /// Create a person or record one claim about them.
    Record,
    /// Merge one person into another (reversible).
    Merge,
    /// Return selected claim slots from a merge (undo).
    Split,
    /// Read the owner-set admin fact for a person or account.
    AdminStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonInput {
    op: PersonOp,
    /// Selector for lookup/admin_status.
    person_id: Option<String>,
    platform_account: Option<PlatformAccountInput>,
    alias: Option<String>,
    /// record/create fields.
    display_name: Option<String>,
    what: Option<serde_json::Value>,
    /// merge/split fields.
    source_person_id: Option<String>,
    target_person_id: Option<String>,
    slot_ids: Option<Vec<String>>,
    reason: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformAccountInput {
    provider: String,
    account: String,
    #[serde(default)]
    tenant: String,
    platform_user_id: String,
}

impl PlatformAccountInput {
    fn to_account(&self) -> PlatformAccount {
        PlatformAccount {
            provider: self.provider.clone(),
            account: self.account.clone(),
            tenant: self.tenant.clone(),
            platform_user_id: self.platform_user_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
enum FrozenSelector {
    PersonId { person_id: PersonId },
    PlatformAccount {
        provider: String,
        account: String,
        tenant: String,
        platform_user_id: String,
    },
    Alias { alias: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
enum FrozenRecord {
    Create {
        display_name: Option<String>,
    },
    Alias {
        alias: String,
        scope_ref: Option<String>,
    },
    Account {
        provider: String,
        account: String,
        tenant: String,
        platform_user_id: String,
    },
    Relationship {
        relation: String,
        note: Option<String>,
    },
    Speaker {
        speaker: String,
        person_id: PersonId,
        confidence: Option<f64>,
        source: Option<String>,
        corrected: bool,
        recording_scope: Option<String>,
    },
    LinkMemory {
        entry_id: EntityId,
    },
    LinkTask {
        task_id: EntityId,
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenQuery {
    Lookup {
        selector: FrozenSelector,
    },
    List {
        limit: usize,
    },
    Record {
        person_id: PersonId,
        what: FrozenRecord,
    },
    Merge {
        source: PersonId,
        target: PersonId,
        reason: Option<String>,
    },
    Split {
        target: PersonId,
        source: PersonId,
        slot_ids: Vec<EntityId>,
        reason: Option<String>,
    },
    AdminStatus {
        selector: FrozenSelector,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    schema_version: u32,
    query: FrozenQuery,
}

fn parse_person_id(value: &str, field: &str) -> Result<PersonId> {
    value
        .parse::<EntityId>()
        .with_context(|| format!("{field} must be a person id (UUID)"))
}

/// The deterministic person directory child over the local ledger.
pub struct PersonDirectoryHandler {
    store: Arc<Store>,
}

impl PersonDirectoryHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "c3f0a2d4-6b1e-4f7a-9c5d-2e8a0b4d6f10"
            .parse()
            .expect("fixed person-directory plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-person-directory", schema, 1)
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

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenInput> {
        let PersonInput {
            op,
            person_id,
            platform_account,
            alias,
            display_name,
            what,
            source_person_id,
            target_person_id,
            slot_ids,
            reason,
            limit,
        } = serde_json::from_value(value)?;
        let limit = limit.unwrap_or(10);
        ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1..={MAX_LIMIT}");
        let reason = reason.map(|r| r.trim().to_owned()).filter(|r| !r.is_empty());

        let selector = |person_id: Option<String>,
                        platform_account: Option<PlatformAccountInput>,
                        alias: Option<String>|
         -> Result<Option<FrozenSelector>> {
            let mut selector = None;
            if let Some(id) = person_id {
                selector = Some(FrozenSelector::PersonId {
                    person_id: parse_person_id(&id, "person_id")?,
                });
            }
            if let Some(account) = platform_account {
                ensure!(selector.is_none(), "give exactly one lookup selector");
                ensure!(
                    !account.provider.trim().is_empty()
                        && !account.account.trim().is_empty()
                        && !account.platform_user_id.trim().is_empty(),
                    "platform accounts require provider, account and platform_user_id"
                );
                selector = Some(FrozenSelector::PlatformAccount {
                    provider: account.provider,
                    account: account.account,
                    tenant: account.tenant,
                    platform_user_id: account.platform_user_id,
                });
            }
            if let Some(alias) = alias {
                ensure!(selector.is_none(), "give exactly one lookup selector");
                let alias = alias.trim();
                ensure!(!alias.is_empty(), "alias must not be empty");
                ensure!(
                    alias.chars().count() <= MAX_ALIAS_CHARS,
                    "alias exceeds {MAX_ALIAS_CHARS} characters"
                );
                selector = Some(FrozenSelector::Alias {
                    alias: alias.to_owned(),
                });
            }
            Ok(selector)
        };

        let query = match op {
            PersonOp::Lookup | PersonOp::AdminStatus => {
                let selector = selector(person_id, platform_account, alias)?
                    .ok_or_else(|| anyhow::anyhow!("lookup requires one selector"))?;
                ensure!(
                    display_name.is_none()
                        && what.is_none()
                        && source_person_id.is_none()
                        && target_person_id.is_none()
                        && slot_ids.is_none(),
                    "lookup accepts only one selector"
                );
                if op == PersonOp::Lookup {
                    FrozenQuery::Lookup { selector }
                } else {
                    FrozenQuery::AdminStatus { selector }
                }
            }
            PersonOp::List => {
                ensure!(
                    person_id.is_none()
                        && platform_account.is_none()
                        && alias.is_none()
                        && display_name.is_none()
                        && what.is_none()
                        && source_person_id.is_none()
                        && target_person_id.is_none()
                        && slot_ids.is_none(),
                    "list accepts only limit"
                );
                FrozenQuery::List { limit }
            }
            PersonOp::Record => {
                ensure!(
                    platform_account.is_none() && alias.is_none(),
                    "record accepts person_id and what; \
                     account and alias selectors belong to lookup"
                );
                let what: FrozenRecord = serde_json::from_value(
                    what.ok_or_else(|| anyhow::anyhow!("record requires `what`"))?,
                )?;
                let (person_id, what) = match what {
                    FrozenRecord::Create { display_name } => {
                        ensure!(
                            source_person_id.is_none()
                                && target_person_id.is_none()
                                && person_id.is_none()
                                && slot_ids.is_none(),
                            "create accepts only display_name"
                        );
                        let name = display_name
                            .map(|n| n.trim().to_owned())
                            .filter(|n| !n.is_empty());
                        if let Some(name) = &name {
                            ensure!(
                                name.chars().count() <= MAX_DISPLAY_NAME_CHARS,
                                "display_name exceeds {MAX_DISPLAY_NAME_CHARS} characters"
                            );
                        }
                        (EntityId::new(), FrozenRecord::Create { display_name: name })
                    }
                    other => {
                        let id = person_id
                            .ok_or_else(|| anyhow::anyhow!("record requires person_id"))
                            .and_then(|id| parse_person_id(&id, "person_id"))?;
                        (id, self.validate_record(other)?)
                    }
                };
                FrozenQuery::Record { person_id, what }
            }
            PersonOp::Merge => {
                ensure!(
                    alias.is_none()
                        && platform_account.is_none()
                        && display_name.is_none()
                        && what.is_none()
                        && slot_ids.is_none(),
                    "merge accepts source_person_id, target_person_id and reason"
                );
                let source = parse_person_id(
                    &source_person_id.ok_or_else(|| anyhow::anyhow!("merge requires source_person_id"))?,
                    "source_person_id",
                )?;
                let target = parse_person_id(
                    &target_person_id.ok_or_else(|| anyhow::anyhow!("merge requires target_person_id"))?,
                    "target_person_id",
                )?;
                FrozenQuery::Merge { source, target, reason }
            }
            PersonOp::Split => {
                ensure!(
                    alias.is_none()
                        && platform_account.is_none()
                        && display_name.is_none()
                        && what.is_none(),
                    "split accepts source_person_id, target_person_id, slot_ids and reason"
                );
                let source = parse_person_id(
                    &source_person_id.ok_or_else(|| anyhow::anyhow!("split requires source_person_id"))?,
                    "source_person_id",
                )?;
                let target = parse_person_id(
                    &target_person_id.ok_or_else(|| anyhow::anyhow!("split requires target_person_id"))?,
                    "target_person_id",
                )?;
                let slot_ids = slot_ids
                    .ok_or_else(|| anyhow::anyhow!("split requires slot_ids"))?
                    .iter()
                    .map(|id| parse_person_id(id, "slot_ids"))
                    .collect::<Result<Vec<_>>>()?;
                ensure!(!slot_ids.is_empty(), "split requires at least one slot");
                FrozenQuery::Split { target, source, slot_ids, reason }
            }
        };
        Ok(FrozenInput { schema_version: 1, query })
    }

    fn validate_record(&self, record: FrozenRecord) -> Result<FrozenRecord> {
        Ok(match record {
            FrozenRecord::Alias { alias, scope_ref } => {
                let alias = alias.trim();
                ensure!(!alias.is_empty(), "alias must not be empty");
                ensure!(
                    alias.chars().count() <= MAX_ALIAS_CHARS,
                    "alias exceeds {MAX_ALIAS_CHARS} characters"
                );
                FrozenRecord::Alias { alias: alias.to_owned(), scope_ref }
            }
            FrozenRecord::Relationship { relation, note } => {
                let relation = relation.trim();
                ensure!(!relation.is_empty(), "relationship requires a relation");
                ensure!(
                    relation.chars().count() <= MAX_RELATION_CHARS,
                    "relation exceeds {MAX_RELATION_CHARS} characters"
                );
                if let Some(note) = &note {
                    ensure!(
                        note.chars().count() <= MAX_NOTE_CHARS,
                        "note exceeds {MAX_NOTE_CHARS} characters"
                    );
                }
                FrozenRecord::Relationship { relation: relation.to_owned(), note }
            }
            FrozenRecord::Account {
                provider,
                account,
                tenant,
                platform_user_id,
            } => {
                ensure!(
                    !provider.trim().is_empty()
                        && !account.trim().is_empty()
                        && !platform_user_id.trim().is_empty(),
                    "platform accounts require provider, account and platform_user_id"
                );
                FrozenRecord::Account { provider, account, tenant, platform_user_id }
            }
            FrozenRecord::Speaker {
                speaker,
                person_id,
                confidence,
                source,
                corrected,
                recording_scope,
            } => {
                ensure!(!speaker.trim().is_empty(), "speaker requires a provider tag");
                if let Some(value) = confidence {
                    ensure!((0.0..=1.0).contains(&value), "confidence must be within [0, 1]");
                }
                FrozenRecord::Speaker {
                    speaker,
                    person_id,
                    confidence,
                    source,
                    corrected,
                    recording_scope,
                }
            }
            FrozenRecord::LinkTask { task_id, note } => {
                if let Some(note) = &note {
                    ensure!(
                        note.chars().count() <= MAX_NOTE_CHARS,
                        "note exceeds {MAX_NOTE_CHARS} characters"
                    );
                }
                FrozenRecord::LinkTask { task_id, note }
            }
            other => other,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "person_directory_frozen_input_mismatch"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        ensure!(
            frozen.schema_version == 1,
            "unsupported_person_directory_input_schema"
        );
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "person.directory.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "local_person_directory".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "person-directory-binding")?;
        let acceptance = self.seal(
            &"a bounded read or an append-only revision of the local person directory, \
              or an explicit typed failure",
            "person-directory-acceptance",
        )?;
        let retry = self.seal(
            &"re-run only the frozen query under the original armed attempt; \
              every write is idempotent on replay",
            "person-directory-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:person-directory-v1".into(),
            },
            symbol: "person.directory.execute".into(),
            schema: SchemaRef {
                name: "person-directory-observation-v1".into(),
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
            operation_name: "person.directory.v1".into(),
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
}

fn claim_json(claim: &PersonClaim) -> serde_json::Value {
    serde_json::json!({
        "claim_id": claim.claim_id,
        "slot_id": claim.slot_id,
        "kind": claim.kind.as_str(),
        "claim_key": claim.claim_key,
        "revision": claim.revision,
        "state": claim.state.as_str(),
        "payload": claim.payload,
        "confidence": claim.confidence,
        "provenance": claim.provenance,
        "scope": claim.scope,
        "created_at_ms": claim.created_at_ms,
        "commit_seq": claim.commit_seq,
    })
}

fn display_name_of(view: &PersonView) -> Option<String> {
    view.claims
        .iter()
        .find(|c| c.kind == PersonClaimKind::Profile)
        .and_then(|c| {
            c.payload
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
}

impl PersonDirectoryHandler {
    fn provenance(&self) -> Provenance {
        Provenance {
            source: ClaimSource::ModelInference {
                model: "subject-self".into(),
                revision: None,
            },
            evidence_ref: None,
            observed_at_ms: None,
        }
    }

    fn view_json(&self, view: &PersonView) -> serde_json::Value {
        let mut memory = Vec::new();
        let mut tasks = Vec::new();
        for claim in &view.claims {
            match claim.kind {
                PersonClaimKind::MemoryAnchor => {
                    let Some(entry_id) = claim
                        .payload
                        .get("entry_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<EntityId>().ok())
                    else {
                        continue;
                    };
                    let mut anchor = claim_json(claim);
                    match self.store.memory_entry(entry_id) {
                        Ok(Some(entry)) => {
                            let mut chars = entry.content.chars();
                            let preview: String =
                                chars.by_ref().take(MAX_MEMORY_PREVIEW_CHARS).collect();
                            anchor["memory"] = serde_json::json!({
                                "entry_id": entry.entry_id,
                                "kind": entry.kind,
                                "scope": entry.scope,
                                "created_at_ms": entry.created_at_ms,
                                "content_preview": preview,
                                "content_preview_truncated": chars.next().is_some(),
                            });
                        }
                        Ok(None) => {
                            anchor["memory"] = serde_json::json!({ "missing": true });
                        }
                        Err(error) => {
                            anchor["memory"] =
                                serde_json::json!({ "read_error": error.to_string() });
                        }
                    }
                    memory.push(anchor);
                }
                PersonClaimKind::TaskAnchor => {
                    let Some(task_id) = claim
                        .payload
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<EntityId>().ok())
                    else {
                        continue;
                    };
                    let mut anchor = claim_json(claim);
                    match self.store.get_task(task_id) {
                        Ok(Some(task)) => {
                            let state = serde_json::to_value(&task.state)
                                .ok()
                                .and_then(|v| v.get("status").cloned())
                                .unwrap_or_else(|| serde_json::json!("unknown"));
                            anchor["task"] = serde_json::json!({
                                "task_id": task.task_id,
                                "kind": task.kind,
                                "state": state,
                                "first_run_at": task.first_run_at,
                            });
                        }
                        Ok(None) | Err(_) => {
                            anchor["task"] = serde_json::json!({ "missing": true });
                        }
                    }
                    tasks.push(anchor);
                }
                _ => {}
            }
        }

        let other: Vec<serde_json::Value> = view
            .claims
            .iter()
            .filter(|c| {
                !matches!(
                    c.kind,
                    PersonClaimKind::MemoryAnchor | PersonClaimKind::TaskAnchor
                )
            })
            .map(claim_json)
            .collect();

        serde_json::json!({
            "person_id": view.person_id,
            "merged_from": view.merged_from,
            "display_name": display_name_of(view),
            "claims": other,
            "memory_anchors": memory,
            "task_anchors": tasks,
            "admin": view.admin,
        })
    }

    fn lookup(&self, agent_id: EntityId, selector: &FrozenSelector) -> Result<serde_json::Value> {
        let mut views = Vec::new();
        match selector {
            FrozenSelector::PersonId { person_id } => {
                if let Some(view) = self.store.person_view(agent_id, *person_id)? {
                    views.push(view);
                }
            }
            FrozenSelector::PlatformAccount {
                provider,
                account,
                tenant,
                platform_user_id,
            } => {
                views = self.store.person_by_platform_account(
                    agent_id,
                    &PlatformAccount {
                        provider: provider.clone(),
                        account: account.clone(),
                        tenant: tenant.clone(),
                        platform_user_id: platform_user_id.clone(),
                    },
                )?;
            }
            FrozenSelector::Alias { alias } => {
                views = self.store.person_by_alias(agent_id, alias)?;
            }
        }
        views.truncate(MAX_LOOKUPS);
        Ok(serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "op": "lookup",
            "persons": views.iter().map(|v| self.view_json(v)).collect::<Vec<_>>(),
        }))
    }

    fn admin_status(
        &self,
        agent_id: EntityId,
        selector: &FrozenSelector,
    ) -> Result<serde_json::Value> {
        let account_key = match selector {
            FrozenSelector::PersonId { person_id } => {
                let person = self.store.admin_fact(
                    agent_id,
                    AdminSubjectKind::Person,
                    &person_id.to_string(),
                )?;
                serde_json::json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "admin_status",
                    "person_id": person_id,
                    "admin": person,
                    "note": "管理员由 owner 显式设置，与亲密度、群身份、平台角色和声纹无关；\
                             普通对话默认开放，本接口只读。",
                })
            }
            FrozenSelector::PlatformAccount {
                provider,
                account,
                tenant,
                platform_user_id,
            } => {
                let platform = PlatformAccount {
                    provider: provider.clone(),
                    account: account.clone(),
                    tenant: tenant.clone(),
                    platform_user_id: platform_user_id.clone(),
                };
                let fact = self.store.admin_fact(
                    agent_id,
                    AdminSubjectKind::PlatformAccount,
                    &platform.key(),
                )?;
                serde_json::json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "admin_status",
                    "account": platform,
                    "admin": fact,
                    "note": "管理员由 owner 显式设置，与亲密度、群身份、平台角色和声纹无关；\
                             普通对话默认开放，本接口只读。",
                })
            }
            FrozenSelector::Alias { alias } => {
                let views = self.store.person_by_alias(agent_id, alias)?;
                let admin = match views.first() {
                    Some(view) => self.store.admin_fact(
                        agent_id,
                        AdminSubjectKind::Person,
                        &view.person_id.to_string(),
                    )?,
                    None => None,
                };
                serde_json::json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "admin_status",
                    "alias": alias,
                    "persons_matched": views.len(),
                    "admin": admin,
                    "note": "管理员由 owner 显式设置，与亲密度、群身份、平台角色和声纹无关；\
                             普通对话默认开放，本接口只读。",
                })
            }
        };
        Ok(account_key)
    }

    fn record(
        &self,
        agent_id: EntityId,
        person_id: PersonId,
        what: &FrozenRecord,
    ) -> Result<(serde_json::Value, OperationOutcome)> {
        let provenance = self.provenance();
        let input = match what {
            FrozenRecord::Create { display_name } => PersonClaimInput {
                person_id,
                kind: PersonClaimKind::Profile,
                claim_key: "profile".into(),
                payload: serde_json::json!({ "display_name": display_name }),
                provenance,
                confidence: Confidence::unknown(),
                state: PersonClaimState::Active,
                scope: ClaimScope::private(),
                slot_id: None,
            },
            FrozenRecord::Alias { alias, scope_ref } => PersonClaimInput {
                person_id,
                kind: PersonClaimKind::Alias,
                claim_key: format!(
                    "alias|{}|{alias}",
                    scope_ref.as_deref().unwrap_or("default")
                ),
                payload: serde_json::json!({ "alias": alias }),
                provenance,
                confidence: Confidence::inferred(None),
                state: PersonClaimState::Active,
                scope: ClaimScope {
                    kind: ClaimScopeKind::Conversation,
                    reference: scope_ref.clone(),
                },
                slot_id: Some(person_slot_id(
                    agent_id,
                    PersonClaimKind::Alias,
                    person_id,
                    alias,
                )),
            },
            FrozenRecord::Account {
                provider,
                account,
                tenant,
                platform_user_id,
            } => {
                let platform = PlatformAccount {
                    provider: provider.clone(),
                    account: account.clone(),
                    tenant: tenant.clone(),
                    platform_user_id: platform_user_id.clone(),
                };
                PersonClaimInput {
                    person_id,
                    kind: PersonClaimKind::PlatformAccount,
                    claim_key: platform.key(),
                    payload: serde_json::to_value(&platform)?,
                    provenance,
                    confidence: Confidence::inferred(None),
                    // The subject's association guess; verified bindings are
                    // admitted by the platform side, never by this surface.
                    state: PersonClaimState::Candidate,
                    scope: ClaimScope {
                        kind: ClaimScopeKind::Platform,
                        reference: Some(provider.clone()),
                    },
                    slot_id: None,
                }
            }
            FrozenRecord::Relationship { relation, note } => PersonClaimInput {
                person_id,
                kind: PersonClaimKind::Relationship,
                claim_key: format!("relationship|{relation}"),
                payload: serde_json::json!({ "relation": relation, "note": note }),
                provenance,
                confidence: Confidence::inferred(None),
                state: PersonClaimState::Active,
                scope: ClaimScope::private(),
                slot_id: Some(person_slot_id(
                    agent_id,
                    PersonClaimKind::Relationship,
                    person_id,
                    relation,
                )),
            },
            FrozenRecord::Speaker {
                speaker,
                person_id: linked,
                confidence,
                source,
                corrected,
                recording_scope,
            } => PersonClaimInput {
                person_id: *linked,
                kind: PersonClaimKind::SpeakerObservation,
                claim_key: format!("speaker|{speaker}"),
                payload: serde_json::json!({
                    "speaker": speaker,
                    "observed_person_id": person_id,
                    "corrected": corrected,
                    "recording_scope": recording_scope,
                }),
                provenance: Provenance {
                    source: ClaimSource::MediaProvider {
                        provider: "speaker-identity".into(),
                        model_revision: source.clone(),
                    },
                    evidence_ref: None,
                    observed_at_ms: None,
                },
                confidence: Confidence::inferred(*confidence),
                // Speaker matches are clues; they never grant identity or
                // admin authority (§15.2, R060).
                state: PersonClaimState::Candidate,
                scope: ClaimScope {
                    kind: ClaimScopeKind::Recording,
                    reference: recording_scope.clone(),
                },
                slot_id: Some(person_slot_id(
                    agent_id,
                    PersonClaimKind::SpeakerObservation,
                    *linked,
                    speaker,
                )),
            },
            FrozenRecord::LinkMemory { entry_id } => PersonClaimInput {
                person_id,
                kind: PersonClaimKind::MemoryAnchor,
                claim_key: format!("memory|{entry_id}"),
                payload: serde_json::json!({ "entry_id": entry_id }),
                provenance,
                confidence: Confidence::unknown(),
                state: PersonClaimState::Active,
                scope: ClaimScope::private(),
                slot_id: Some(person_slot_id(
                    agent_id,
                    PersonClaimKind::MemoryAnchor,
                    person_id,
                    &entry_id.to_string(),
                )),
            },
            FrozenRecord::LinkTask { task_id, note } => PersonClaimInput {
                person_id,
                kind: PersonClaimKind::TaskAnchor,
                claim_key: format!("task|{task_id}"),
                payload: serde_json::json!({ "task_id": task_id, "note": note }),
                provenance,
                confidence: Confidence::unknown(),
                state: PersonClaimState::Active,
                scope: ClaimScope::private(),
                slot_id: Some(person_slot_id(
                    agent_id,
                    PersonClaimKind::TaskAnchor,
                    person_id,
                    &task_id.to_string(),
                )),
            },
        };

        let receipt = self.store.admit_person_claim(agent_id, &input)?;
        let outcome = OperationOutcome::Succeeded;
        let view = self.store.person_view(agent_id, receipt.person_id)?;
        Ok((
            serde_json::json!({
                "schema_version": 1,
                "status": "ok",
                "op": "record",
                "receipt": receipt,
                "person": view.as_ref().map(|v| self.view_json(v)),
            }),
            outcome,
        ))
    }

    fn query(&self, agent_id: EntityId, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome)> {
        match &frozen.query {
            FrozenQuery::Lookup { selector } => {
                Ok((self.lookup(agent_id, selector)?, OperationOutcome::Succeeded))
            }
            FrozenQuery::List { limit } => {
                let views = self.store.persons(agent_id, *limit)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "list",
                        "persons": views
                            .iter()
                            .map(|view| serde_json::json!({
                                "person_id": view.person_id,
                                "display_name": display_name_of(view),
                                "claim_kinds": view.claims.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(),
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::Record { person_id, what } => self.record(agent_id, *person_id, what),
            FrozenQuery::Merge { source, target, reason } => {
                let event = self.store.merge_persons(
                    agent_id,
                    *source,
                    *target,
                    reason.clone(),
                    self.provenance(),
                )?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "merge",
                        "event": event,
                        "person_id": event.target_person_id,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::Split { target, source, slot_ids, reason } => {
                let event = self.store.split_persons(
                    agent_id,
                    *target,
                    *source,
                    slot_ids,
                    reason.clone(),
                    self.provenance(),
                )?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "split",
                        "event": event,
                        "person_id": event.source_person_id,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::AdminStatus { selector } => {
                Ok((self.admin_status(agent_id, selector)?, OperationOutcome::Succeeded))
            }
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        agent_id: EntityId,
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = match self.query(agent_id, frozen) {
            Ok((value, outcome)) => (value, outcome),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": { "code": "person_directory_query_failed", "message": format!("{error:#}") },
                }),
                OperationOutcome::Failed,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "person-directory-observation")?;
        let normalized = if bytes.len() as u64 <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            outcome = OperationOutcome::Failed;
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; \
                                    narrow the lookup before delivery"
                    }
                }),
                "person-directory-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "person.directory.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("person-directory:{}", attempt.attempt_id)),
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
            invocation.task.kind == "person.directory"
                && invocation.task.definition_version == 1,
            "person_directory_task_identity_mismatch"
        );
        let agent_id = invocation.task.agent_id;
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "person-directory-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        self.observe(&operation, &attempt, agent_id, &frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for PersonDirectoryHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "person.directory".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-person-directory/1".into(),
            authority_refs: vec!["person.directory.read/v1".into(), "person.directory.write/v1".into()],
            prompt_section: Some(
                "person.directory/v1 是已安装的联系人目录 child，负责稳定的人 ID、平台账号、别名、关系及其修订历史。\
                 查人：[delegate:person.directory:1] {\"op\":\"lookup\",\"alias\":\"小王\"}（也可按 person_id 或 platform_account 查）；\
                 列人用 op=list；记一条用 op=record：新建人 {\"op\":\"record\",\"what\":{\"what\":\"create\",\"display_name\":\"…\"}}，\
                 给人加别名/账号/关系/记忆或任务锚点时在 what 外加 person_id。账号绑定与说话人线索都以候选置信记录，\
                 已验证绑定只能来自平台侧；合并用 op=merge、撤销合并用 op=split。\
                 管理员状态用 op=admin_status 只读查询——管理员只能由 owner 显式设置，和亲密度、群身份、声纹无关，普通对话默认开放。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "person.directory.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative; a later
            // successful read must not erase it.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, task.agent_id, &frozen)
    }
}
