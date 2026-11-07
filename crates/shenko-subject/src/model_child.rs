//! Real finite, durable, multi-step tool-using model children.
//!
//! SPEC §5.3 (委派与对话), §5.5 (主体—child 双向协作), §6 (child 的独立
//! system prompt / 模型 / 上下文 / 工具 / 输出预算), §14.5 (递归收敛：父预算
//! 账户、去重), §41.5 (有限子任务), R106/R107.
//!
//! A model child is NOT one generation with `tools=[]`. It is a bounded agent
//! run:
//!
//! * every model step is its own durable operation with its own T1/E1/T2 on
//!   the shared inference broker and the shared quota gate;
//! * a tool call runs a REAL registered native handler as a durable child
//!   subtask with its own operation, attempt, evidence and T2;
//! * the transcript, the step results, the tool results and the child's final
//!   answer are durable facts, so a restart continues the same run instead of
//!   starting a second one;
//! * the child's own system prompt, model, context, tool choice and output
//!   budget are authored by the delegator. Its per-step output budget is NOT
//!   the delegating step's single-turn cap (SPEC §6); only real transport
//!   facts and the child's own total budget bound it;
//! * the run is finite: a step budget, a tool-call budget and a total token
//!   budget. A child never becomes a second infinite primary loop.
//!
//! Tool calls use the product's own control-line protocol
//! (`[tool:kind:version] {json}`), because the shared provider wire does not
//! carry provider-native tool blocks yet. The protocol, the tool catalog and
//! the run's facts are the child's; the authority, the execution and the
//! receipts belong to the shared native runtime.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::Context;
use serde::{Deserialize, Serialize};
use shenko_context_policy::{ContributionPlan, ContributionRegistry, ContributionScope, ContributionUsage};
use shenko_broker::{
    broker::{BrokerHandle, DispatchFence, InferenceReceiptSink, InferenceRecoverySource},
    request::*,
};
use shenko_store::{
    Store, TaskInvocation,
    external::{
        InferenceSendBinding, MODEL_CHILD_KIND as STORE_MODEL_CHILD_KIND,
        MODEL_CHILD_RESULT_OPERATION, MODEL_CHILD_STEP_OPERATION,
    },
};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    output::{
        ModelChildBudget, ModelChildReplyGrant, ModelChildSpec, ModelChildTool, PublicationTarget,
    },
    subject::{BudgetHint, DelegationIntent, PublicationIntent, ReplyGrant},
    task::*,
};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

use crate::{
    ChildToolCall, ChildToolOutcome, ChildToolRuntime, NativeTaskDescriptor, NativeTaskHandler,
    NativeTaskInvocation,
};
use crate::quota_gate::{
    QuotaGate, observe_quota_response, quota_dispatch_gate, settle_quota_terminal,
};

/// The native definition kind of a model child task. The store admits the
/// very same kind, so the sealed contract and the ledger agree.
pub const MODEL_CHILD_KIND: &str = STORE_MODEL_CHILD_KIND;
const ROLE: &str = "model-child/tool-agent@2";
/// The frozen child contract version. Schema 2 replaced the single frozen
/// generation with a frozen RUN contract: the step payloads are derived from
/// it, one durable operation per step.
const CONTRACT_SCHEMA: u32 = 3;
/// The contract version that still derived the outbound account from the
/// target's audience kind. Sealed contracts of that version keep their own
/// derivation, so a historical run still recovers exactly as it was sealed.
const LEGACY_CONTRACT_SCHEMA: u32 = 2;
/// The frozen step input version.
const STEP_SCHEMA: u32 = 2;
/// Bounded text kept from one model result.
const MAX_STEP_TEXT_BYTES: usize = 64 * 1024;
/// Bounded tool result handed back to the model.
const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;
/// The whole transcript kept in one step payload.
const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;

const ROLE_CONTRACT: &str = "运行角色：有明确任务边界的有限模型 child，不是持续主体，也没有第二条无限主动循环。\n\
你可以分多步完成一个明确任务：每一步先判断还需要什么事实或真实效果，需要时用工具调用行请求一个已注册能力，拿到结果后继续下一步，最后给出结论。\n\
工具调用行格式：[tool:kind:版本] {json}；一次输出可以有多行工具调用，按出现顺序执行；一行只写一个 JSON 对象。\n\
工具结果会以 [tool_result:kind:版本] 开头的用户消息交回给你；工具失败、未注册、超出预算或结果未知时同样会给你一条事实消息，不要臆造成功，也不要无意义地重复同一次调用。\n\
可用工具目录（列出本轮已注册且可调用的工具；未列出不等于环境中不存在相应能力）：\n\
本轮没有可调用的工具时，请用你自己的知识与推理完成任务，并如实说明缺少哪些事实。\n\
你不能直接写文件、执行 shell、访问网络或对外发消息，除非它作为已注册工具出现在目录里。\n\
只有在你被明确授权外发时，才可以用 [reply] 正文发出一条消息；目标由授权固定，你不能选择或改投。\n\
每一步有独立的输出预算，整个运行还有总步数、总工具调用和总 token 预算；预算用尽时如实说明还差什么，不要假装完成。\n\
如果你缺少必须由 owner 决定的信息，用 [ask] 写下一个问题结束本轮；owner 的回答会作为同一讨论的下一条用户消息交回给你。\n\
你的输出是交回 owner 的结果正文，不是主体控制协议；不得声称未实际执行的工具或外部动作已经完成。\n\
以下是 owner 为你独立编写的完整语义 system prompt：";

/// One tool call the child asked for, parsed from its own output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChildToolCall {
    /// The child's stable call identity (derived, so a restart repeats it).
    pub call_id: String,
    pub kind: String,
    pub definition_version: u32,
    pub input: serde_json::Value,
}

/// One reply the child was granted to send, parsed from its own output. The
/// destination is NOT chosen by the child: the grant names the single target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChildReply {
    pub body: String,
}

/// The frozen child contract: the sealed RUN, not one generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenModelChild {
    schema_version: u32,
    task_id: EntityId,
    created_by_step_id: EntityId,
    spec: ModelChildSpec,
    provider_binding: ProviderBinding,
    software_generation: RuntimeGeneration,
    role_revision: String,
    /// The child's own finite budget.
    budget: ModelChildBudget,
    /// The child's per-step output budget.
    max_tokens: u64,
    /// The outbound authority bound to this child, when one was granted.
    reply_grant: Option<ReplyGrant>,
    /// The single destination that grant names. The child never chooses or
    /// narrows it; the grant owns it.
    reply_target: Option<PublicationTarget>,
    /// The earlier child run this one continues, when the owner answered a
    /// question instead of starting a new discussion.
    continued_from: Option<EntityId>,
    /// Digest of the authored semantic prompt.
    prompt_fingerprint: Digest,
    /// Digest of the authored tool choice.
    toolset_fingerprint: Digest,
}

impl FrozenModelChild {
    fn new(
        task_id: EntityId,
        step_id: EntityId,
        spec: ModelChildSpec,
        transport: &ProviderBinding,
        generation: RuntimeGeneration,
        continued_from: Option<EntityId>,
    ) -> Result<Self> {
        Self::build(
            task_id,
            step_id,
            spec,
            transport,
            generation,
            continued_from,
            CONTRACT_SCHEMA,
        )
    }

    fn build(
        task_id: EntityId,
        step_id: EntityId,
        spec: ModelChildSpec,
        transport: &ProviderBinding,
        generation: RuntimeGeneration,
        continued_from: Option<EntityId>,
        schema: u32,
    ) -> Result<Self> {
        ensure!(
            !spec.goal.trim().is_empty() && spec.goal.len() <= 64 * 1024,
            "invalid model child goal"
        );
        ensure!(
            !spec.system_prompt.trim().is_empty() && spec.system_prompt.len() <= 64 * 1024,
            "invalid child semantic prompt"
        );
        ensure!(
            !spec.model.trim().is_empty()
                && spec.model.len() <= 512
                && !spec.model.chars().any(char::is_control),
            "invalid child model"
        );
        ensure!(spec.max_tokens > 0, "invalid child output budget");
        for context in &spec.context {
            ensure!(
                !context.label.trim().is_empty()
                    && context.label.len() <= 512
                    && !context.text.trim().is_empty()
                    && context.text.len() <= 64 * 1024,
                "invalid child context block"
            );
        }
        for tool in &spec.tools {
            ensure!(
                !tool.kind.is_empty()
                    && tool.kind.len() <= 128
                    && tool
                        .kind
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                    && tool.definition_version > 0,
                "invalid child tool choice"
            );
        }
        if let Some(answer) = &spec.owner_answer {
            ensure!(
                !answer.trim().is_empty() && answer.len() <= 64 * 1024,
                "invalid owner answer"
            );
        }
        let budget = ModelChildBudget::resolve(&spec);
        ensure!(budget.is_finite(), "child budget is not finite");
        ensure!(
            budget.max_total_tokens >= spec.max_tokens,
            "child total budget is below its own per-step output budget"
        );
        // A transport default is not a maximum. Preserve the authored request;
        // the broker and provider admit it against actual resource facts.
        let provider_binding = transport.with_model(&spec.model)?;
        if let Some(grant) = spec.reply_grant.as_ref() {
            ensure!(
                schema == LEGACY_CONTRACT_SCHEMA || !grant.contradicts_target_account(),
                "reply grant account contradicts the account of its own publication target"
            );
        }
        let granted_account = spec.reply_grant.as_ref().map(|grant| match schema {
            LEGACY_CONTRACT_SCHEMA => grant
                .account
                .clone()
                .unwrap_or_else(|| grant.target.kind.clone()),
            _ => grant.account_namespace().to_string(),
        });
        let reply_grant = spec.reply_grant.as_ref().map(|grant| ReplyGrant {
            grant_id: derived_id(
                "shenko.model-child.reply-grant",
                &[task_id.to_string().as_str(), grant.target.kind.as_str()],
            ),
            task_id,
            target_provider: grant.target.channel.clone(),
            target_account: granted_account.clone().unwrap_or_default(),
            target_conversation: grant.target.id.to_string(),
            reply_to_event_id: grant.reply_to_event_id.unwrap_or_else(EntityId::nil),
            allowed_message_types: grant.allowed_message_types.clone(),
            expires_at: grant.expires_at,
        });
        let reply_target = spec.reply_grant.as_ref().map(|grant| grant.target.clone());
        let max_tokens = spec.max_tokens;
        let prompt_fingerprint = Digest::blake3(spec.system_prompt.as_bytes());
        let toolset_fingerprint = Digest::blake3(&serde_json::to_vec(&spec.tools)?);
        Ok(Self {
            schema_version: schema,
            task_id,
            created_by_step_id: step_id,
            spec,
            provider_binding,
            software_generation: generation,
            role_revision: ROLE.into(),
            budget,
            max_tokens,
            reply_grant,
            reply_target,
            continued_from,
            prompt_fingerprint,
            toolset_fingerprint,
        })
    }

