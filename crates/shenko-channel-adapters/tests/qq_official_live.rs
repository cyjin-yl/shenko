//! Live acceptance for the official QQ bot channel — operator-run, never CI.
//!
//! Contracts: docs/specs/channels.md and docs/specs/channel-adapters.md
//! (official QQ bot: the app ticket and its renewal, the single-chat reply
//! window, the acceptance predicate of a send, and the strict split between a
//! refusal and a lost answer).
//!
//! The repository has never observed a real platform round trip for this
//! channel: the capability audit compares official documents with the plugin's
//! source and says so in its own boundary section. This harness answers that
//! with evidence instead of a promise. It drives the real OpenAPI of one
//! operator-provisioned bot and records what the platform actually answered.
//!
//! Facts it keeps apart, because the contract keeps them apart:
//!
//! * Credentials arrive only through the environment. Nothing is stored, and
//!   the receipt document is scanned at the end to prove that neither the app
//!   secret nor the access token entered it. The configuration's own debug
//!   line is asserted to redact the secret.
//! * A probe is `accepted` only when the platform's own answer says so
//!   (`err_code == 0`). A 2xx answer without that field is `unverified`, never
//!   acceptance; a refusal keeps the platform's status, code and message.
//! * A lost answer is `unknown`: the request may have reached the platform. It
//!   is never reported as delivered, and it is never followed by a blind
//!   retry.
//! * The two single-chat paths the shipped plugin does not produce from a
//!   reply — the documented streaming endpoint and the interaction-recall
//!   (`is_wakeup`) message — are measured independently of the plugin, with
//!   the platform's documented `QQBot` authorization header, so a refusal
//!   there is the platform's own answer about those paths and not a claim
//!   about the capability table.
//!
//! Run it deliberately:
//!
//! ```text
//! SHENKO_QQ_OFFICIAL_APP_ID=...        the bot AppID
//! SHENKO_QQ_OFFICIAL_APP_SECRET=...    the bot AppSecret (never stored)
//! SHENKO_QQ_OFFICIAL_LIVE_CONSENT=1    accept the live effects listed below
//! SHENKO_QQ_OFFICIAL_LIVE_OPENID=...   the target user OpenID of a single chat
//! # or, instead of the OpenID, one real captured C2C gateway frame:
//! SHENKO_QQ_OFFICIAL_LIVE_INBOUND_EVENT=/path/to/frame.json
//! cargo test -p shenko-channel-adapters --test qq_official_live -- --nocapture
//! ```
//!
//! Live effects of one run: one single-chat message through the shipped
//! plugin (a passive reply when a captured event supplies the platform message
//! id, otherwise an active message), one request to the documented streaming
//! endpoint, one interaction-recall (`is_wakeup`) message, and — unless
//! `SHENKO_QQ_OFFICIAL_LIVE_KEEP=1` — one retract of what the plugin sent.
//! The recall message consumes one of the four the platform grants per 30 days
//! for that user; a refusal is recorded, never retried.
//!
//! Optional: `SHENKO_QQ_OFFICIAL_API_BASE` and
//! `SHENKO_QQ_OFFICIAL_TOKEN_BASE` (a sandbox deployment),
//! `SHENKO_QQ_OFFICIAL_LIVE_TIMEOUT_SECS` (default 60, clamped to 5..=300) and
//! `SHENKO_QQ_OFFICIAL_LIVE_RECEIPT` (where the receipt document is written;
//! default: the temporary directory).
//!
//! Without the credentials, the consent and a target, the harness prints
//! `SKIP:` for each missing precondition and touches nothing. It never
//! fabricates a result: a run that obtains no platform answer reports the
//! failure it saw.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use shenko_channel_adapters::common::{endpoint_provenance, integer_field, text_field};
use shenko_channel_adapters::qq_official::api::{QqApiError, QqOfficialApi};
use shenko_channel_adapters::qq_official::config::{
    DEFAULT_API_BASE, DEFAULT_TOKEN_BASE, QqOfficialConfig,
};
use shenko_channel_adapters::qq_official::events::{
    QQ_OFFICIAL_CHANNEL_ID, QQ_OFFICIAL_KIND_PRIVATE, QQ_OFFICIAL_KIND_RETRACT, QQ_OFFICIAL_SOURCE,
};
use shenko_channel_adapters::qq_official::outbound::{OutboundScope, build_outbound};
use shenko_channel_adapters::qq_official::plugin::QqOfficialChannel;
use shenko_channel_adapters::{
    HealthState, PlatformAnswer, PlatformHttp, ReplyContext, ReplyContextCache,
};
use shenko_types::channel::{ChannelAction, ChannelError, ChannelPlugin, ChannelReceipt};

/// The bot AppID. Required for a live run.
const ENV_APP_ID: &str = "SHENKO_QQ_OFFICIAL_APP_ID";
/// The bot AppSecret. Required, used only to buy the access token.
const ENV_APP_SECRET: &str = "SHENKO_QQ_OFFICIAL_APP_SECRET";
/// The explicit consent to the live effects of one run.
const ENV_CONSENT: &str = "SHENKO_QQ_OFFICIAL_LIVE_CONSENT";
/// The user OpenID of the single chat to answer.
const ENV_OPENID: &str = "SHENKO_QQ_OFFICIAL_LIVE_OPENID";
/// One captured C2C gateway frame; stands in for the OpenID and supplies the
/// platform message id a passive reply must carry.
const ENV_INBOUND_EVENT: &str = "SHENKO_QQ_OFFICIAL_LIVE_INBOUND_EVENT";
/// The OpenAPI host of an alternative deployment.
const ENV_API_BASE: &str = "SHENKO_QQ_OFFICIAL_API_BASE";
/// The token host of an alternative deployment.
const ENV_TOKEN_BASE: &str = "SHENKO_QQ_OFFICIAL_TOKEN_BASE";
/// Keep the message the plugin sent instead of retracting it.
const ENV_KEEP: &str = "SHENKO_QQ_OFFICIAL_LIVE_KEEP";
/// Where the receipt document is written.
const ENV_RECEIPT: &str = "SHENKO_QQ_OFFICIAL_LIVE_RECEIPT";
/// The per-request deadline in seconds.
const ENV_TIMEOUT: &str = "SHENKO_QQ_OFFICIAL_LIVE_TIMEOUT_SECS";

/// The variable NAMES this harness reads. Recorded in the receipt document so
/// an operator can reproduce the run; the values never are.
const ENVIRONMENT: [&str; 10] = [
    ENV_APP_ID,
    ENV_APP_SECRET,
    ENV_CONSENT,
    ENV_OPENID,
    ENV_INBOUND_EVENT,
    ENV_API_BASE,
    ENV_TOKEN_BASE,
    ENV_KEEP,
    ENV_RECEIPT,
    ENV_TIMEOUT,
];

