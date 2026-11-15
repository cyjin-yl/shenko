//! Private two-phase form bridge for the existing owned browser.
//!
//! A protocol adapter can authorize a cookie-bound hidden value between these
//! phases. The value, form data and evaluated scripts never enter observations,
//! ActionReport, Store or model context. This module has no protocol-specific
//! dependency and does not send authorization headers or copy browser cookies.
//!
//! The caller owns durable intent/dispatch recording before both browser I/O
//! phases and any external authorization between them. A challenge cannot be
//! serialized or restored after a browser restart. Submission consumes it;
//! failures and missing observations never trigger an automatic second POST.

use std::{
    future::{Future, poll_fn},
    sync::{Arc, atomic::Ordering},
    task::Poll,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;
use tokio_tungstenite::tungstenite::http::Uri;

use super::{
    BrowserProvider, BrowserSession, ControllerRole, Lease, MAX_URL,
    protocol::{MAX_SCRIPT_BYTES, TabHandle},
};

const FORM_LIMIT: usize = 32 * 1024;
const FIELD_LIMIT: usize = 8 * 1024;
const LOAD_TIMEOUT: Duration = Duration::from_secs(12);
const CHALLENGE_LIFETIME: Duration = Duration::from_secs(120);

/// The host revalidates the exact operation's current authority. This is
/// separate from a browser lease: an unchanged lease cannot revive a cancelled
/// or fenced task. The bridge never interprets the host's protocol or ledger.
pub type PrivateFormGuard = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Only public binding facts. The protocol adapter derives endpoint and field
/// name from its verified protocol metadata, never from a model-provided code.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivateFormRequest {
    pub session_id: EntityId,
    pub tab_id: Option<String>,
    pub lease_token: EntityId,
    pub controller_epoch: u64,
    pub endpoint: String,
    pub field_name: String,
}

/// This capability refers to one exact form in one live document. Secret
/// access is for a trusted native adapter; no Debug, Clone or Serialize exists.
pub struct PrivateFormChallenge {
    session_id: EntityId,
    run_id: String,
    caller: EntityId,
    tab: TabHandle,
    lease_token: EntityId,
    controller_epoch: u64,
    endpoint: String,
    field_name: String,
    field_value: String,
    form_index: u64,
    form_data: String,
    document_identity: String,
    captured_at: Instant,
}

