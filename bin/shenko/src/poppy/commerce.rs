//! Assembly-owned UCP execution. The native handler freezes and arms its task
//! before calling this module; this module never replays an armed mutation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::recorded::RecordedCall;
use shenko_compat_mcp::protocol::{Response, Session};
use shenko_compat_poppy::ucp::{
    CheckoutObservation, CheckoutStatus, CompletionBinding, NegotiatedProfile, Operation,
    RequestPlan, RestSchemas, ToolSchemas, Transport, referenced_schema_documents,
};
use shenko_store::Store;
use shenko_types::{
    evidence::{DecodeStatus, ObservationLevel},
    ids::EntityId,
    object::{Digest, ObjectRef},
};
use tokio_util::sync::CancellationToken;

use super::transport::{CompanyClient, InstrumentRef};

const HANDLER: &str = "compat.poppy.commerce";
const MAX_PRIOR_BYTES: u64 = 2 * 1024 * 1024;

/// A checkout ID is scoped to the original advertised service, independently
/// of a refreshable Session or access token. Preserve literal OAuth resource
/// identity (including the distinction between a default token and a URL).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServiceBinding {
    pub schema_version: u32,
    pub transport: Transport,
    pub endpoint: String,
    pub oauth_resource: std::option::Option<std::string::String>,
}

pub(super) fn service_binding(client: &CompanyClient, transport: Transport) -> Result<ServiceBinding> {
    let business = &client.connection().report.ucp.as_ref()
        .context("company has no UCP profile")?.metadata;
    let negotiated = NegotiatedProfile::from_business(business, client.profile())?;
    Ok(ServiceBinding {
        schema_version: 1,
        transport,
        endpoint: negotiated.service(transport)?.endpoint.clone(),
        oauth_resource: client.resource().map(str::to_owned),
    })
}