/// The receipt document format.
const RECEIPT_SCHEMA_VERSION: u32 = 1;
/// The default deadline of one platform call.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The plugin's own acceptance predicate, recorded with every receipt.
const ACCEPTANCE_PREDICATE: &str = "qq_official_err_code_zero";
/// Every probe is recorded exactly once, with its skip reason when it did not
/// run.
const PROBES: [&str; 5] = [
    "token",
    "single_chat_send",
    "single_chat_stream",
    "interaction_recall",
    "single_chat_retract",
];

// ---------------------------------------------------------------------------
// Preconditions
// ---------------------------------------------------------------------------

/// One provisioned live run.
#[derive(Debug, Clone)]
struct LiveEnv {
    app_id: String,
    app_secret: String,
    /// The configured single-chat audience, when one was named directly.
    openid: Option<String>,
    /// One captured gateway frame, when the audience comes from a real event.
    inbound_event: Option<PathBuf>,
    api_base: String,
    token_base: String,
    keep: bool,
    receipt: Option<PathBuf>,
    timeout: Duration,
}

/// Whether a live run may start, and if not, exactly why not.
#[derive(Debug, Clone)]
enum LiveGate {
    Ready(LiveEnv),
    Skip(Vec<String>),
}

fn read<F: Fn(&str) -> Option<String>>(lookup: &F, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl LiveGate {
    /// Decide from an injectable environment lookup, so the skip path is
    /// testable without touching the process environment.
    fn from_lookup<F: Fn(&str) -> Option<String>>(lookup: F) -> Self {
        let mut missing = Vec::new();
        let app_id = read(&lookup, ENV_APP_ID);
        if app_id.is_none() {
            missing.push(format!("{ENV_APP_ID}=<the bot AppID>"));
        }
        let app_secret = read(&lookup, ENV_APP_SECRET);
        if app_secret.is_none() {
            missing.push(format!("{ENV_APP_SECRET}=<the bot AppSecret>"));
        }
        let consent = read(&lookup, ENV_CONSENT);
        if consent.is_none() {
            missing.push(format!(
                "{ENV_CONSENT}=1 (accepts one live single-chat message, one streaming request, \
                 one interaction-recall message and one retract)"
            ));
        }
        let openid = read(&lookup, ENV_OPENID);
        let inbound_event = read(&lookup, ENV_INBOUND_EVENT);
        if openid.is_none() && inbound_event.is_none() {
            missing.push(format!(
                "{ENV_OPENID}=<the target user OpenID> or \
                 {ENV_INBOUND_EVENT}=<one captured C2C gateway frame>"
            ));
        }
        if !missing.is_empty() {
            return LiveGate::Skip(missing);
        }
        let timeout = read(&lookup, ENV_TIMEOUT)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(5, 300);
        LiveGate::Ready(LiveEnv {
            // The required values were checked above; a missing one returned.
            app_id: app_id.unwrap_or_default(),
            app_secret: app_secret.unwrap_or_default(),
            openid,
            inbound_event: inbound_event.map(PathBuf::from),
            api_base: read(&lookup, ENV_API_BASE).unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            token_base: read(&lookup, ENV_TOKEN_BASE)
                .unwrap_or_else(|| DEFAULT_TOKEN_BASE.to_string()),
            keep: read(&lookup, ENV_KEEP).as_deref() == Some("1"),
            receipt: read(&lookup, ENV_RECEIPT).map(PathBuf::from),
            timeout: Duration::from_secs(timeout),
        })
    }
}

// ---------------------------------------------------------------------------
// What the platform actually answered
// ---------------------------------------------------------------------------

/// The verdict of one probe, decided only by the platform's own answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
enum LiveOutcome {
    /// The platform answered and its own acceptance field is zero.
    Accepted {
        code: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
    },
    /// The platform answered 2xx, but the answer carries no acceptance field:
    /// recorded as unverified rather than read as success.
    Unverified {
        detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
    },
    /// The platform answered and refused.
    Refused {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
    },
    /// The answer was lost; the request may have reached the platform.
    Unknown { detail: String },
    /// The probe was not attempted, with the reason.
    Skipped { reason: String },
}

impl LiveOutcome {
    fn name(&self) -> &'static str {
        match self {
            LiveOutcome::Accepted { .. } => "accepted",
            LiveOutcome::Unverified { .. } => "unverified",
            LiveOutcome::Refused { .. } => "refused",
            LiveOutcome::Unknown { .. } => "unknown",
            LiveOutcome::Skipped { .. } => "skipped",
        }
    }
}

/// The plugin's own verdict on one dispatch, recorded next to the platform's
/// answer so an inverted verdict is detectable.
#[derive(Debug, Serialize)]
struct PluginVerdict {
    accepted: bool,
    retcode: i64,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_message_id: Option<String>,
}

/// The parts of a receipt that do not depend on the answer.
struct ProbeParts {
    probe: &'static str,
    platform_action: String,
    /// `plugin-dispatch` when the shipped plugin produced the call, `absent`
    /// when the harness reached the documented platform path the plugin does
    /// not produce.
    adapter_path: &'static str,
    endpoint: String,
    /// `platform_body` when this harness read the platform's own
    /// err_code/code, `client_typed` when only the shipped client's typed
    /// answer was exposed, `not_attempted` when nothing was sent.
    basis: &'static str,
}

/// One probe's evidence.
#[derive(Debug, Serialize)]
struct LiveReceipt {
    schema_version: u32,
    probe: &'static str,
    platform_action: String,
    adapter_path: &'static str,
    endpoint: String,
    basis: &'static str,
    /// The request the platform was asked, without any credential.
    request: Value,
    outcome: LiveOutcome,
    /// The platform's own answer, untouched, when the call exposed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin_verdict: Option<PluginVerdict>,
    observed_at: String,
}

