//! Durable web input service — SPEC §41.18 WebInputSubmission contract.
//!
//! One service owns admission (append/steer), the reconnect view, withdrawal
//! and UI-action submission. Transport success is never presented as subject
//! acceptance: every stage of the view is read back from the ledger.
//!
//! Placement honesty: with the current adapter contract (no verified in-request
//! control capability) the only placement this service produces is
//! `next_boundary` — the input is durably admitted and the subject's normal S0
//! selects it at the next admissible boundary (SPEC §21.12 UI02). `live_steer`
//! and `cancel_reseal` exist in the wire contract for the adapter generation
//! that ArchBroker/ArchSubject land; this service refuses to fabricate them.

use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_store::web_input::{WEB_INPUT_SOURCE, WebAdmission};
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::ids::EntityId;
use shenko_types::store::RawInboxEvent;

use crate::presentation::{
    ActionRejection, PresentationService, SurfaceCapabilities, capability_present,
    validate_action,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionMode {
    Append,
    Steer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionStage {
    Accepted,
    Conflict,
    Rejected,
    /// Admission outcome could not be established; never claims absence.
    Unavailable,
}

/// Where an admitted input is actually handled (§41.18 placement). Only
/// `NextBoundary` is produced until the adapter publishes a verified
/// in-request control capability; the other variants are wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementActual {
    Pending,
    NextBoundary,
    LiveSteer,
    CancelReseal,
}

#[derive(Debug, Clone, Serialize)]
pub struct Placement {
    pub requested: SubmissionMode,
    pub actual: PlacementActual,
    /// The request this steer was bound to at admission, if one was sealed.
    /// Never re-bound to a later request when the target ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WithdrawalView {
    pub cancel_id: String,
    pub commit_ref: String,
    pub result: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct InputReceipt {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub submission_id: String,
    pub stage: SubmissionStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SubmissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replayed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    pub detail: String,
}

/// Durable view of one submission — §41.18 get_web_input. Never invents
/// selected/committed/withdrawn facts.
#[derive(Debug, Clone, Serialize)]
pub struct SubmissionView {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub submission_id: String,
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_step_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawal: Option<WithdrawalView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail_note: Option<String>,
    /// Echo of the client's send-attempt counter so stale views cannot be
    /// applied to a newer retry; never a server-side sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_epoch: Option<u64>,
}

impl SubmissionView {
    /// The view used when the input service is unreachable — an honest
    /// "temporarily_unavailable", never a fabricated not_admitted.
    pub fn unavailable(submission_id: &str, send_epoch: Option<u64>) -> Self {
        Self {
            schema_version: 2,
            kind: "submission_view",
            submission_id: submission_id.to_owned(),
            result: "temporarily_unavailable",
            stage: None,
            event_id: None,
            accepted_commit: None,
            selected_step_id: None,
            committed_at: None,
            placement: None,
            withdrawal: None,
            detail_note: None,
            send_epoch,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSubmission {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub submission_id: String,
    pub message_id: String,
    pub subject_id: String,
    pub mode: SubmissionMode,
    pub content: String,
    #[serde(default)]
    pub attachment_refs: Vec<serde_json::Value>,
    #[serde(default)]
    pub rich_content_ref: Option<serde_json::Value>,
    #[serde(default)]
    pub observed_step_id: Option<String>,
    #[serde(default)]
    pub observed_request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawRequest {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub submission_id: String,
    pub subject_id: String,
    pub cancel_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSubmission {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    /// Dedup key owned by the client: "<document_id>:<action_id>:<nonce>".
    /// A retry with the same key and payload replays the original receipt.
    pub submission_id: String,
    pub subject_id: String,
    pub document_id: String,
    pub action_id: String,
    pub revision: String,
    pub nonce: String,
    pub payload: serde_json::Value,
}

/// The canonical payload admitted under one submission key, serialized with
/// fixed ordering so identical retries compare equal byte-for-byte.
fn submission_payload(submission: &UserSubmission) -> String {
    serde_json::json!({
        "schema_version": submission.schema_version,
        "type": &submission.kind,
        "submission_id": &submission.submission_id,
        "message_id": &submission.message_id,
        "subject_id": &submission.subject_id,
        "mode": submission.mode,
        "content": &submission.content,
        "attachment_refs": &submission.attachment_refs,
        "rich_content_ref": &submission.rich_content_ref,
        "observed_step_id": &submission.observed_step_id,
        "observed_request_id": &submission.observed_request_id,
    })
    .to_string()
}

fn action_payload(action: &ActionSubmission) -> String {
    serde_json::json!({
        "schema_version": action.schema_version,
        "type": &action.kind,
        "submission_id": &action.submission_id,
        "subject_id": &action.subject_id,
        "document_id": &action.document_id,
        "action_id": &action.action_id,
        "revision": &action.revision,
        "nonce": &action.nonce,
        "payload": &action.payload,
    })
    .to_string()
}

pub struct WebInputService {
    store: Arc<Store>,
    subject_id: EntityId,
    inbox_tx: Option<Arc<tokio::sync::mpsc::Sender<InboxEvent>>>,
    presentation: Arc<PresentationService>,
}

impl WebInputService {
    pub fn new(
        store: Arc<Store>,
        subject_id: EntityId,
        inbox_tx: Option<Arc<tokio::sync::mpsc::Sender<InboxEvent>>>,
        presentation: Arc<PresentationService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            subject_id,
            inbox_tx,
            presentation,
        })
    }

    fn unavailable_receipt(submission_id: &str, mode: Option<SubmissionMode>) -> InputReceipt {
        InputReceipt {
            schema_version: 2,
            kind: "receipt",
            submission_id: submission_id.to_owned(),
            stage: SubmissionStage::Unavailable,
            mode,
            durability: None,
            event_id: None,
            accepted_commit: None,
            replayed: None,
            placement: None,
            detail: "持久接纳暂不可用；结果未知，同一 ID 可重查或重试。".to_owned(),
        }
    }

    fn validate_submission(&self, submission: &UserSubmission) -> Result<(), &'static str> {
        if submission.schema_version != 2 || submission.kind != "submit" {
            return Err("不支持的输入协议版本。");
        }
        if submission.submission_id.is_empty()
            || submission.message_id.is_empty()
            || submission.content.trim().is_empty()
        {
            return Err("输入身份或内容为空。");
        }
        if !submission.attachment_refs.is_empty() || submission.rich_content_ref.is_some() {
            return Err("附件引用解析尚未接入；未接纳输入。");
        }
        if self.subject_id.to_string() != submission.subject_id {
            return Err("真实 subject 绑定不匹配；未接纳输入。");
        }
        Ok(())
    }

    /// The request a steer binds to at admission: the sealed in-flight subject
    /// request, if any. This reads durable head state; it never blocks on the
    /// model and never invents a request.
    fn bound_request(&self) -> Option<EntityId> {
        let Ok((head, step)) = self.store.load_subject(self.subject_id) else {
            return None;
        };
        let _ = head;
        if step.active_request_id.is_some() {
            step.active_request_id
        } else {
            None
        }
    }

    /// Resolve placement from durable facts + the adapter capability contract.
    /// Without a verified in-request control capability the honest answer is
    /// always `next_boundary` (SPEC §21.12 UI02).
    fn placement(&self, mode: SubmissionMode) -> Placement {
        let target_request_id = match mode {
            SubmissionMode::Steer => self.bound_request().map(|id| id.to_string()),
            SubmissionMode::Append => None,
        };
        Placement {
            requested: mode,
            actual: PlacementActual::NextBoundary,
            target_request_id,
        }
    }

    /// Durable admission of one user text input (append or steer). No queue
    /// wait: once the original and admission commit, the receipt is available
    /// even while the subject is paused or its inbox is full.
    pub fn submit(&self, input: &UserSubmission) -> InputReceipt {
        let mut reply = Self::unavailable_receipt(&input.submission_id, Some(input.mode));
        if let Err(detail) = self.validate_submission(input) {
            reply.stage = SubmissionStage::Rejected;
            reply.detail = detail.to_owned();
            return reply;
        }
        let payload = submission_payload(input);
        let placement = self.placement(input.mode);
        let bound_request_id = placement.target_request_id.clone();
        let mut event_payload = serde_json::json!({
            "content": &input.content,
            "message_id": &input.message_id,
            "submission_id": &input.submission_id,
            "subject_id": &input.subject_id,
            "mode": match input.mode {
                SubmissionMode::Append => "append",
                SubmissionMode::Steer => "steer",
            },
            "attachment_refs": &input.attachment_refs,
            "observed_step_id": &input.observed_step_id,
            "observed_request_id": &input.observed_request_id,
            "role": "user",
        });
        if let Some(request_id) = bound_request_id {
            event_payload["bound_request_id"] = serde_json::Value::String(request_id);
        }
        let event = InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::Message,
            payload: event_payload,
            created_at: Utc::now(),
            source: WEB_INPUT_SOURCE.to_owned(),
        };
        let original = match serde_json::to_string(&event) {
            Ok(original) => original,
            Err(error) => {
                tracing::warn!(%error, "web input could not be serialized");
                return reply;
            }
        };
        let raw = RawInboxEvent {
            event_id: event.event_id,
            agent_id: self.subject_id,
            source: event.source.clone(),
            kind: "message".into(),
            payload_json: original,
            received_at_ms: event.created_at.timestamp_millis(),
        };
        let admission =
            match self
                .store
                .admit_web_submission(self.subject_id, &input.submission_id, &payload, &raw)
            {
                Ok(admission) => admission,
                Err(error) => {
                    tracing::warn!(%error, "durable input admission failed; reconciling original key");
                    // with_mutation may reject before the replay lookup when a
                    // writer has closed. Only a read of durable truth can
                    // establish presence or absence after a lost ACK.
                    match self.store.web_submission(self.subject_id, &input.submission_id) {
                        Ok(Some(record)) if record.payload == payload => WebAdmission::Replay(record),
                        Ok(Some(_)) => WebAdmission::Conflict,
                        Ok(None) => {
                            reply.stage = SubmissionStage::Rejected;
                            reply.detail =
                                "持久查询未发现此提交；原 ID 与内容可在恢复后重试。".to_owned();
                            return reply;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "durable admission outcome remains unavailable");
                            return reply;
                        }
                    }
                }
            };
        let (record, replayed) = match admission {
            WebAdmission::Admitted(record) => {
                // Only a wake/delivery hint: the normal loop also drains
                // persisted pending rows, so full/closed inboxes lose nothing.
                let delivered = self
                    .inbox_tx
                    .as_ref()
                    .is_some_and(|tx| tx.try_send(event).is_ok());
                reply.detail = if delivered {
                    "已持久接纳；已通知当前循环，选入与处置另查回执。".to_owned()
                } else {
                    "已持久接纳；等待后续步骤补送，重启亦不丢失。".to_owned()
                };
                (record, false)
            }
            WebAdmission::Replay(record) => {
                reply.detail = "返回原持久回执，未新增输入；选入与处置另查回执。".to_owned();
                (record, true)
            }
            WebAdmission::Conflict => {
                reply.stage = SubmissionStage::Conflict;
                reply.detail = "同一 submission_id 已关联不同内容；原输入未改写。".to_owned();
                return reply;
            }
        };
        reply.stage = SubmissionStage::Accepted;
        reply.durability = Some("durable");
        reply.event_id = Some(record.event_id.to_string());
        reply.accepted_commit = Some(record.accepted_commit.to_string());
        reply.replayed = Some(replayed);
        reply.placement = Some(placement);
        reply
    }

    /// Validate one UI action against its committed document binding and
    /// admit it as a durable input. Action authority comes from the subject's
    /// disposition of that input, never from the click itself (§21.8).
    pub fn submit_action(
        &self,
        action: &ActionSubmission,
        caps: &SurfaceCapabilities,
    ) -> InputReceipt {
        let mut reply = Self::unavailable_receipt(&action.submission_id, None);
        if action.schema_version != 2 || action.kind != "action" {
            reply.stage = SubmissionStage::Rejected;
            reply.detail = "不支持的动作协议版本。".to_owned();
            return reply;
        }
        if action.submission_id.is_empty()
            || action.document_id.is_empty()
            || action.action_id.is_empty()
            || action.revision.is_empty()
            || action.nonce.is_empty()
        {
            reply.stage = SubmissionStage::Rejected;
            reply.detail = "动作身份字段为空。".to_owned();
            return reply;
        }
        if self.subject_id.to_string() != action.subject_id {
            reply.stage = SubmissionStage::Rejected;
            reply.detail = "真实 subject 绑定不匹配；未接纳动作。".to_owned();
            return reply;
        }
        let Some(document) = self.presentation.document(&action.document_id, &action.revision)
        else {
            reply.stage = SubmissionStage::Rejected;
            reply.detail = "文档不存在或 revision 已过期；动作未接纳，请基于当前内容重试。".to_owned();
            return reply;
        };
        let binding = match validate_action(
            &document,
            &action.action_id,
            &action.revision,
            &action.nonce,
            &action.payload,
            Utc::now().timestamp(),
        ) {
            Ok(binding) => binding,
            Err(rejection) => {
                reply.stage = SubmissionStage::Rejected;
                reply.detail = match rejection {
                    ActionRejection::UnknownDocument => "文档不存在。".to_owned(),
                    ActionRejection::StaleRevision { current } => {
                        format!("动作 revision 已过期；当前为 {current}。")
                    }
                    ActionRejection::UnknownAction => "文档中不存在此动作。".to_owned(),
                    ActionRejection::NonceMismatch => "动作 nonce 不匹配；可能是过期界面。".to_owned(),
                    ActionRejection::Expired => "动作已过有效期；未接纳。".to_owned(),
                    ActionRejection::PayloadHashMismatch => {
                        "动作 payload 与绑定的语义 hash 不一致；未接纳。".to_owned()
                    }
                    ActionRejection::MissingCapability(capability) => {
                        format!("当前界面能力不足以执行此动作（{capability}）。")
                    }
                    ActionRejection::ActorScopeViolation => {
                        "当前会话不在此动作允许的 actor 范围内。".to_owned()
                    }
                };
                return reply;
            }
        };
        if let Some(scope) = &binding.actor_scope {
            if scope != "owner" {
                reply.stage = SubmissionStage::Rejected;
                reply.detail = "动作 actor 范围不被当前会话满足。".to_owned();
                return reply;
            }
        }
        for required in &binding.required_capabilities {
            if !capability_present(caps, required) {
                reply.stage = SubmissionStage::Rejected;
                reply.detail = format!("当前界面缺少所需能力：{required}。");
                return reply;
            }
        }
        let payload = action_payload(action);
        let event = InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::Message,
            payload: serde_json::json!({
                "role": "user",
                "action": {
                    "document_id": &action.document_id,
                    "action_id": &action.action_id,
                    "revision": &action.revision,
                    "nonce": &action.nonce,
                    "payload": &action.payload,
                },
                "submission_id": &action.submission_id,
                "subject_id": &action.subject_id,
            }),
            created_at: Utc::now(),
            source: WEB_INPUT_SOURCE.to_owned(),
        };
        let original = match serde_json::to_string(&event) {
            Ok(original) => original,
            Err(error) => {
                tracing::warn!(%error, "ui action could not be serialized");
                return reply;
            }
        };
        let raw = RawInboxEvent {
            event_id: event.event_id,
            agent_id: self.subject_id,
            source: event.source.clone(),
            kind: "ui_action".into(),
            payload_json: original,
            received_at_ms: event.created_at.timestamp_millis(),
        };
        let admission =
            match self
                .store
                .admit_web_submission(self.subject_id, &action.submission_id, &payload, &raw)
            {
                Ok(admission) => admission,
                Err(error) => {
                    tracing::warn!(%error, "ui action admission failed; reconciling original key");
                    match self.store.web_submission(self.subject_id, &action.submission_id) {
                        Ok(Some(record)) if record.payload == payload => WebAdmission::Replay(record),
                        Ok(Some(_)) => WebAdmission::Conflict,
                        Ok(None) => {
                            reply.stage = SubmissionStage::Rejected;
                            reply.detail = "持久查询未发现此动作；可重试。".to_owned();
                            return reply;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "ui action admission remains unavailable");
                            return reply;
                        }
                    }
                }
            };
        let (record, replayed) = match admission {
            WebAdmission::Admitted(record) => {
                let delivered = self
                    .inbox_tx
                    .as_ref()
                    .is_some_and(|tx| tx.try_send(event).is_ok());
                reply.detail = if delivered {
                    "动作已持久接纳；已通知当前循环，处置另查回执。".to_owned()
                } else {
                    "动作已持久接纳；等待后续步骤补送，重启亦不丢失。".to_owned()
                };
                (record, false)
            }
            WebAdmission::Replay(record) => {
                reply.detail = "返回原动作回执，未重复提交。".to_owned();
                (record, true)
            }
            WebAdmission::Conflict => {
                reply.stage = SubmissionStage::Conflict;
                reply.detail = "同一动作键已关联不同内容；原动作未改写。".to_owned();
                return reply;
            }
        };
        reply.stage = SubmissionStage::Accepted;
        reply.durability = Some("durable");
        reply.event_id = Some(record.event_id.to_string());
        reply.accepted_commit = Some(record.accepted_commit.to_string());
        reply.replayed = Some(replayed);
        reply
    }

    /// Withdraw one admitted submission (§41.18). The withdrawal is a durable
    /// control record in the same ledger; it never deletes admitted facts and
    /// never claims to undo effects the subject already produced.
    pub fn withdraw(&self, request: &WithdrawRequest) -> SubmissionView {
        let base = self.view_base(&request.submission_id, None);
        if request.schema_version != 2 || request.kind != "withdraw" {
            return SubmissionView {
                result: "invalid",
                detail_note: Some("不支持的撤回协议版本。"),
                ..base
            };
        }
        if request.submission_id.is_empty() || request.cancel_id.is_empty() {
            return SubmissionView {
                result: "invalid",
                detail_note: Some("撤回身份字段为空。"),
                ..base
            };
        }
        if self.subject_id.to_string() != request.subject_id {
            return SubmissionView {
                result: "not_admitted",
                ..base
            };
        }
        let result = self.store.submit_web_withdrawal(
            self.subject_id,
            &request.submission_id,
            &request.cancel_id,
        );
        match result {
            Ok(Ok(record)) => {
                let mut view = self.view_for(&request.submission_id, None);
                view.withdrawal = Some(WithdrawalView {
                    cancel_id: record.cancel_id,
                    commit_ref: record.commit.to_string(),
                    result: match record.result {
                        shenko_store::web_input::WebWithdrawResultKind::WithdrawnBeforePresentation => {
                            "withdrawn_before_presentation"
                        }
                        shenko_store::web_input::WebWithdrawResultKind::WithdrawalQueued => {
                            "withdrawal_queued"
                        }
                    },
                });
                view
            }
            Ok(Err(shenko_store::web_input::WebWithdrawRejection::NotAdmitted)) => {
                SubmissionView {
                    result: "not_admitted",
                    ..base
                }
            }
            Err(error) => {
                tracing::warn!(%error, "withdrawal admission failed");
                SubmissionView {
                    result: "temporarily_unavailable",
                    ..base
                }
            }
        }
    }

    fn view_base(&self, submission_id: &str, send_epoch: Option<u64>) -> SubmissionView {
        SubmissionView {
            schema_version: 2,
            kind: "submission_view",
            submission_id: submission_id.to_owned(),
            result: "temporarily_unavailable",
            stage: None,
            event_id: None,
            accepted_commit: None,
            selected_step_id: None,
            committed_at: None,
            placement: None,
            withdrawal: None,
            send_epoch,
            detail_note: None,
        }
    }

    fn view_for(&self, submission_id: &str, send_epoch: Option<u64>) -> SubmissionView {
        let mut view = self.view_base(submission_id, send_epoch);
        view.result = "found";
        let Some(record) = self
            .store
            .web_submission(self.subject_id, submission_id)
            .unwrap_or(None)
        else {
            view.result = "not_admitted";
            return view;
        };
        view.event_id = Some(record.event_id.to_string());
        view.accepted_commit = Some(record.accepted_commit.to_string());
        match self.store.event_disposition_commit(self.subject_id, record.event_id) {
            Ok(Some(commit)) => {
                view.stage = Some("committed");
                view.committed_at = Some(commit.to_string());
            }
            Ok(None) => match self.store.web_input_selected_step(self.subject_id, record.event_id) {
                Ok(Some(step_id)) => {
                    view.stage = Some("selected");
                    view.selected_step_id = Some(step_id.to_string());
                }
                Ok(None) => view.stage = Some("accepted"),
                Err(_) => {
                    view.result = "temporarily_unavailable";
                    view.stage = None;
                }
            },
            Err(_) => {
                view.result = "temporarily_unavailable";
                view.stage = None;
            }
        }
        if let Ok(Some(withdrawal)) = self.store.web_input_withdrawal(self.subject_id, submission_id) {
            view.withdrawal = Some(WithdrawalView {
                cancel_id: withdrawal.cancel_id,
                commit_ref: withdrawal.commit.to_string(),
                result: match withdrawal.result {
                    shenko_store::web_input::WebWithdrawResultKind::WithdrawnBeforePresentation => {
                        "withdrawn_before_presentation"
                    }
                    shenko_store::web_input::WebWithdrawResultKind::WithdrawalQueued => {
                        "withdrawal_queued"
                    }
                },
            });
        }
        view
    }

    /// Reconnect/replay query — §41.18 get_web_input.
    pub fn get(&self, submission_id: &str, subject_id: &str, send_epoch: Option<u64>) -> SubmissionView {
        if self.subject_id.to_string() != subject_id {
            return SubmissionView {
                result: "not_admitted",
                ..self.view_base(submission_id, send_epoch)
            };
        }
        match self.store.web_submission(self.subject_id, submission_id) {
            Ok(Some(_)) => self.view_for(submission_id, send_epoch),
            Ok(None) => SubmissionView {
                result: "not_admitted",
                ..self.view_base(submission_id, send_epoch)
            },
            Err(_) => self.view_base(submission_id, send_epoch),
        }
    }

    /// The placement reported for fresh submissions of this mode; used by the
    /// WS hello so the client can render the input contract honestly.
    pub fn advertised_placement(&self, mode: SubmissionMode) -> Placement {
        self.placement(mode)
    }
}
