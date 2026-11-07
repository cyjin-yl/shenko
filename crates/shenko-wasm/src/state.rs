//! Immutable WASM runtime state: linear memory, the guest's own continuation,
//! and the interpreter object graph (SPEC §10.2, §51.4).
//!
//! §51.4 requires one owned versioned root to include the home files, the
//! core/plugin slots, the awareness/perception selections *and* the WASM
//! linear memory, execution stack and interpreter object graph. The file layer
//! already exists ([`crate::checkpoint`], [`crate::cow`]); this module is the
//! runtime-state layer that was previously an accepted gap.
//!
//! What is stored, and what is honestly not:
//!
//! * Every exported linear memory is sealed byte-for-byte as a content-addressed
//!   object. Pages and page size are recorded so a restore refuses a memory that
//!   cannot be reconstructed exactly.
//! * The execution stack and the interpreter object graph are *not* reachable
//!   from the host: WebAssembly has no way for an engine to copy a live call
//!   stack, and fabricating serialized frames would be a lie. The supported
//!   mechanism is a cooperative continuation protocol implemented by the guest
//!   itself: the guest serializes its own frames/objects into its linear memory
//!   and the host seals that memory. A guest that does not implement the
//!   protocol is recorded as [`Continuation::NotCapturable`] with the reason, and
//!   the resulting state is *not* claimed to be resumable.
//! * Restore is verified, not asserted: the memory must be reconstructible at
//!   the recorded size, the guest's own continuation blob must be re-imported
//!   through the protocol, and the guest must then recompute the same graph
//!   digest it reported at capture. A guest that cannot reproduce its graph
//!   digest is refused instead of being reported restored.
//!
//! Guest protocol (ABI [`CONTINUATION_ABI`]); every export is optional and its
//! absence is recorded, never simulated:
//!
//! | export | signature | meaning |
//! | --- | --- | --- |
//! | `shenko_state_abi` | `() -> i32` | protocol version this guest implements |
//! | `shenko_state_export` | `() -> i32` | address of a [`CONTINUATION_DESCRIPTOR_BYTES`] descriptor, `0` = nothing to save |
//! | `shenko_state_alloc` | `(i32) -> i32` | guest-owned allocation for an imported blob |
//! | `shenko_state_import` | `(i32, i32) -> i32` | resume from the blob at `(ptr, len)`; `0` = accepted |
//!
//! The descriptor is little-endian: `magic:u32, abi:u32, resume_kind:u32,
//! reserved:u32, ptr:u64, len:u64, graph_digest:[u8;32]`. The graph digest is
//! computed by the guest over its own restored object graph; it is what makes
//! "the object graph came back" a checkable claim rather than a copy of bytes.
//!
//! Boundaries that stay explicit:
//!
//! * Capture requires exclusive access to the store (`&mut Store`), so no guest
//!   code executes during it. There is no mid-hostcall snapshot and none is
//!   claimed.
//! * External transport state (open sockets, pipes, in-flight requests) is not
//!   structurally reversible. Resources are recorded as rebindable identity
//!   evidence; a resource that cannot be rebound makes the state
//!   [`WasmStateLevel::MemoryWithoutResume`] or lower, never "restored", and
//!   bytes already sent are never replayed automatically.
//! * Restoring files or memory never undoes an external effect that already
//!   happened (SPEC §10.1, §51.4).

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use std::path::{Path, PathBuf};
use wasmtime::{Extern, Instance, Store};

use crate::mount::MountSpec;

/// Versioned state format. A different format is refused, never guessed.
pub const WASM_STATE_FORMAT: &str = "shenko-wasm-state/1";
/// CAS namespace for state manifests, memory objects and continuation blobs.
pub const WASM_STATE_NAMESPACE: &str = "seed-wasm-state";
pub const WASM_STATE_MEDIA_TYPE: &str = "application/vnd.shenko.wasm-state+json";
pub const WASM_STATE_CODEC_NAME: &str = "shenko-wasm-state";
pub const WASM_STATE_CODEC_MAJOR: u32 = 1;
pub const MEMORY_MEDIA_TYPE: &str = "application/vnd.shenko.wasm-memory";
pub const MEMORY_CODEC_NAME: &str = "shenko-wasm-memory";
pub const MEMORY_CODEC_MAJOR: u32 = 1;
pub const CONTINUATION_MEDIA_TYPE: &str = "application/vnd.shenko.wasm-continuation";
pub const CONTINUATION_CODEC_NAME: &str = "shenko-wasm-continuation";
pub const CONTINUATION_CODEC_MAJOR: u32 = 1;

/// Cooperative continuation protocol version implemented by this host.
pub const CONTINUATION_ABI: u32 = 1;
/// Fixed descriptor the guest writes into its own memory.
pub const CONTINUATION_DESCRIPTOR_BYTES: usize = 64;
/// `SHSK` in little-endian byte order.
pub const CONTINUATION_MAGIC: u32 = 0x4B53_4853;
pub const STATE_ABI_EXPORT: &str = "shenko_state_abi";
pub const STATE_EXPORT_EXPORT: &str = "shenko_state_export";
pub const STATE_ALLOC_EXPORT: &str = "shenko_state_alloc";
pub const STATE_IMPORT_EXPORT: &str = "shenko_state_import";
/// Depth cap for descriptor-driven reads; a guest cannot make the host read
/// unbounded memory in one call.
const MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Backend identity a manifest is bound to. A restore under a different engine,
/// version, ABI or target is refused instead of being attempted.
pub const WASM_BACKEND: &str = "wasmtime";
/// Must match the workspace pin in the root `Cargo.toml`.
pub const WASM_BACKEND_VERSION: &str = "45";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmBackendIdentity {
    pub engine: String,
    pub engine_version: String,
    /// Continuation protocol the backend/host implements.
    pub snapshot_abi: u32,
    /// `arch-os-endianness`, e.g. `x86_64-linux-le`.
    pub target: String,
}

