//! Shared native session backends and their generation-owned handler bindings.
use std::{path::Path, sync::Arc};

use anyhow::Result;

use crate::update_runtime;

pub(super) async fn install_device_sessions(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::Store>,
    data_dir: &Path,
) -> Result<()> {
    // Desktop and VM handlers share one provider registry, lease authority
    // and session state across tasks.
    let computer_use_backend =
        Arc::new(shenko_exec_host::ComputerUseBackend::new(Arc::clone(store)));
    let computer_use_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 6] = [
        Arc::new(shenko_exec_host::ComputerUseSessionHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseObserveHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseLeaseHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseInputHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseCapabilitiesHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseVmHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
    ];
    for handler in computer_use_handlers {
        let _computer_use_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    let browser_provider = Arc::new(shenko_exec_host::BrowserProvider::new(
        Arc::clone(store),
        data_dir.join("browser"),
    ));
    computer_use_backend.register_human_input_observer(browser_provider.clone());
    let browser_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 5] = [
        Arc::new(shenko_exec_host::browser::handlers::BrowserSessionHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserPageHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserActionHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserControlHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserIdentityHandler::new(
            Arc::clone(&browser_provider),
        )),
    ];
    for handler in browser_handlers {
        let _browser_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    let mobile_bridge: Arc<dyn shenko_mobile::DeviceBridge> = {
        #[cfg(target_os = "android")]
        {
            Arc::new(shenko_mobile::SocketBridge::new(data_dir))
        }
        #[cfg(not(target_os = "android"))]
        {
            Arc::new(shenko_mobile::UnavailableBridge::new())
        }
    };
    let mobile_backend = Arc::new(shenko_mobile::AndroidBackend::new(
        Arc::clone(store),
        mobile_bridge,
        data_dir.to_path_buf(),
    ));
    let mobile_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 6] = [
        Arc::new(shenko_mobile::AndroidDeviceProbeHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidScreenObserveHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidControlLeaseHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidControlInputHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidShellExecHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidAppDbSnapshotHandler::new(Arc::clone(&mobile_backend))),
    ];
    for handler in mobile_handlers {
        let _mobile_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    Ok(())
}

pub(super) async fn install_repl_sessions(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::Store>,
    data_dir: &Path,
    handoff_participants: &Arc<shenko_update::handoff::HandoffParticipantRegistry>,
) -> Result<()> {
    // Stateful sessions and journaled cells share one session owner and the
    // same native registry used by runtime discovery.
    let repl_session_handler = shenko_repl::task::ReplSessionHandler::new(
        Arc::clone(store),
        data_dir.join("repl"),
    )?;
    let repl_sessions = repl_session_handler.sessions();
    handoff_participants.register(Arc::new(update_runtime::ReplSessions(
        Arc::clone(&repl_sessions),
    )));
    let repl_cell_handler = shenko_repl::task::ReplCellHandler::new(
        Arc::clone(store),
        repl_sessions,
    );
    let repl_session_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(repl_session_handler);
    let repl_cell_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(repl_cell_handler);
    let _repl_session_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, repl_session_handler)
        .await?;
    let _repl_cell_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, repl_cell_handler)
        .await?;
    Ok(())
}

pub(super) async fn install_seat_sessions(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::Store>,
) -> Result<()> {
    // SSH PTY, managed OMP/Claude seats and seat question/answer operations
    // share one live seat owner.
    let seat_runtime = Arc::new(shenko_exec_ssh::seat::runtime::SeatRuntime::new(Arc::clone(store)));
    let seat_ssh_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 7] = [
        Arc::new(shenko_exec_ssh::SshExecHandler::new(Arc::clone(store))),
        Arc::new(shenko_exec_ssh::SshPtyHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::OmpSeatHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::ClaudeSeatHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::SeatObserveHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::SeatRespondHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::SeatFleetHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
    ];
    for handler in seat_ssh_handlers {
        let _seat_ssh_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    // R070: durable seat records outlive this process. Rebind every active
    // record whose transport survives a restart (a local tmux pane) so the
    // subject can observe and answer questions recorded before the restart;
    // managed processes and PTYs stay durable-only. A record the runtime
    // cannot reach is reported, never fabricated or retired here.
    match seat_runtime.reattach_active_seats().await {
        Ok(report) => tracing::info!(
            reattached = report.reattached.len(),
            incarnation_bumped = report.incarnation_bumped.len(),
            skipped = report.skipped.len(),
            "seat runtime reattached durable seat records"
        ),
        Err(error) => tracing::warn!(
            error = format!("{error:#}"),
            "seat runtime reattach scan failed; durable seat records remain the authority"
        ),
    }
    Ok(())
}
