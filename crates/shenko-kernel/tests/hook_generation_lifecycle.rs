//! Hook generation install/upgrade/uninstall boundaries — SPEC §41.8.4.
//!
//! These cases exercise the real registry, not a mock: an upgrade is one
//! atomic publication, a leased snapshot keeps dispatching the generation it
//! took, a stale publication cannot withdraw its successor, and a retirement
//! waits for the actual lease barrier instead of guessing that the old
//! dispatch already finished.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use shenko_kernel::hooks::{
    HOOK_SCHEMA_VERSION, HookAdmissionError, HookEntry, HookHandler, HookPayload, HookPointId,
    HookRegistry, HookResult, HookSpec, STANDARD_HOOK_POINT_MAJOR,
};
use shenko_types::ids::EntityId;
use shenko_types::object::{ArtifactRef, Digest, SchemaRef, VersionedRef};

const PLUGIN_A: &str = "00000000-0000-0000-0000-0000000000a1";
const PLUGIN_B: &str = "00000000-0000-0000-0000-0000000000b1";

type Seen = Arc<Mutex<Vec<&'static str>>>;

struct Observe {
    label: &'static str,
    seen: Seen,
}

#[async_trait]
impl HookHandler for Observe {
    async fn handle(&self, _: &HookPayload) -> Result<HookResult> {
        self.seen.lock().unwrap().push(self.label);
        Ok(HookResult::NoChange)
    }
}

fn plugin(id: &str) -> EntityId {
    id.parse().unwrap()
}

fn entry(
    hook_id: &str,
    plugin_id: &str,
    label: &'static str,
    point: HookPointId,
    requires: Vec<EntityId>,
    seen: &Seen,
) -> HookEntry {
    let digest = Digest::blake3(format!("hook-impl/{label}").as_bytes());
    HookEntry::new(
        HookSpec {
            hook_id: hook_id.parse().unwrap(),
            plugin_id: plugin(plugin_id),
            point,
            name: label.into(),
            schema_version: HOOK_SCHEMA_VERSION,
            point_major: STANDARD_HOOK_POINT_MAJOR,
            implementation: VersionedRef {
                artifact: ArtifactRef {
                    digest: digest.clone(),
                    locator: format!("test:hook/{label}"),
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
            requires,
            possible_model_call: false,
            deterministic: true,
        },
        Arc::new(Observe {
            label,
            seen: Arc::clone(seen),
        }),
    )
}

/// The labels that ran, sorted: the assertion is about which generation ran,
/// not about the point's deterministic tie-break order.
fn ran(seen: &Seen) -> Vec<&'static str> {
    let mut labels = seen.lock().unwrap().clone();
    labels.sort_unstable();
    labels
}

fn payload() -> HookPayload {
    HookPayload::CapabilityChanged {
        capability_id: "test.capability".into(),
        enabled: true,
    }
}

#[tokio::test]
async fn upgrade_replaces_one_plugin_and_a_leased_snapshot_keeps_its_generation() {
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let registry = HookRegistry::new();
    let point = HookPointId::RUNTIME_CAPABILITY_CHANGED;

    // Two plugins publish handlers into the same point.
    let old = registry
        .install_generation(vec![entry(
            "00000000-0000-0000-0000-0000000000a2",
            PLUGIN_A,
            "a-old",
            point.clone(),
            vec![],
            &seen,
        )])
        .expect("install plugin A");
    registry
        .install_generation(vec![entry(
            "00000000-0000-0000-0000-0000000000b2",
            PLUGIN_B,
            "b",
            point.clone(),
            vec![],
            &seen,
        )])
        .expect("install plugin B");
    let old_revision = old.generation;
    let leased_revision = registry.generation();
    let leased = registry.snapshot();

    let swap = registry
        .upgrade_generation(
            plugin(PLUGIN_A),
            vec![entry(
                "00000000-0000-0000-0000-0000000000a3",
                PLUGIN_A,
                "a-new",
                point.clone(),
                vec![],
                &seen,
            )],
        )
        .expect("upgrade plugin A");
    assert_eq!(swap.retired, vec!["00000000-0000-0000-0000-0000000000a2".parse().unwrap()]);
    assert_eq!(swap.retired_revision, leased_revision);
    // The projection names what the new generation owns; the old handler id is
    // gone and the other plugin is untouched.
    assert_eq!(
        registry.handlers_of(plugin(PLUGIN_A)),
        vec!["00000000-0000-0000-0000-0000000000a3".parse().unwrap()]
    );

    // A new dispatch observes exactly the new generation of A plus B.
    seen.lock().unwrap().clear();
    registry.dispatch(&payload()).await;
    assert_eq!(ran(&seen), vec!["a-new", "b"]);

    // The already-leased snapshot keeps running the generation it took: one
    // dispatch never mixes the two versions of the plugin.
    seen.lock().unwrap().clear();
    leased.dispatch(&payload()).await;
    assert_eq!(ran(&seen), vec!["a-old", "b"]);

    // Retirement of the replaced generation is already done as a graph
    // mutation, but its barrier is real: the leased snapshot still holds the
    // old revision.
    let retire = registry.retire_generation(&old);
    tokio::pin!(retire);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut retire)
            .await
            .is_err(),
        "retirement must not report completion while an old-generation dispatch is leased"
    );
    drop(leased);
    let receipt = retire.await.expect("retire the replaced generation");
    assert!(receipt.already_retired);
    assert!(receipt.quiesced);
    // The barrier is the publication's own identity plus the fact that no
    // dispatch of any later revision is still holding these handlers.
    assert_eq!(receipt.revision, old_revision);
    assert_eq!(receipt.live_leases, 0);
    assert!(receipt.withdrawn.is_empty());
    // The successor of the same plugin id is still admitted.
    assert_eq!(
        registry.handlers_of(plugin(PLUGIN_A)),
        vec!["00000000-0000-0000-0000-0000000000a3".parse().unwrap()]
    );
}