pub(super) fn action_service_binding(
    store: &Store,
    client: &CompanyClient,
    action: &CommerceAction,
) -> Result<Value> {
    let transport = match action {
        CommerceAction::Negotiate => return Ok(Value::Null),
        CommerceAction::Request(request) => request.transport,
        CommerceAction::Reconcile { operation_id } => original_request(store, *operation_id, client)?.transport,
    };
    Ok(serde_json::to_value(service_binding(client, transport)?)?)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum CommerceAction {
    Negotiate,
    Request(CommerceRequest),
    Reconcile { operation_id: EntityId },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommerceRequest {
    pub operation: Operation,
    #[serde(default = "default_transport")]
    pub transport: Transport,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Actual buyer address, when supplied by the user-facing host. Never guess
    /// the buyer's network address from the agent's egress IP.
    #[serde(default)]
    pub buyer_ip: Option<String>,
    /// An actual prior compat.poppy.commerce operation, never supplied terms.
    #[serde(default)]
    pub checkout_observation: Option<EntityId>,
    #[serde(default)]
    pub payment_credential_ref: Option<String>,
}

fn default_transport() -> Transport {
    Transport::Mcp
}

impl CommerceAction {
    /// Run before the native handler freezes input, so inline payment secrets
    /// cannot enter shared CAS through an ostensibly ordinary commerce payload.
    pub(super) fn validate(&self) -> Result<()> {
        if let Self::Request(request) = self {
            ensure!(
                request.id.is_some() == request.operation.needs_resource_id(),
                "commerce resource id presence mismatch"
            );
            if let Some(id) = &request.id {
                ensure!(
                    !id.is_empty()
                        && id.len() <= 4096
                        && !matches!(id.as_str(), "." | "..")
                        && !id.chars().any(char::is_control),
                    "invalid opaque commerce resource id"
                );
            }
            ensure!(
                !request.operation.changes_state() || request.idempotency_key.is_some(),
                "commerce mutation requires its original idempotency UUID"
            );
            if let Some(key) = &request.idempotency_key {
                let parsed = key
                    .parse::<EntityId>()
                    .context("invalid commerce idempotency UUID")?;
                ensure!(
                    parsed.to_string() == key.to_ascii_lowercase(),
                    "commerce idempotency key must be a hyphenated UUID"
                );
            }
            ensure!(
                serde_json::to_vec(&request.payload)?.len() <= 64 * 1024,
                "commerce payload exceeds 64 KiB"
            );
            if let Some(ip) = &request.buyer_ip {
                ip.parse::<std::net::IpAddr>().context("invalid buyer IP")?;
            }
            ensure!(
                request.payload.get("payment").is_none(),
                "payment data must be supplied by a node-local credential reference"
            );
            if request.operation == Operation::CompleteCheckout {
                ensure!(
                    request.checkout_observation.is_some()
                        && request.payment_credential_ref.is_some(),
                    "completion requires an observed checkout operation and payment credential reference"
                );
                ensure!(
                    request.payload.is_null()
                        || request.payload.as_object().is_some_and(|v| v.is_empty()),
                    "completion cannot replace purchase terms or carry inline payment credentials"
                );
                let reference = request
                    .payment_credential_ref
                    .as_deref()
                    .and_then(|reference| reference.strip_prefix("instrument-"))
                    .context("payment credential must be an instrument handle")?;
                let parsed = reference
                    .parse::<EntityId>()
                    .context("invalid instrument handle")?;
                ensure!(
                    parsed.to_string() == reference,
                    "instrument handle must be canonical"
                );
            } else {
                ensure!(
                    request.payment_credential_ref.is_none()
                        && request.checkout_observation.is_none(),
                    "payment credential and purchase observation are only used for completion"
                );
            }
        }
        Ok(())
    }
}

pub(super) async fn execute(
    client: &CompanyClient,
    store: &Store,
    call: &RecordedCall,
    action: &CommerceAction,
    cancellation: &CancellationToken,
) -> Result<Value> {
    action.validate()?;
    let business = &client
        .connection()
        .report
        .ucp
        .as_ref()
        .context("company has no UCP profile")?
        .metadata;
    let negotiated = NegotiatedProfile::from_business(business, client.profile())?;
    let mut envelope = json!({
        "commerce_schema": 1, "company": client.connection().report.company_identifier,
        "user_id": client.connection().user_id, "client_id": client.client_id(),
        "account_binding": client.account_binding(), "platform_profile": client.profile().url(),
        "service_binding": action_service_binding(store, client, action)?,
        "action": action, "request_dispatched": false, "network_started": false,
        "observed_at": chrono::Utc::now(), "status": "unknown"
    });
    if let CommerceAction::Request(request) = action {
        envelope["original_idempotency_key"] = json!(request.idempotency_key);
        envelope["recovery_query"] = recovery_query(request);
    }
    let result = match action {
        CommerceAction::Negotiate => {
            negotiate(client, &negotiated, cancellation, &mut envelope).await
        }
        CommerceAction::Request(request) => {
            run_request(
                client,
                store,
                call,
                &negotiated,
                request,
                cancellation,
                &mut envelope,
            )
            .await
        }
        CommerceAction::Reconcile { operation_id } => {
            reconcile(
                client,
                store,
                call,
                &negotiated,
                *operation_id,
                cancellation,
                &mut envelope,
            )
            .await
        }
    };
    if let Err(error) = result {
        // Protocol and schema errors deliberately omit raw credential-bearing
        // payloads. The original request/key stay in the envelope on failure.
        envelope["reason"] = error.to_string().into();
        envelope["status"] = if envelope["network_started"] == false {
            "rejected"
        } else {
            "unknown"
        }
        .into();
    }
    envelope["observed_at"] = json!(chrono::Utc::now());
    client.redact_observation(&mut envelope)?;
    Ok(envelope)
}

async fn negotiate(
    client: &CompanyClient,
    negotiated: &NegotiatedProfile,
    cancellation: &CancellationToken,
    envelope: &mut Value,
) -> Result<()> {
    if !negotiated
        .services
        .iter()
        .any(|service| service.transport == Transport::Mcp)
    {
        ensure!(
            !cancellation.is_cancelled(),
            "commerce cancelled before schema discovery"
        );
        envelope["network_started"] = true.into();
        let documents = rest_documents(client, negotiated, cancellation).await?;
        envelope["negotiated"] = serde_json::to_value(negotiated)?;
        envelope["service_schema"] = documents.document;
        envelope["schema_source"] = documents.source.into();
        envelope["negotiation_evidence"] = "local_intersection_and_advertised_rest_schema".into();
        envelope["status"] = "observed".into();
        envelope["effect_observation"] = "resource_read".into();
        return Ok(());
    }
    let endpoint = &negotiated.service(Transport::Mcp)?.endpoint;
    ensure!(
        !cancellation.is_cancelled(),
        "commerce cancelled before discovery"
    );
    envelope["network_started"] = true.into();
    let mut session = client
        .connect_mcp(endpoint, profile_headers(client, None))
        .await?;
    let result = async {
        let tools = tools_list(&session, client, cancellation).await?;
        ToolSchemas::from_tools_list(&tools, BTreeMap::new())?;
        envelope["status"] = "observed".into();
        envelope["effect_observation"] = "resource_read".into();
        envelope["negotiated"] = serde_json::to_value(negotiated)?;
        envelope["negotiation_evidence"] = "local_intersection_and_actual_mcp_tools".into();
        envelope["server_info"] = session.server_info()?;
        envelope["tools"] = tools["tools"].clone();
        Ok(())
    }
    .await;
    close(&mut session, envelope).await;
    result
}

async fn run_request(
    client: &CompanyClient,
    store: &Store,
    call: &RecordedCall,
    negotiated: &NegotiatedProfile,
    request: &CommerceRequest,
    cancellation: &CancellationToken,
    envelope: &mut Value,
) -> Result<()> {
    if request.transport == Transport::Rest {
        return run_rest_request(client, store, call, negotiated, request, cancellation, envelope).await;
    }
    ensure!(
        !cancellation.is_cancelled(),
        "commerce cancelled before discovery"
    );
    envelope["network_started"] = true.into();
    let mut session = client
        .connect_mcp(
            &negotiated.service(Transport::Mcp)?.endpoint,
            profile_headers(client, request.buyer_ip.as_deref()),
        )
        .await?;
    let result = async {
        let tools = tools_list(&session, client, cancellation).await?;
        let schemas = ToolSchemas::from_tools_list(&tools, BTreeMap::new())?;
        let plan = if request.operation == Operation::CompleteCheckout {
            let original = load_checkout(
                store,
                request
                    .checkout_observation
                    .context("original checkout observation missing")?,
                client,
                request.transport,
            )?;
            ensure!(
                request.id.as_deref() == Some(&original.identity.resource_id),
                "completion id differs from original checkout"
            );
            let latest = read_checkout(
                &session,
                &schemas,
                client,
                negotiated,
                &original.identity.resource_id,
                cancellation,
            )
            .await?;
            check_checkout_expiry(&latest)?;
            let binding = CompletionBinding::new(
                &original,
                request
                    .idempotency_key
                    .as_deref()
                    .context("completion idempotency key missing")?,
                request
                    .payment_credential_ref
                    .as_deref()
                    .context("payment credential reference missing")?,
            )?;
            envelope["completion_binding"] = serde_json::to_value(&binding)?;
            envelope["before"] = serde_json::to_value(&latest)?;
            binding.prepare_completion(
                &latest,
                negotiated,
                Transport::Mcp,
                completion_credential(client, store, call, &binding, request.transport)?,
            )?
        } else {
            if request.operation == Operation::UpdateCheckout {
                let latest = read_checkout(
                    &session,
                    &schemas,
                    client,
                    negotiated,
                    request.id.as_deref().context("checkout id missing")?,
                    cancellation,
                )
                .await?;
                ensure!(
                    latest.status != CheckoutStatus::CompleteInProgress,
                    "UCP forbids a new update while completion is in progress"
                );
                check_checkout_expiry(&latest)?;
            }
            RequestPlan::prepare(
                negotiated,
                Transport::Mcp,
                request.operation,
                request.id.as_deref(),
                request.payload.clone(),
                request.idempotency_key.as_deref(),
            )?
        };
        let plan = request_metadata(plan, request.buyer_ip.as_deref())?;
        schemas.validate_request(&plan)?;
        envelope["request"] = request_binding(&plan)?;
        client.prepare_commerce_request(&plan, &envelope["completion_binding"])?;
        ensure!(
            !cancellation.is_cancelled(),
            "commerce cancelled before tool dispatch"
        );
        // The tools/list session cannot cache the idempotency header from a
        // different call. The request authorizer derives it from this exact
        // tools/call body; the same profile remains in HTTP and MCP metadata.
        envelope["request_dispatched"] = true.into();
        match call_tool(&session, &plan, client, cancellation).await? {
            Response::Received { result } => {
                let resource = schemas.response_resource(negotiated, plan.operation, &result)?;
                record_resource(client, plan.operation, resource, envelope)?;
            }
            Response::Rejected { error } => {
                envelope["protocol_error"] = error;
                if !plan.operation.changes_state() {
                    envelope["status"] = "rejected".into();
                }
            }
            Response::Unknown { reason } => {
                envelope["reason"] = reason.into();
            }
        }
        Ok(())
    }
    .await;
    close(&mut session, envelope).await;
    result
}

struct RestDocuments {
    source: String,
    document: Value,
    resources: BTreeMap<String, Value>,
}

impl RestDocuments {
    fn schemas(&self, negotiated: &NegotiatedProfile, operation: Operation) -> Result<RestSchemas> {
        RestSchemas::from_openapi(
            negotiated,
            operation,
            &self.source,
            &self.document,
            self.resources.clone(),
        )
    }
}

async fn rest_documents(
    client: &CompanyClient,
    negotiated: &NegotiatedProfile,
    cancellation: &CancellationToken,
) -> Result<RestDocuments> {
    let source = negotiated.service(Transport::Rest)?.schema.clone();
    let mut pending = BTreeSet::from([source.clone()]);
    for descriptor in negotiated.capabilities.values() {
        pending.insert(
            descriptor["schema"]
                .as_str()
                .context("capability schema missing")?
                .to_owned(),
        );
    }
    let mut resources = BTreeMap::new();
    let mut bytes = 0_usize;
    while let Some(uri) = pending.pop_first() {
        if resources.contains_key(&uri) {
            continue;
        }
        ensure!(
            resources.len() < 128,
            "UCP schema closure exceeds document bound"
        );
        ensure!(
            !cancellation.is_cancelled(),
            "commerce schema discovery cancelled"
        );
        let document = tokio::select! {
            _ = cancellation.cancelled() => anyhow::bail!("commerce schema discovery cancelled"),
            _fetched = client.fetch_schema(&uri) => _fetched?,
        };
        bytes = bytes.saturating_add(serde_json::to_vec(&document)?.len());
        ensure!(
            bytes <= 8 * 1024 * 1024,
            "UCP schema closure exceeds byte bound"
        );
        let references = referenced_schema_documents(&uri, &document)?;
        resources.insert(uri, document);
        pending.extend(
            references
                .into_iter()
                .filter(|uri| !resources.contains_key(uri)),
        );
        ensure!(
            pending.len() + resources.len() <= 128,
            "UCP schema closure exceeds document bound"
        );
    }
    let document = resources
        .get(&source)
        .context("REST service schema unavailable")?
        .clone();
    Ok(RestDocuments {
        source,
        document,
        resources,
    })
}

async fn run_rest_request(
    client: &CompanyClient,
    store: &Store,
    call: &RecordedCall,
    negotiated: &NegotiatedProfile,
    request: &CommerceRequest,
    cancellation: &CancellationToken,
    envelope: &mut Value,
) -> Result<()> {
    ensure!(
        !cancellation.is_cancelled(),
        "commerce cancelled before schema discovery"
    );
    envelope["network_started"] = true.into();
    let documents = rest_documents(client, negotiated, cancellation).await?;
    let schemas = documents.schemas(negotiated, request.operation)?;
    let plan = if request.operation == Operation::CompleteCheckout {
        let original = load_checkout(
            store,
            request
                .checkout_observation
                .context("original checkout observation missing")?,
            client,
            request.transport,
        )?;
        ensure!(
            request.id.as_deref() == Some(&original.identity.resource_id),
            "completion id differs from original checkout"
        );
        let latest = read_rest_checkout(
            client,
            negotiated,
            &documents,
            &original.identity.resource_id,
            request.buyer_ip.as_deref(),
            cancellation,
        )
        .await?;
        check_checkout_expiry(&latest)?;
        let binding = CompletionBinding::new(
            &original,
            request
                .idempotency_key
                .as_deref()
                .context("completion idempotency key missing")?,
            request
                .payment_credential_ref
                .as_deref()
                .context("payment credential reference missing")?,
        )?;
        envelope["completion_binding"] = serde_json::to_value(&binding)?;
        envelope["before"] = serde_json::to_value(&latest)?;
        binding.prepare_completion(
            &latest,
            negotiated,
            Transport::Rest,
            completion_credential(client, store, call, &binding, request.transport)?,
        )?
    } else {
        if request.operation == Operation::UpdateCheckout {
            let latest = read_rest_checkout(
                client,
                negotiated,
                &documents,
                request.id.as_deref().context("checkout id missing")?,
                request.buyer_ip.as_deref(),
                cancellation,
            )
            .await?;
            ensure!(
                latest.status != CheckoutStatus::CompleteInProgress,
                "UCP forbids a new update while completion is in progress"
            );
            check_checkout_expiry(&latest)?;
        }
        RequestPlan::prepare(
            negotiated,
            Transport::Rest,
            request.operation,
            request.id.as_deref(),
            request.payload.clone(),
            request.idempotency_key.as_deref(),
        )?
    };
    let plan = request_metadata(plan, request.buyer_ip.as_deref())?;
    schemas.validate_request(&plan)?;
    envelope["request"] = request_binding(&plan)?;
    client.prepare_commerce_request(&plan, &envelope["completion_binding"])?;
    ensure!(
        !cancellation.is_cancelled(),
        "commerce cancelled before REST dispatch"
    );
    envelope["request_dispatched"] = true.into();
    let response = tokio::select! {
        _ = cancellation.cancelled() => anyhow::bail!("commerce cancelled after dispatch; outcome is unknown"),
        _sent = client.send_signed_plan(&plan) => _sent?,
    };
    envelope["http_status"] = response.status.into();
    let mut resource: Value =
        serde_json::from_slice(&response.body).context("REST result is not UCP JSON")?;
    client.redact_observation(&mut resource)?;
    schemas.validate_response(response.status, &resource)?;
    negotiated.validate_response_capabilities(&resource, plan.operation)?;
    // An HTTP status or a valid JSON document alone is not an order receipt.
    // UCP business errors remain observations with unknown mutation effects.
    record_resource(client, plan.operation, resource, envelope)
}

async fn read_rest_checkout(
    client: &CompanyClient,
    negotiated: &NegotiatedProfile,
    documents: &RestDocuments,
    id: &str,
    buyer_ip: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<CheckoutObservation> {
    let plan = request_metadata(
        RequestPlan::prepare(
            negotiated,
            Transport::Rest,
            Operation::GetCheckout,
            Some(id),
            Value::Null,
            None,
        )?,
        buyer_ip,
    )?;
    let schemas = documents.schemas(negotiated, Operation::GetCheckout)?;
    schemas.validate_request(&plan)?;
    let response = tokio::select! {
        _ = cancellation.cancelled() => anyhow::bail!("checkout observation cancelled"),
        _sent = client.send_signed_plan(&plan) => _sent?,
    };
    let mut resource: Value =
        serde_json::from_slice(&response.body).context("checkout result is not UCP JSON")?;
    client.redact_observation(&mut resource)?;
    schemas.validate_response(response.status, &resource)?;
    negotiated.validate_response_capabilities(&resource, Operation::GetCheckout)?;
    let checkout = CheckoutObservation::from_resource(
        &client.connection().report.company_identifier,
        &client.connection().user_id,
        resource,
    )?;
    ensure!(
        checkout.identity.resource_id == id,
        "merchant returned a different checkout"
    );
    Ok(checkout)
}

fn request_metadata(mut plan: RequestPlan, buyer_ip: Option<&str>) -> Result<RequestPlan> {
    // Tracing identity is not the business idempotency key and never replaces it.
    plan = plan.with_request_id(&EntityId::new().to_string())?;
    if let Some(ip) = buyer_ip {
        plan = plan.with_buyer_ip(ip)?;
    }
    Ok(plan)
}

fn profile_headers(client: &CompanyClient, buyer_ip: Option<&str>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([("UCP-Agent".into(), client.profile().agent_header())]);
    if let Some(ip) = buyer_ip {
        headers.insert("Shopify-Buyer-IP".into(), ip.into());
    }
    headers
}

async fn tools_list(
    session: &Session,
    client: &CompanyClient,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    for _ in 0..16 {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |value| json!({"cursor": value}));
        let request = serde_json::from_value(json!({"method": "tools/list", "params": params}))?;
        let response = session
            .request(request, client.timeout(), cancellation)
            .await?;
        let Response::Received { mut result } = response else {
            anyhow::bail!("UCP tools discovery did not return a complete result")
        };
        client.redact_observation(&mut result)?;
        let page = result["tools"]
            .as_array()
            .context("MCP tools list missing")?;
        tools.extend(page.iter().cloned());
        ensure!(tools.len() <= 128, "UCP tools discovery exceeds tool bound");
        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(next) = &cursor {
            ensure!(
                seen.insert(next.clone()),
                "UCP tools pagination repeated a cursor"
            );
        } else {
            return Ok(json!({"tools": tools}));
        }
    }
    anyhow::bail!("UCP tools pagination exceeds page bound")
}

async fn call_tool(
    session: &Session,
    plan: &RequestPlan,
    client: &CompanyClient,
    cancellation: &CancellationToken,
) -> Result<Response> {
    let request =
        serde_json::from_value(json!({"method": "tools/call", "params": plan.mcp_params()}))?;
    let mut response = session
        .request(request, client.timeout(), cancellation)
        .await?;
    match &mut response {
        Response::Received { result } => client.redact_observation(result)?,
        Response::Rejected { error } => client.redact_observation(error)?,
        Response::Unknown { reason } => client.redact_observation(&mut json!(reason))?,
    }
    Ok(response)
}

async fn read_checkout(
    session: &Session,
    schemas: &ToolSchemas,
    client: &CompanyClient,
    negotiated: &NegotiatedProfile,
    id: &str,
    cancellation: &CancellationToken,
) -> Result<CheckoutObservation> {
    let plan = RequestPlan::prepare(
        negotiated,
        Transport::Mcp,
        Operation::GetCheckout,
        Some(id),
        Value::Null,
        None,
    )?;
    schemas.validate_request(&plan)?;
    let Response::Received { result } = call_tool(session, &plan, client, cancellation).await?
    else {
        anyhow::bail!("current checkout could not be observed")
    };
    let resource = schemas.response_resource(negotiated, Operation::GetCheckout, &result)?;
    let checkout = CheckoutObservation::from_resource(
        &client.connection().report.company_identifier,
        &client.connection().user_id,
        resource,
    )?;
    ensure!(
        checkout.identity.resource_id == id,
        "merchant returned a different checkout"
    );
    Ok(checkout)
}

fn recovery_query(request: &CommerceRequest) -> Value {
    let operation = match request.operation.capability() {
        shenko_compat_poppy::ucp::CHECKOUT => Operation::GetCheckout,
        shenko_compat_poppy::ucp::CART => Operation::GetCart,
        shenko_compat_poppy::ucp::ORDER => Operation::GetOrder,
        _ => return Value::Null,
    };
    request.id.as_ref().map_or(Value::Null, |id| json!({
        "operation": operation, "transport": request.transport, "id": id, "observation_only": true
    }))
}

fn request_binding(plan: &RequestPlan) -> Result<Value> {
    Ok(
        json!({"operation": plan.operation, "transport": plan.transport, "endpoint": plan.endpoint,
        "id": plan.arguments().get("id"), "idempotency_key": plan.arguments()["meta"].get("idempotency-key"),
        "payload_sha256": plan.payload_sha256()?}),
    )
}

fn completion_credential(
    client: &CompanyClient,
    store: &Store,
    call: &RecordedCall,
    binding: &CompletionBinding,
    transport: Transport,
) -> Result<Value> {
    let frozen = read_observation(store, &call.operation.resource_bindings_ref)?;
    let instrument: InstrumentRef = serde_json::from_value(frozen["extra"]["payment_credential"].clone())
        .context("completion lacks its frozen immutable instrument reference")?;
    client.credential_for_checkout(&instrument, binding, &service_binding(client, transport)?)
}

pub(super) fn record_resource(
    client: &CompanyClient,
    operation: Operation,
    resource: Value,
    envelope: &mut Value,
) -> Result<()> {
    if resource["ucp"]["status"] != "error"
        && let Some(expected) = envelope["request"]["id"].as_str()
    {
        ensure!(
            resource["id"].as_str() == Some(expected),
            "merchant returned a different resource identity"
        );
    }
    envelope["business_status"] = resource["ucp"]["status"]
        .as_str()
        .unwrap_or("success")
        .into();
    envelope["effect_observation"] = if operation.changes_state() {
        "resource_changed"
    } else {
        "resource_read"
    }
    .into();
    if resource["ucp"]["status"] == "error" {
        envelope["effect_observation"] = "unknown".into();
    } else if operation.capability() == shenko_compat_poppy::ucp::CHECKOUT {
        let checkout = CheckoutObservation::from_resource(
            &client.connection().report.company_identifier,
            &client.connection().user_id,
            resource.clone(),
        )?;
        envelope["terms_sha256"] = checkout.terms_sha256()?.into();
        envelope["checkout"] = serde_json::to_value(&checkout)?;
        if let Some(url) = checkout.user_handoff_url()? {
            envelope["continuation"] = json!({"kind": "user_browser", "url": url,
                "headers": "none", "observe_operation": "get_checkout", "id": checkout.identity.resource_id});
        }
        if operation == Operation::CompleteCheckout {
            envelope["completion_state"] = match checkout.status {
                CheckoutStatus::Completed => "order_observed",
                CheckoutStatus::CompleteInProgress => "accepted_processing",
                _ => "continuation_required",
            }
            .into();
            if checkout.status != CheckoutStatus::Completed {
                envelope["effect_observation"] = "unknown".into();
            }
        }
    }
    envelope["resource"] = resource;
    envelope["status"] = "observed".into();
    Ok(())
}

async fn reconcile(
    client: &CompanyClient,
    store: &Store,
    call: &RecordedCall,
    negotiated: &NegotiatedProfile,
    operation_id: EntityId,
    cancellation: &CancellationToken,
    envelope: &mut Value,
) -> Result<()> {
    let request = original_request(store, operation_id, client)?;
    envelope["reconciles_operation"] = json!(operation_id);
    envelope["original_idempotency_key"] = json!(request.idempotency_key);
    envelope["original_request_replayed"] = false.into();
    envelope["original_effect_resolved"] = false.into();
    let attempt = store.external_operation_attempt(operation_id)?;
    let observed = if request.id.is_none() && attempt.final_receipt_id.is_some() {
        Some(load_observation(store, operation_id, client)?)
    } else {
        None
    };
    let id = request
        .id
        .as_deref()
        .or_else(|| observed.as_ref().and_then(|v| v["resource"]["id"].as_str()));
    let Some(id) = id else {
        envelope["reason"] = "original mutation has no observed resource ID; result remains unknown and is not replayed".into();
        return Ok(());
    };
    let query_operation = match request.operation.capability() {
        shenko_compat_poppy::ucp::CHECKOUT => Operation::GetCheckout,
        shenko_compat_poppy::ucp::CART => Operation::GetCart,
        shenko_compat_poppy::ucp::ORDER => Operation::GetOrder,
        _ => anyhow::bail!("original operation has no workflow status query"),
    };
    let query = CommerceRequest {
        operation: query_operation,
        transport: request.transport,
        id: Some(id.into()),
        payload: Value::Null,
        idempotency_key: None,
        checkout_observation: None,
        payment_credential_ref: None,
        buyer_ip: request.buyer_ip.clone(),
    };
    run_request(client, store, call, negotiated, &query, cancellation, envelope).await?;
    observe_reconciliation(client, store, operation_id, envelope)
}

/// Fold one already validated, identity-bound read into a claim about the
/// original operation. This does no I/O beyond reading immutable local facts.
pub(super) fn observe_reconciliation(
    client: &CompanyClient,
    store: &Store,
    operation_id: EntityId,
    envelope: &mut Value,
) -> Result<()> {
    let request = original_request(store, operation_id, client)?;
    validate_binding_owner(envelope, client, request.transport)?;
    envelope["reconciles_operation"] = json!(operation_id);
    envelope["original_idempotency_key"] = json!(request.idempotency_key);
    envelope["original_request_replayed"] = false.into();
    envelope["original_effect_resolved"] = false.into();
    if request.operation.changes_state() {
        envelope["original_final_request"] = super::transport::commerce_request_reference(
            store, &store.external_operation(operation_id)?,
        )?;
    }
    if request.operation == Operation::CompleteCheckout
        && envelope["status"] == "observed"
        && envelope["checkout"]["status"] == "completed"
    {
        // Completion is monotonic and identity-bound. The exact original
        // account grant was checked above; an order on an unrelated account
        // cannot settle this original attempt.
        let authorized = load_checkout(
            store,
            request
                .checkout_observation
                .context("original purchase terms observation missing")?,
            client,
            request.transport,
        )?;
        let current = CheckoutObservation::from_resource(
            &client.connection().report.company_identifier,
            &client.connection().user_id,
            envelope["resource"].clone(),
        )?;
        ensure!(
            current.status == CheckoutStatus::Completed
                && request.id.as_deref() == Some(current.identity.resource_id.as_str()),
            "recovery did not observe the original completed checkout"
        );
        let id = &current.identity.resource_id;
        ensure!(
            authorized.identity == current.identity,
            "completed checkout differs from original purchase"
        );
        envelope["original_terms_match"] =
            (authorized.terms_sha256()? == current.terms_sha256()?).into();
        if envelope["original_terms_match"] != true {
            envelope["reason"] = "completed checkout terms differ from original observation; original effect remains unresolved".into();
            return Ok(());
        }
        let order_id = envelope["resource"]["order"]["id"]
            .as_str()
            .context("completed checkout has no order identity")?
            .to_owned();
        envelope["original_effect_resolved"] = true.into();
        envelope["original_effect_observation"] = json!({"checkout_id": id, "order_id": order_id,
            "level": "postcondition_verified", "payment_settlement_observed": false});
    }
    // A later cart/checkout state alone cannot prove which update or cancel
    // caused it. Preserve the fact without converting it into a mutation ACK.
    Ok(())
}

fn original_request(
    store: &Store,
    operation_id: EntityId,
    client: &CompanyClient,
) -> Result<CommerceRequest> {
    let original = store.external_operation(operation_id)?;
    ensure!(
        original.operation_name == HANDLER,
        "reconciliation target is not Poppy commerce"
    );
    let frozen = read_observation(store, &original.final_input_ref)?;
    let action: CommerceAction = serde_json::from_value(frozen["commerce"].clone())
        .context("original operation has no frozen commerce action")?;
    action.validate()?;
    let CommerceAction::Request(request) = action else {
        anyhow::bail!("original operation has no commerce business request")
    };
    let bindings = read_observation(store, &original.resource_bindings_ref)?;
    validate_binding_owner(&bindings["extra"], client, request.transport)?;
    if request.operation.changes_state() {
        // An old high-level intent without a retained final send is never
        // enough to reconstruct, replay, or settle an original mutation.
        client.original_commerce_request(store, &original, &request)?;
    }
    Ok(request)
}

fn validate_binding_owner(extra: &Value, client: &CompanyClient, transport: Transport) -> Result<()> {
    ensure!(
        extra["company"].as_str() == Some(&client.connection().report.company_identifier)
            && extra["user_id"].as_str() == Some(&client.connection().user_id)
            && extra["client_id"].as_str() == Some(client.client_id())
            && extra["platform_profile"].as_str() == Some(client.profile().url())
            && extra.get("account_binding").is_some()
            && extra["account_binding"].as_str() == client.account_binding(),
        "original dispatch belongs to a different company, user, operator, or account grant"
    );
    ensure!(
        extra.get("service_binding").is_some()
            && extra["service_binding"] == serde_json::to_value(service_binding(client, transport)?)?,
        "original commerce service or OAuth resource changed, or the old record lacks its immutable service binding"
    );
    Ok(())
}

async fn close(session: &mut Session, envelope: &mut Value) {
    if session.close().await.is_err() {
        // A failed session DELETE does not erase the observed business result.
        envelope["transport_cleanup"] = "completion_unobserved".into();
    }
}

fn check_checkout_expiry(checkout: &CheckoutObservation) -> Result<()> {
    if let Some(expires) = checkout.resource.get("expires_at") {
        let expires = chrono::DateTime::parse_from_rfc3339(
            expires.as_str().context("invalid checkout expiry")?,
        )?;
        ensure!(chrono::Utc::now() < expires, "checkout has expired");
    }
    Ok(())
}

pub(super) fn load_checkout(
    store: &Store,
    operation_id: EntityId,
    client: &CompanyClient,
    transport: Transport,
) -> Result<CheckoutObservation> {
    let request = original_request(store, operation_id, client)?;
    ensure!(
        request.transport == transport,
        "checkout cannot move to another UCP transport or service"
    );
    let observed = load_observation(store, operation_id, client)?;
    ensure!(
        observed["status"] == "observed",
        "original checkout has no complete observation"
    );
    let checkout: CheckoutObservation = serde_json::from_value(observed["checkout"].clone())
        .context("original operation has no checkout observation")?;
    checkout.identity.validate_owner(
        &client.connection().report.company_identifier,
        &client.connection().user_id,
    )?;
    let checked = CheckoutObservation::from_resource(
        &checkout.identity.company,
        &checkout.identity.pairwise_user_id,
        checkout.resource,
    )?;
    ensure!(
        checked.identity == checkout.identity && checked.status == checkout.status,
        "stored checkout projection differs from original merchant resource"
    );
    Ok(checked)
}

/// Local credential import binds a real provider instrument to a CAS-observed
/// checkout. This record belongs only in the node-local encrypted vault; the
/// model and shared ledger receive its generated instrument handle.
pub(super) fn payment_credential_record(
    client: &CompanyClient,
    store: &Store,
    checkout_observation: EntityId,
    instrument: Value,
) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&instrument)?.len() <= 64 * 1024,
        "payment instrument exceeds import bound"
    );
    ensure!(
        instrument.is_object(),
        "payment instrument must be an object"
    );
    for field in ["id", "handler_id", "type"] {
        ensure!(
            instrument[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "payment instrument identity is incomplete"
        );
    }
    let request = original_request(store, checkout_observation, client)?;
    let checkout = load_checkout(store, checkout_observation, client, request.transport)?;
    Ok(json!({
        "schema_version": 2,
        "company": checkout.identity.company,
        "user_id": checkout.identity.pairwise_user_id,
        "client_id": client.client_id(),
        "account_binding": client.account_binding(),
        "service_binding": service_binding(client, request.transport)?,
        "checkout_id": checkout.identity.resource_id,
        "terms_sha256": checkout.terms_sha256()?,
        "instrument": instrument
    }))
}

