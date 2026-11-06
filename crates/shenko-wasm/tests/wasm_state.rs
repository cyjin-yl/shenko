//! Immutable WASM runtime state: linear memory, the guest's own continuation
//! and the interpreter object graph (SPEC §10.2, §51.4).
//!
//! Every case here is a real wasmtime instance, real compiled WAT and a real
//! on-disk content-addressed store. The guest below is a tiny "interpreter":
//! `step` advances a program counter and writes a marker byte, `shenko_state_*`
//! is the cooperative continuation protocol. The decisive check is that a fresh
//! instance restored from the sealed state *continues* the interpreter — the
//! program counter, the marker bytes and the guest-computed graph digest all
//! come back — rather than only receiving a copy of a memory blob.
//!
//! Scenarios:
//!
//! 1. Capture seals memory + continuation; a restore resumes the guest's own
//!    execution at the captured instruction and the graph digest matches.
//! 2. A guest without the protocol is captured as memory-only and is never
//!    reported as resumed.
//! 3. A different module, a different backend identity and a destroyed memory
//!    object are all refused instead of being papered over.
//! 4. The published state record pins every state object for reclamation, and a
//!    state without a continuation is reported as a gap, not as success.

use anyhow::Result;
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_wasm::gc::{CasGc, CasRootProducer, GcOutcome};
use shenko_wasm::state::{
    state_roots, Continuation, WasmBackendIdentity, WasmStateLevel, WasmStateProvider,
    WasmStateRegistry, WasmStateProducer,
};
use shenko_wasm::{MountMode, MountSpec, WasiCommand, WasmRuntime, HOME_GUEST_PATH};
use std::fs;
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!(
        "shenko-wasm-state-{tag}-{}",
        shenko_types::ids::EntityId::new()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn no_stdio() -> WasiCommand<std::io::Empty, std::io::Empty, std::io::Empty> {
    WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty())
}

/// A tiny resumable interpreter.
///
/// * `$pc` is the interpreter's program counter (its "continuation").
/// * `step` advances `$pc` and writes a marker byte at `3000 + $pc`.
/// * `shenko_state_export` writes the fixed descriptor: the blob is `$pc`;
///   the graph digest is 32 bytes derived from `$pc`, so it changes exactly
///   when the restored continuation differs.
/// * `shenko_state_import` restores `$pc` from the imported blob.
const RESUMABLE_INTERPRETER_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $pc (mut i32) (i32.const 0))
  (global $bump (mut i32) (i32.const 8192))

  (func (export "shenko_state_abi") (result i32) (i32.const 1))

  ;; descriptor 1024..1088, blob 2048..2052, bump allocator from 8192.
  (func $write_graph_digest (local $i i32)
    (local.set $i (i32.const 0))
    (block $done
      (loop $loop
        (br_if $done (i32.ge_u (local.get $i) (i32.const 32)))
        (i32.store8
          (i32.add (i32.const 1056) (local.get $i))
          (i32.and (i32.add (global.get $pc) (local.get $i)) (i32.const 255)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop))))

  (func (export "shenko_state_export") (result i32)
    (i32.store (i32.const 1024) (i32.const 1263544403))   ;; "SHSK"
    (i32.store (i32.const 1028) (i32.const 1))            ;; abi
    (i32.store (i32.const 1032) (i32.const 7))            ;; resume_kind
    (i32.store (i32.const 1036) (i32.const 0))            ;; reserved
    (i64.store (i32.const 1040) (i64.const 2048))         ;; blob ptr
    (i64.store (i32.const 1048) (i64.const 4))            ;; blob len
    (i32.store (i32.const 2048) (global.get $pc))
    (call $write_graph_digest)
    (i32.const 1024))

  (func (export "shenko_state_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump (i32.add (local.get $ptr) (local.get $len)))
    (local.get $ptr))

  (func (export "shenko_state_import") (param $ptr i32) (param $len i32) (result i32)
    (if (i32.lt_u (local.get $len) (i32.const 4)) (then (return (i32.const 1))))
    (global.set $pc (i32.load (local.get $ptr)))
    (i32.const 0))

  (func (export "step") (result i32)
    (global.set $pc (i32.add (global.get $pc) (i32.const 1)))
    (i32.store8
      (i32.add (i32.const 3000) (global.get $pc))
      (i32.const 42))
    (global.get $pc))

  (func (export "pc") (result i32) (global.get $pc))
  (func (export "marker") (result i32)
    (i32.load8_u (i32.add (i32.const 3000) (global.get $pc))))
)
"#;

/// Same interpreter without the continuation protocol: the host can only see
/// linear memory, and must say so.
const MEMORY_ONLY_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $state (mut i32) (i32.const 0))
  (func (export "mutate") (result i32)
    (global.set $state (i32.add (global.get $state) (i32.const 1)))
    (i32.store (i32.const 128) (global.get $state))
    (global.get $state))
)
"#;

