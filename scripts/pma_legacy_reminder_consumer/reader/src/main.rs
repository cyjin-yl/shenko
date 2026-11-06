//! The **candidate** reader of one old-producer installation.
//!
//! What this program does
//! ----------------------
//! It reopens the installation the old producer closed, through the candidate
//! library only, and reads it back with the product's public readers:
//!
//! * the agent-scoped drain (`Store::pending_intervention_reminders`),
//! * the prepared-scope path (`Store::pending_intervention_scopes` →
//!   `Store::pending_interventions_for_scope` → the real
//!   `PmaContributionProvider` publication),
//! * the real projection runtime (`MemoryInterventionPlugin` /
//!   `PmaContributionRuntime`) whose published batch a request would collect,
//!   including its `ReminderPublisher::refresh` hook,
//! * the typed omission report (`Store::pending_intervention_omissions`),
//! * the committed decision, its evidence receipt, the operation, the attempt,
//!   the epoch and the lineage — all byte-compared against what the old
//!   producer recorded.
//!
//! What it deliberately never does
//! -------------------------------
//! * it never sends: this crate has no broker, no provider and no HTTP client,
//!   and it never admits a task, so the original bounded request cannot be
//!   resent. It also re-checks that the operation still holds exactly one
//!   attempt and one settlement receipt;
//! * it never writes a durable product fact: every pass asserts that the
//!   recorded facts and the durable files are identical to the previous pass;
//! * it never fabricates an omission: the omission must be explained by the
//!   accepted `pma-decision:<id>` receipt → operation → task chain and by the
//!   epoch's committed owning task, which this program recomputes itself.
//!
//! Honest limits (stated in the receipt, never asserted away)
//! ---------------------------------------------------------
//! * the refresh hook's **failure** path (`commit_seq` invalidation) is not
//!   induced here; only its success path is exercised;
//! * the first candidate open runs the candidate's own additive schema
//!   migration, so the ledger file may gain schema objects once; that delta is
//!   reported, while byte identity is asserted between candidate reopens;
//! * SQLite's own side files (`store.db-shm`, the cognition-import gate
//!   database) are recorded but excluded from the asserted digest;
//! * this is a bounded local mechanism observation, not model efficacy.
//!
//! Current reading (SPEC §51.1–§51.10, decisions 0002/0003, PR180 merge
//! 2cb17172)
//! ---------------------------------------------------------------------
//! The reminder/operation/receipt rows are **mechanism records**; the canonical
//! atoms contract keeps them out of the cognition attitude store. The sealed
//! decision object is immutable, digest addressed **content**, and the accepted
//! settlement receipt is the **reason terminal**. So this reader asserts:
//!
//! * the same content revision is still referenced (not a copy, not a
//!   re-encoding) and the record which names it is byte-identical;
//! * the published own body still carries the provenance references the
//!   decision committed — material travels with its reason;
//! * content from another agent gains no authority by having been read and
//!   committed: it stays readable, is never prepared for the epoch owner, and
//!   is reported as a typed refusal derived from accepted evidence;
//! * a reopen creates no durable product fact, and `agent_id`/branch head
//!   bytes never change (§51.4/.10: a failed switch or reopen is not an
//!   identity change);
//! * no confidence is invented: mechanism records carry none, absent reads as
//!   legacy/unknown, and a `0.0` default would be a defect.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use serde_json::{Map, Value, json};
use shenko_context_policy::{
    ContributionHost, ContributionPayload, ContributionProvider, ContributionRegistry,
    ContributionScope, scope_wide,
};
use shenko_kernel::Supervisor;
use shenko_memory::{
    MemoryInterventionRegistry, ModelMemoryInterventionPolicy, PmaContributionProvider,
    ReminderPublisher,
    intervention_runtime::{
        MemoryInterventionConfig, MemoryInterventionPlugin, PmaContributionRuntime,
    },
};
use shenko_store::{
    InterventionDecisionKind, InterventionOmission, InterventionOmissionKind, InterventionReminder,
    ResearchEpochStatus, Store,
};
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::operation::{AttemptRecord, OperationOutcome, OperationRecord, OperationState};
use tokio_util::sync::CancellationToken;

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn entry<T: serde::Serialize + ?Sized>(value: &T) -> Result<Value> {
    let bytes = serde_json::to_vec(value)?;
    Ok(json!({
        "json": serde_json::from_slice::<Value>(&bytes)?,
        "canonical_len": bytes.len(),
        "canonical_sha256": sha256_hex(&bytes),
    }))
}

fn read_sealed(store: &Store, object: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "sealed object closure mismatch for {}",
        object.digest
    );
    Ok(bytes)
}

struct Args {
    installation: PathBuf,
    before_receipt: PathBuf,
    out: PathBuf,
    passes: usize,
    deadline_secs: u64,
}

fn parse_args() -> Result<Args> {
    let mut installation = None;
    let mut before_receipt = None;
    let mut out = None;
    let mut passes = 2usize;
    let mut deadline_secs = 60u64;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--installation" => {
                installation = Some(PathBuf::from(
                    args.next().context("--installation needs a directory")?,
                ))
            }
            "--before-receipt" => {
                before_receipt = Some(PathBuf::from(
                    args.next().context("--before-receipt needs a file")?,
                ))
            }
            "--out" => out = Some(PathBuf::from(args.next().context("--out needs a file")?)),
            "--passes" => {
                passes = args
                    .next()
                    .context("--passes needs a number")?
                    .parse()
                    .context("--passes is not a number")?
            }
            "--deadline-secs" => {
                deadline_secs = args
                    .next()
                    .context("--deadline-secs needs a number")?
                    .parse()
                    .context("--deadline-secs is not a number")?
            }
            "--help" | "-h" => {
                println!(
                    "pma-legacy-reminder-reader --installation <dir> --before-receipt <json> \
                     --out <json> [--passes 2] [--deadline-secs 60]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(Args {
        installation: installation.context("missing --installation")?,
        before_receipt: before_receipt.context("missing --before-receipt")?,
        out: out.context("missing --out")?,
        passes: passes.max(2),
        deadline_secs,
    })
}

