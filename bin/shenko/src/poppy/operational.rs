//! Operational Poppy capabilities use the existing native task/effect ledger.
#[cfg(test)]
mod tests;
use std::{borrow::Cow, convert::Infallible, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::{
    installation::{UnitRef, plugin_enabled},
    recorded::{RecordedCall, Workers, joined, reconcile_original},
};
use shenko_compat_poppy::UNIT_ID;
use shenko_exec_host::BrowserProvider;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_node::Node;
use shenko_store::{Store, child::NativeDispatchTransition};
use shenko_subject::NativeTaskHandlerPlugin;
use shenko_types::{
    effect::*,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::EntityId,
    object::{Digest, ObjectRef},
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::TaskRecord,
};

use super::{
    account::{self, AccountAction},
    commerce::{self, CommerceAction},
    connection::Connection,
    transport::{Authentication, CompanyClient, validate_public_header},
    vault::Vault,
};

pub(super) struct BrowserAccess(pub Arc<BrowserProvider>);
impl Service for BrowserAccess {
    const NAME: &'static str = "shenko.host.browser-access";
}
pub(crate) struct BrowserAccessPlugin;
impl Plugin for BrowserAccessPlugin {
    type Config = Arc<BrowserProvider>;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("shenko-host-browser-access")
    }
    fn inject(&self) -> InjectSpec {
        InjectSpec::none()
    }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }
    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx
            .provide(Arc::new(BrowserAccess(config.clone())))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Account,
    Commerce,
    Request,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Account => "compat.poppy.account",
            Self::Commerce => "compat.poppy.commerce",
            Self::Request => "compat.poppy.request",
        }
    }
    fn prompt(self) -> &'static str {
        match self {
            Self::Account => {
                "Poppy account and browser Session control. Input {connection:{unit_id,revision,artifact_digest},authentication?:{mode:'signed_out'|'account',resource?},timeout_secs?,account:{action,...}}. Actions: status; begin_direct{redirect_uri,scopes}; complete_direct{callback_ref}; begin_device{scopes}; poll_device; refresh{resource,scopes?}; revoke; join_browser{session_id,tab_id?,lease_token,controller_epoch}. Sign-in occurs on the company's page; callback references and tokens are private node-local state. A pending or unknown credential rotation preserves the original grant and is not replayed. Browser join uses the same owned browser and its live lease, and never exposes its code. Returned scopes, account binding and website unscoped declaration describe actual access; discovery does not authorize account actions."
            }
            Self::Commerce => {
                "Use negotiated UCP commerce through a Poppy company connection. Input {connection:{unit_id,revision,artifact_digest},authentication?:{mode:'signed_out'|'account',resource?},timeout_secs?,commerce:{action,...}}. Actions: negotiate; request{operation,transport?,id?,payload?,idempotency_key?,checkout_observation?,payment_credential_ref?,buyer_ip?}; reconcile{operation_id}. Supported operations are catalog search/lookup, cart create/get/update, checkout create/get/update/complete/cancel and order get. Mutations require an original UUID idempotency key. Completing checkout binds the prior observed checkout operation, its terms and account, and an opaque node-local instrument reference; observe a returned continuation URL in the user's browser when needed. Reconciliation reads original IDs without resending an unknown mutation. Capability intersection and request acceptance do not prove a purchase; retain actual checkout/order observations and any unknown outcome."
            }
            Self::Request => {
                "Access a company's declared MCP/OpenAPI resource or read a website page with a fresh signed Poppy Session. Input {connection:{unit_id,revision,artifact_digest},authentication?:{mode:'signed_out'|'account',resource?},timeout_secs?,request:{action,...}}. Actions: mcp{endpoint,request:<MCP request>}; api{description_url,method,url,headers?,body?}; page{url}. OAuth endpoints and schema documents never receive a Poppy Session. MCP account tokens must name the exact endpoint resource and its verified issuer. Non-browser page reads are signed out. Results are external observations and protocol receipts, not authority or evidence that a business goal succeeded. An unknown original request is never automatically repeated."
            }
        }
    }
    fn descriptor(self) -> NativeTaskDescriptor {
        let request_contract = match self {
            Self::Commerce => "; immutable original service/OAuth resource and instrument digest; protected final serialized request/RPC ID/Request-Id sealed before mutation send",
            Self::Account | Self::Request => "",
        };
        let contract = format!(
            "shenko.{}:1; installed Node connection; node-local secrets; frozen operator/company/user/session/grant/input; T1 then owned dispatch marker; per-send authority guard; actual observations; no automatic mutation replay; private browser challenge stays outside ledger{request_contract}",
            self.name()
        );
        NativeTaskDescriptor {
            kind: self.name().into(),
            definition_version: 1,
            implementation_ref: shenko_types::task::ImplementationRef {
                plugin_id: EntityId(uuid::Uuid::from_u128(0xd6ae8350c88a4132bd352bc8b7738eba)),
                artifact_digest: Digest::blake3(contract.as_bytes()),
                registry_revision: 0,
            },
            role_revision: "native-child/1".into(),
            authority_refs: vec!["compat.poppy/v1".into()],
            prompt_section: Some(self.prompt().into()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    connection: UnitRef,
    #[serde(default)]
    authentication: Authentication,
    #[serde(default = "super::default_timeout")]
    timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<AccountAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commerce: Option<CommerceAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<ApiAction>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ApiAction {
    Mcp {
        endpoint: String,
        request: Value,
    },
    Api {
        description_url: String,
        method: String,
        url: String,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        body: Option<Value>,
    },
    Page {
        url: String,
    },
}

struct Runtime {
    store: Arc<Store>,
    node: Arc<Mutex<Node>>,
    data_dir: PathBuf,
    browser: Option<Arc<BrowserProvider>>,
    workers: Workers,
}

pub(super) async fn install(
    ctx: Context,
    store: Arc<Store>,
    node: Arc<Mutex<Node>>,
    data_dir: PathBuf,
) -> Result<(), std::io::Error> {
    let browser = ctx
        .try_service::<BrowserAccess>()
        .ok()
        .map(|service| service.0.clone());
    let runtime = Arc::new(Runtime {
        store,
        node,
        data_dir,
        browser,
        workers: Workers::default(),
    });
    let cleanup = runtime.clone();
    ctx.effect(move || async move {
        cleanup.workers.close().await;
    })
    .map_err(std::io::Error::other)?;
    for kind in [Kind::Account, Kind::Commerce, Kind::Request] {
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(Handler {
            runtime: runtime.clone(),
            kind,
        });
        NativeTaskHandlerPlugin.apply(ctx.clone(), &handler).await?;
    }
    Ok(())
}

impl Runtime {
    async fn run(
        self: Arc<Self>,
        kind: Kind,
        mut invocation: NativeTaskInvocation,
    ) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == kind.name() && invocation.task.definition_version == 1,
            "Poppy task identity mismatch"
        );
        ensure!(
            serde_json::to_vec(&invocation.input)?.len() <= 128 * 1024,
            "Poppy task input exceeds 128 KiB"
        );
        let input: Input = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        ensure!(
            (1..=120).contains(&input.timeout_secs),
            "Poppy timeout must be 1..=120 seconds"
        );
        ensure!(
            match kind {
                Kind::Account =>
                    input.account.is_some() && input.commerce.is_none() && input.request.is_none(),
                Kind::Commerce =>
                    input.commerce.is_some() && input.account.is_none() && input.request.is_none(),
                Kind::Request =>
                    input.request.is_some() && input.account.is_none() && input.commerce.is_none(),
            },
            "exactly this handler's action is required"
        );
        if let Some(action) = &input.commerce {
            action.validate()?;
        }
        if let Some(AccountAction::CompleteDirect { callback_ref }) = &input.account {
            uuid::Uuid::parse_str(
                callback_ref
                    .strip_prefix("oauth-callback-")
                    .context("callback_ref must be an imported oauth-callback-<UUID> handle")?,
            )?;
        }
        if let Some(ApiAction::Api { headers, .. }) = &input.request {
            for name in headers.keys() {
                validate_public_header(name)?;
            }
        }
        let connection = Connection::read(&self.node.lock(), &input.connection)?;
        // Account control loads and fences the exact stored grant itself. An
        // expired access token must not prevent refresh, sign-out or inspection.
        let authentication = if input
            .account
            .as_ref()
            .is_some_and(|action| !matches!(action, AccountAction::JoinBrowser { .. }))
        {
            &Authentication::SignedOut
        } else {
            &input.authentication
        };
        let client = CompanyClient::new(
            Vault::open(&self.data_dir, false)?,
            connection,
            authentication,
            Duration::from_secs(input.timeout_secs),
        )?;
        let mut binding = json!({"unit_id":UNIT_ID,"connection":input.connection,
            "company":client.connection().report.company_identifier,"user_id":client.connection().user_id,
            "session_id":client.session_id(),"session_extensions":client.session_extensions(),"client_id":client.client_id(),
            "account_binding":client.account_binding(),"platform_profile":client.profile().url()});
        if input.account.is_some() {
            binding["account_state"] = account::binding_snapshot(&client)?;
        }
        if let Some(action) = &input.commerce {
            binding["service_binding"] = commerce::action_service_binding(&self.store, &client, action)?;
            if let CommerceAction::Request(request) = action
                && let Some(reference) = &request.payment_credential_ref
            {
                binding["payment_credential"] = serde_json::to_value(client.instrument_reference(
                    reference, &commerce::service_binding(&client, request.transport)?,
                )?)?;
            }
        }
        let mut call = RecordedCall::prepare(
            &self.store,
            &invocation,
            &kind.descriptor(),
            &input,
            binding,
            "actual protocol and account-bound resource observations only; capability negotiation and transport ACK are not business completion",
            "preserve original frozen input, identifiers, terms, grant and idempotency key; never resend an armed mutation; reconcile original IDs using a separate read task",
        )?;
        if let Some(existing) = call.arm(&self.store, &invocation)? {
            apply_reconciliation(&self.store, &existing)?;
            return Ok(existing);
        }
        match self.store.commit_native_dispatch(
            &invocation.task_invocation,
            call.operation.operation_id,
            call.attempt.attempt_id,
        )? {
            NativeDispatchTransition::AlreadyCommitted => {
                return self.store.external_operation(call.operation.operation_id);
            }
            NativeDispatchTransition::Fenced => {
                return call.observe(
                    &self.store,
                    json!({"status":"cancelled_before_dispatch","request_dispatched":false}),
                    Some(OperationOutcome::Cancelled),
                    EffectStatus::None,
                    None,
                );
            }
            NativeDispatchTransition::Won => {}
        }
        let owner = self.clone();
        let context = invocation.task_invocation.clone();
        let frozen_connection = input.connection.clone();
        let cancellation = self.workers.stop.child_token();
        let send_cancellation = cancellation.clone();
        let client = client
            .with_cancellation(cancellation.clone())
            .with_guard(Arc::new(move || {
                ensure!(
                    !send_cancellation.is_cancelled()
                        && !owner.workers.stop.is_cancelled()
                        && plugin_enabled(&owner.node.lock(), UNIT_ID)?
                        && owner.store.native_invocation_current(&context)?,
                    "Poppy operation lost its current dispatch authority"
                );
                Connection::read(&owner.node.lock(), &frozen_connection)?;
                Ok(())
            }));
        let client = if input.commerce.is_some() {
            client.with_commerce_call(self.store.clone(), &call)?
        } else {
            client
        };
        let work = async {
            if let Some(action) = &input.account {
                account::execute(
                    &client,
                    action,
                    invocation.operation_id,
                    self.browser.as_ref(),
                    invocation.context.caller_instance_id,
                )
                .await
            } else if let Some(action) = &input.commerce {
                commerce::execute(&client, &self.store, &call, action, &cancellation).await
            } else {
                api(
                    &client,
                    input.request.as_ref().context("missing request")?,
                    &cancellation,
                )
                .await
            }
        };
        tokio::pin!(work);
        let deadline = tokio::time::sleep(Duration::from_secs(input.timeout_secs));
        tokio::pin!(deadline);
        let mut stop_reason = None;
        let observed = loop {
            tokio::select! { biased;
                _result = &mut work => break _result,
                _ = self.workers.stop.cancelled() => {
                    stop_reason = Some("Poppy plugin retired");
                    cancellation.cancel();
                    break work.as_mut().await;
                },
                _ = &mut deadline => {
                    stop_reason = Some("Poppy operation deadline reached");
                    cancellation.cancel();
                    break work.as_mut().await;
                },
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if !self.store.native_invocation_current(&invocation.task_invocation).unwrap_or(false)
                        || Connection::read(&self.node.lock(), &input.connection).is_err() {
                        stop_reason = Some("Poppy task lost its current authority");
                        cancellation.cancel();
                        break work.as_mut().await;
                    }
                }
            }
        };
        cancellation.cancel();
        let mut value = match observed {
            Ok(value) => value,
            Err(error) => json!({"status":"unknown","reason":format!("{error:#}"),
                "original_operation":invocation.operation_id,"original_input":call.operation.final_input_ref,
                "request_dispatched":null,"remote_effects_stopped":false}),
        };
        if let Some(reason) = stop_reason {
            // Retain any response actually observed during cooperative cleanup.
            // A deadline never proves that the merchant stopped its operation.
            value["stop_requested"] = reason.into();
            value["remote_effects_stopped"] = false.into();
        }
        if let Some(reference) = client.final_commerce_request() {
            value["final_request"] = reference;
        }
        if client.redact_observation(&mut value).is_err() {
            value = json!({"status":"unknown","reason":"external response contained a private credential; observation withheld",
                "original_operation":invocation.operation_id,"request_dispatched":null});
        }
        let (outcome, effect, level) = settlement(&value);
        let operation = call.observe(&self.store, value, outcome, effect, level)?;
        apply_reconciliation(&self.store, &operation)?;
        Ok(operation)
    }
}