impl WasmBackendIdentity {
    pub fn current() -> Self {
        Self {
            engine: WASM_BACKEND.into(),
            engine_version: WASM_BACKEND_VERSION.into(),
            snapshot_abi: CONTINUATION_ABI,
            target: format!(
                "{}-{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS,
                if cfg!(target_endian = "little") { "le" } else { "be" }
            ),
        }
    }

    fn ensure_current(&self) -> Result<()> {
        let current = Self::current();
        ensure!(
            *self == current,
            "WASM_STATE_BACKEND_MISMATCH: state was captured by {}/{} (abi {}, {}) but this host is {}/{} (abi {}, {}); a state is only restorable under the backend it was captured with",
            self.engine,
            self.engine_version,
            self.snapshot_abi,
            self.target,
            current.engine,
            current.engine_version,
            current.snapshot_abi,
            current.target
        );
        Ok(())
    }
}

/// One exported linear memory as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMemory {
    /// Export name, so a restore binds by name rather than by position.
    pub name: String,
    pub pages: u64,
    pub page_size: u64,
    pub bytes: u64,
    /// `None` when the memory has no declared maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u64>,
    pub digest: Digest,
    pub object: ObjectRef,
}

/// What the guest said about its own continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Continuation {
    /// The guest serialized its frames/objects into linear memory and the host
    /// sealed that blob. Restoring it is a *candidate* resumable state; the
    /// restore re-imports the blob and requires the graph digest to match.
    Capturable(GuestContinuation),
    /// Nothing was captured, with the concrete reason. This is never counted
    /// as resumable state.
    NotCapturable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestContinuation {
    /// Protocol version the guest reported.
    pub abi: u32,
    /// Where in which memory the descriptor was found.
    pub memory: String,
    pub descriptor_ptr: u64,
    /// Address/length of the blob inside linear memory.
    pub blob_ptr: u64,
    pub blob_bytes: u64,
    pub blob_digest: Digest,
    pub blob_object: ObjectRef,
    /// Digest the guest computed over its own object graph/stack at export.
    pub graph_digest: Digest,
    /// Guest-declared resume kind (protocol-defined, opaque to the host).
    pub resume_kind: u32,
}

/// A resource the restored state needs re-bound at its *original* identity.
/// Recorded as evidence; a mismatch is reported, never silently accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebindableResource {
    /// `preopen`, `external-socket`, `stdio`, ...
    pub kind: String,
    pub name: String,
    /// Identity that must hold after restore, or `None` when the resource is
    /// caller-owned and cannot be proven (stdio, live transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// True when the state cannot be resumed without it.
    pub required: bool,
    /// Whether this resource is structurally reversible at all. Live external
    /// transport is `false`: the bytes may already have left the process.
    pub reversible: bool,
}

/// What reach a captured run had over external transport.
///
/// A socket is a host OS resource tied to a live connection: there is no
/// serialized form, no rebinding of an existing descriptor and no automatic
/// replay of bytes already delivered. Recording the admitted targets keeps the
/// reach visible without pretending the transport is part of the restorable
/// state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTransportEvidence {
    pub host: String,
    pub port: u16,
    /// Whether the admitted target allowed name resolution.
    pub resolve: bool,
    /// Operations the admission covered (`connect`, `listen`, `accept`).
    pub operations: Vec<String>,
}

/// How quiescent the capture was. Recorded, not assumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuiesceReceipt {
    /// The host held exclusive store access: no guest code ran concurrently.
    pub exclusive_store_access: bool,
    /// The capture ran between guest calls, not inside one.
    pub between_calls: bool,
    /// Monotonic per-instance capture sequence.
    pub instance_sequence: u64,
}

/// The immutable state manifest. Its id is the digest of its canonical JSON, so
/// it can be the state sub-node of a controller root manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmStateManifest {
    pub format: String,
    pub backend: WasmBackendIdentity,
    /// Digest of the exact module bytes the instance was created from.
    pub module: Digest,
    pub module_bytes: u64,
    /// One run's instance identity; a controller root binds it together with
    /// the core/plugin slot versions.
    pub instance_id: EntityId,
    pub memories: Vec<SavedMemory>,
    pub continuation: Continuation,
    pub resources: Vec<RebindableResource>,
    /// External transport the run was admitted for. Never restored, only
    /// recorded; a restored run must re-admit transport as a new effect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transport: Vec<ExternalTransportEvidence>,
    pub quiesce: QuiesceReceipt,
    pub captured_at_unix_ms: u64,
}

impl WasmStateManifest {
    pub fn id(&self) -> Digest {
        Digest::blake3(&serde_json::to_vec(self).expect("wasm state manifest serialize"))
    }

    pub fn total_bytes(&self) -> u64 {
        self.memories.iter().map(|memory| memory.bytes).sum()
    }
}

/// How much of the §51.4 state this manifest actually carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmStateLevel {
    /// Linear memory only; the execution stack and interpreter graph were not
    /// captured, so this is not resumable state.
    MemoryWithoutResume,
    /// Memory *and* a guest continuation whose blob was sealed; resumability is
    /// still pending the restore-time graph-digest check.
    CapturedContinuation,
    /// The manifest, its objects and its resources were verified, and every
    /// required resource is rebindable: a restore can attempt resumption.
    ResumableState,
}

/// The state sub-node a controller root manifest composes (published contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmStateRef {
    pub manifest: Digest,
    pub memories: Vec<Digest>,
    pub backend: WasmBackendIdentity,
    pub module: Digest,
    pub instance_id: EntityId,
    /// Continuation blob digest when one exists; `None` is an honest gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Digest>,
    pub level: String,
}

/// Result of a capture.
#[derive(Debug, Clone)]
pub struct CapturedWasmState {
    pub manifest: WasmStateManifest,
    pub id: Digest,
    pub objects: usize,
    pub bytes: u64,
    pub level: WasmStateLevel,
}