/// The identities and recorded bytes the old producer handed over.
struct Before {
    raw: Value,
    owner: EntityId,
    author: EntityId,
    foreign_epoch: EntityId,
    foreign_lineage: EntityId,
    own_epoch: EntityId,
    author_task: EntityId,
    owner_task: EntityId,
    epoch_owner_task: EntityId,
    foreign_reminder: EntityId,
    own_reminder: EntityId,
    foreign_operation: EntityId,
    foreign_attempt: EntityId,
    foreign_receipt: EntityId,
    foreign_content: String,
    own_content: String,
    /// The provenance references the committed own decision recorded. The
    /// published body must still carry them (content plus its reason).
    own_provenance: Vec<String>,
}

fn id_at(value: &Value, path: &str) -> Result<EntityId> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .with_context(|| format!("the old receipt has no id at {path}"))?
        .parse()
        .with_context(|| format!("{path} is not an entity id"))
}

fn str_at<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .with_context(|| format!("the old receipt has no string at {path}"))
}

fn load_before(path: &Path) -> Result<Before> {
    let raw: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        raw["phase"].as_str() == Some("before"),
        "{} is not an old-producer receipt",
        path.display()
    );
    ensure!(
        raw["old_path"]["vulnerable"].as_bool() == Some(true),
        "the old producer did not establish the vulnerable preparation path; \
         there is nothing for the candidate to omit"
    );
    Ok(Before {
        owner: id_at(&raw, "/agents/owner")?,
        author: id_at(&raw, "/agents/author")?,
        foreign_epoch: id_at(&raw, "/epochs/foreign/json/epoch_id")?,
        foreign_lineage: id_at(&raw, "/epochs/foreign/json/lineage_id")?,
        own_epoch: id_at(&raw, "/epochs/own/json/epoch_id")?,
        author_task: id_at(&raw, "/tasks/author_task/json/task_id")?,
        owner_task: id_at(&raw, "/tasks/owner_task/json/task_id")?,
        epoch_owner_task: id_at(&raw, "/epochs/epoch_owner_task")?,
        foreign_reminder: id_at(&raw, "/reminders/foreign/json/reminder_id")?,
        own_reminder: id_at(&raw, "/reminders/own/json/reminder_id")?,
        foreign_operation: id_at(&raw, "/calls/foreign/operation/json/operation_id")?,
        foreign_attempt: id_at(&raw, "/calls/foreign/attempt/json/attempt_id")?,
        foreign_receipt: id_at(&raw, "/calls/foreign/receipt/json/receipt_id")?,
        foreign_content: str_at(&raw, "/reminders/foreign/json/content")?.to_string(),
        own_content: str_at(&raw, "/reminders/own/json/content")?.to_string(),
        own_provenance: raw
            .pointer("/reminders/own/json/provenance")
            .and_then(Value::as_array)
            .context("the old receipt records no own-decision provenance")?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        raw,
    })
}

struct Checks {
    prefix: String,
    items: Vec<Value>,
}

impl Checks {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            items: Vec::new(),
        }
    }

    fn check(&mut self, name: &str, ok: bool, detail: Value) -> bool {
        self.items.push(json!({
            "name": format!("{}{name}", self.prefix),
            "ok": ok,
            "detail": detail,
        }));
        ok
    }

    fn merge(&mut self, other: Checks) {
        self.items.extend(other.items);
    }

    fn all_ok(&self) -> bool {
        self.items
            .iter()
            .all(|item| item["ok"].as_bool() == Some(true))
    }

    fn failures(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| item["ok"].as_bool() != Some(true))
            .map(|item| item["name"].as_str().unwrap_or("<unnamed>").to_string())
            .collect()
    }

    fn value(&self) -> Value {
        json!({"items": self.items, "all_ok": self.all_ok(), "failures": self.failures()})
    }
}

fn byte_equal(checks: &mut Checks, name: &str, observed: &Value, expected: &Value) -> bool {
    let observed_sha = observed["canonical_sha256"].as_str().unwrap_or_default();
    let expected_sha = expected["canonical_sha256"].as_str().unwrap_or_default();
    let ok = !observed_sha.is_empty() && observed_sha == expected_sha;
    checks.check(
        name,
        ok,
        json!({
            "observed_sha256": observed_sha,
            "expected_sha256": expected_sha,
            "observed_len": observed["canonical_len"],
            "expected_len": expected["canonical_len"],
        }),
    )
}

fn scope_json(scope: &ContributionScope) -> Result<Value> {
    Ok(serde_json::to_value(scope)?)
}

