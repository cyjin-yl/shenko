//! One real model-free startup/reopen journey; no provider or model fixture.
use super::*;

#[tokio::test]
async fn model_free_start_binds_node_before_owned_services_and_reopen() -> Result<()> {
    let fixture_id = EntityId::new();
    let dir = std::env::temp_dir().join(format!("shenko-node-start-order-{fixture_id}"));
    std::fs::create_dir(&dir)?;
    let mut observations = Vec::new();
    let result: Result<()> = async {
        let profile = shenko_seed::ModelProfile {
            provider: None,
            model: None,
            configured: false,
            capacity: shenko_seed::ModelCapacity::Blocked,
        };
        let store = Store::open(dir.join("shenko.db"), dir.join("cas"))?;
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            agent_name: None,
            napcat_ws_url: None,
        }).prepare(store.ledger.ledger_id())?;
        crate::runtime_bootstrap::bootstrap_genesis(&store, None, &home.manifest, &profile)?;
        drop(home);
        let before = Node::open_inspect(&dir.join("node"))?;
        ensure!(before.as_ref().map(Node::local_node_id).transpose()?.flatten().is_none(),
            "fixture already has a Node identity before startup");
        drop(before);
        drop(store);

        let mut original_node = None;
        for pass in 0..2 {
            let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas"))?);
            let expected_identity = crate::node_sync::bound_subject_identity(&store)?
                .context("fixture Store has no durable subject identity")?;
            let cancel = CancellationToken::new();
            let runtime_id = EntityId::new();
            let eligibility = Arc::new(parking_lot::RwLock::new(ModelEligibility::default()));
            let selection = establish_runtime(
                &dir, &store, &cancel, runtime_id, &profile, Arc::clone(&eligibility),
            ).await?;
            let heartbeat = match selection {
                RuntimeSelection::RecoveryOnly { heartbeat, reason } => {
                    observations.push(json!({"pass":pass,"selection":"recovery_only", "reason":reason}));
                    heartbeat
                }
                RuntimeSelection::Selected { heartbeat, .. } => {
                    cancel.cancel();
                    heartbeat.await.context("unexpected selected worker did not join")??;
                    bail!("an unconfigured model acquired subject authority");
                }
                RuntimeSelection::Stopped => bail!("uncancelled startup stopped unexpectedly"),
            };
            // The read-only observer never initializes a Node. The original
            // startup returned here with a presence record but no Node ID.
            let observed: Result<EntityId> = (|| {
                let node = Node::open_inspect(&dir.join("node"))?
                    .context("startup did not create the real Node store")?;
                let node_id = node.local_node_id()?;
                let identity = node.local_identity()?;
                let primary = node.committed_primary_claim()?;
                let fence = node.fencing_token()?;
                let candidate = eligibility.read().candidate(
                    runtime_id, &Digest::blake3(&serde_json::to_vec(&profile)?), None,
                );
                observations.push(json!({"pass":pass,"runtime_id":runtime_id,
                    "node_id":node_id,"identity":identity,"expected_identity":expected_identity,
                    "primary":primary,"fence":fence,"candidate":candidate}));
                let id = node_id.context("RecoveryOnly returned before Node identity initialization")?;
                ensure!(identity.as_ref() == Some(&expected_identity),
                    "Node identity is not the actual Store identity");
                ensure!(primary.is_none() && fence.is_none() && candidate.is_none(),
                    "identity initialization fabricated model or dispatch authority");
                ensure!(original_node.is_none_or(|original| original == id),
                    "reopen replaced the original Node identity");
                Ok(id)
            })();
            // Retain observation failures, but always cancel and await the
            // original owned worker before returning or closing this Store.
            cancel.cancel();
            let joined = heartbeat.await.context("runtime heartbeat join failed")
                .and_then(|result| result);
            observations.push(json!({"pass":pass,"heartbeat_joined":true,
                "heartbeat_result":joined.as_ref().map(|_| ()).map_err(|error| format!("{error:#}"))}));
            original_node = Some(match (observed, joined) {
                (Ok(id), Ok(())) => id,
                (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
                (Err(observation), Err(join)) => {
                    bail!("original observation: {observation:#}; original heartbeat: {join:#}");
                }
            });
        }
        Ok(())
    }.await;

    let retained: Result<()> = (|| {
        let Some(path) = std::env::var_os("SHENKO_SSH_WASI_EVIDENCE") else { return Ok(()); };
        let path = PathBuf::from(path);
        let parent = path.parent().context("SSH evidence path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let destination = parent.join(format!("node-start-order-{fixture_id}.json"));
        let receipt = json!({"schema_version":1,
            "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),
            "scope":"real Store/Node startup and reopen, not a process restart or model test",
            "fixture_id":fixture_id,"observations":observations,
            "result":result.as_ref().map(|_| ()).map_err(|error| format!("{error:#}"))});
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(destination)?;
        file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;
        file.sync_all()?;
        Ok(())
    })();
    match (result, retained) {
        (Ok(()), Ok(())) => std::fs::remove_dir_all(dir).map_err(Into::into),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(original), Err(export)) => bail!("original journey: {original:#}; evidence export: {export:#}"),
    }
}