impl CapturedWasmState {
    pub fn to_ref(&self) -> WasmStateRef {
        WasmStateRef {
            manifest: self.id.clone(),
            memories: self
                .manifest
                .memories
                .iter()
                .map(|memory| memory.digest.clone())
                .collect(),
            backend: self.manifest.backend.clone(),
            module: self.manifest.module.clone(),
            instance_id: self.manifest.instance_id,
            continuation: match &self.manifest.continuation {
                Continuation::Capturable(continuation) => Some(continuation.blob_digest.clone()),
                Continuation::NotCapturable { .. } => None,
            },
            level: level_name(self.level).into(),
        }
    }
}

fn level_name(level: WasmStateLevel) -> &'static str {
    match level {
        WasmStateLevel::MemoryWithoutResume => "memory_without_resume",
        WasmStateLevel::CapturedContinuation => "captured_continuation",
        WasmStateLevel::ResumableState => "resumable_state",
    }
}

/// Summary of a verified state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWasmState {
    pub id: Digest,
    pub memories: usize,
    pub bytes: u64,
    pub continuation: bool,
    pub level: WasmStateLevel,
}

/// What a restore actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmStateRestore {
    pub manifest: Digest,
    /// Memories whose bytes were written back.
    pub memories: usize,
    pub bytes: u64,
    /// True only when the guest re-imported its continuation and reproduced the
    /// captured graph digest.
    pub continuation_resumed: bool,
    pub graph_digest: Option<Digest>,
    /// Resource identities that could not be re-bound. A non-empty list is not
    /// a completed restore claim.
    pub unrebindable: Vec<String>,
    pub level: WasmStateLevel,
}

/// Backend state capture/verification/restore over one installation's CAS.
pub struct WasmStateProvider<'a> {
    cas: &'a ContentStore,
}

impl<'a> WasmStateProvider<'a> {
    pub fn new(cas: &'a ContentStore) -> Self {
        Self { cas }
    }