/// Everything one read-only reopen observes. The same function runs on every
/// pass and on the runtime pass, so "unchanged" is compared between opens of
/// the same closed installation.
fn observe(store: &Arc<Store>, before: &Before, checks: &mut Checks) -> Result<Value> {
    let now = chrono::Utc::now().timestamp_millis();
    let (head, _) = store.load_subject(before.owner)?;
    let foreign_epoch = store
        .research_epoch(before.foreign_epoch)?
        .context("the foreign epoch is not readable")?;
    let own_epoch = store
        .research_epoch(before.own_epoch)?
        .context("the own epoch is not readable")?;
    let lineage = store.research_lineage(before.foreign_lineage)?;
    let author_task = store.external_task(before.author_task)?;
    let owner_task = store.external_task(before.owner_task)?;
    let epoch_owner_task = store.external_task(before.epoch_owner_task)?;

    // The original bounded call: exactly one operation, one attempt, one receipt.
    let operations = store.external_task_operations(before.author_task, "pma.evaluate.v1")?;
    let operation: &OperationRecord = operations
        .iter()
        .find(|operation| operation.operation_id == before.foreign_operation)
        .context("the old authoring operation is not readable")?;
    let attempts = store.external_operation_attempts(before.foreign_operation)?;
    let attempt: &AttemptRecord = attempts
        .iter()
        .find(|attempt| attempt.attempt_id == before.foreign_attempt)
        .context("the old armed attempt is not readable")?;
    let receipts = store.external_operation_evidence(before.foreign_operation)?;
    let receipt: &EvidenceReceipt = receipts
        .iter()
        .find(|receipt| receipt.receipt_id == before.foreign_receipt)
        .context("the old settlement receipt is not readable")?;
    let foreign_reminder = store
        .intervention_reminder(before.foreign_reminder)?
        .context("the committed foreign reminder is not readable")?;
    let own_reminder = store
        .intervention_reminder(before.own_reminder)?
        .context("the committed own reminder is not readable")?;
    let latest_foreign = store.latest_intervention(before.foreign_epoch)?;
    let latest_own = store.latest_intervention(before.own_epoch)?;

    checks.check(
        "original_call_not_resent",
        operations.len() == 1
            && attempts.len() == 1
            && receipts.len() == 1
            && operation.state == OperationState::Resolved
            && operation.outcome == Some(OperationOutcome::Succeeded)
            && attempt.final_receipt_id == Some(before.foreign_receipt),
        json!({
            "operations": operations.len(),
            "attempts": attempts.len(),
            "receipts": receipts.len(),
            "operation_state": format!("{:?}", operation.state),
            "operation_outcome": operation.outcome.map(|outcome| format!("{outcome:?}")),
            "attempt_receipt": attempt.final_receipt_id.map(|id| id.to_string()),
        }),
    );

    // The accepted evidence still says, in its own bytes, which decision it is.
    let normalized_ref = receipt
        .normalized_ref
        .as_ref()
        .context("the settlement receipt carries no normalized decision")?;
    let normalized_bytes = read_sealed(store, normalized_ref)?;
    let normalized: Value = serde_json::from_slice(&normalized_bytes)?;
    let decision_ref: ObjectRef = serde_json::from_value(normalized["decision_ref"].clone())
        .context("the normalized decision names no decision object")?;
    let decision_bytes = read_sealed(store, &decision_ref)?;
    let sealed: InterventionReminder = serde_json::from_slice(&decision_bytes)?;
    let normalized_sha256 = sha256_hex(&normalized_bytes);
    let decision_sha256 = sha256_hex(&decision_bytes);
    checks.check(
        "accepted_evidence_names_the_decision",
        matches!(receipt.decode_status, DecodeStatus::Decoded)
            && receipt.source_event_key.as_deref()
                == Some(format!("pma-decision:{}", before.foreign_reminder).as_str())
            && receipt.operation_id == Some(before.foreign_operation)
            && sealed == foreign_reminder
            && normalized_sha256 == str_at(&before.raw, "/calls/foreign/normalized_sha256")?
            && decision_sha256 == str_at(&before.raw, "/calls/foreign/decision_sha256")?,
        json!({
            "decode_status": format!("{:?}", receipt.decode_status),
            "source_event_key": receipt.source_event_key,
            "operation_id": receipt.operation_id.map(|id| id.to_string()),
            "sealed_decision_equals_row": sealed == foreign_reminder,
            "normalized_sha256": normalized_sha256,
            "decision_sha256": decision_sha256,
        }),
    );

    // The typed omission identity, recomputed here from the accepted chain.
    let evidence_author = author_task.agent_id;
    let epoch_owner = epoch_owner_task.agent_id;
    let foreign_id = before.foreign_reminder.to_string();
    let own_id = before.own_reminder.to_string();
    // SPEC §51.1 read through the canonical atoms contract: the committed
    // reminder/operation/receipt rows stay MECHANISM records (they are never
    // migrated into the cognition attitude store), while the sealed decision
    // object is immutable, digest addressed content and the accepted
    // settlement receipt is the reason terminal. A reopen may re-read them and
    // must reach the very same content revision, not a copy and not a
    // re-encoded reading.
    let recorded_decision_digest = before
        .raw
        .pointer("/calls/foreign/decision_object/digest")
        .and_then(Value::as_str)
        .context("the old receipt names no decision content digest")?;
    checks.check(
        "decision_content_revision_unchanged",
        decision_ref.digest.0 == recorded_decision_digest
            && normalized["reminder_id"].as_str() == Some(foreign_id.as_str())
            && normalized["decision"].as_str() == Some("remind")
            && normalized["epoch_id"].as_str() == Some(before.foreign_epoch.to_string().as_str())
            && normalized["task_id"].as_str() == Some(before.author_task.to_string().as_str()),
        json!({
            "content_digest": decision_ref.digest,
            "recorded_content_digest": recorded_decision_digest,
            "reason_terminal_receipt": {"source_event_key": receipt.source_event_key,
                "raw_ref": receipt.raw_ref,
                "normalized_ref": receipt.normalized_ref},
            "normalized_reminder": normalized["reminder_id"],
            "normalized_decision": normalized["decision"],
        }),
    );
    checks.check(
        "evidence_chain_matches_recorded_agents",
        evidence_author == before.author
            && epoch_owner == before.owner
            && operation.task_id == before.author_task
            && foreign_epoch.task_id == Some(before.epoch_owner_task)
            && operation.operation_name == "pma.evaluate.v1",
        json!({
            "author_from_receipt_operation_task": evidence_author.to_string(),
            "epoch_owner_from_epoch_task": epoch_owner.to_string(),
            "operation_task": operation.task_id.to_string(),
            "epoch_task": foreign_epoch.task_id.map(|id| id.to_string()),
        }),
    );

    // Path 1a: the agent-scoped drain.
    let drain_owner: Vec<InterventionReminder> =
        store.pending_intervention_reminders(before.owner, now, 64)?;
    let drain_author: Vec<InterventionReminder> =
        store.pending_intervention_reminders(before.author, now, 64)?;
    let drain_owner_ids: Vec<String> = drain_owner
        .iter()
        .map(|reminder| reminder.reminder_id.to_string())
        .collect();
    let drain_author_ids: Vec<String> = drain_author
        .iter()
        .map(|reminder| reminder.reminder_id.to_string())
        .collect();
    checks.check(
        "agent_drain_omits_foreign_reminder",
        !drain_owner_ids.contains(&foreign_id) && drain_owner_ids.contains(&own_id),
        json!({"ids": drain_owner_ids, "foreign": foreign_id, "own": own_id}),
    );
    checks.check(
        "other_agent_drain_holds_neither",
        !drain_author_ids.contains(&foreign_id) && !drain_author_ids.contains(&own_id),
        json!({"ids": drain_author_ids}),
    );

    // Path 1b: the prepared scopes and the real publication provider.
    let scopes = store.pending_intervention_scopes(before.owner, now)?;
    let mut scope_readers = Vec::new();
    let mut scope_ids: Vec<String> = Vec::new();
    let mut pending_scopes = Vec::new();
    for scope in &scopes {
        let ids: Vec<String> = store
            .pending_interventions_for_scope(scope, now)?
            .into_iter()
            .map(|reminder| reminder.reminder_id.to_string())
            .collect();
        scope_ids.extend(ids.iter().cloned());
        pending_scopes.push(scope_json(scope)?);
        scope_readers.push(json!({"scope": scope_json(scope)?, "reminder_ids": ids}));
    }
    let foreign_scope = store
        .intervention_delivery_scope(&foreign_reminder)?
        .context("the committed foreign decision has no delivery scope")?;
    let own_scope = store
        .intervention_delivery_scope(&own_reminder)?
        .context("the committed own decision has no delivery scope")?;
    checks.check(
        "scoped_preparation_omits_foreign_reminder",
        !scope_ids.contains(&foreign_id) && scope_ids.contains(&own_id) && !scopes.is_empty(),
        json!({"pending_scopes": pending_scopes, "reminder_ids": scope_ids}),
    );
    checks.check(
        "both_decisions_resolve_to_one_owner_scope",
        foreign_scope == own_scope,
        json!({"foreign_scope": scope_json(&foreign_scope)?, "own_scope": scope_json(&own_scope)?}),
    );

    // Publication itself seals reminder bodies, so it belongs to the runtime
    // pass (a reopen must not write). The read-only prepared paths asserted
    // here are the ones a *reader* uses: the scope list and the per-scope
    // prepared material.

    // Path 2: the typed omission report.
    let omissions: Vec<InterventionOmission> =
        store.pending_intervention_omissions(before.owner, now, 64)?;
    let omissions_author: Vec<InterventionOmission> =
        store.pending_intervention_omissions(before.author, now, 64)?;
    let omission = omissions.first();
    checks.check(
        "typed_omission_matches_evidence_chain",
        omissions.len() == 1
            && omission.map(|omission| omission.reminder_id) == Some(before.foreign_reminder)
            && omission.map(|omission| omission.epoch_id) == Some(before.foreign_epoch)
            && omission.map(|omission| omission.kind)
                == Some(InterventionOmissionKind::ForeignAuthor)
            && omission.and_then(|omission| omission.author_agent) == Some(evidence_author)
            && omission.and_then(|omission| omission.epoch_agent) == Some(epoch_owner)
            && omission.map(|omission| omission.address.as_str())
                == Some(foreign_reminder.scope.as_str())
            && omission.is_some_and(|omission| {
                omission.reason.contains(&before.author.to_string())
                    && omission.reason.contains(&before.owner.to_string())
            }),
        json!({
            "omissions": serde_json::to_value(&omissions)?,
            "expected_author": before.author.to_string(),
            "expected_owner": before.owner.to_string(),
        }),
    );
    checks.check(
        "omission_is_the_only_refusal",
        omissions_author.is_empty()
            && omissions
                .iter()
                .all(|omission| omission.reminder_id != before.own_reminder),
        json!({"for_author": serde_json::to_value(&omissions_author)?}),
    );
    checks.check(
        "own_reminder_still_a_reminder",
        own_reminder.decision == InterventionDecisionKind::Remind
            && own_reminder.content.as_deref() == Some(before.own_content.as_str()),
        json!({"own_reminder_decision": format!("{:?}", own_reminder.decision)}),
    );

    // Byte-identical committed facts, compared against the old producer.
    let mut facts = Map::new();
    facts.insert("head".into(), entry(&head)?);
    facts.insert("foreign_epoch".into(), entry(&foreign_epoch)?);
    facts.insert("own_epoch".into(), entry(&own_epoch)?);
    facts.insert("lineage".into(), entry(&lineage)?);
    facts.insert("author_task".into(), entry(&author_task)?);
    facts.insert("owner_task".into(), entry(&owner_task)?);
    facts.insert("epoch_owner_task".into(), entry(&epoch_owner_task)?);
    facts.insert("foreign_operation".into(), entry(operation)?);
    facts.insert("foreign_attempt".into(), entry(attempt)?);
    facts.insert("foreign_receipt".into(), entry(receipt)?);
    facts.insert("foreign_reminder".into(), entry(&foreign_reminder)?);
    facts.insert("own_reminder".into(), entry(&own_reminder)?);
    facts.insert("latest_foreign".into(), entry(&latest_foreign)?);
    facts.insert("latest_own".into(), entry(&latest_own)?);
    facts.insert("normalized_sha256".into(), json!(normalized_sha256));
    facts.insert("decision_sha256".into(), json!(decision_sha256));
    facts.insert("attempt_count".into(), json!(attempts.len()));
    facts.insert("receipt_count".into(), json!(receipts.len()));
    facts.insert("commit_seq".into(), json!(store.current_commit_seq()?));
    facts.insert(
        "omissions_sha256".into(),
        json!(sha256_hex(&serde_json::to_vec(&omissions)?)),
    );

    checks.check(
        "latest_decision_unchanged",
        latest_foreign.as_ref().map(|reminder| reminder.reminder_id)
            == Some(before.foreign_reminder)
            && latest_own.as_ref().map(|reminder| reminder.reminder_id) == Some(before.own_reminder)
            && foreign_epoch.status == ResearchEpochStatus::Active
            && lineage.len() == 1
            && lineage[0].epoch_id == before.foreign_epoch,
        json!({
            "latest_foreign": latest_foreign.map(|reminder| reminder.reminder_id.to_string()),
            "latest_own": latest_own.map(|reminder| reminder.reminder_id.to_string()),
            "epoch_status": format!("{:?}", foreign_epoch.status),
            "lineage_len": lineage.len(),
        }),
    );

    Ok(json!({
        "facts": Value::Object(facts),
        "prepared_paths": {
            "agent_drain_ids": drain_owner_ids,
            "other_agent_drain_ids": drain_author_ids,
            "pending_scopes": pending_scopes,
            "scope_reminder_ids": scope_readers,
        },
        "omissions": serde_json::to_value(&omissions)?,
    }))
}