    fn validate(&self) -> Result<()> {
        let expected = match self.schema_version {
            CONTRACT_SCHEMA => Self::new(
                self.task_id,
                self.created_by_step_id,
                self.spec.clone(),
                &self.provider_binding,
                self.software_generation.clone(),
                self.continued_from,
            )?,
            LEGACY_CONTRACT_SCHEMA => Self::build(
                self.task_id,
                self.created_by_step_id,
                self.spec.clone(),
                &self.provider_binding,
                self.software_generation.clone(),
                self.continued_from,
                LEGACY_CONTRACT_SCHEMA,
            )?,
            other => anyhow::bail!("unsupported model child contract version {other}"),
        };
        ensure!(self == &expected, "frozen model child contract mismatch");
        Ok(())
    }

    /// The tool kinds the child may call. An empty list means a text-only
    /// child: it has no tools at all.
    fn allowed_tools(&self) -> &[ModelChildTool] {
        &self.spec.tools
    }

    /// The token budget this run has left for further steps.
    fn remaining_tokens(&self, reserved: u64) -> u64 {
        self.budget
            .max_total_tokens
            .saturating_sub(reserved)
    }
}

/// The frozen input of ONE model step. It descends from the sealed child
/// contract and carries the exact payload this step sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenModelChildStep {
    schema_version: u32,
    contract_digest: Digest,
    step_index: u32,
    request_id: EntityId,
    provider_binding: ProviderBinding,
    max_tokens: u64,
    payload: InferencePayload,
    payload_fingerprint: Digest,
    /// Historical schema 1 had no contribution collection. Schema 2 always
    /// seals a plan, including an explicitly empty plan. A missing/null plan
    /// must never turn a schema-2 request into an unused contribution.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "read_contribution_plan")]
    contribution_plan: Option<ContributionPlan>,
}

fn read_contribution_plan<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<ContributionPlan>, D::Error> {
    // A plan sealed before the SPEC §51.9 cutover spells the sourced-content
    // kind `fact`. The sealed bytes are never rewritten; this reader migrates
    // its parsed copy through the one named migration.
    let mut document = serde_json::Value::deserialize(deserializer)?;
    shenko_types::plugin_runtime::migrate_legacy_contribution_kinds(&mut document);
    ContributionPlan::deserialize(document)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl FrozenModelChildStep {
    fn contribution_usage(&self, task_id: EntityId, seal: &ObjectRef) -> Result<Option<ContributionUsage>> {
        let Some(plan) = &self.contribution_plan else { return Ok(None); };
        let scope = ContributionScope { scope_id: task_id, task_id: Some(task_id) };
        ensure!(plan.missing_required.is_empty()
            && plan.contributions.iter().all(|item| item.scope == scope && item.tokens <= item.budget_tokens)
            && plan.contributions.iter().try_fold(0u64, |total, item| total.checked_add(item.tokens)) == Some(plan.total_tokens),
            "sealed child contribution scope/token evidence mismatch");
        if plan.contributions.is_empty() && plan.omitted.is_empty() { return Ok(None); }
        Ok(Some(plan.usage(self.request_id, u64::from(self.step_index), task_id.to_string(), seal.digest.clone())))
    }
}

/// The durable record of one finished model step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChildStepRecord {
    schema_version: u32,
    task_id: EntityId,
    step_index: u32,
    request_id: EntityId,
    provider_binding: ProviderBinding,
    stop_reason: String,
    text: String,
    text_truncated: bool,
    tool_calls: Vec<ModelChildToolCall>,
    reply: Option<ModelChildReply>,
    question: Option<String>,
    usage: Option<TokenUsage>,
    output_ref: Option<ObjectRef>,
    complete: bool,
}

/// Why a model child run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelChildTermination {
    /// The child produced a final answer.
    Completed,
    /// The child asked the owner a question and waits for the answer.
    NeedsDecision,
    /// The step budget was exhausted before a final answer.
    StepsExhausted,
    /// The total output budget was exhausted before a final answer.
    BudgetExhausted,
    /// The tool-call budget was exhausted before a final answer.
    ToolBudgetExhausted,
    /// A generation failed its existing terminal acceptance predicate.
    /// Preserve its partial text and E1 without authorizing effects.
    GenerationFailed,
}

/// Only the subject's admitted structured control reaches this constructor.
/// The binding is its sealed S0 transport, never mutable runtime environment.
pub(crate) fn prepare_model_delegation(
    task_id: EntityId,
    step_id: EntityId,
    spec: &ModelChildSpec,
    transport: &ProviderBinding,
    generation: &RuntimeGeneration,
) -> Result<DelegationIntent> {
    let frozen = FrozenModelChild::new(
        task_id,
        step_id,
        spec.clone(),
        transport,
        generation.clone(),
        spec.continue_task_id,
    )?;
    Ok(DelegationIntent {
        intent_id: task_id,
        goal: spec.goal.clone(),
        system_prompt: spec.system_prompt.clone(),
        model: Some(spec.model.clone()),
        capabilities: vec![],
        budget_hint: Some(BudgetHint {
            max_tokens: Some(spec.max_tokens),
            max_duration_secs: None,
        }),
        reply_grant: frozen.reply_grant.clone(),
        definition: Some(NativeTaskDefinition {
            kind: MODEL_CHILD_KIND.into(),
            definition_version: 1,
            input: serde_json::to_value(&frozen)?,
        }),
    })
}

/// A deterministic identity derived from stable facts. The same facts always
/// produce the same identity, so a restarted child reuses its own steps and
/// tool calls instead of creating a second run.
fn derived_id(namespace: &str, parts: &[&str]) -> EntityId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(namespace.as_bytes());
    for part in parts {
        hasher.update(&[0_u8]);
        hasher.update(part.as_bytes());
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    // Shape the derived bytes like a UUIDv5 so the id stays a normal id.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EntityId(Uuid::from_bytes(bytes))
}

/// The durable subtask identity of one tool call. It is derived from the
/// call identity alone, so a restarted run and a continued discussion read
/// the very same subtask instead of executing the tool twice.
fn tool_task_id(call_id: &str) -> EntityId {
    derived_id("shenko.model-child.tool", &[call_id])
}

fn plugin_id() -> EntityId {
    "de5bdc7a-ae54-45d5-948c-125f515f866d"
        .parse()
        .expect("fixed model child plugin UUID")
}

/// The delegation grammar the subject reads.
///
/// The outbound authority is described by the versioned publication target
/// itself: `schema_version` 2, an opaque string id and the account
/// namespace. An unversioned target or a numeric id cannot be bound to a
/// channel at all, and the audience kind is not an account.
fn delegation_grammar(profile_id: &str) -> String {
    format!(
        "有限模型 child 已注册（多步有界运行）：[delegate] {{\"goal\":\"…\",\"system_prompt\":\"你为该 child 独立写的完整语义 prompt\",\"model\":\"…\",\"max_tokens\":2048,\
 \"tools\":[{{\"kind\":\"<已注册定义 kind>\",\"definition_version\":1}}],\
 \"context\":[{{\"label\":\"…\",\"text\":\"…\"}}],\
 \"budget\":{{\"max_total_tokens\":8192,\"max_steps\":8,\"max_tool_calls\":8}},\
 \"reply_grant\":{{\"target\":{{\"schema_version\":2,\"channel\":\"…\",\"kind\":\"…\",\"id\":\"…\",\"account\":\"…\"}},\"allowed_message_types\":[\"text\"],\"expires_at\":\"…\"}},\
 \"continue_task_id\":\"<先前已完成的 child 任务 id>\",\"owner_answer\":\"<你对它提问的回答>\"}}。\
只有 goal/system_prompt/model/max_tokens 必填；tools 为空即纯文本 child；continue_task_id 与 owner_answer 必须同时出现（继续同一讨论）。\
reply_grant 是可选的外发授权：target 必须是已注册通道的版本化目标——schema_version 固定 2，id 是平台不透明字符串（不要写数字），account 是账号命名空间（未知时留空字符串）；kind 是受众类别，不是账号。\
child 每步一次真实 generation，可按 tools 调用**已注册**能力，工具结果回灌下一步；它自己的输出预算不受你本步上限限制，但受 transport 配置上限与它自己的 budget 限制。\
当前 provider 通路为 {profile_id}；model 名称不代表已验证可用。T1/E1/T2 与主体共用 broker/资源域。"
    )
}

fn contract_digest() -> Digest {
    // Recovery pins the executing code as well as its role contract.
    Digest::blake3(include_bytes!("model_child.rs"))
}

fn seal<T: Serialize + ?Sized>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    seal_value(store, &serde_json::to_value(value)?, schema, 1)
}

/// Seal an already canonical JSON value. The bytes are the canonical value
/// serialization, so the object's digest equals the digest the store computes
/// from the sealed delegation input.
fn seal_value(store: &Store, value: &serde_json::Value, schema: &str, version: u32) -> Result<ObjectRef> {
    match store.cas.seal_object(
        &serde_json::to_vec(value)?,
        "application/json",
        "model-child-effects",
        schema,
        version,
    ) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

fn read_frozen(store: &Store, object: &ObjectRef) -> Result<FrozenModelChild> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "model child input closure mismatch"
    );
    let frozen: FrozenModelChild = serde_json::from_slice(&bytes)?;
    frozen.validate()?;
    Ok(frozen)
}

fn read_step(store: &Store, object: &ObjectRef) -> Result<FrozenModelChildStep> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "model child step input closure mismatch"
    );
    let step: FrozenModelChildStep = serde_json::from_slice(&bytes)?;
    ensure!(
        matches!((step.schema_version, &step.contribution_plan), (1, None) | (STEP_SCHEMA, Some(_))),
        "unsupported model child step input"
    );
    Ok(step)
}

/// What one model output asked for, and the text that is left over.
struct ModelChildOutput {
    tool_calls: Vec<ModelChildToolCall>,
    reply: Option<ModelChildReply>,
    question: Option<String>,
    text: String,
}

