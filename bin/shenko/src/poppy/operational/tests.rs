//! One checkout continuity journey. Network results are explicit fixtures;
//! installation, key storage, schema checks, T1/E1, CAS and repair are real.

use super::*;
use crate::poppy::connection::{Operator, OperatorConfig};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use shenko_compat::recorded::seal;
use shenko_compat_mcp::protocol::HttpRequestAuthorizer;
use shenko_compat_poppy::{
    discovery::{
        DeclaredCapabilities, DiscoveredDocument, DiscoveryReport, DiscoverySource, DiscoveryState,
        OAuthDiscovery,
    },
    transport::{EndpointBinding, RequestSigner},
    ucp::{
        CHECKOUT, CheckoutStatus, CompletionBinding, NegotiatedProfile, Operation, RequestPlan,
        SHOPPING_SERVICE, ToolSchemas, Transport, VERSION,
    },
};
use shenko_node::UnitRequest;
use shenko_types::{
    agent::CallContext,
    commit::CommitRef,
    ids::{ControllerEpoch, LedgerSeq, RecordRevision, RuntimeGeneration, TaskRevision},
    store::WriteBatch,
    task::{DispatchEligibility, RunMode, TaskInvocation, TaskOwner, TaskState},
};
use std::{collections::BTreeMap, path::Path};

const ACCEPTANCE: &str = "an identity-bound completed checkout and unchanged purchase terms";
const RETRY: &str = "observe the original checkout without repeating complete";
const PROCESSOR: &str = "com.example.fixture_processor";

