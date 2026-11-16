//! One account lifetime across a lost refresh, replacement and disconnection.
//! Merchant token responses below are fixtures, not a live authorization claim.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use shenko_compat_poppy::discovery::{
    DeclaredCapabilities, DiscoveredDocument, DiscoveryReport, DiscoverySource, DiscoveryState,
    OAuthDiscovery,
};

use super::{super::connection::OperatorConfig, super::transport::Authentication, *};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("shenko-poppy-account-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture_connection() -> Connection {
    let source = |url: &str| DiscoverySource {
        requested_url: url.into(),
        url: url.into(),
        redirects: Vec::new(),
        http_status: 200,
        fetched_at_unix_ms: 1,
        body_sha256: None,
    };
    let poppy = source("https://shop.example/.well-known/poppy.json");
    Connection {
        schema_version: 1,
        report: DiscoveryReport {
            state: DiscoveryState::Discovered,
            discovery_source: poppy.clone(),
            source: poppy,
            company_origin: "https://shop.example".into(),
            company_identifier: "https://identity.shop.example".into(),
            issuer: Some("https://identity.shop.example".into()),
            protocol_version: "1.0".into(),
            request_protocol_version: "1.0".into(),
            metadata: json!({"protocol_version":"1.0","organization":{"name":"Fixture shop","domain":"shop.example"}}),
            capabilities: DeclaredCapabilities {
                agent: Vec::new(),
                apis: Vec::new(),
                web: Some(json!({})),
                tasks_endpoint: None,
            },
            oauth: OAuthDiscovery {
                protected_resource_source: source(
                    "https://shop.example/.well-known/oauth-protected-resource",
                ),
                protected_resource_metadata: Some(json!({"resource":"https://shop.example",
                    "authorization_servers":["https://identity.shop.example"]})),
                authorization_server: Some(DiscoveredDocument {
                    source: source(
                        "https://identity.shop.example/.well-known/oauth-authorization-server",
                    ),
                    metadata: json!({
                        "issuer":"https://identity.shop.example",
                        "protected_resources":["https://shop.example"],
                        "authorization_endpoint":"https://identity.shop.example/authorize",
                        "token_endpoint":"https://identity.shop.example/token",
                        "revocation_endpoint":"https://identity.shop.example/revoke",
                        "grant_types_supported":["authorization_code","refresh_token"],
                        "dpop_signing_alg_values_supported":["ES256"]
                    }),
                }),
            },
            ucp: None,
        },
        user_id: uuid::Uuid::new_v4().to_string(),
        session_id: uuid::Uuid::new_v4().to_string(),
        agent_id: None,
    }
}

fn reopen(path: &Path, connection: &Connection, sends: &Arc<AtomicUsize>) -> Result<CompanyClient> {
    let sends = sends.clone();
    Ok(CompanyClient::new(
        Vault::open(path, false)?,
        connection.clone(),
        &Authentication::SignedOut,
        Duration::from_secs(1),
    )?
    .with_guard(Arc::new(move || {
        sends.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("the continuity journey must not replay a network request")
    })))
}

fn restore_attempt(client: &CompanyClient, pending: &PendingFlow) -> Result<AuthorizationAttempt> {
    AuthorizationAttempt::restore_node_local_secret(
        &decode_state(pending)?,
        OAuthServer::from_discovery(&client.connection().report)?,
        &client.operator().signer,
        &client.connection().identity()?,
        &client.operator().config.redirect_uris,
    )
}

fn jwt_parts(jwt: &str) -> Result<(Value, Value)> {
    let mut parts = jwt.split('.');
    let header = parts.next().context("JWT has no header")?;
    let claims = parts.next().context("JWT has no claims")?;
    ensure!(
        parts.next().is_some() && parts.next().is_none(),
        "JWT has invalid parts"
    );
    Ok((
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header)?)?,
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims)?)?,
    ))
}