/// Compare one pass's observed facts against the old producer's recorded bytes.
fn compare_with_before(observed: &Value, before: &Before, checks: &mut Checks) -> Result<()> {
    let facts = &observed["facts"];
    let pairs = [
        (
            "head_unchanged",
            "head",
            "/heads/owner",
        ),
        ("foreign_epoch_unchanged", "foreign_epoch", "/epochs/foreign"),
        ("own_epoch_unchanged", "own_epoch", "/epochs/own"),
        ("lineage_unchanged", "lineage", "/epochs/foreign_lineage"),
        ("author_task_unchanged", "author_task", "/tasks/author_task"),
        ("owner_task_unchanged", "owner_task", "/tasks/owner_task"),
        (
            "epoch_owner_task_unchanged",
            "epoch_owner_task",
            "/tasks/epoch_owner_task",
        ),
        (
            "operation_unchanged",
            "foreign_operation",
            "/calls/foreign/operation",
        ),
        (
            "attempt_unchanged",
            "foreign_attempt",
            "/calls/foreign/attempt",
        ),
        (
            "receipt_unchanged",
            "foreign_receipt",
            "/calls/foreign/receipt",
        ),
        (
            "foreign_reminder_unchanged",
            "foreign_reminder",
            "/reminders/foreign",
        ),
        ("own_reminder_unchanged", "own_reminder", "/reminders/own"),
    ];
    for (name, observed_key, before_path) in pairs {
        let expected = before
            .raw
            .pointer(before_path)
            .with_context(|| format!("the old receipt has no {before_path}"))?;
        byte_equal(checks, name, &facts[observed_key], expected);
    }
    checks.check(
        "decision_bytes_unchanged",
        facts["normalized_sha256"].as_str()
            == before
                .raw
                .pointer("/calls/foreign/normalized_sha256")
                .and_then(Value::as_str)
            && facts["decision_sha256"].as_str()
                == before
                    .raw
                    .pointer("/calls/foreign/decision_sha256")
                    .and_then(Value::as_str),
        json!({
            "normalized_sha256": facts["normalized_sha256"],
            "decision_sha256": facts["decision_sha256"],
        }),
    );
    Ok(())
}