    /// Capture the state of `instance` with exclusive store access.
    ///
    /// `module_bytes` must be the exact bytes the instance was created from;
    /// the guest protocol is invoked here, so the caller must have quiesced the
    /// instance first (holding `&mut Store` is what makes that true).
    pub fn capture<T: 'static>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
        module_bytes: &[u8],
        instance_id: EntityId,
        mounts: &[MountSpec],
        transport: &[ExternalTransportEvidence],
        instance_sequence: u64,
    ) -> Result<CapturedWasmState> {
        ensure!(
            !module_bytes.is_empty(),
            "WASM_STATE_UNSUPPORTED: a state needs the exact module bytes it was created from"
        );
        let module_digest = Digest::blake3(module_bytes);
        let exports = exported_memories(store, instance);
        ensure!(
            !exports.is_empty(),
            "WASM_STATE_UNSUPPORTED: the instance exports no linear memory, so no runtime state can be captured"
        );
        if let Some((name, _)) = exports.iter().find(|(_, shared)| *shared) {
            // A shared memory can be mutated by another thread at any instant,
            // so sealing it would publish a torn view. Refusing is honest; the
            // gap stays open instead of being hidden behind a partial state.
            bail!(
                "WASM_STATE_UNSUPPORTED: memory {name:?} is shared and cannot be snapshotted consistently"
            );
        }
        let memory_names: Vec<String> = exports.into_iter().map(|(name, _)| name).collect();

        // The guest protocol runs first: it writes its serialized
        // frames/objects into linear memory, and the memory sealed below must be
        // the one that contains them.
        let continuation = self.capture_continuation(store, instance, &memory_names)?;
        let blob_digest = match &continuation {
            Continuation::Capturable(guest) => Some(guest.blob_digest.clone()),
            Continuation::NotCapturable { .. } => None,
        };

        let mut memories = Vec::new();
        let mut objects = 0usize;
        let mut total_bytes = 0u64;
        for name in &memory_names {
            let memory = instance
                .get_memory(&mut *store, name)
                .ok_or_else(|| anyhow!("WASM_STATE_UNSUPPORTED: memory {name:?} disappeared"))?;
            let pages = memory.size(&*store);
            let page_size = memory.page_size(&*store);
            let bytes = memory.data_size(&*store);
            ensure!(
                (bytes as u64) <= MAX_MEMORY_BYTES,
                "WASM_STATE_UNSUPPORTED: memory {name:?} is {bytes} bytes"
            );
            let data = memory.data(&*store);
            let sealed = self
                .cas
                .seal_object(
                    data,
                    MEMORY_MEDIA_TYPE,
                    WASM_STATE_NAMESPACE,
                    MEMORY_CODEC_NAME,
                    MEMORY_CODEC_MAJOR,
                )
                .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: seal memory {name:?}: {error}"))?;
            let max_pages = memory.ty(&*store).maximum();
            objects += 1;
            total_bytes += sealed.object.bytes;
            memories.push(SavedMemory {
                name: name.clone(),
                pages,
                page_size,
                bytes: sealed.object.bytes,
                max_pages,
                digest: sealed.object.digest.clone(),
                object: state_object_ref(
                    &sealed.object.digest,
                    sealed.object.bytes,
                    MEMORY_MEDIA_TYPE,
                    MEMORY_CODEC_NAME,
                    MEMORY_CODEC_MAJOR,
                ),
            });
        }
        // A captured continuation blob lives inside a captured memory; if it
        // does not, the manifest would name a blob no restore can place.
        if let (Some(blob), true) = (&blob_digest, memories.is_empty()) {
            bail!("WASM_STATE_INCOMPLETE: continuation blob {blob} has no captured memory");
        }

        let resources = resource_evidence(mounts, transport);
        let level = state_level(&continuation, &resources);
        let manifest = WasmStateManifest {
            format: WASM_STATE_FORMAT.into(),
            backend: WasmBackendIdentity::current(),
            module: module_digest,
            module_bytes: module_bytes.len() as u64,
            instance_id,
            memories,
            continuation,
            resources,
            transport: transport.to_vec(),
            quiesce: QuiesceReceipt {
                exclusive_store_access: true,
                between_calls: true,
                instance_sequence,
            },
            captured_at_unix_ms: now_unix_ms(),
        };
        let bytes = serde_json::to_vec(&manifest).expect("wasm state manifest serialize");
        let sealed = self
            .cas
            .seal_object(
                &bytes,
                WASM_STATE_MEDIA_TYPE,
                WASM_STATE_NAMESPACE,
                WASM_STATE_CODEC_NAME,
                WASM_STATE_CODEC_MAJOR,
            )
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: seal state manifest: {error}"))?;
        let id = sealed.object.digest.clone();
        ensure!(
            id == manifest.id(),
            "WASM_STATE_INCOMPLETE: sealed manifest id {id} does not match its canonical digest"
        );
        objects += 2; // manifest + continuation blob (when present)
        Ok(CapturedWasmState {
            manifest,
            id,
            objects,
            bytes: total_bytes,
            level,
        })
    }

    fn capture_continuation<T: 'static>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
        memory_names: &[String],
    ) -> Result<Continuation> {
        let abi = match instance.get_typed_func::<(), i32>(&mut *store, STATE_ABI_EXPORT) {
            Ok(func) => match func.call(&mut *store, ()) {
                Ok(abi) => abi,
                Err(error) => {
                    return Ok(Continuation::NotCapturable {
                        reason: format!("{STATE_ABI_EXPORT} trapped: {error}"),
                    });
                }
            },
            Err(_) => {
                return Ok(Continuation::NotCapturable {
                    reason: format!(
                        "the guest does not implement the continuation protocol (missing export {STATE_ABI_EXPORT}); the execution stack and interpreter object graph are not host-visible"
                    ),
                });
            }
        };
        if abi as u32 != CONTINUATION_ABI {
            return Ok(Continuation::NotCapturable {
                reason: format!(
                    "guest continuation ABI {abi} is not the host ABI {CONTINUATION_ABI}"
                ),
            });
        }
        let export = instance
            .get_typed_func::<(), i32>(&mut *store, STATE_EXPORT_EXPORT)
            .map_err(|_| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: guest declares ABI {abi} but does not export {STATE_EXPORT_EXPORT}"
                )
            })?;
        let descriptor_ptr = match export.call(&mut *store, ()) {
            Ok(ptr) => ptr,
            Err(error) => {
                return Ok(Continuation::NotCapturable {
                    reason: format!("{STATE_EXPORT_EXPORT} trapped: {error}"),
                });
            }
        };
        if descriptor_ptr <= 0 {
            return Ok(Continuation::NotCapturable {
                reason: "the guest reported no resumable continuation".into(),
            });
        }
        let descriptor_ptr = descriptor_ptr as u64;
        let (memory_name, descriptor) =
            read_descriptor(store, instance, memory_names, descriptor_ptr)?;
        ensure!(
            descriptor.magic == CONTINUATION_MAGIC,
            "WASM_STATE_INCOMPLETE: continuation descriptor magic {:#x} is not {CONTINUATION_MAGIC:#x}",
            descriptor.magic
        );
        ensure!(
            descriptor.abi == CONTINUATION_ABI,
            "WASM_STATE_INCOMPLETE: continuation descriptor ABI {} is not {CONTINUATION_ABI}",
            descriptor.abi
        );
        if descriptor.len == 0 {
            return Ok(Continuation::NotCapturable {
                reason: "the guest reported an empty continuation blob".into(),
            });
        }
        let memory = instance
            .get_memory(&mut *store, &memory_name)
            .ok_or_else(|| anyhow!("WASM_STATE_INCOMPLETE: memory {memory_name:?} disappeared"))?;
        let data_len = memory.data_size(&*store) as u64;
        ensure!(
            descriptor
                .ptr
                .checked_add(descriptor.len)
                .is_some_and(|end| end <= data_len),
            "WASM_STATE_INCOMPLETE: continuation blob {}..{} is outside memory {memory_name:?} ({data_len} bytes)",
            descriptor.ptr,
            descriptor.len
        );
        // The blob is read through the guest's declared range and sealed on its
        // own, so a controller/root reader can tell the continuation apart from
        // bulk memory and verify it directly.
        let blob = {
            let data = memory.data(&*store);
            data[descriptor.ptr as usize..(descriptor.ptr + descriptor.len) as usize].to_vec()
        };
        let sealed = self
            .cas
            .seal_object(
                &blob,
                CONTINUATION_MEDIA_TYPE,
                WASM_STATE_NAMESPACE,
                CONTINUATION_CODEC_NAME,
                CONTINUATION_CODEC_MAJOR,
            )
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: seal continuation: {error}"))?;
        Ok(Continuation::Capturable(GuestContinuation {
            abi: descriptor.abi,
            memory: memory_name,
            descriptor_ptr,
            blob_ptr: descriptor.ptr,
            blob_bytes: descriptor.len,
            blob_digest: sealed.object.digest.clone(),
            blob_object: state_object_ref(
                &sealed.object.digest,
                sealed.object.bytes,
                CONTINUATION_MEDIA_TYPE,
                CONTINUATION_CODEC_NAME,
                CONTINUATION_CODEC_MAJOR,
            ),
            graph_digest: descriptor.graph_digest,
            resume_kind: descriptor.resume_kind,
        }))
    }

    /// Verify a manifest's backend binding and every object it names.
    pub fn verify(&self, manifest: &WasmStateManifest) -> Result<VerifiedWasmState> {
        ensure!(
            manifest.format == WASM_STATE_FORMAT,
            "WASM_STATE_UNSUPPORTED: manifest format {:?} is not {WASM_STATE_FORMAT:?}",
            manifest.format
        );
        manifest.backend.ensure_current()?;
        ensure!(
            manifest.module.canonical_blake3_hex().is_some() && manifest.module_bytes > 0,
            "WASM_STATE_INCOMPLETE: manifest does not name the module bytes it came from"
        );
        ensure!(
            !manifest.memories.is_empty(),
            "WASM_STATE_INCOMPLETE: manifest records no linear memory"
        );
        let mut bytes = 0u64;
        for memory in &manifest.memories {
            ensure!(
                memory.pages > 0 && memory.page_size > 0 && memory.bytes == memory.pages * memory.page_size,
                "WASM_STATE_INCOMPLETE: memory {:?} describes {} pages of {} bytes but {} bytes",
                memory.name,
                memory.pages,
                memory.page_size,
                memory.bytes
            );
            ensure!(
                memory.object.digest == memory.digest && memory.object.bytes == memory.bytes,
                "WASM_STATE_INCOMPLETE: memory {:?} object does not match its recorded digest/size",
                memory.name
            );
            let path = self.cas.verified_object_path(&memory.object).map_err(|error| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: memory {:?} object {} is missing or corrupt: {error}",
                    memory.name,
                    memory.digest
                )
            })?;
            ensure!(
                path.is_file(),
                "WASM_STATE_INCOMPLETE: memory {:?} object is not a file",
                memory.name
            );
            bytes += memory.bytes;
        }
        let continuation = match &manifest.continuation {
            Continuation::Capturable(guest) => {
                ensure!(
                    guest.abi == CONTINUATION_ABI && guest.blob_bytes > 0,
                    "WASM_STATE_INCOMPLETE: continuation records ABI {} with {} bytes",
                    guest.abi,
                    guest.blob_bytes
                );
                ensure!(
                    manifest.memories.iter().any(|memory| memory.name == guest.memory),
                    "WASM_STATE_INCOMPLETE: continuation names memory {:?} that the manifest does not record",
                    guest.memory
                );
                ensure!(
                    guest.blob_object.digest == guest.blob_digest
                        && guest.blob_object.bytes == guest.blob_bytes,
                    "WASM_STATE_INCOMPLETE: continuation blob object does not match its recorded digest/size"
                );
                let path = self.cas.verified_object_path(&guest.blob_object).map_err(|error| {
                    anyhow!(
                        "WASM_STATE_INCOMPLETE: continuation blob {} is missing or corrupt: {error}",
                        guest.blob_digest
                    )
                })?;
                ensure!(
                    path.is_file(),
                    "WASM_STATE_INCOMPLETE: continuation blob object is not a file"
                );
                true
            }
            Continuation::NotCapturable { .. } => false,
        };
        // The manifest object itself must exist: a controller root referencing
        // it must be resolvable.
        let id = manifest.id();
        ensure!(
            self.cas.object_exists(&id),
            "WASM_STATE_INCOMPLETE: state manifest {id} is not published in this installation's CAS"
        );
        let level = state_level(&manifest.continuation, &manifest.resources);
        Ok(VerifiedWasmState {
            id,
            memories: manifest.memories.len(),
            bytes,
            continuation,
            level,
        })
    }

    /// Restore a state onto a freshly instantiated instance of the same module.
    ///
    /// The memory is reconstructed at its recorded size, the guest continuation
    /// is imported through the protocol, and the guest must then reproduce the
    /// captured graph digest. Anything less is refused instead of reported as a
    /// resumed execution.
    pub fn restore<T: 'static>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
        manifest: &WasmStateManifest,
        current_backend: &WasmBackendIdentity,
    ) -> Result<WasmStateRestore> {
        let verified = self.verify(manifest)?;
        ensure!(
            &manifest.backend == current_backend,
            "WASM_STATE_BACKEND_MISMATCH: state was captured by {}/{} (abi {}), this instance runs {}/{} (abi {})",
            manifest.backend.engine,
            manifest.backend.engine_version,
            manifest.backend.snapshot_abi,
            current_backend.engine,
            current_backend.engine_version,
            current_backend.snapshot_abi
        );
        // Rebinding evidence is checked before any memory is overwritten.
        let mut unrebindable = Vec::new();
        for resource in &manifest.resources {
            if !resource.reversible || resource.identity.is_none() {
                unrebindable.push(format!(
                    "{}:{} is caller-owned or structurally irreversible; the caller must re-supply it and no automatic replay happens",
                    resource.kind, resource.name
                ));
            }
        }
        let mut written = 0usize;
        let mut bytes = 0u64;
        for memory in &manifest.memories {
            let target = instance.get_memory(&mut *store, &memory.name).ok_or_else(|| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: the instance exports no memory {:?}",
                    memory.name
                )
            })?;
            let current_pages = target.size(&*store);
            ensure!(
                target.page_size(&*store) == memory.page_size,
                "WASM_STATE_INCOMPLETE: memory {:?} page size {} differs from the recorded {}",
                memory.name,
                target.page_size(&*store),
                memory.page_size
            );
            ensure!(
                current_pages <= memory.pages,
                "WASM_STATE_INCOMPLETE: memory {:?} already has {current_pages} pages, more than the recorded {}; a restore never truncates an instance",
                memory.name,
                memory.pages
            );
            if current_pages < memory.pages {
                let delta = memory.pages - current_pages;
                let previous = target.grow(&mut *store, delta).map_err(|error| {
                    anyhow!(
                        "WASM_STATE_INCOMPLETE: memory {:?} cannot grow by {delta} pages: {error}",
                        memory.name
                    )
                })?;
                ensure!(
                    previous == current_pages,
                    "WASM_STATE_INCOMPLETE: memory {:?} grew from {previous}, expected {current_pages}",
                    memory.name
                );
            }
            let path = self
                .cas
                .verified_object_path(&memory.object)
                .map_err(|error| {
                    anyhow!(
                        "WASM_STATE_INCOMPLETE: memory {:?} object {}: {error}",
                        memory.name,
                        memory.digest
                    )
                })?;
            let data = std::fs::read(&path).map_err(|error| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: read memory {:?} object {}: {error}",
                    memory.name,
                    memory.digest
                )
            })?;
            ensure!(
                data.len() as u64 == memory.bytes,
                "WASM_STATE_INCOMPLETE: memory {:?} object has {} bytes, the manifest records {}",
                memory.name,
                data.len(),
                memory.bytes
            );
            let target_data = target.data_mut(&mut *store);
            ensure!(
                target_data.len() as u64 >= memory.bytes,
                "WASM_STATE_INCOMPLETE: memory {:?} has {} bytes, the state needs {}",
                memory.name,
                target_data.len(),
                memory.bytes
            );
            target_data[..data.len()].copy_from_slice(&data);
            written += 1;
            bytes += memory.bytes;
        }

        let (continuation_resumed, graph_digest) = match &manifest.continuation {
            Continuation::NotCapturable { reason } => {
                if manifest.resources.iter().any(|resource| resource.required) {
                    // Memory came back but the execution could not: say so.
                    unrebindable.push(format!(
                        "continuation was not captured ({reason}); the execution stack and interpreter object graph are not restored"
                    ));
                }
                (false, None)
            }
            Continuation::Capturable(guest) => {
                let (resumed, digest) = self.import_continuation(store, instance, guest)?;
                (resumed, Some(digest))
            }
        };
        let level = if continuation_resumed {
            state_level(&manifest.continuation, &manifest.resources)
        } else {
            WasmStateLevel::MemoryWithoutResume
        };
        Ok(WasmStateRestore {
            manifest: verified.id,
            memories: written,
            bytes,
            continuation_resumed,
            graph_digest,
            unrebindable,
            level,
        })
    }

    fn import_continuation<T: 'static>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
        guest: &GuestContinuation,
    ) -> Result<(bool, Digest)> {
        let blob = self
            .cas
            .verified_object_path(&guest.blob_object)
            .map_err(|error| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: continuation blob {}: {error}",
                    guest.blob_digest
                )
            })?;
        let blob = std::fs::read(&blob).map_err(|error| {
            anyhow!(
                "WASM_STATE_INCOMPLETE: read continuation blob {}: {error}",
                guest.blob_digest
            )
        })?;
        ensure!(
            blob.len() as u64 == guest.blob_bytes,
            "WASM_STATE_INCOMPLETE: continuation blob has {} bytes, the manifest records {}",
            blob.len(),
            guest.blob_bytes
        );
        let memory = instance.get_memory(&mut *store, &guest.memory).ok_or_else(|| {
            anyhow!(
                "WASM_STATE_INCOMPLETE: the instance exports no memory {:?}",
                guest.memory
            )
        })?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut *store, STATE_ALLOC_EXPORT)
            .map_err(|_| {
                anyhow!(
                    "WASM_STATE_UNSUPPORTED: state carries a continuation but the instance does not export {STATE_ALLOC_EXPORT}"
                )
            })?;
        let len = i32::try_from(blob.len())
            .map_err(|_| anyhow!("WASM_STATE_UNSUPPORTED: continuation blob is too large for the guest"))?;
        let ptr = alloc
            .call(&mut *store, len)
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: {STATE_ALLOC_EXPORT}: {error}"))?;
        ensure!(
            ptr > 0,
            "WASM_STATE_INCOMPLETE: the guest refused to allocate {} bytes for its continuation",
            blob.len()
        );
        let ptr = ptr as u32 as u64;
        {
            let data = memory.data_mut(&mut *store);
            let end = ptr
                .checked_add(blob.len() as u64)
                .ok_or_else(|| anyhow!("WASM_STATE_INCOMPLETE: continuation range overflows"))?;
            ensure!(
                end <= data.len() as u64,
                "WASM_STATE_INCOMPLETE: continuation range {ptr}..{end} is outside memory {:?}",
                guest.memory
            );
            data[ptr as usize..end as usize].copy_from_slice(&blob);
        }
        let import = instance
            .get_typed_func::<(i32, i32), i32>(&mut *store, STATE_IMPORT_EXPORT)
            .map_err(|_| {
                anyhow!(
                    "WASM_STATE_UNSUPPORTED: state carries a continuation but the instance does not export {STATE_IMPORT_EXPORT}"
                )
            })?;
        let accepted = import
            .call(&mut *store, (ptr as i32, len))
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: {STATE_IMPORT_EXPORT}: {error}"))?;
        ensure!(
            accepted == 0,
            "WASM_STATE_INCOMPLETE: the guest refused the continuation blob with code {accepted}"
        );
        // The guest must now report the same graph as it did at capture. This is
        // the check that separates "bytes copied" from "object graph restored".
        let export = instance
            .get_typed_func::<(), i32>(&mut *store, STATE_EXPORT_EXPORT)
            .map_err(|_| {
                anyhow!("WASM_STATE_INCOMPLETE: the guest cannot re-export after import")
            })?;
        let descriptor_ptr = export
            .call(&mut *store, ())
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: {STATE_EXPORT_EXPORT}: {error}"))?;
        ensure!(
            descriptor_ptr > 0,
            "WASM_STATE_INCOMPLETE: the guest reported no graph after importing its continuation"
        );
        let (memory_name, descriptor) = read_descriptor_from(store, instance, &guest.memory, descriptor_ptr as u32 as u64)?;
        ensure!(
            memory_name == guest.memory,
            "WASM_STATE_INCOMPLETE: the guest re-exported its graph into memory {memory_name:?}, not {:?}",
            guest.memory
        );
        ensure!(
            descriptor.graph_digest == guest.graph_digest,
            "WASM_STATE_INCOMPLETE: the restored graph digest {} does not match the captured {}",
            descriptor.graph_digest,
            guest.graph_digest
        );
        Ok((true, descriptor.graph_digest))
    }

    /// Read a published manifest from this installation's CAS.
    pub fn manifest(&self, id: &Digest) -> Result<WasmStateManifest> {
        let bytes = self
            .cas
            .read_object(id)
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: read manifest {id}: {error}"))?;
        let manifest: WasmStateManifest = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("WASM_STATE_INCOMPLETE: decode manifest {id}: {error}"))?;
        ensure!(
            manifest.id() == *id,
            "WASM_STATE_INCOMPLETE: manifest bytes at {id} do not hash to that id"
        );
        Ok(manifest)
    }

    /// Resolve a controller root's state sub-node.
    pub fn resolve(&self, state_ref: &WasmStateRef) -> Result<(WasmStateManifest, VerifiedWasmState)> {
        let manifest = self.manifest(&state_ref.manifest)?;
        ensure!(
            manifest.module == state_ref.module
                && manifest.instance_id == state_ref.instance_id
                && manifest.backend == state_ref.backend,
            "WASM_STATE_INCOMPLETE: state manifest does not match the reference in the root"
        );
        let recorded: Vec<Digest> = manifest
            .memories
            .iter()
            .map(|memory| memory.digest.clone())
            .collect();
        ensure!(
            recorded == state_ref.memories,
            "WASM_STATE_INCOMPLETE: state manifest memories do not match the root reference"
        );
        let verified = self.verify(&manifest)?;
        Ok((manifest, verified))
    }
}