fn load_observation(
    store: &Store,
    operation_id: EntityId,
    client: &CompanyClient,
) -> Result<Value> {
    let request = original_request(store, operation_id, client)?;
    let attempt = store.external_operation_attempt(operation_id)?;
    let receipt = store.external_evidence(
        attempt
            .final_receipt_id
            .context("original operation has no observed receipt")?,
    )?;
    ensure!(
        receipt.operation_id == Some(operation_id)
            && receipt.attempt_id == Some(attempt.attempt_id)
            && matches!(
                receipt.source_id.as_str(),
                HANDLER | "compat.poppy.commerce.recovery"
            )
            && receipt.decode_status == DecodeStatus::Decoded,
        "original commerce receipt identity mismatch"
    );
    let observed = read_observation(store, &receipt.raw_ref)?;
    if receipt.source_id == "compat.poppy.commerce.recovery" {
        ensure!(
            receipt.observation_level == Some(ObservationLevel::PostconditionVerified)
                && observed["reconciles_operation"] == json!(operation_id)
                && observed["original_effect_resolved"] == true
                && observed["original_terms_match"] == true
                && observed["original_request_replayed"] == false,
            "original commerce recovery receipt lacks its bound completion proof"
        );
    }
    ensure!(
        observed["commerce_schema"] == 1
            && observed["company"].as_str() == Some(&client.connection().report.company_identifier)
            && observed["user_id"].as_str() == Some(&client.connection().user_id)
            && observed["client_id"].as_str() == Some(client.client_id())
            && observed["platform_profile"].as_str() == Some(client.profile().url())
            && observed.get("account_binding").is_some()
            && observed["account_binding"].as_str() == client.account_binding(),
        "original commerce observation belongs to a different company, user, operator, or account grant"
    );
    validate_binding_owner(&observed, client, request.transport)?;
    Ok(observed)
}