/// The child's own control lines: tool calls, one granted reply and one
/// question to the owner. Every other line is the answer body.
fn parse_output(text: &str) -> ModelChildOutput {
    let mut calls: Vec<ModelChildToolCall> = Vec::new();
    let mut reply: Option<ModelChildReply> = None;
    let mut question: Option<String> = None;
    let mut rest = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(body) = trimmed.strip_prefix("[tool:") {
            if let Some((header, body)) = body.split_once(']') {
                if let Some((kind, version)) = header.rsplit_once(':') {
                    if let Ok(version) = version.trim().parse::<u32>() {
                        if version > 0 && !kind.trim().is_empty() {
                            if let Ok(input) = serde_json::from_str::<serde_json::Value>(body.trim())
                            {
                                calls.push(ModelChildToolCall {
                                    call_id: String::new(),
                                    kind: kind.trim().to_owned(),
                                    definition_version: version,
                                    input,
                                });
                                continue;
                            }
                        }
                    }
                }
            }
            rest.push_str(line);
            continue;
        }
        if let Some(body) = trimmed.strip_prefix("[reply]") {
            if !body.trim().is_empty() {
                reply = Some(ModelChildReply {
                    body: body.trim_start_matches(' ').to_owned(),
                });
                continue;
            }
            rest.push_str(line);
            continue;
        }
        if let Some(body) = trimmed.strip_prefix("[ask]") {
            if !body.trim().is_empty() {
                question = Some(body.trim().to_owned());
                continue;
            }
            rest.push_str(line);
            continue;
        }
        rest.push_str(line);
    }
    ModelChildOutput {
        tool_calls: calls,
        reply,
        question,
        text: rest,
    }
}

/// The durable history of one child run, rebuilt from the ledger.
#[derive(Default)]
struct ChildHistory {
    /// The finished steps, in order. A continued run starts with the
    /// transcript it inherited from the run it continues.
    records: Vec<ModelChildStepRecord>,
    /// How many leading records were inherited from an earlier run.
    inherited_count: usize,
    /// The own step operations of this run, by step index.
    operations: BTreeMap<u32, OperationRecord>,
    /// The number of steps THIS run has taken.
    own_steps: u32,
    /// The step index the run continues at.
    next_index: u32,
    /// Output tokens the finished steps reserved.
    reserved_tokens: u64,
    /// Output tokens the finished steps actually used, when the provider
    /// reported them.
    spent_tokens: u64,
    /// Tool calls the finished steps made.
    tool_calls: u32,
    /// A step whose outcome is not conclusive yet.
    pending: Option<OperationRecord>,
}

pub struct ModelChildHandler {
    store: Arc<Store>,
    broker: BrokerHandle,
    plugin_context: Context,
    contribution_registry: parking_lot::Mutex<Option<Arc<ContributionRegistry>>>,
}

impl ModelChildHandler {
    /// Restore debt BEFORE starting the broker or subject. Include cancelled
    /// tasks: cancellation is not provider termination, and no callback is
    /// needed to reconstruct an original armed/unknown request after restart.
    pub fn new(store: Arc<Store>, broker: BrokerHandle, plugin_context: Context) -> Result<Self> {
        let registry = plugin_context.try_service::<ContributionRegistry>().ok();
        let mut after = None;
        loop {
            let page = store.external_operations_after(MODEL_CHILD_STEP_OPERATION, after, 128)?;
            if page.is_empty() {
                break;
            }
            for operation in &page {
                let attempt = store.external_operation_attempt(operation.operation_id)?;
                let step = read_step(&store, &operation.final_input_ref)?;
                if let (Some(registry), Some(usage)) = (&registry, step.contribution_usage(operation.task_id, &operation.final_input_ref)?) {
                    registry.record_usage(usage)?;
                }
                broker.register_recovered(Arc::new(RecoveryReceipt {
                    store: Arc::clone(&store),
                    operation: operation.clone(),
                    attempt,
                    step,
                }))?;
                after = Some(operation.operation_id);
            }
        }
        Ok(Self { store, broker, plugin_context, contribution_registry: parking_lot::Mutex::new(registry) })
    }

    /// Resolve the live root service, not the registry that happened to exist
    /// when this long-running child was claimed. Replacement replays every
    /// accepted T1, including completed/cancelled tasks and older steps.
    fn refresh_contributions(&self) -> Result<Option<Arc<ContributionRegistry>>> {
        let mut previous = self.contribution_registry.lock();
        let current = self.plugin_context.try_service::<ContributionRegistry>().ok();
        if let Some(registry) = &current {
            if previous.as_ref().is_none_or(|old| !Arc::ptr_eq(old, registry)) {
                let mut after = None;
                loop {
                    let page = self.store.external_operations_after(MODEL_CHILD_STEP_OPERATION, after, 128)?;
                    if page.is_empty() { break; }
                    for operation in &page {
                        let step = read_step(&self.store, &operation.final_input_ref)?;
                        if let Some(usage) = step.contribution_usage(operation.task_id, &operation.final_input_ref)? {
                            registry.record_usage(usage)?;
                        }
                        after = Some(operation.operation_id);
                    }
                }
            }
        }
        *previous = current.clone();
        Ok(current)
    }

    /// The child's system prompt: the role contract, the authored semantic
    /// prompt, the installed tool catalog and — when the delegation granted
    /// one — the single reply target this child may use.
    fn system_prompt(
        contract: &FrozenModelChild,
        catalog: &[(String, u32, String)],
        cognition: Option<&str>,
    ) -> String {
        let mut prompt = String::from(ROLE_CONTRACT);
        prompt.push_str("\n\n");
        prompt.push_str(&contract.spec.system_prompt);
        prompt.push_str("\n\n");
        if catalog.is_empty() {
            prompt.push_str("可用工具目录：本轮没有任何已注册工具。\n");
        } else {
            prompt.push_str("可用工具目录：\n");
            for (kind, version, description) in catalog {
                prompt.push_str(&format!("- {kind} v{version}"));
                if !description.trim().is_empty() {
                    prompt.push_str(&format!("：{description}"));
                }
                prompt.push('\n');
            }
        }
        // Installed cognition is ONE directory: a finite child sees the same
        // bounded cards the main subject sees, and each card names the call
        // this child can actually make. The index is advisory material — it
        // grants nothing, and a card without a granted read call says so.
        if let Some(cognition) = cognition.filter(|index| !index.trim().is_empty()) {
            prompt.push_str("\n已安装的认知单元目录（感知／直觉／Skill／代码；与本主体共享同一安装）：\n");
            prompt.push_str(cognition);
        }
        if let Some(grant) = &contract.reply_grant {
            prompt.push_str(&format!(
                "\n你被授权向 {} 的 {} 直接发送一条消息：用 [reply] 正文。目标由授权固定，你不能选择或改投；授权过期后不再有效。\n",
                grant.target_provider,
                grant.target_conversation,
            ));
        }
        prompt
    }

    /// The installed cognition index as this child can actually address it.
    ///
    /// A finite child shares the installation the subject discloses, but it
    /// may only call the tools its own delegation granted. The index therefore
    /// names the child's own `[tool:…]` form for the grants that exist, and
    /// says plainly that the directory is all this child has when none does: a
    /// delegate-form call would only produce a refused step.
    fn cognition_index(&self, catalog: &[(String, u32, String)]) -> Option<String> {
        let cognition = match self
            .plugin_context
            .try_service::<shenko_awareness_core::CognitionCatalog>()
        {
            Ok(cognition) => cognition,
            Err(error) => {
                tracing::debug!(
                    %error,
                    "no published cognition catalog; this child is not shown the installed units"
                );
                return None;
            }
        };
        match cognition.render_index(cognition_grant(catalog)) {
            Ok(index) => Some(index),
            Err(error) => {
                tracing::warn!(%error, "installed cognition index unavailable for this child run");
                None
            }
        }
    }

    /// The first user message: the goal and the authored context blocks.
    fn initial_messages(contract: &FrozenModelChild) -> Vec<InferenceMessage> {
        let mut parts = vec![InferencePart::text(contract.spec.goal.clone())];
        for context in &contract.spec.context {
            parts.push(InferencePart::text(format!(
                "[{}]\n{}",
                context.label, context.text
            )));
        }
        vec![InferenceMessage {
            role: "user".into(),
            parts,
        }]
    }

    /// The transcript of the run: the opening message, then every finished
    /// step and the result of every tool call it made.
    fn transcript(
        contract: &FrozenModelChild,
        history: &ChildHistory,
        tool_results: &[Vec<(String, String, String)>],
    ) -> Vec<InferenceMessage> {
        let mut messages = Self::initial_messages(contract);
        for (index, record) in history.records.iter().enumerate() {
            if index == history.inherited_count {
                // The owner's answer belongs to the same discussion: it is
                // the next user turn after the inherited transcript.
                if let Some(answer) = &contract.spec.owner_answer {
                    messages.push(InferenceMessage::text(
                        "user",
                        format!("[owner 回答]\n{answer}"),
                    ));
                }
            }
            let mut assistant = record.text.clone();
            for call in &record.tool_calls {
                assistant.push_str(&format!(
                    "\n[tool:{}:{}] {}",
                    call.kind,
                    call.definition_version,
                    serde_json::to_string(&call.input).unwrap_or_default()
                ));
            }
            messages.push(InferenceMessage::text("assistant", assistant));
            for (kind, version, body) in tool_results.get(index).map(Vec::as_slice).unwrap_or(&[]) {
                messages.push(InferenceMessage::text(
                    "user",
                    format!("[tool_result:{kind}:{version}] {body}"),
                ));
            }
        }
        if history.inherited_count >= history.records.len() {
            if let Some(answer) = &contract.spec.owner_answer {
                messages.push(InferenceMessage::text(
                    "user",
                    format!("[owner 回答]\n{answer}"),
                ));
            }
        }
        // The transcript stays bounded: the oldest exchange is dropped first,
        // the opening goal is never dropped.
        let mut total: usize = messages
            .iter()
            .map(|message| {
                message
                    .parts
                    .iter()
                    .map(|part| match part {
                        InferencePart::Text { text } => text.len(),
                        InferencePart::Image { .. } => 0,
                        InferencePart::Document { .. }
                        | InferencePart::Citation { .. }
                        | InferencePart::SearchResult { .. } => 0,
                    })
                    .sum::<usize>()
            })
            .sum();
        let mut index = 1;
        while total > MAX_TRANSCRIPT_BYTES && index < messages.len() {
            total = total.saturating_sub(
                messages[index]
                    .parts
                    .iter()
                    .map(|part| match part {
                        InferencePart::Text { text } => text.len(),
                        InferencePart::Image { .. } => 0,
                        InferencePart::Document { .. }
                        | InferencePart::Citation { .. }
                        | InferencePart::SearchResult { .. } => 0,
                    })
                    .sum::<usize>(),
            );
            messages.remove(index);
        }
        messages
    }

