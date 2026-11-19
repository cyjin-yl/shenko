//! Independent Agent Skills native read surface. It neither loads an MCP SDK
//! nor starts scripts, installs upstream dependencies or authorizes accounts.
use std::{borrow::Cow, convert::Infallible, sync::Arc};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::{installation::UnitRef, recorded::{RecordedCall, Workers, joined, reconcile_original}};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeChildService, NativeTaskHandlerPlugin};
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{evidence::ObservationLevel, ids::EntityId, operation::*,
    plugin_runtime::NativeCapabilityDeclaration, task::TaskRecord};
use crate::{UNIT_ID, SourceQuery, source_view, validate_member_path};

const NAME: &str = "compat.skill.source";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input { unit: UnitRef, #[serde(default)] query: SourceQuery }

struct Runtime { store: Arc<Store>, node: Arc<Mutex<Node>>, workers: Workers }
impl Runtime {
    fn source(&self, input: &Input) -> Result<Value> {
        source_view(&self.store.cas, &self.node.lock(), &input.unit, &input.query, true)
    }

    async fn run(self: Arc<Self>, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(invocation.task.kind == NAME && invocation.task.definition_version == 2, "Skill source task identity mismatch");
        let input: Input = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        if let Some(path) = &input.query.path { validate_member_path(path)?; }
        input.unit.installed(&self.node.lock(), UNIT_ID)?;
        let mut call = RecordedCall::prepare(&self.store, &invocation, &descriptor()?, &input, serde_json::to_value(&input.unit)?,
            "exact original source membership/bytes or an explicit read failure; never execution",
            "reconcile original receipt, never manufacture a second installation or a different source version")?;
        if let Some(existing) = call.arm(&self.store, &invocation)? { return Ok(existing); }
        if self.workers.stop.is_cancelled() || self.store.external_task(invocation.task.task_id)?.abort_request.is_some() {
            return call.observe(&self.store, json!({"source":input.unit,"status":"cancelled_before_read"}), Some(OperationOutcome::Cancelled), EffectStatus::None, None);
        }
        match self.source(&input) {
            Ok(value) => call.observe(&self.store, value, Some(OperationOutcome::Succeeded), EffectStatus::None, Some(ObservationLevel::PostconditionVerified)),
            Err(error) => call.observe(&self.store, json!({"source":input.unit,"status":"source_read_failed","error":error.to_string()}), Some(OperationOutcome::Failed), EffectStatus::None, None),
        }
    }
}

const PROMPT: &str = "Optional Agent Skills compatibility. Input {unit:{unit_id,revision,artifact_digest},query?:{path?,offset?,limit?}}. Without path, offset is a member ordinal; with a root-relative path, offset is a byte position. limit is 1..131072 bytes (default 131072); follow next_offset until null. Reads authenticate only the requested archive chunks, not unread bytes. Original YAML, scripts and dependencies stay source material, not grants or automatic execution. Receipts can support independent, correctable cognition units through learning_source; upstream source is never rewritten by learning.";

fn descriptor() -> anyhow::Result<NativeTaskDescriptor> {
    Ok(NativeTaskDescriptor { kind: NAME.into(), definition_version: 2,
        implementation_ref: shenko_types::task::ImplementationRef {
            plugin_id: "cd54a861-bf5b-4826-88ac-eb7a311af149".parse().expect("fixed Skill plugin identity"),
            artifact_digest: shenko_kernel::native_artifact::native_artifact_digest()?, registry_revision: 0 },
        role_revision: "native-child/1".into(), authority_refs: vec!["compat.skill/v2".into()],
        prompt_section: Some(PROMPT.into()) })
}

struct Handler(Arc<Runtime>);
#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> anyhow::Result<NativeTaskDescriptor> { descriptor() }
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration { capability_id: NAME.into(), version: "2".into(), description: PROMPT.into(),
            usage: Some("delegate:compat.skill.source:2 {\"unit\":{\"unit_id\":\"...\",\"revision\":\"...\",\"artifact_digest\":\"blake3:...\"},\"query\":{\"path\":\"SKILL.md\"}}".into()) }]
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(self.0.workers.spawn(self.0.clone().run(invocation))?).await
    }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_original(&self.0.store, task, operation_id, NAME)
    }
}

pub struct SkillPlugin;
impl Plugin for SkillPlugin {
    type Config = shenko_compat::runtime::AdapterHost;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-skill") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        shenko_compat::runtime::watch_installed(ctx, config.clone(), UNIT_ID, SkillGeneration,
            |_| std::future::ready(Ok(()))).await.map_err(std::io::Error::other)
    }
}

#[derive(Clone)]
struct SkillGeneration;
impl Plugin for SkillGeneration {
    type Config = (Arc<Store>, Arc<Mutex<Node>>);
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-skill-generation") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let runtime = Arc::new(Runtime { store: config.0.clone(), node: config.1.clone(), workers: Workers::default() });
        let cleanup = runtime.clone();
        ctx.effect(move || async move { cleanup.workers.close().await; }).map_err(std::io::Error::other)?;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(Handler(runtime));
        NativeTaskHandlerPlugin.apply(ctx, &handler).await
    }
}
