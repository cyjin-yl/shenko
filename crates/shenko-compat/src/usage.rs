//! Bounded deterministic digest of the durable observations this
//! installation's compat adapters recorded. Groups evidence by method and
//! outcome so the subject can pick CAS-authenticated exemplars for
//! `cognition.unit.write` candidates. These are observations only; nothing
//! here adopts, enables or judges a unit.
use std::{borrow::Cow, collections::{BTreeMap, BTreeSet}, convert::Infallible, sync::Arc};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_subject::NativeChildService;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{effect::*, evidence::ObservationLevel, ids::EntityId, object::Digest,
    operation::*, plugin_runtime::NativeCapabilityDeclaration, task::TaskRecord};
use crate::recorded::{RecordedCall, Workers, joined, reconcile_original};

pub const NAME: &str = "compat.usage.digest";
/// Hard bounds: the input can ask for fewer records, never more.
pub const MAX_WINDOW_RECORDS: u32 = 200;
/// One scan is bounded across all observed compat operation kinds.
const MAX_SCANNED_OPERATIONS: usize = 3072;
const MAX_GROUPS: usize = 200;
const MAX_EXEMPLARS: usize = 3;
const MAX_EXEMPLAR_BYTES: u64 = 1024 * 1024;
const MAX_METHOD_CHARS: usize = 256;
const OPERATION_KINDS: [&str; 3] = ["compat.mcp.request", "compat.skill.source", crate::artifact::NAME];
const CONTRACT: &[u8] = b"shenko.compat.usage/v1: deterministic bounded aggregation over durable compat observations; groups by request method and settled outcome; at most 200 records per digest and 3 CAS-verified exemplar digests per group; a digest that fails CAS authentication is omitted, never asserted; observations only, adoption stays an explicit cognition.unit.write candidate; no remote effect";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageDigestInput {
    /// Restrict to observations recorded under one installed adapter unit.
    /// Absent fields match any value; an absent unit matches nothing.
    #[serde(default)] pub unit: Option<UnitFilter>,
    #[serde(default)] pub window: Window,
    /// "method" or "method+status".
    pub grouping: Grouping,
    /// Opaque continuation cursor from a previous `next_cursor`. Absent on
    /// the first page; the reader resumes at strictly older observations.
    #[serde(default)] pub cursor: Option<Cursor>,
}

/// Continuation in descending (observed_at, receipt_id) order. A bounded
/// scan can return no matches but still advance past its last candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub observed_at_rfc3339: Option<String>,
    pub receipt_id: EntityId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitFilter {
    #[serde(default)] pub unit_id: Option<String>,
    #[serde(default)] pub revision: Option<String>,
    #[serde(default)] pub artifact_digest: Option<Digest>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    #[serde(default = "default_max_records")] pub max_records: u32,
    #[serde(default)] pub since_rfc3339: Option<String>,
}
fn default_max_records() -> u32 { MAX_WINDOW_RECORDS }
impl Default for Window { fn default() -> Self { Self { max_records: MAX_WINDOW_RECORDS, since_rfc3339: None } } }

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Grouping {
    #[serde(rename = "method")] Method,
    #[serde(rename = "method+status")] MethodStatus,
}

/// One durable observation selected for aggregation.
struct Record {
    /// RFC 3339 evidence timestamp; absent observations sort last.
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
    receipt_id: EntityId,
    method: String,
    status: String,
    normalized: Option<Digest>,
}

struct Runtime { store: Arc<Store>, workers: Workers }
impl Service for Runtime { const NAME: &'static str = "shenko.compat.usage.reader"; }

impl Runtime {
    /// The frozen compat input names the request; MCP keeps the protocol
    /// method there. Verified CAS read: the digest authenticates the bytes.
    fn frozen_input(&self, operation: &OperationRecord) -> Option<Value> {
        let digest = &operation.final_input_ref.digest;
        if operation.final_input_ref.bytes > MAX_EXEMPLAR_BYTES { return None; }
        let bytes = self.store.cas.read_object(digest).ok()?;
        if Digest::blake3(&bytes) != *digest || bytes.len() as u64 != operation.final_input_ref.bytes { return None; }
        serde_json::from_slice(&bytes).ok()
    }