    /// Rebuild the run's durable history from the ledger.
    fn history(&self, contract: &FrozenModelChild) -> Result<ChildHistory> {
        let mut history = ChildHistory::default();
        let operations = self
            .store
            .external_task_operations(contract.task_id, MODEL_CHILD_STEP_OPERATION)?;
        let mut steps = Vec::new();
        for operation in operations {
            let step = read_step(&self.store, &operation.final_input_ref)?;
            steps.push((operation, step));
        }
        steps.sort_by_key(|(_, step)| step.step_index);
        for (operation, step) in steps {
            ensure!(
                step.contract_digest == contract_digest_of(contract)?,
                "model child step does not descend from its sealed contract"
            );
            if operation.state != OperationState::Resolved {
                history.pending = Some(operation);
                return Ok(history);
            }
            let record = read_step_record(&self.store, &operation)?;
            history.reserved_tokens = history.reserved_tokens.saturating_add(step.max_tokens);
            history.spent_tokens = history
                .spent_tokens
                .saturating_add(
                    record
                        .usage
                        .as_ref()
                        .and_then(|usage| usage.output_tokens)
                        .unwrap_or(0),
                );
            history.tool_calls = history.tool_calls.saturating_add(record.tool_calls.len() as u32);
            history.next_index = step.step_index + 1;
            history.own_steps = history.own_steps.saturating_add(1);
            history.operations.insert(step.step_index, operation);
            history.records.push(record);
        }
        Ok(history)
    }

    /// The transcript a continued run inherits from the run it continues.
    ///
    /// The continued run's own result object holds its full transcript, so a
    /// continuation never re-executes a completed action and never starts a
    /// second discussion.
    fn inherited_transcript(
        &self,
        contract: &FrozenModelChild,
    ) -> Result<(Vec<ModelChildStepRecord>, ModelChildBudget, u64, u32)> {
        let Some(previous_id) = contract.continued_from else {
            return Ok((vec![], contract.budget.clone(), 0, 0));
        };
        let previous = self.store.external_task(previous_id)?;
        ensure!(
            previous.kind == MODEL_CHILD_KIND,
            "continued task is not a model child"
        );
        // The continued run's result operation is named by the task itself:
        // the task row carries no operation id, so the ledger is asked for
        // the task's own result operation.
        let operation = self
            .store
            .external_task_operations(previous_id, MODEL_CHILD_RESULT_OPERATION)?
            .into_iter()
            .next_back()
            .context("continued model child has no result operation")?;
        ensure!(
            operation.operation_name == MODEL_CHILD_RESULT_OPERATION
                && operation.state == OperationState::Resolved,
            "continued model child has no conclusive result"
        );
        let receipt_id = self
            .store
            .external_operation_attempt(operation.operation_id)?
            .final_receipt_id
            .context("continued model child result lacks receipt")?;
        let evidence = self.store.external_evidence(receipt_id)?;
        let normalized = evidence
            .normalized_ref
            .context("continued model child result has no normalized record")?;
        let bytes = self.store.cas.read_object(&normalized.digest)?;
        ensure!(
            bytes.len() as u64 == normalized.bytes && Digest::blake3(&bytes) == normalized.digest,
            "continued model child result closure mismatch"
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let records: Vec<ModelChildStepRecord> = serde_json::from_value(
            value
                .get("transcript")
                .cloned()
                .context("continued model child result has no transcript")?,
        )?;
        let budget: ModelChildBudget = serde_json::from_value(
            value
                .get("budget")
                .cloned()
                .context("continued model child result has no budget")?,
        )?;
        let reserved = value
            .get("reserved_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let tool_calls = value
            .get("tool_calls")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        Ok((records, budget, reserved, tool_calls))
    }

    // T1 and send-boundary commits advance the mutable record revision even
    // though this worker still owns the same invocation. Read that revision
    // before its next operation, never adopt a replacement task or authority.
    // The Store still rechecks this snapshot atomically at the next T1.
    fn refresh_invocation(&self, invocation: &mut NativeTaskInvocation) -> Result<()> {
        let current = self.store.external_task(invocation.task.task_id)?;
        let bound = &mut invocation.task_invocation;
        ensure!(current.task_revision == bound.task_revision
            && current.abort_request.is_none()
            && current.input_ref == invocation.task.input_ref
            && current.implementation_ref == invocation.task.implementation_ref
            && matches!(current.state, TaskState::Running { invocation_id, epoch, mode: RunMode::Run, .. }
                if invocation_id == bound.invocation_id && epoch == bound.epoch),
            "model child invocation superseded before next operation");
        bound.record_revision = current.record_revision;
        invocation.task.record_revision = current.record_revision;
        Ok(())
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let contract: FrozenModelChild = serde_json::from_value(invocation.input.clone())?;
        contract.validate()?;
        ensure!(
            contract.task_id == invocation.task.task_id
                && Some(contract.created_by_step_id) == invocation.task.created_by_step_id
                && contract.software_generation == invocation.context.software_generation
                && invocation.context.role_revision == ROLE,
            "model child task/role/generation mismatch"
        );
        let broker = self.broker.for_model(&contract.spec.model)?;
        ensure!(
            broker.binding() == &contract.provider_binding,
            "MODEL_PROVIDER_BINDING_MISMATCH: reconcile frozen child before changing transport"
        );
        let Some(tools) = invocation.child_tools.clone() else {
            anyhow::bail!("model child run has no shared native runtime");
        };
        let catalog = tools.describe_tools(contract.allowed_tools());
        // One projection per run: the installed units and the grants this
        // child actually received do not change between its steps.
        let cognition = self.cognition_index(&catalog);
        let mut history = self.history(&contract)?;
        let (inherited, previous_budget, previous_reserved, previous_tool_calls) =
            self.inherited_transcript(&contract)?;
        if contract.continued_from.is_some() {
            // A continuation may only spend what the run it continues has
            // left. Spawning never increases the total budget (SPEC §14.5).
            ensure!(
                contract.budget.max_total_tokens
                    <= previous_budget
                        .max_total_tokens
                        .saturating_sub(previous_reserved),
                "continued child exceeds the remaining budget of the run it continues"
            );
            ensure!(
                contract.budget.max_tool_calls
                    <= previous_budget.max_tool_calls.saturating_sub(previous_tool_calls),
                "continued child exceeds the remaining tool-call budget of the run it continues"
            );
            let own_next = history.next_index;
            let own = std::mem::take(&mut history.records);
            history.inherited_count = inherited.len();
            let inherited_next = inherited
                .last()
                .map(|record| record.step_index + 1)
                .unwrap_or(0);
            history.records = inherited;
            history.records.extend(own);
            history.next_index = own_next.max(inherited_next);
        }
        if let Some(pending) = history.pending {
            // The original step has no conclusive outcome. The child must not
            // send again and must not invent a result: it waits for the
            // original evidence and is parked as reconciling meanwhile.
            tracing::warn!(task_id=%contract.task_id, operation_id=%pending.operation_id, "model child step outcome is unresolved");
            return Ok(self.unresolved_claim(&invocation, contract.task_id));
        }
        // A worker may disappear after E1 but before the run result exists.
        // Recover the failed own step, not a fresh generation. Inherited
        // transcript belongs to an explicitly authored continuation instead.
        if let Some(failed) = history.records[history.inherited_count..]
            .iter().find(|record| !record.complete)
        {
            self.refresh_invocation(&mut invocation)?;
            return self.finish(&invocation, &contract, &history,
                ModelChildTermination::GenerationFailed, failed.text.clone(), vec![]);
        }
        let mut termination = ModelChildTermination::Completed;
        let mut publications: Vec<PublicationIntent> = vec![];
        let mut answer = String::new();
        loop {
            if history.own_steps >= contract.budget.max_steps {
                termination = ModelChildTermination::StepsExhausted;
                break;
            }
            if history
                .reserved_tokens
                .saturating_add(contract.max_tokens)
                > contract.budget.max_total_tokens
            {
                termination = ModelChildTermination::BudgetExhausted;
                break;
            }
            if history.tool_calls >= contract.budget.max_tool_calls {
                termination = ModelChildTermination::ToolBudgetExhausted;
                break;
            }
            let step_index = history.next_index;
            let tool_results = self.tool_results(&contract, &history)?;
            let payload = InferencePayload {
                system_prompt: Self::system_prompt(&contract, &catalog, cognition.as_deref()),
                messages: Self::transcript(&contract, &history, &tool_results),
                max_tokens: Some(contract.max_tokens),
                // The shared provider wire does not carry provider-native tool
                // blocks yet; the child's tool protocol is textual.
                tools: vec![],
                native_options: Default::default(),
            };
            self.refresh_invocation(&mut invocation)?;
            let step = self
                .run_step(&invocation, &contract, &broker, &history, step_index, payload)
                .await?;
            let record = step.record;
            answer = record.text.clone();
            let conclusive = record.complete;
            let calls = record.tool_calls.clone();
            let question = record.question.clone();
            history.reserved_tokens = history.reserved_tokens.saturating_add(contract.max_tokens);
            history.spent_tokens = history.spent_tokens.saturating_add(
                record
                    .usage
                    .as_ref()
                    .and_then(|usage| usage.output_tokens)
                    .unwrap_or(0),
            );
            history.tool_calls = history.tool_calls.saturating_add(calls.len() as u32);
            history.next_index = step_index + 1;
            history.own_steps = history.own_steps.saturating_add(1);
            history.operations.insert(step_index, step.operation);
            history.records.push(record);
            if !conclusive {
                // Keep failed/partial output as evidence, never as authority
                // to publish a reply or dispatch tools.
                termination = ModelChildTermination::GenerationFailed;
                break;
            }
            if let Some(reply) = history.records.last().and_then(|record| record.reply.as_ref()) {
                if let Some(intent) = self.reply_intent(&contract, reply)? {
                    let decision_ref = invocation.task.input_ref.clone();
                    if let Some(admitted) =
                        self.store
                            .admit_child_publication(&invocation.task, &intent, &decision_ref)?
                    {
                        publications.push(admitted);
                    }
                }
            }
            if !calls.is_empty() {
                // Execute the requested tools as durable child subtasks. Their
                // results are durable facts read back into the next step.
                for call in calls.iter() {
                    self.refresh_invocation(&mut invocation)?;
                    let _ = self
                        .call_tool(&tools, &invocation, &contract, call)
                        .await?;
                }
                if question.is_some() {
                    termination = ModelChildTermination::NeedsDecision;
                    break;
                }
                continue;
            }
            if question.is_some() {
                termination = ModelChildTermination::NeedsDecision;
                break;
            }
            termination = ModelChildTermination::Completed;
            break;
        }
        self.refresh_invocation(&mut invocation)?;
        self.finish(&invocation, &contract, &history, termination, answer, publications)
    }

    /// The publication intent of one granted reply, when the grant still
    /// covers exactly this target. A reply without a matching grant is not
    /// published at all.
    fn reply_intent(
        &self,
        contract: &FrozenModelChild,
        reply: &ModelChildReply,
    ) -> Result<Option<PublicationIntent>> {
        let Some(grant) = &contract.reply_grant else {
            return Ok(None);
        };
        let Some(target) = &contract.reply_target else {
            return Ok(None);
        };
        if reply.body.trim().is_empty() || grant.expires_at <= chrono::Utc::now() {
            return Ok(None);
        }
        Ok(Some(PublicationIntent {
            intent_id: derived_id(
                "shenko.model-child.publication",
                &[contract.task_id.to_string().as_str(), &grant.grant_id.to_string()],
            ),
            // The grant names the single destination. The child never chooses
            // or narrows it.
            target: target.clone(),
            content: reply.body.clone(),
            reply_grant_id: Some(grant.grant_id),
        }))
    }

    /// The claim of a child whose original step is unresolved. It carries no
    /// outcome: the dispatcher parks the child as reconciling instead of
    /// treating the uncertainty as a terminal failure, and the original
    /// attempt stays the only authority on what happened.
    fn unresolved_claim(
        &self,
        invocation: &NativeTaskInvocation,
        task_id: EntityId,
    ) -> OperationRecord {
        let digest = contract_digest();
        OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id,
            logical_key: "model-child-result".into(),
            operation_name: MODEL_CHILD_RESULT_OPERATION.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: invocation.task.input_ref.clone(),
            input_digest: invocation.task.input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: plugin_id(),
                artifact_digest: digest.clone(),
                contract_digest: digest.clone(),
                receipt_reader_ref: VersionRef {
                    artifact: ArtifactRef {
                        digest: digest.clone(),
                        locator: "builtin:model-child-v2".into(),
                    },
                    symbol: MODEL_CHILD_RESULT_OPERATION.into(),
                    schema: SchemaRef {
                        name: "model-child-run-result".into(),
                        major: 1,
                        digest: digest.clone(),
                    },
                },
            },
            resource_bindings_ref: invocation.task.input_ref.clone(),
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: invocation.task.input_ref.clone(),
            retry_contract_ref: invocation.task.input_ref.clone(),
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
            evidence_through: None,
        }
    }

