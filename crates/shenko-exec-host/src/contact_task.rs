//! Read-only native NapCat contact/identity queries — SPEC §15.2, §16, §35.6.
//!
//! This handler runs `get_login_info` / `get_friend_list` / `get_stranger_info`
//! / `get_group_list` / `get_group_member_list` / `get_group_member_info` over
//! the already-connected NapCat OneBot v11 WebSocket, using the same echo
//! correlation and durable operation/evidence path as every native child. The
//! action allowlist is the `ContactQuery` type itself: no caller can reach a
//! `send_*` action through this surface, and a missing or foreign response is
//! a typed observation — never a fabricated empty contact list.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_channel_napcat::{ContactQuery, NapCatClient, SendError};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::TaskRecord,
};
use std::sync::Arc;

const CONTRACT: &[u8] = b"shenko.napcat.contacts/v1: read-only OneBot v11 contact/identity queries over the connected socket; echo-correlated raw receipt sealed; typed unavailable/unknown/rejected outcomes; never an empty list in place of a missing answer";
const MAX_PAGE: usize = 200;
const MAX_OFFSET: usize = 1_000_000;
const MAX_NORMALIZED_BYTES: u64 = 480 * 1024;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContactOp {
    Identity,
    Friends,
    Contact,
    Groups,
    Members,
    Member,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContactInput {
    op: ContactOp,
    user_id: Option<u64>,
    group_id: Option<u64>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
    #[serde(default)]
    no_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenQuery {
    Identity,
    Friends { offset: usize, limit: usize },
    Contact { user_id: u64, no_cache: bool },
    Groups { offset: usize, limit: usize },
    Members { group_id: u64, offset: usize, limit: usize },
    Member { group_id: u64, user_id: u64, no_cache: bool },
}

impl FrozenQuery {
    fn label(&self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Friends { .. } => "friends",
            Self::Contact { .. } => "contact",
            Self::Groups { .. } => "groups",
            Self::Members { .. } => "members",
            Self::Member { .. } => "member",
        }
    }

    fn wire(&self) -> ContactQuery {
        match *self {
            Self::Identity => ContactQuery::LoginInfo,
            Self::Friends { .. } => ContactQuery::FriendList,
            Self::Contact { user_id, no_cache } => ContactQuery::StrangerInfo {
                user_id,
                no_cache,
            },
            Self::Groups { .. } => ContactQuery::GroupList,
            Self::Members { group_id, .. } => ContactQuery::GroupMemberList { group_id },
            Self::Member {
                group_id,
                user_id,
                no_cache,
            } => ContactQuery::GroupMemberInfo {
                group_id,
                user_id,
                no_cache,
            },
        }
    }

    fn page(&self) -> Option<(usize, usize)> {
        match *self {
            Self::Friends { offset, limit }
            | Self::Groups { offset, limit }
            | Self::Members { offset, limit, .. } => Some((offset, limit)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    schema_version: u32,
    query: FrozenQuery,
}

/// What came back over the echo-correlated wire, classified for the result.
enum WireExchange {
    /// A response frame matched our echo token.
    Answered(shenko_channel_napcat::QueryReceipt),
    /// The request provably never reached the socket (action channel closed).
    Unavailable(String),
    /// The request may have reached NapCat; no matched response arrived.
    Unknown(String),
}

pub struct NapCatContactsHandler {
    store: Arc<Store>,
    client: Arc<NapCatClient>,
}

impl NapCatContactsHandler {
    /// The handler is only constructed with the real connected client; without
    /// a NapCat connection it is not installed and not advertised.
    pub fn new(store: Arc<Store>, client: Arc<NapCatClient>) -> Self {
        Self { store, client }
    }

    fn plugin_id() -> EntityId {
        "4a3d8e9f-6b2c-4d5e-a7f8-9c1d2e3f4a5b"
            .parse()
            .expect("fixed napcat-contacts plugin UUID")
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "napcat-contact-queries",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenInput> {
        let ContactInput {
            op,
            user_id,
            group_id,
            offset,
            limit,
            no_cache,
        } = serde_json::from_value(value)?;
        ensure!(offset <= MAX_OFFSET, "offset exceeds 1000000");
        let limit_was_specified = limit.is_some();
        let limit = limit.unwrap_or(50);
        ensure!((1..=MAX_PAGE).contains(&limit), "limit must be 1..=200");
        let page_args = || {
            ensure!(
                user_id.is_none() && group_id.is_none() && !no_cache,
                "list queries accept only offset/limit"
            );
            Ok::<(usize, usize), anyhow::Error>((offset, limit))
        };
        let query = match op {
            ContactOp::Identity => {
                ensure!(
                    user_id.is_none()
                        && group_id.is_none()
                        && offset == 0
                        && !limit_was_specified
                        && !no_cache,
                    "identity accepts no target or pagination fields"
                );
                FrozenQuery::Identity
            }
            ContactOp::Friends => {
                let (offset, limit) = page_args()?;
                FrozenQuery::Friends { offset, limit }
            }
            ContactOp::Contact => {
                let user_id = user_id.context("contact requires user_id")?;
                ensure!(user_id > 0, "user_id must be a nonzero unsigned id");
                ensure!(
                    group_id.is_none() && offset == 0 && !limit_was_specified,
                    "contact accepts exactly user_id (and no_cache)"
                );
                FrozenQuery::Contact { user_id, no_cache }
            }
            ContactOp::Groups => {
                let (offset, limit) = page_args()?;
                FrozenQuery::Groups { offset, limit }
            }
            ContactOp::Members => {
                let group_id = group_id.context("members requires group_id")?;
                ensure!(group_id > 0, "group_id must be a nonzero unsigned id");
                ensure!(user_id.is_none() && !no_cache, "members accepts group_id plus offset/limit");
                FrozenQuery::Members {
                    group_id,
                    offset,
                    limit,
                }
            }
            ContactOp::Member => {
                let group_id = group_id.context("member requires group_id")?;
                let user_id = user_id.context("member requires user_id")?;
                ensure!(
                    group_id > 0 && user_id > 0,
                    "group_id/user_id must be nonzero unsigned ids"
                );
                ensure!(
                    offset == 0 && !limit_was_specified,
                    "member accepts exactly group_id and user_id (and no_cache)"
                );
                FrozenQuery::Member {
                    group_id,
                    user_id,
                    no_cache,
                }
            }
        };
        Ok(FrozenInput {
            schema_version: 1,
            query,
        })
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "napcat.onebot.ws.v1".into(),
            account_identity: self.client.config.bot_qq.to_string(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_onebot_contacts".into(),
            extra: serde_json::json!({
                "ws_url": shenko_channel_napcat::endpoint_provenance(&self.client.config.ws_url),
                "action_allowlist": ContactQuery::ACTIONS,
            }),
        };
        let resource = self.seal(&bindings, "napcat-contacts-binding")?;
        let acceptance = self.seal(
            &"one echo-matched OneBot read response bound to the configured account, or a typed unavailable/unknown outcome",
            "napcat-contacts-acceptance",
        )?;
        let retry = self.seal(
            &"a read-only query may be re-issued only when the original attempt provably ended without a request reaching the socket; an answered-or-unknown armed attempt is never replayed",
            "napcat-contacts-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:napcat-contacts-v1".into(),
            },
            symbol: "napcat.contacts.execute".into(),
            schema: SchemaRef {
                name: "napcat-contacts-observation-v1".into(),
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
            operation_name: "napcat.contacts.v1".into(),
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

    /// One wire exchange bound to the armed attempt. The echo token is the
    /// attempt identity so a late pre-crash response can never be mistaken for
    /// a new request's answer.
    async fn exchange(
        &self,
        attempt_id: EntityId,
        query: &FrozenQuery,
    ) -> WireExchange {
        match self
            .client
            .query_with_echo(query.wire(), attempt_id.to_string())
            .await
        {
            Ok(receipt) => WireExchange::Answered(receipt),
            Err(SendError::Disconnected) => {
                WireExchange::Unavailable("napcat action channel closed".into())
            }
            Err(SendError::Unknown(detail)) => WireExchange::Unknown(detail),
        }
    }

    /// Classify a matched raw frame into result status, outcome and the
    /// bounded normalized payload. The raw frame is always sealed separately;
    /// this decides what the subject may rely on.
    fn classify(
        &self,
        frozen: &FrozenInput,
        receipt: &shenko_channel_napcat::QueryReceipt,
    ) -> (serde_json::Value, OperationOutcome) {
        let parsed = receipt.receipt();
        let retcode = parsed.retcode;
        // Read queries accept only a real "ok" status with retcode 0. The
        // send-path is_success() also tolerates "async"; a query's answer must
        // actually be present, so any other status is a rejection.
        let succeeded = parsed.status == "ok" && retcode == 0;
        let mut result = serde_json::json!({
            "schema_version": 1,
            "op": frozen.query.label(),
            "account": receipt.configured_account,
            "observed_self_id": receipt.observed_self_id(),
            "ws_url": receipt.ws_url,
            "echo": receipt.echo,
            "observed_at": receipt.observed_at,
            "wire_status": parsed.status.clone(),
            "retcode": retcode,
            "message": parsed.message.clone(),
        });
        if let Some(foreign) = receipt.foreign_self_id() {
            result["status"] = serde_json::json!("account_mismatch");
            result["error"] = serde_json::json!({
                "code": "foreign_self_id",
                "message": format!(
                    "response carried self_id {foreign}, not the configured account {}",
                    receipt.configured_account
                ),
            });
            // A foreign account's list is not this bot's contact data: the raw
            // frame is sealed but no items are projected.
            return (result, OperationOutcome::Failed);
        }
        if !succeeded {
            result["status"] = serde_json::json!("rejected");
            result["error"] = serde_json::json!({
                "code": "provider_retcode",
                "retcode": retcode,
                "message": parsed.message.clone().unwrap_or_else(|| "napcat rejected the query".into()),
            });
            return (result, OperationOutcome::Failed);
        }
        match frozen.query.page() {
            Some((offset, limit)) => {
                let Some(items) = parsed.data.as_array() else {
                    result["status"] = serde_json::json!("malformed");
                    result["error"] = serde_json::json!({
                        "code": "unexpected_payload",
                        "message": "expected an array payload; exact raw response is sealed",
                    });
                    return (result, OperationOutcome::Failed);
                };
                let total = items.len();
                let page: Vec<&serde_json::Value> =
                    items.iter().skip(offset).take(limit).collect();
                result["status"] = serde_json::json!("ok");
                result["total"] = serde_json::json!(total);
                result["offset"] = serde_json::json!(offset);
                result["limit"] = serde_json::json!(limit);
                result["truncated"] = serde_json::json!(offset + page.len() < total);
                result["items"] = serde_json::json!(page);
                (result, OperationOutcome::Succeeded)
            }
            None => {
                // Single-target reads require exactly one object payload — a
                // list is never an identity or a member record.
                if !parsed.data.is_object() {
                    result["status"] = serde_json::json!("malformed");
                    result["error"] = serde_json::json!({
                        "code": "unexpected_payload",
                        "message": "expected an object payload; exact raw response is sealed",
                    });
                    return (result, OperationOutcome::Failed);
                }
                // Identity reads bind the observed account inside the payload:
                // even a frame without a top-level self_id cannot be admitted
                // when the login identity is not the configured account.
                if matches!(frozen.query, FrozenQuery::Identity) {
                    let observed = parsed.data.get("user_id").and_then(|v| v.as_u64());
                    if observed != Some(receipt.configured_account) {
                        result["status"] = serde_json::json!("account_mismatch");
                        result["error"] = serde_json::json!({
                            "code": "foreign_login_identity",
                            "message": format!(
                                "login identity {observed:?} is not the configured account {}",
                                receipt.configured_account
                            ),
                        });
                        return (result, OperationOutcome::Failed);
                    }
                }
                result["status"] = serde_json::json!("ok");
                result["data"] = parsed.data;
                (result, OperationOutcome::Succeeded)
            }
        }
    }

    async fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let wire = self.exchange(attempt.attempt_id, &frozen.query).await;
        let (normalized, outcome, effect, decode_status, level) = match &wire {
            WireExchange::Answered(receipt) => {
                let (mut result, outcome) = self.classify(frozen, receipt);
                if serde_json::to_vec(&result)?.len() as u64 > MAX_NORMALIZED_BYTES {
                    result = serde_json::json!({
                        "schema_version": 1,
                        "status": "failed",
                        "op": frozen.query.label(),
                        "error": {
                            "code": "normalized_result_too_large",
                            "message": "the exact response is sealed in raw evidence; request a smaller page"
                        }
                    });
                    (
                        result,
                        Some(OperationOutcome::Failed),
                        EffectStatus::None,
                        DecodeStatus::Decoded,
                        None,
                    )
                } else {
                    let level = if outcome == OperationOutcome::Succeeded {
                        Some(ObservationLevel::PostconditionVerified)
                    } else {
                        Some(ObservationLevel::RemoteCompleted)
                    };
                    (
                        result,
                        Some(outcome),
                        EffectStatus::None,
                        DecodeStatus::Decoded,
                        level,
                    )
                }
            }
            WireExchange::Unavailable(detail) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "unavailable",
                    "op": frozen.query.label(),
                    "account": self.client.config.bot_qq,
                    "error": {"code": "connector_unavailable", "message": detail},
                }),
                Some(OperationOutcome::Failed),
                EffectStatus::None,
                DecodeStatus::Uninterpreted,
                None,
            ),
            WireExchange::Unknown(detail) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "unknown",
                    "op": frozen.query.label(),
                    "account": self.client.config.bot_qq,
                    "error": {
                        "code": "response_unknown",
                        "message": format!(
                            "no matched response; the request may have reached napcat ({detail})"
                        ),
                    },
                }),
                // No matched answer: the request may have been served. Like the
                // printf transport deadline this records no outcome at all —
                // reconcile may only seal recovery observations, never replay.
                None,
                EffectStatus::Unknown,
                DecodeStatus::Uninterpreted,
                None,
            ),
        };
        // The raw observation always carries what actually crossed the wire:
        // the untouched matched frame, or the transport fact that no frame did.
        let raw_value = match &wire {
            WireExchange::Answered(receipt) => serde_json::json!({
                "operation_id": operation.operation_id,
                "attempt_id": attempt.attempt_id,
                "action": receipt.action,
                "echo": receipt.echo,
                "configured_account": receipt.configured_account,
                "ws_url": receipt.ws_url,
                "observed_at": receipt.observed_at,
                "raw_response": receipt.raw,
            }),
            WireExchange::Unavailable(detail) => serde_json::json!({
                "operation_id": operation.operation_id,
                "attempt_id": attempt.attempt_id,
                "configured_account": self.client.config.bot_qq,
                "transport": "unavailable",
                "detail": detail,
            }),
            WireExchange::Unknown(detail) => serde_json::json!({
                "operation_id": operation.operation_id,
                "attempt_id": attempt.attempt_id,
                "configured_account": self.client.config.bot_qq,
                "transport": "unknown",
                "detail": detail,
            }),
        };
        let raw = self.seal(&raw_value, "napcat-contacts-observation")?;
        let normalized_ref = self.seal(&normalized, "napcat-contacts-result")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "napcat.contacts.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("onebot-echo:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status,
            observation_level: level,
            normalized_ref: Some(normalized_ref),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            effect,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "napcat.contacts"
                && invocation.task.definition_version == 1,
            "contacts_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "napcat-contacts-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        // T1 is durable before any bytes leave the process.
        self.observe(&operation, &attempt, &frozen).await
    }
}