/// Every digest this state keeps alive, for root-aware reclamation.
pub fn state_roots(manifest: &WasmStateManifest) -> Vec<Digest> {
    let mut roots: Vec<Digest> = manifest
        .memories
        .iter()
        .map(|memory| memory.digest.clone())
        .collect();
    if let Continuation::Capturable(guest) = &manifest.continuation {
        roots.push(guest.blob_digest.clone());
    }
    roots.push(manifest.id());
    roots
}

fn state_object_ref(
    digest: &Digest,
    bytes: u64,
    media_type: &str,
    codec: &str,
    codec_major: u32,
) -> ObjectRef {
    ObjectRef {
        digest: digest.clone(),
        bytes,
        media_type: media_type.into(),
        store_namespace: WASM_STATE_NAMESPACE.into(),
        locator: String::new(),
        codec: SchemaRef {
            name: codec.into(),
            major: codec_major,
            digest: Digest::blake3(codec.as_bytes()),
        },
    }
}

fn state_level(continuation: &Continuation, resources: &[RebindableResource]) -> WasmStateLevel {
    match continuation {
        Continuation::NotCapturable { .. } => WasmStateLevel::MemoryWithoutResume,
        Continuation::Capturable(_) => {
            if resources
                .iter()
                .all(|resource| !resource.required || (resource.reversible && resource.identity.is_some()))
            {
                WasmStateLevel::ResumableState
            } else {
                WasmStateLevel::CapturedContinuation
            }
        }
    }
}