    /// One durable model step: arm T1, send once, record E1.
    async fn run_step(
        &self,
        invocation: &NativeTaskInvocation,
        contract: &FrozenModelChild,
        broker: &BrokerHandle,
        history: &ChildHistory,
        step_index: u32,
        mut payload: InferencePayload,
    ) -> Result<StepOutcome> {
        let logical_key = format!("model-child-step:{step_index}");
        // A recorded step is read, never sent again. The quota reservation of
        // a step belongs to that step alone.
        if let Some(operation) = history.operations.get(&step_index) {
            ensure!(
                operation.state == OperationState::Resolved,
                "model child step outcome is unresolved; reconcile the original attempt"
            );
            let record = read_step_record(&self.store, operation)?;
            return Ok(StepOutcome {
                record,
                operation: operation.clone(),
            });
        }
        let request_id = derived_id(
            "shenko.model-child.request",
            &[contract.task_id.to_string().as_str(), &step_index.to_string()],
        );
        // Preparation belongs to plugins. This path only reads prepared
        // material and seals it in this request before reserving quota, never in
        // the rolling transcript reconstructed from model/tool results.
        let registry = self.refresh_contributions()?;
        let scope = ContributionScope { scope_id: contract.task_id, task_id: Some(contract.task_id) };
        let collected = crate::context_projection::collect_contributions(&self.store, registry.as_deref(), &scope)?;
        let contribution_plan = match collected {
            Some(collected) => {
                if let Some(text) = collected.material {
                    payload.messages.push(InferenceMessage::text("user", text));
                }
                collected.plan
            }
            None => ContributionPlan::default(),
        };
        let sealed_contract_digest = contract_digest_of(contract)?;
        let step = FrozenModelChildStep {
            schema_version: STEP_SCHEMA,
            contract_digest: sealed_contract_digest,
            step_index,
            request_id,
            provider_binding: contract.provider_binding.clone(),
            max_tokens: contract.max_tokens,
            payload_fingerprint: Digest::blake3(&serde_json::to_vec(&payload)?),
            payload,
            contribution_plan: Some(contribution_plan),
        };
        let input_ref = seal_value(
            &self.store,
            &serde_json::to_value(&step)?,
            "model-child-step-input",
            STEP_SCHEMA,
        )?;
        let operation_id = derived_id(
            "shenko.model-child.step",
            &[contract.task_id.to_string().as_str(), &step_index.to_string()],
        );
        // The child decides with the same installed strategy set as its
        // parent: a finite child request is not a second policy source.
        let quota_strategies = crate::quota_strategies::installed(&self.plugin_context);
        let quota_send = match quota_dispatch_gate(
            &self.store,
            &contract.provider_binding,
            request_id,
            &tokio_util::sync::CancellationToken::new(),
            &quota_strategies,
            Some(&invocation.task_invocation),
            // A finite child's round is not a committed subject step: the
            // subject's measured useful work is a different unit and is not
            // projected onto the child's expectation.
            None,
        )
        .await?
        {
            QuotaGate::Ready(send) => send,
            QuotaGate::Cancelled => anyhow::bail!("model child cancelled before quota-bound T1"),
        };
        let send_binding = InferenceSendBinding {
            schema_version: 1,
            request_id,
            send_id: quota_send,
        };
        let transport_ref = seal(&self.store, &send_binding, "model-child-selected-send")?;
        let resources = ResourceBindings {
            provider: contract.provider_binding.profile_id().into(),
            account_identity: match &contract.provider_binding {
                ProviderBinding::Anthropic {
                    credential_fingerprint,
                    ..
                } => credential_fingerprint
                    .as_ref()
                    .map(|fingerprint| format!("credential:{fingerprint}"))
                    .unwrap_or_default(),
                _ => String::new(),
            },
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "finite_model_child_step".into(),
            extra: serde_json::json!({
                "provider_binding": contract.provider_binding,
                "request_id": request_id,
                "task_id": contract.task_id,
                "step_index": step_index,
                "selected_send_id": quota_send,
                "role_revision": ROLE,
                "max_tokens": contract.max_tokens,
                "prompt_fingerprint": contract.prompt_fingerprint,
                "toolset_fingerprint": contract.toolset_fingerprint,
                "payload_fingerprint": step.payload_fingerprint,
                "checkpoint_identity_verified": false,
            }),
        };
        let digest = contract_digest();
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: digest.clone(),
                locator: "builtin:model-child-v2".into(),
            },
            symbol: MODEL_CHILD_STEP_OPERATION.into(),
            schema: SchemaRef {
                name: "model-child-provider-result".into(),
                major: 1,
                digest: digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id,
            record_revision: RecordRevision(0),
            task_id: contract.task_id,
            logical_key,
            operation_name: MODEL_CHILD_STEP_OPERATION.into(),
            input_schema_version: STEP_SCHEMA,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: plugin_id(),
                artifact_digest: digest.clone(),
                contract_digest: digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: seal(&self.store, &resources, "model-child-resource-binding")?,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: seal(
                &self.store,
                "matching terminal generation with stop/end_turn/stop_sequence; not a semantic goal-success judgement",
                "model-child-acceptance",
            )?,
            retry_contract_ref: seal(
                &self.store,
                "one armed attempt per step; no automatic replay, including uncertain send; reconcile original evidence",
                "model-child-retry",
            )?,
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
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: operation.operation_id,
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
            transport_ref: Some(transport_ref),
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        if let Some(existing) = self.store.arm_external_inference_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
            &send_binding,
        )? {
            // The step was already armed. A repeated identity is never a
            // second send: read the original outcome instead.
            ensure!(
                existing.state == OperationState::Resolved,
                "model child step outcome is unresolved; reconcile the original attempt"
            );
            let record = read_step_record(&self.store, &existing)?;
            return Ok(StepOutcome {
                record,
                operation: existing,
            });
        }
        // CAS writes alone own no consumption. T1 has now accepted the
        // exact request. Re-resolve after admission as a service replacement
        // could have replayed history before this T1 became visible.
        if let Some(usage) = step.contribution_usage(contract.task_id, &input_ref)? {
            let current = self.refresh_contributions()?;
            if let Some(current) = current.as_ref().filter(|current| registry.as_ref().is_none_or(|old| !Arc::ptr_eq(old, current))) {
                current.record_usage(usage.clone())?;
            }
            if let Some(registry) = &registry { registry.record_usage(usage)?; }
        }
        let sink = Arc::new(ChildStepSink {
            store: Arc::clone(&self.store),
            invocation: invocation.task_invocation.clone(),
            contract: contract.clone(),
            step: step.clone(),
            input_ref,
            operation: operation.clone(),
            attempt,
            send_binding,
        });
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = InferenceRequest {
            request_id,
            agent_id: invocation.task.agent_id,
            conversation_id: invocation.task.conversation_id,
            provider_binding: contract.provider_binding.clone(),
            priority: InferencePriority::Normal,
            epoch: invocation.task_invocation.epoch,
            payload: step.payload.clone(),
            result_tx,
        };
        if let Err(error) = broker.submit_recorded(request, sink) {
            // A generic admission error is not authority to reinterpret an
            // already armed identity as safe to replay (it may be a duplicate).
            broker.recover_unknown(request_id);
            return Err(error);
        }
        let result = result_rx
            .await
            .context("model child step outcome not durably settled; reconcile original attempt")?;
        ensure!(
            result.request_id == request_id,
            "model child step result identity mismatch"
        );
        let operation = self.store.external_operation(operation.operation_id)?;
        ensure!(
            operation.state == OperationState::Resolved,
            "model child step outcome is unresolved; reconcile the original attempt"
        );
        let record = read_step_record(&self.store, &operation)?;
        Ok(StepOutcome { record, operation })
    }

    /// Execute one tool call as a durable child subtask.
    async fn call_tool(
        &self,
        tools: &Arc<dyn ChildToolRuntime>,
        invocation: &NativeTaskInvocation,
        contract: &FrozenModelChild,
        call: &ModelChildToolCall,
    ) -> Result<ChildToolOutcome> {
        let allowed = contract
            .allowed_tools()
            .iter()
            .any(|tool| tool.kind == call.kind && tool.definition_version == call.definition_version);
        if !allowed {
            return Ok(ChildToolOutcome {
                call_id: call.call_id.clone(),
                kind: call.kind.clone(),
                definition_version: call.definition_version,
                task_id: EntityId::nil(),
                operation_id: EntityId::nil(),
                outcome: "not_allowed".into(),
                result: Some(serde_json::json!({
                    "reason": "this tool is not part of the child's granted tool choice",
                    "kind": call.kind,
                })),
                evidence_ref: None,
            });
        }
        let tool_task_id = tool_task_id(&call.call_id);
        if call.kind == MODEL_CHILD_KIND {
            return self.call_nested_child(invocation, contract, call, tool_task_id).await;
        }
        let request = ChildToolCall {
            call_id: call.call_id.clone(),
            task_id: tool_task_id,
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            input: call.input.clone(),
        };
        tools
            .call_tool(&request, &invocation.task, &invocation.authority)
            .await
    }

    /// Run a nested finite model child as a durable subtask of this child.
    ///
    /// The nested child is a real child run with its own contract, its own
    /// steps and its own transcript. It may only spend what this child has
    /// left: spawning never increases the total budget (SPEC §14.5). The
    /// nested child has no reply grant of its own — the outer child owns the
    /// outbound authority it was granted.
    async fn call_nested_child(
        &self,
        invocation: &NativeTaskInvocation,
        contract: &FrozenModelChild,
        call: &ModelChildToolCall,
        tool_task_id: EntityId,
    ) -> Result<ChildToolOutcome> {
        let unavailable = |reason: &str, detail: serde_json::Value| ChildToolOutcome {
            call_id: call.call_id.clone(),
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            task_id: tool_task_id,
            operation_id: EntityId::nil(),
            outcome: reason.into(),
            result: Some(detail),
            evidence_ref: None,
        };
        let nested_spec: ModelChildSpec = match serde_json::from_value(call.input.clone()) {
            Ok(spec) => spec,
            Err(error) => {
                return Ok(unavailable(
                    "failed",
                    serde_json::json!({
                        "reason": format!("nested model child spec is invalid: {error}"),
                    }),
                ));
            }
        };
        // The nested child has no outbound authority of its own: the outer
        // child owns the reply grant it was given, and one logical reply has
        // exactly one responsible publisher.
        let mut nested_spec = nested_spec;
        nested_spec.reply_grant = None;
        nested_spec.continue_task_id = None;
        nested_spec.owner_answer = None;
        let nested_budget = ModelChildBudget::resolve(&nested_spec);
        if !nested_budget.is_finite() {
            return Ok(unavailable(
                "failed",
                serde_json::json!({"reason": "nested model child budget is not finite"}),
            ));
        }
        let reserved = self
            .store
            .external_task_operations(contract.task_id, MODEL_CHILD_STEP_OPERATION)?
            .iter()
            .filter_map(|operation| read_step(&self.store, &operation.final_input_ref).ok())
            .map(|step| step.max_tokens)
            .sum::<u64>();
        if nested_budget.max_total_tokens > contract.remaining_tokens(reserved) {
            return Ok(unavailable(
                "failed",
                serde_json::json!({
                    "reason": "nested model child exceeds the remaining budget of its parent",
                    "remaining_tokens": contract.remaining_tokens(reserved),
                    "requested_tokens": nested_budget.max_total_tokens,
                }),
            ));
        }
        let nested = match FrozenModelChild::new(
            tool_task_id,
            contract.created_by_step_id,
            nested_spec,
            &contract.provider_binding,
            contract.software_generation.clone(),
            None,
        ) {
            Ok(nested) => nested,
            Err(error) => {
                return Ok(unavailable(
                    "failed",
                    serde_json::json!({"reason": format!("nested model child contract rejected: {error}")}),
                ));
            }
        };
        let input_ref = seal_value(
            &self.store,
            &serde_json::to_value(&nested)?,
            "model-child-frozen-input",
            1,
        )?;
        let Some(claim) = self.store.claim_child_tool_task(
            &invocation.task,
            tool_task_id,
            MODEL_CHILD_KIND,
            1,
            input_ref,
            ImplementationRef {
                plugin_id: plugin_id(),
                artifact_digest: contract_digest(),
                registry_revision: 0,
            },
        )?
        else {
            // The nested run already exists durably. Its result is read
            // instead of running the nested child a second time.
            return Ok(self.nested_outcome(call, tool_task_id));
        };
        let context = shenko_types::agent::CallContext {
            caller_instance_id: self.store.capabilities.incarnation,
            role_revision: ROLE.into(),
            task_id: Some(tool_task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: vec!["model.inference.finite-tool-agent/v2".into()],
            trace_id: EntityId::new(),
        };
        let nested_invocation = NativeTaskInvocation {
            context,
            task_invocation: claim.invocation.clone(),
            task: claim.task.clone(),
            operation_id: claim.operation_id,
            authority: invocation.authority.clone(),
            input: serde_json::to_value(&nested)?,
            child_tools: invocation.child_tools.clone(),
        };
        // A nested child runs the same finite contract, so the recursive
        // call is boxed: the future's own size cannot be computed otherwise.
        let operation = match Box::pin(self.run(nested_invocation)).await {
            Ok(operation) => operation,
            Err(error) => {
                self.store.park_native_task(
                    claim.task.task_id,
                    claim.task.record_revision,
                    "handler_failed",
                    &format!("nested model child did not produce a settled result: {error}"),
                    false,
                )?;
                return Ok(unavailable(
                    "failed",
                    serde_json::json!({
                        "reason": format!("nested model child failed: {error}"),
                    }),
                ));
            }
        };
        let settled = self.store.complete_native_task(&claim, &operation)?;
        Ok(ChildToolOutcome {
            call_id: call.call_id.clone(),
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            task_id: tool_task_id,
            operation_id: operation.operation_id,
            outcome: match operation.outcome {
                Some(OperationOutcome::Succeeded) => "succeeded",
                Some(OperationOutcome::Cancelled) => "cancelled",
                Some(OperationOutcome::Failed) | Some(OperationOutcome::Interrupted) => "failed",
                None => "unknown",
            }
            .into(),
            result: settled.as_ref().and_then(|result| result.result.clone()),
            evidence_ref: settled.as_ref().map(|result| result.evidence_ref.clone()),
        })
    }

    /// The durable outcome of an already existing nested child run.
    fn nested_outcome(
        &self,
        call: &ModelChildToolCall,
        tool_task_id: EntityId,
    ) -> ChildToolOutcome {
        let result = self.store.native_task_result(tool_task_id).ok().flatten();
        let outcome = match &result {
            Some(result) => match result.outcome {
                TaskOutcome::Succeeded => "succeeded",
                TaskOutcome::Cancelled => "cancelled",
                TaskOutcome::Failed { .. } => "failed",
                TaskOutcome::Interrupted => "unknown",
            },
            None => "unknown",
        };
        ChildToolOutcome {
            call_id: call.call_id.clone(),
            kind: call.kind.clone(),
            definition_version: call.definition_version,
            task_id: tool_task_id,
            operation_id: result
                .as_ref()
                .map(|result| result.operation_id)
                .unwrap_or_else(EntityId::nil),
            outcome: outcome.into(),
            result: result.as_ref().and_then(|result| result.result.clone()),
            evidence_ref: result.map(|result| result.evidence_ref),
        }
    }

    /// The tool results of the finished steps, read back from the durable
    /// tool subtasks. They are facts, not model text.
    fn tool_results(
        &self,
        contract: &FrozenModelChild,
        history: &ChildHistory,
    ) -> Result<Vec<Vec<(String, String, String)>>> {
        let mut results = vec![];
        for record in history.records.iter() {
            let mut step_results = vec![];
            for call in record.tool_calls.iter() {
                let subtask = tool_task_id(&call.call_id);
                let outcome = self.store.native_task_result(subtask)?;
                let body = match outcome {
                    Some(result) => {
                        let payload = result
                            .result
                            .as_ref()
                            .map(|value| value.to_string())
                            .unwrap_or_default();
                        format!("{} {}", task_outcome_name(&result.outcome), payload)
                    }
                    None => "unknown: the tool subtask has no durable result yet".to_string(),
                };
                let body = if body.len() > MAX_TOOL_RESULT_BYTES {
                    let mut end = MAX_TOOL_RESULT_BYTES;
                    while !body.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}…[truncated]", &body[..end])
                } else {
                    body
                };
                step_results.push((
                    call.kind.clone(),
                    call.definition_version.to_string(),
                    body,
                ));
            }
            results.push(step_results);
        }
        let _ = contract;
        Ok(results)
    }

    /// Commit the child's own run operation and its result evidence. This is
    /// the T2 of the child: the durable result the subject consumes.
    fn finish(
        &self,
        invocation: &NativeTaskInvocation,
        contract: &FrozenModelChild,
        history: &ChildHistory,
        termination: ModelChildTermination,
        answer: String,
        publications: Vec<PublicationIntent>,
    ) -> Result<OperationRecord> {
        let contract_ref = seal_value(
            &self.store,
            &serde_json::to_value(contract)?,
            "model-child-frozen-input",
            1,
        )?;
        let result = serde_json::json!({
            "schema_version": 1,
            "task_id": contract.task_id,
            "termination": termination,
            "answer": answer,
            "model": contract.spec.model,
            "provider_binding": contract.provider_binding,
            "steps": history.records.len(),
            "tool_calls": history.tool_calls,
            "budget": {
                "max_total_tokens": contract.budget.max_total_tokens,
                "max_steps": contract.budget.max_steps,
                "max_tool_calls": contract.budget.max_tool_calls,
            },
            "reserved_tokens": history.reserved_tokens,
            "spent_tokens": history.spent_tokens,
            "prompt_fingerprint": contract.prompt_fingerprint,
            "toolset_fingerprint": contract.toolset_fingerprint,
            "continued_from": contract.continued_from,
            "step_records": history.records.iter().map(|record| serde_json::json!({
                "step_index": record.step_index,
                "request_id": record.request_id,
                "stop_reason": record.stop_reason,
                "text_bytes": record.text.len(),
                "tool_calls": record.tool_calls.len(),
                "question": record.question,
            })).collect::<Vec<_>>(),
            "transcript": history.records.clone(),
            "publications": publications,
        });
        let result_ref = seal_value(&self.store, &result, "model-child-result", 1)?;
        let digest = contract_digest();
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: digest.clone(),
                locator: "builtin:model-child-v2".into(),
            },
            symbol: MODEL_CHILD_RESULT_OPERATION.into(),
            schema: SchemaRef {
                name: "model-child-run-result".into(),
                major: 1,
                digest: digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: contract.task_id,
            logical_key: "model-child-result".into(),
            operation_name: MODEL_CHILD_RESULT_OPERATION.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: contract_ref.clone(),
            input_digest: contract_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: plugin_id(),
                artifact_digest: digest.clone(),
                contract_digest: digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: contract_ref.clone(),
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: seal(
                &self.store,
                "the child run reached a conclusive termination with its durable transcript",
                "model-child-acceptance",
            )?,
            retry_contract_ref: seal(
                &self.store,
                "the child run result is written once; a restart re-reads the original steps",
                "model-child-retry",
            )?,
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
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: operation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version.clone(),
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: Some(contract_ref),
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        let armed = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?;
        let operation = match armed {
            Some(existing) => existing,
            None => operation,
        };
        let attempt = self.store.external_operation_attempt(operation.operation_id)?;
        let outcome = match termination {
            // A completed run and a run that ended with a question for the
            // owner both passed the generation acceptance predicate. Failed
            // generations and exhausted budgets remain failed runs.
            ModelChildTermination::Completed | ModelChildTermination::NeedsDecision => {
                OperationOutcome::Succeeded
            }
            _ => OperationOutcome::Failed,
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: MODEL_CHILD_RESULT_OPERATION.into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: Some(format!("model-child-run:{}", contract.task_id)),
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: result_ref.clone(),
            decoder_ref: Some(version),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(result_ref),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            EffectStatus::Complete,
        )
    }
}