#[test]
fn checkout_unknown_is_repaired_from_immutable_observation_without_replay() -> Result<()> {
    let directory = TemporaryDirectory::new()?;
    let root = &directory.0;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: root.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())?;
    let agent = crate::bootstrap_genesis(
        &store,
        None,
        &home.manifest,
        &shenko_seed::ModelProfile {
            provider: None,
            model: None,
            configured: false,
            capacity: shenko_seed::ModelCapacity::Blocked,
        },
    )?;

    let declaration = json!({"id":"fixture-handler","version":VERSION,
        "spec":"https://example.com/fixture-processor",
        "schema":"https://example.com/fixture-processor/schema.json",
        "available_instruments":[{"type":"card"}]});
    Operator::initialize(
        root,
        OperatorConfig {
            schema_version: 1,
            client_name: "Shenko continuity fixture".into(),
            client_id: "https://platform.example/client.json".into(),
            jwks_uri: "https://platform.example/jwks.json".into(),
            profile_uri: "https://platform.example/.well-known/ucp".into(),
            redirect_uris: vec!["https://platform.example/callback".into()],
            payment_handlers: BTreeMap::from([(PROCESSOR.into(), vec![declaration.clone()])]),
        },
    )?;
    let mut node = Node::open(&root.join("node"))?;
    let connection = Connection::new(report(declaration), &node)?;
    let export = connection.export("fixture.poppy.company", true)?;
    let reference = UnitRef {
        unit_id: export.root_unit_id.clone(),
        revision: export
            .root_revision
            .clone()
            .context("connection revision")?,
        artifact_digest: export
            .root_artifact_digest
            .clone()
            .context("connection artifact")?,
    };
    node.handle_unit_request(UnitRequest::Import {
        export,
        package_root: root.join("units").display().to_string(),
        enable: Some(true),
    })?;
    // Import retains the dependency without enabling it. Match the separate
    // `compat enable compat.poppy` action before using the company connection.
    node.handle_unit_request(UnitRequest::SetEnabled {
        unit_id: UNIT_ID.into(),
        enabled: true,
    })?;
    let client = CompanyClient::new(
        Vault::open(root, false)?,
        Connection::read(&node, &reference)?,
        &Authentication::SignedOut,
        Duration::from_secs(10),
    )?;
    let negotiated = NegotiatedProfile::from_business(
        &client
            .connection()
            .report
            .ucp
            .as_ref()
            .context("UCP fixture profile")?
            .metadata,
        client.profile(),
    )?;
    let schemas = ToolSchemas::from_tools_list(
        &serde_json::from_str(include_str!("fixtures/checkout-tool-schemas.json"))?,
        BTreeMap::new(),
    )?;

    // A genuine get_checkout wire shape is projected through the production
    // validator before its observation becomes an allowed purchase binding.
    let ready_resource = checkout_resource();
    let read_plan = RequestPlan::prepare(
        &negotiated,
        Transport::Mcp,
        Operation::GetCheckout,
        Some("checkout-original"),
        Value::Null,
        None,
    )?;
    schemas.validate_request(&read_plan)?;
    let ready_resource = schemas.response_resource(
        &negotiated,
        Operation::GetCheckout,
        &json!({"structuredContent":ready_resource}),
    )?;
    let mut ready_value = envelope(&client)?;
    commerce::record_resource(
        &client,
        Operation::GetCheckout,
        ready_resource,
        &mut ready_value,
    )?;
    let read_input = input(
        &reference,
        json!({"action":"request","operation":"get_checkout",
        "id":"checkout-original"}),
    );
    let read_invocation = invocation(&store, agent, read_input.clone())?;
    let mut read_call = prepare(&store, &read_invocation, &read_input, &client, &reference)?;
    ensure!(
        read_call.arm(&store, &read_invocation)?.is_none(),
        "read already armed"
    );
    let (outcome, effect, level) = settlement(&ready_value);
    read_call.observe(&store, ready_value, outcome, effect, level)?;
    let ready = commerce::load_checkout(&store, read_call.operation.operation_id, &client, Transport::Mcp)?;

    // Only the encrypted local vault receives this deliberately synthetic
    // processor token. The frozen task carries a checkout-scoped handle.
    let instrument_ref = format!("instrument-{}", EntityId::new());
    let instrument = json!({"id":"fixture-instrument","handler_id":"fixture-handler","type":"card",
        "credential":{"type":"token","token":"fixture-private-token"}});
    let record = commerce::payment_credential_record(
        &client,
        &store,
        read_call.operation.operation_id,
        instrument,
    )?;
    client
        .vault()
        .write_immutable(&instrument_ref, &serde_json::to_vec(&record)?)?;
    let service = commerce::service_binding(&client, Transport::Mcp)?;
    let instrument_reference = client.instrument_reference(&instrument_ref, &service)?;
    let idempotency_key = EntityId::new().to_string();
    let binding = CompletionBinding::new(&ready, &idempotency_key, &instrument_ref)?;
    let mut other_checkout = binding.clone();
    other_checkout.identity.resource_id = "checkout-other".into();
    ensure!(
        client
            .credential_for_checkout(&instrument_reference, &other_checkout, &service)
            .is_err(),
        "instrument escaped its observed checkout scope"
    );
    let request_id = EntityId::new().to_string();
    let plan = binding
        .prepare_completion(
            &ready,
            &negotiated,
            Transport::Mcp,
            client.credential_for_checkout(&instrument_reference, &binding, &service)?,
        )?
        .with_request_id(&request_id)?;
    schemas.validate_request(&plan)?;
    ensure!(
        plan.mcp_params()["arguments"]["meta"]["idempotency-key"] == idempotency_key,
        "completion changed its durable key"
    );
    let final_request = verify_checkout_http_signature(&client, &plan, &idempotency_key, &request_id)?;

    let complete_input = input(
        &reference,
        json!({"action":"request","operation":"complete_checkout",
        "id":"checkout-original","idempotency_key":idempotency_key,
        "checkout_observation":read_call.operation.operation_id,"payment_credential_ref":instrument_ref}),
    );
    let complete_action: CommerceAction =
        serde_json::from_value(complete_input["commerce"].clone())?;
    complete_action.validate()?;
    let complete_invocation = invocation(&store, agent, complete_input.clone())?;
    let mut original_call = prepare(
        &store,
        &complete_invocation,
        &complete_input,
        &client,
        &reference,
    )?;
    ensure!(
        original_call.arm(&store, &complete_invocation)?.is_none(),
        "completion already armed"
    );
    let original_id = original_call.operation.operation_id;
    let frozen_input = original_call.operation.final_input_ref.clone();
    let guard_store = store.clone();
    let client = client
        .with_guard(Arc::new(move || {
            ensure!(guard_store.external_operation(original_id)?.outcome.is_none(),
                "fixture request no longer owns its unsettled operation");
            Ok(())
        }))
        .with_commerce_call(store.clone(), &original_call)?;
    client.prepare_commerce_request(&plan, &serde_json::to_value(&binding)?)?;
    let authorizer = client.authorizer(
        EndpointBinding::from_ucp_plan(&client.connection().report, &plan)?,
        BTreeMap::new(), None,
    )?;
    // Exercise the production final-send callback with the exact signed HTTP
    // body. No fixture merchant is contacted and no response is invented here.
    authorizer.before_send(&final_request)?;
    let final_reference = client.final_commerce_request().context("final request reference")?;
    let request_head = store.external_operation(original_id)?;
    let request_append_commit = request_head.evidence_through.context("final request append cursor")?;
    let request_attempt = store.external_operation_attempt(original_id)?;
    let sealed_request_receipt = store.external_operation_evidence(original_id)?.into_iter()
        .find(|receipt| receipt.source_id == "compat.poppy.commerce.request")
        .context("pre-send final request receipt")?;
    ensure!(
        final_reference["rpc_id"] == 1 && final_reference["request_id"] == request_id
            && final_reference["idempotency_key"] == idempotency_key
            && request_head.state == OperationState::InFlight && request_head.outcome.is_none()
            && request_attempt.dispatch_stage == AttemptDispatchStage::Armed
            && request_attempt.final_receipt_id.is_none(),
        "pre-send request identity was lost or was mistaken for a merchant response"
    );
    ensure!(
        sealed_request_receipt.received_commit == request_append_commit
            && request_append_commit.ledger_id == original_call.operation.prepared_commit.ledger_id
            && request_append_commit.seq > original_call.operation.prepared_commit.seq,
        "final request receipt was not assigned its actual append commit after T1"
    );
    let mut receipt_replay = sealed_request_receipt.clone();
    receipt_replay.received_commit = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let mut replay_batch = WriteBatch::new();
    replay_batch.evidence_upserts.push(receipt_replay);
    store.commit(replay_batch)?;
    ensure!(
        store.external_evidence(sealed_request_receipt.receipt_id)? == sealed_request_receipt
            && store.external_operation(original_id)?.evidence_through == Some(request_append_commit)
            && store.external_operation_attempt(original_id)?.dispatch_stage == AttemptDispatchStage::Armed
            && store.external_operation_attempt(original_id)?.final_receipt_id.is_none(),
        "receipt replay rewrote its first admission coordinate or changed the armed attempt"
    );
    ensure!(authorizer.before_send(&final_request).is_err(), "sealed mutation was eligible for replay");
    drop(authorizer);
    let unknown = original_call.observe(&store,
        json!({"status":"unknown","reason":"controller stopped after send; no merchant result observed",
            "original_operation":original_id,"request_dispatched":null}),
        None, EffectStatus::Unknown, None)?;
    ensure!(
        unknown.state == OperationState::Reconciling && unknown.outcome.is_none(),
        "transport uncertainty was converted to a result"
    );
    let unknown_receipt = store
        .external_operation_attempt(original_id)?
        .final_receipt_id
        .context("unknown receipt")?;

    // A lost reply must not make an instrument handle mutable. A separately
    // imported instrument cannot replace either the T1 digest or final bytes.
    let mut changed_record = record.clone();
    changed_record["instrument"]["credential"]["token"] = "fixture-replacement-token".into();
    let changed_bytes = serde_json::to_vec(&changed_record)?;
    ensure!(
        client.vault().write(&instrument_ref, &changed_bytes).is_err()
            && client.vault().write_immutable(&instrument_ref, &changed_bytes).is_err(),
        "lost reply allowed the original instrument reference to be replaced"
    );
    let replacement_ref = format!("instrument-{}", EntityId::new());
    client.vault().write_immutable(&replacement_ref, &changed_bytes)?;
    let replacement = client.instrument_reference(&replacement_ref, &service)?;
    ensure!(
        client.credential_for_checkout(&replacement, &binding, &service).is_err(),
        "a new instrument was substituted into the sealed completion"
    );

    // Another service may expose exactly the same checkout and order IDs and
    // terms under the same issuer/user/grant. Refuse it before any lookup/send.
    drop(client);
    let mut changed_connection = Connection::read(&node, &reference)?;
    changed_connection.report.ucp.as_mut().context("UCP collision fixture")?
        .metadata["ucp"]["services"][SHOPPING_SERVICE][0]["endpoint"] =
        "https://merchant.example/other-service/mcp".into();
    let changed_client = CompanyClient::new(
        Vault::open(root, false)?, changed_connection,
        &Authentication::SignedOut, Duration::from_secs(10),
    )?;
    let mut collision = envelope(&changed_client)?;
    let mut collision_resource = ready.resource.clone();
    collision_resource["status"] = "completed".into();
    collision_resource["order"] = json!({"id":"order-original"});
    commerce::record_resource(&changed_client, Operation::GetCheckout, collision_resource, &mut collision)?;
    ensure!(
        commerce::load_checkout(&store, read_call.operation.operation_id, &changed_client, Transport::Mcp).is_err()
            && commerce::payment_credential_record(&changed_client, &store, read_call.operation.operation_id,
                changed_record["instrument"].clone()).is_err()
            && changed_client.instrument_reference(&instrument_ref, &commerce::service_binding(&changed_client, Transport::Mcp)?).is_err()
            && commerce::observe_reconciliation(&changed_client, &store, original_id, &mut collision).is_err(),
        "changed service admitted a colliding original checkout, instrument or recovery proof"
    );
    drop(changed_client);
    let mut refreshed_connection = Connection::read(&node, &reference)?;
    refreshed_connection.session_id = shenko_compat_poppy::session::SessionIdentity::new(
        &refreshed_connection.report.company_identifier, &refreshed_connection.user_id,
    )?.session_id().into();
    let client = CompanyClient::new(
        Vault::open(root, false)?, refreshed_connection,
        &Authentication::SignedOut, Duration::from_secs(10),
    )?;
    ensure!(
        commerce::load_checkout(&store, read_call.operation.operation_id, &client, Transport::Mcp)?.identity == ready.identity,
        "compatible Session refresh lost the original service-owned checkout"
    );
    let CommerceAction::Request(original_request) = complete_action else {
        anyhow::bail!("completion fixture lost its original action")
    };
    ensure!(
        client.original_commerce_request(&store, &original_call.operation, &original_request)? == final_reference,
        "reopening with a new instrument or Session changed the original final send identity"
    );

    let recovery_plan = binding.recovery_request(
        &negotiated,
        Transport::Mcp,
        &client.connection().report.company_identifier,
        &client.connection().user_id,
    )?;
    schemas.validate_request(&recovery_plan)?;
    ensure!(
        recovery_plan.operation == Operation::GetCheckout
            && recovery_plan.arguments()["id"] == "checkout-original"
            && recovery_plan.arguments()["meta"]
                .get("idempotency-key")
                .is_none(),
        "recovery prepared another completion instead of an identity-bound read"
    );
    let mut completed_resource = ready.resource.clone();
    completed_resource["status"] = "completed".into();
    completed_resource["ucp"]["payment_handlers"] = json!({});
    completed_resource["order"] = json!({"id":"order-original",
        "permalink_url":"https://merchant.example/orders/order-original"});
    let completed_resource = schemas.response_resource(
        &negotiated,
        Operation::GetCheckout,
        &json!({"structuredContent":completed_resource}),
    )?;
    let mut proof = envelope(&client)?;
    commerce::record_resource(
        &client,
        Operation::GetCheckout,
        completed_resource,
        &mut proof,
    )?;
    commerce::observe_reconciliation(&client, &store, original_id, &mut proof)?;
    ensure!(
        proof["original_effect_resolved"] == true && proof["original_terms_match"] == true,
        "completed receipt lost the original purchase terms when handler metadata disappeared"
    );
    ensure!(
        proof["original_effect_observation"]["payment_settlement_observed"] == false,
        "order observation claimed payment settlement"
    );
    client.redact_observation(&mut proof)?;
    let recovery_input = input(
        &reference,
        json!({"action":"reconcile","operation_id":original_id}),
    );
    let recovery_invocation = invocation(&store, agent, recovery_input.clone())?;
    let mut recovery_call = prepare(
        &store,
        &recovery_invocation,
        &recovery_input,
        &client,
        &reference,
    )?;
    ensure!(
        recovery_call.arm(&store, &recovery_invocation)?.is_none(),
        "recovery already armed"
    );
    let (outcome, effect, level) = settlement(&proof);
    let recovery = recovery_call.observe(&store, proof, outcome, effect, level)?;
    let recovery_receipt = store.external_evidence(
        store
            .external_operation_attempt(recovery.operation_id)?
            .final_receipt_id
            .context("recovery receipt")?,
    )?;
    ensure!(
        store.external_operation(original_id)?.effect_status == EffectStatus::Unknown,
        "recovery observation mutated the original before its durable fold"
    );

    // Simulate termination between immutable E1 and its local fold. Reopening
    // repairs from that exact receipt, without credentials or a remote call.
    drop(client);
    drop(node);
    drop(store);
    let reopened = Store::open(root.join("shenko.db"), root.join("cas"))?;
    let recovered_read = reopened.external_operation(recovery.operation_id)?;
    apply_reconciliation(&reopened, &recovered_read)?;
    let repaired = reopened.external_operation(original_id)?;
    ensure!(
        repaired.state == OperationState::Resolved
            && repaired.outcome == Some(OperationOutcome::Succeeded)
            && repaired.effect_status == EffectStatus::Complete,
        "original completion was not repaired"
    );
    let repaired_receipt = reopened.external_evidence(
        reopened
            .external_operation_attempt(original_id)?
            .final_receipt_id
            .context("repaired receipt")?,
    )?;
    ensure!(
        repaired_receipt.raw_ref == recovery_receipt.raw_ref
            && repaired_receipt.observation_level == Some(ObservationLevel::PostconditionVerified),
        "repair replaced the immutable merchant observation"
    );
    apply_reconciliation(&reopened, &recovered_read)?;
    ensure!(
        reopened.external_operation(original_id)?.record_revision == repaired.record_revision,
        "local repair applied the same observation twice"
    );

    let reopened_node = Node::open(&root.join("node"))?;
    let reopened_client = CompanyClient::new(
        Vault::open(root, false)?,
        Connection::read(&reopened_node, &reference)?,
        &Authentication::SignedOut,
        Duration::from_secs(10),
    )?;
    ensure!(
        commerce::load_checkout(&reopened, original_id, &reopened_client, Transport::Mcp)?.status
            == CheckoutStatus::Completed,
        "derived recovery receipt became unreadable after restart"
    );
    let mut repeated = prepare(
        &reopened,
        &complete_invocation,
        &complete_input,
        &reopened_client,
        &reference,
    )?;
    ensure!(
        repeated.arm(&reopened, &complete_invocation)?.is_some()
            && reopened.external_operation_attempts(original_id)?.len() == 1,
        "an already armed completion gained another attempt"
    );
    ensure!(
        repaired.final_input_ref == frozen_input
            && commerce::read_observation(&reopened, &frozen_input)?["commerce"]["idempotency_key"]
                == idempotency_key,
        "recovery changed the original frozen business request"
    );
    let evidence = reopened.external_operation_evidence(original_id)?;
    ensure!(
        evidence.len() == 3
            && evidence
                .iter()
                .any(|receipt| receipt.receipt_id == unknown_receipt)
            && evidence.iter().filter(|receipt| receipt.source_id == "compat.poppy.commerce.request").count() == 1
            && reopened_client.original_commerce_request(&reopened, &repaired, &original_request)? == final_reference,
        "recovery erased uncertainty or duplicated the observed external event"
    );
    let request_receipt = evidence.iter().find(|receipt| receipt.source_id == "compat.poppy.commerce.request")
        .context("original final request receipt missing")?;
    ensure!(
        request_receipt == &sealed_request_receipt
            && request_receipt.received_commit == request_append_commit
            && request_receipt.received_commit != original_call.operation.prepared_commit,
        "reopened final request receipt lost the actual durable append coordinate or was backdated to T1"
    );
    let request_reference = commerce::read_observation(&reopened, &request_receipt.raw_ref)?;
    ensure!(
        request_reference == final_reference
            && !serde_json::to_string(&request_reference)?.contains("fixture-private-token")
            && !serde_json::to_string(&request_reference)?.contains("fixture-replacement-token"),
        "shared final request reference lost its authenticated identity or exposed an instrument"
    );
    ensure!(
        !serde_json::to_string(&commerce::read_observation(&reopened, &frozen_input)?)?
            .contains("fixture-private-token")
            && !serde_json::to_string(&commerce::read_observation(
                &reopened,
                &repaired_receipt.raw_ref
            )?)?
            .contains("fixture-private-token"),
        "local processor credential entered shared CAS"
    );
    let private = reopened_client.vault().read(final_reference["private_ref"].as_str().context("private request reference")?)?
        .context("retained private request missing after reopen")?;
    let private: Value = serde_json::from_slice(&private)?;
    let body = URL_SAFE_NO_PAD.decode(private["body_base64url"].as_str().context("retained final body")?)?;
    ensure!(
        body == final_request.body().and_then(reqwest::Body::as_bytes).context("original signed body")?
            && serde_json::from_slice::<Value>(&body)?["params"]["arguments"]["checkout"]["payment"]["instruments"][0]["credential"]["token"] == "fixture-private-token"
            && !serde_json::to_string(&final_reference)?.contains("fixture-private-token"),
        "lost reply or later instrument import changed the retained bytes or exposed their credential"
    );
    Ok(())
}

