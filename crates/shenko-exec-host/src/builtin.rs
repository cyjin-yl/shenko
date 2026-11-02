//! The complete built-in native handler set, installed onto a real Cordis
//! supervisor runtime.
//!
//! A composition root (the REPL server, the playground server, a node seat)
//! calls [`spawn_builtin_runtime`] instead of hand-wiring each handler. The
//! research, retention and model-child-intervention children are then real
//! runtime citizens: the dispatcher claims their task kinds, the registry
//! resolves their frozen implementation identities, and recovery reconciles
//! their armed attempts — through the same plugin path as every other native
//! child.

use anyhow::Result;
use shenko_broker::broker::BrokerHandle;
use shenko_kernel::Supervisor;
use shenko_memory::intervention::{MemoryInterventionRegistry, ModelMemoryInterventionPolicy};
use shenko_store::Store;
use shenko_subject::{
    NativeChildPlugin, NativeChildService, NativeTaskHandler, NativeTaskHandlerPlugin,
    PmaEvaluateHandler,
};
use shenko_types::ids::RuntimeGeneration;
use std::sync::Arc;

use crate::research_task::{
    ResearchBankHandler, ResearchCandidateHandler, ResearchResetHandler,
};
use crate::retention_task::RetentionApplyHandler;

pub struct BuiltinNativeRuntime {
    pub supervisor: Supervisor,
    pub native: Arc<NativeChildService>,
}

/// Spawn the native child runtime with every built-in handler installed and
/// recovered. `model_child` is the inference handle the model children and
/// the PMA intervention worker share; it is the same handle the composition
/// root gives to `ModelChildHandler`.
pub async fn spawn_builtin_runtime(
    store: Arc<Store>,
    model_child: BrokerHandle,
    generation: RuntimeGeneration,
) -> Result<BuiltinNativeRuntime> {
    let supervisor = Supervisor::new(generation.clone());
    let _child = supervisor
        .spawn(NativeChildPlugin, (store.clone(), generation))
        .await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    for handler in builtin_handlers(store, model_child) {
        supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
    }
    native.recover()?;
    Ok(BuiltinNativeRuntime { supervisor, native })
}

/// The built-in research/retention/intervention handler set. Each kind and
/// definition version is distinct, so install order is irrelevant.
pub fn builtin_handlers(
    store: Arc<Store>,
    model_child: BrokerHandle,
) -> Vec<Arc<dyn NativeTaskHandler>> {
    let policy = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        ModelMemoryInterventionPolicy,
    )));
    vec![
        Arc::new(ResearchResetHandler::new(store.clone())),
        Arc::new(ResearchCandidateHandler::new(store.clone())),
        Arc::new(ResearchBankHandler::new(store.clone())),
        Arc::new(RetentionApplyHandler::new(store.clone())),
        Arc::new(PmaEvaluateHandler::new(store.clone(), model_child, policy)),
    ]
}
