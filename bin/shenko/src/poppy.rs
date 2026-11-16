//! Assembly-owned installation and ledger bridge for the protocol-only crate.
mod account;
mod command;
mod commerce;
mod connection;
mod operational;
mod transport;
mod vault;
pub(crate) use operational::BrowserAccessPlugin;
pub(crate) use command::command;
use std::{borrow::Cow, convert::Infallible, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shenko_compat::{
    installation::plugin_enabled,
    recorded::{RecordedCall, Workers, joined, reconcile_original},
    runtime::{AdapterHost, watch_installed},
};
use shenko_compat_poppy::{UNIT_ID, discovery};
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeChildService, NativeTaskHandlerPlugin};
use shenko_types::{
    effect::*, evidence::ObservationLevel, ids::EntityId, object::Digest, operation::*,
    plugin_runtime::NativeCapabilityDeclaration, task::TaskRecord,
};

const NAME: &str = "compat.poppy.discover";
const PROMPT: &str = "Read public Poppy discovery and advertised UCP metadata for one HTTPS origin. Input {origin,timeout_secs?}; timeout is 1..=60 seconds. Results retain company identity, source URLs and declared capabilities. Discovery does not establish an account session, negotiate UCP, or execute a purchase. An unknown observation is preserved on recovery; a new read requires a separate task.";
const CONTRACT: &[u8] = b"shenko.compat.poppy.discover/v1: explicitly installed public metadata reader; T1 before bounded HTTPS GETs; discovery records declared capabilities, not negotiation, account authorization or commerce; no Session, token or cookies; original receipts survive retirement; armed calls are never replayed";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    origin: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

struct PoppyRuntime {
    store: Arc<Store>,
    node: Arc<Mutex<Node>>,
    workers: Workers,
}

impl PoppyRuntime {
    async fn discover(
        &self,
        input: &Input,
        task_id: EntityId,
    ) -> Result<discovery::DiscoveryReport> {
        let request = tokio::time::timeout(
            Duration::from_secs(input.timeout_secs),
            discovery::discover(&input.origin),
        );
        tokio::pin!(request);
        loop {
            tokio::select! {
                biased;
                _ = self.workers.stop.cancelled() => anyhow::bail!("discovery retired; completion unobserved"),
                result = &mut request => return result.map_err(anyhow::Error::from)?,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    ensure!(self.store.external_task(task_id)?.abort_request.is_none(),
                        "discovery cancelled; completion unobserved");
                    ensure!(plugin_enabled(&self.node.lock(), UNIT_ID)?,
                        "discovery plugin disabled; completion unobserved");
                }
            }
        }
    }

    async fn run(self: Arc<Self>, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == NAME && invocation.task.definition_version == 1,
            "Poppy discovery task identity mismatch"
        );
        ensure!(
            serde_json::to_vec(&invocation.input)?.len() <= 4096,
            "Poppy discovery input exceeds 4 KiB"
        );
        let input: Input = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        ensure!(
            (1..=60).contains(&input.timeout_secs),
            "timeout must be 1..=60 seconds"
        );
        discovery::validate_origin(&input.origin)?;
        ensure!(
            plugin_enabled(&self.node.lock(), UNIT_ID)?,
            "Poppy plugin is not enabled"
        );
        let mut call = RecordedCall::prepare(
            &self.store,
            &invocation,
            &descriptor(),
            &input,
            json!({"unit_id":UNIT_ID,"origin":input.origin}),
            "validated public metadata observation; declared capabilities do not prove negotiation or business success",
            "an armed discovery is never automatically repeated; recovery reads the original observation",
        )?;
        if let Some(existing) = call.arm(&self.store, &invocation)? {
            return Ok(existing);
        }
        if self.workers.stop.is_cancelled()
            || self
                .store
                .external_task(invocation.task.task_id)?
                .abort_request
                .is_some()
        {
            return call.observe(
                &self.store,
                json!({"origin":input.origin,"status":"cancelled_before_dispatch"}),
                Some(OperationOutcome::Cancelled),
                EffectStatus::None,
                None,
            );
        }
        let (value, outcome, effect, level) =
            match self.discover(&input, invocation.task.task_id).await {
                Ok(report) => (
                    json!({"status":"discovered","report":report}),
                    Some(OperationOutcome::Succeeded),
                    EffectStatus::Complete,
                    Some(ObservationLevel::RemoteCompleted),
                ),
                Err(error) => (
                    json!({"status":"unknown","reason":format!("{error:#}")}),
                    None,
                    EffectStatus::Unknown,
                    None,
                ),
            };
        call.observe(
            &self.store,
            json!({
                "origin":input.origin,"protocol":value,"effect_scope":"public_metadata_read",
                "negotiation_attempted":false,"account_access_attempted":false,
                "business_operation_attempted":false,
            }),
            outcome,
            effect,
            level,
        )
    }
}