struct StepOutcome {
    record: ModelChildStepRecord,
    operation: OperationRecord,
}

fn task_outcome_name(outcome: &TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Succeeded => "succeeded",
        TaskOutcome::Failed { .. } => "failed",
        TaskOutcome::Cancelled => "cancelled",
        TaskOutcome::Interrupted => "interrupted",
    }
}

/// The digest of the sealed child contract. The store computes the very same
/// value from the delegation intent it admitted.
fn contract_digest_of(contract: &FrozenModelChild) -> Result<Digest> {
    let value = serde_json::to_value(contract)?;
    Ok(Digest::blake3(&serde_json::to_vec(&value)?))
}

/// Read the durable record of a finished step.
fn read_step_record(store: &Store, operation: &OperationRecord) -> Result<ModelChildStepRecord> {
    ensure!(
        operation.state == OperationState::Resolved,
        "model child step has no conclusive outcome"
    );
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    let receipt_id = attempt
        .final_receipt_id
        .context("ended model child step lacks receipt")?;
    let evidence = store.external_evidence(receipt_id)?;
    ensure!(
        evidence.operation_id == Some(operation.operation_id)
            && evidence.attempt_id == Some(attempt.attempt_id),
        "model child step receipt identity mismatch"
    );
    let normalized = evidence
        .normalized_ref
        .context("model child step receipt has no normalized record")?;
    let bytes = store.cas.read_object(&normalized.digest)?;
    ensure!(
        bytes.len() as u64 == normalized.bytes && Digest::blake3(&bytes) == normalized.digest,
        "model child step record closure mismatch"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

/// The receipt sink of ONE model step. It records the provider result as E1
/// on the step's own operation, exactly like the subject's own sink does.
struct ChildStepSink {
    store: Arc<Store>,
    invocation: TaskInvocation,
    contract: FrozenModelChild,
    step: FrozenModelChildStep,
    input_ref: ObjectRef,
    operation: OperationRecord,
    attempt: AttemptRecord,
    send_binding: InferenceSendBinding,
}

impl InferenceReceiptSink for ChildStepSink {
    fn may_dispatch(&self) -> Result<DispatchFence> {
        // The durable send-boundary transition: serialized with cancellation
        // and authority changes inside one immediate transaction. A cancelled
        // task is fenced before any send; a committed dispatch is reported as
        // unknown so the original attempt stays recoverable and is never
        // re-sent or marked never-dispatched.
        Ok(match self.store.commit_native_dispatch(
            &self.invocation, self.operation.operation_id, self.attempt.attempt_id,
        )? {
            shenko_store::child::NativeDispatchTransition::Won => DispatchFence::Send,
            shenko_store::child::NativeDispatchTransition::Fenced => DispatchFence::Fenced,
            shenko_store::child::NativeDispatchTransition::AlreadyCommitted => {
                DispatchFence::Unknown
            }
        })
    }

    fn record(&self, result: &InferenceResult) -> Result<()> {
        ensure!(
            result.request_id == self.step.request_id
                && result
                    .effective
                    .matches_seal(&self.step.provider_binding, &self.step.payload),
            "model child step result differs from frozen request"
        );
        let complete = result.status == InferenceStatus::Success
            && result.termination.kind == TerminationKind::ProviderTerminal
            && result.response.as_ref().is_some_and(|response| {
                matches!(
                    response.stop_reason.as_str(),
                    "stop" | "end_turn" | "stop_sequence"
                )
            });
        let (outcome, effect) = match result.termination.kind {
            TerminationKind::ProviderTerminal => (
                Some(if complete {
                    OperationOutcome::Succeeded
                } else {
                    OperationOutcome::Failed
                }),
                EffectStatus::Complete,
            ),
            TerminationKind::NeverDispatched => (
                Some(if result.status == InferenceStatus::Cancelled {
                    OperationOutcome::Cancelled
                } else {
                    OperationOutcome::Failed
                }),
                EffectStatus::None,
            ),
            TerminationKind::TransportRejected => (Some(OperationOutcome::Failed), EffectStatus::None),
            TerminationKind::Unknown => (None, EffectStatus::Unknown),
        };
        let output_ref = result.response.as_ref().map(|response| {
            self.store
                .cas
                .seal_object(
                    response.content.as_bytes(),
                    "text/plain",
                    "model-child-effects",
                    "model-child-text",
                    1,
                )
                .map(|sealed| sealed.object)
        })
        .transpose()
        .map_err(|error| {
            self.store.close_external_writer();
            error
        })?;
        let text = result.response.as_ref().map(|response| {
            let end = response
                .content
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index > MAX_STEP_TEXT_BYTES)
                .unwrap_or(response.content.len());
            &response.content[..end]
        });
        let (tool_calls, reply, question, answer_text) = match text {
            Some(text) => {
                let parsed = parse_output(text);
                let mut calls = parsed.tool_calls;
                for (index, call) in calls.iter_mut().enumerate() {
                    let input_digest = Digest::blake3(&serde_json::to_vec(&call.input)?);
                    call.call_id = format!(
                        "{}:step{}.call{index}.{}",
                        self.contract.task_id,
                        self.step.step_index,
                        &input_digest.hex()[..16]
                    );
                }
                (calls, parsed.reply, parsed.question, parsed.text)
            }
            None => (vec![], None, None, String::new()),
        };
        let record = ModelChildStepRecord {
            schema_version: 1,
            task_id: self.contract.task_id,
            step_index: self.step.step_index,
            request_id: result.request_id,
            provider_binding: self.step.provider_binding.clone(),
            stop_reason: result
                .response
                .as_ref()
                .map(|response| response.stop_reason.clone())
                .unwrap_or_default(),
            text: answer_text,
            text_truncated: result.response.as_ref().is_some_and(|response| {
                text.is_some_and(|text| text.len() != response.content.len())
            }),
            tool_calls,
            reply,
            question,
            usage: result.response.as_ref().and_then(|response| response.usage.clone()),
            output_ref,
            complete,
        };
        let raw = seal(&self.store, result, "model-child-provider-result")?;
        let normalized = seal_value(
            &self.store,
            &serde_json::to_value(&record)?,
            "model-child-step-record",
            1,
        )?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(self.operation.operation_id),
            attempt_id: Some(self.attempt.attempt_id),
            source_id: MODEL_CHILD_STEP_OPERATION.into(),
            source_instance_id: self.invocation.node_id.to_string(),
            source_event_key: Some(format!("model-request:{}", result.request_id)),
            producer_epoch: Some(self.invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(self.operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: (result.termination.kind == TerminationKind::ProviderTerminal)
                .then_some(ObservationLevel::RemoteCompleted),
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: self.operation.prepared_commit,
        };
        // E1 intentionally has no task commit authority. Cancellation, epoch
        // changes and worker teardown cannot discard this original fact.
        self.store.record_external_evidence(
            self.operation.operation_id,
            self.attempt.attempt_id,
            evidence,
            outcome,
            effect,
        )?;
        if let Some(send_id) = self.send_binding.send_id {
            observe_quota_response(
                &self.store,
                &self.step.provider_binding,
                result,
                self.step.request_id,
                send_id,
            );
            settle_quota_terminal(&self.store, result, self.step.request_id, send_id);
        }
        Ok(())
    }
}

impl InferenceRecoverySource for ChildStepSink {
    fn request_id(&self) -> EntityId {
        self.step.request_id
    }
    fn binding(&self) -> &ProviderBinding {
        &self.step.provider_binding
    }
    fn payload(&self) -> &InferencePayload {
        &self.step.payload
    }
    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(
            &self.store,
            &self.operation,
            &self.attempt,
            &self.step,
        )
    }
}

#[async_trait]
impl NativeTaskHandler for ModelChildHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: MODEL_CHILD_KIND.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(),
                artifact_digest: contract_digest(),
                registry_revision: 0,
            },
            role_revision: ROLE.into(),
            authority_refs: vec!["model.inference.finite-tool-agent/v2".into()],
            prompt_section: Some(delegation_grammar(
                self.broker.binding().profile_id(),
            )),
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
                && operation.operation_name == MODEL_CHILD_RESULT_OPERATION,
            "original model child operation mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        // The run operation exists, so the run had reached its end. Rebuild
        // the result from the durable steps and record the missing E1.
        let contract = read_frozen(&self.store, &operation.final_input_ref)?;
        let history = self.history(&contract)?;
        ensure!(
            history.pending.is_none(),
            "model child run has an unresolved step; reconcile the original step first"
        );
        let answer = history
            .records
            .last()
            .map(|record| record.text.clone())
            .unwrap_or_default();
        let termination = if history
            .records
            .iter()
            .any(|record| !record.complete)
        {
            ModelChildTermination::GenerationFailed
        } else if history
            .records
            .last()
            .map(|record| record.tool_calls.is_empty())
            .unwrap_or(false)
        {
            ModelChildTermination::Completed
        } else {
            ModelChildTermination::StepsExhausted
        };
        self.finish(
            &NativeTaskInvocation {
                context: shenko_types::agent::CallContext {
                    caller_instance_id: self.store.capabilities.incarnation,
                    role_revision: ROLE.into(),
                    task_id: Some(task.task_id),
                    expected_task_revision: Some(task.task_revision.0),
                    invocation_id: None,
                    controller_epoch: ControllerEpoch(0),
                    software_generation: contract.software_generation.clone(),
                    authority_refs: vec![],
                    trace_id: EntityId::new(),
                },
                task_invocation: TaskInvocation {
                    task_id: task.task_id,
                    task_revision: task.task_revision,
                    record_revision: task.record_revision,
                    invocation_id: EntityId::nil(),
                    epoch: ControllerEpoch(0),
                    software_generation: contract.software_generation.clone(),
                    worker_id: self.store.capabilities.incarnation,
                    node_id: self.store.ledger.ledger_id(),
                },
                task: task.clone(),
                operation_id,
                authority: operation.authority_ref.clone(),
                input: serde_json::to_value(&contract)?,
                child_tools: None,
            },
            &contract,
            &history,
            termination,
            answer,
            vec![],
        )
    }
}