fn settlement(
    value: &Value,
) -> (
    Option<OperationOutcome>,
    EffectStatus,
    Option<ObservationLevel>,
) {
    match value["status"].as_str() {
        Some("unknown") => (None, EffectStatus::Unknown, None),
        Some("rejected")
            if value["request_dispatched"] == false || value["effect_observation"] == "none" =>
        {
            (
                Some(OperationOutcome::Failed),
                EffectStatus::None,
                Some(ObservationLevel::RemoteCompleted),
            )
        }
        Some("rejected") => (
            None,
            EffectStatus::Unknown,
            Some(ObservationLevel::RemoteCompleted),
        ),
        _ if value["effect_observation"] == "unknown" => (
            None,
            EffectStatus::Unknown,
            Some(ObservationLevel::RemoteCompleted),
        ),
        Some("observed")
            if value["effect_observation"] == "none"
                || value["effect_observation"] == "resource_read" =>
        {
            (
                Some(OperationOutcome::Succeeded),
                EffectStatus::None,
                Some(ObservationLevel::RemoteCompleted),
            )
        }
        Some("observed") => (
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
            Some(ObservationLevel::RemoteCompleted),
        ),
        _ => (None, EffectStatus::Unknown, None),
    }
}

async fn api(
    client: &CompanyClient,
    action: &ApiAction,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Value> {
    match action {
        ApiAction::Mcp { endpoint, request } => {
            let request = serde_json::from_value(request.clone())?;
            let mut session = client.connect_mcp(endpoint, Default::default()).await?;
            let result = session
                .request(request, client.timeout(), cancellation)
                .await;
            let closed = session.close().await;
            let result = result?;
            let mut value = json!({"status":match &result {shenko_compat_mcp::protocol::Response::Unknown{..}=>"unknown", _=>"observed"},
                "protocol":result,"connection_closed":closed.is_ok(),"effect_observation":"unknown","business_outcome_verified":false});
            client.redact_observation(&mut value)?;
            Ok(value)
        }
        ApiAction::Api {
            description_url,
            method,
            url,
            headers,
            body,
        } => {
            ensure!(
                client
                    .connection()
                    .report
                    .capabilities
                    .apis
                    .iter()
                    .any(|api| api.api_type == "openapi" && api.url == *description_url),
                "OpenAPI document is not declared by this company"
            );
            let description = client.fetch_schema(description_url).await?;
            let mut request = reqwest::Request::new(
                reqwest::Method::from_bytes(method.as_bytes())?,
                url::Url::parse(url)?,
            );
            for (name, value) in headers {
                validate_public_header(name)?;
                request.headers_mut().insert(
                    reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                    reqwest::header::HeaderValue::from_str(value)?,
                );
            }
            if let Some(body) = body {
                request.headers_mut().insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/json"),
                );
                *request.body_mut() = Some(serde_json::to_vec(body)?.into());
            }
            let response = client
                .send_api(request, description_url, &description)
                .await?;
            Ok(http_value(
                response,
                method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD"),
            ))
        }
        ApiAction::Page { url } => Ok(http_value(client.read_website(url).await?, true)),
    }
}