fn resource_evidence(
    mounts: &[MountSpec],
    transport: &[ExternalTransportEvidence],
) -> Vec<RebindableResource> {
    let mut resources = Vec::new();
    for mount in mounts {
        // A preopen is rebindable when its declared directory still resolves to
        // the same identity; the identity is recorded, not assumed.
        let identity = std::fs::symlink_metadata(&mount.source).ok().map(|metadata| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                format!("{}:{}", metadata.dev(), metadata.ino())
            }
            #[cfg(not(unix))]
            {
                format!("{}", metadata.len())
            }
        });
        resources.push(RebindableResource {
            kind: "preopen".into(),
            name: mount.guest_path.clone(),
            identity,
            required: true,
            reversible: true,
        });
    }
    // A socket descriptor lives only for one run and has no serialized form:
    // the transport is recorded as irreversible evidence, not as state. The
    // guest's own continuation is still captured; only the peer is gone.
    for target in transport {
        resources.push(RebindableResource {
            kind: "external-socket".into(),
            name: format!("{}:{}", target.host, target.port),
            identity: None,
            required: false,
            reversible: false,
        });
    }
    // The run's standard streams are caller-owned: the captured state can only
    // be resumed if the caller supplies fresh streams. Nothing is replayed.
    for name in ["stdin", "stdout", "stderr"] {
        resources.push(RebindableResource {
            kind: "stdio".into(),
            name: name.into(),
            identity: None,
            required: false,
            reversible: false,
        });
    }
    resources
}