impl LiveReceipt {
    fn from_parts(
        parts: ProbeParts,
        request: Value,
        outcome: LiveOutcome,
        answer: Option<Value>,
        plugin_verdict: Option<PluginVerdict>,
    ) -> Self {
        Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            probe: parts.probe,
            platform_action: parts.platform_action,
            adapter_path: parts.adapter_path,
            endpoint: parts.endpoint,
            basis: parts.basis,
            request,
            outcome,
            answer,
            plugin_verdict,
            observed_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// The whole run: the credential-free provenance plus the probes.
#[derive(Debug, Serialize)]
struct LiveDocument {
    schema_version: u32,
    harness: &'static str,
    started_at: String,
    finished_at: String,
    /// Credential-free endpoint provenance.
    api_base: String,
    token_base: String,
    /// The account namespace this bot serves (`qq:<app_id>`).
    account: String,
    /// How the interaction was bound (never the credential).
    interaction: Value,
    /// The variable NAMES read, never their values.
    environment: Vec<&'static str>,
    receipts: Vec<LiveReceipt>,
}

fn document_of(
    started_at: String,
    account: String,
    api_base: String,
    token_base: String,
    interaction: Value,
    receipts: Vec<LiveReceipt>,
) -> LiveDocument {
    LiveDocument {
        schema_version: RECEIPT_SCHEMA_VERSION,
        harness: "qq-official-live-acceptance",
        started_at,
        finished_at: chrono::Utc::now().to_rfc3339(),
        api_base,
        token_base,
        account,
        interaction,
        environment: ENVIRONMENT.to_vec(),
        receipts,
    }
}

/// Decide one probe from a raw platform answer.
fn classify_answer(answer: &PlatformAnswer) -> (LiveOutcome, Option<Value>) {
    match answer {
        PlatformAnswer::Answered { status, body } => {
            (classify_body(Some(*status), body), Some(body.clone()))
        }
        PlatformAnswer::Rejected {
            status,
            code,
            message,
            body,
        } => (
            LiveOutcome::Refused {
                message: message.clone(),
                code: *code,
                http_status: Some(*status),
            },
            (!body.is_null()).then(|| body.clone()),
        ),
        PlatformAnswer::Unknown { detail } => (
            LiveOutcome::Unknown {
                detail: detail.clone(),
            },
            None,
        ),
    }
}

/// Decide one probe from the body alone, when the observation path (the
/// shipped plugin's receipt) does not expose the HTTP status line.
fn classify_body(http_status: Option<u16>, body: &Value) -> LiveOutcome {
    let code = integer_field(body, "err_code").or_else(|| integer_field(body, "code"));
    match code {
        Some(0) => LiveOutcome::Accepted {
            code: 0,
            http_status,
        },
        Some(code) => LiveOutcome::Refused {
            message: text_field(body, "message")
                .or_else(|| text_field(body, "msg"))
                .unwrap_or_default(),
            code: Some(code),
            http_status,
        },
        None => LiveOutcome::Unverified {
            detail: "the platform answer carried no err_code/code, so acceptance is not stated"
                .to_string(),
            http_status,
        },
    }
}

/// The platform's own code in an answer, if it stated one.
fn observed_code(answer: &Value) -> Option<i64> {
    integer_field(answer, "err_code").or_else(|| integer_field(answer, "code"))
}

/// Decide one probe from one plugin dispatch result.
fn classify_dispatch(
    dispatch: Result<ChannelReceipt, ChannelError>,
) -> (LiveOutcome, Option<Value>, Option<PluginVerdict>) {
    match dispatch {
        Ok(receipt) => {
            let verdict = PluginVerdict {
                accepted: receipt.accepted,
                retcode: receipt.retcode,
                status: receipt.status.clone(),
                provider_message_id: receipt.provider_message_id.clone(),
            };
            let outcome = classify_body(None, &receipt.raw);
            (outcome, Some(receipt.raw), Some(verdict))
        }
        Err(error) => (classify_error(&error), None, None),
    }
}

/// Decide one probe from a plugin error. A deterministic refusal (an
/// unsupported target, an unresolved id) never reached the platform; a
/// transport failure or an unknown outcome may have.
fn classify_error(error: &ChannelError) -> LiveOutcome {
    match error {
        ChannelError::UnsupportedTarget { channel, kind } => LiveOutcome::Skipped {
            reason: format!("the plugin does not serve {kind} on {channel}"),
        },
        ChannelError::UnresolvedReply { channel, id } => LiveOutcome::Skipped {
            reason: format!("the plugin cannot resolve {id} on {channel}"),
        },
        ChannelError::Transport { detail, .. } | ChannelError::Unknown { detail, .. } => {
            LiveOutcome::Unknown {
                detail: detail.clone(),
            }
        }
        ChannelError::Rejected { detail, .. } => LiveOutcome::Refused {
            message: detail.clone(),
            code: None,
            http_status: None,
        },
    }
}

/// Decide one probe from the token request's typed error.
fn classify_api_error(error: &QqApiError) -> LiveOutcome {
    match error {
        QqApiError::Rejected {
            status,
            code,
            message,
        } => LiveOutcome::Refused {
            message: message.clone(),
            code: *code,
            http_status: Some(*status),
        },
        QqApiError::Unknown(detail) => LiveOutcome::Unknown {
            detail: detail.clone(),
        },
        QqApiError::Incomplete(detail) => LiveOutcome::Unverified {
            detail: detail.clone(),
            http_status: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Bookkeeping: no claim without the platform's own answer
// ---------------------------------------------------------------------------

/// Everything wrong with a receipt document, as text an operator can act on.
///
/// This is the harness's teeth: an acceptance without the platform's answer, a
/// plugin verdict that contradicts the recorded answer, a probe missing from
/// the document, or any credential inside it is a failure — never a pass.
fn bookkeeping_violations(document: &LiveDocument, secrets: &[&str]) -> Vec<String> {
    let mut violations = Vec::new();
    for probe in PROBES {
        let count = document
            .receipts
            .iter()
            .filter(|receipt| receipt.probe == probe)
            .count();
        if count != 1 {
            violations.push(format!(
                "probe {probe} appears {count} times: every probe is recorded exactly once, \
                 including its skip reason"
            ));
        }
    }
    for receipt in &document.receipts {
        match (&receipt.outcome, receipt.answer.as_ref()) {
            (LiveOutcome::Accepted { code, .. }, Some(answer)) => {
                let observed = observed_code(answer);
                if observed != Some(*code) {
                    violations.push(format!(
                        "{}: the acceptance claims err_code {code} but the recorded answer \
                         states {observed:?}",
                        receipt.probe
                    ));
                }
            }
            (LiveOutcome::Accepted { .. }, None) => {
                // The token client admits only `code == 0` with an
                // `access_token` present and never exposes the body — that body
                // carries the credential — so its acceptance is recorded as
                // `client_typed`. Any other acceptance without the platform's
                // own body is not evidence.
                if receipt.basis == "platform_body" {
                    violations.push(format!(
                        "{}: an acceptance without the platform's answer is not evidence",
                        receipt.probe
                    ));
                }
            }
            (LiveOutcome::Refused { code, .. }, Some(answer)) => {
                if let (Some(claimed), Some(observed)) = (code, observed_code(answer)) {
                    if *claimed != observed {
                        violations.push(format!(
                            "{}: the refusal claims err_code {claimed} but the recorded answer \
                             states {observed}",
                            receipt.probe
                        ));
                    }
                }
            }
            (LiveOutcome::Skipped { reason }, _) if reason.trim().is_empty() => {
                violations.push(format!("{}: a skip states why it did not run", receipt.probe));
            }
            (LiveOutcome::Unknown { detail }, _) if detail.trim().is_empty() => violations.push(
                format!("{}: an unknown outcome states what was lost", receipt.probe),
            ),
            _ => {}
        }
        if matches!(receipt.outcome, LiveOutcome::Skipped { .. })
            && receipt.plugin_verdict.is_some()
        {
            violations.push(format!(
                "{}: a probe that did not run cannot carry a plugin verdict",
                receipt.probe
            ));
        }
        if let Some(verdict) = &receipt.plugin_verdict {
            if let Some(conflict) = verdict_conflict(&receipt.outcome, verdict) {
                violations.push(format!("{}: {conflict}", receipt.probe));
            }
        }
        if receipt.endpoint.trim().is_empty() {
            violations.push(format!("{}: the receipt names no endpoint", receipt.probe));
        }
        if receipt.basis.is_empty() {
            violations.push(format!(
                "{}: the receipt names no evidence basis",
                receipt.probe
            ));
        }
    }
    let text = serde_json::to_string(document).unwrap_or_default();
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        if text.contains(secret) {
            violations.push(
                "the receipt document carries a credential: the app secret and the access token \
                 are never recorded"
                    .to_string(),
            );
        }
    }
    violations
}

/// Whether the plugin's own verdict disagrees with the platform's answer.
fn verdict_conflict(outcome: &LiveOutcome, verdict: &PluginVerdict) -> Option<String> {
    let outcome_says_accepted = matches!(
        outcome,
        LiveOutcome::Accepted { .. } | LiveOutcome::Unverified { .. }
    );
    if verdict.accepted != outcome_says_accepted {
        return Some(format!(
            "the plugin reported accepted={} while the platform's answer is {}",
            verdict.accepted,
            outcome.name()
        ));
    }
    None
}

/// The probes produced by the shipped plugin; a refusal or a lost answer there
/// is an acceptance failure, while the two platform-only probes are
/// measurements.
fn shipped_path_failures(document: &LiveDocument) -> Vec<String> {
    let mut failures = Vec::new();
    for receipt in &document.receipts {
        if !matches!(receipt.probe, "single_chat_send" | "single_chat_retract") {
            continue;
        }
        match &receipt.outcome {
            LiveOutcome::Refused {
                code,
                message,
                http_status,
            } => failures.push(format!(
                "{}: the platform refused it (http {http_status:?}, err_code {code:?}): {message}",
                receipt.probe
            )),
            LiveOutcome::Unverified { detail, .. } => failures.push(format!(
                "{}: the platform answered without an acceptance field: {detail}",
                receipt.probe
            )),
            LiveOutcome::Unknown { detail } => failures.push(format!(
                "{}: the answer was lost ({detail}); reconcile on the platform before retrying",
                receipt.probe
            )),
            LiveOutcome::Accepted { .. } | LiveOutcome::Skipped { .. } => {}
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// The interaction a live run answers
// ---------------------------------------------------------------------------

/// The conversation this run addresses.
#[derive(Debug, Clone)]
struct Interaction {
    /// The plugin's reply kind (`qqo-private`, `qqo-group`, ...).
    kind: String,
    /// The opaque audience id the plugin resolves.
    audience: String,
    /// The conversation key the plugin's reply context is stored under.
    conversation: String,
    /// The platform message id a passive reply must carry, when the platform
    /// gave one.
    message_id: Option<String>,
    /// `captured_event` or `configured_openid`.
    source: &'static str,
    /// The platform event name of the captured frame, when there is one.
    event: Option<String>,
}

impl Interaction {
    /// Whether the audience is one single chat.
    fn single_chat(&self) -> bool {
        self.kind == QQ_OFFICIAL_KIND_PRIVATE
    }

    /// The credential-free description recorded in the document.
    fn describe(&self) -> Value {
        json!({
            "source": self.source,
            "kind": self.kind.clone(),
            "audience": self.audience.clone(),
            "event": self.event.clone(),
            "passive_reply": self.message_id.is_some(),
        })
    }
}

fn resolve_interaction(
    channel: &QqOfficialChannel,
    env: &LiveEnv,
) -> Result<Interaction, Box<dyn std::error::Error>> {
    if let Some(path) = &env.inbound_event {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {ENV_INBOUND_EVENT} {}: {error}", path.display()))?;
        let raw: Value = serde_json::from_str(&text)
            .map_err(|error| format!("{ENV_INBOUND_EVENT} {} is not JSON: {error}", path.display()))?;
        let envelope = channel.classify(QQ_OFFICIAL_SOURCE, &raw).ok_or_else(|| {
            format!(
                "{} is not an official QQ message event this plugin classifies \
                 (C2C_MESSAGE_CREATE, GROUP_AT_MESSAGE_CREATE, GROUP_MESSAGE_CREATE, \
                 AT_MESSAGE_CREATE or DIRECT_MESSAGE_CREATE)",
                path.display()
            )
        })?;
        // A captured frame is only useful as a passive reply when it names the
        // platform message: without that id the plugin would send an ACTIVE
        // message while the operator believes an interaction was answered.
        assert!(
            !envelope.message_id.is_empty(),
            "{} carries no platform message id, so a passive reply cannot be bound; name the \
             single chat with {ENV_OPENID} instead",
            path.display()
        );
        let target = envelope.reply_target.ok_or_else(|| {
            format!(
                "{} names no reply audience: this event cannot be answered",
                path.display()
            )
        })?;
        assert!(
            !target.id.is_empty(),
            "{} names an empty reply audience: nothing can be addressed",
            path.display()
        );
        let message_id = Some(envelope.message_id.clone());
        let conversation = target
            .conversation
            .clone()
            .unwrap_or_else(|| target.id.clone());
        return Ok(Interaction {
            kind: target.kind,
            audience: target.id,
            conversation,
            message_id,
            source: "captured_event",
            event: raw.get("t").and_then(Value::as_str).map(str::to_owned),
        });
    }
    let audience = env.openid.clone().ok_or_else(|| {
        format!("{ENV_OPENID} or {ENV_INBOUND_EVENT} must name the single chat to answer")
    })?;
    Ok(Interaction {
        kind: QQ_OFFICIAL_KIND_PRIVATE.to_string(),
        audience: audience.clone(),
        conversation: audience,
        message_id: None,
        source: "configured_openid",
        event: None,
    })
}

// ---------------------------------------------------------------------------
// The live run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn official_qq_live_acceptance() -> Result<(), Box<dyn std::error::Error>> {
    let env = match LiveGate::from_lookup(|name: &str| std::env::var(name).ok()) {
        LiveGate::Ready(env) => env,
        LiveGate::Skip(reasons) => {
            for reason in reasons {
                eprintln!("SKIP: {reason}");
            }
            return Ok(());
        }
    };
    let started_at = chrono::Utc::now().to_rfc3339();
    let http = PlatformHttp::new()
        .map_err(|error| format!("the platform client did not build: {error}"))?;
    let mut config = QqOfficialConfig::new(env.app_id.clone(), env.app_secret.clone());
    config.api_base = env.api_base.clone();
    config.token_base = env.token_base.clone();
    let account = config.account_scope();
    let config = Arc::new(config);
    // A credential must never reach a log line: the plugin's own debug output
    // is checked before anything is sent.
    assert!(
        !format!("{config:?}").contains(&env.app_secret),
        "the configuration never renders the app secret"
    );
    let api = QqOfficialApi::new(Arc::clone(&config), http);
    let channel = QqOfficialChannel::new(
        Arc::clone(&config),
        api.clone(),
        HealthState::disabled(
            QQ_OFFICIAL_CHANNEL_ID,
            account.as_str(),
            "operator-run live acceptance harness",
        ),
        ReplyContextCache::new(
            config.reply_context_capacity,
            Duration::from_secs(config.reply_window_seconds.max(1)),
        ),
    );
    let interaction = resolve_interaction(&channel, &env)?;
    // The same step the gateway performs when it classifies one inbound
    // message: remember the platform message id so a passive reply can carry
    // it. Nothing is invented when the platform gave none.
    if let Some(message_id) = &interaction.message_id {
        channel.replies().record(
            &interaction.conversation,
            ReplyContext {
                message_id: message_id.clone(),
                context_token: None,
                thread_id: None,
                at: chrono::Utc::now(),
            },
        );
    }
    let run_id = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let api_base_provenance = endpoint_provenance(config.api_base());
    let token_base_provenance = endpoint_provenance(config.token_base());
    let token_endpoint = format!("{token_base_provenance}/app/getAppAccessToken");
    let mut receipts: Vec<LiveReceipt> = Vec::new();
    let mut failure: Option<String> = None;

    // ── probe: token ───────────────────────────────────────────────────────
    let mut token: Option<String> = None;
    match tokio::time::timeout(env.timeout, api.access_token()).await {
        Ok(Ok(value)) => {
            token = Some(value);
            receipts.push(LiveReceipt::from_parts(
                ProbeParts {
                    probe: "token",
                    platform_action: "app_access_token".to_string(),
                    adapter_path: "plugin-dispatch",
                    endpoint: token_endpoint.clone(),
                    basis: "client_typed",
                },
                json!({"grant": "app_access_token", "account": account.clone()}),
                LiveOutcome::Accepted {
                    code: 0,
                    http_status: None,
                },
                None,
                None,
            ));
        }
        Ok(Err(error)) => {
            let outcome = classify_api_error(&error);
            failure = Some(format!("the platform refused the app ticket: {error}"));
            receipts.push(LiveReceipt::from_parts(
                ProbeParts {
                    probe: "token",
                    platform_action: "app_access_token".to_string(),
                    adapter_path: "plugin-dispatch",
                    endpoint: token_endpoint.clone(),
                    basis: "client_typed",
                },
                json!({"grant": "app_access_token", "account": account.clone()}),
                outcome,
                None,
                None,
            ));
        }
        Err(_) => {
            let detail = format!(
                "the token request did not answer within {}s",
                env.timeout.as_secs()
            );
            failure = Some(detail.clone());
            receipts.push(LiveReceipt::from_parts(
                ProbeParts {
                    probe: "token",
                    platform_action: "app_access_token".to_string(),
                    adapter_path: "plugin-dispatch",
                    endpoint: token_endpoint.clone(),
                    basis: "client_typed",
                },
                json!({"grant": "app_access_token", "account": account.clone()}),
                LiveOutcome::Unknown { detail },
                None,
                None,
            ));
        }
    }

    let Some(token) = token else {
        // Nothing can reach the platform without a token; every probe is
        // recorded with the reason, never left silent.
        for probe in [
            "single_chat_send",
            "single_chat_stream",
            "interaction_recall",
            "single_chat_retract",
        ] {
            receipts.push(LiveReceipt::from_parts(
                ProbeParts {
                    probe,
                    platform_action: "none".to_string(),
                    adapter_path: "absent",
                    endpoint: api_base_provenance.clone(),
                    basis: "not_attempted",
                },
                json!({"skipped": true}),
                LiveOutcome::Skipped {
                    reason: "no access token was issued, so no platform call was attempted"
                        .to_string(),
                },
                None,
                None,
            ));
        }
        let document = document_of(
            started_at,
            account,
            api_base_provenance,
            token_base_provenance,
            interaction.describe(),
            receipts,
        );
        let path = write_document(&document, env.receipt.as_deref())?;
        eprintln!("receipt document: {}", path.display());
        report(&document);
        let mut failures = bookkeeping_violations(&document, &[env.app_secret.as_str()]);
        if let Some(failure) = failure {
            failures.push(failure);
        }
        if !failures.is_empty() {
            return Err(failures.join("\n").into());
        }
        return Ok(());
    };

    let authorization = format!("QQBot {token}");

    // ── probe: single_chat_send (the shipped plugin's own path) ────────────
    let send_target = channel
        .resolve_reply(&interaction.kind, &interaction.audience)
        .ok_or_else(|| {
            format!(
                "the plugin does not resolve reply kind {} for a live send",
                interaction.kind
            )
        })?;
    let send_dispatch = channel
        .describe(&send_target)
        .map_err(|error| format!("the plugin cannot describe its own single-chat send: {error}"))?;
    assert_eq!(
        send_dispatch.action,
        ChannelAction::Send,
        "a single-chat reply is a send"
    );
    assert_eq!(
        send_dispatch.acceptance_predicate, ACCEPTANCE_PREDICATE,
        "the plugin states how acceptance is judged"
    );
    assert_eq!(
        send_dispatch
            .params
            .get("msg_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        interaction.message_id.clone(),
        "a passive reply carries exactly the platform message id of the event it answers, and an \
         active message carries none"
    );
    let send_content = format!("shenko live acceptance {run_id} / single chat send");
    let (send_outcome, send_answer, send_verdict) = classify_dispatch(
        tokio::time::timeout(
            env.timeout,
            channel.dispatch(&send_dispatch, &send_content),
        )
        .await
        .unwrap_or_else(|_| {
            Err(ChannelError::Unknown {
                channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
                detail: format!(
                    "the send did not answer within {}s; the platform may have delivered it",
                    env.timeout.as_secs()
                ),
            })
        }),
    );
    let send_accepted = matches!(send_outcome, LiveOutcome::Accepted { .. });
    receipts.push(LiveReceipt::from_parts(
        ProbeParts {
            probe: "single_chat_send",
            platform_action: send_dispatch.platform_action.clone(),
            adapter_path: "plugin-dispatch",
            endpoint: api_base_provenance.clone(),
            basis: if send_answer.is_some() {
                "platform_body"
            } else {
                "client_typed"
            },
        },
        json!({
            "kind": interaction.kind.clone(),
            "audience": interaction.audience.clone(),
            "msg_id": send_dispatch.params.get("msg_id"),
            "msg_seq": send_dispatch.params.get("msg_seq"),
            "content": send_content.clone(),
        }),
        send_outcome,
        send_answer,
        send_verdict,
    ));

    // ── probe: single_chat_stream (documented endpoint, not produced here) ─
    if interaction.single_chat() {
        let body = stream_request_body(&send_content, &interaction, 2);
        let url = format!(
            "{}/v2/users/{}/stream_messages",
            config.api_base(),
            interaction.audience
        );
        let answer = tokio::time::timeout(
            env.timeout,
            api.http().post_json_with_headers(
                &url,
                &[("Authorization".to_string(), authorization.clone())],
                &body,
            ),
        )
        .await;
        let (outcome, raw) = match answer {
            Ok(answer) => classify_answer(&answer),
            Err(_) => (
                LiveOutcome::Unknown {
                    detail: format!(
                        "the streaming request did not answer within {}s; the platform may have \
                         delivered it",
                        env.timeout.as_secs()
                    ),
                },
                None,
            ),
        };
        receipts.push(LiveReceipt::from_parts(
            ProbeParts {
                probe: "single_chat_stream",
                platform_action: "stream_private_message".to_string(),
                adapter_path: "absent",
                endpoint: endpoint_provenance(&url),
                basis: "platform_body",
            },
            body,
            outcome,
            raw,
            None,
        ));
    } else {
        receipts.push(LiveReceipt::from_parts(
            ProbeParts {
                probe: "single_chat_stream",
                platform_action: "stream_private_message".to_string(),
                adapter_path: "absent",
                endpoint: api_base_provenance.clone(),
                basis: "not_attempted",
            },
            json!({"kind": interaction.kind.clone()}),
            LiveOutcome::Skipped {
                reason: "the platform does not stream group messages; this probe is single-chat \
                         only"
                    .to_string(),
            },
            None,
            None,
        ));
    }

    // ── probe: interaction_recall (documented `is_wakeup`, not produced here)
    if interaction.single_chat() {
        let mut body = build_outbound(
            &format!("shenko live acceptance {run_id} / interaction recall"),
            OutboundScope::Private,
        )
        .wire_body();
        body["is_wakeup"] = json!(true);
        let url = format!(
            "{}/v2/users/{}/messages",
            config.api_base(),
            interaction.audience
        );
        let answer = tokio::time::timeout(
            env.timeout,
            api.http().post_json_with_headers(
                &url,
                &[("Authorization".to_string(), authorization.clone())],
                &body,
            ),
        )
        .await;
        let (outcome, raw) = match answer {
            Ok(answer) => classify_answer(&answer),
            Err(_) => (
                LiveOutcome::Unknown {
                    detail: format!(
                        "the interaction-recall request did not answer within {}s; the platform \
                         may have delivered it",
                        env.timeout.as_secs()
                    ),
                },
                None,
            ),
        };
        receipts.push(LiveReceipt::from_parts(
            ProbeParts {
                probe: "interaction_recall",
                platform_action: "send_private_wakeup".to_string(),
                adapter_path: "absent",
                endpoint: endpoint_provenance(&url),
                basis: "platform_body",
            },
            body,
            outcome,
            raw,
            None,
        ));
    } else {
        receipts.push(LiveReceipt::from_parts(
            ProbeParts {
                probe: "interaction_recall",
                platform_action: "send_private_wakeup".to_string(),
                adapter_path: "absent",
                endpoint: api_base_provenance.clone(),
                basis: "not_attempted",
            },
            json!({"kind": interaction.kind.clone()}),
            LiveOutcome::Skipped {
                reason: "the interaction-recall message is a single-chat path; this interaction \
                         is not one"
                    .to_string(),
            },
            None,
            None,
        ));
    }

    // ── probe: single_chat_retract (the shipped plugin's recall path) ──────
    let retract = if !send_accepted {
        LiveReceipt::from_parts(
            ProbeParts {
                probe: "single_chat_retract",
                platform_action: "retract_message".to_string(),
                adapter_path: "plugin-dispatch",
                endpoint: api_base_provenance.clone(),
                basis: "not_attempted",
            },
            json!({"conversation": interaction.conversation.clone()}),
            LiveOutcome::Skipped {
                reason: "nothing of this bot's own was accepted in this conversation, so there is \
                         nothing to retract and no blind call is made"
                    .to_string(),
            },
            None,
            None,
        )
    } else if env.keep {
        LiveReceipt::from_parts(
            ProbeParts {
                probe: "single_chat_retract",
                platform_action: "retract_message".to_string(),
                adapter_path: "plugin-dispatch",
                endpoint: api_base_provenance.clone(),
                basis: "not_attempted",
            },
            json!({"conversation": interaction.conversation.clone()}),
            LiveOutcome::Skipped {
                reason: format!("{ENV_KEEP}=1 keeps the message the plugin sent"),
            },
            None,
            None,
        )
    } else {
        match channel.resolve_reply(QQ_OFFICIAL_KIND_RETRACT, &interaction.conversation) {
            Some(retract_target) => match channel.describe(&retract_target) {
                Ok(retract_dispatch) => {
                    let (outcome, answer, verdict) = classify_dispatch(
                        tokio::time::timeout(env.timeout, channel.dispatch(&retract_dispatch, ""))
                            .await
                            .unwrap_or_else(|_| {
                                Err(ChannelError::Unknown {
                                    channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
                                    detail: format!(
                                        "the retract did not answer within {}s; the platform may \
                                         have applied it",
                                        env.timeout.as_secs()
                                    ),
                                })
                            }),
                    );
                    LiveReceipt::from_parts(
                        ProbeParts {
                            probe: "single_chat_retract",
                            platform_action: "retract_message".to_string(),
                            adapter_path: "plugin-dispatch",
                            endpoint: api_base_provenance.clone(),
                            basis: if answer.is_some() {
                                "platform_body"
                            } else {
                                "client_typed"
                            },
                        },
                        json!({
                            "conversation": interaction.conversation.clone(),
                            "message_id": retract_dispatch.params.get("message_id"),
                            "scope": retract_dispatch.params.get("scope"),
                        }),
                        outcome,
                        answer,
                        verdict,
                    )
                }
                Err(error) => {
                    let outcome = classify_error(&error);
                    // A refusal or a lost answer reached the plugin's typed
                    // client; only a deterministic refusal of the target never
                    // left this process.
                    let basis = match &outcome {
                        LiveOutcome::Refused { .. } | LiveOutcome::Unknown { .. } => {
                            "client_typed"
                        }
                        _ => "not_attempted",
                    };
                    LiveReceipt::from_parts(
                        ProbeParts {
                            probe: "single_chat_retract",
                            platform_action: "retract_message".to_string(),
                            adapter_path: "plugin-dispatch",
                            endpoint: api_base_provenance.clone(),
                            basis,
                        },
                        json!({"conversation": interaction.conversation.clone()}),
                        outcome,
                        None,
                        None,
                    )
                }
            },
            None => LiveReceipt::from_parts(
                ProbeParts {
                    probe: "single_chat_retract",
                    platform_action: "retract_message".to_string(),
                    adapter_path: "plugin-dispatch",
                    endpoint: api_base_provenance.clone(),
                    basis: "not_attempted",
                },
                json!({"conversation": interaction.conversation.clone()}),
                LiveOutcome::Skipped {
                    reason: "the plugin does not resolve the retract kind for this conversation"
                        .to_string(),
                },
                None,
                None,
            ),
        }
    };
    receipts.push(retract);

    let document = document_of(
        started_at,
        account,
        api_base_provenance,
        token_base_provenance,
        interaction.describe(),
        receipts,
    );
    let path = write_document(&document, env.receipt.as_deref())?;
    eprintln!("receipt document: {}", path.display());
    report(&document);

    let mut failures =
        bookkeeping_violations(&document, &[env.app_secret.as_str(), token.as_str()]);
    failures.extend(shipped_path_failures(&document));
    if let Some(failure) = failure {
        failures.push(failure);
    }
    if !failures.is_empty() {
        return Err(failures.join("\n").into());
    }
    Ok(())
}

/// The single-chat streaming request: the plugin's own single-chat body at the
/// documented streaming endpoint.
///
/// The repository has no verified streaming continuation protocol, so this
/// harness sends the body the shipped single-chat path already produces and
/// records the platform's answer — including a parameter refusal — instead of
/// inventing continuation fields. The sequence number differs from the plain
/// send because the platform refuses a repeated `msg_id` + `msg_seq` pair.
fn stream_request_body(content: &str, interaction: &Interaction, msg_seq: u32) -> Value {
    let mut body = build_outbound(content, OutboundScope::Private).wire_body();
    if let Some(message_id) = &interaction.message_id {
        body["msg_id"] = json!(message_id);
        body["msg_seq"] = json!(msg_seq);
    }
    body
}

fn write_document(
    document: &LiveDocument,
    configured: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = match configured {
        Some(path) => path.to_path_buf(),
        None => std::env::temp_dir().join(format!(
            "shenko-qq-official-live-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%S")
        )),
    };
    let text = serde_json::to_string_pretty(document)?;
    std::fs::write(&path, text).map_err(|error| {
        format!(
            "cannot write the receipt document {}: {error}",
            path.display()
        )
    })?;
    Ok(path)
}

/// Print every probe's verdict so an operator reads the platform's own codes.
fn report(document: &LiveDocument) {
    eprintln!(
        "official QQ live acceptance: account {} · api {}",
        document.account, document.api_base
    );
    eprintln!("interaction: {}", document.interaction);
    for receipt in &document.receipts {
        eprintln!(
            "probe {} [{} via {}]: {}",
            receipt.probe,
            receipt.platform_action,
            receipt.adapter_path,
            serde_json::to_string(&receipt.outcome).unwrap_or_default()
        );
    }
}

// ---------------------------------------------------------------------------
// Offline assertions: the harness is honest even without a platform
// ---------------------------------------------------------------------------

fn gate_from(pairs: &[(&str, &str)]) -> LiveGate {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    LiveGate::from_lookup(move |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, value)| value.clone())
    })
}

fn probe_receipt(probe: &'static str, outcome: LiveOutcome) -> LiveReceipt {
    LiveReceipt::from_parts(
        ProbeParts {
            probe,
            platform_action: "fixture".to_string(),
            adapter_path: "absent",
            endpoint: "https://api.bot.qq.com".to_string(),
            basis: "platform_body",
        },
        json!({"fixture": true}),
        outcome,
        None,
        None,
    )
}

/// A document with one receipt per probe, all skipped, for bookkeeping tests.
fn all_skipped(reason: &str) -> LiveDocument {
    document_of(
        "2026-11-22T00:00:00+00:00".to_string(),
        "qq:fixture".to_string(),
        "https://api.bot.qq.com".to_string(),
        "https://bots.qq.com".to_string(),
        json!({"source": "fixture"}),
        PROBES
            .iter()
            .map(|probe| {
                probe_receipt(
                    *probe,
                    LiveOutcome::Skipped {
                        reason: reason.to_string(),
                    },
                )
            })
            .collect(),
    )
}

#[test]
fn the_gate_names_every_missing_precondition() {
    let LiveGate::Skip(reasons) = gate_from(&[]) else {
        panic!("nothing was provisioned, so no live run may start");
    };
    for name in [ENV_APP_ID, ENV_APP_SECRET, ENV_CONSENT, ENV_OPENID] {
        assert!(
            reasons.iter().any(|reason| reason.contains(name)),
            "the skip path names {name}: {reasons:?}"
        );
    }
}

#[test]
fn an_empty_credential_counts_as_absent() {
    let LiveGate::Skip(reasons) = gate_from(&[(ENV_APP_ID, "  "), (ENV_APP_SECRET, "")]) else {
        panic!("an empty credential is not a provisioned credential");
    };
    assert!(reasons.iter().any(|reason| reason.contains(ENV_APP_ID)));
    assert!(reasons.iter().any(|reason| reason.contains(ENV_APP_SECRET)));
}

#[test]
fn a_provisioned_run_needs_explicit_live_consent() {
    let LiveGate::Skip(reasons) = gate_from(&[
        (ENV_APP_ID, "102000000"),
        (ENV_APP_SECRET, "fixture-secret"),
        (ENV_OPENID, "OPENID"),
    ]) else {
        panic!("a live run needs the operator's consent, not only credentials");
    };
    assert!(reasons.iter().any(|reason| reason.contains(ENV_CONSENT)));
}

#[test]
fn a_captured_event_can_stand_in_for_the_target_openid() {
    let LiveGate::Ready(env) = gate_from(&[
        (ENV_APP_ID, "102000000"),
        (ENV_APP_SECRET, "fixture-secret"),
        (ENV_CONSENT, "1"),
        (ENV_INBOUND_EVENT, "/tmp/frame.json"),
    ]) else {
        panic!("the captured event names the single chat");
    };
    assert_eq!(env.openid, None);
    assert_eq!(env.inbound_event, Some(PathBuf::from("/tmp/frame.json")));
    assert_eq!(env.api_base, DEFAULT_API_BASE);
    assert_eq!(env.token_base, DEFAULT_TOKEN_BASE);
    assert_eq!(env.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    assert!(!env.keep);
}

#[test]
fn a_configured_openid_is_enough_for_a_live_run() {
    let LiveGate::Ready(env) = gate_from(&[
        (ENV_APP_ID, "102000000"),
        (ENV_APP_SECRET, "fixture-secret"),
        (ENV_CONSENT, "yes"),
        (ENV_OPENID, "OPENID"),
        (ENV_TIMEOUT, "9999"),
        (ENV_KEEP, "1"),
    ]) else {
        panic!("the four required facts are present");
    };
    assert_eq!(env.openid.as_deref(), Some("OPENID"));
    assert_eq!(env.inbound_event, None);
    assert_eq!(
        env.timeout,
        Duration::from_secs(300),
        "an unreasonable deadline is clamped, not honoured"
    );
    assert!(env.keep);
}

#[test]
fn the_classifier_reads_the_platform_own_code() {
    assert_eq!(
        classify_body(Some(200), &json!({"err_code": 0, "id": "msg-1"})),
        LiveOutcome::Accepted {
            code: 0,
            http_status: Some(200)
        }
    );
    assert_eq!(
        classify_body(
            Some(200),
            &json!({"err_code": 40034002, "message": "invalid openid"})
        ),
        LiveOutcome::Refused {
            message: "invalid openid".to_string(),
            code: Some(40034002),
            http_status: Some(200)
        }
    );
    assert_eq!(
        classify_body(None, &json!({"code": "0"})),
        LiveOutcome::Accepted {
            code: 0,
            http_status: None
        },
        "the platform may send its code as a string"
    );
    assert!(
        matches!(
            classify_body(Some(200), &json!({"id": "msg-2"})),
            LiveOutcome::Unverified { .. }
        ),
        "a 2xx answer without the acceptance field is not an acceptance"
    );
}

#[test]
fn a_lost_answer_is_never_an_acceptance() {
    let (outcome, answer) = classify_answer(&PlatformAnswer::Unknown {
        detail: "connection reset by peer".to_string(),
    });
    assert_eq!(
        outcome,
        LiveOutcome::Unknown {
            detail: "connection reset by peer".to_string()
        }
    );
    assert!(answer.is_none(), "a lost answer leaves no body to read");

    let (outcome, answer) = classify_answer(&PlatformAnswer::Rejected {
        status: 401,
        code: Some(11244),
        message: "token invalid".to_string(),
        body: json!({"code": 11244}),
    });
    assert_eq!(
        outcome,
        LiveOutcome::Refused {
            message: "token invalid".to_string(),
            code: Some(11244),
            http_status: Some(401)
        }
    );
    assert_eq!(answer, Some(json!({"code": 11244})));
}

#[test]
fn an_acceptance_without_the_platform_answer_is_a_violation() {
    let mut document = all_skipped("not provisioned");
    document.receipts[0].outcome = LiveOutcome::Accepted {
        code: 0,
        http_status: None,
    };
    let violations = bookkeeping_violations(&document, &[]);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("without the platform's answer")),
        "an acceptance is only evidence together with the platform's answer: {violations:?}"
    );
}

#[test]
fn a_typed_acceptance_states_its_own_basis() {
    let mut document = all_skipped("not provisioned");
    document.receipts[0].outcome = LiveOutcome::Accepted {
        code: 0,
        http_status: None,
    };
    document.receipts[0].basis = "client_typed";
    assert_eq!(
        bookkeeping_violations(&document, &[]),
        Vec::<String>::new(),
        "the token client's typed acceptance is evidence of its own kind, recorded as such"
    );
}

#[test]
fn an_inverted_plugin_verdict_is_a_violation() {
    let mut document = all_skipped("not provisioned");
    document.receipts[1].outcome = LiveOutcome::Refused {
        message: "active messages are disabled".to_string(),
        code: Some(40054005),
        http_status: Some(200),
    };
    document.receipts[1].plugin_verdict = Some(PluginVerdict {
        accepted: true,
        retcode: 0,
        status: "ok".to_string(),
        provider_message_id: None,
    });
    let violations = bookkeeping_violations(&document, &[]);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("the plugin reported accepted=true")),
        "a plugin verdict that contradicts the answer is caught: {violations:?}"
    );
}

