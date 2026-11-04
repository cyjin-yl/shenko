//! MCP alone owns these connections, workers, native definition and lifetime.
use std::{borrow::Cow, collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::{installation::UnitRef, recorded::{RecordedCall, Workers, joined, reconcile_original}};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeChildService, NativeTaskDescriptor, NativeTaskHandler, NativeTaskHandlerPlugin, NativeTaskInvocation};
use shenko_types::{effect::*, evidence::ObservationLevel, ids::EntityId, object::Digest, operation::*,
    plugin_runtime::NativeCapabilityDeclaration, task::TaskRecord};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use crate::{McpConnection, UNIT_ID, protocol::{Response, Session}};

const NAME: &str = "compat.mcp.request";
const CONTRACT: &[u8] = b"shenko.compat.mcp/v1: exact installed profile; T1 before connection/request; effect scope is one protocol exchange, completed only by its observed response; remote business effects and pending server tasks are not settled here; missing response remains unknown; no replay; close and join owned sessions on retirement";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    unit: UnitRef,
    request: Value,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}
fn default_timeout() -> u64 { 60 }

type ConnectionKey = (String, String, String);
struct Connection { session: RwLock<Option<Session>>, stop: CancellationToken }

pub struct McpRuntime {
    store: Arc<Store>, node: Arc<Mutex<Node>>, workers: Workers,
    connections: Mutex<BTreeMap<ConnectionKey, Arc<Connection>>>,
}
impl Service for McpRuntime { const NAME: &'static str = "shenko.compat.mcp.runtime"; }

impl McpRuntime {
    async fn request(&self, input: &Input, task_id: EntityId) -> Result<(Value, Response)> {
        let (fact, bytes) = input.unit.installed(&self.node.lock(), UNIT_ID)?;
        ensure!(fact.descriptor.config["compat"]["format"] == "mcp-connection", "unit is not an MCP connection");
        let profile: McpConnection = serde_json::from_slice(&bytes)?;
        let request = serde_json::from_value(input.request.clone())?;
        let key = (input.unit.unit_id.clone(), input.unit.revision.clone(), input.unit.artifact_digest.0.clone());
        let connection = self.connections.lock().entry(key.clone()).or_insert_with(|| Arc::new(Connection {
            session: RwLock::new(None), stop: self.workers.stop.child_token(),
        })).clone();
        // Single-flight opening per profile; established requests share a read
        // lease and can run concurrently in the same real MCP session.
        if connection.session.read().await.is_none() {
            let mut slot = connection.session.write().await;
            if slot.is_none() {
                ensure!(!connection.stop.is_cancelled(), "MCP connection is retiring");
                *slot = Some(Session::connect(&profile.connection, Duration::from_secs(input.timeout_secs.min(30))).await?);
            }
        }
        let cancellation = connection.stop.child_token();
        let server;
        let result = {
            let slot = connection.session.read().await;
            let session = slot.as_ref().context("MCP connection was retired")?;
            // Recheck a durable abort after the handshake: a cancellation
            // committed while Session::connect awaited negotiation must not
            // still dispatch tools/call once the read lease is held.
            if self.store.external_task(task_id)?.abort_request.is_some() || connection.stop.is_cancelled() {
                return Ok((session.server_info()?, Response::Unknown { reason: "cancelled_before_request_dispatch" }));
            }
            server = session.server_info()?;
            let request = session.request(request, Duration::from_secs(input.timeout_secs), &cancellation);
            tokio::pin!(request);
            loop {
                tokio::select! {
                    result = &mut request => break result,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        match self.store.external_task(task_id) {
                            Ok(task) if task.abort_request.is_some() => cancellation.cancel(),
                            Ok(_) => {},
                            Err(error) => { cancellation.cancel(); let _ = (&mut request).await; break Err(error); }
                        }
                    }
                }
            }
        };
        if matches!(&result, Err(_) | Ok(Response::Unknown { .. })) {
            connection.stop.cancel();
            let mut slot = connection.session.write().await;
            let cleanup = if let Some(mut session) = slot.take() { session.close().await } else { Ok(()) };
            let mut connections = self.connections.lock();
            if connections.get(&key).is_some_and(|current| Arc::ptr_eq(current, &connection)) { connections.remove(&key); }
            match (result, cleanup) {
                (Err(error), Err(cleanup)) => Err(error.context(format!("MCP retirement also failed: {cleanup}"))),
                (Err(error), _) => Err(error), (_, Err(error)) => Err(error),
                (Ok(response), Ok(())) => Ok((server, response)),
            }
        } else { result.map(|response| (server, response)) }
    }