fn verify_checkout_http_signature(
    client: &CompanyClient,
    plan: &RequestPlan,
    original_key: &str,
    request_id: &str,
) -> Result<reqwest::Request> {
    // The verifier receives public JWKS and the exact materialized request.
    // It rebuilds the RFC 9421 base from required wire components, not signer
    // internals, and checks the digest against the same checkout body.
    let mut request = reqwest::Request::new(
        reqwest::Method::from_bytes(plan.method.as_bytes())?,
        url::Url::parse(&plan.endpoint)?,
    );
    for (name, value) in &plan.headers {
        request.headers_mut().insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
            reqwest::header::HeaderValue::from_str(value)?,
        );
    }
    let body = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,
        "method":"tools/call","params":plan.mcp_params()}))?;
    *request.body_mut() = Some(body.into());
    let operator = client.operator();
    let signer = RequestSigner::new(
        operator.signer.clone(),
        operator
            .signer
            .delegate(client.connection().identity()?, Duration::from_secs(3600))?,
        EndpointBinding::from_ucp_plan(&client.connection().report, plan)?,
        &operator.config.jwks_uri,
    )?;
    signer.authorize(&mut request, None)?;
    let header = |name: &str| -> Result<&str> {
        request
            .headers()
            .get(name)
            .context("missing signed checkout header")?
            .to_str()
            .context("invalid signed checkout header")
    };
    ensure!(
        header("ucp-agent")? == client.profile().agent_header()
            && header("idempotency-key")? == original_key
            && header("request-id")? == request_id
            && plan.arguments()["meta"]["request-id"] == request_id
            && header("poppy-version")? == "1.0"
            && header("poppy-extensions")? == "\"ucp\";v=\"1\"",
        "signed checkout metadata differs from the original request"
    );
    let wire_body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .context("missing materialized checkout body")?;
    ensure!(
        request.method().as_str() == plan.method
            && request.url().as_str() == plan.endpoint
            && serde_json::from_slice::<Value>(wire_body)?["params"] == plan.mcp_params(),
        "signing changed the prepared checkout request"
    );
    ensure!(
        header("content-digest")?
            == format!(
                "sha-256=:{}:",
                STANDARD.encode(ring::digest::digest(&ring::digest::SHA256, wire_body))
            ),
        "checkout content digest does not cover the final body"
    );
    let agent = format!("\"{}\"", operator.config.jwks_uri);
    ensure!(
        header("signature-agent")? == format!("poppy={agent}"),
        "checkout verification key location changed"
    );
    let mut components = vec![
        ("\"@method\"", request.method().as_str().to_owned()),
        (
            "\"@authority\"",
            request.url()[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
        ),
        ("\"@path\"", request.url().path().to_owned()),
        ("\"@target-uri\"", request.url().as_str().to_owned()),
        ("\"signature-agent\";key=\"poppy\"", agent),
    ];
    for name in [
        "poppy-session",
        "poppy-version",
        "poppy-extensions",
        "ucp-agent",
        "idempotency-key",
        "request-id",
        "content-digest",
        "content-type",
    ] {
        components.push((name, header(name)?.to_owned()));
    }
    let components: Vec<_> = components
        .into_iter()
        .map(|(name, value)| {
            let name = if name.starts_with('"') {
                name.to_owned()
            } else {
                format!("\"{name}\"")
            };
            (name, value)
        })
        .collect();
    let covered = components
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let params = header("signature-input")?
        .strip_prefix("poppy=")
        .context("missing checkout signature label")?;
    ensure!(
        params.starts_with(&format!("({covered});"))
            && params.contains(&format!(";keyid=\"{}\"", operator.signer.key_id()))
            && params.contains(";alg=\"ecdsa-p256-sha256\"")
            && params.contains(";tag=\"web-bot-auth\""),
        "HTTP signature omits a checkout integrity component"
    );
    let base = format!(
        "{}\n\"@signature-params\": {params}",
        components
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let signature = STANDARD.decode(
        header("signature")?
            .strip_prefix("poppy=:")
            .and_then(|value| value.strip_suffix(':'))
            .context("invalid checkout signature encoding")?,
    )?;
    let public = operator.signer.public_jwks();
    let key = &public["keys"][0];
    let mut point = vec![4_u8];
    point.extend(URL_SAFE_NO_PAD.decode(key["x"].as_str().context("public key x")?)?);
    point.extend(URL_SAFE_NO_PAD.decode(key["y"].as_str().context("public key y")?)?);
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
        .verify(base.as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("merchant could not verify final checkout HTTP signature"))?;
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD.decode(
            header("poppy-session")?
                .split('.')
                .nth(1)
                .context("missing signed Poppy Session claims")?,
        )?,
    )?;
    ensure!(
        claims["aud"] == client.connection().report.company_identifier
            && claims["sub"] == client.connection().user_id
            && claims["sid"] == client.session_id()
            && claims["iss"] == client.client_id()
            && claims["htm"] == plan.method
            && claims["htu"] == plan.endpoint,
        "signed checkout Session lost its original owner or destination"
    );
    Ok(request)
}

