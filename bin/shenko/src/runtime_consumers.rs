//! Ownership and settlement of durable dispatch and replication consumers.
use std::{path::Path, sync::Arc};

use anyhow::Result;

use crate::{node_sync, runtime_authority::RuntimeAuthority};

pub(crate) struct RuntimeConsumers {
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    dispatcher_task: tokio::task::JoinHandle<Result<()>>,
    replication_task: tokio::task::JoinHandle<()>,
    _stop_on_exit: tokio_util::sync::DropGuard,
}

impl RuntimeConsumers {
    pub(crate) fn spawn(
        store: &Arc<shenko_store::Store>,
        native_child: &Arc<shenko_subject::NativeChildService>,
        agent_id: shenko_types::ids::EntityId,
        dispatch_results: Option<tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>>,
        data_dir: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Self {
        let consumer_cancel = tokio_util::sync::CancellationToken::new();
        let dispatch_store = Arc::clone(store);
        let dispatch_native = Arc::clone(native_child);
        let dispatch_stop = consumer_cancel.clone();
        let dispatch_subject_stopping = cancel.clone();
        let failure_cancel = cancel.clone();
        let dispatcher_task = tokio::spawn(async move {
            let result = shenko_subject::dispatch::run_committed_dispatcher(
                dispatch_store,
                dispatch_native,
                agent_id,
                dispatch_results,
                dispatch_subject_stopping,
                dispatch_stop,
            ).await;
            if result.is_err() { failure_cancel.cancel(); }
            result
        });
        let _consumer_stop_on_exit = consumer_cancel.clone().drop_guard();
        let replication_task = node_sync::spawn_runtime(
            Arc::clone(store), data_dir.to_path_buf(), consumer_cancel.clone(),
        );
        Self { cancel: consumer_cancel, dispatcher_task, replication_task, _stop_on_exit: _consumer_stop_on_exit }
    }

    pub(crate) async fn stop(self, authority_runtime: &RuntimeAuthority) -> Result<()> {
        let Self { cancel: consumer_cancel, dispatcher_task, replication_task, _stop_on_exit } = self;
        consumer_cancel.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(2), dispatcher_task).await {
            Ok(Ok(Ok(()))) => {},
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => tracing::warn!("dispatcher stop pending; durable work retained"),
        }
        if authority_runtime.recovery_required().is_some() {
            match tokio::time::timeout(std::time::Duration::from_secs(2), replication_task).await {
                Ok(joined) => joined?,
                Err(_) => tracing::warn!("replication drain pending; retained sync debt will resume"),
            }
        } else { replication_task.await?; }
        Ok(())
    }
}