    fn unit_matches(input: &Value, filter: &UnitFilter) -> bool {
        let unit = &input["unit"];
        filter.unit_id.as_ref().is_none_or(|id| unit["unit_id"].as_str() == Some(id))
            && filter.revision.as_ref().is_none_or(|revision| unit["revision"].as_str() == Some(revision))
            && filter.artifact_digest.as_ref().is_none_or(|digest| unit["artifact_digest"].as_str() == Some(digest.0.as_str()))
    }

    /// A digest enters the digest only after this store can authenticate the
    /// exact bytes it names; anything else is omitted, never asserted.
    fn verify_exemplar(&self, digest: &Digest) -> bool {
        digest.canonical_blake3_hex().is_some()
            && self.store.cas.object_size(digest).is_some_and(|bytes| bytes <= MAX_EXEMPLAR_BYTES)
            && self.store.cas.read_object(digest).is_ok_and(|bytes| Digest::blake3(&bytes) == *digest)
    }

    /// One normalized receipt object, authenticated. It carries the
    /// observation used for grouping and is itself the exemplar the subject
    /// reads back through compat.artifact.read.
    fn normalized_result(&self, receipt: &shenko_types::evidence::EvidenceReceipt, verified: &mut BTreeSet<String>) -> Option<Value> {
        let reference = receipt.normalized_ref.as_ref()?;
        if reference.bytes > MAX_EXEMPLAR_BYTES { return None; }
        let bytes = self.store.cas.read_object(&reference.digest).ok()?;
        if Digest::blake3(&bytes) != reference.digest { return None; }
        verified.insert(reference.digest.0.clone());
        serde_json::from_slice(&bytes).ok()
    }

