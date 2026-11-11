//! Ordered uninstall of one installed plugin generation — docs/specs/context-policies.md#spec-41-8-4/A18.
//!
//! The real Cordis Fiber, the real hook registry and the real live registry
//! are torn down in the documented order: hooks leave new-request discovery,
//! their dispatch barrier is awaited, then the generation's registrations are
//! withdrawn, then Cordis finishes the plugin's own cleanup. The test holds a
//! dispatch lease across the uninstall to prove the barrier is real.
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cordis_core::{Context, Plugin};
use shenko_kernel::hooks::{
    HOOK_SCHEMA_VERSION, HookEntry, HookHandler, HookPayload, HookPointId, HookResult, HookSpec,
    STANDARD_HOOK_POINT_MAJOR,
};
use shenko_kernel::runtime::{GenerationRegistrations, ProviderRegistration};
use shenko_kernel::{InstalledPlugin, Supervisor};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::{ArtifactRef, Digest, SchemaRef, VersionedRef};
use shenko_types::plugin_runtime::ProviderHealth;

const PLUGIN: &str = "00000000-0000-0000-0000-0000000000c1";
const HOOK: &str = "00000000-0000-0000-0000-0000000000c2";

struct Live {
    cleaned: Arc<AtomicBool>,
}

impl Plugin for Live {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }

    async fn apply(&self, ctx: Context, (): &()) -> Result<(), std::io::Error> {
        let cleaned = Arc::clone(&self.cleaned);
        ctx.effect(move || async move {
            cleaned.store(true, Ordering::SeqCst);
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}

struct Observe;

#[async_trait::async_trait]
impl HookHandler for Observe {
    async fn handle(&self, _: &HookPayload) -> anyhow::Result<HookResult> {
        Ok(HookResult::NoChange)
    }
}

fn entry() -> HookEntry {
    let digest = Digest::blake3(b"generation-uninstall/hook");
    HookEntry::new(
        HookSpec {
            hook_id: HOOK.parse().unwrap(),
            plugin_id: PLUGIN.parse().unwrap(),
            point: HookPointId::RUNTIME_CAPABILITY_CHANGED,
            name: "generation-uninstall".into(),
            schema_version: HOOK_SCHEMA_VERSION,
            point_major: STANDARD_HOOK_POINT_MAJOR,
            implementation: VersionedRef {
                artifact: ArtifactRef {
                    digest: digest.clone(),
                    locator: "test:generation-uninstall".into(),
                },
                symbol: "hook.generation_uninstall.handle".into(),
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
        Arc::new(Observe),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_retires_hooks_then_registrations_then_the_plugin() {
    let supervisor = Supervisor::new(RuntimeGeneration::new("generation-uninstall"));
    let cleaned = Arc::new(AtomicBool::new(false));
    let fiber = supervisor
        .spawn(
            Live {
                cleaned: Arc::clone(&cleaned),
            },
            (),
        )
        .await
        .expect("install the real Cordis plugin");
    let plugin_id: EntityId = PLUGIN.parse().unwrap();
    let hook_id: EntityId = HOOK.parse().unwrap();
    let digest = Digest::blake3(b"generation-uninstall/provider");
    let implementation = VersionedRef {
        artifact: ArtifactRef {
            digest: digest.clone(),
            locator: "test:generation-uninstall/provider".into(),
        },
        symbol: "provider.generation_uninstall".into(),
        schema: SchemaRef {
            name: "provider-v1".into(),
            major: 1,
            digest,
        },
    };
    let publication = supervisor
        .install_generation(plugin_id, vec![entry()])
        .expect("publish the plugin's hook generation");
    let provider = supervisor
        .runtime
        .register_provider(ProviderRegistration {
            provider_id: "test.generation-uninstall".into(),
            kind: "native.task".into(),
            version: "1.0.0".into(),
            plugin_id,
            implementation,
            description: "generation uninstall fixture".into(),
            operations: vec![],
            capabilities: vec![],
            health: ProviderHealth::Healthy,
        })
        .expect("register the generation's provider");
    let mut registrations = GenerationRegistrations::new();
    registrations.record_provider(provider);

    // An in-flight dispatch leases the graph the handler lives in.
    let leased = supervisor.hooks.snapshot();
    let uninstall = supervisor.uninstall_installed(
        plugin_id,
        InstalledPlugin {
            plugin_id,
            fiber,
            hooks: Some(publication),
            registrations,
        },
    );
    tokio::pin!(uninstall);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut uninstall)
            .await
            .is_err(),
        "uninstall must wait for the real dispatch barrier"
    );
    // Discovery already changed; nothing was released yet.
    assert!(supervisor.hooks.handlers_of(plugin_id).is_empty());
    assert!(
        supervisor.runtime.provider("test.generation-uninstall").is_some(),
        "registrations are withdrawn only after the dispatch barrier"
    );
    assert!(
        !cleaned.load(Ordering::SeqCst),
        "the Cordis cleanup has not run while a dispatch still holds the graph"
    );

    drop(leased);
    let receipt = uninstall.await.expect("the ordered uninstall completes");
    let hooks = receipt.hooks.expect("the plugin published a hook generation");
    assert_eq!(hooks.withdrawn, vec![hook_id]);
    assert!(!hooks.already_retired);
    assert!(hooks.quiesced);
    assert_eq!(hooks.live_leases, 0);
    assert_eq!(
        receipt.registrations.providers,
        vec!["test.generation-uninstall".to_string()]
    );
    assert!(receipt.plugin_disposed);
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(supervisor.runtime.provider("test.generation-uninstall").is_none());
    assert!(supervisor.hooks.handlers_of(plugin_id).is_empty());
    supervisor.stop().await.expect("stop the host");
}