fn home_mount(home: &PathBuf) -> MountSpec {
    MountSpec::new(home.clone(), HOME_GUEST_PATH, MountMode::ReadWrite, true, false)
}

#[test]
fn captured_continuation_resumes_the_guests_own_execution() -> Result<()> {
    let home = temp_dir("resume-home");
    let cas = ContentStore::new(temp_dir("resume-cas"))?;
    let wat = RESUMABLE_INTERPRETER_WAT.as_bytes().to_vec();

    let mut runtime = WasmRuntime::with_mounts(vec![home_mount(&home)])?;
    let mut live = runtime.instantiate_stateful(&wat, no_stdio())?;
    for _ in 0..3 {
        runtime.call_stateful(&live, "step", &[])?;
    }
    let (pc,) = {
        let out = runtime.call_stateful(&live, "pc", &[])?;
        (out[0].unwrap_i32(),)
    };
    assert_eq!(pc, 3, "the interpreter ran three steps");

    let captured = runtime.capture_state(&mut live, &cas, &[])?;
    assert_eq!(captured.level, WasmStateLevel::ResumableState);
    match &captured.manifest.continuation {
        Continuation::Capturable(guest) => {
            assert_eq!(guest.resume_kind, 7);
            assert!(cas.object_exists(&guest.blob_digest));
        }
        other => panic!("expected a captured continuation, got {other:?}"),
    }
    assert_eq!(captured.manifest.memories.len(), 1);
    assert!(cas.object_exists(&captured.id));
    for digest in state_roots(&captured.manifest) {
        assert!(cas.object_exists(&digest), "state object {digest} is sealed");
    }

    // The live instance keeps running, so a restore cannot be confused with the
    // state that was captured.
    for _ in 0..3 {
        runtime.call_stateful(&live, "step", &[])?;
    }

    let (mut restored, receipt) =
        runtime.restore_state(&cas, &captured.manifest, &wat, no_stdio())?;
    assert!(receipt.continuation_resumed, "the guest must re-import and resume");
    assert_eq!(
        receipt.graph_digest.as_ref(),
        match &captured.manifest.continuation {
            Continuation::Capturable(guest) => Some(&guest.graph_digest),
            Continuation::NotCapturable { .. } => None,
        },
        "the restored object graph has the captured identity"
    );
    assert_eq!(receipt.memories, 1);
    assert!(
        receipt.unrebindable.iter().all(|entry| !entry.starts_with("preopen")),
        "the home preopen is rebindable: {:?}",
        receipt.unrebindable
    );

    // Continuation, not a memory copy: the guest still knows it is at step 3
    // and its marker byte came back with the memory.
    let out = runtime.call_stateful(&restored, "pc", &[])?;
    assert_eq!(out[0].unwrap_i32(), 3);
    let out = runtime.call_stateful(&restored, "marker", &[])?;
    assert_eq!(out[0].unwrap_i32(), 42, "the captured memory holds the marker");
    let out = runtime.call_stateful(&restored, "step", &[])?;
    assert_eq!(out[0].unwrap_i32(), 4, "execution continues from the captured point");
    Ok(())
}

#[test]
fn a_guest_without_the_protocol_is_memory_only_and_never_claimed_resumed() -> Result<()> {
    let home = temp_dir("memory-only-home");
    let cas = ContentStore::new(temp_dir("memory-only-cas"))?;
    let wat = MEMORY_ONLY_WAT.as_bytes();

    let mut runtime = WasmRuntime::with_mounts(vec![home_mount(&home)])?;
    let mut live = runtime.instantiate_stateful(wat, no_stdio())?;
    for _ in 0..2 {
        runtime.call_stateful(&live, "mutate", &[])?;
    }
    let captured = runtime.capture_state(&mut live, &cas, &[])?;
    assert_eq!(captured.level, WasmStateLevel::MemoryWithoutResume);
    match &captured.manifest.continuation {
        Continuation::NotCapturable { reason } => {
            assert!(
                reason.contains("continuation protocol"),
                "the reason names the missing mechanism: {reason}"
            );
        }
        other => panic!("a protocol-less guest must not claim a continuation: {other:?}"),
    }
    assert!(captured.to_ref().continuation.is_none());
    // Its memory is still a real captured object.
    let (memories,) = (captured.manifest.memories.len(),);
    assert_eq!(memories, 1);

    let (_restored, receipt) =
        runtime.restore_state(&cas, &captured.manifest, wat, no_stdio())?;
    assert!(!receipt.continuation_resumed);
    assert_eq!(receipt.level, WasmStateLevel::MemoryWithoutResume);
    Ok(())
}