pub(super) fn read_observation(store: &Store, object: &ObjectRef) -> Result<Value> {
    ensure!(
        object.bytes <= 1024 * 1024,
        "commerce receipt exceeds index bound"
    );
    let raw = store.cas.read_indexed_range(
        &object.digest,
        object.bytes,
        0,
        object.bytes as usize,
        &object.digest,
    )?;
    let value: Value = serde_json::from_slice(&raw)?;
    if object.codec.name != "compat-observation-index" {
        return Ok(value);
    }
    ensure!(
        value["format"] == "compat-indexed-observation"
            && value["schema_version"] == 1
            && value["chunk_bytes"] == 1024 * 1024,
        "unsupported commerce receipt index"
    );
    let bytes = value["bytes"]
        .as_u64()
        .context("commerce receipt size missing")?;
    ensure!(
        bytes <= MAX_PRIOR_BYTES,
        "commerce receipt exceeds read bound"
    );
    let digest: Digest = serde_json::from_value(value["artifact_digest"].clone())?;
    let chunks: Vec<Digest> = serde_json::from_value(value["chunks"].clone())?;
    ensure!(
        chunks.len() as u64 == bytes.div_ceil(1024 * 1024),
        "commerce receipt index does not cover artifact"
    );
    let mut body = Vec::with_capacity(bytes as usize);
    for (index, chunk) in chunks.iter().enumerate() {
        let offset = index as u64 * 1024 * 1024;
        let count = (bytes - offset).min(1024 * 1024) as usize;
        body.extend(
            store
                .cas
                .read_indexed_range(&digest, bytes, offset, count, chunk)?,
        );
    }
    ensure!(
        Digest::blake3(&body) == digest,
        "commerce receipt artifact digest mismatch"
    );
    Ok(serde_json::from_slice(&body)?)
}
