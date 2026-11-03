//! Store-only research definitions shared by the live CLI and capability
//! discovery. The composition root installs them on its existing native
//! runtime; model-backed PMA uses that root's broker and memory plugin.

use shenko_store::Store;
use shenko_subject::NativeTaskHandler;
use std::sync::Arc;

use crate::research_task::{ResearchBankHandler, ResearchCandidateHandler, ResearchResetHandler};
use crate::retention_task::RetentionApplyHandler;

pub fn research_handlers(store: Arc<Store>) -> Vec<Arc<dyn NativeTaskHandler>> {
    vec![
        Arc::new(ResearchResetHandler::new(store.clone())),
        Arc::new(ResearchCandidateHandler::new(store.clone())),
        Arc::new(ResearchBankHandler::new(store.clone())),
        Arc::new(RetentionApplyHandler::new(store)),
    ]
}