fn descriptor() -> NativeTaskDescriptor {
    NativeTaskDescriptor {
        kind: NAME.into(),
        definition_version: 1,
        implementation_ref: shenko_types::task::ImplementationRef {
            plugin_id: EntityId(uuid::Uuid::from_u128(0xd6ae8350c88a4132bd352bc8b7738eba)),
            artifact_digest: Digest::blake3(CONTRACT),
            registry_revision: 0,
        },
        role_revision: "native-child/1".into(),
        authority_refs: vec!["compat.poppy/v1".into()],
        prompt_section: Some(PROMPT.into()),
    }
}

struct Handler(Arc<PoppyRuntime>);

#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor()
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: NAME.into(),
            version: "1".into(),
            description: PROMPT.into(),
            usage: Some(
                "delegate:compat.poppy.discover:1 {\"origin\":\"https://merchant.example\"}".into(),
            ),
        }]
    }

    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = descriptor();
        vec![EffectContractBuilder::new(descriptor.implementation_ref.plugin_id,
            descriptor.implementation(), NAME, 1)
            .resource("public_company_metadata", "explicit_https_origin", ResourceAccess::Read)
            .action(EffectActionKind::Cancel)
            .idempotency(IdempotencyGuarantee::None)
            .external_effect(ExternalEffect {
                kind: "public_metadata_get".into(),
                description: "Bounded public HTTPS metadata reads; remote servers may log requests; no account or business action".into(),
                reversible: false, compensation_possible: false,
            })
            .cancel(CancelContract {
                can_cancel_before_start: true, can_cancel_in_flight: true,
                cancel_confirms_stopped: false, may_have_partial_effect: true,
            })
            .checkpoint(CheckpointContract {
                capturable: vec!["frozen origin and original metadata observation".into()],
                excluded: vec!["remote logs and unobserved HTTP responses".into()],
                coordinated: false, consistency: "local ledger only".into(),
            })
            .lifecycle(LifecycleContract {
                on_upgrade: "retire owned reads; retain original observations without replay".into(),
                on_uninstall: "withdraw the handler, cancel and join owned reads".into(),
                on_crash: "reconcile original receipts; missing completion stays unknown".into(),
                receipt_readable_after_removal: true,
            })
            .basis(GuaranteeBasis::ClaimOnly)
            .build()]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(self.0.workers.spawn(self.0.clone().run(invocation))?).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        reconcile_original(&self.0.store, task, operation_id, NAME)
    }
}

pub struct PoppyPlugin { pub data_dir: std::path::PathBuf }

impl Plugin for PoppyPlugin {
    type Config = AdapterHost;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-compat-poppy")
    }
    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        watch_installed(ctx, config.clone(), UNIT_ID, PoppyGeneration { data_dir: self.data_dir.clone() }, |_| async {
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)
    }
}

#[derive(Clone)]
struct PoppyGeneration { data_dir: std::path::PathBuf }

impl Plugin for PoppyGeneration {
    type Config = (Arc<Store>, Arc<Mutex<Node>>);
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-compat-poppy-generation")
    }
    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let runtime = Arc::new(PoppyRuntime {
            store: config.0.clone(),
            node: config.1.clone(),
            workers: Workers::default(),
        });
        let cleanup = runtime.clone();
        ctx.effect(move || async move {
            cleanup.workers.close().await;
        })
        .map_err(std::io::Error::other)?;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(Handler(runtime));
        NativeTaskHandlerPlugin.apply(ctx.clone(), &handler).await?;
        operational::install(ctx, config.0.clone(), config.1.clone(), self.data_dir.clone()).await
    }
}

// One assembly-owned reader serves every active producer. Its lifetime never
// depends on which adapter was enabled first; no duplicate native registration
// or registry-presence check becomes a second installation authority.
pub struct SharedArtifactPlugin;

impl Plugin for SharedArtifactPlugin {
    type Config = AdapterHost;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-compat-shared-artifact-owner")
    }
    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        const PRODUCERS: &[&str] = &[
            UNIT_ID,
            #[cfg(feature = "compat-mcp")]
            shenko_compat_mcp::UNIT_ID,
        ];
        shenko_compat::runtime::watch_any_installed(
            ctx,
            config.clone(),
            PRODUCERS,
            ArtifactGeneration,
            |_| async { Ok(()) },
        )
        .await
        .map_err(std::io::Error::other)
    }
}

#[derive(Clone)]
struct ArtifactGeneration;

