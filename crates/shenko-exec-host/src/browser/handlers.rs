//! Native task handlers for the browser provider.
//!
//! Five kinds share one `BrowserProvider`, so the session registry, the
//! identity registry and the controller lease are one truth across task
//! kinds. Every handler follows the same contract: arm the durable T1 record
//! before any external effect, run the real browser work, then settle the
//! operation with fresh evidence. Nothing is ever replayed.
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::sync::Arc;

use super::{
    ActionRequest, BrowserProvider, ControlRequest, IdentityRequest, ObserveOptions, SessionRequest,
};
use std::future::Future;

const CONTRACT: &[u8] = super::CONTRACT;

/// One handler outcome: what actually happened, in durable form.
pub(crate) enum BrowserOutcome {
    /// The work completed; `normalized` is the durable result document.
    Done {
        outcome: OperationOutcome,
        effect: EffectStatus,
        level: ObservationLevel,
        normalized: serde_json::Value,
    },
    /// The requested capability is honestly unavailable on this node. The
    /// operation settles Failed with the exact missing thing named.
    Unavailable {
        reason: String,
        detail: String,
        normalized: serde_json::Value,
    },
}

impl BrowserOutcome {
    pub(crate) fn done(
        outcome: OperationOutcome,
        effect: EffectStatus,
        level: ObservationLevel,
        normalized: serde_json::Value,
    ) -> Self {
        Self::Done {
            outcome,
            effect,
            level,
            normalized,
        }
    }

    fn unavailable(reason: &str, detail: String, normalized: serde_json::Value) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            detail,
            normalized,
        }
    }
}

/// Classify a provider error into a typed-unavailable outcome. Runtime
/// errors that are not capability gaps are returned as-is so the dispatcher
/// can park the task with an honest reason.
fn typed_outcome(error: anyhow::Error) -> Result<BrowserOutcome> {
    let text = error.to_string();
    let unavailable = text.contains("no installed browser engine")
        || text.contains("no display capability")
        || text.contains("virtual-headed mode was requested but unavailable");
    if unavailable {
        return Ok(BrowserOutcome::unavailable(
            "browser_capability_unavailable",
            text.clone(),
            serde_json::json!({
                "status": "unavailable",
                "reason": "browser_capability_unavailable",
                "detail": text,
            }),
        ));
    }
    Err(error)
}

// ---------------------------------------------------------------------------
// Shared drive: arm -> work -> settle
// ---------------------------------------------------------------------------