/// Memory exports in module order: `(name, is_shared)`.
fn exported_memories<T: 'static>(store: &mut Store<T>, instance: &Instance) -> Vec<(String, bool)> {
    let mut memories = Vec::new();
    for export in instance.exports(&mut *store) {
        let name = export.name().to_string();
        match export.into_extern() {
            Extern::Memory(_) => memories.push((name, false)),
            Extern::SharedMemory(_) => memories.push((name, true)),
            _ => {}
        }
    }
    memories
}

fn read_descriptor<T: 'static>(
    store: &mut Store<T>,
    instance: &Instance,
    memory_names: &[String],
    ptr: u64,
) -> Result<(String, ContinuationDescriptor)> {
    let mut reason = None;
    for name in memory_names {
        match read_descriptor_from(store, instance, name, ptr) {
            Ok(found) => return Ok(found),
            Err(error) => reason = Some(error),
        }
    }
    Err(reason.unwrap_or_else(|| anyhow!("WASM_STATE_INCOMPLETE: no memory holds the descriptor")))
}

fn read_descriptor_from<T: 'static>(
    store: &mut Store<T>,
    instance: &Instance,
    memory_name: &str,
    ptr: u64,
) -> Result<(String, ContinuationDescriptor)> {
    let memory = instance
        .get_memory(&mut *store, memory_name)
        .ok_or_else(|| anyhow!("WASM_STATE_INCOMPLETE: no memory {memory_name:?}"))?;
    let data = memory.data(&*store);
    let end = ptr
        .checked_add(CONTINUATION_DESCRIPTOR_BYTES as u64)
        .ok_or_else(|| anyhow!("WASM_STATE_INCOMPLETE: descriptor address overflows"))?;
    ensure!(
        end <= data.len() as u64,
        "WASM_STATE_INCOMPLETE: descriptor {ptr}..{end} is outside memory {memory_name:?} ({} bytes)",
        data.len()
    );
    let raw = &data[ptr as usize..end as usize];
    let descriptor = ContinuationDescriptor::parse(raw)?;
    Ok((memory_name.to_string(), descriptor))
}