fn input(reference: &UnitRef, commerce: Value) -> Value {
    json!({"connection":reference,"authentication":{"mode":"signed_out"},"timeout_secs":10,"commerce":commerce})
}

fn owner(client: &CompanyClient, reference: &UnitRef) -> Value {
    json!({"unit_id":UNIT_ID,"connection":reference,"company":client.connection().report.company_identifier,
        "user_id":client.connection().user_id,"session_id":client.connection().session_id,
        "client_id":client.client_id(),"account_binding":client.account_binding(),"platform_profile":client.profile().url()})
}

fn envelope(client: &CompanyClient) -> Result<Value> {
    Ok(json!({"commerce_schema":1,"company":client.connection().report.company_identifier,
        "user_id":client.connection().user_id,"client_id":client.client_id(),
        "account_binding":client.account_binding(),"platform_profile":client.profile().url(),
        "service_binding":commerce::service_binding(client, Transport::Mcp)?,
        "request":{"operation":"get_checkout","id":"checkout-original"},
        "status":"unknown","request_dispatched":true}))
}

fn prepare(
    store: &Store,
    invocation: &NativeTaskInvocation,
    input: &Value,
    client: &CompanyClient,
    reference: &UnitRef,
) -> Result<RecordedCall> {
    let action: CommerceAction = serde_json::from_value(input["commerce"].clone())?;
    let mut binding = owner(client, reference);
    binding["service_binding"] = commerce::action_service_binding(store, client, &action)?;
    if let CommerceAction::Request(request) = &action
        && let Some(reference) = &request.payment_credential_ref
    {
        binding["payment_credential"] = serde_json::to_value(client.instrument_reference(
            reference, &commerce::service_binding(client, request.transport)?,
        )?)?;
    }
    RecordedCall::prepare(
        store,
        invocation,
        &Kind::Commerce.descriptor(),
        input,
        binding,
        ACCEPTANCE,
        RETRY,
    )
}