/// Arm the durable T1 record, run the browser work, settle with fresh
/// evidence. Mirrors the desktop provider's drive; nothing replays.
#[allow(clippy::too_many_arguments)]
async fn drive(
    provider: &Arc<BrowserProvider>,
    invocation: NativeTaskInvocation,
    operation_name: &str,
    logical_key: &str,
    frozen_input: serde_json::Value,
    extra_binding: serde_json::Value,
    acceptance: &str,
    work: impl Future<Output = Result<BrowserOutcome>>,
) -> Result<OperationRecord> {
    let store = provider.store();
    let input_ref = seal(store, &frozen_input, "browser-frozen-input")?;
    let bindings = ResourceBindings {
        provider: operation_name.into(),
        account_identity: String::new(),
        runtime_node: invocation.task_invocation.node_id.to_string(),
        mount_generation: 0,
        capability_level: "real_browser_engine_bound_by_frozen_input".into(),
        extra: extra_binding,
    };
    let resource = seal(store, &bindings, "browser-binding")?;
    let acceptance_ref = seal(store, &acceptance, "browser-acceptance")?;
    let retry = seal(
        store,
        &"no automatic replay; an armed browser action without conclusive evidence \
          stays unknown and is reconciled by observation",
        "browser-retry",
    )?;
    let contract_digest = Digest::blake3(CONTRACT);
    let version = VersionRef {
        artifact: ArtifactRef {
            digest: contract_digest.clone(),
            locator: format!("builtin:{operation_name}"),
        },
        symbol: format!("{operation_name}.execute"),
        schema: SchemaRef {
            name: "browser-observation-v1".into(),
            major: 1,
            digest: contract_digest.clone(),
        },
    };
    let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let mut operation = OperationRecord {
        record_schema_version: 1,
        operation_id: invocation.operation_id,
        record_revision: RecordRevision(0),
        task_id: invocation.task_invocation.task_id,
        logical_key: logical_key.into(),
        operation_name: operation_name.into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: input_ref.clone(),
        input_digest: input_ref.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: browser_plugin_id(),
            artifact_digest: contract_digest.clone(),
            contract_digest,
            receipt_reader_ref: version.clone(),
        },
        resource_bindings_ref: resource,
        authority_ref: invocation.authority,
        acceptance_predicate_ref: acceptance_ref,
        retry_contract_ref: retry,
        checkpoint_scope_ref: None,
        state: OperationState::InFlight,
        outcome: None,
        outcome_conflict: false,
        effect_status: EffectStatus::Unknown,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: zero,
        evidence_through: None,
    };
    let mut attempt = AttemptRecord {
        record_schema_version: 1,
        attempt_id: EntityId::new(),
        operation_id: invocation.operation_id,
        attempt_no: 1,
        invocation_id: invocation.task_invocation.invocation_id,
        worker_id: invocation.task_invocation.worker_id,
        node_id: invocation.task_invocation.node_id,
        epoch: invocation.task_invocation.epoch,
        implementation_ref: version,
        dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: zero,
        provider_handle: None,
        provider_idempotency: None,
        transport_ref: None,
        started_at: None,
        last_observed_at: None,
        final_receipt_id: None,
    };
    if let Some(existing) = store.arm_external_operation(
        &invocation.task_invocation,
        &mut operation,
        &mut attempt,
    )? {
        return Ok(existing);
    }

    let outcome = match work.await {
        Ok(outcome) => outcome,
        Err(error) => match typed_outcome(error) {
            Ok(outcome) => outcome,
            Err(error) => return Err(error),
        },
    };
    let (op_outcome, effect, level, normalized) = match outcome {
        BrowserOutcome::Done {
            outcome,
            effect,
            level,
            normalized,
        } => (Some(outcome), effect, Some(level), normalized),
        BrowserOutcome::Unavailable {
            reason,
            detail,
            normalized,
        } => {
            let _ = (reason, detail);
            (
                Some(OperationOutcome::Failed),
                EffectStatus::None,
                None,
                normalized,
            )
        }
    };
    let raw = seal(store, &normalized, "browser-observation")?;
    let normalized_ref = seal(store, &normalized, "browser-result")?;
    let evidence = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(invocation.operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: operation_name.into(),
        source_instance_id: invocation.task_invocation.node_id.to_string(),
        source_event_key: None,
        producer_epoch: Some(invocation.task_invocation.epoch.0),
        producer_schema_version: Some("1".into()),
        raw_ref: raw,
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: DecodeStatus::Decoded,
        observation_level: level,
        normalized_ref: Some(normalized_ref),
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    store.record_external_evidence(
        invocation.operation_id,
        attempt.attempt_id,
        evidence,
        op_outcome,
        effect,
    )
}

fn seal(store: &Store, value: &serde_json::Value, schema: &str) -> Result<ObjectRef> {
    match store
        .cas
        .seal_object(&serde_json::to_vec(value)?, "application/json", "browser-effects", schema, 1)
    {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

fn browser_plugin_id() -> EntityId {
    "7a9c1e2b-4d6f-4a8b-9e3c-1f2a3b4c5d6e"
        .parse()
        .expect("fixed browser plugin UUID")
}

fn descriptor_for(kind: &str, authority: &str, prompt: &str) -> NativeTaskDescriptor {
    NativeTaskDescriptor {
        kind: kind.into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: browser_plugin_id(),
            artifact_digest: Digest::blake3(CONTRACT),
            registry_revision: 0,
        },
        role_revision: "native-browser/1".into(),
        authority_refs: vec![authority.into()],
        prompt_section: Some(prompt.into()),
    }
}

// ---------------------------------------------------------------------------
// browser.session — open / close / list real browser sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionOpInput {
    op: Option<String>,
    profile_id: Option<String>,
    mode: Option<String>,
    engine: Option<String>,
    url: Option<String>,
    group: Option<String>,
    session_id: Option<EntityId>,
}

const SESSION_PROMPT: &str = "browser.session/v1 opens, closes and lists real browser sessions of a real installed engine {\"op\":\"open\"|\"close\"|\"list\",\"profile_id\":\"name\",\"mode\":\"auto\"|\"virtual_headed\"|\"headless\",\"engine\":\"auto\"|\"chromium\"|\"firefox\"|<path>,\"url\":\"https://…\",\"group\":\"label\",\"session_id\":uuid?}. Multiple profiles are persistent identities with separate cookies and storage; the default mode is virtual-headed (a real headful browser on a virtual display, take-over-able by a human), headless is the real no-display option. A reopened profile restores persistent profile state only — live tabs and in-page memory are never restored.";