impl PrivateFormChallenge {
    pub fn secret_value(&self) -> &str {
        &self.field_value
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn session_id(&self) -> EntityId {
        self.session_id
    }
    pub fn tab_id(&self) -> &str {
        &self.tab.id
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn controller_epoch(&self) -> u64 {
        self.controller_epoch
    }
}

/// Browser transport and observations only. In particular `submitted` is not
/// evidence that a merchant associated the intended Session/account cookie.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivateFormReceipt {
    pub session_id: EntityId,
    pub tab_id: String,
    pub controller_epoch: u64,
    /// "submitted", "refused", or "unknown".
    pub transport: String,
    pub navigation_observed: bool,
    /// Available only when the browser exposes responseStatus for the newly
    /// loaded document. Zero/missing is unknown, not a fabricated HTTP 200.
    pub observed_http_status: Option<u16>,
    /// Query, fragment, path and user information are never exposed here.
    pub final_origin: Option<String>,
    /// Always unverified: transport observations do not prove application state.
    pub application_effect: String,
}

impl BrowserProvider {
    pub async fn read_private_form(
        &self,
        request: PrivateFormRequest,
        caller: EntityId,
        guard: PrivateFormGuard,
    ) -> Result<PrivateFormChallenge> {
        check_operation(&guard)?;
        validate_endpoint(&request.endpoint)?;
        ensure!(
            !request.field_name.is_empty()
                && request.field_name.len() <= 64
                && request
                    .field_name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid private form field name"
        );
        let session = self.session(request.session_id)?;
        let tab = guarded_protocol(
            &guard,
            self.resolve_tab(&session, request.tab_id.as_deref()),
        )
        .await?
        .handle;
        // Hold the existing input lease across navigation and private capture.
        // The human input observer still changes the epoch synchronously; it
        // is checked before every protocol operation and after every await.
        let lease = session.lease.lock().await;
        check_operation(&guard)?;
        check_lease(
            &session,
            lease.as_ref(),
            caller,
            request.lease_token,
            request.controller_epoch,
        )?;
        let before = private_evaluate(
            &session,
            &tab,
            "JSON.stringify({stamp: String(performance.timeOrigin)})",
            &guard,
        )
        .await?;
        let before_stamp = before
            .get("stamp")
            .and_then(Value::as_str)
            .context("browser did not identify its current document")?
            .to_owned();
        check_lease(
            &session,
            lease.as_ref(),
            caller,
            request.lease_token,
            request.controller_epoch,
        )?;
        let nav = tokio::time::timeout(
            LOAD_TIMEOUT,
            guarded_protocol(&guard, session.protocol.navigate(&tab, &request.endpoint)),
        )
        .await
        .map_err(|_| anyhow!("private form navigation timed out; outcome unknown"))?
        .map_err(|_| anyhow!("private form navigation failed; outcome unknown"))?;
        ensure!(
            nav.error_text.is_none(),
            "browser refused private form navigation"
        );
        session.observations.lock().await.remove(&tab.id);
        check_operation(&guard)?;
        check_lease(
            &session,
            lease.as_ref(),
            caller,
            request.lease_token,
            request.controller_epoch,
        )?;
        let config = json!({"endpoint":request.endpoint,"field":request.field_name,"before":before_stamp,
            "fieldLimit":FIELD_LIMIT,"formLimit":FORM_LIMIT});
        let script = format!("({READ_FORM})({})", serde_json::to_string(&config)?);
        let started = Instant::now();
        let captured = loop {
            check_lease(
                &session,
                lease.as_ref(),
                caller,
                request.lease_token,
                request.controller_epoch,
            )?;
            let value = private_evaluate(&session, &tab, &script, &guard).await?;
            check_lease(
                &session,
                lease.as_ref(),
                caller,
                request.lease_token,
                request.controller_epoch,
            )?;
            if value.get("ready").and_then(Value::as_bool) == Some(true) {
                break value;
            }
            ensure!(
                value.get("rejected").and_then(Value::as_bool) != Some(true),
                "private form page or form differs from the exact endpoint"
            );
            ensure!(
                started.elapsed() < LOAD_TIMEOUT,
                "private form did not become ready before timeout"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let field_value = private_string(&captured, "value", FIELD_LIMIT)?;
        let form_data = private_string(&captured, "formData", FORM_LIMIT)?;
        let document_identity = private_string(&captured, "stamp", 128)?;
        let form_index = captured
            .get("index")
            .and_then(Value::as_u64)
            .context("private form index is absent")?;
        let challenge = PrivateFormChallenge {
            session_id: session.session_id,
            run_id: session.run_id.clone(),
            caller,
            tab,
            lease_token: request.lease_token,
            controller_epoch: request.controller_epoch,
            endpoint: request.endpoint,
            field_name: request.field_name,
            field_value,
            form_data,
            form_index,
            document_identity,
            captured_at: Instant::now(),
        };
        drop(lease);
        Ok(challenge)
    }

    /// Call only after the protocol's authorization response was observed.
    /// The same original form is submitted through this browser's cookie jar;
    /// no token, global extra header, new tab or replacement HTTP client is used.
    pub async fn submit_private_form(
        &self,
        challenge: PrivateFormChallenge,
        caller: EntityId,
        guard: PrivateFormGuard,
    ) -> Result<PrivateFormReceipt> {
        check_operation(&guard)?;
        ensure!(
            caller == challenge.caller,
            "private form belongs to another controller"
        );
        ensure!(
            challenge.captured_at.elapsed() <= CHALLENGE_LIFETIME,
            "private form observation expired"
        );
        let session = self.session(challenge.session_id)?;
        ensure!(
            session.run_id == challenge.run_id,
            "private form belongs to an earlier browser run"
        );
        let tab = guarded_protocol(&guard, self.resolve_tab(&session, Some(&challenge.tab.id)))
            .await?
            .handle;
        ensure!(tab == challenge.tab, "private form tab changed");
        let lease = session.lease.lock().await;
        check_operation(&guard)?;
        check_lease(
            &session,
            lease.as_ref(),
            caller,
            challenge.lease_token,
            challenge.controller_epoch,
        )?;
        let config = json!({"endpoint":challenge.endpoint,"field":challenge.field_name,"value":challenge.field_value,
            "index":challenge.form_index,"stamp":challenge.document_identity,"formData":challenge.form_data,
            "formLimit":FORM_LIMIT});
        let script = format!("({SUBMIT_FORM})({})", serde_json::to_string(&config)?);
        // All DOM and form checks are repeated in the same script immediately
        // before the browser's native form submission, closing the page race.
        check_lease(
            &session,
            lease.as_ref(),
            caller,
            challenge.lease_token,
            challenge.controller_epoch,
        )?;
        let dispatched = private_evaluate(&session, &tab, &script, &guard).await;
        session.observations.lock().await.remove(&tab.id);
        check_operation(&guard)?;
        let mut receipt = PrivateFormReceipt {
            session_id: session.session_id,
            tab_id: tab.id.clone(),
            controller_epoch: challenge.controller_epoch,
            transport: "unknown".into(),
            navigation_observed: false,
            observed_http_status: None,
            final_origin: None,
            application_effect: "unverified".into(),
        };
        if let Ok(value) = dispatched {
            if value.get("submitted").and_then(Value::as_bool) == Some(false) {
                receipt.transport = "refused".into();
                return Ok(receipt);
            }
            if value.get("submitted").and_then(Value::as_bool) == Some(true) {
                receipt.transport = "submitted".into();
            }
        }
        // A navigation can destroy the evaluating context before its ACK.
        // Observe the current document once it settles, without retrying POST.
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if check_operation(&guard).is_err()
                || check_lease(
                    &session,
                    lease.as_ref(),
                    caller,
                    challenge.lease_token,
                    challenge.controller_epoch,
                )
                .is_err()
            {
                break;
            }
            let observed = private_evaluate(&session, &tab, OBSERVE_NAVIGATION, &guard).await;
            if let Ok(value) = observed {
                let changed = value
                    .get("stamp")
                    .and_then(Value::as_str)
                    .is_some_and(|stamp| stamp != challenge.document_identity);
                if changed && value.get("complete").and_then(Value::as_bool) == Some(true) {
                    receipt.navigation_observed = true;
                    receipt.observed_http_status = value
                        .get("status")
                        .and_then(Value::as_u64)
                        .and_then(|status| u16::try_from(status).ok())
                        .filter(|status| (100..=599).contains(status));
                    receipt.final_origin = value
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(redacted_origin);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(lease);
        Ok(receipt)
    }
}

fn check_lease(
    session: &Arc<BrowserSession>,
    lease: Option<&Lease>,
    caller: EntityId,
    token: EntityId,
    epoch: u64,
) -> Result<()> {
    ensure!(
        !session.closed.load(Ordering::Relaxed),
        "private form browser session is closed"
    );
    let lease = lease.context("private form requires an existing input lease")?;
    ensure!(
        lease.token == token && lease.holder == caller && lease.role == ControllerRole::Agent,
        "private form input lease is stale or belongs to another controller"
    );
    ensure!(
        lease.epoch == epoch && session.epoch.load(Ordering::Relaxed) == epoch,
        "private form controller epoch changed"
    );
    Ok(())
}

async fn private_evaluate(
    session: &Arc<BrowserSession>,
    tab: &TabHandle,
    script: &str,
    guard: &PrivateFormGuard,
) -> Result<Value> {
    ensure!(
        script.len() <= MAX_SCRIPT_BYTES,
        "private form script exceeds protocol bound"
    );
    // Protocol errors may embed page exception text or script inputs. Keep
    // them private instead of attaching their source chain to an observation.
    tokio::time::timeout(
        LOAD_TIMEOUT,
        guarded_protocol(guard, session.protocol.evaluate_json(tab, script)),
    )
    .await
    .map_err(|_| anyhow!("private form browser evaluation timed out; outcome unknown"))?
    .map_err(|_| anyhow!("private form browser evaluation failed; outcome unknown"))
}

fn check_operation(guard: &PrivateFormGuard) -> Result<()> {
    // The callback is a native host boundary. Its source chain is unnecessary
    // here and must not accidentally expose private host data in a receipt.
    guard().map_err(|_| anyhow!("private form operation lost authority; outcome unknown"))
}

async fn guarded_protocol<T>(
    guard: &PrivateFormGuard,
    request: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut request = Box::pin(request);
    let result = poll_fn(|context| {
        if let Err(error) = check_operation(guard) {
            return Poll::Ready(Err(error));
        }
        request.as_mut().poll(context)
    })
    .await;
    // Protocol helpers may await internally before dispatch. Rechecking on
    // every resumed poll prevents a fenced operation continuing after that
    // wait. A command already queued or sent may still complete: no cancellation
    // path asserts that its remote effect was stopped or permits a second POST.
    check_operation(guard)?;
    result
}

fn private_string(value: &Value, name: &str, limit: usize) -> Result<String> {
    let text = value
        .get(name)
        .and_then(Value::as_str)
        .context("private form field is absent")?;
    ensure!(
        !text.is_empty() && text.len() <= limit,
        "private form field exceeds bound"
    );
    Ok(text.into())
}

fn validate_endpoint(text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_URL
            && !text.contains('#')
            && !text.bytes().any(|b| b.is_ascii_control() || b == b'\\'),
        "invalid private form endpoint"
    );
    let uri: Uri = text
        .parse()
        .map_err(|_| anyhow!("invalid private form endpoint"))?;
    ensure!(
        matches!(uri.scheme_str(), Some("https" | "http"))
            && uri
                .authority()
                .is_some_and(|authority| !authority.as_str().contains('@')),
        "private form endpoint must be an absolute HTTP(S) URL without credentials"
    );
    Ok(())
}

fn redacted_origin(text: &str) -> Option<String> {
    let text = text.split(['?', '#']).next()?;
    let uri: Uri = text.parse().ok()?;
    let scheme = uri.scheme_str()?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = uri.authority()?.as_str();
    if authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

const READ_FORM: &str = r#"function(c) {
  const stamp = String(performance.timeOrigin);
  if (stamp === c.before) return JSON.stringify({ready:false});
  if (location.href !== new URL(c.endpoint).href) {
    return JSON.stringify({ready:false,rejected:stamp !== c.before});
  }
  if (document.readyState === 'loading') return JSON.stringify({ready:false});
  const candidates = [];
  for (let i = 0; i < document.forms.length; i++) {
    const f = document.forms[i];
    const fields = Array.from(f.elements).filter(e => e.name === c.field);
    if (fields.length !== 1 || fields[0].tagName !== 'INPUT' || fields[0].type !== 'hidden') continue;
    if (f.method.toLowerCase() !== 'post' || new URL(f.action,location.href).href !== new URL(c.endpoint).href) continue;
    if (f.target && f.target !== '_self') continue;
    const field = fields[0];
    if (field.disabled || !field.value || field.value.length > c.fieldLimit) continue;
    const values = Array.from(new FormData(f).entries());
    if (values.some(pair => typeof pair[1] !== 'string')) continue;
    const formData = JSON.stringify(values);
    if (formData.length > c.formLimit) continue;
    candidates.push({ready:true,index:i,stamp:stamp,value:field.value,formData:formData});
  }
  return JSON.stringify(candidates.length === 1 ? candidates[0] : {ready:false,rejected:document.readyState === 'complete'});
}"#;

const SUBMIT_FORM: &str = r#"function(c) {
  const refuse = () => JSON.stringify({submitted:false});
  if (location.href !== new URL(c.endpoint).href || String(performance.timeOrigin) !== c.stamp) return refuse();
  const f = document.forms[c.index];
  if (!f || f.method.toLowerCase() !== 'post' || new URL(f.action,location.href).href !== new URL(c.endpoint).href) return refuse();
  if (f.target && f.target !== '_self') return refuse();
  const fields = Array.from(f.elements).filter(e => e.name === c.field);
  if (fields.length !== 1 || fields[0].tagName !== 'INPUT' || fields[0].type !== 'hidden' || fields[0].disabled || fields[0].value !== c.value) return refuse();
  const values = Array.from(new FormData(f).entries());
  if (values.some(pair => typeof pair[1] !== 'string')) return refuse();
  const formData = JSON.stringify(values);
  if (formData.length > c.formLimit || formData !== c.formData) return refuse();
  HTMLFormElement.prototype.submit.call(f);
  return JSON.stringify({submitted:true});
}"#;

const OBSERVE_NAVIGATION: &str = r#"JSON.stringify({
  stamp: String(performance.timeOrigin), complete: document.readyState === 'complete',
  url: location.href, status: (performance.getEntriesByType('navigation')[0] || {}).responseStatus || null
})"#;