#[async_trait]
impl NativeTaskHandler for NapCatContactsHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "napcat.contacts".into(),
            definition_version: 1,
            implementation_ref: shenko_types::task::ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "napcat-contact-reader/1".into(),
            authority_refs: vec!["napcat.onebot.read.contacts/v1".into()],
            prompt_section: Some(
                "napcat.contacts/v1 是已安装的只读 QQ 联系人与身份 child（NapCat OneBot，现有连接）。\
                 查询：[delegate:napcat.contacts:1] {\"op\":\"identity\"} 读当前 bot 账号身份；\
                 {\"op\":\"friends\",\"offset\":0,\"limit\":50} 列好友；\
                 {\"op\":\"contact\",\"user_id\":123456} 按精确 QQ 号读资料；\
                 {\"op\":\"groups\",\"offset\":0,\"limit\":50} 列群；\
                 {\"op\":\"members\",\"group_id\":123,\"offset\":0,\"limit\":50} 列群成员；\
                 {\"op\":\"member\",\"group_id\":123,\"user_id\":456} 读指定群成员。\
                 limit 1..200，列表分页用 total/offset/truncated 判断；contact/member 可加 \"no_cache\":true。\
                 结果是后续真实 task_result；status=ok 才有数据，rejected/unavailable/unknown/account_mismatch 是真实分类，\
                 绝不能把失败或没回答当成“没有联系人/不在群里”。"
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
            operation.task_id == task.task_id
                && operation.operation_name == "napcat.contacts.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        if operation.state == OperationState::InFlight {
            let attempt = self.store.external_operation_attempt(operation_id)?;
            if attempt.dispatch_stage != AttemptDispatchStage::Ended {
                // The controller may have crashed between T1 and the wire
                // answer. Whether the request ever reached NapCat is unknown:
                // seal that fact as evidence and keep the original operation
                // unresolved — never arm a second attempt during reconcile.
                let raw = self.seal(
                    &serde_json::json!({
                        "operation_id": operation_id,
                        "attempt_id": attempt.attempt_id,
                        "observation": "controller recovered without a conclusive wire answer for the original echo",
                        "request_delivery_proven": false,
                    }),
                    "napcat-contacts-recovery",
                )?;
                let evidence = EvidenceReceipt {
                    record_schema_version: 1,
                    receipt_id: EntityId::new(),
                    operation_id: Some(operation_id),
                    attempt_id: Some(attempt.attempt_id),
                    source_id: "napcat.contacts.recovery".into(),
                    source_instance_id: self.store.capabilities.incarnation.to_string(),
                    source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
                    producer_epoch: None,
                    producer_schema_version: Some("1".into()),
                    raw_ref: raw,
                    decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
                    decode_status: DecodeStatus::Decoded,
                    observation_level: None,
                    normalized_ref: None,
                    observed_at: Some(chrono::Utc::now()),
                    received_commit: operation.prepared_commit,
                };
                return self.store.record_external_evidence(
                    operation_id,
                    attempt.attempt_id,
                    evidence,
                    None,
                    EffectStatus::Unknown,
                );
            }
        }
        Ok(operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use shenko_channel_napcat::{NapCatClient, NapCatConfig};
    use shenko_store::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::WriteBatch;
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    fn make_task(handler: &NapCatContactsHandler) -> (TaskRecord, TaskInvocation, EntityId) {
        let (task_id, agent_id, invocation_id, operation_id) = (
            EntityId::new(),
            EntityId::new(),
            EntityId::new(),
            EntityId::new(),
        );
        let epoch = ControllerEpoch(1);
        let result_id = EntityId::new();
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut memos = serde_json::Map::new();
        memos.insert(
            shenko_store::child::NATIVE_RESULT_ID.into(),
            serde_json::Value::String(result_id.to_string()),
        );
        // The stored task only needs a consistent input object; run() re-freezes
        // and seals the actual input it was invoked with.
        let input_ref = handler
            .seal(&serde_json::json!({"schema_version": 0, "query": "pending"}), "task-input-placeholder")
            .unwrap();
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: EntityId::new() },
            background: false,
            kind: "napcat.contacts".into(),
            definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
            input_ref,
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos,
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        (task, invocation, operation_id)
    }

    fn invocation(
        task: TaskRecord,
        task_invocation: TaskInvocation,
        operation_id: EntityId,
        input: serde_json::Value,
    ) -> NativeTaskInvocation {
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task.task_id);
        context.invocation_id = Some(task_invocation.invocation_id);
        context.controller_epoch = task_invocation.epoch;
        NativeTaskInvocation {
            context,
            task_invocation,
            task,
            operation_id,
            authority: AuthorityRef {
                delegation_id: EntityId::new(),
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input,
        }
    }

    /// A real loopback exchange through the armed durable operation: the echoed
    /// answer is classified, paginated and sealed; rejected and foreign-account
    /// frames never become contact data.
    #[tokio::test]
    async fn contacts_query_resolves_durable_result_with_bound_identity() {
        let dir = std::env::temp_dir().join(format!("shenko-contacts-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        // T2 completion requires a current subject dispatcher owner: the same
        // gate production installs before any native child may settle. Arm/E1
        // do not need it, so install it before the first complete call rather
        // than faking task state to bypass the check.
        store.start_subject_dispatcher().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let config = NapCatConfig {
            ws_url: url.clone(),
            bot_qq: 9,
            reconnect_interval_ms: 50,
            ..Default::default()
        };
        let (client, _events, actions) = NapCatClient::new(config.clone());
        let client = Arc::new(client);
        let handler = NapCatContactsHandler::new(store.clone(), client.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let connector = tokio::spawn(NapCatClient::run_connection_loop(
            config,
            client.event_sender(),
            actions,
            client.pending_actions.clone(),
            client.spool.clone(),
            cancel.clone(),
        ));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let echo = request["echo"].as_str().unwrap_or_default().to_string();
                let response = match request["action"].as_str().unwrap_or_default() {
                    "get_friend_list" => json!({
                        "status": "ok", "retcode": 0, "echo": echo, "self_id": 9,
                        "data": [
                            {"user_id": 11, "nickname": "一"},
                            {"user_id": 22, "nickname": "二"},
                            {"user_id": 18446744073709551615u64, "nickname": "高位"}
                        ],
                    }),
                    "get_login_info" => json!({
                        // No top-level self_id: the payload's own user_id is
                        // what binds (or repudiates) the serving account.
                        "status": "ok", "retcode": 0, "echo": echo,
                        "data": {"user_id": 4242, "nickname": "foreign"},
                    }),
                    _ => continue,
                };
                ws.send(Message::Text(response.to_string().into())).await.unwrap();
            }
        });

        // 1. Paginated friend list: durable task → operation → evidence → result.
        let input = json!({"op":"friends","offset":1,"limit":1});
        let (task, task_invocation, operation_id) = make_task(&handler);
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let operation = handler
            .run(invocation(task.clone(), task_invocation.clone(), operation_id, input))
            .await
            .unwrap();
        assert_eq!(operation.state, OperationState::Resolved);
        assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded));
        let attempt = store.external_operation_attempt(operation_id).unwrap();
        assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Ended);
        let claim = shenko_store::child::NativeTaskClaim {
            task: store.external_task(task.task_id).unwrap(),
            invocation: task_invocation.clone(),
            operation_id,
            result_id: EntityId::nil(), // memo carries the stable id
            software_generation: RuntimeGeneration::default(),
            settlement_only: false,
        };
        let result = store.complete_native_task(&claim, &operation).unwrap().unwrap();
        assert_eq!(result.outcome, shenko_types::task::TaskOutcome::Succeeded);
        let normalized = result.result.unwrap();
        assert_eq!(normalized["status"], "ok");
        assert_eq!(normalized["total"], 3);
        assert_eq!(normalized["offset"], 1);
        assert_eq!(normalized["limit"], 1);
        assert_eq!(normalized["truncated"], true);
        assert_eq!(
            normalized["items"][0]["user_id"].as_u64(),
            Some(22)
        );
        assert_eq!(normalized["account"], 9);
        assert_eq!(normalized["observed_self_id"], 9);
        // Raw evidence binds the exact frame to the armed attempt's echo.
        let evidence = store.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let raw: serde_json::Value = serde_json::from_slice(
            &store.cas.read_object(&evidence[0].raw_ref.digest).unwrap(),
        ).unwrap();
        assert_eq!(raw["echo"], serde_json::json!(attempt.attempt_id.to_string()));
        assert_eq!(raw["raw_response"]["self_id"], 9);
        assert_eq!(
            raw["raw_response"]["data"][2]["user_id"].as_u64(),
            Some(u64::MAX)
        );

        // 2. A frame answering for a different login identity — even with no
        // top-level self_id — is a real observation, never this bot's data.
        let (task2, inv2, op2) = make_task(&handler);
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task2.clone());
        store.commit(batch).unwrap();
        let operation = handler
            .run(invocation(task2, inv2, op2, json!({"op":"identity"})))
            .await
            .unwrap();
        assert_eq!(operation.outcome, Some(OperationOutcome::Failed));
        let evidence = store.external_operation_evidence(op2).unwrap();
        let normalized: serde_json::Value = serde_json::from_slice(
            &store.cas.read_object(&evidence[0].normalized_ref.as_ref().unwrap().digest).unwrap(),
        ).unwrap();
        assert_eq!(normalized["status"], "account_mismatch");
        assert!(normalized.get("data").is_none());
        let raw: serde_json::Value = serde_json::from_slice(
            &store.cas.read_object(&evidence[0].raw_ref.digest).unwrap(),
        ).unwrap();
        assert_eq!(raw["raw_response"]["data"]["user_id"], 4242);
        assert!(raw["raw_response"].get("self_id").is_none());

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), connector)
            .await.unwrap().unwrap().unwrap();
        server.abort();
    }

    /// Provider-side answers that are not a real "ok"/retcode-0 answer — or a
    /// payload of the wrong shape — are classified as typed failures with the
    /// raw frame still sealed, never as empty contact data.
    #[tokio::test]
    async fn contacts_query_typed_failures_are_classified() {
        let dir = std::env::temp_dir().join(format!("shenko-contacts-fail-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = NapCatConfig {
            ws_url: format!("ws://{}", listener.local_addr().unwrap()),
            bot_qq: 9,
            reconnect_interval_ms: 50,
            ..Default::default()
        };
        let (client, _events, actions) = NapCatClient::new(config.clone());
        let client = Arc::new(client);
        let handler = NapCatContactsHandler::new(store.clone(), client.clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        let connector = tokio::spawn(NapCatClient::run_connection_loop(
            config,
            client.event_sender(),
            actions,
            client.pending_actions.clone(),
            client.spool.clone(),
            cancel.clone(),
        ));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let echo = request["echo"].as_str().unwrap_or_default().to_string();
                let response = match request["action"].as_str().unwrap_or_default() {
                    // An "ok" status carrying a failing retcode is still a
                    // rejection, not a successful empty profile.
                    "get_stranger_info" => json!({
                        "status": "ok", "retcode": 1404, "echo": echo, "self_id": 9,
                        "data": null, "message": "unknown user",
                    }),
                    // "async" is not an answer: a send may tolerate it, a read
                    // query must not.
                    "get_group_member_info" => json!({
                        "status": "async", "retcode": 0, "echo": echo, "self_id": 9,
                        "data": {"user_id": 7},
                    }),
                    _ => continue,
                };
                ws.send(Message::Text(response.to_string().into())).await.unwrap();
            }
        });

        let rejected_retcode =
            run_failing_case(&store, &handler, json!({"op":"contact","user_id":7})).await;
        assert_eq!(rejected_retcode["status"], "rejected");
        assert_eq!(rejected_retcode["retcode"], 1404);

        let rejected_status = run_failing_case(
            &store, &handler, json!({"op":"member","group_id":100,"user_id":7}),
        ).await;
        assert_eq!(rejected_status["status"], "rejected");
        assert_eq!(rejected_status["retcode"], 0);

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), connector)
            .await.unwrap().unwrap().unwrap();
        server.abort();
    }

    /// Drive one durable query expected to fail and return its normalized
    /// result: the raw frame and typed status are sealed, no contact data is.
    async fn run_failing_case(
        store: &Arc<Store>,
        handler: &NapCatContactsHandler,
        input: serde_json::Value,
    ) -> serde_json::Value {
        let (task, inv, op) = make_task(handler);
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let operation = handler.run(invocation(task, inv, op, input)).await.unwrap();
        assert_eq!(operation.outcome, Some(OperationOutcome::Failed));
        let evidence = store.external_operation_evidence(op).unwrap();
        assert_eq!(evidence.len(), 1);
        serde_json::from_slice::<serde_json::Value>(
            &store
                .cas
                .read_object(&evidence[0].normalized_ref.as_ref().unwrap().digest)
                .unwrap(),
        )
        .unwrap()
    }

    /// An armed operation whose controller vanished between T1 and the wire
    /// answer stays unresolved: recovery seals the gap as evidence and never
    /// replays a request that may already have been served.
    #[tokio::test]
    async fn armed_contacts_query_recovers_without_replay() {
        let dir = std::env::temp_dir().join(format!("shenko-contacts-recovery-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
        let store = Arc::new(Store::open(db.clone(), cas.clone()).unwrap());
        let (client, _events, _actions) = NapCatClient::new(NapCatConfig {
            bot_qq: 9,
            ..Default::default()
        });
        let handler = NapCatContactsHandler::new(store.clone(), Arc::new(client));
        let input = json!({"op":"groups","limit":10});
        let (task, task_invocation, operation_id) = make_task(&handler);
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let frozen = handler.freeze(input).unwrap();
        let input_ref = handler.seal(&frozen, "napcat-contacts-frozen-input").unwrap();
        let inv = invocation(task.clone(), task_invocation.clone(), operation_id, serde_json::Value::Null);
        let (mut operation, mut attempt) = handler.operation_parts(&inv, input_ref).unwrap();
        assert!(
            store
                .arm_external_operation(&task_invocation, &mut operation, &mut attempt)
                .unwrap()
                .is_none()
        );
        let attempt_id = attempt.attempt_id;
        assert_eq!(operation.state, OperationState::InFlight);
        // Deterministic crash point: T1 durable, no wire answer recorded.
        drop(handler);
        drop(store);
        let reopened = Arc::new(Store::open(db, cas).unwrap());
        let (client, _events, _actions) = NapCatClient::new(NapCatConfig {
            bot_qq: 9,
            ..Default::default()
        });
        let recovered = NapCatContactsHandler::new(reopened.clone(), Arc::new(client));
        let task = reopened.external_task(task.task_id).unwrap();
        let settled = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(settled.state, OperationState::Reconciling);
        assert_eq!(settled.outcome, None);
        assert_eq!(settled.effect_status, EffectStatus::Unknown);
        assert_eq!(
            reopened.external_operation_attempts(operation_id).unwrap().len(),
            1
        );
        let evidence = reopened.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let raw: serde_json::Value = serde_json::from_slice(
            &reopened.cas.read_object(&evidence[0].raw_ref.digest).unwrap(),
        ).unwrap();
        assert_eq!(raw["request_delivery_proven"], false);
        assert_eq!(raw["attempt_id"], serde_json::json!(attempt_id.to_string()));
        // Repeating reconcile adds no new request or evidence.
        let again = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(again.state, OperationState::Reconciling);
        assert_eq!(
            reopened.external_operation_evidence(operation_id).unwrap().len(),
            1
        );
        drop(recovered);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

}