#[test]
fn a_credential_in_the_document_is_a_violation() {
    let mut document = all_skipped("not provisioned");
    document.receipts[0].request = json!({"echo": "fixture-app-secret"});
    let violations = bookkeeping_violations(&document, &["fixture-app-secret"]);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("carries a credential")),
        "the receipt document must not carry a credential: {violations:?}"
    );
}

#[test]
fn a_complete_skip_document_is_clean() {
    let document = all_skipped("SHENKO_QQ_OFFICIAL_APP_ID is not set");
    assert_eq!(
        bookkeeping_violations(&document, &["fixture-app-secret"]),
        Vec::<String>::new(),
        "skipping without a platform is honest, not a violation"
    );
    assert_eq!(shipped_path_failures(&document), Vec::<String>::new());
}

#[test]
fn a_refusal_on_the_shipped_path_is_an_acceptance_failure() {
    let mut document = all_skipped("not provisioned");
    document.receipts[1].outcome = LiveOutcome::Refused {
        message: "invalid openid".to_string(),
        code: Some(40034002),
        http_status: Some(200),
    };
    let failures = shipped_path_failures(&document);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("40034002"), "{failures:?}");
    assert!(
        failures[0].contains("invalid openid"),
        "the platform's own message is reported: {failures:?}"
    );
}

#[test]
fn the_configuration_debug_never_renders_the_secret() {
    let secret = "fixture-app-secret-0192";
    let config = QqOfficialConfig::new("102000000", secret);
    let rendered = format!("{config:?}");
    assert!(
        !rendered.contains(secret),
        "the app secret is a credential and never appears in a debug line: {rendered}"
    );
    assert!(rendered.contains("102000000"), "the account still does");
}
