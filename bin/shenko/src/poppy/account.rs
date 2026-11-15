//! Account orchestration over the shared durable task and node-local vault.
//! Protocol secrets never become task parameters, observations or shared CAS.
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat_poppy::auth::{
    AccountTokens, AuthorizationAttempt, BrowserJoin, DeviceAuthorization, DeviceSignIn,
    DirectSignIn, OAuthServer, oauth_failure,
};
use shenko_exec_host::browser::{BrowserProvider, private_form::PrivateFormRequest};
use shenko_types::ids::EntityId;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use super::{
    connection::{Connection, Operator},
    transport::{CompanyClient, HttpObservation},
    vault::Vault,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum AccountAction {
    Status,
    BeginDirect {
        redirect_uri: String,
        scopes: Vec<String>,
    },
    CompleteDirect {
        callback_ref: String,
    },
    BeginDevice {
        scopes: Vec<String>,
    },
    PollDevice,
    Refresh {
        #[serde(default)]
        resource: Option<String>,
        #[serde(default)]
        scopes: Vec<String>,
    },
    Revoke,
    JoinBrowser {
        session_id: EntityId,
        tab_id: Option<String>,
        lease_token: EntityId,
        controller_epoch: u64,
    },
}

pub(super) struct StoredGrant {
    pub tokens: AccountTokens,
    pub binding: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Flight {
    operation_id: String,
    kind: String,
    started_at_unix_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Completion {
    operation_id: String,
    kind: String,
    observed_http_status: u16,
    recorded_at_unix_ms: u64,
}

/// Tokens, refresh key, grant identity and send marker are one atomic vault
/// record. Keeping the marker in another file could resurrect stale tokens.
#[derive(Serialize, Deserialize)]
struct GrantRecord {
    binding: String,
    tokens: String,
    source_flow: String,
    flight: Option<Flight>,
    last_completion: Option<Completion>,
}

impl Drop for GrantRecord {
    fn drop(&mut self) {
        self.tokens.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
struct AccountRecord {
    version: u8,
    company_identifier: String,
    user_id: String,
    client_id: String,
    active: Option<GrantRecord>,
    /// Replaced refresh grants are kept until their own revocation returns 200.
    /// An unknown rotation/revocation remains quarantined, never erased.
    replaced: Vec<GrantRecord>,
    revocations: Vec<Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FlowKind {
    Direct,
    Device,
}

#[derive(Clone, Serialize, Deserialize)]
struct ArchivedFlow {
    reference: String,
    flow_id: String,
    flight: Option<Flight>,
}

#[derive(Serialize, Deserialize)]
struct PendingFlow {
    version: u8,
    company_identifier: String,
    user_id: String,
    client_id: String,
    flow_id: String,
    kind: FlowKind,
    #[serde(default)]
    completed: bool,
    /// None only while the device endpoint's first response is outstanding.
    secret_state: Option<String>,
    flight: Option<Flight>,
    /// Previous unresolved exchanges survive a new explicit sign-in attempt.
    archived: Vec<ArchivedFlow>,
}

impl Drop for PendingFlow {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.secret_state {
            secret.zeroize();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CallbackRecord {
    version: u8,
    company_identifier: String,
    user_id: String,
    flow_id: String,
    callback_url: String,
}

impl Drop for CallbackRecord {
    fn drop(&mut self) {
        self.callback_url.zeroize();
    }
}

pub(super) fn load_account(
    vault: &Vault,
    operator: &Operator,
    connection: &Connection,
) -> Result<Option<StoredGrant>> {
    let Some(record) = read_account(vault, operator, connection)? else {
        return Ok(None);
    };
    let Some(active) = record.active.as_ref() else {
        return Ok(None);
    };
    if active.flight.is_some() {
        return Ok(None);
    }
    Ok(Some(StoredGrant {
        tokens: restore_tokens(active, operator, connection)?,
        binding: active.binding.clone(),
    }))
}

/// CLI-only secret import. The raw callback is read from a private file by the
/// caller; native task inputs contain only this opaque, flow-bound reference.
pub(super) fn store_callback(
    vault: &Vault,
    connection: &Connection,
    callback_url: &str,
) -> Result<String> {
    ensure!(
        callback_url.len() <= 16 * 1024,
        "OAuth callback exceeds size bound"
    );
    let url = Url::parse(callback_url).map_err(|_| anyhow!("invalid OAuth callback URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid OAuth callback URL"
    );
    let operator = Operator::load(vault)?;
    let pending =
        read_pending(vault, &operator, connection)?.context("no direct sign-in is pending")?;
    ensure!(
        pending.kind == FlowKind::Direct,
        "pending sign-in does not use a callback"
    );
    ensure!(!pending.completed, "sign-in flow has already completed");
    let record = CallbackRecord {
        version: 1,
        company_identifier: connection.report.company_identifier.clone(),
        user_id: connection.user_id.clone(),
        flow_id: pending.flow_id.clone(),
        callback_url: callback_url.into(),
    };
    let reference = format!("oauth-callback-{}", uuid::Uuid::new_v4());
    write_secret(vault, &reference, &record)?;
    Ok(reference)
}

pub(super) async fn execute(
    client: &CompanyClient,
    action: &AccountAction,
    operation_id: EntityId,
    browser: Option<&Arc<BrowserProvider>>,
    caller: EntityId,
) -> Result<Value> {
    let mut result = match action {
        AccountAction::Status => status(client),
        AccountAction::BeginDirect {
            redirect_uri,
            scopes,
        } => begin_direct(client, redirect_uri, scopes, operation_id).await,
        AccountAction::CompleteDirect { callback_ref } => {
            complete_direct(client, callback_ref, operation_id).await
        }
        AccountAction::BeginDevice { scopes } => begin_device(client, scopes, operation_id).await,
        AccountAction::PollDevice => poll_device(client, operation_id).await,
        AccountAction::Refresh { resource, scopes } => {
            refresh(client, resource.as_deref(), scopes, operation_id).await
        }
        AccountAction::Revoke => revoke(client, operation_id).await,
        AccountAction::JoinBrowser {
            session_id,
            tab_id,
            lease_token,
            controller_epoch,
        } => {
            join_browser(
                client,
                browser.context("native browser provider is unavailable")?,
                PrivateFormRequest {
                    session_id: *session_id,
                    tab_id: tab_id.clone(),
                    lease_token: *lease_token,
                    controller_epoch: *controller_epoch,
                    endpoint: String::new(),
                    field_name: "code".into(),
                },
                caller,
            )
            .await
        }
    }?;
    if result.get("status").is_none() {
        let state = result
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let (status, effect) = match state {
            "unknown" | "incomplete" | "browser_join_observed" | "browser_join_not_authorized" => {
                ("unknown", "unknown")
            }
            "rejected" => ("rejected", "none"),
            "observed" | "no_local_grants" | "pending" | "expired" => ("observed", "none"),
            _ => ("observed", "complete"),
        };
        result["status"] = status.into();
        result["effect_observation"] = effect.into();
    }
    Ok(result)
}

/// Captured before the outer T1 seal; no network and no secret projection.
pub(super) fn binding_snapshot(client: &CompanyClient) -> Result<Value> {
    status(client)
}

fn status(client: &CompanyClient) -> Result<Value> {
    let record = read_account(client.vault(), client.operator(), client.connection())?;
    let pending = read_pending(client.vault(), client.operator(), client.connection())?;
    let active = record
        .as_ref()
        .and_then(|r| r.active.as_ref())
        .map(|g| grant_status(client, g))
        .transpose()?;
    let replaced = record
        .as_ref()
        .map(|r| {
            r.replaced
                .iter()
                .map(|g| grant_status(client, g))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(
        json!({"state":"observed","active":active,"replaced_grants":replaced,
        "pending_sign_in":pending.as_ref().map(flow_status),
        "recorded_revocations":record.as_ref().map(|r| &r.revocations),
        "credentials_present":record.as_ref().is_some_and(|r| r.active.as_ref().is_some_and(|g| g.flight.is_none())),
        "account_access_verified_remotely":false}),
    )
}

async fn begin_direct(
    client: &CompanyClient,
    redirect_uri: &str,
    scopes: &[String],
    operation_id: EntityId,
) -> Result<Value> {
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let mut attempt = AuthorizationAttempt::new(
        server.clone(),
        &client.operator().signer,
        &client.connection().identity()?,
        DirectSignIn {
            redirect_uri: redirect_uri.into(),
            registered_redirect_uris: client.operator().config.redirect_uris.clone(),
            scopes: scopes.to_vec(),
            redirect_received_on_initiating_device: false,
        },
    )?;
    let secret_state = encode_secret(attempt.export_node_local_secret()?);
    let mut pending = new_flow(client, FlowKind::Direct, Some(secret_state))?;
    if let Some(endpoint) = server.par_endpoint() {
        let request = attempt.par_request(
            &client.operator().signer,
            client.nonce_for(endpoint.as_str()).as_deref(),
        )?;
        pending.flight = Some(flight(operation_id, "pushed_authorization")?);
        write_pending(client, &pending)?;
        let response = client.send_oauth(request.into_request()).await?;
        if response.status != 201 {
            return rejected_flow(client, &mut pending, &response);
        }
        // A malformed success could have accepted the pushed request. Preserve
        // its original flight rather than guessing that it never happened.
        attempt.accept_par_response(response.status, &response.body)?;
        pending.secret_state = Some(encode_secret(attempt.export_node_local_secret()?));
        pending.flight = None;
        write_pending(client, &pending)?;
    }
    Ok(
        json!({"state":"authorization_required","method":"direct","flow_id":pending.flow_id,
        "authorization_url":attempt.authorization_url()?.as_str(),"existing_account_unchanged":true}),
    )
}

async fn complete_direct(
    client: &CompanyClient,
    callback_ref: &str,
    operation_id: EntityId,
) -> Result<Value> {
    let mut pending = read_pending(client.vault(), client.operator(), client.connection())?
        .context("no sign-in is pending")?;
    ensure!(
        pending.kind == FlowKind::Direct,
        "pending sign-in is not direct"
    );
    if let Some(recovered) = recovered_grant(client, &pending)? {
        return Ok(recovered);
    }
    if pending.flight.is_some() {
        return Ok(unknown_flow(&pending));
    }
    let suffix = callback_ref
        .strip_prefix("oauth-callback-")
        .context("invalid callback reference")?;
    ensure!(
        uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.to_string() == suffix),
        "invalid callback reference"
    );
    let bytes = client
        .vault()
        .read(callback_ref)?
        .context("OAuth callback is absent on this node")?;
    let callback: CallbackRecord = parse_secret(&bytes, "invalid stored OAuth callback")?;
    ensure!(
        callback.version == 1
            && callback.company_identifier == pending.company_identifier
            && callback.user_id == pending.user_id
            && callback.flow_id == pending.flow_id,
        "callback belongs to another sign-in flow"
    );
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let attempt = AuthorizationAttempt::restore_node_local_secret(
        &decode_state(&pending)?,
        server.clone(),
        &client.operator().signer,
        &client.connection().identity()?,
        &client.operator().config.redirect_uris,
    )?;
    let request = attempt.exchange_request(
        &client.operator().signer,
        &callback.callback_url,
        None,
        client
            .nonce_for(server.token_endpoint().as_str())
            .as_deref(),
    )?;
    pending.flight = Some(flight(operation_id, "authorization_code")?);
    write_pending(client, &pending)?;
    let response = client.send_oauth(request.into_request()).await?;
    if response.status != 200 {
        return rejected_flow(client, &mut pending, &response);
    }
    let tokens = attempt.accept_token_response(response.status, &response.body, None)?;
    let result = install_grant(client, &pending, tokens, operation_id).await?;
    client.vault().remove(callback_ref)?;
    Ok(result)
}

async fn begin_device(
    client: &CompanyClient,
    scopes: &[String],
    operation_id: EntityId,
) -> Result<Value> {
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let attempt = DeviceSignIn::new(
        server,
        &client.operator().signer,
        &client.connection().identity()?,
        scopes.to_vec(),
    )?;
    let request = attempt.request(&client.operator().signer)?;
    let mut pending = new_flow(client, FlowKind::Device, None)?;
    pending.flight = Some(flight(operation_id, "device_authorization")?);
    write_pending(client, &pending)?;
    let response = client.send_oauth(request.into_request()).await?;
    if response.status != 200 {
        return rejected_flow(client, &mut pending, &response);
    }
    let authorized = attempt.accept_response(response.status, &response.body)?;
    pending.secret_state = Some(encode_secret(authorized.export_node_local_secret()?));
    pending.flight = None;
    write_pending(client, &pending)?;
    Ok(
        json!({"state":"authorization_required","method":"device","flow_id":pending.flow_id,
        "presentation":authorized.presentation(),"existing_account_unchanged":true}),
    )
}

async fn poll_device(client: &CompanyClient, operation_id: EntityId) -> Result<Value> {
    let mut pending = read_pending(client.vault(), client.operator(), client.connection())?
        .context("no sign-in is pending")?;
    ensure!(
        pending.kind == FlowKind::Device,
        "pending sign-in is not device authorization"
    );
    if let Some(recovered) = recovered_grant(client, &pending)? {
        return Ok(recovered);
    }
    if pending.flight.is_some() {
        return Ok(unknown_flow(&pending));
    }
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let mut attempt = DeviceAuthorization::restore_node_local_secret(
        &decode_state(&pending)?,
        server.clone(),
        &client.operator().signer,
        &client.connection().identity()?,
    )?;
    let presentation = attempt.presentation();
    let time = now_ms()? / 1000;
    if time >= presentation.expires_at_unix_secs {
        return Ok(
            json!({"state":"expired","flow_id":pending.flow_id,"existing_account_unchanged":true}),
        );
    }
    if time < presentation.next_poll_at_unix_secs {
        return Ok(
            json!({"state":"pending","flow_id":pending.flow_id,"next_poll_at_unix_secs":presentation.next_poll_at_unix_secs}),
        );
    }
    let request = attempt.poll_request(
        &client.operator().signer,
        None,
        client
            .nonce_for(server.token_endpoint().as_str())
            .as_deref(),
    )?;
    pending.secret_state = Some(encode_secret(attempt.export_node_local_secret()?));
    pending.flight = Some(flight(operation_id, "device_token")?);
    write_pending(client, &pending)?;
    let response = match client.send_oauth(request.into_request()).await {
        Ok(response) => response,
        Err(error) => {
            // Slower timing is persisted while the flight remains unknown.
            // This does not authorize another poll that might rotate a grant.
            attempt.record_transport_timeout()?;
            pending.secret_state = Some(encode_secret(attempt.export_node_local_secret()?));
            write_pending(client, &pending)?;
            return Err(error);
        }
    };
    if response.status == 200 {
        let tokens = attempt.accept_token_response(response.status, &response.body, None)?;
        return install_grant(client, &pending, tokens, operation_id).await;
    }
    if let Some(failure) = known_rejection(&response)? {
        let still_pending = attempt.observe_failure(&failure)?;
        pending.secret_state = Some(encode_secret(attempt.export_node_local_secret()?));
        pending.flight = None;
        write_pending(client, &pending)?;
        return Ok(
            json!({"state":if still_pending {"pending"} else {"rejected"},
            "flow_id":pending.flow_id,"error":failure,"next_poll_at_unix_secs":attempt.presentation().next_poll_at_unix_secs,
            "existing_account_unchanged":true}),
        );
    }
    Ok(unknown_flow(&pending))
}

async fn refresh(
    client: &CompanyClient,
    resource: Option<&str>,
    scopes: &[String],
    operation_id: EntityId,
) -> Result<Value> {
    let mut record = read_account(client.vault(), client.operator(), client.connection())?
        .context("no account grant is stored")?;
    let active = record
        .active
        .as_ref()
        .context("no active account grant is stored")?;
    if active.flight.is_some() {
        return unknown_grant(client, active);
    }
    let mut tokens = restore_tokens(active, client.operator(), client.connection())?;
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let requested = (!scopes.is_empty()).then_some(scopes);
    let request = tokens.refresh_request(
        &server,
        &client.operator().signer,
        requested,
        resource,
        client
            .nonce_for(server.token_endpoint().as_str())
            .as_deref(),
    )?;
    record
        .active
        .as_mut()
        .context("active grant disappeared")?
        .flight = Some(flight(operation_id, "refresh_token")?);
    write_account(client, &record)?;
    let response = client.send_oauth(request.into_request()).await?;
    if response.status != 200 {
        if let Some(failure) = known_rejection(&response)? {
            record
                .active
                .as_mut()
                .context("active grant disappeared")?
                .flight = None;
            write_account(client, &record)?;
            return Ok(
                json!({"state":"rejected","error":failure,"grant_binding":record.active.as_ref().map(|g| &g.binding)}),
            );
        }
        return unknown_grant(
            client,
            record.active.as_ref().context("active grant disappeared")?,
        );
    }
    tokens.accept_refresh_response(response.status, &response.body, requested, resource)?;
    let active = record.active.as_mut().context("active grant disappeared")?;
    active.tokens.zeroize();
    active.tokens = encode_secret(tokens.export_node_local_secret()?);
    active.flight = None;
    active.last_completion = Some(completion(operation_id, "refresh_token", 200)?);
    write_account(client, &record)?;
    Ok(
        json!({"state":"refreshed","grant_binding":record.active.as_ref().map(|g| &g.binding),"account":tokens.metadata()}),
    )
}

async fn revoke(client: &CompanyClient, operation_id: EntityId) -> Result<Value> {
    let Some(mut record) = read_account(client.vault(), client.operator(), client.connection())?
    else {
        let pending = read_pending(client.vault(), client.operator(), client.connection())?;
        let unresolved = pending.as_ref().is_some_and(flow_may_have_issued_tokens);
        if !unresolved {
            client.vault().remove(&pending_ref(client.connection()))?;
        }
        return Ok(
            json!({"state":if unresolved {"incomplete"} else {"no_local_grants"},"merchant_account_signout_verified":false,
            "pending_sign_in_cancelled_locally":!unresolved,"pending_sign_in":pending.as_ref().filter(|_| unresolved).map(flow_status)}),
        );
    };
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    if let Some(pending) = read_pending(client.vault(), client.operator(), client.connection())?
        && record_has_flow(&record, &pending.flow_id)
    {
        finish_pending(client, &pending)?;
    }
    // Move active into the same persisted revoke queue. No new API request can
    // use a grant once disconnect starts, even if a later revoke is unknown.
    if let Some(active) = record.active.take() {
        record.replaced.push(active);
    }
    write_account(client, &record)?;
    let rejected = drain_replaced(client, &server, &mut record, operation_id).await?;
    let pending = read_pending(client.vault(), client.operator(), client.connection())?;
    let unresolved_flow = pending.as_ref().is_some_and(flow_may_have_issued_tokens);
    let remaining = record
        .replaced
        .iter()
        .map(|g| grant_status(client, g))
        .collect::<Result<Vec<_>>>()?;
    let complete = remaining.is_empty() && !unresolved_flow;
    if complete {
        let receipt = json!({"state":"known_grants_revoked","operation_id":operation_id,"revocations":record.revocations,
            "recorded_at_unix_ms":now_ms()?});
        write_secret(
            client.vault(),
            &format!("{}-revoked", client.connection().credential_ref()),
            &receipt,
        )?;
        client
            .vault()
            .remove(&client.connection().credential_ref())?;
        client.vault().remove(&pending_ref(client.connection()))?;
    }
    Ok(
        json!({"state":if complete {"revoked"} else {"incomplete"},"all_locally_known_grants_revoked":remaining.is_empty(),
        "revocations":record.revocations,"remaining_grants":remaining,"rejected":rejected,
        "unresolved_sign_in":pending.as_ref().filter(|_| unresolved_flow).map(flow_status),
        "merchant_account_signout_verified":false}),
    )
}

async fn join_browser(
    client: &CompanyClient,
    browser: &Arc<BrowserProvider>,
    mut request: PrivateFormRequest,
    caller: EntityId,
) -> Result<Value> {
    let join = BrowserJoin::from_discovery(&client.connection().report)?;
    if let Some(tokens) = client.tokens() {
        join.check_account(tokens)?;
    }
    request.endpoint = join.endpoint().as_str().into();
    let challenge = browser.read_private_form(request, caller).await?;
    let endpoint = Url::parse(challenge.endpoint())?;
    let pending = join.from_controlled_browser(
        challenge.run_id(),
        &endpoint,
        &endpoint,
        challenge.secret_value(),
    )?;
    let response = client.send_signed(pending.authorization_request()?).await?;
    if response.status != 204 {
        return Ok(
            json!({"state":"browser_join_not_authorized","observed_http_status":response.status,
            "browser_form_submitted":false,"application_effect":"unverified"}),
        );
    }
    let authorized = pending.accept_authorized(response.status)?;
    let submission = authorized.submission(challenge.run_id(), &endpoint)?;
    ensure!(
        submission.endpoint() == &endpoint
            && submission.hidden_code_for_native_adapter() == challenge.secret_value(),
        "private browser form binding changed"
    );
    let receipt = browser.submit_private_form(challenge, caller).await?;
    Ok(json!({"state":"browser_join_observed","receipt":receipt,
        "signed_in_authorization":client.tokens().is_some(),"website_declares_unscoped_access":join.website_has_full_account_access(),
        "merchant_session_cookie_verified":false}))
}

async fn install_grant(
    client: &CompanyClient,
    pending: &PendingFlow,
    tokens: AccountTokens,
    operation_id: EntityId,
) -> Result<Value> {
    let mut record = read_account(client.vault(), client.operator(), client.connection())?
        .unwrap_or_else(|| empty_account(client));
    if let Some(active) = record.active.take() {
        record.replaced.push(active);
    }
    let binding = uuid::Uuid::new_v4().to_string();
    let metadata = tokens.metadata();
    record.active = Some(GrantRecord {
        binding: binding.clone(),
        tokens: encode_secret(tokens.export_node_local_secret()?),
        source_flow: pending.flow_id.clone(),
        flight: None,
        last_completion: Some(completion(operation_id, "new_grant", 200)?),
    });
    // This atomic write precedes deleting pending state. source_flow allows a
    // local recovery to recognize successful persistence without token replay.
    write_account(client, &record)?;
    let server = OAuthServer::from_discovery(&client.connection().report)?;
    let rejected = drain_replaced(client, &server, &mut record, operation_id).await?;
    finish_pending(client, pending)?;
    let unknown = record.replaced.iter().any(|g| g.flight.is_some());
    Ok(
        json!({"state":"authorized","status":if unknown {"unknown"} else {"observed"},
        "effect_observation":if unknown {"unknown"} else {"complete"},"grant_binding":binding,"account":metadata,
        "replaced_grants_pending_revocation":record.replaced.len(),"replaced_revocation_rejections":rejected}),
    )
}

async fn drain_replaced(
    client: &CompanyClient,
    server: &OAuthServer,
    record: &mut AccountRecord,
    operation_id: EntityId,
) -> Result<Vec<Value>> {
    let mut index = 0;
    let mut rejected = Vec::new();
    while index < record.replaced.len() {
        let grant = &record.replaced[index];
        if grant.flight.is_some() {
            index += 1;
            continue;
        }
        let tokens = restore_tokens(grant, client.operator(), client.connection())?;
        let request = tokens.revocation_request(server, &client.operator().signer)?;
        record.replaced[index].flight = Some(flight(operation_id, "revocation")?);
        write_account(client, record)?;
        let response = client.send_oauth(request.into_request()).await?;
        if response.status == 200 {
            let revoked = record.replaced.remove(index);
            record.revocations.push(
                json!({"grant_binding":revoked.binding,"operation_id":operation_id,
                "source_flow":revoked.source_flow,"observed_http_status":200,"recorded_at_unix_ms":now_ms()?}),
            );
            write_account(client, record)?;
        } else if let Some(failure) = known_rejection(&response)? {
            record.replaced[index].flight = None;
            rejected.push(json!({"grant_binding":record.replaced[index].binding,"error":failure}));
            write_account(client, record)?;
            index += 1;
        } else {
            index += 1;
        }
    }
    Ok(rejected)
}

fn recovered_grant(client: &CompanyClient, pending: &PendingFlow) -> Result<Option<Value>> {
    let Some(record) = read_account(client.vault(), client.operator(), client.connection())? else {
        return Ok(None);
    };
    let active = record
        .active
        .as_ref()
        .filter(|grant| grant.source_flow == pending.flow_id);
    let grant = active.or_else(|| {
        record
            .replaced
            .iter()
            .find(|g| g.source_flow == pending.flow_id)
    });
    let Some(grant) = grant else {
        if record_has_flow(&record, &pending.flow_id) {
            finish_pending(client, pending)?;
            return Ok(Some(
                json!({"state":"revoked","status":"observed","effect_observation":"none",
                "recovered_local_result":true,"flow_id":pending.flow_id}),
            ));
        }
        return Ok(None);
    };
    let unknown = grant.flight.is_some() || record.replaced.iter().any(|g| g.flight.is_some());
    let value = json!({"state":if active.is_some() {"authorized"} else {"retired"},"status":if unknown {"unknown"} else {"observed"},
        "effect_observation":if unknown {"unknown"} else {"none"},"grant":grant_status(client,grant)?,"recovered_local_result":true,
        "replaced_grants_pending_revocation":record.replaced.len()});
    finish_pending(client, pending)?;
    Ok(Some(value))
}

fn finish_pending(client: &CompanyClient, pending: &PendingFlow) -> Result<()> {
    // Archived unknown exchanges must not disappear when a later flow succeeds.
    // Keep a small index so status/revoke can report unresolved token issuance.
    if pending.archived.is_empty() {
        client.vault().remove(&pending_ref(client.connection()))?;
    } else {
        let completed = PendingFlow {
            version: 1,
            company_identifier: pending.company_identifier.clone(),
            user_id: pending.user_id.clone(),
            client_id: pending.client_id.clone(),
            flow_id: pending.flow_id.clone(),
            kind: pending.kind,
            completed: true,
            secret_state: None,
            flight: None,
            archived: pending.archived.clone(),
        };
        write_pending(client, &completed)?;
    }
    Ok(())
}

fn rejected_flow(
    client: &CompanyClient,
    pending: &mut PendingFlow,
    response: &HttpObservation,
) -> Result<Value> {
    if let Some(failure) = known_rejection(response)? {
        pending.flight = None;
        write_pending(client, pending)?;
        Ok(
            json!({"state":"rejected","flow_id":pending.flow_id,"error":failure,"existing_account_unchanged":true}),
        )
    } else {
        Ok(unknown_flow(pending))
    }
}

fn known_rejection(
    response: &HttpObservation,
) -> Result<Option<shenko_compat_poppy::auth::OAuthFailure>> {
    if response.status == 429 {
        // The shared transport persisted/enforces Retry-After before exposing
        // this response. A rate-limit rejection did not issue or revoke a token.
        return Ok(Some(shenko_compat_poppy::auth::OAuthFailure {
            status: 429,
            error: "rate_limited".into(),
        }));
    }
    if !matches!(response.status, 400 | 401 | 403) {
        return Ok(None);
    }
    let Ok(Some(failure)) = oauth_failure(response.status, &response.body) else {
        return Ok(None);
    };
    if !matches!(
        failure.error.as_str(),
        "invalid_request"
            | "invalid_client"
            | "invalid_grant"
            | "unauthorized_client"
            | "unsupported_grant_type"
            | "invalid_scope"
            | "invalid_target"
            | "access_denied"
            | "authorization_pending"
            | "slow_down"
            | "expired_token"
            | "use_dpop_nonce"
            | "invalid_dpop_proof"
    ) {
        return Ok(None);
    }
    if failure.error == "use_dpop_nonce" && !response.headers.contains_key("dpop-nonce") {
        return Ok(None);
    }
    Ok(Some(failure))
}

fn new_flow(
    client: &CompanyClient,
    kind: FlowKind,
    secret_state: Option<String>,
) -> Result<PendingFlow> {
    let mut archived = Vec::new();
    if let Some(previous) = read_pending(client.vault(), client.operator(), client.connection())? {
        archived.extend(previous.archived.iter().cloned());
        let completed = read_account(client.vault(), client.operator(), client.connection())?
            .is_some_and(|record| record_has_flow(&record, &previous.flow_id));
        if previous.flight.is_some() && !completed {
            let reference = format!("oauth-flow-{}", previous.flow_id);
            write_secret(client.vault(), &reference, &previous)?;
            archived.push(ArchivedFlow {
                reference,
                flow_id: previous.flow_id.clone(),
                flight: previous.flight.clone(),
            });
        }
    }
    let pending = PendingFlow {
        version: 1,
        company_identifier: client.connection().report.company_identifier.clone(),
        user_id: client.connection().user_id.clone(),
        client_id: client.operator().config.client_id.clone(),
        flow_id: uuid::Uuid::new_v4().to_string(),
        kind,
        completed: false,
        secret_state,
        flight: None,
        archived,
    };
    write_pending(client, &pending)?;
    Ok(pending)
}

fn grant_status(client: &CompanyClient, grant: &GrantRecord) -> Result<Value> {
    let tokens = restore_tokens(grant, client.operator(), client.connection())?;
    Ok(
        json!({"binding":grant.binding,"account":tokens.metadata(),"quarantined":grant.flight.is_some(),
        "original_flight":grant.flight,"last_completion":grant.last_completion}),
    )
}

fn unknown_grant(client: &CompanyClient, grant: &GrantRecord) -> Result<Value> {
    Ok(json!({"state":"unknown","grant":grant_status(client,grant)?,"automatic_replay":false}))
}

fn unknown_flow(pending: &PendingFlow) -> Value {
    json!({"state":"unknown","pending_sign_in":flow_status(pending),"automatic_replay":false,"existing_account_unchanged":true})
}

fn flow_status(pending: &PendingFlow) -> Value {
    json!({"flow_id":pending.flow_id,"method":pending.kind,"completed":pending.completed,"original_flight":pending.flight,
        "quarantined":pending.flight.is_some(),"archived_unresolved":pending.archived})
}

fn flow_may_have_issued_tokens(pending: &PendingFlow) -> bool {
    let is_token =
        |flight: &Flight| matches!(flight.kind.as_str(), "authorization_code" | "device_token");
    pending.flight.as_ref().is_some_and(is_token)
        || pending
            .archived
            .iter()
            .any(|flow| flow.flight.as_ref().is_some_and(is_token))
}

fn record_has_flow(record: &AccountRecord, flow: &str) -> bool {
    record
        .active
        .as_ref()
        .is_some_and(|g| g.source_flow == flow)
        || record.replaced.iter().any(|g| g.source_flow == flow)
        || record
            .revocations
            .iter()
            .any(|r| r.get("source_flow").and_then(Value::as_str) == Some(flow))
}

fn empty_account(client: &CompanyClient) -> AccountRecord {
    AccountRecord {
        version: 1,
        company_identifier: client.connection().report.company_identifier.clone(),
        user_id: client.connection().user_id.clone(),
        client_id: client.operator().config.client_id.clone(),
        active: None,
        replaced: Vec::new(),
        revocations: Vec::new(),
    }
}

fn read_account(
    vault: &Vault,
    operator: &Operator,
    connection: &Connection,
) -> Result<Option<AccountRecord>> {
    let Some(bytes) = vault.read(&connection.credential_ref())? else {
        return Ok(None);
    };
    let record: AccountRecord = parse_secret(&bytes, "invalid stored account record")?;
    ensure!(
        record.version == 1
            && record.company_identifier == connection.report.company_identifier
            && record.user_id == connection.user_id
            && record.client_id == operator.config.client_id,
        "stored account record binding mismatch"
    );
    Ok(Some(record))
}

fn read_pending(
    vault: &Vault,
    operator: &Operator,
    connection: &Connection,
) -> Result<Option<PendingFlow>> {
    let Some(bytes) = vault.read(&pending_ref(connection))? else {
        return Ok(None);
    };
    let pending: PendingFlow = parse_secret(&bytes, "invalid stored sign-in flow")?;
    ensure!(
        pending.version == 1
            && pending.company_identifier == connection.report.company_identifier
            && pending.user_id == connection.user_id
            && pending.client_id == operator.config.client_id,
        "stored sign-in flow binding mismatch"
    );
    Ok(Some(pending))
}

fn restore_tokens(
    grant: &GrantRecord,
    operator: &Operator,
    connection: &Connection,
) -> Result<AccountTokens> {
    let bytes = decode_secret(&grant.tokens)?;
    AccountTokens::restore_node_local_secret(
        &bytes,
        &operator.config.client_id,
        &connection.identity()?,
    )
}

fn write_account(client: &CompanyClient, account: &AccountRecord) -> Result<()> {
    write_secret(
        client.vault(),
        &client.connection().credential_ref(),
        account,
    )
}

fn write_pending(client: &CompanyClient, pending: &PendingFlow) -> Result<()> {
    write_secret(client.vault(), &pending_ref(client.connection()), pending)
}

fn pending_ref(connection: &Connection) -> String {
    format!("{}-pending", connection.credential_ref())
}

fn write_secret<T: Serialize>(vault: &Vault, reference: &str, value: &T) -> Result<()> {
    let bytes = Zeroizing::new(serde_json::to_vec(value)?);
    vault.write(reference, &bytes)
}

fn parse_secret<T: serde::de::DeserializeOwned>(bytes: &[u8], message: &'static str) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| anyhow!(message))
}

fn encode_secret(bytes: Vec<u8>) -> String {
    STANDARD.encode(Zeroizing::new(bytes))
}

fn decode_secret(encoded: &str) -> Result<Zeroizing<Vec<u8>>> {
    STANDARD
        .decode(encoded)
        .map(Zeroizing::new)
        .map_err(|_| anyhow!("invalid encoded credential state"))
}

fn decode_state(pending: &PendingFlow) -> Result<Zeroizing<Vec<u8>>> {
    decode_secret(
        pending
            .secret_state
            .as_deref()
            .context("sign-in response was not saved before interruption")?,
    )
}

fn flight(operation_id: EntityId, kind: &str) -> Result<Flight> {
    Ok(Flight {
        operation_id: operation_id.to_string(),
        kind: kind.into(),
        started_at_unix_ms: now_ms()?,
    })
}

fn completion(operation_id: EntityId, kind: &str, status: u16) -> Result<Completion> {
    Ok(Completion {
        operation_id: operation_id.to_string(),
        kind: kind.into(),
        observed_http_status: status,
        recorded_at_unix_ms: now_ms()?,
    })
}

fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock precedes Unix epoch")?
        .as_millis()
        .try_into()
        .context("clock value exceeds persisted range")
}