fn invocation(store: &Store, agent: EntityId, input: Value) -> Result<NativeTaskInvocation> {
    let descriptor = Kind::Commerce.descriptor();
    let task_id = EntityId::new();
    let invocation_id = EntityId::new();
    let epoch = ControllerEpoch(1);
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: agent,
        conversation_id: agent,
        owner: TaskOwner::Task {
            id: EntityId::new(),
        },
        background: false,
        kind: descriptor.kind.clone(),
        definition_version: descriptor.definition_version,
        implementation_ref: descriptor.implementation_ref,
        input_ref: seal(store, &input, "poppy-journey-input")?,
        state: TaskState::Running {
            checkpoint_ref: None,
            invocation_id,
            mode: RunMode::Run,
            epoch,
        },
        abort_request: None,
        memos: Default::default(),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task.clone());
    store.commit(batch)?;
    let mut context = CallContext::new_system(RuntimeGeneration::default());
    context.task_id = Some(task_id);
    context.invocation_id = Some(invocation_id);
    context.controller_epoch = epoch;
    Ok(NativeTaskInvocation {
        context,
        task_invocation: TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        },
        task,
        operation_id: EntityId::new(),
        authority: AuthorityRef {
            delegation_id: task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: EntityId::new(),
        },
        input,
        child_tools: None,
    })
}