    async fn run(self: Arc<Self>, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(invocation.task.kind == NAME && invocation.task.definition_version == 1, "MCP task identity mismatch");
        ensure!(serde_json::to_vec(&invocation.input)?.len() <= 64 * 1024, "MCP request exceeds 64 KiB");
        let input: Input = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        ensure!((1..=600).contains(&input.timeout_secs), "timeout must be 1..=600 seconds");
        input.unit.installed(&self.node.lock(), UNIT_ID)?;
        // Reject malformed protocol input before arming, without connecting.
        let _: rmcp::model::ClientRequest = serde_json::from_value(input.request.clone())?;
        let mut call = RecordedCall::prepare(&self.store, &invocation, &descriptor(), &input, serde_json::to_value(&input.unit)?,
            "observed MCP response or explicit unknown; isError remains failure; no claim of remote business success",
            "an armed MCP request is never automatically repeated, including after controller replacement")?;
        if let Some(existing) = call.arm(&self.store, &invocation)? { return Ok(existing); }
        if self.workers.stop.is_cancelled() || self.store.external_task(invocation.task.task_id)?.abort_request.is_some() {
            return call.observe(&self.store, json!({"source":input.unit,"status":"cancelled_before_dispatch"}),
                Some(OperationOutcome::Cancelled), EffectStatus::None, None);
        }
        let (value, outcome, level) = match self.request(&input, invocation.task.task_id).await {
            Ok((server, Response::Received { result })) => {
                let outcome = if result.get("isError").and_then(Value::as_bool) == Some(true) { OperationOutcome::Failed } else { OperationOutcome::Succeeded };
                (json!({"status":"received","result":result,"server":server}), Some(outcome), Some(ObservationLevel::RemoteCompleted))
            }
            Ok((server, Response::Rejected { error })) => (json!({"status":"rejected","error":error,"server":server}), Some(OperationOutcome::Failed), Some(ObservationLevel::RemoteCompleted)),
            Ok((server, Response::Unknown { reason: "cancelled_before_request_dispatch" })) => (json!({"status":"cancelled_before_dispatch","server":server}), Some(OperationOutcome::Cancelled), None),
            Ok((server, Response::Unknown { reason })) => (json!({"status":"unknown","reason":reason,"server":server}), None, None),
            Err(error) => (json!({"status":"unknown","reason":error.to_string()}), None, None),
        };
        // This operation owns the request/response exchange, not the remote
        // business goal. A response completes that exchange even if isError
        // reports failure or the result describes an ongoing server task.
        // Absence of a response never becomes a completed exchange.
        let effect = match outcome {
            Some(OperationOutcome::Cancelled) => EffectStatus::None,
            Some(_) => EffectStatus::Complete,
            None => EffectStatus::Unknown,
        };
        call.observe(&self.store, json!({"source":input.unit,"protocol":value,
            "effect_scope":"protocol_exchange","remote_business_success_proven":false,
            "remote_business_effect_status":"not_independently_verified"}), outcome, effect, level)
    }

    /// Reconcile idle as well as active connections against Node authority.
    /// Read failure retires the affected connection instead of keeping it live.
    pub async fn reconcile_connections(&self) -> Result<()> {
        let mut error = None;
        let mut retired = Vec::new();
        {
            let node = self.node.lock();
            self.connections.lock().retain(|(id, revision, digest), connection| {
                let keep = match node.installed_unit_fact(id) {
                    Ok(Some(fact)) => fact.enabled && fact.revision == *revision && fact.artifact_digest.0 == *digest,
                    Ok(None) => false,
                    Err(failure) => { if error.is_none() { error = Some(failure); } false },
                };
                if !keep { connection.stop.cancel(); retired.push(connection.clone()); }
                keep
            });
        }
        for connection in retired {
            if let Some(mut session) = connection.session.write().await.take() {
                if let Err(failure) = session.close().await { if error.is_none() { error = Some(failure); } }
            }
        }
        match error { Some(error) => Err(error), None => Ok(()) }
    }

    async fn retire(&self) -> Result<()> {
        self.workers.close().await;
        let connections = std::mem::take(&mut *self.connections.lock());
        let mut errors = Vec::new();
        for connection in connections.into_values() {
            connection.stop.cancel();
            if let Some(mut session) = connection.session.write().await.take() {
                if let Err(error) = session.close().await { errors.push(error.to_string()); }
            }
        }
        ensure!(errors.is_empty(), "MCP retirement failed: {}", errors.join("; "));
        Ok(())
    }
}