#[tokio::test]
async fn withdrawal_that_would_break_a_required_dependency_is_refused() {
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let registry = HookRegistry::new();
    let point = HookPointId::RUNTIME_CAPABILITY_CHANGED;
    let dependency: EntityId = "00000000-0000-0000-0000-0000000000b2".parse().unwrap();

    let dependency_publication = registry
        .install_generation(vec![entry(
            "00000000-0000-0000-0000-0000000000b2",
            PLUGIN_B,
            "b",
            point.clone(),
            vec![],
            &seen,
        )])
        .expect("install the dependency");
    // Plugin A declares B's handler as a hard dependency.
    registry
        .install_generation(vec![entry(
            "00000000-0000-0000-0000-0000000000a2",
            PLUGIN_A,
            "a",
            point.clone(),
            vec![dependency],
            &seen,
        )])
        .expect("install a dependent handler");

    let error = registry
        .withdraw_generation(&dependency_publication)
        .expect_err("withdrawing a required dependency must be refused");
    assert!(matches!(
        error,
        HookAdmissionError::MissingDependency { missing, .. } if missing == dependency
    ));

    // The refused candidate left the graph in force: both handlers still run.
    seen.lock().unwrap().clear();
    registry.dispatch(&payload()).await;
    assert_eq!(ran(&seen), vec!["a", "b"]);
}

#[tokio::test]
async fn a_stale_publication_never_withdraws_its_successor() {
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let registry = HookRegistry::new();
    let point = HookPointId::RUNTIME_CAPABILITY_CHANGED;

    let first = registry
        .install_generation(vec![entry(
            "00000000-0000-0000-0000-0000000000a2",
            PLUGIN_A,
            "a-first",
            point.clone(),
            vec![],
            &seen,
        )])
        .expect("install the first generation");
    // The successor reuses the same stable hook id with a different artifact.
    registry
        .upgrade_generation(
            plugin(PLUGIN_A),
            vec![entry(
                "00000000-0000-0000-0000-0000000000a2",
                PLUGIN_A,
                "a-second",
                point.clone(),
                vec![],
                &seen,
            )],
        )
        .expect("publish the successor generation");
    let revision = registry.generation();

    assert!(
        registry
            .withdraw_generation(&first)
            .expect("a stale withdrawal is a no-op")
            .is_empty()
    );
    assert_eq!(registry.generation(), revision, "a no-op withdrawal must not advance the graph");

    seen.lock().unwrap().clear();
    registry.dispatch(&payload()).await;
    assert_eq!(ran(&seen), vec!["a-second"]);
}