fn checkout_resource() -> Value {
    json!({"ucp":{"version":VERSION,"status":"success","services":{},
        "capabilities":{CHECKOUT:[{"version":VERSION}]},
        "payment_handlers":{PROCESSOR:[{"id":"fixture-handler","version":VERSION,
            "available_instruments":[{"type":"card"}]}]}},
        "id":"checkout-original","status":"ready_for_complete","currency":"USD",
        "line_items":[{"id":"line-original","item":{"id":"variant-original","title":"Fixture shoes","price":1000},
            "quantity":1,"totals":[{"type":"subtotal","amount":1000},{"type":"total","amount":1000}]}],
        "totals":[{"type":"subtotal","amount":1000},{"type":"total","amount":1000}],
        "links":[{"type":"terms_of_service","url":"https://merchant.example/terms"}],
        "com.example.purchase_terms":{"delivery":"standard"}})
}

fn report(handler: Value) -> DiscoveryReport {
    let source = DiscoverySource {
        requested_url: "https://merchant.example/.well-known/poppy.json".into(),
        url: "https://merchant.example/.well-known/poppy.json".into(),
        redirects: vec![],
        http_status: 200,
        fetched_at_unix_ms: 0,
        body_sha256: None,
    };
    DiscoveryReport {
        state: DiscoveryState::Discovered,
        discovery_source: source.clone(),
        source: source.clone(),
        company_origin: "https://merchant.example".into(),
        company_identifier: "https://merchant.example".into(),
        issuer: None,
        protocol_version: "1.0".into(),
        request_protocol_version: "1.0".into(),
        metadata: json!({"protocol_version":"1.0","organization":{"domain":"merchant.example"},
            "auth":{"disabled":true},"extensions":{"ucp":[{"version":"1"}]}}),
        capabilities: DeclaredCapabilities {
            agent: vec![],
            apis: vec![],
            web: Some(json!({})),
            tasks_endpoint: None,
        },
        oauth: OAuthDiscovery {
            protected_resource_source: DiscoverySource {
                http_status: 404,
                ..source.clone()
            },
            protected_resource_metadata: None,
            authorization_server: None,
        },
        ucp: Some(DiscoveredDocument {
            source: DiscoverySource {
                requested_url: "https://merchant.example/.well-known/ucp".into(),
                url: "https://merchant.example/.well-known/ucp".into(),
                ..source
            },
            metadata: json!({"ucp":{"version":VERSION,
                "services":{SHOPPING_SERVICE:[{"version":VERSION,"transport":"mcp",
                    "spec":format!("https://ucp.dev/{VERSION}/specification/overview/"),
                    "schema":format!("https://ucp.dev/{VERSION}/services/shopping/mcp.openrpc.json"),
                    "endpoint":"https://merchant.example/ucp/mcp"}]},
                "capabilities":{CHECKOUT:[{"version":VERSION,
                    "schema":format!("https://ucp.dev/{VERSION}/schemas/shopping/checkout.json")}]},
                "payment_handlers":{PROCESSOR:[handler]}}}),
        }),
    }
}

struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("shenko-poppy-commerce-{}", EntityId::new()));
        std::fs::create_dir_all(&root)?;
        Ok(Self(root))
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(Path::new(&self.0));
    }
}
