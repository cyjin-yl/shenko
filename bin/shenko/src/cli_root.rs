//! Cognitive root commands over the owned append-only version chain.
use std::path::Path;

use anyhow::Result;

use crate::cli::RootCli;
use crate::runtime_bootstrap::open_installation_store;

/// Cognitive root CLI commands.
pub(crate) fn root_command(data_dir: &Path, command: RootCli) -> Result<()> {
    let store = open_installation_store(data_dir)?;
    let agent_id = store
        .get_existing_agent_id()?
        .ok_or_else(|| anyhow::anyhow!("no initialized subject"))?;

    match command {
        RootCli::Head => {
            let head = store.cognitive_root_head(agent_id)?;
            match head {
                Some(head) => println!("{}", serde_json::to_string_pretty(&head)?),
                None => println!("{{\"status\":\"no cognitive root\"}}"),
            }
        }
        RootCli::Chain { limit } => {
            let chain = store.cognitive_root_chain(agent_id, limit)?;
            println!("{}", serde_json::to_string_pretty(&chain)?);
        }
        RootCli::Node { revision } => {
            let node = store.cognitive_root_node(revision)?;
            match node {
                Some(node) => println!("{}", serde_json::to_string_pretty(&node)?),
                None => anyhow::bail!("root revision {} not found", revision),
            }
        }
        RootCli::Capture => {
            use shenko_types::cognitive_root::*;
            use shenko_types::cognition::*;

            // Read current state from store + seed.
            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.to_owned(),
                ..Default::default()
            });
            let installation_id = store.installation_id_for_subject(agent_id)?;
            let prepared = bootstrap.prepare(installation_id)?;
            prepared.record_local_provisioning(&store)?;
            let (home_bytes, home_ref) = shenko_seed::bootstrap::seed_home_content(&prepared.manifest)?;

            // Seal home content in CAS.
            let home_sealed = store.cas.seal_object(
                &home_bytes,
                "application/json",
                HOME_CONTENT_NAMESPACE,
                HOME_CONTENT_CODEC,
                HOME_CONTENT_CODEC_MAJOR,
            )?;
            let _ = home_sealed; // content registered inside capture_cognitive_root

            // Read selector + head for fence.
            let (head, _step) = store.load_subject(agent_id)?;
            let selector = store
                .activation_selector(agent_id)?
                .map(|s| s.selector_revision)
                .unwrap_or(0);

            // Build unit selector snapshot.
            let unit_snapshot = UnitSelectorSnapshot::ok(Vec::new());
            let unit_bytes = unit_snapshot.canonical_bytes()?;
            let unit_sealed = store.cas.seal_object(
                &unit_bytes,
                "application/json",
                UNIT_SELECTOR_NAMESPACE,
                UNIT_SELECTOR_CODEC,
                UNIT_SELECTOR_CODEC_MAJOR,
            )?;

            // Build software slot ref using current generation.
            let slot_manifest = serde_json::json!({
                "generation": head.software_generation.0,
            });
            let slot_bytes = serde_json::to_vec(&slot_manifest)?;
            let slot_sealed = store.cas.seal_object(
                &slot_bytes,
                "application/json",
                SOFTWARE_SLOT_NAMESPACE,
                SOFTWARE_SLOT_CODEC,
                SOFTWARE_SLOT_CODEC_MAJOR,
            )?;

            let content = CognitiveRootContent {
                schema_version: CognitiveRootContent::SCHEMA_VERSION,
                home: home_ref,
                software: SoftwareSlotRef {
                    manifest: slot_sealed.object.digest.clone(),
                    generation: head.software_generation.clone(),
                    artifact_digest: None,
                },
                units: CognitionSelectorRef {
                    snapshot: unit_sealed.object.digest.clone(),
                },
                runtime: RuntimeStateParticipant::NotCaptured(RuntimeStateGap::new(
                    "CLI capture: WASM state backend not available offline",
                )),
            };

            let capture = CognitiveRootCapture {
                request_id: shenko_types::ids::EntityId::new(),
                fence: RootFence {
                    generation: head.software_generation.clone(),
                    epoch: head.controller_epoch,
                    selector_revision: selector,
                },
                content,
                reasons: Vec::new(),
                confidence: AttitudeConfidence::Unknown,
                effects: Vec::new(),
            };

            let node = store.capture_cognitive_root(capture)?;
            println!("{}", serde_json::to_string_pretty(&node)?);
        }
        RootCli::Reinstate { target_revision } => {
            use shenko_types::cognitive_root::*;
            use shenko_types::cognition::*;

            let target_node = store
                .cognitive_root_node(target_revision)?
                .ok_or_else(|| anyhow::anyhow!("target root revision {} not found", target_revision))?;

            let (head, _step) = store.load_subject(agent_id)?;
            let selector = store
                .activation_selector(agent_id)?
                .map(|s| s.selector_revision)
                .unwrap_or(0);

            let reinstatement = CognitiveRootReinstatement {
                request_id: shenko_types::ids::EntityId::new(),
                fence: RootFence {
                    generation: head.software_generation.clone(),
                    epoch: head.controller_epoch,
                    selector_revision: selector,
                },
                target: RootRef::of(&target_node),
                reasons: Vec::new(),
                confidence: AttitudeConfidence::Unknown,
                effects: Vec::new(),
            };

            let node = store.reinstate_cognitive_root(reinstatement)?;
            println!("{}", serde_json::to_string_pretty(&node)?);
        }
    }
    Ok(())
}