struct RecoveryReceipt {
    store: Arc<Store>,
    operation: OperationRecord,
    attempt: AttemptRecord,
    step: FrozenModelChildStep,
}

impl InferenceRecoverySource for RecoveryReceipt {
    fn request_id(&self) -> EntityId {
        self.step.request_id
    }
    fn binding(&self) -> &ProviderBinding {
        &self.step.provider_binding
    }
    fn payload(&self) -> &InferencePayload {
        &self.step.payload
    }

    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(&self.store, &self.operation, &self.attempt, &self.step)
    }
}

fn read_original_terminal(
    store: &Store,
    original: &OperationRecord,
    original_attempt: &AttemptRecord,
    step: &FrozenModelChildStep,
) -> Result<Option<InferenceResult>> {
    let operation = store.external_operation(original.operation_id)?;
    let attempt = store.external_attempt(original_attempt.attempt_id)?;
    ensure!(
        operation.operation_name == MODEL_CHILD_STEP_OPERATION
            && attempt.operation_id == operation.operation_id
            && attempt.transport_ref == original_attempt.transport_ref
            && attempt.armed_commit == original_attempt.armed_commit
            && attempt.invocation_id == original_attempt.invocation_id
            && attempt.epoch == original_attempt.epoch
            && operation.final_input_ref == original.final_input_ref
            && operation.contract_ref.contract_digest == original.contract_ref.contract_digest
            && operation.input_digest == original.input_digest
            && operation.input_digest == operation.final_input_ref.digest,
        "recovered inference operation/input identity mismatch"
    );
    if attempt.dispatch_stage != AttemptDispatchStage::Ended {
        return Ok(None);
    }
    let id = attempt
        .final_receipt_id
        .context("ended model attempt lacks receipt")?;
    let evidence = store.external_evidence(id)?;
    ensure!(
        evidence.receipt_id == id && evidence.operation_id == Some(operation.operation_id)
            && evidence.attempt_id == Some(attempt.attempt_id)
            && evidence.source_id == MODEL_CHILD_STEP_OPERATION,
        "recovered inference final E1 identity mismatch"
    );
    let bytes = store.cas.read_object(&evidence.raw_ref.digest)?;
    ensure!(
        bytes.len() as u64 == evidence.raw_ref.bytes
            && Digest::blake3(&bytes) == evidence.raw_ref.digest,
        "recovered inference receipt CAS mismatch"
    );
    let result: InferenceResult = serde_json::from_slice(&bytes)?;
    ensure!(
        result.request_id == step.request_id
            && result
                .effective
                .matches_seal(&step.provider_binding, &step.payload)
            && result.termination.kind != TerminationKind::Unknown,
        "recovered inference final E1 lacks original terminal evidence"
    );
    let binding_ref = attempt
        .transport_ref
        .as_ref()
        .context("original inference attempt lacks selected-send metadata")?;
    let bytes = store.cas.read_object(&binding_ref.digest)?;
    ensure!(
        bytes.len() as u64 == binding_ref.bytes && Digest::blake3(&bytes) == binding_ref.digest,
        "original selected-send metadata CAS mismatch"
    );
    let binding: InferenceSendBinding = serde_json::from_slice(&bytes)?;
    ensure!(
        binding.schema_version == 1 && binding.request_id == step.request_id,
        "original quota send belongs to another request"
    );
    if let Some(send_id) = binding.send_id {
        // Retry a missing settlement from the exact original E1. Do not
        // replay header observations with a fresh clock/TTL on recovery.
        settle_quota_terminal(store, &result, step.request_id, send_id);
    }
    Ok(Some(result))
}

