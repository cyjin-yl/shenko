//! Page actions, controller epoch/lease control and identity operations.
//!
//! SPEC §24.3/§24.4/§24.6.2: an action is bound to one observation and one
//! live controller epoch. A stale observation, a revoked lease or a page that
//! changed underneath the agent is rejected before anything is sent. A
//! delivered action whose application effect cannot be seen is recorded as
//! unknown — never as a success, and never replayed.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use shenko_types::ids::EntityId;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::protocol::TabHandle;
use super::engine::EngineFamily;
use super::{
    ActionReceipt, ActionReport, ActionRequest, BrowserProvider, BrowserSession, ControlFacts, ControlRequest,
    ControllerRole, IdentityFacts, IdentityRequest, ObserveOptions, PageAction, StorageState,
    now_rfc3339,
};

/// Longest a `wait_for` action may poll.
const MAX_WAIT_MS: u64 = 30_000;
/// Longest accepted typed text.
const MAX_TYPED_CHARS: usize = 4096;
/// Longest accepted action script.
const MAX_SCRIPT_BYTES: usize = 64 * 1024;

/// Read the focused element's current value length, so a typed text can be
/// verified instead of merely assumed.
const ACTIVE_VALUE_SCRIPT: &str = r#"(function () {
  const el = document.activeElement;
  if (!el) return JSON.stringify({ok: false});
  const value = el.value === undefined ? "" : String(el.value);
  return JSON.stringify({ok: true, length: value.length, tag: el.tagName.toLowerCase()});
})()"#;

impl BrowserProvider {
    /// Perform a bounded list of actions on one tab of a live session.
    pub async fn act(&self, request: ActionRequest, caller: EntityId) -> Result<ActionReport> {
        let session = self.session(request.session_id)?;
        ensure!(
            !request.actions.is_empty() && request.actions.len() <= 32,
            "actions must be 1..=32 entries"
        );
        // The lease is checked before anything is sent: a human takeover
        // moves the epoch, and the agent's late input is rejected.
        self.check_input_lease(&session, &request, caller).await?;
        let tab = self.resolve_tab(&session, request.tab_id.as_deref()).await?;

        // The observation the caller bound to must be one this session
        // actually produced, and the page must still be the page it saw.
        let bound = {
            let observations = session.observations.lock().await;
            observations.get(&tab.handle.id).cloned()
        };
        let Some(bound) = bound else {
            bail!("this browser session has no observation to bind the action to; observe first");
        };
        ensure!(
            bound.observation_id == request.observation_id,
            "the observation id does not belong to the current observation of this tab"
        );
        let current = self
            .observe_tab(&session, &tab.handle, &ObserveOptions::default())
            .await
            .context("re-read the page before acting")?;
        ensure!(
            current.identity_digest == bound.identity_digest,
            "the page changed since the observation ({} -> {}); observe again before acting",
            bound.identity_digest,
            current.identity_digest
        );

        // The lease and the controller epoch are re-validated before every
        // dispatch, not only before the batch: a takeover may land while an
        // earlier action (for example a wait_for) is still running, and every
        // later action must then be refused instead of reaching the human's
        // session.
        let mut receipts = Vec::new();
        for (index, action) in request.actions.iter().enumerate() {
            if let Err(error) = self.check_input_lease(&session, &request, caller).await {
                for (skipped, remaining) in
                    request.actions.iter().enumerate().skip(receipts.len())
                {
                    receipts.push(ActionReceipt {
                        index: skipped,
                        kind: remaining.kind().to_string(),
                        transport: "refused".into(),
                        effect: "none".into(),
                        detail: Some(format!(
                            "control of this session changed while the batch was running; \
                             the action was never dispatched: {error}"
                        )),
                        observed_effect: None,
                    });
                }
                break;
            }
            let receipt = self
                .run_action(&session, &tab.handle, action, index)
                .await;
            receipts.push(receipt);
        }
        let after = self
            .observe_tab(&session, &tab.handle, &ObserveOptions::default())
            .await
            .ok();
        if let Some(after) = after.as_ref() {
            let mut observations = session.observations.lock().await;
            observations.insert(tab.handle.id.clone(), after.clone());
        }
        let effect = aggregate_effect(&receipts);
        Ok(ActionReport {
            session_id: session.session_id,
            tab_id: tab.handle.id.clone(),
            observation_id: request.observation_id,
            controller_epoch: session.epoch.load(Ordering::Relaxed),
            receipts,
            after,
            effect,
        })
    }