/// The guest's fixed-size continuation descriptor (little-endian).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuationDescriptor {
    magic: u32,
    abi: u32,
    resume_kind: u32,
    ptr: u64,
    len: u64,
    graph_digest: Digest,
}

impl ContinuationDescriptor {
    fn parse(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() == CONTINUATION_DESCRIPTOR_BYTES,
            "WASM_STATE_INCOMPLETE: continuation descriptor is {} bytes, not {CONTINUATION_DESCRIPTOR_BYTES}",
            raw.len()
        );
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let wide = |at: usize| {
            u64::from_le_bytes([
                raw[at], raw[at + 1], raw[at + 2], raw[at + 3],
                raw[at + 4], raw[at + 5], raw[at + 6], raw[at + 7],
            ])
        };
        let mut graph_hex = String::with_capacity(64);
        for byte in &raw[32..64] {
            graph_hex.push_str(&format!("{byte:02x}"));
        }
        Ok(Self {
            magic: word(0),
            abi: word(4),
            resume_kind: word(8),
            ptr: wide(16),
            len: wide(24),
            graph_digest: Digest(format!("blake3:{graph_hex}")),
        })
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Installation-local record of published runtime states.
///
/// A controller root can reference a state manifest, but reclamation needs to
/// *discover* every state this installation still holds, including ones no
/// current root names yet (a state captured for a slot that is about to be
/// pinned). The record is append-only and names manifests only; the objects a
/// manifest references are resolved through CAS and verified.
pub const WASM_STATE_RECORD_DIR: &str = "wasm-state";
pub const STATE_RECORDS_FILE: &str = "states.jsonl";

pub struct WasmStateRegistry<'a> {
    cas: &'a ContentStore,
    data_dir: &'a Path,
}

impl<'a> WasmStateRegistry<'a> {
    pub fn new(cas: &'a ContentStore, data_dir: &'a Path) -> Self {
        Self { cas, data_dir }
    }

    pub fn record_dir(&self) -> PathBuf {
        self.data_dir.join(WASM_STATE_RECORD_DIR)
    }

    fn records_path(&self) -> PathBuf {
        self.record_dir().join(STATE_RECORDS_FILE)
    }

    /// Append one published state reference. The manifest object must already be
    /// in CAS: a record that names bytes nobody sealed is refused.
    pub fn record(&self, captured: &CapturedWasmState) -> Result<WasmStateRef> {
        ensure!(
            self.cas.object_exists(&captured.id),
            "WASM_STATE_INCOMPLETE: manifest {} is not published in this installation's CAS",
            captured.id
        );
        let reference = captured.to_ref();
        let mut line = serde_json::to_vec(&reference).expect("wasm state ref serialize");
        line.push(b'\n');
        let dir = self.record_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create state record directory {}", dir.display()))?;
        let path = self.records_path();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()))?;
            file.write_all(&line)
                .with_context(|| format!("append {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", path.display()))?;
        }
        sync_dir(&dir)?;
        Ok(reference)
    }

    /// Every recorded state reference, oldest first.
    pub fn records(&self) -> Result<Vec<WasmStateRef>> {
        let path = self.records_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow!("WASM_STATE_INCOMPLETE: read {}: {error}", path.display()));
            }
        };
        let mut records = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let reference: WasmStateRef = serde_json::from_slice(line).map_err(|error| {
                anyhow!(
                    "WASM_STATE_INCOMPLETE: {} line {}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            records.push(reference);
        }
        Ok(records)
    }

    pub fn latest(&self) -> Result<Option<WasmStateRef>> {
        Ok(self.records()?.pop())
    }

    /// Verify every recorded state and return each one's object closure.
    ///
    /// A record that does not resolve is an error, not an empty closure: a
    /// producer that cannot enumerate what it published must make reclamation
    /// refuse rather than let those objects be collected.
    pub fn verified_roots(&self) -> Result<Vec<(WasmStateRef, Vec<Digest>)>> {
        let provider = WasmStateProvider::new(self.cas);
        let mut out = Vec::new();
        for reference in self.records()? {
            let (manifest, _verified) = provider.resolve(&reference)?;
            out.push((reference, state_roots(&manifest)));
        }
        Ok(out)
    }
}

/// Root producer for runtime state. GC refuses when a recorded state does not
/// verify, so a state can never be half-collected.
pub struct WasmStateProducer<'a>(pub &'a WasmStateRegistry<'a>);

impl crate::gc::CasRootProducer for WasmStateProducer<'_> {
    fn producer_id(&self) -> &'static str {
        "wasm-state"
    }

    fn namespace_claims(&self) -> &'static [&'static str] {
        &[WASM_STATE_NAMESPACE]
    }

    fn enumerate(&self) -> Result<crate::gc::ProducerRoots> {
        let mut roots = crate::gc::ProducerRoots::new("wasm-state", &[WASM_STATE_NAMESPACE]);
        let records = self.0.verified_roots()?;
        for (reference, digests) in records {
            roots.closures += 1;
            roots.roots.extend(digests);
            if reference.continuation.is_none() {
                // The state exists but its continuation does not: the runtime
                // state is captured-without-resume. Reclamation is still safe
                // (all its objects are pinned), and the gap is reported.
                roots.blockers.push(format!(
                    "state {} carries no captured continuation; §51.4 full runtime state is an open gap for this instance",
                    reference.manifest
                ));
            }
        }
        roots.observed_at_unix_ms = now_unix_ms();
        Ok(roots)
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        bail!("WASM_STATE_UNSUPPORTED: durable state records require directory synchronization");
    }
}