fn descriptor() -> NativeTaskDescriptor {
    NativeTaskDescriptor { kind: NAME.into(), definition_version: 1,
        implementation_ref: shenko_types::task::ImplementationRef {
            plugin_id: "485e5654-274e-490b-939a-ab1255864246".parse().expect("fixed MCP plugin identity"),
            artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 },
        role_revision: "native-child/1".into(), authority_refs: vec!["compat.mcp/v1".into()],
        prompt_section: Some("Optional MCP adapter, not a bundled external service. Input {unit:{unit_id,revision,artifact_digest},request:{method,params},timeout_secs?}. One request on the exact installed connection; pagination and follow-ups are explicit. Results retain server metadata and unknown completion. A receipt's learning_source can support a separate, correctable cognition unit; it does not authorize adoption or global rules.".into()) }
}

struct Handler(Arc<McpRuntime>);
#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor { descriptor() }
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration { capability_id: NAME.into(), version: "1".into(), description: descriptor().prompt_section.unwrap(),
            usage: Some("delegate:compat.mcp.request:1 {\"unit\":{\"unit_id\":\"...\",\"revision\":\"...\",\"artifact_digest\":\"blake3:...\"},\"request\":{\"method\":\"tools/list\",\"params\":{}}}".into()) }]
    }
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = descriptor();
        vec![EffectContractBuilder::new(descriptor.implementation_ref.plugin_id, descriptor.implementation(), NAME, 1)
            .resource("external_mcp_service", "exact_installed_profile", ResourceAccess::Execute)
            .action(EffectActionKind::Cancel).idempotency(IdempotencyGuarantee::None)
            .external_effect(ExternalEffect { kind: "mcp_protocol_request".into(), description: "May start an explicitly configured stdio server and invoke external capabilities; tool annotations are not effect guarantees".into(), reversible: false, compensation_possible: false })
            .cancel(CancelContract { can_cancel_before_start: true, can_cancel_in_flight: true, cancel_confirms_stopped: false, may_have_partial_effect: true })
            .checkpoint(CheckpointContract { capturable: vec!["frozen profile identity and observed protocol receipts".into()], excluded: vec!["remote effects and live process/session state".into()], coordinated: false, consistency: "local ledger only".into() })
            .lifecycle(LifecycleContract { on_upgrade: "retire old connections; never replay armed requests".into(), on_uninstall: "withdraw native definition, cancel and join owned workers and sessions; remote effects may remain".into(), on_crash: "reconcile original receipts; unknown remains unknown".into(), receipt_readable_after_removal: true })
            .basis(GuaranteeBasis::ClaimOnly).build()]
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(self.0.workers.spawn(self.0.clone().run(invocation))?).await
    }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_original(&self.0.store, task, operation_id, NAME)
    }
}

pub struct McpPlugin;
impl Plugin for McpPlugin {
    type Config = shenko_compat::runtime::AdapterHost;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-mcp") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        shenko_compat::runtime::watch_installed(ctx, config.clone(), UNIT_ID, McpGeneration, |ctx| async move {
            ctx.try_service::<McpRuntime>()?.reconcile_connections().await
        }).await.map_err(std::io::Error::other)
    }
}

#[derive(Clone)]
struct McpGeneration;
impl Plugin for McpGeneration {
    type Config = (Arc<Store>, Arc<Mutex<Node>>);
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-mcp-generation") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let runtime = Arc::new(McpRuntime { store: config.0.clone(), node: config.1.clone(), workers: Workers::default(), connections: Mutex::new(BTreeMap::new()) });
        let cleanup = runtime.clone();
        // Native fallible cleanup retains failure at Cordis's disposal
        // boundary. Logging and returning () would attest false success.
        ctx.effect(move || async move {
            cleanup.retire().await.map_err(std::io::Error::other)
        })
        .map_err(std::io::Error::other)?;
        let _published = ctx.provide(runtime.clone()).map_err(std::io::Error::other)?;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(Handler(runtime));
        NativeTaskHandlerPlugin.apply(ctx.clone(), &handler).await?;
        install_artifact_reader(ctx.clone(), config.0.clone()).await?;
        install_usage_digest(ctx, config.0.clone()).await
    }
}

/// Publishes the shared bounded CAS reader alongside the MCP definition so a
/// `learning_source` digest is actually retrievable while this plugin runs.
pub(crate) async fn install_artifact_reader(ctx: Context, store: Arc<Store>) -> Result<(), std::io::Error> {
    shenko_compat::artifact::ArtifactReadPlugin.apply(ctx, &store).await
}

/// Publishes the bounded observation digest with the same generation so MCP
/// usage evidence can be summarized while this plugin runs.
pub(crate) async fn install_usage_digest(ctx: Context, store: Arc<Store>) -> Result<(), std::io::Error> {
    shenko_compat::usage::UsageDigestPlugin.apply(ctx, &store).await
}
