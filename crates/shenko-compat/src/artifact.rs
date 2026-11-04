//! Bounded read of objects this installation sealed: plain small objects and
//! the chunk-indexed observation manifests emitted for large compat outputs.
//! A digest in `learning_source` is thus retrievable by the subject itself.
use std::{borrow::Cow, convert::Infallible, sync::Arc};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use base64::Engine as _;
use cordis_core::{Context, InjectSpec, Plugin, Service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_subject::{NativeChildService, NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{effect::*, evidence::ObservationLevel, ids::EntityId, object::Digest,
    operation::*, plugin_runtime::NativeCapabilityDeclaration, task::TaskRecord};
use crate::recorded::{RecordedCall, Workers, joined, reconcile_original};

pub const NAME: &str = "compat.artifact.read";
/// Returned per call; base64 keeps the normalized body inside the 512 KiB
/// inline result bound with header room to spare.
pub const MAX_RANGE_BYTES: u64 = 128 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const CONTRACT: &[u8] = b"shenko.compat.artifact/v1: read an installation CAS object in authenticated ranges; accepts a plain object up to 1 MiB or a compat-indexed-observation manifest; returns at most 131072 bytes per call; no remote effect";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReadInput {
    /// Digest from `learning_source.digest` / `source_receipt.digest`.
    pub digest: Digest,
    #[serde(default)] pub offset: u64,
    #[serde(default)] pub limit: u64,
}

struct Runtime { store: Arc<Store>, workers: Workers }
impl Service for Runtime { const NAME: &'static str = "shenko.compat.artifact.reader"; }

impl Runtime {
    fn read(&self, input: &ArtifactReadInput) -> Result<Value> {
        ensure!(input.digest.canonical_blake3_hex().is_some(), "invalid artifact digest");
        ensure!(input.limit <= MAX_RANGE_BYTES, "artifact read limit must be <= {MAX_RANGE_BYTES}");
        let object_bytes = self.store.cas.object_size(&input.digest)
            .context("CAS object is absent from this installation")?;
        ensure!(object_bytes <= MAX_MANIFEST_BYTES, "object exceeds the bounded read bound; only manifest-indexed artifacts are supported");
        let raw = self.store.cas.read_object(&input.digest)?;
        ensure!(raw.len() as u64 == object_bytes, "CAS object size mismatch");
        let manifest: Option<Value> = serde_json::from_slice::<Value>(&raw).ok()
            .filter(|value: &Value| value["format"] == "compat-indexed-observation" && value["schema_version"] == 1);
        let bound = if input.limit == 0 { MAX_RANGE_BYTES } else { input.limit.min(MAX_RANGE_BYTES) };
        let Some(manifest) = manifest else {
            ensure!(input.offset <= object_bytes, "artifact read offset out of range");
            let count = (object_bytes - input.offset).min(bound) as usize;
            let body = &raw[input.offset as usize..input.offset as usize + count];
            let position = input.offset + count as u64;
            return Ok(json!({"status":"received","format":"raw","digest":input.digest,
                "total_bytes":object_bytes,"offset":input.offset,
                "next_offset":(position < object_bytes).then_some(position),
                "content_base64":base64::engine::general_purpose::STANDARD.encode(body)}));
        };
        let digest: Digest = serde_json::from_value(manifest["artifact_digest"].clone())?;
        let bytes = manifest["bytes"].as_u64().context("artifact manifest size missing")?;
        let chunk_bytes = manifest["chunk_bytes"].as_u64().context("artifact manifest chunk size missing")? as usize;
        let chunks: Vec<Digest> = serde_json::from_value(manifest["chunks"].clone())?;
        ensure!(chunks.iter().all(|d| d.canonical_blake3_hex().is_some())
            && chunks.len() as u64 == bytes.div_ceil(chunk_bytes as u64), "artifact manifest coverage mismatch");
        ensure!(input.offset <= bytes, "artifact read offset out of range");
        let end = bytes.min(input.offset.saturating_add(bound));
        let mut body = Vec::with_capacity((end - input.offset) as usize);
        let mut position = input.offset;
        while position < end {
            let chunk = position / chunk_bytes as u64;
            let chunk_start = chunk * chunk_bytes as u64;
            let chunk_len = (bytes - chunk_start).min(chunk_bytes as u64) as usize;
            let data = self.store.cas.read_indexed_range(&digest, bytes, chunk_start, chunk_len,
                &chunks[chunk as usize])?;
            let first = (position - chunk_start) as usize;
            let count = (data.len() - first).min((end - position) as usize);
            body.extend_from_slice(&data[first..first + count]);
            position += count as u64;
        }
        Ok(json!({"status":"received","format":"compat-indexed-observation","digest":input.digest,
            "artifact_digest":digest,"total_bytes":bytes,"offset":input.offset,
            "next_offset":(position < bytes).then_some(position),
            "content_base64":base64::engine::general_purpose::STANDARD.encode(body)}))
    }

    async fn run(self: Arc<Self>, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(invocation.task.kind == NAME && invocation.task.definition_version == 1, "artifact read task identity mismatch");
        let input: ArtifactReadInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let mut call = RecordedCall::prepare(&self.store, &invocation, &descriptor(), &input, serde_json::to_value(&input)?,
            "authenticated byte range of a sealed CAS object or explicit failure",
            "read the original object; never substitute different bytes")?;
        if let Some(existing) = call.arm(&self.store, &invocation)? { return Ok(existing); }
        if self.workers.stop.is_cancelled() || self.store.external_task(invocation.task.task_id)?.abort_request.is_some() {
            return call.observe(&self.store, json!({"digest":input.digest,"status":"cancelled_before_read"}),
                Some(OperationOutcome::Cancelled), EffectStatus::None, None);
        }
        match self.read(&input) {
            Ok(value) => call.observe(&self.store, value, Some(OperationOutcome::Succeeded), EffectStatus::None, Some(ObservationLevel::PostconditionVerified)),
            Err(error) => call.observe(&self.store, json!({"digest":input.digest,"status":"artifact_read_failed","error":error.to_string()}),
                Some(OperationOutcome::Failed), EffectStatus::None, None),
        }
    }
}

fn descriptor() -> NativeTaskDescriptor {
    NativeTaskDescriptor { kind: NAME.into(), definition_version: 1,
        implementation_ref: shenko_types::task::ImplementationRef {
            plugin_id: "2d9f3e7a-8b5c-4c1e-9f6a-0e2b4a1d8c6f".parse().expect("fixed artifact reader identity"),
            artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 },
        role_revision: "native-child/1".into(), authority_refs: vec!["compat.artifact/v1".into()],
        prompt_section: Some("Read a sealed CAS object in bounded ranges. Input {digest,offset?,limit?}; limit<=131072 bytes, default whole object when small. Accepts a plain object <=1 MiB or a compat-indexed-observation manifest (e.g. from learning_source.digest); manifest ranges are authenticated per 1 MiB chunk. Follow next_offset until null. Read-only; no remote effect.".into()) }
}

struct Handler(Arc<Runtime>);
#[async_trait]
impl NativeTaskHandler for Handler {
    fn descriptor(&self) -> NativeTaskDescriptor { descriptor() }
    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration { capability_id: NAME.into(), version: "1".into(), description: descriptor().prompt_section.unwrap(),
            usage: Some("delegate:compat.artifact.read:1 {\"digest\":\"blake3:...\",\"offset\":0,\"limit\":131072}".into()) }]
    }
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = descriptor();
        vec![EffectContractBuilder::new(descriptor.implementation_ref.plugin_id, descriptor.implementation(), NAME, 1)
            .resource("installation_cas", "sealed_object", ResourceAccess::Read)
            .action(EffectActionKind::Cancel)
            .idempotency(IdempotencyGuarantee::Strict { key_namespace: "compat.artifact.digest".into(), max_age_secs: u64::MAX })
            .cancel(CancelContract { can_cancel_before_start: true, can_cancel_in_flight: false, cancel_confirms_stopped: false, may_have_partial_effect: false })
            .checkpoint(CheckpointContract { capturable: vec!["frozen digest and returned range".into()], excluded: vec![], coordinated: false, consistency: "local CAS read only".into() })
            .lifecycle(LifecycleContract { on_upgrade: "read objects by digest across versions".into(), on_uninstall: "withdraw native definition".into(), on_crash: "reconcile original receipt".into(), receipt_readable_after_removal: true })
            .basis(GuaranteeBasis::ClaimOnly).build()]
    }
    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        joined(self.0.workers.spawn(self.0.clone().run(invocation))?).await
    }
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_original(&self.0.store, task, operation_id, NAME)
    }
}

/// Registers the bounded CAS reader as a native definition; the caller owns
/// the generation whose lifecycle withdraws it.
pub struct ArtifactReadPlugin;
impl Plugin for ArtifactReadPlugin {
    type Config = Arc<Store>;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;
    fn name(&self) -> Cow<'_, str> { Cow::Borrowed("shenko-compat-artifact-reader") }
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