#[test]
fn account_continuity_preserves_unknown_refresh_and_revoke_across_reopen() -> Result<()> {
    let directory = TestDirectory::new()?;
    Operator::initialize(
        &directory.0,
        OperatorConfig {
            schema_version: 1,
            client_name: "Shenko continuity fixture".into(),
            client_id: "https://agent.example/client.json".into(),
            jwks_uri: "https://agent.example/jwks.json".into(),
            profile_uri: "https://agent.example/.well-known/ucp".into(),
            redirect_uris: vec!["https://agent.example/callback".into()],
            payment_handlers: BTreeMap::new(),
        },
    )?;
    let connection = fixture_connection();
    let sends = Arc::new(AtomicUsize::new(0));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let client = reopen(&directory.0, &connection, &sends)?;
    let caller = EntityId::new();
    let begin = AccountAction::BeginDirect {
        redirect_uri: "https://agent.example/callback".into(),
        scopes: vec!["orders:read".into()],
    };
    let started = runtime.block_on(execute(&client, &begin, EntityId::new(), None, caller))?;
    let authorization_url = Url::parse(
        started["authorization_url"]
            .as_str()
            .context("missing authorization URL")?,
    )?;
    let query = authorization_url
        .query_pairs()
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(query.get("prompt").map(String::as_str), Some("consent"));
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let mut pending = read_pending(client.vault(), client.operator(), &connection)?
        .context("missing pending grant")?;
    let attempt = restore_attempt(&client, &pending)?;
    let saved_attempt: Value = serde_json::from_slice(&decode_state(&pending)?)?;
    let private_key = saved_attempt
        .pointer("/dpop/pkcs8")
        .and_then(Value::as_str)
        .context("missing original DPoP key")?
        .to_owned();
    let mut callback = Url::parse("https://agent.example/callback")?;
    callback
        .query_pairs_mut()
        .append_pair("iss", &connection.report.company_identifier)
        .append_pair("state", query.get("state").context("missing OAuth state")?)
        .append_pair("code", "fixture-one-time-code");
    let callback_ref = store_callback(client.vault(), &connection, callback.as_str())?;
    let exchange = attempt
        .exchange_request(
            &client.operator().signer,
            callback.as_str(),
            None,
            Some("fixture-as-nonce"),
        )?
        .into_request();
    assert_eq!(exchange.method(), reqwest::Method::POST);
    assert_eq!(
        exchange.url().as_str(),
        "https://identity.shop.example/token"
    );
    assert!(!exchange.headers().contains_key("poppy-session"));
    assert!(!exchange.headers().contains_key("poppy-version"));
    let form = url::form_urlencoded::parse(
        exchange
            .body()
            .and_then(reqwest::Body::as_bytes)
            .context("missing token request body")?,
    )
    .into_owned()
    .collect::<BTreeMap<_, _>>();
    assert_eq!(
        form.get("code").map(String::as_str),
        Some("fixture-one-time-code")
    );
    assert_eq!(
        form.get("code_verifier").map(String::as_str),
        saved_attempt["verifier"].as_str()
    );
    let (_, assertion) = jwt_parts(
        form.get("client_assertion")
            .context("missing client assertion")?,
    )?;
    assert_eq!(assertion["aud"], connection.report.company_identifier);
    let (original_proof_header, proof) = jwt_parts(
        exchange
            .headers()
            .get("dpop")
            .context("missing DPoP proof")?
            .to_str()?,
    )?;
    assert_eq!(proof["htu"], exchange.url().as_str());
    assert_eq!(proof["nonce"], "fixture-as-nonce");
    assert!(proof.get("ath").is_none());

    // Persist the genuine grant envelope using a fixture token response. No
    // HTTP sender is replaced or given permission to contact the fixture host.
    let first_operation = EntityId::new();
    pending.flight = Some(flight(first_operation, "authorization_code")?);
    write_pending(&client, &pending)?;
    let tokens = attempt.accept_token_response(200, br#"{"access_token":"fixture-access-original","refresh_token":"fixture-refresh-original","token_type":"DPoP","scope":"orders:read","expires_in":3600}"#, None)?;
    let installed = runtime.block_on(install_grant(&client, &pending, tokens, first_operation))?;
    client.vault().remove(&callback_ref)?;
    assert_eq!(installed["state"], "authorized");
    let first_binding = installed["grant_binding"]
        .as_str()
        .context("missing grant binding")?
        .to_owned();
    let stored = load_account(client.vault(), client.operator(), &connection)?
        .context("grant was not persisted")?;
    assert_eq!(stored.binding, first_binding);
    let server = OAuthServer::from_discovery(&connection.report)?;
    let refresh_wire = stored
        .tokens
        .refresh_request(
            &server,
            &client.operator().signer,
            None,
            None,
            Some("fixture-next-as-nonce"),
        )?
        .into_request();
    let (refresh_header, refresh_proof) = jwt_parts(
        refresh_wire
            .headers()
            .get("dpop")
            .context("refresh lost original DPoP key")?
            .to_str()?,
    )?;
    assert_eq!(refresh_header["jwk"], original_proof_header["jwk"]);
    assert_ne!(refresh_proof["jti"], proof["jti"]);
    assert_eq!(refresh_proof["nonce"], "fixture-next-as-nonce");
    assert!(!refresh_wire.headers().contains_key("poppy-session"));
    let refresh_form = url::form_urlencoded::parse(
        refresh_wire
            .body()
            .and_then(reqwest::Body::as_bytes)
            .context("missing refresh body")?,
    )
    .into_owned()
    .collect::<BTreeMap<_, _>>();
    assert_eq!(
        refresh_form.get("refresh_token").map(String::as_str),
        Some("fixture-refresh-original")
    );

    // Crash at the durable send boundary: the merchant may have rotated this
    // refresh token, while the new token never reached this node.
    let refresh_operation = EntityId::new();
    let mut record = read_account(client.vault(), client.operator(), &connection)?
        .context("missing account record")?;
    record
        .active
        .as_mut()
        .context("missing active grant")?
        .flight = Some(flight(refresh_operation, "refresh_token")?);
    write_account(&client, &record)?;
    drop(client);
    let client = reopen(&directory.0, &connection, &sends)?;
    assert!(load_account(client.vault(), client.operator(), &connection)?.is_none());
    let unresolved = runtime.block_on(execute(
        &client,
        &AccountAction::Refresh {
            resource: None,
            scopes: Vec::new(),
        },
        EntityId::new(),
        None,
        caller,
    ))?;
    assert_eq!(unresolved["status"], "unknown");
    assert_eq!(
        unresolved["grant"]["original_flight"]["operation_id"],
        refresh_operation.to_string()
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);

    // A later explicit sign-in may itself lose its exchange response. Starting
    // one more sign-in must archive that uncertainty, not erase either grant.
    runtime.block_on(execute(&client, &begin, EntityId::new(), None, caller))?;
    let mut lost_flow = read_pending(client.vault(), client.operator(), &connection)?
        .context("missing replacement flow")?;
    let lost_operation = EntityId::new();
    lost_flow.flight = Some(flight(lost_operation, "authorization_code")?);
    write_pending(&client, &lost_flow)?;
    runtime.block_on(execute(&client, &begin, EntityId::new(), None, caller))?;
    let mut replacement = read_pending(client.vault(), client.operator(), &connection)?
        .context("missing final replacement")?;
    assert_eq!(replacement.archived.len(), 1);
    assert_eq!(replacement.archived[0].flow_id, lost_flow.flow_id);
    assert!(
        client
            .vault()
            .read(&replacement.archived[0].reference)?
            .is_some()
    );
    let replacement_attempt = restore_attempt(&client, &replacement)?;
    let replacement_tokens = replacement_attempt.accept_token_response(200, br#"{"access_token":"fixture-access-replacement","refresh_token":"fixture-refresh-replacement","token_type":"DPoP","scope":"orders:read","expires_in":3600}"#, None)?;
    let replacement_operation = EntityId::new();
    replacement.flight = Some(flight(replacement_operation, "authorization_code")?);
    write_pending(&client, &replacement)?;
    let replacement_result = runtime.block_on(install_grant(
        &client,
        &replacement,
        replacement_tokens,
        replacement_operation,
    ))?;
    assert_ne!(replacement_result["grant_binding"], first_binding);
    assert_eq!(replacement_result["status"], "unknown");
    let retained = read_pending(client.vault(), client.operator(), &connection)?
        .context("lost archived uncertainty")?;
    assert!(retained.completed && retained.secret_state.is_none());
    assert!(flow_may_have_issued_tokens(&retained));

    // The replacement's own revocation can also lose its response. Reopening
    // and asking to disconnect must neither send it again nor claim sign-out.
    let revoke_operation = EntityId::new();
    let mut record = read_account(client.vault(), client.operator(), &connection)?
        .context("missing replacement record")?;
    assert_eq!(record.replaced.len(), 1);
    assert_eq!(record.replaced[0].binding, first_binding);
    record
        .active
        .as_mut()
        .context("missing replacement grant")?
        .flight = Some(flight(revoke_operation, "revocation")?);
    write_account(&client, &record)?;
    drop(client);
    let client = reopen(&directory.0, &connection, &sends)?;
    let disconnected = runtime.block_on(execute(
        &client,
        &AccountAction::Revoke,
        EntityId::new(),
        None,
        caller,
    ))?;
    assert_eq!(disconnected["state"], "incomplete");
    assert_eq!(disconnected["status"], "unknown");
    assert_eq!(disconnected["all_locally_known_grants_revoked"], false);
    assert_eq!(disconnected["merchant_account_signout_verified"], false);
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    let snapshot = binding_snapshot(&client)?;
    assert!(snapshot["active"].is_null());
    assert_eq!(
        snapshot["replaced_grants"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(
        snapshot["pending_sign_in"]["archived_unresolved"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let public = serde_json::to_string(&snapshot)?;
    assert!(public.contains(&refresh_operation.to_string()));
    assert!(public.contains(&revoke_operation.to_string()));
    assert!(public.contains(&lost_operation.to_string()));
    assert!(!public.contains("fixture-access-original"));
    assert!(!public.contains("fixture-refresh-original"));
    assert!(!public.contains("fixture-access-replacement"));
    assert!(!public.contains("fixture-refresh-replacement"));
    assert!(!public.contains("fixture-one-time-code"));
    assert!(!public.contains(&private_key));
    assert!(
        !public.contains(
            saved_attempt["verifier"]
                .as_str()
                .context("missing PKCE verifier")?
        )
    );
    let sealed = std::fs::read(
        directory
            .0
            .join("poppy-private")
            .join(format!("{}.sealed", connection.credential_ref())),
    )?;
    assert!(
        !sealed
            .windows(b"fixture-refresh".len())
            .any(|bytes| bytes == b"fixture-refresh")
    );
    Ok(())
}
