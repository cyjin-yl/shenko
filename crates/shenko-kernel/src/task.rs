//! The native task handler contract: what a plugin puts on the capability
//! shelf (docs/specs/kernel.md#kernel-7).
//!
//! A handler describes one native definition, declares its effect contracts
//! and capabilities, executes an invocation and reconciles an unknown outcome
//! from the original handle (docs/specs/kernel.md#kernel-5). The types here
//! name only protocol data from `shenko-types`, so a plugin that provides a
//! handler depends on the kernel interface and not on the subject or the
//! ledger. Registration, dispatch and the durable receipts stay with the
//! runtime that owns the registry.
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_types::{
    agent::CallContext,
    effect::EffectContract,
    ids::EntityId,
    operation::{AuthorityRef, OperationRecord},
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskInvocation, TaskRecord},
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct NativeTaskDescriptor {
    pub kind: String,
    pub definition_version: u32,
    pub implementation_ref: ImplementationRef,
    pub role_revision: String,
    pub authority_refs: Vec<String>,
    /// Model-facing usage shown only while this definition is installed.
    pub prompt_section: Option<String>,
}

impl NativeTaskDescriptor {
    /// The versioned identity of this definition's implementation.
    ///
    /// One construction, so the live provider view, the effect contract of
    /// an operation and the artifact digest in the ledger all name the same
    /// implementation of the same definition.
    pub fn implementation(&self) -> shenko_types::object::VersionedRef {
        shenko_types::object::VersionedRef {
            artifact: shenko_types::object::ArtifactRef {
                digest: self.implementation_ref.artifact_digest.clone(),
                locator: format!("native:{}", self.kind),
            },
            symbol: format!("native.{}.execute", self.kind),
            schema: shenko_types::object::SchemaRef {
                name: format!("native-task/{}", self.kind),
                major: self.definition_version,
                digest: self.implementation_ref.artifact_digest.clone(),
            },
        }
    }
}

pub struct NativeTaskInvocation {
    pub context: CallContext,
    pub task_invocation: TaskInvocation,
    pub task: TaskRecord,
    pub operation_id: EntityId,
    pub authority: AuthorityRef,
    pub input: serde_json::Value,
    /// The shared native runtime as seen by a finite model child. It is
    /// present only for handlers that may call other handlers; a tool
    /// subtask receives the same runtime, so a nested child keeps working.
    pub child_tools: Option<Arc<dyn ChildToolRuntime>>,
}

/// One tool call a finite model child asks the shared native runtime for.
/// The call names a registered native definition; the runtime owns the
/// authority, the execution and the receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildToolCall {
    /// The child's own call identity, stable across a restart.
    pub call_id: String,
    /// The durable subtask identity this call runs as.
    pub task_id: EntityId,
    pub kind: String,
    pub definition_version: u32,
    pub input: serde_json::Value,
}

/// What the shared runtime actually did with one tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildToolOutcome {
    pub call_id: String,
    pub kind: String,
    pub definition_version: u32,
    pub task_id: EntityId,
    pub operation_id: EntityId,
    /// `succeeded`, `failed`, `cancelled`, `unknown` or `unavailable`.
    pub outcome: String,
    pub result: Option<serde_json::Value>,
    pub evidence_ref: Option<shenko_types::object::ObjectRef>,
}

/// The shared native runtime as seen by a finite model child. A child may
/// only call handlers that are actually registered: the runtime owns their
/// authority and their durable effects.
#[async_trait]
pub trait ChildToolRuntime: Send + Sync + 'static {
    /// The installed description of every allowed tool that is actually
    /// registered. An allowed tool the registry does not have is not
    /// offered: the child's catalog is the runtime's truth, not a wish.
    fn describe_tools(
        &self,
        allowed: &[shenko_types::output::ModelChildTool],
    ) -> Vec<(String, u32, String)>;

    async fn call_tool(
        &self,
        call: &ChildToolCall,
        parent: &TaskRecord,
        authority: &AuthorityRef,
    ) -> anyhow::Result<ChildToolOutcome>;
}

#[async_trait]
pub trait NativeTaskHandler: Send + Sync + 'static {
    fn descriptor(&self) -> NativeTaskDescriptor;

    /// The docs/specs/durable-state.md#spec-36 effect contract of this definition's operations.
    ///
    /// A declaration is bound to the implementation that produced it, so the
    /// recovery planner reads the real boundary of the operation it is about
    /// to run. A definition with no external effect may leave this empty; the
    /// runtime then publishes the definition without an effect contract
    /// instead of pretending one exists.
    fn effect_contracts(&self) -> Vec<EffectContract> {
        Vec::new()
    }

    /// The capabilities this definition offers while it is installed.
    ///
    /// The declaration is a description of what the implementation can
    /// actually do; it never grants a permission and never becomes a
    /// keyword filter over the subject's requests.
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        Vec::new()
    }

    /// T1 and external effects belong to the handler. The returned operation is
    /// evidence, not authority to mark the task complete by itself.
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord>;

    /// Recovery may query the original provider handle/receipt. It receives no
    /// new CallContext and must never arm another attempt.
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId)
    -> Result<OperationRecord>;
}