impl Plugin for ArtifactGeneration {
    type Config = (Arc<Store>, Arc<Mutex<Node>>);
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-compat-shared-artifact-generation")
    }
    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        shenko_compat::artifact::ArtifactReadPlugin
            .apply(ctx, &config.0)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_compat::installation::plugin_export;
    use shenko_kernel::Supervisor;
    use shenko_node::UnitRequest;
    use shenko_subject::NativeChildPlugin;
    use shenko_types::ids::RuntimeGeneration;

    // One real installation lifecycle, including writes through another Node
    // connection. No model or remote service is substituted by this fixture.
    #[test]
    fn installed_discovery_is_withdrawn_and_republished() -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(installed_lifecycle())
    }

    async fn installed_lifecycle() -> Result<()> {
        let root = std::env::temp_dir().join(format!("shenko-poppy-{}", EntityId::new()));
        std::fs::create_dir_all(&root)?;
        let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
        let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
        node.lock().handle_unit_request(UnitRequest::Import {
            export: plugin_export(UNIT_ID, false)?,
            package_root: root.join("units").display().to_string(),
            enable: Some(false),
        })?;
        #[cfg(feature = "compat-mcp")]
        node.lock().handle_unit_request(UnitRequest::Import {
            export: plugin_export(shenko_compat_mcp::UNIT_ID, false)?,
            package_root: root.join("units").display().to_string(),
            enable: Some(false),
        })?;
        let supervisor = Supervisor::new(RuntimeGeneration::default());
        let outcome: Result<()> = async {
            supervisor.publish_services().await?;
            supervisor
                .spawn(
                    NativeChildPlugin,
                    (store.clone(), RuntimeGeneration::default()),
                )
                .await?;
            crate::compat::spawn_adapters(&supervisor, store, node, &root).await?;
            let native = supervisor.context().try_service::<NativeChildService>()?;
            ensure!(
                !native.has_handler(NAME, 1),
                "disabled installation published discovery"
            );
            let mut editor = Node::open(&root.join("node"))?;
            #[cfg(feature = "compat-mcp")]
            {
                editor.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: shenko_compat_mcp::UNIT_ID.into(),
                    enabled: true,
                })?;
                wait_for(&native, "compat.mcp.request", true).await?;
                wait_for(&native, shenko_compat::artifact::NAME, true).await?;
            }
            editor.handle_unit_request(UnitRequest::SetEnabled {
                unit_id: UNIT_ID.into(),
                enabled: true,
            })?;
            wait_for(&native, NAME, true).await?;
            wait_for(&native, shenko_compat::artifact::NAME, true).await?;
            #[cfg(feature = "compat-mcp")]
            {
                editor.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: shenko_compat_mcp::UNIT_ID.into(),
                    enabled: false,
                })?;
                wait_for(&native, "compat.mcp.request", false).await?;
                ensure!(
                    native.has_handler(NAME, 1)
                        && native.has_handler(shenko_compat::artifact::NAME, 1),
                    "MCP retirement withdrew Poppy's receipt reader"
                );
            }
            editor.handle_unit_request(UnitRequest::SetEnabled {
                unit_id: UNIT_ID.into(),
                enabled: false,
            })?;
            wait_for(&native, NAME, false).await?;
            wait_for(&native, shenko_compat::artifact::NAME, false).await?;
            editor.handle_unit_request(UnitRequest::SetEnabled {
                unit_id: UNIT_ID.into(),
                enabled: true,
            })?;
            wait_for(&native, NAME, true).await?;
            wait_for(&native, shenko_compat::artifact::NAME, true).await?;
            #[cfg(feature = "compat-mcp")]
            {
                editor.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: shenko_compat_mcp::UNIT_ID.into(),
                    enabled: true,
                })?;
                wait_for(&native, "compat.mcp.request", true).await?;
                editor.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: UNIT_ID.into(),
                    enabled: false,
                })?;
                wait_for(&native, NAME, false).await?;
                ensure!(
                    native.has_handler("compat.mcp.request", 1)
                        && native.has_handler(shenko_compat::artifact::NAME, 1),
                    "Poppy retirement withdrew MCP's receipt reader"
                );
                editor.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: shenko_compat_mcp::UNIT_ID.into(),
                    enabled: false,
                })?;
                wait_for(&native, "compat.mcp.request", false).await?;
                wait_for(&native, shenko_compat::artifact::NAME, false).await?;
            }
            Ok(())
        }
        .await;
        // Always dispose and join the watcher, including a failed assertion.
        let shutdown = supervisor.stop().await;
        drop(supervisor);
        let cleanup = std::fs::remove_dir_all(&root);
        outcome?;
        shutdown?;
        cleanup?;
        Ok(())
    }

    async fn wait_for(native: &NativeChildService, name: &str, enabled: bool) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while native.has_handler(name, 1) != enabled {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        Ok(())
    }
}