pub struct BrowserSessionHandler {
    provider: Arc<BrowserProvider>,
}

impl BrowserSessionHandler {
    pub fn new(provider: Arc<BrowserProvider>) -> Self {
        Self { provider }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: SessionOpInput = serde_json::from_value(invocation.input.clone())?;
        let op = input.op.clone().unwrap_or_else(|| "open".into());
        let caller = invocation.context.caller_instance_id;
        let node = invocation.task_invocation.node_id.to_string();
        match op.as_str() {
            "open" => {
                let request = SessionRequest {
                    mode: input.mode.clone(),
                    engine: input.engine.clone(),
                    profile_id: input.profile_id.clone(),
                    url: input.url.clone(),
                    group: input.group.clone(),
                };
                let frozen = serde_json::to_value(&request)?;
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.session",
                    "browser-session-open",
                    frozen.clone(),
                    serde_json::json!({"op": "open"}),
                    "a real browser process is bound to the persistent profile, its control \
                     endpoint answers, and the session facts name the engine, the mode and the \
                     restore coverage",
                    async move {
                        let facts = provider
                            .open_session(request, caller, &node)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::to_value(&facts)?,
                        ))
                    },
                )
                .await
            }
            "close" => {
                let session_id = input
                    .session_id
                    .ok_or_else(|| anyhow!("close requires session_id"))?;
                let frozen = serde_json::json!({"op":"close","session_id":session_id});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.session",
                    "browser-session-close",
                    frozen.clone(),
                    serde_json::json!({"op": "close"}),
                    "the browser process is stopped, the virtual display is released and the \
                     profile's single write ownership is given back",
                    async move {
                        let facts = provider
                            .close_session(session_id, caller)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::to_value(&facts)?,
                        ))
                    },
                )
                .await
            }
            "list" => {
                let frozen = serde_json::json!({"op":"list"});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.session",
                    "browser-session-list",
                    frozen.clone(),
                    serde_json::json!({"op": "list"}),
                    "every live session of this process is listed with its engine, mode and tabs",
                    async move {
                        let facts = provider.list_sessions().await;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::json!({"sessions": facts}),
                        ))
                    },
                )
                .await
            }
            other => Err(anyhow!("unknown browser.session op {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// browser.page — observe / navigate / tab management (read path)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageOpInput {
    op: Option<String>,
    session_id: Option<EntityId>,
    tab_id: Option<String>,
    url: Option<String>,
    screenshot: Option<bool>,
    accessibility: Option<bool>,
}

const PAGE_PROMPT: &str = "browser.page/v1 reads and navigates pages of a live browser session {\"op\":\"observe\"|\"navigate\"|\"tabs\"|\"open_tab\"|\"close_tab\",\"session_id\":uuid,\"tab_id\":str?,\"url\":\"https://…\",\"screenshot\":bool,\"accessibility\":bool}. observe returns a content-bound observation_id, the page text, structured links/inputs/buttons and an optional screenshot and accessibility tree; later browser.action calls must carry that observation_id, and a page that changed since it was taken rejects the action. Reading never needs an input lease.";

pub struct BrowserPageHandler {
    provider: Arc<BrowserProvider>,
}

impl BrowserPageHandler {
    pub fn new(provider: Arc<BrowserProvider>) -> Self {
        Self { provider }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: PageOpInput = serde_json::from_value(invocation.input.clone())?;
        let op = input.op.clone().unwrap_or_else(|| "observe".into());
        let session_id = input
            .session_id
            .ok_or_else(|| anyhow!("browser.page requires session_id"))?;
        match op.as_str() {
            "observe" => {
                let options = ObserveOptions {
                    screenshot: input.screenshot.unwrap_or(false),
                    accessibility: input.accessibility.unwrap_or(false),
                };
                let frozen = serde_json::to_value(&input)?;
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.page",
                    "browser-page-observe",
                    frozen.clone(),
                    serde_json::json!({"op": "observe"}),
                    "the observation is content-bound: its id is the digest of the page state it \
                     actually captured, and its restore coverage says a reopened profile does not \
                     restore live tabs or in-page memory",
                    async move {
                        let observation = provider
                            .observe(session_id, input.tab_id.as_deref(), options)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::to_value(&observation)?,
                        ))
                    },
                )
                .await
            }
            "navigate" => {
                let url = input
                    .url
                    .clone()
                    .ok_or_else(|| anyhow!("navigate requires url"))?;
                ensure!(url.len() <= 4096, "url is too long");
                let frozen = serde_json::json!({"op":"navigate","session_id":session_id,"tab_id":input.tab_id,"url":url});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.page",
                    "browser-page-navigate",
                    frozen.clone(),
                    serde_json::json!({"op": "navigate"}),
                    "the engine accepted the navigation and reports the resulting URL, or names \
                     its own refusal (e.g. net::ERR_NAME_NOT_RESOLVED)",
                    async move {
                        let session = provider
                            .session(session_id)
                            .map_err(|error| anyhow!(error))?;
                        let tab = provider
                            .resolve_tab(&session, input.tab_id.as_deref())
                            .await
                            .map_err(|error| anyhow!(error))?;
                        let outcome = session
                            .protocol
                            .navigate(&tab.handle, &url)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        let (success, effect, detail) = match &outcome.error_text {
                            Some(error) => (false, EffectStatus::None, error.clone()),
                            None => (true, EffectStatus::None, outcome.url.clone()),
                        };
                        Ok(BrowserOutcome::done(
                            if success {
                                OperationOutcome::Succeeded
                            } else {
                                OperationOutcome::Failed
                            },
                            effect,
                            ObservationLevel::RemoteCompleted,
                            serde_json::to_value(&outcome)?,
                        )
                        .with_detail(detail))
                    },
                )
                .await
            }
            "tabs" => {
                let frozen = serde_json::json!({"op":"tabs","session_id":session_id});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.page",
                    "browser-page-tabs",
                    frozen.clone(),
                    serde_json::json!({"op": "tabs"}),
                    "the engine lists its own open tabs",
                    async move {
                        let session = provider
                            .session(session_id)
                            .map_err(|error| anyhow!(error))?;
                        let tabs = session
                            .protocol
                            .list_tabs()
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::json!({"tabs": tabs}),
                        ))
                    },
                )
                .await
            }
            "open_tab" => {
                let url = input.url.clone().unwrap_or_else(|| "about:blank".into());
                ensure!(url.len() <= 4096, "url is too long");
                let frozen = serde_json::json!({"op":"open_tab","session_id":session_id,"url":url});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.page",
                    "browser-page-open-tab",
                    frozen.clone(),
                    serde_json::json!({"op": "open_tab"}),
                    "the engine created a new tab and reports its own tab id",
                    async move {
                        let session = provider
                            .session(session_id)
                            .map_err(|error| anyhow!(error))?;
                        let tab = session
                            .protocol
                            .create_tab(&url)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::to_value(&tab)?,
                        ))
                    },
                )
                .await
            }
            "close_tab" => {
                let tab_id = input
                    .tab_id
                    .clone()
                    .ok_or_else(|| anyhow!("close_tab requires tab_id"))?;
                let frozen = serde_json::json!({"op":"close_tab","session_id":session_id,"tab_id":tab_id});
                let provider = Arc::clone(&self.provider);
                drive(
                    &provider,
                    invocation,
                    "browser.page",
                    "browser-page-close-tab",
                    frozen.clone(),
                    serde_json::json!({"op": "close_tab"}),
                    "the engine closed the tab",
                    async move {
                        let session = provider
                            .session(session_id)
                            .map_err(|error| anyhow!(error))?;
                        let tab = provider
                            .resolve_tab(&session, Some(&tab_id))
                            .await
                            .map_err(|error| anyhow!(error))?;
                        session
                            .protocol
                            .close_tab(&tab.handle)
                            .await
                            .map_err(|error| anyhow!(error))?;
                        Ok(BrowserOutcome::done(
                            OperationOutcome::Succeeded,
                            EffectStatus::None,
                            ObservationLevel::PostconditionVerified,
                            serde_json::json!({"closed": tab_id}),
                        ))
                    },
                )
                .await
            }
            other => Err(anyhow!("unknown browser.page op {other}")),
        }
    }
}