    /// Verify the caller still holds this session's input lease at the
    /// current controller epoch. A human takeover moves the epoch and drops
    /// the agent's lease, so late input is rejected instead of racing the
    /// human (SPEC §24.3, §24.6.2).
    async fn check_input_lease(
        &self,
        session: &Arc<BrowserSession>,
        request: &ActionRequest,
        caller: EntityId,
    ) -> Result<()> {
        let lease = session.lease.lock().await;
        let Some(lease) = lease.as_ref() else {
            bail!("no input lease is held for this browser session; acquire control first");
        };
        ensure!(
            lease.token == request.lease_token,
            "lease token does not match the current input lease"
        );
        ensure!(
            lease.holder == caller,
            "the input lease belongs to a different controller instance"
        );
        ensure!(
            lease.epoch == session.epoch.load(Ordering::Relaxed),
            "the controller epoch moved (a human took over this session); \
             take a fresh observation and acquire a new lease before acting"
        );
        ensure!(
            lease.role == ControllerRole::Agent,
            "a human currently holds control of this browser session"
        );
        Ok(())
    }

    /// Run one action and describe what actually happened.
    async fn run_action(
        &self,
        session: &Arc<BrowserSession>,
        tab: &TabHandle,
        action: &PageAction,
        index: usize,
    ) -> ActionReceipt {
        let kind = action.kind();
        let mut receipt = ActionReceipt {
            index,
            kind: kind.to_string(),
            transport: "acked".into(),
            effect: "unknown".into(),
            detail: None,
            observed_effect: None,
        };
        match action {
            PageAction::Navigate { url } => {
                if url.len() > 4096 {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("url is too long".into());
                    return receipt;
                }
                match session.protocol.navigate(tab, url).await {
                    Ok(outcome) => {
                        if let Some(error) = outcome.error_text {
                            receipt.effect = "none".into();
                            receipt.detail = Some(format!("engine refused the navigation: {error}"));
                        } else {
                            receipt.effect = "complete".into();
                            receipt.observed_effect = Some(outcome.url);
                        }
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.effect = "unknown".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Click { selector, text } => {
                if selector.is_none() && text.is_none() {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("a click needs a selector or a text".into());
                    return receipt;
                }
                let script = super::scripts::click_script(selector.as_deref(), text.as_deref());
                match session.protocol.evaluate_json(tab, &script).await {
                    Ok(result) => {
                        let found = result.get("found").and_then(Value::as_bool).unwrap_or(false);
                        if !found {
                            receipt.transport = "rejected".into();
                            receipt.effect = "none".into();
                            receipt.detail = Some("no element matched the requested target".into());
                        } else {
                            // The click was delivered to the page. Whether the
                            // application acted on it is only visible in the
                            // page's own state, so the effect stays unknown
                            // until the next observation shows a change.
                            receipt.effect = "unknown".into();
                            receipt.observed_effect = result.get("tag").and_then(Value::as_str).map(
                                |tag| format!("clicked a <{tag}> element"),
                            );
                        }
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Fill {
                selector,
                value,
                submit,
            } => {
                if value.len() > 64 * 1024 {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("value is too long".into());
                    return receipt;
                }
                let script = super::scripts::fill_script(selector, value, *submit);
                match session.protocol.evaluate_json(tab, &script).await {
                    Ok(result) => {
                        let found = result.get("found").and_then(Value::as_bool).unwrap_or(false);
                        if !found {
                            receipt.transport = "rejected".into();
                            receipt.effect = "none".into();
                            receipt.detail = Some("no element matched the requested field".into());
                        } else {
                            let submitted = result
                                .get("submitted")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let length = result.get("valueLen").and_then(Value::as_u64).unwrap_or(0);
                            receipt.effect = "partial".into();
                            receipt.observed_effect = Some(format!(
                                "the field now holds {length} characters{}",
                                if submitted {
                                    " and the form was submitted"
                                } else {
                                    ""
                                }
                            ));
                        }
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Type { text } => {
                if text.is_empty() || text.chars().count() > MAX_TYPED_CHARS {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("text must be 1..=4096 characters".into());
                    return receipt;
                }
                match session.protocol.insert_text(tab, text).await {
                    Ok(()) => match session.protocol.evaluate_json(tab, ACTIVE_VALUE_SCRIPT).await {
                        Ok(result) => {
                            let length = result
                                .get("length")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            receipt.effect = "partial".into();
                            receipt.observed_effect =
                                Some(format!("the focused field holds {length} characters"));
                        }
                        Err(error) => {
                            receipt.effect = "unknown".into();
                            receipt.detail = Some(format!(
                                "the key events were delivered but the field could not be read back: {error}"
                            ));
                        }
                    },
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Press { key } => {
                if key.is_empty() || key.len() > 32 {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("key name must be 1..=32 characters".into());
                    return receipt;
                }
                match session.protocol.press_key(tab, key).await {
                    Ok(()) => {
                        receipt.effect = "unknown".into();
                        receipt.observed_effect = Some(format!("pressed {key}"));
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Scroll { dy } => {
                match session.protocol.scroll(tab, *dy).await {
                    Ok(()) => {
                        receipt.effect = "unknown".into();
                        receipt.observed_effect = Some(format!("scrolled by {dy}"));
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::WaitFor {
                selector,
                timeout_ms,
            } => {
                let timeout = timeout_ms.unwrap_or(5_000).min(MAX_WAIT_MS);
                let script = super::scripts::exists_script(selector);
                let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
                let mut found = false;
                while tokio::time::Instant::now() < deadline {
                    match session.protocol.evaluate_json(tab, &script).await {
                        Ok(result) => {
                            if result.get("found").and_then(Value::as_bool).unwrap_or(false) {
                                found = true;
                                break;
                            }
                        }
                        Err(error) => {
                            receipt.transport = "failed".into();
                            receipt.detail = Some(error.to_string());
                            return receipt;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if found {
                    receipt.effect = "complete".into();
                    receipt.observed_effect = Some(format!("{selector} matched"));
                } else {
                    receipt.effect = "none".into();
                    receipt.detail = Some(format!("{selector} did not match within {timeout}ms"));
                }
            }
            PageAction::Evaluate { script } => {
                if script.len() > MAX_SCRIPT_BYTES {
                    receipt.transport = "rejected".into();
                    receipt.effect = "none".into();
                    receipt.detail = Some("script is too long".into());
                    return receipt;
                }
                match session.protocol.evaluate_json(tab, script).await {
                    Ok(result) => {
                        receipt.effect = "complete".into();
                        receipt.observed_effect = Some(truncate_json(&result, 512));
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
            PageAction::Screenshot => {
                match session.protocol.screenshot(tab).await {
                    Ok(bytes) => {
                        let sealed = self.store.cas.seal_object(
                            &bytes,
                            "image/png",
                            "browser-effects",
                            "browser-screenshot",
                            1,
                        );
                        match sealed {
                            Ok(sealed) => {
                                receipt.effect = "complete".into();
                                receipt.observed_effect =
                                    Some(sealed.object.digest.to_string());
                            }
                            Err(error) => {
                                self.store.close_external_writer();
                                receipt.transport = "failed".into();
                                receipt.detail = Some(error.to_string());
                            }
                        }
                    }
                    Err(error) => {
                        receipt.transport = "failed".into();
                        receipt.detail = Some(error.to_string());
                    }
                }
            }
        }
        receipt
    }

    // ------------------------------------------------------------------
    // Controller epoch and input lease
    // ------------------------------------------------------------------

    /// Manage the single controller of one session: acquire, release,
    /// human takeover, hand-back and status.
    pub async fn control(
        &self,
        request: ControlRequest,
        caller: EntityId,
    ) -> Result<ControlFacts> {
        let session = self.session(request.session_id)?;
        let mut lease = session.lease.lock().await;
        match request.op.as_str() {
            "acquire" => {
                if lease
                    .as_ref()
                    .is_some_and(|lease| lease.role == ControllerRole::Human)
                {
                    bail!("a human currently holds control of this browser session");
                }
                let token = EntityId::new();
                let epoch = session.epoch.load(Ordering::Relaxed);
                *lease = Some(super::Lease {
                    token,
                    holder: caller,
                    role: ControllerRole::Agent,
                    epoch,
                    acquired_at: now_rfc3339(),
                });
                Ok(ControlFacts {
                    session_id: session.session_id,
                    controller_epoch: epoch,
                    holder: Some(caller.to_string()),
                    role: Some(ControllerRole::Agent),
                    lease_token: Some(token),
                    acquired_at: lease.as_ref().map(|lease| lease.acquired_at.clone()),
                    display_domain_id: session.domain_id.clone(),
                    note: "the agent holds the input lease at this controller epoch".into(),
                })
            }
            "release" => {
                if let Some(current) = lease.as_ref() {
                    if let Some(token) = request.lease_token {
                        ensure!(
                            current.token == token,
                            "the lease token does not match the current input lease"
                        );
                    }
                    ensure!(
                        current.role == ControllerRole::Agent,
                        "a human holds control; use hand_back instead"
                    );
                }
                *lease = None;
                Ok(ControlFacts {
                    session_id: session.session_id,
                    controller_epoch: session.epoch.load(Ordering::Relaxed),
                    holder: None,
                    role: None,
                    lease_token: None,
                    acquired_at: None,
                    display_domain_id: session.domain_id.clone(),
                    note: "the input lease was released; a new lease must be acquired before acting"
                        .into(),
                })
            }
            "takeover" => {
                // A human takes over the live session: the epoch moves, so
                // every input the agent still holds becomes invalid.
                let epoch = session.epoch.fetch_add(1, Ordering::Relaxed) + 1;
                let token = EntityId::new();
                *lease = Some(super::Lease {
                    token,
                    holder: caller,
                    role: ControllerRole::Human,
                    epoch,
                    acquired_at: now_rfc3339(),
                });
                Ok(ControlFacts {
                    session_id: session.session_id,
                    controller_epoch: epoch,
                    holder: Some(caller.to_string()),
                    role: Some(ControllerRole::Human),
                    lease_token: Some(token),
                    acquired_at: lease.as_ref().map(|lease| lease.acquired_at.clone()),
                    display_domain_id: session.domain_id.clone(),
                    note: "a human holds control; the agent's previous epoch is invalid and its \
                            late input is rejected"
                        .into(),
                })
            }
            "hand_back" => {
                if lease
                    .as_ref()
                    .is_some_and(|lease| lease.role == ControllerRole::Human)
                {
                    *lease = None;
                }
                Ok(ControlFacts {
                    session_id: session.session_id,
                    controller_epoch: session.epoch.load(Ordering::Relaxed),
                    holder: None,
                    role: None,
                    lease_token: None,
                    acquired_at: None,
                    display_domain_id: session.domain_id.clone(),
                    note: "control was handed back; the agent must acquire a new lease and take a \
                            fresh observation before acting"
                        .into(),
                })
            }
            "status" => Ok(ControlFacts {
                session_id: session.session_id,
                controller_epoch: session.epoch.load(Ordering::Relaxed),
                holder: lease.as_ref().map(|_| caller.to_string()),
                role: lease.as_ref().map(|lease| lease.role),
                lease_token: lease.as_ref().map(|lease| lease.token),
                acquired_at: lease.as_ref().map(|lease| lease.acquired_at.clone()),
                display_domain_id: session.domain_id.clone(),
                note: "current controller state of this session".into(),
            }),
            other => bail!("unknown control op {other}; use acquire, release, takeover, hand_back or status"),
        }
    }

    // ------------------------------------------------------------------
    // Identity operations
    // ------------------------------------------------------------------

    /// Manage persistent identities and the weaker storageState form.
    pub async fn identity_op(
        &self,
        request: IdentityRequest,
        caller: EntityId,
        node: &str,
    ) -> Result<IdentityFacts> {
        match request.op.as_str() {
            "create" => {
                let profile_id = request
                    .profile_id
                    .clone()
                    .ok_or_else(|| anyhow!("create requires a profile_id"))?;
                let family = match request.engine.as_deref() {
                    Some(engine) if engine.to_ascii_lowercase().contains("firefox") => {
                        EngineFamily::Firefox
                    }
                    Some(engine) if engine.to_ascii_lowercase().contains("camoufox") => {
                        EngineFamily::Firefox
                    }
                    _ => EngineFamily::Chromium,
                };
                self.create_identity(&profile_id, family)
            }
            "list" => {
                let identities = self.list_identities();
                let mut facts = IdentityFacts {
                    profile_id: String::new(),
                    directory: String::new(),
                    engine_family: String::new(),
                    created_at: now_rfc3339(),
                    writer: None,
                    live_sessions: vec![],
                    storage_state: None,
                    note: format!("{} known persistent identities", identities.len()),
                };
                facts.storage_state = None;
                Ok(facts)
            }
            "describe" => {
                let profile_id = request
                    .profile_id
                    .clone()
                    .ok_or_else(|| anyhow!("describe requires a profile_id"))?;
                let identities = self.list_identities();
                identities
                    .into_iter()
                    .find(|identity| identity.profile_id == profile_id)
                    .ok_or_else(|| anyhow!("unknown persistent identity {profile_id}"))
            }
            "export_state" => {
                let session_id = request
                    .session_id
                    .ok_or_else(|| anyhow!("export_state requires a live session_id"))?;
                let session = self.session(session_id)?;
                let tab = self.resolve_tab(&session, request.tab_id.as_deref()).await?;
                let cookies = session.protocol.cookies().await?;
                let local = session
                    .protocol
                    .evaluate_json(&tab.handle, &super::scripts::local_storage_script())
                    .await?;
                let items = local
                    .get("items")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let state = StorageState {
                    origin: local
                        .get("origin")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    cookies,
                    local_storage: items,
                    exported_at: now_rfc3339(),
                    note: "storageState is cookies plus origin-scoped localStorage. It is not \
                            live page state: tabs, in-page memory and open sockets are not \
                            included and are not restored by it"
                        .into(),
                };
                let profile_id = session.profile_id.clone();
                let mut facts = self
                    .list_identities()
                    .into_iter()
                    .find(|identity| identity.profile_id == profile_id)
                    .unwrap_or(IdentityFacts {
                        profile_id: profile_id.clone(),
                        directory: self.profile_dir(&profile_id).to_string_lossy().to_string(),
                        engine_family: String::new(),
                        created_at: now_rfc3339(),
                        writer: None,
                        live_sessions: vec![],
                        storage_state: None,
                        note: String::new(),
                    });
                facts.storage_state = Some(state);
                Ok(facts)
            }
            "import_state" => {
                let session_id = request
                    .session_id
                    .ok_or_else(|| anyhow!("import_state requires a live session_id"))?;
                let session = self.session(session_id)?;
                let tab = self.resolve_tab(&session, request.tab_id.as_deref()).await?;
                let state = request
                    .state
                    .ok_or_else(|| anyhow!("import_state requires the storage state document"))?;
                let cookies = state
                    .get("cookies")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                Some(super::protocol::Cookie {
                                    name: item.get("name")?.as_str()?.to_string(),
                                    value: item.get("value")?.as_str()?.to_string(),
                                    domain: item
                                        .get("domain")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    path: item
                                        .get("path")
                                        .and_then(Value::as_str)
                                        .unwrap_or("/")
                                        .to_string(),
                                    expires: item.get("expires").and_then(Value::as_f64).unwrap_or(-1.0),
                                    http_only: item
                                        .get("http_only")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    secure: item
                                        .get("secure")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    same_site: item
                                        .get("same_site")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !cookies.is_empty() {
                    session.protocol.set_cookies(&cookies).await?;
                }
                let items = state
                    .get("local_storage")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if !items.is_empty() {
                    let script = super::scripts::set_local_storage_script(&items);
                    session.protocol.evaluate_json(&tab.handle, &script).await?;
                }
                let profile_id = session.profile_id.clone();
                let mut facts = self
                    .list_identities()
                    .into_iter()
                    .find(|identity| identity.profile_id == profile_id)
                    .unwrap_or(IdentityFacts {
                        profile_id: profile_id.clone(),
                        directory: self.profile_dir(&profile_id).to_string_lossy().to_string(),
                        engine_family: String::new(),
                        created_at: now_rfc3339(),
                        writer: None,
                        live_sessions: vec![],
                        storage_state: None,
                        note: String::new(),
                    });
                facts.note = format!(
                    "imported {} cookies and {} localStorage entries into the live session",
                    cookies.len(),
                    items.len()
                );
                Ok(facts)
            }
            other => bail!(
                "unknown identity op {other}; use create, list, describe, export_state or import_state"
            ),
        }
    }
}

/// The strongest effect any receipt in the batch proved.
fn aggregate_effect(receipts: &[ActionReceipt]) -> String {
    if receipts.is_empty() {
        return "none".into();
    }
    let mut unknown = false;
    let mut partial = false;
    let mut failed = false;
    for receipt in receipts {
        match receipt.effect.as_str() {
            "complete" => {}
            "partial" => partial = true,
            "unknown" => unknown = true,
            _ => failed = true,
        }
    }
    if failed {
        "unknown".into()
    } else if unknown {
        "unknown".into()
    } else if partial {
        "partial".into()
    } else {
        "complete".into()
    }
}

fn truncate_json(value: &Value, limit: usize) -> String {
    let text = value.to_string();
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::engine::{EngineFamily, EngineSpec, SessionMode};
    use crate::browser::protocol::{
        AxNode, BrowserProtocol, Cookie, NavOutcome, TabInfo,
    };
    use crate::desktop::HumanInputObserver;
    use shenko_store::Store;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tokio::sync::Mutex as AsyncMutex;

    /// A protocol boundary that stands in for the real browser: the page
    /// itself is synthetic, but every lease/epoch/observer/refusal decision
    /// runs through the production `act` path, and every input dispatch is
    /// recorded so refused actions can be proven never to have been sent.
    struct RecordingProtocol {
        wait_found: bool,
        wait_selector: String,
        wait_started: Arc<tokio::sync::Notify>,
        pressed: StdMutex<Vec<String>>,
        typed: StdMutex<Vec<String>>,
    }

    impl RecordingProtocol {
        fn snapshot(&self) -> Value {
            serde_json::json!({
                "url": "https://example.test/",
                "title": "fixture",
                "readyState": "complete",
                "text": "fixture",
                "textTruncated": false,
                "viewport": {"width": 1280, "height": 720, "deviceScaleFactor": 1.0},
                "links": [],
                "inputs": [],
                "buttons": []
            })
        }
    }

    #[async_trait::async_trait]
    impl BrowserProtocol for RecordingProtocol {
        fn kind(&self) -> &'static str {
            "test"
        }
        fn endpoint(&self) -> String {
            "test://fixture".into()
        }
        fn product(&self) -> String {
            "fixture".into()
        }
        async fn create_tab(&self, _url: &str) -> Result<TabHandle> {
            Ok(TabHandle {
                id: "tab-1".into(),
                kind: "page".into(),
            })
        }
        async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
            Ok(vec![])
        }
        async fn close_tab(&self, _tab: &TabHandle) -> Result<()> {
            Ok(())
        }
        async fn navigate(&self, _tab: &TabHandle, url: &str) -> Result<NavOutcome> {
            Ok(NavOutcome {
                url: url.into(),
                loader: None,
                error_text: None,
                ready_state: Some("complete".into()),
            })
        }
        async fn evaluate_json(&self, _tab: &TabHandle, script: &str) -> Result<Value> {
            if script == super::super::scripts::exists_script(&self.wait_selector) {
                self.wait_started.notify_one();
                return Ok(serde_json::json!({"found": self.wait_found}));
            }
            if script.contains("MAX_TARGETS = 256") {
                return Ok(self.snapshot());
            }
            if script == ACTIVE_VALUE_SCRIPT {
                return Ok(serde_json::json!({"ok": true, "length": 5, "tag": "input"}));
            }
            Ok(serde_json::json!({"found": true, "tag": "button"}))
        }
        async fn screenshot(&self, _tab: &TabHandle) -> Result<Vec<u8>> {
            Ok(b"png".to_vec())
        }
        async fn ax_nodes(&self, _tab: &TabHandle) -> Result<Option<Vec<AxNode>>> {
            Ok(None)
        }
        async fn cookies(&self) -> Result<Vec<Cookie>> {
            Ok(vec![])
        }
        async fn set_cookies(&self, _cookies: &[Cookie]) -> Result<()> {
            Ok(())
        }
        async fn insert_text(&self, _tab: &TabHandle, text: &str) -> Result<()> {
            self.typed.lock().unwrap().push(text.into());
            Ok(())
        }
        async fn press_key(&self, _tab: &TabHandle, key: &str) -> Result<()> {
            self.pressed.lock().unwrap().push(key.into());
            Ok(())
        }
        async fn scroll(&self, _tab: &TabHandle, _delta_y: f64) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self) {}
    }

    /// A live session registered with the provider exactly like an open one,
    /// bound to the shared display domain the human-input observer watches.
    fn test_provider(wait_found: bool) -> (Arc<BrowserProvider>, Arc<RecordingProtocol>, EntityId) {
        let dir = std::env::temp_dir().join(format!("shenko-browser-act-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let provider = Arc::new(BrowserProvider::new(store, dir));
        let protocol = Arc::new(RecordingProtocol {
            wait_found,
            wait_selector: "#ready".into(),
            wait_started: Arc::new(tokio::sync::Notify::new()),
            pressed: StdMutex::new(vec![]),
            typed: StdMutex::new(vec![]),
        });
        let session_id = EntityId::new();
        let handle = TabHandle {
            id: "tab-1".into(),
            kind: "page".into(),
        };
        let mut tabs = BTreeMap::new();
        tabs.insert(
            "tab-1".to_string(),
            crate::browser::TabEntry {
                handle,
                group: "default".into(),
                url: "https://example.test/".into(),
                title: "fixture".into(),
                revision: 1,
            },
        );
        let session = Arc::new(BrowserSession {
            session_id,
            profile_id: "default".into(),
            run_id: "run-1".into(),
            engine: EngineSpec {
                path: PathBuf::from("/fixture/browser"),
                name: "fixture".into(),
                family: EngineFamily::Chromium,
                version: "1".into(),
            },
            mode_requested: "headless".into(),
            mode_effective: SessionMode::Headless,
            mode_degradation: None,
            display: None,
            domain_id: Some("test-domain".into()),
            endpoint: "test://fixture".into(),
            product: "fixture".into(),
            protocol: protocol.clone(),
            process: AsyncMutex::new(None),
            display_handle: AsyncMutex::new(None),
            epoch: AtomicU64::new(1),
            lease: AsyncMutex::new(None),
            tabs: AsyncMutex::new(tabs),
            observations: AsyncMutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
        });
        provider.state().sessions.insert(session_id, session);
        (provider, protocol, session_id)
    }

    fn action_request(
        session_id: EntityId,
        observation_id: String,
        lease_token: EntityId,
    ) -> ActionRequest {
        ActionRequest {
            session_id,
            tab_id: None,
            observation_id,
            lease_token,
            actions: vec![
                PageAction::WaitFor {
                    selector: "#ready".into(),
                    timeout_ms: Some(2_000),
                },
                PageAction::Press {
                    key: "Enter".into(),
                },
                PageAction::Type {
                    text: "hello".into(),
                },
            ],
        }
    }

    /// A human takeover lands while the first action is still waiting. The
    /// real human-input observer moves the epoch and revokes the lease, so
    /// every later action must be refused and must never reach the page.
    #[tokio::test]
    async fn takeover_during_a_running_action_refuses_the_rest_of_the_batch() {
        let (provider, protocol, session_id) = test_provider(false);
        let caller = EntityId::new();
        let facts = provider
            .control(
                ControlRequest {
                    session_id,
                    op: "acquire".into(),
                    role: None,
                    lease_token: None,
                },
                caller,
            )
            .await
            .unwrap();
        let observation = provider
            .observe(session_id, None, ObserveOptions::default())
            .await
            .unwrap();

        let acting = Arc::clone(&provider);
        let request = action_request(
            session_id,
            observation.observation_id,
            facts.lease_token.unwrap(),
        );
        let batch = tokio::spawn(async move { acting.act(request, caller).await });
        // Deterministic rendezvous: the first wait_for poll was dispatched
        // and the action keeps waiting for its bounded deadline, so the
        // takeover provably lands while the batch is mid-run.
        protocol.wait_started.notified().await;
        // The real observer path: same display domain, synchronous epoch
        // move plus the leased-agent revocation it schedules.
        HumanInputObserver::on_human_input(provider.as_ref(), "test-domain");

        let report = batch.await.unwrap().unwrap();
        assert_eq!(report.receipts.len(), 3);
        assert_eq!(report.receipts[0].kind, "wait_for");
        assert_eq!(report.receipts[0].transport, "acked");
        assert_eq!(report.receipts[1].kind, "press");
        assert_eq!(report.receipts[1].transport, "refused");
        assert_eq!(report.receipts[1].effect, "none");
        assert!(
            report.receipts[1]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("control"))
        );
        assert_eq!(report.receipts[2].kind, "type");
        assert_eq!(report.receipts[2].transport, "refused");
        // Nothing reached the human-controlled page after the takeover.
        assert!(protocol.pressed.lock().unwrap().is_empty());
        assert!(protocol.typed.lock().unwrap().is_empty());
        assert_eq!(report.controller_epoch, 2);
        assert_eq!(report.effect, "unknown");
    }

    /// Without a takeover the batch is unchanged: every action dispatches in
    /// order and the report carries no refusal.
    #[tokio::test]
    async fn batch_without_takeover_runs_every_action() {
        let (provider, protocol, session_id) = test_provider(true);
        let caller = EntityId::new();
        let facts = provider
            .control(
                ControlRequest {
                    session_id,
                    op: "acquire".into(),
                    role: None,
                    lease_token: None,
                },
                caller,
            )
            .await
            .unwrap();
        let observation = provider
            .observe(session_id, None, ObserveOptions::default())
            .await
            .unwrap();
        let request = action_request(
            session_id,
            observation.observation_id,
            facts.lease_token.unwrap(),
        );
        let report = provider.act(request, caller).await.unwrap();

        assert_eq!(report.receipts.len(), 3);
        assert!(
            report
                .receipts
                .iter()
                .all(|receipt| receipt.transport == "acked")
        );
        assert_eq!(report.receipts[0].effect, "complete");
        assert_eq!(protocol.pressed.lock().unwrap().clone(), vec!["Enter"]);
        assert_eq!(protocol.typed.lock().unwrap().clone(), vec!["hello"]);
        assert_eq!(report.controller_epoch, 1);
    }
}