#[test]
fn mismatched_module_backend_and_missing_objects_are_refused() -> Result<()> {
    let home = temp_dir("refuse-home");
    let cas = ContentStore::new(temp_dir("refuse-cas"))?;
    let wat = RESUMABLE_INTERPRETER_WAT.as_bytes().to_vec();

    let mut runtime = WasmRuntime::with_mounts(vec![home_mount(&home)])?;
    let mut live = runtime.instantiate_stateful(&wat, no_stdio())?;
    runtime.call_stateful(&live, "step", &[])?;
    let captured = runtime.capture_state(&mut live, &cas, &[])?;

    // A different module is refused before anything is overwritten.
    let other = MEMORY_ONLY_WAT.as_bytes();
    let refused = runtime.restore_state(&cas, &captured.manifest, other, no_stdio());
    assert!(refused.is_err(), "a different module must not accept the state");
    let message = format!("{:#}", refused.err().unwrap());
    assert!(
        message.contains("WASM_STATE_INCOMPLETE"),
        "the refusal carries a typed cause: {message}"
    );

    // A different backend identity is refused by verification.
    let mut foreign = captured.manifest.clone();
    foreign.backend = WasmBackendIdentity {
        engine: foreign.backend.engine.clone(),
        engine_version: "0".into(),
        snapshot_abi: foreign.backend.snapshot_abi,
        target: foreign.backend.target.clone(),
    };
    let provider = WasmStateProvider::new(&cas);
    let refused = provider.verify(&foreign);
    assert!(refused.is_err());
    assert!(format!("{:#}", refused.err().unwrap()).contains("WASM_STATE_BACKEND_MISMATCH"));

    // A destroyed memory object is refused, not restored as zeros.
    let memory = captured.manifest.memories[0].digest.clone();
    assert!(cas.remove_object(&memory)?);
    let refused = provider.verify(&captured.manifest);
    assert!(refused.is_err());
    assert!(format!("{:#}", refused.err().unwrap()).contains("missing or corrupt"));
    Ok(())
}

#[test]
fn published_state_records_pin_their_objects_and_report_missing_continuations() -> Result<()> {
    let home = temp_dir("record-home");
    let data = temp_dir("record-data");
    let cas = ContentStore::new(data.join("cas"))?;
    let wat = RESUMABLE_INTERPRETER_WAT.as_bytes().to_vec();

    let mut runtime = WasmRuntime::with_mounts(vec![home_mount(&home)])?;
    let mut live = runtime.instantiate_stateful(&wat, no_stdio())?;
    runtime.call_stateful(&live, "step", &[])?;
    let captured = runtime.capture_state(&mut live, &cas, &[])?;

    let registry = WasmStateRegistry::new(&cas, &data);
    let reference = registry.record(&captured)?;
    assert_eq!(registry.latest()?.as_ref(), Some(&reference));
    let producer = WasmStateProducer(&registry);
    let roots = producer.enumerate()?;
    assert!(roots.complete);
    assert!(roots.blockers.is_empty(), "a resumed-capable state is not a gap");
    for digest in state_roots(&captured.manifest) {
        assert!(roots.roots.contains(&digest), "{digest} is pinned");
    }

    // A protocol-less state is pinned too, but its missing continuation is
    // reported truthfully.
    let memory_only = MEMORY_ONLY_WAT.as_bytes();
    let mut plain = runtime.instantiate_stateful(memory_only, no_stdio())?;
    runtime.call_stateful(&plain, "mutate", &[])?;
    let captured_plain = runtime.capture_state(&mut plain, &cas, &[])?;
    registry.record(&captured_plain)?;
    let roots = producer.enumerate()?;
    assert!(roots.complete);
    assert_eq!(roots.blockers.len(), 1);
    assert!(roots.blockers[0].contains("open gap"));

    // The GC pins the state objects: an unreferenced orphan is collected while
    // every state object survives.
    let gc = CasGc::new(&cas, &data);
    gc.adopt_registry(&[&producer], &["seed-wasm-state"])?;
    let orphan = cas
        .seal_object(b"orphan", "application/octet-stream", "seed-wasm-state", "x", 1)
        .expect("seal orphan")
        .object
        .digest;
    match gc.reclaim(&[&producer])? {
        GcOutcome::Reclaimed(receipt) => {
            assert!(receipt.collected.iter().any(|entry| entry.digest == orphan));
            for digest in state_roots(&captured.manifest) {
                assert!(cas.object_exists(&digest), "state object {digest} survived GC");
            }
        }
        GcOutcome::Refused(refusal) => panic!("unexpected refusal: {refusal}"),
    }
    // A record whose manifest object is gone makes reclamation refuse instead
    // of treating the state as empty.
    assert!(cas.remove_object(&captured_plain.id)?);
    match gc.reclaim(&[&producer])? {
        GcOutcome::Refused(refusal) => {
            assert!(
                format!("{refusal}").contains("CAS_ROOT_PRODUCER_INCOMPLETE"),
                "unexpected refusal: {refusal}"
            );
        }
        GcOutcome::Reclaimed(_) => panic!("a missing manifest must refuse reclamation"),
    }
    let _ = EntityId::new();
    Ok(())
}