    fn scan(&self, input: &UsageDigestInput) -> Result<Value> {
        ensure!(input.window.max_records <= MAX_WINDOW_RECORDS,
            "usage digest window max_records must be <= {MAX_WINDOW_RECORDS}");
        let since = input.window.since_rfc3339.as_deref()
            .map(|raw| chrono::DateTime::parse_from_rfc3339(raw).map(|parsed| parsed.with_timezone(&chrono::Utc)))
            .transpose().context("window.since_rfc3339 is not an RFC 3339 timestamp")?;
        let cursor_at = match input.cursor.as_ref() {
            Some(cursor) => match cursor.observed_at_rfc3339.as_deref() {
                Some(raw) => Some(chrono::DateTime::parse_from_rfc3339(raw)
                    .map(|parsed| parsed.with_timezone(&chrono::Utc))
                    .context("cursor.observed_at_rfc3339 is not an RFC 3339 timestamp")?),
                None => None,
            },
            None => None,
        };
        let before = input.cursor.as_ref().map(|cursor| (cursor_at, cursor.receipt_id));
        let mut candidates = self.store.external_observations_before(
            &OPERATION_KINDS, since, before,
            if input.window.max_records == 0 { 0 } else { MAX_SCANNED_OPERATIONS + 1 },
        )?;
        // One metadata-only lookahead establishes whether the bounded scan
        // has more candidates. No CAS payload is read for that lookahead.
        let truncated = candidates.len() > MAX_SCANNED_OPERATIONS;
        candidates.truncate(MAX_SCANNED_OPERATIONS);
        let scanned = candidates.len();
        let last_scanned = candidates.last().map(|(_, receipt)| Cursor {
            observed_at_rfc3339: receipt.observed_at.map(|seen| seen.to_rfc3339()),
            receipt_id: receipt.receipt_id,
        });
        let mut verified = BTreeSet::<String>::new();
        let mut records = Vec::new();
        let mut matched = 0usize;
        for (operation, receipt) in candidates {
            let frozen = self.frozen_input(&operation).unwrap_or(Value::Null);
            if input.unit.as_ref().is_some_and(|filter| !Self::unit_matches(&frozen, filter)) { continue; }
            matched += 1;
            if records.len() >= input.window.max_records as usize { continue; }
            let normalized = self.normalized_result(&receipt, &mut verified).unwrap_or(Value::Null);
            let method = normalized["observation"]["protocol"]["method"].as_str()
                .or_else(|| frozen["request"]["method"].as_str())
                .unwrap_or(&operation.operation_name);
            let status = match operation.outcome {
                Some(OperationOutcome::Succeeded) => "succeeded",
                Some(OperationOutcome::Failed) => "failed",
                Some(OperationOutcome::Cancelled) => "cancelled",
                Some(OperationOutcome::Interrupted) => "interrupted",
                None => "unknown",
            };
            records.push(Record { observed_at: receipt.observed_at, receipt_id: receipt.receipt_id,
                method: method.chars().take(MAX_METHOD_CHARS).collect(), status: status.into(),
                normalized: receipt.normalized_ref.as_ref().map(|reference| reference.digest.clone()) });
        }
        // Preserve matching records excluded by the output window. Once all
        // matches fit, advance beyond every scanned candidate, even on an
        // empty page, so rare units remain reachable past unrelated traffic.
        let next_cursor = if matched > records.len() {
            records.last().map(|last| Cursor {
                observed_at_rfc3339: last.observed_at.map(|seen| seen.to_rfc3339()),
                receipt_id: last.receipt_id,
            })
        } else if truncated { last_scanned } else { None };
        let mut groups = BTreeMap::<(String, Option<String>), (u64, Vec<chrono::DateTime<chrono::Utc>>, Vec<Digest>)>::new();
        for record in &records {
            let key = (record.method.clone(),
                matches!(input.grouping, Grouping::MethodStatus).then(|| record.status.clone()));
            let group = groups.entry(key).or_default();
            group.0 += 1;
            if let Some(seen) = record.observed_at { group.1.push(seen); }
            if let Some(digest) = &record.normalized {
                if group.2.len() < MAX_EXEMPLARS && !group.2.contains(digest)
                    && (verified.contains(&digest.0) || self.verify_exemplar(digest)) {
                    group.2.push(digest.clone());
                }
            }
        }
        let groups_truncated = groups.len() > MAX_GROUPS;
        let groups: Vec<Value> = groups.into_iter().take(MAX_GROUPS).map(|((method, status), (count, seen, exemplars))| {
            json!({"method":method,"status":status,"count":count,
                "first_seen_utc":seen.iter().min().map(|seen| seen.to_rfc3339()),
                "latest_occurrence_utc":seen.iter().max().map(|seen| seen.to_rfc3339()),
                "exemplar_digests":exemplars})
        }).collect();
        Ok(json!({"status":"digest_ready","operation_kinds":OPERATION_KINDS,
            "scanned_operations":scanned,"scan_truncated":truncated,
            "matched_records":matched,"windowed_records":records.len(),
            "max_records":input.window.max_records,"groups_truncated":groups_truncated,"groups":groups,
            "next_cursor":next_cursor,
            "observations_only":true,
            "adoption":"observations only; derive learning by cognition.unit.write with derived_from={\"kind\":\"cas_object\",\"digest\":<exemplar digest>}; a unit learned from these exemplars remains a candidate"}))
    }