impl BrowserOutcome {
    fn with_detail(self, detail: String) -> Self {
        match self {
            BrowserOutcome::Done {
                outcome,
                effect,
                level,
                normalized,
            } => {
                let mut normalized = normalized;
                if let Some(object) = normalized.as_object_mut() {
                    object.insert("detail".into(), serde_json::Value::String(detail));
                }
                BrowserOutcome::Done {
                    outcome,
                    effect,
                    level,
                    normalized,
                }
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// browser.action — bound, leased page actions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionOpInput {
    session_id: Option<EntityId>,
    tab_id: Option<String>,
    observation_id: Option<String>,
    lease_token: Option<EntityId>,
    actions: Option<serde_json::Value>,
}

const ACTION_PROMPT: &str = "browser.action/v1 performs page actions bound to one observation and one input lease {\"session_id\":uuid,\"tab_id\":str?,\"observation_id\":\"blake3:…\",\"lease_token\":uuid,\"actions\":[{\"kind\":\"navigate\",\"url\":\"…\"}|{\"kind\":\"click\",\"selector\":\"…\"}|{\"kind\":\"click\",\"text\":\"Sign in\"}|{\"kind\":\"fill\",\"selector\":\"…\",\"value\":\"…\",\"submit\":bool}|{\"kind\":\"type\",\"text\":\"…\"}|{\"kind\":\"press\",\"key\":\"Enter\"}|{\"kind\":\"scroll\",\"dy\":n}|{\"kind\":\"wait_for\",\"selector\":\"…\",\"timeout_ms\":n}|{\"kind\":\"evaluate\",\"script\":\"…\"}|{\"kind\":\"screenshot\"}]}. Actions run only while the controller epoch is live; a human takeover revokes it and the agent's late input is rejected. A delivered action whose effect cannot be seen is recorded unknown, never replayed.";

pub struct BrowserActionHandler {
    provider: Arc<BrowserProvider>,
}

impl BrowserActionHandler {
    pub fn new(provider: Arc<BrowserProvider>) -> Self {
        Self { provider }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ActionOpInput = serde_json::from_value(invocation.input.clone())?;
        let session_id = input
            .session_id
            .ok_or_else(|| anyhow!("browser.action requires session_id"))?;
        let observation_id = input
            .observation_id
            .clone()
            .ok_or_else(|| anyhow!("browser.action requires observation_id"))?;
        let lease_token = input
            .lease_token
            .ok_or_else(|| anyhow!("browser.action requires lease_token"))?;
        let actions: Vec<super::PageAction> =
            serde_json::from_value(input.actions.clone().unwrap_or(serde_json::json!([])))
                .context("parse browser action list")?;
        ensure!(
            !actions.is_empty() && actions.len() <= 32,
            "actions must be 1..=32 entries"
        );
        let caller = invocation.context.caller_instance_id;
        let frozen = serde_json::json!({
            "session_id": session_id,
            "tab_id": input.tab_id,
            "observation_id": observation_id,
            "lease_token": lease_token,
            "actions": actions,
        });
        let provider = Arc::clone(&self.provider);
        drive(
            &provider,
            invocation,
            "browser.action",
            "browser-action",
            frozen.clone(),
            serde_json::json!({"op": "action"}),
            "every action ran only after the bound observation re-verified and the lease epoch \
             was live; a delivered action without a visible effect is recorded unknown",
            async move {
                let report = provider
                    .act(ActionRequest {
                        session_id,
                        tab_id: input.tab_id.clone(),
                        observation_id,
                        lease_token,
                        actions,
                    }, caller)
                    .await
                    .map_err(|error| anyhow!(error))?;
                let effect = match report.effect.as_str() {
                    "complete" => EffectStatus::Complete,
                    "partial" => EffectStatus::Partial,
                    "none" => EffectStatus::None,
                    _ => EffectStatus::Unknown,
                };
                let level = if effect == EffectStatus::Complete {
                    ObservationLevel::PostconditionVerified
                } else {
                    ObservationLevel::RemoteCompleted
                };
                Ok(BrowserOutcome::done(
                    OperationOutcome::Succeeded,
                    effect,
                    level,
                    serde_json::to_value(&report)?,
                ))
            },
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// browser.control — controller epoch / input lease
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlOpInput {
    op: Option<String>,
    session_id: Option<EntityId>,
    role: Option<super::ControllerRole>,
    lease_token: Option<EntityId>,
}

const CONTROL_PROMPT: &str = "browser.control/v1 manages the single controller of a live browser session {\"op\":\"acquire\"|\"release\"|\"takeover\"|\"hand_back\"|\"status\",\"session_id\":uuid,\"role\":\"agent\"|\"human\",\"lease_token\":uuid?}. Exactly one controller writes input at a time: takeover revokes the agent's epoch so its late input is rejected; hand_back lets the agent acquire a new lease and observe again. Reading never needs the lease.";

pub struct BrowserControlHandler {
    provider: Arc<BrowserProvider>,
}

impl BrowserControlHandler {
    pub fn new(provider: Arc<BrowserProvider>) -> Self {
        Self { provider }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ControlOpInput = serde_json::from_value(invocation.input.clone())?;
        let op = input.op.clone().unwrap_or_else(|| "status".into());
        let session_id = input
            .session_id
            .ok_or_else(|| anyhow!("browser.control requires session_id"))?;
        let caller = invocation.context.caller_instance_id;
        let frozen = serde_json::json!({
            "op": op,
            "session_id": session_id,
            "role": input.role,
            "lease_token": input.lease_token,
        });
        let provider = Arc::clone(&self.provider);
        drive(
            &provider,
            invocation,
            "browser.control",
            "browser-control",
            frozen.clone(),
            serde_json::json!({"op": op}),
            "exactly one controller holds the session's input at any time; a takeover moves the \
             controller epoch so the previous controller's late input is rejected",
            async move {
                let facts = provider
                    .control(
                        ControlRequest {
                            session_id,
                            op,
                            role: input.role,
                            lease_token: input.lease_token,
                        },
                        caller,
                    )
                    .await
                    .map_err(|error| anyhow!(error))?;
                Ok(BrowserOutcome::done(
                    OperationOutcome::Succeeded,
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::to_value(&facts)?,
                ))
            },
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// browser.identity — persistent identities and storageState
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityOpInput {
    op: Option<String>,
    profile_id: Option<String>,
    engine: Option<String>,
    session_id: Option<EntityId>,
    tab_id: Option<String>,
    state: Option<serde_json::Value>,
}

const IDENTITY_PROMPT: &str = "browser.identity/v1 manages persistent browser identities and the weaker storageState form {\"op\":\"create\"|\"list\"|\"describe\"|\"export_state\"|\"import_state\",\"profile_id\":\"name\",\"engine\":\"chromium\"|\"firefox\",\"session_id\":uuid,\"tab_id\":str?,\"state\":{…}}. Identities are the browser's own profile directories: separate identities keep separate cookies and storage. storageState (cookies + origin localStorage) can be exported/imported through a live session; it is not live page state.";

pub struct BrowserIdentityHandler {
    provider: Arc<BrowserProvider>,
}

impl BrowserIdentityHandler {
    pub fn new(provider: Arc<BrowserProvider>) -> Self {
        Self { provider }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: IdentityOpInput = serde_json::from_value(invocation.input.clone())?;
        let op = input.op.clone().unwrap_or_else(|| "list".into());
        let caller = invocation.context.caller_instance_id;
        let node = invocation.task_invocation.node_id.to_string();
        let frozen = serde_json::json!({
            "op": op,
            "profile_id": input.profile_id,
            "engine": input.engine,
        });
        let provider = Arc::clone(&self.provider);
        drive(
            &provider,
            invocation,
            "browser.identity",
            "browser-identity",
            frozen.clone(),
            serde_json::json!({"op": op}),
            "the operation is bound to the real profile directory or the live session's own \
             storage; storageState is reported as the weaker form it is, never as live page \
             continuity",
            async move {
                let facts = provider
                    .identity_op(IdentityRequest {
                        op,
                        profile_id: input.profile_id,
                        engine: input.engine,
                        session_id: input.session_id,
                        tab_id: input.tab_id,
                        state: input.state,
                    }, caller, &node)
                    .await
                    .map_err(|error| anyhow!(error))?;
                Ok(BrowserOutcome::done(
                    OperationOutcome::Succeeded,
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::to_value(&facts)?,
                ))
            },
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// NativeTaskHandler impls + reconcile
// ---------------------------------------------------------------------------

#[async_trait]
impl NativeTaskHandler for BrowserSessionHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for("browser.session", "browser.session.manage/v1", SESSION_PROMPT)
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_browser(&self.provider, task, operation_id, "browser.session").await
    }
}

#[async_trait]
impl NativeTaskHandler for BrowserPageHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for("browser.page", "browser.page.observe/v1", PAGE_PROMPT)
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_browser(&self.provider, task, operation_id, "browser.page").await
    }
}

#[async_trait]
impl NativeTaskHandler for BrowserActionHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for("browser.action", "browser.action.perform/v1", ACTION_PROMPT)
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_browser(&self.provider, task, operation_id, "browser.action").await
    }
}

#[async_trait]
impl NativeTaskHandler for BrowserControlHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for("browser.control", "browser.control.lease/v1", CONTROL_PROMPT)
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_browser(&self.provider, task, operation_id, "browser.control").await
    }
}

#[async_trait]
impl NativeTaskHandler for BrowserIdentityHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for("browser.identity", "browser.identity.manage/v1", IDENTITY_PROMPT)
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_browser(&self.provider, task, operation_id, "browser.identity").await
    }
}

/// Shared reconcile. An armed operation with no conclusive evidence is not
/// replayed: the live session, when it still exists, is re-observed and that
/// fresh state is recorded; the original effect stays unknown. A session
/// that is gone stays unknown with the reason named.
async fn reconcile_browser(
    provider: &Arc<BrowserProvider>,
    task: &TaskRecord,
    operation_id: EntityId,
    operation_name: &str,
) -> Result<OperationRecord> {
    let store = provider.store();
    let operation = store.external_operation(operation_id)?;
    ensure!(
        operation.task_id == task.task_id && operation.operation_name == operation_name,
        "original_operation_contract_mismatch"
    );
    if operation.state == OperationState::Resolved {
        return Ok(operation);
    }
    let attempt = store.external_operation_attempt(operation_id)?;
    if attempt.dispatch_stage == AttemptDispatchStage::Ended {
        return Ok(operation);
    }
    // The live session is the only honest oracle. When it is gone, the
    // effect of the armed operation is not observable: keep it unknown.
    let session = resolve_reconcile_session(provider, &operation);
    let observation = match session {
        Some(session) => {
            let tab = provider.resolve_tab(&session, None).await.ok();
            match tab {
                Some(tab) => provider
                    .observe_tab(&session, &tab.handle, &ObserveOptions::default())
                    .await
                    .ok()
                    .map(|observation| {
                        (
                            observation,
                            "the live session still exists; the current page state is recorded as \
                             fresh evidence and the original effect stays unknown until observed"
                                .to_string(),
                        )
                    }),
                None => None,
            }
        }
        None => None,
    };
    let (normalized, detail) = match observation {
        Some((observation, note)) => (
            serde_json::json!({
                "status": "reconciling",
                "note": note,
                "current_page": serde_json::to_value(&observation).unwrap_or(serde_json::Value::Null),
            }),
            note,
        ),
        None => (
            serde_json::json!({
                "status": "reconciling",
                "note": "the browser session is not live in this process; the armed operation's \
                         external effect is not observable and is never replayed",
            }),
            "browser session not live".to_string(),
        ),
    };
    let _ = detail;
    let raw = seal(store, &normalized, "browser-recovery")?;
    let evidence = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: format!("{operation_name}.recovery"),
        source_instance_id: store.capabilities.incarnation.to_string(),
        source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
        producer_epoch: None,
        producer_schema_version: Some("1".into()),
        raw_ref: raw,
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: DecodeStatus::Decoded,
        observation_level: None,
        normalized_ref: None,
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    store.record_external_evidence(
        operation_id,
        attempt.attempt_id,
        evidence,
        None,
        EffectStatus::Unknown,
    )
}

/// Recover the live session an armed operation belongs to. The frozen input
/// was sealed into the operation before any external effect, so it is the
/// one trustworthy source of the session/profile identity; caller text is
/// never trusted. A session open names only its profile, so the live
/// session is resolved through the profile's current session.
fn resolve_reconcile_session(
    provider: &Arc<BrowserProvider>,
    operation: &OperationRecord,
) -> Option<Arc<super::BrowserSession>> {
    let frozen = provider
        .store()
        .cas
        .read_object(&operation.final_input_ref.digest)
        .ok()?;
    let frozen: serde_json::Value = serde_json::from_slice(&frozen).ok()?;
    if let Some(session_id) = frozen.get("session_id").and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse::<EntityId>().ok())
            .or_else(|| None)
    }) {
        return provider.session(session_id).ok();
    }
    let profile_id = frozen.get("profile_id").and_then(|value| value.as_str())?;
    let state = provider.state();
    state
        .sessions
        .values()
        .find(|session| session.profile_id == profile_id)
        .cloned()
}