fn http_value(response: super::transport::HttpObservation, read_only: bool) -> Value {
    let body = serde_json::from_slice::<Value>(&response.body)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&response.body)));
    json!({"status":"observed","http_status":response.status,"body":body,
        "content_type":response.headers.get("content-type").and_then(|v|v.to_str().ok()),
        "retry_after":response.headers.get("retry-after").and_then(|v|v.to_str().ok()),
        "www_authenticate":response.headers.get("www-authenticate").and_then(|v|v.to_str().ok()),
        "effect_observation":if read_only {"resource_read"} else {"unknown"},"business_outcome_verified":false})
}

struct Handler {
    runtime: Arc<Runtime>,
    kind: Kind,
}
#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        self.kind.descriptor()
    }
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: self.kind.name().into(),
            version: "1".into(),
            description: self.kind.prompt().into(),
            usage: Some(format!("delegate:{}:1", self.kind.name())),
        }]
    }
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.kind.descriptor();
        vec![EffectContractBuilder::new(descriptor.implementation_ref.plugin_id, descriptor.implementation(), self.kind.name(), 1)
            .resource("company_protocol_and_node_credentials", "frozen_connection_company_user_grant", ResourceAccess::ReadWrite)
            .action(EffectActionKind::Cancel).idempotency(IdempotencyGuarantee::None)
            .external_effect(ExternalEffect { kind:"company_protocol_request".into(),
                description:"Declared account, API, browser or commerce request; remote effects require actual receipts".into(),
                reversible:false,compensation_possible:false })
            .cancel(CancelContract {can_cancel_before_start:true,can_cancel_in_flight:true,cancel_confirms_stopped:false,may_have_partial_effect:true})
            .checkpoint(CheckpointContract {capturable:vec!["original company/user/grant binding, input, identifiers, terms and idempotency key".into()],
                excluded:vec!["node-local secrets, browser run state and unobserved remote effects".into()],coordinated:false,consistency:"local ledger and original merchant observations".into()})
            .lifecycle(LifecycleContract {on_upgrade:"retire and join owned requests; retain original uncertainty".into(),
                on_uninstall:"withdraw native capabilities and join workers".into(),on_crash:"read original receipts; separate identity-bound queries reconcile mutations without replay".into(),
                receipt_readable_after_removal:true}).basis(GuaranteeBasis::ClaimOnly).build()]
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(
            self.runtime
                .workers
                .spawn(self.runtime.clone().run(self.kind, invocation))?,
        )
        .await
    }
    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation =
            reconcile_original(&self.runtime.store, task, operation_id, self.kind.name())?;
        apply_reconciliation(&self.runtime.store, &operation)?;
        Ok(operation)
    }
}