/// The cognition calls this finite child can actually make.
///
/// `describe_tools` only reports tools the runtime has registered and the
/// delegation actually granted, so this reads the runtime's truth rather than
/// the delegation's wish. `None` means this child has no call at all: the
/// directory still says what is installed, but nothing in it is advertised as
/// callable.
fn cognition_grant(
    catalog: &[(String, u32, String)],
) -> Option<shenko_awareness_core::CognitionCall> {
    let granted = |kind: &str| {
        catalog.iter().any(|(name, version, _)| {
            name == kind && *version == shenko_awareness_core::TOOL_VERSION
        })
    };
    let call = shenko_awareness_core::CognitionCall::Tool {
        read: granted(shenko_awareness_core::READ_TOOL),
        list: granted(shenko_awareness_core::LIST_TOOL),
    };
    (!call.is_empty()).then_some(call)
}

#[cfg(test)]
mod grant_account_tests {
    use super::*;

    fn binding() -> ProviderBinding {
        ProviderBinding::Local {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "scripted".into(),
            allow_invalid_certs: false,
            serializer_revision: 1,
            default_max_tokens: 4096,
        }
    }

    fn spec(target_account: &str, grant_account: Option<&str>) -> ModelChildSpec {
        ModelChildSpec {
            goal: "keep the external id".into(),
            system_prompt: "you are the worker".into(),
            model: "scripted".into(),
            max_tokens: 256,
            tools: vec![],
            context: vec![],
            budget: None,
            reply_grant: Some(ModelChildReplyGrant {
                target: PublicationTarget::new("qq", "group", "12345", target_account),
                account: grant_account.map(str::to_string),
                allowed_message_types: vec!["text".into()],
                reply_to_event_id: None,
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            }),
            continue_task_id: None,
            owner_answer: None,
        }
    }

    fn contract(
        spec: ModelChildSpec,
        schema: u32,
    ) -> Result<FrozenModelChild> {
        FrozenModelChild::build(
            EntityId::new(),
            EntityId::new(),
            spec,
            &binding(),
            RuntimeGeneration("grant-account-test".into()),
            None,
            schema,
        )
    }

    /// The audience kind names which kind of audience an id belongs to; the
    /// account names the connector that owns the conversation.
    #[test]
    fn the_contract_binds_the_account_namespace_not_the_audience_kind() {
        let frozen = contract(spec("qq:9", None), CONTRACT_SCHEMA).unwrap();
        assert_eq!(frozen.schema_version, CONTRACT_SCHEMA);
        let grant = frozen.reply_grant.as_ref().unwrap();
        assert_eq!(grant.target_account, "qq:9");
        assert_eq!(grant.target_provider, "qq");
        assert_eq!(grant.target_conversation, "12345");
    }

    /// An explicit grant account that agrees with the target is kept.
    #[test]
    fn an_explicit_grant_account_wins_when_it_agrees() {
        let frozen = contract(spec("qq:9", Some("qq:9")), CONTRACT_SCHEMA).unwrap();
        assert_eq!(frozen.reply_grant.as_ref().unwrap().target_account, "qq:9");
    }

    /// A grant that names another account than its own target is refused
    /// instead of being resolved by guesswork.
    #[test]
    fn a_contradicting_account_is_refused() {
        let error = contract(spec("qq:10", Some("qq:9")), CONTRACT_SCHEMA).unwrap_err();
        assert!(
            error.to_string().contains("contradicts"),
            "unexpected error: {error}"
        );
    }

    /// A contract sealed before the account namespace was separated from the
    /// audience kind still recovers exactly as it was sealed.
    #[test]
    fn a_legacy_contract_still_validates() {
        let legacy = contract(spec("qq:9", None), LEGACY_CONTRACT_SCHEMA).unwrap();
        assert_eq!(legacy.schema_version, LEGACY_CONTRACT_SCHEMA);
        assert_eq!(
            legacy.reply_grant.as_ref().unwrap().target_account,
            "group",
            "the legacy derivation named the audience kind"
        );
        legacy.validate().unwrap();
        let current = contract(spec("qq:9", None), CONTRACT_SCHEMA).unwrap();
        assert_eq!(current.reply_grant.as_ref().unwrap().target_account, "qq:9");
    }

    /// An unknown contract version is not silently reinterpreted.
    #[test]
    fn an_unknown_contract_version_is_refused() {
        let mut frozen = contract(spec("qq:9", None), CONTRACT_SCHEMA).unwrap();
        frozen.schema_version = CONTRACT_SCHEMA + 1;
        assert!(frozen.validate().is_err());
    }

    /// The installed cognition directory is advisory material for this child,
    /// and a child with no published catalogue is not told anything about
    /// units it cannot see.
    #[test]
    fn the_cognition_directory_travels_only_when_it_exists() {
        let frozen = contract(spec("qq:9", None), CONTRACT_SCHEMA).unwrap();
        let index = "- unit@1 [perception] card。正文按需读取：[tool:cognition.unit.read:1] {}";
        let with_index = ModelChildHandler::system_prompt(&frozen, &[], Some(index));
        assert!(with_index.contains("已安装的认知单元目录"), "{with_index}");
        assert!(with_index.contains(index), "{with_index}");
        // The authored prompt and the tool catalog still lead.
        assert!(with_index.starts_with(ROLE_CONTRACT));
        assert!(with_index.contains("you are the worker"));
        let without = ModelChildHandler::system_prompt(&frozen, &[], None);
        assert!(!without.contains("已安装的认知单元目录"), "{without}");
    }

    /// Only the tools the runtime actually registered and the delegation
    /// granted name a call; nothing else is advertised.
    #[test]
    fn only_the_registered_grants_name_a_call() {
        use shenko_awareness_core::{CognitionCall, LIST_TOOL, READ_TOOL, TOOL_VERSION};
        let tool = |kind: &str, version: u32| (kind.to_string(), version, String::new());
        assert_eq!(
            cognition_grant(&[tool(READ_TOOL, TOOL_VERSION)]),
            Some(CognitionCall::Tool { read: true, list: false })
        );
        assert_eq!(
            cognition_grant(&[tool(LIST_TOOL, TOOL_VERSION)]),
            Some(CognitionCall::Tool { read: false, list: true })
        );
        assert_eq!(
            cognition_grant(&[tool(READ_TOOL, TOOL_VERSION), tool(LIST_TOOL, TOOL_VERSION)]),
            Some(CognitionCall::Tool { read: true, list: true })
        );
        // An allowed tool the registry does not hold, or another definition
        // version, is not a grant this child can use.
        assert_eq!(cognition_grant(&[tool(READ_TOOL, TOOL_VERSION + 1)]), None);
        assert_eq!(cognition_grant(&[]), None);
    }

}
