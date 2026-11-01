//! Native graph admission, point contracts, synthesis and snapshot lifetime,
//! without model/provider calls.
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use shenko_kernel::hooks::{
    HOOK_SCHEMA_VERSION, HookAdmissionError, HookEntry, HookHandler, HookMode, HookPayload,
    HookPointContract, HookPointId, HookRegistry, HookResult, HookSpec,
    STANDARD_HOOK_POINT_MAJOR,
};
use shenko_types::ids::EntityId;
use shenko_types::object::{ArtifactRef, Digest, SchemaRef, VersionedRef};

struct Observe {
    label: &'static str,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl HookHandler for Observe {
    async fn handle(&self, _: &HookPayload) -> Result<HookResult> {
        self.seen.lock().unwrap().push(self.label);
        Ok(HookResult::NoChange)
    }
}

fn entry(
    id: &str,
    label: &'static str,
    point: HookPointId,
    seen: &Arc<Mutex<Vec<&'static str>>>,
) -> HookEntry {
    let digest = Digest::blake3(format!("hook-impl/{label}").as_bytes());
    HookEntry::new(
        HookSpec {
            hook_id: id.parse().unwrap(),
            plugin_id: "00000000-0000-0000-0000-000000000010".parse().unwrap(),
            point,
            name: label.into(),
            schema_version: HOOK_SCHEMA_VERSION,
            point_major: STANDARD_HOOK_POINT_MAJOR,
            implementation: VersionedRef {
                artifact: ArtifactRef {
                    digest: digest.clone(),
                    locator: format!("example:hook/{label}"),
                },
                symbol: format!("hook.{label}.handle"),
                schema: SchemaRef {
                    name: "hook-payload-v1".into(),
                    major: HOOK_SCHEMA_VERSION,
                    digest,
                },
            },
            before: vec![],
            after: vec![],
            requires: vec![],
            possible_model_call: false,
            deterministic: true,
        },
        Arc::new(Observe {
            label,
            seen: Arc::clone(seen),
        }),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let seen = Arc::new(Mutex::new(vec![]));
    let registry = HookRegistry::new();
    let first = entry(
        "00000000-0000-0000-0000-000000000002",
        "first",
        HookPointId::SUBJECT_BEFORE_INFERENCE,
        &seen,
    );
    let first_id = first.spec.hook_id;
    let mut second = entry(
        "00000000-0000-0000-0000-000000000001",
        "second",
        HookPointId::SUBJECT_BEFORE_INFERENCE,
        &seen,
    );
    second.spec.after.push(first.spec.hook_id);
    second.spec.requires.push(first.spec.hook_id);
    let admitted = registry.swap_generation(vec![second, first])?;
    let original_registration = admitted
        .registrations
        .iter()
        .find(|registration| registration.hook_id() == first_id)
        .unwrap();
    let original = registry.snapshot();
    let payload = HookPayload::BeforeInference {
        request_id: EntityId::new(),
        agent_id: EntityId::new(),
        model: "no-inference-demo".into(),
    };
    let results = original.dispatch(&payload).await;
    anyhow::ensure!(
        results
            .results
            .iter()
            .all(|outcome| outcome.allowed && outcome.error.is_none())
    );
    anyhow::ensure!(!results.stopped_early);
    anyhow::ensure!(*seen.lock().unwrap() == ["first", "second"]);

    let mut incompatible = entry(
        "00000000-0000-0000-0000-000000000003",
        "incompatible",
        HookPointId::SUBJECT_BEFORE_INFERENCE,
        &seen,
    );
    incompatible.spec.schema_version += 1;
    let rejected = matches!(
        registry.swap_generation(vec![incompatible]),
        Err(HookAdmissionError::UnknownSchema { .. })
    );
    anyhow::ensure!(rejected && registry.generation() == original.generation());

    let replacement = registry.swap_generation(vec![entry(
        "00000000-0000-0000-0000-000000000002",
        "replacement",
        HookPointId::SUBJECT_BEFORE_INFERENCE,
        &seen,
    )])?;
    let revision = registry.generation();
    let stale_removed = registry.unregister(original_registration)?;
    anyhow::ensure!(!stale_removed && registry.generation() == revision);
    original.dispatch(&payload).await;
    registry.dispatch(&payload).await;
    anyhow::ensure!(*seen.lock().unwrap() == ["first", "second", "first", "second", "replacement"]);
    let owner_removed = registry.unregister(&replacement.registrations[0])?;
    anyhow::ensure!(
        owner_removed
            && registry
                .order(&HookPointId::SUBJECT_BEFORE_INFERENCE)
                .is_empty()
    );

    // A newly declared point is admitted with its own contract and version.
    let point = HookPointId::new("example.collect").expect("qualified point id");
    let mut contract = HookPointContract::standard(point.clone());
    contract.mode = HookMode::Collect;
    contract.writable_slots = vec![shenko_types::plugin_runtime::ContributionSlot::TransientReminder];
    registry.declare_point(contract)?;
    let declared = registry.contracts().contains_key(&point);
    anyhow::ensure!(declared);

    let binding = original.binding();
    println!(
        "{}",
        serde_json::json!({
            "order": *seen.lock().unwrap(),
            "rejected_incompatible_schema": rejected,
            "original_graph_digest": original.graph_digest().to_string(),
            "original_revision": original.generation(),
            "active_revision": registry.generation(),
            "stale_owner_removed_replacement": stale_removed,
            "current_owner_withdrawal_succeeded": owner_removed,
            "declared_point": point.as_str(),
            "binding_revision": binding.revision,
            "binding_points": binding.point_ids,
            "model_calls": 0,
        })
    );
    Ok(())
}