    async fn run(self: Arc<Self>, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(invocation.task.kind == NAME && invocation.task.definition_version == 1, "usage digest task identity mismatch");
        let input: UsageDigestInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let mut call = RecordedCall::prepare(&self.store, &invocation, &descriptor(), &input, serde_json::to_value(&input)?,
            "a deterministic digest of durable compat observations; every exemplar digest verifies against this installation's CAS",
            "re-run the bounded scan; never replay an adapter effect or assert an unverifiable digest")?;
        if let Some(existing) = call.arm(&self.store, &invocation)? { return Ok(existing); }
        if self.workers.stop.is_cancelled() || self.store.external_task(invocation.task.task_id)?.abort_request.is_some() {
            return call.observe(&self.store, json!({"status":"cancelled_before_scan"}),
                Some(OperationOutcome::Cancelled), EffectStatus::None, None);
        }
        match self.scan(&input) {
            Ok(value) => call.observe(&self.store, value, Some(OperationOutcome::Succeeded), EffectStatus::None, Some(ObservationLevel::PostconditionVerified)),
            Err(error) => call.observe(&self.store, json!({"status":"usage_digest_failed","error":error.to_string()}),
                Some(OperationOutcome::Failed), EffectStatus::None, None),
        }
    }
}

fn descriptor() -> NativeTaskDescriptor {
    NativeTaskDescriptor { kind: NAME.into(), definition_version: 1,
        implementation_ref: shenko_types::task::ImplementationRef {
            plugin_id: "6f1a2b3c-9d4e-4a5b-8c7d-1e2f3a4b5c6d".parse().expect("fixed usage digest identity"),
            artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 },
        role_revision: "native-child/1".into(), authority_refs: vec!["compat.usage/v1".into()],
        prompt_section: Some("Summarize durable compat adapter observations. Input {\"unit\"?:{\"unit_id\"?,\"revision\"?,\"artifact_digest\"?},\"window\"?:{\"max_records\"?:<=200,\"since_rfc3339\"?},\"grouping\":\"method\"|\"method+status\",\"cursor\"?:<previous next_cursor>}. Scans at most 3072 candidates newest first. Continue with next_cursor until null, including empty pages; counts describe this scan. Groups the latest observations by request method and outcome status; each group reports count, first/latest UTC and up to 3 CAS-verified exemplar digests readable via compat.artifact.read. Read-only observations; adoption stays cognition.unit.write.".into()) }
}

struct Handler(Arc<Runtime>);
#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor { descriptor() }
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration { capability_id: NAME.into(), version: "1".into(), description: descriptor().prompt_section.unwrap(),
            usage: Some("delegate:compat.usage.digest:1 {\"grouping\":\"method+status\",\"window\":{\"max_records\":50}}".into()) }]
    }
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = descriptor();
        vec![EffectContractBuilder::new(descriptor.implementation_ref.plugin_id, descriptor.implementation(), NAME, 1)
            .resource("installation_ledger", "compat_observations", ResourceAccess::Read)
            .action(EffectActionKind::Cancel)
            .idempotency(IdempotencyGuarantee::Strict { key_namespace: "compat.usage.digest".into(), max_age_secs: u64::MAX })
            .cancel(CancelContract { can_cancel_before_start: true, can_cancel_in_flight: false, cancel_confirms_stopped: false, may_have_partial_effect: false })
            .checkpoint(CheckpointContract { capturable: vec!["frozen input and computed groups".into()], excluded: vec![], coordinated: false, consistency: "local ledger and CAS reads only".into() })
            .lifecycle(LifecycleContract { on_upgrade: "recompute the digest from the same durable observations".into(), on_uninstall: "withdraw native definition".into(), on_crash: "reconcile original receipt".into(), receipt_readable_after_removal: true })
            .basis(GuaranteeBasis::ClaimOnly).build()]
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(self.0.workers.spawn(self.0.clone().run(invocation))?).await
    }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_original(&self.0.store, task, operation_id, NAME)
    }
}

/// Registers the bounded observation digest as a native definition; the
/// caller owns the generation whose lifecycle withdraws it.
pub struct UsageDigestPlugin;
impl Plugin for UsageDigestPlugin {
    type Config = Arc<Store>;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-usage-digest") }
    fn inject(&self) -> InjectSpec { InjectSpec::none().require(NativeChildService::NAME) }
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> { Ok(config) }
    async fn apply(&self, ctx: Context, store: &Self::Input) -> Result<(), Self::ApplyError> {
        let runtime = Arc::new(Runtime { store: store.clone(), workers: Workers::default() });
        let cleanup = runtime.clone();
        ctx.effect(move || async move { cleanup.workers.close().await; Ok::<(), std::io::Error>(()) })
            .map_err(std::io::Error::other)?;
        let _published = ctx.provide(runtime.clone()).map_err(std::io::Error::other)?;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(Handler(runtime));
        shenko_subject::NativeTaskHandlerPlugin.apply(ctx, &handler).await
    }
}
