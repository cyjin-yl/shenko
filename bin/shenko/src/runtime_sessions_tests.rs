//! One actual Cordis registration/restart/dispose journey, without a seat process.
use super::SeatSessionsPlugin;
use anyhow::{Context as _, Result, ensure};
use shenko_kernel::Supervisor;
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

const KINDS: [&str; 8] = [
    "ssh.exec", "ssh.pty", "omp.seat", "claude.seat",
    "seat.observe", "seat.respond", "seat.fleet", "seat.attach.local",
];

fn installed(native: &NativeChildService, supervisor: &Supervisor) -> Result<()> {
    let expected: BTreeSet<_> = KINDS.into_iter().map(|kind| (kind.to_owned(), 1)).collect();
    let actual: BTreeSet<_> = native.installed_definitions().into_iter().collect();
    ensure!(actual == expected, "seat definitions differ: {actual:?}");
    for kind in KINDS {
        ensure!(supervisor.runtime.provider(&format!("native.{kind}")).is_some(),
            "live provider missing for {kind}");
    }
    Ok(())
}

fn withdrawn(native: &NativeChildService, supervisor: &Supervisor) -> Result<()> {
    ensure!(native.installed_definitions().is_empty(), "disposed generation kept native handlers");
    let view = supervisor.runtime.view();
    ensure!(view.providers.is_empty() && view.capabilities.is_empty() && view.help.is_empty(),
        "disposed generation kept live registry entries: {view:?}");
    Ok(())
}

#[test]
fn seat_generation_restart_dispose_and_reinstall_own_all_eight_handlers() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let root = std::env::temp_dir().join(format!("shenko-seat-generation-{}", EntityId::new()));
        std::fs::create_dir(&root)?;
        let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
        let generation = RuntimeGeneration::new("seat-generation-fixture");
        let supervisor = Supervisor::new(generation.clone());
        let checked = tokio::time::timeout(Duration::from_secs(15), async {
            supervisor.publish_services().await?;
            supervisor.spawn(NativeChildPlugin, (store.clone(), generation)).await?;
            let native = supervisor.context().try_service::<NativeChildService>()?;
            withdrawn(&native, &supervisor)?;
            let owner = supervisor.spawn(SeatSessionsPlugin, store.clone()).await?;
            installed(&native, &supervisor)?;
            let initial_revision = supervisor.runtime.revision();

            // Same owner Fiber: the former generation must withdraw all eight
            // registrations before re-applying the same sealed Store input.
            supervisor.restart(&owner).await?;
            installed(&native, &supervisor)?;
            ensure!(supervisor.runtime.revision() > initial_revision,
                "restart did not replace the actual live registrations");
            supervisor.uninstall(&owner).await?;
            withdrawn(&native, &supervisor)?;

            // A fresh owner must also install after disposal without relying
            // on global removal of another plugin of the same concrete type.
            let replacement = supervisor.spawn(SeatSessionsPlugin, store.clone()).await?;
            installed(&native, &supervisor)?;
            supervisor.uninstall(&replacement).await?;
            withdrawn(&native, &supervisor)?;
            Ok::<_, anyhow::Error>(())
        }).await.context("seat generation lifecycle exceeded its observation budget")
            .and_then(|result| result);
        // Always observe the real host cleanup, including after an assertion
        // or lifecycle error. Failure evidence keeps its owned directory.
        let stopped = tokio::time::timeout(Duration::from_secs(15), supervisor.stop()).await
            .context("seat fixture host did not finish its stop barrier")
            .and_then(|result| result);
        match (checked, stopped) {
            (Ok(()), Ok(())) => {
                drop(supervisor);
                drop(store);
                std::fs::remove_dir_all(&root)?;
                Ok(())
            }
            (Err(error), Ok(())) => Err(error.context(format!(
                "seat generation fixture retained at {}", root.display()
            ))),
            (Ok(()), Err(error)) => Err(error.context(format!(
                "seat generation cleanup evidence retained at {}", root.display()
            ))),
            (Err(error), Err(cleanup)) => Err(error.context(format!(
                "seat generation cleanup also failed: {cleanup:#}; evidence at {}", root.display()
            ))),
        }
    })
}

#[cfg(unix)]
#[path = "runtime_sessions_owner_native_tests.rs"]
mod owner_native;