/// A separate read may establish the original completion's postcondition.
/// Repair this local fold from its immutable receipt after a controller crash;
/// it never performs another network request or changes the original input.
fn apply_reconciliation(store: &Store, recovery: &OperationRecord) -> Result<()> {
    if recovery.operation_name != Kind::Commerce.name()
        || recovery.outcome != Some(OperationOutcome::Succeeded)
    {
        return Ok(());
    }
    let input = read_json(store, &recovery.final_input_ref)?;
    if input.pointer("/commerce/action").and_then(Value::as_str) != Some("reconcile") {
        return Ok(());
    }
    let original_id: EntityId = serde_json::from_value(input["commerce"]["operation_id"].clone())?;
    let attempt = store.external_operation_attempt(recovery.operation_id)?;
    let receipt = store.external_evidence(
        attempt
            .final_receipt_id
            .context("recovery observation is missing")?,
    )?;
    ensure!(
        receipt.operation_id == Some(recovery.operation_id)
            && receipt.attempt_id == Some(attempt.attempt_id)
            && receipt.source_id == Kind::Commerce.name()
            && receipt.decode_status == DecodeStatus::Decoded,
        "recovery receipt identity mismatch"
    );
    let proof = read_json(store, &receipt.raw_ref)?;
    if proof["original_effect_resolved"] != true {
        return Ok(());
    }
    ensure!(
        proof["commerce_schema"] == 1
            && proof["status"] == "observed"
            && proof["reconciles_operation"] == json!(original_id)
            && proof["original_terms_match"] == true
            && proof["original_request_replayed"] == false
            && proof["original_effect_observation"]["level"] == "postcondition_verified",
        "incomplete original completion proof"
    );
    ensure!(
        proof["resource"]["status"] == "completed"
            && proof["resource"]["id"] == proof["original_effect_observation"]["checkout_id"]
            && proof["resource"]["order"]["id"] == proof["original_effect_observation"]["order_id"],
        "recovery proof differs from the actual merchant resource"
    );
    let original = store.external_operation(original_id)?;
    ensure!(
        original.operation_name == Kind::Commerce.name(),
        "recovered operation is not Poppy commerce"
    );
    let frozen = read_json(store, &original.final_input_ref)?;
    ensure!(
        frozen["commerce"]["operation"] == "complete_checkout"
            && frozen["commerce"]["id"] == proof["original_effect_observation"]["checkout_id"]
            && frozen["commerce"]["idempotency_key"] == proof["original_idempotency_key"]
            && proof["original_effect_observation"]["order_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
        "recovery proof does not match the original completion"
    );
    let bindings = read_json(store, &original.resource_bindings_ref)?;
    for field in [
        "company",
        "user_id",
        "client_id",
        "account_binding",
        "platform_profile",
        "service_binding",
    ] {
        ensure!(
            bindings["extra"][field] == proof[field],
            "recovery changed the original {field} binding"
        );
    }
    ensure!(
        bindings["extra"].get("service_binding").is_some()
            && !bindings["extra"]["service_binding"].is_null()
            && proof["original_final_request"]
                == super::transport::commerce_request_reference(store, &original)?,
        "recovery lacks the original immutable service and final request proof"
    );
    let original_attempt = store.external_operation_attempt(original_id)?;
    store.record_external_evidence(
        original_id,
        original_attempt.attempt_id,
        EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(original_id),
            attempt_id: Some(original_attempt.attempt_id),
            source_id: "compat.poppy.commerce.recovery".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(receipt.receipt_id.to_string()),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: receipt.raw_ref,
            decoder_ref: receipt.decoder_ref,
            decode_status: DecodeStatus::Decoded,
            observation_level: Some(ObservationLevel::PostconditionVerified),
            normalized_ref: receipt.normalized_ref,
            observed_at: receipt.observed_at,
            received_commit: original.prepared_commit,
        },
        Some(OperationOutcome::Succeeded),
        EffectStatus::Complete,
    )?;
    Ok(())
}

fn read_json(store: &Store, reference: &ObjectRef) -> Result<Value> {
    commerce::read_observation(store, reference)
}