/// The real projection runtime: what a request would actually collect, plus the
/// candidate's refresh hook. It performs no send: the plugin only reads durable
/// decisions and publishes prepared batches.
async fn runtime_pass(
    store: Arc<Store>,
    before: &Before,
    checks: &mut Checks,
    deadline_secs: u64,
) -> Result<Value> {
    let (head, _) = store.load_subject(before.owner)?;
    let scope = scope_wide(EntityId(head.branch_id.0));
    let supervisor = Supervisor::new(head.software_generation.clone());
    let registry = Arc::new(ContributionRegistry::new());
    supervisor
        .spawn(ContributionHost, registry.clone())
        .await?;
    let failure_cancel = CancellationToken::new();
    let policy = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        ModelMemoryInterventionPolicy,
    )));
    let _projection = supervisor
        .spawn(
            MemoryInterventionPlugin,
            MemoryInterventionConfig {
                store: store.clone(),
                agent_id: before.owner,
                policy,
                failure_cancel: failure_cancel.clone(),
            },
        )
        .await?;
    let runtime = supervisor
        .context()
        .try_service::<PmaContributionRuntime>()?;
    let foreign_key = format!("pma.reminder.{}", before.foreign_reminder);
    let own_key = format!("pma.reminder.{}", before.own_reminder);

    // The publication provider itself: exactly the code the runtime publishes
    // through, driven here for both resolved scopes. It seals reminder bodies,
    // so it is exercised only in this runtime pass.
    let provider = PmaContributionProvider::new(store.clone());
    let mut prepared_scopes = Vec::new();
    let mut prepared_keys: BTreeSet<String> = BTreeSet::new();
    let mut foreign_body_published = false;
    let mut own_body_published = false;
    let mut own_reason_recorded = false;
    for delivery in [&scope, &scope] {
        let items = ContributionProvider::prepare(&provider, delivery)
            .map_err(|error| anyhow::anyhow!("the publication provider failed: {error}"))?;
        let mut rendered = Vec::new();
        for item in &items {
            let bytes = read_sealed(store, &item.data)?;
            let text = ContributionPayload::read(&bytes)
                .map_err(|error| anyhow::anyhow!("prepared body is unreadable: {error}"))?;
            if text == before.foreign_content {
                foreign_body_published = true;
            }
            if text == before.own_content {
                own_body_published = true;
            }
            if item.key == own_key {
                // SPEC §51.1: the prepared material is content plus its reason.
                // The own reminder's body still carries every provenance
                // reference the decision committed, and names its own
                // decision.
                let body: Value = serde_json::from_slice(&bytes)?;
                own_reason_recorded = body["reminder_id"].as_str()
                    == Some(before.own_reminder.to_string().as_str())
                    && !before.own_provenance.is_empty()
                    && body["provenance"].as_array().is_some_and(|items| {
                        before.own_provenance.iter().all(|recorded| {
                            items.iter().any(|item| item.as_str() == Some(recorded.as_str()))
                        })
                    });
            }
            prepared_keys.insert(item.key.clone());
            rendered.push(json!({
                "key": item.key,
                "tokens": item.tokens,
                "single_use": item.single_use(),
                "body_sha256": sha256_hex(text.as_bytes()),
            }));
        }
        prepared_scopes.push(json!({
            "scope": scope_json(delivery)?,
            "contributions": rendered,
        }));
    }
    let prepared_keys: Vec<String> = prepared_keys.into_iter().collect();
    checks.check(
        "provider_batch_omits_foreign_body",
        !foreign_body_published
            && own_body_published
            && !prepared_keys
                .iter()
                .any(|key| key == &format!("pma.reminder.{}", before.foreign_reminder))
            && prepared_keys.iter().any(|key| key == &own_key),
        json!({
            "keys": prepared_keys,
            "foreign_body_published": foreign_body_published,
            "own_body_published": own_body_published,
            "scopes": prepared_scopes,
        }),
    );
    checks.check(
        "own_publication_keeps_its_reason_pointer",
        own_reason_recorded,
        json!({
            "recorded_provenance": before.own_provenance,
            "reminder": before.own_reminder.to_string(),
        }),
    );
    let collect = |registry: &ContributionRegistry| -> Vec<String> {
        let mut keys: Vec<String> = registry
            .collect(&scope, chrono::Utc::now().timestamp_millis())
            .contributions
            .into_iter()
            .map(|item| item.key)
            .collect();
        keys.sort();
        keys
    };
    let mut batch_before = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(deadline_secs) {
        batch_before = collect(&registry);
        if batch_before.contains(&own_key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    checks.check(
        "runtime_batch_publishes_only_the_owned_reminder",
        batch_before.contains(&own_key) && !batch_before.contains(&foreign_key),
        json!({"keys": batch_before, "foreign": foreign_key, "own": own_key}),
    );
    let refresh = runtime.refresh(&scope);
    let refresh_ok = refresh.is_ok();
    let refresh_error = refresh.err().map(|error| error.to_string());
    let batch_after = collect(&registry);
    checks.check(
        "refresh_is_idempotent_and_still_omits_the_foreign_reminder",
        refresh_ok && batch_after == batch_before && !batch_after.contains(&foreign_key),
        json!({"refresh_error": refresh_error, "before": batch_before, "after": batch_after}),
    );
    let published_texts: Vec<String> = registry
        .collect(&scope, chrono::Utc::now().timestamp_millis())
        .contributions
        .iter()
        .filter_map(|item| {
            store
                .cas
                .read_object(&item.data.digest)
                .ok()
                .and_then(|bytes| ContributionPayload::read(&bytes).ok())
        })
        .collect();
    checks.check(
        "runtime_batch_carries_no_foreign_body",
        !published_texts
            .iter()
            .any(|text| text == &before.foreign_content)
            && published_texts.iter().any(|text| text == &before.own_content),
        json!({
            "body_sha256": published_texts
                .iter()
                .map(|text| sha256_hex(text.as_bytes()))
                .collect::<Vec<_>>(),
        }),
    );

    tokio::time::timeout(Duration::from_secs(deadline_secs), supervisor.stop())
        .await
        .context("the candidate supervisor did not stop in time")??;
    drop(runtime);
    drop(supervisor);
    checks.check(
        "runtime_teardown_is_not_a_failure",
        !failure_cancel.is_cancelled(),
        json!({"failure_cancel": failure_cancel.is_cancelled()}),
    );
    Ok(json!({
        "batch_before_refresh": batch_before,
        "batch_after_refresh": batch_after,
        "refresh_ok": refresh_ok,
        "refresh_error": refresh_error,
        "foreign_body_published": published_texts.iter().any(|text| text == &before.foreign_content),
        "own_body_published": published_texts.iter().any(|text| text == &before.own_content),
    }))
}

/// Whole-installation file inventory. `durable` is the asserted set; SQLite's
/// own side files are recorded separately and never asserted.
fn inventory(root: &Path) -> Result<Value> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_dir() {
                walk(root, &path, files)?;
            } else if meta.file_type().is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked path is under the root")
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, sha256_hex(&std::fs::read(&path)?));
            } else {
                anyhow::bail!(
                    "unexpected non-file entry in the installation: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    let mut durable = String::new();
    let mut durable_files = Map::new();
    let mut auxiliary = Map::new();
    for (relative, digest) in &files {
        let product_owned = relative.as_str() == "store.db"
            || relative.starts_with("store.db-wal")
            || relative.starts_with("cas/");
        if product_owned {
            durable.push_str(relative);
            durable.push('\0');
            durable.push_str(digest);
            durable.push('\n');
            durable_files.insert(relative.clone(), json!(digest));
        } else {
            auxiliary.insert(relative.clone(), json!(digest));
        }
    }
    Ok(json!({
        "root": root.display().to_string(),
        "durable_sha256": sha256_hex(durable.as_bytes()),
        "durable_files": Value::Object(durable_files),
        "auxiliary_files": Value::Object(auxiliary),
        "wal_present": files.contains_key("store.db-wal"),
    }))
}

/// The committed-fact projection with the ledger's own sequence removed.
///
/// `commit_seq` counts every committed mutation, so an appended journal row
/// moves it. It is asserted separately as monotonic; it is not evidence that a
/// committed attitude was rewritten.
fn without_commit_seq(facts: &Value) -> Value {
    let mut facts = facts.clone();
    if let Some(object) = facts.as_object_mut() {
        object.remove("commit_seq");
    }
    facts
}

/// Files under `cas/` that the old producer recorded and that later passes must
/// still hold byte-identically. Content-addressed objects are immutable, so a
/// disappearance or a changed body is a real finding; a *new* object is a
/// reported addition, not a rewrite.
fn cas_delta(before_files: &Value, current: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let empty = Map::new();
    let before_map = before_files.as_object().unwrap_or(&empty);
    let current_map = current.as_object().unwrap_or(&empty);
    for (name, digest) in before_map {
        match current_map.get(name) {
            None => removed.push(name.clone()),
            Some(current_digest) if current_digest != digest => changed.push(name.clone()),
            Some(_) => {}
        }
    }
    for name in current_map.keys() {
        if !before_map.contains_key(name) {
            added.push(name.clone());
        }
    }
    (removed, changed, added)
}

async fn run() -> Result<i32> {
    let args = parse_args()?;
    let before = load_before(&args.before_receipt)?;
    let db_path = args.installation.join("store.db");
    let cas_path = args.installation.join("cas");
    ensure!(
        db_path.exists(),
        "--installation does not hold a ledger: {}",
        db_path.display()
    );
    let before_files = &before.raw["files"];

    let mut checks = Checks::new("all/");
    let mut passes: Vec<Value> = Vec::new();
    for index in 0..args.passes {
        let store = Arc::new(Store::open(db_path.clone(), cas_path.clone())?);
        let mut pass_checks = Checks::new(&format!("pass{index}/"));
        let observed = observe(&store, &before, &mut pass_checks)?;
        compare_with_before(&observed, &before, &mut pass_checks)?;
        drop(store);
        let files = inventory(&args.installation)?;
        checks.merge(pass_checks);
        passes.push(json!({"index": index, "observed": observed, "files": files}));
    }

    // Every reopen must agree with every other reopen.
    let first = passes[0].clone();
    for (index, pass) in passes.iter().enumerate().skip(1) {
        checks.check(
            &format!("reopen_{index}_facts_identical"),
            pass["observed"]["facts"] == first["observed"]["facts"],
            json!({"facts": pass["observed"]["facts"]}),
        );
        checks.check(
            &format!("reopen_{index}_durable_bytes_identical"),
            pass["files"]["durable_sha256"] == first["files"]["durable_sha256"],
            json!({
                "observed": pass["files"]["durable_sha256"],
                "previous": first["files"]["durable_sha256"],
            }),
        );
        checks.check(
            &format!("reopen_{index}_no_new_omission_facts"),
            pass["observed"]["omissions"] == first["observed"]["omissions"]
                && pass["observed"]["facts"]["omissions_sha256"]
                    == first["observed"]["facts"]["omissions_sha256"],
            json!({
                "omissions": pass["observed"]["omissions"],
                "sha256": pass["observed"]["facts"]["omissions_sha256"],
            }),
        );
    }

    // The content store is append-only: nothing the old producer sealed may
    // disappear or change. Additions are reported, never called a rewrite.
    let before_cas: Map<String, Value> = before_files["durable_files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter(|(name, _)| name.starts_with("cas/"))
                .map(|(name, digest)| (name.clone(), digest.clone()))
                .collect()
        })
        .unwrap_or_default();
    let first_cas: Map<String, Value> = first["files"]["durable_files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter(|(name, _)| name.starts_with("cas/"))
                .map(|(name, digest)| (name.clone(), digest.clone()))
                .collect()
        })
        .unwrap_or_default();
    let (removed, changed, added) =
        cas_delta(&Value::Object(before_cas), &Value::Object(first_cas.clone()));
    checks.check(
        "content_store_objects_unchanged_since_the_old_producer",
        removed.is_empty() && changed.is_empty(),
        json!({"removed": removed, "changed": changed, "added": added}),
    );
    let ledger_changed_at_first_open = before_files["durable_files"]["store.db"]
        != first["files"]["durable_files"]["store.db"];

    // The real runtime publication and the candidate's refresh hook.
    let store = Arc::new(Store::open(db_path.clone(), cas_path.clone())?);
    let mut runtime_checks = Checks::new("runtime/");
    let runtime =
        runtime_pass(store.clone(), &before, &mut runtime_checks, args.deadline_secs).await?;
    let runtime_observed = observe(&store, &before, &mut runtime_checks)?;
    compare_with_before(&runtime_observed, &before, &mut runtime_checks)?;
    drop(store);
    let runtime_files = inventory(&args.installation)?;
    checks.merge(runtime_checks);

    // A *running* candidate projection may append its own refusal/omission
    // journal facts (the PMA owner declares `runtime_error` rows of kind
    // `pma_omission`, written from the background pass only after
    // `pending_intervention_omissions` returned a record). Those are
    // append-only observations, so the runtime pass asserts append-only
    // behaviour instead of demanding zero writes:
    //   * no committed attitude/content/history row is rewritten,
    //   * the content store stays immutable,
    //   * the commit sequence only moves forward,
    //   * the omission projection is still exactly one record (no duplicated
    //     refusal fact).
    // A reopen without a runtime must still change no bytes at all: that is
    // asserted above and is the strict repeated-reopen requirement.
    let runtime_cas: Map<String, Value> = runtime_files["durable_files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter(|(name, _)| name.starts_with("cas/"))
                .map(|(name, digest)| (name.clone(), digest.clone()))
                .collect()
        })
        .unwrap_or_default();
    let (runtime_removed, runtime_changed, runtime_added) =
        cas_delta(&Value::Object(first_cas.clone()), &Value::Object(runtime_cas));
    checks.check(
        "runtime_pass_rewrote_no_committed_row",
        without_commit_seq(&runtime_observed["facts"])
            == without_commit_seq(&first["observed"]["facts"]),
        json!({"facts_except_commit_seq": without_commit_seq(&runtime_observed["facts"])}),
    );
    checks.check(
        "runtime_pass_kept_content_store_immutable",
        runtime_removed.is_empty() && runtime_changed.is_empty(),
        json!({
            "removed": runtime_removed,
            "changed": runtime_changed,
            "added": runtime_added,
        }),
    );
    let seq_before = first["observed"]["facts"]["commit_seq"]
        .as_u64()
        .context("the first pass reported no commit sequence")?;
    let seq_after = runtime_observed["facts"]["commit_seq"]
        .as_u64()
        .context("the runtime pass reported no commit sequence")?;
    checks.check(
        "runtime_pass_commit_seq_not_rewritten",
        seq_after >= seq_before,
        json!({"before": seq_before, "after": seq_after}),
    );
    checks.check(
        "runtime_pass_projects_one_refusal_fact",
        runtime_observed["omissions"].as_array().map(|items| items.len()) == Some(1)
            && runtime_observed["omissions"] == first["observed"]["omissions"],
        json!({"omissions": runtime_observed["omissions"]}),
    );

    let mut receipt = Map::new();
    receipt.insert("schema_version".into(), json!(1));
    receipt.insert("phase".into(), json!("after"));
    receipt.insert("reader".into(), json!("shenko-pma-legacy-reminder-reader"));
    receipt.insert(
        "before_receipt".into(),
        json!({
            "path": args.before_receipt.display().to_string(),
            "sha256": sha256_hex(&std::fs::read(&args.before_receipt)?),
            "old_producer_vulnerable": before.raw["old_path"]["vulnerable"],
            "old_drain_ids": before.raw["old_path"]["owner_agent_drain_ids"],
            "old_published_keys": before.raw["old_path"]["published_batch_keys"],
        }),
    );
    receipt.insert(
        "installation".into(),
        json!({
            "root": args.installation.display().to_string(),
            "db": db_path.display().to_string(),
            "cas": cas_path.display().to_string(),
        }),
    );
    receipt.insert("passes".into(), Value::Array(passes));
    receipt.insert("runtime".into(), runtime);
    receipt.insert(
        "runtime_durable_delta".into(),
        json!({
            "ledger_bytes_changed": runtime_files["durable_files"]["store.db"]
                != first["files"]["durable_files"]["store.db"],
            "wal_present": runtime_files["wal_present"],
            "commit_seq_before": seq_before,
            "commit_seq_after": seq_after,
            "cas_added": runtime_added,
            "explanation": "a running candidate projection may append its own omission-journal \
                facts (kind pma_omission); the checks require that nothing committed is \
                rewritten, that the content store stays immutable and that the commit sequence \
                only moves forward",
        }),
    );
    receipt.insert("runtime_files".into(), runtime_files);
    receipt.insert("checks".into(), checks.value());
    receipt.insert(
        "contract".into(),
        json!({
            "reading": "SPEC §51.1–§51.10 with docs/decisions/0002 and 0003 (PR180 merge 2cb17172)",
            "content_and_reason": "the canonical atoms contract keeps the reminder/operation/\
                receipt rows as mechanism records (never migrated into cognition_attitudes); the \
                sealed decision object is immutable digest addressed content and the accepted \
                settlement receipt is the reason terminal. The reader asserts the same content \
                revision across reopens and that the published own body still carries its \
                provenance reference",
            "atoms_api": "this slice does not consume record_content_attitude/attitudes_for: the \
                PMA decision migration into cognition attitudes belongs to the attitude owner. \
                It also never calls register_legacy_content, so it fabricates no attitude, no \
                legacy-content registration and no confidence. It asserts only that the existing \
                mechanism records are preserved byte-for-byte and that a read-only reopen \
                creates no new content/attitude row (asserted as zero durable change)",
            "external_content_authority": "another agent's committed decision gains no authority \
                here: it stays readable, is never prepared for the epoch owner, and reading the \
                installation creates no durable product fact in a reopen",
            "agent_identity": "the owner head bytes (agent_id, branch, generation) are compared \
                byte-for-byte; this consumer never switches identity and asserts none changed",
            "no_new_gate": "no keyword table, model allowlist, manual approval or static hard deny \
                is added; the refusal is derived from accepted evidence and epoch ownership",
            "producer_is_old_contract": "the producer is pre-fix code by construction; nothing in \
                this receipt claims §51 acceptance from it",
            "confidence": "mechanism records carry no confidence, so none is asserted or invented \
                here; under the canonical atoms contract an absent confidence reads as legacy/\
                unknown and is never replaced by a default number. A 0.0 default would be a real \
                defect, not a neutral filler",
        }),
    );
    receipt.insert(
        "ledger_schema_at_first_candidate_open".into(),
        json!({
            "store_db_changed_since_old_producer": ledger_changed_at_first_open,
            "explanation": "the first candidate open runs the candidate's own additive schema \
                migration; content-addressed objects must not change, and later reopens must be \
                byte-identical",
        }),
    );
    receipt.insert(
        "limits".into(),
        json!({
            "refresh_failure_path": "not induced: only the successful refresh is exercised",
            "provider_calls": "structurally impossible in this binary: no broker/provider dependency",
            "sqlite_side_files": "store.db-shm and the cognition-import gate database are recorded but excluded from the asserted digest",
            "ledger_schema": "the first candidate open may add the candidate's additive schema objects; the delta is reported, and byte identity is asserted between candidate reopens",
            "scope": "bounded local mechanism observation; not model efficacy and not an account-level acceptance",
        }),
    );
    std::fs::write(&args.out, serde_json::to_vec_pretty(&Value::Object(receipt))?)?;
    println!("{}", args.out.display());
    if checks.all_ok() {
        Ok(0)
    } else {
        eprintln!("candidate reader checks failed: {:?}", checks.failures());
        Ok(4)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("pma-legacy-reminder-reader failed: {error:#}");
            std::process::exit(2);
        }
    }
}
