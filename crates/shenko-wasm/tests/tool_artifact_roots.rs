//! The tool-artifact CAS root producer: an installed tool's chunk manifest
//! and chunk objects are roots of this installation's content store, an
//! unrelated object in the same namespace is not.
//!
//! Everything here is real: a real node installation, a real WASI preview1
//! module whose bytes exceed the artifact chunk size, a real content store
//! that stages it as several chunk objects plus a chunk manifest, and a real
//! reclamation over that store.
//!
//! The registry is adopted *before* the artifact is staged, on purpose. An
//! object created before adoption has an unknown owner and is retained as
//! ungoverned, so only an object created after adoption proves that the
//! producer's roots — not the adoption epoch — protected it.

use anyhow::{Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};
use shenko_node::{Node, UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use shenko_wasm::artifact::{
    ArtifactPointer, CasToolContentStore, ToolArtifactRef, ToolArtifactRoots, ARTIFACT_NAMESPACE,
    TOOL_ARTIFACT_PRODUCER, read_chunk_manifest, read_staged_artifact, stage_artifact_file,
    stream_sha256,
};
use shenko_wasm::gc::{
    CAS_ROOTS_DIR, CasGc, CasRootProducer, GcOutcome, GcRefusalReason, REGISTRY_FILE,
    RecordedRoots, RootRegistry, write_root_record,
};
use std::fs;
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!(
        "shenko-wasm-tool-artifact-{tag}-{}",
        EntityId::new()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A real WASI preview1 command module whose bytes exceed the artifact chunk
/// size, so staging it writes more than one chunk object.
fn multi_chunk_module() -> Vec<u8> {
    let payload = "x".repeat(70 * 1024);
    let wat = format!(
        r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
          (memory (export "memory") 2)
          (data (i32.const 100) "{payload}")
          (func (export "_start")
            (i32.store (i32.const 0) (i32.const 100))
            (i32.store (i32.const 4) (i32.const {len}))
            (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
            (call $exit (i32.const 0))))
        "#,
        len = payload.len()
    );
    wat::parse_str(wat).expect("fixture module must assemble")
}

fn declaration_refusal_preserves_objects(
    cas: &ContentStore,
    gc: &CasGc<'_>,
    producers: &[&dyn CasRootProducer],
    expected_detail: &str,
) -> Result<()> {
    let before = cas.object_digests()?;
    let refusal = match gc.reclaim(producers)? {
        GcOutcome::Refused(refusal) => refusal,
        outcome => bail!("inconsistent root declarations reached reclamation: {outcome:?}"),
    };
    ensure!(
        refusal.reason == GcRefusalReason::RegistryInvalid
            && refusal.detail.contains(expected_detail),
        "wrong declaration refusal: {refusal}"
    );
    assert_eq!(
        cas.object_digests()?, before,
        "a refused declaration must preserve roots and collectable orphans"
    );
    Ok(())
}

/// The installed tool's bytes and its chunk manifest survive reclamation,
/// while an unrelated object in the same namespace is collected — and the
/// receipt names the exact producer set that decided the run.
#[test]
fn installed_tool_bytes_are_roots_and_an_unrelated_orphan_is_collected() -> Result<()> {
    let root = temp_dir("roots");
    let data = root.join("data");
    fs::create_dir_all(&data)?;
    let cas = ContentStore::new(data.join("cas"))?;
    let store = CasToolContentStore::new(&cas);
    let gc = CasGc::new(&cas, &data);

    let mut node = Node::open(&root.join("node"))?;
    node.handle(shenko_node::Request::Initialize {
        node_id: EntityId::new(),
        identity: None,
    })?;

    // Adoption declares this installation's complete namespace set. The
    // tool-artifact namespace is covered because a producer claims it: the
    // registry would refuse an uncovered namespace, and it still does.
    {
        let producer = ToolArtifactRoots::new(&node, &store);
        let refused = gc
            .adopt_registry(
                &[&producer],
                &[ARTIFACT_NAMESPACE, "uncovered-namespace"],
            )
            .expect_err("a namespace no producer enumerates must be refused");
        assert!(
            format!("{refused:#}").contains("CAS_ROOT_NAMESPACE_UNCOVERED"),
            "unexpected refusal: {refused:#}"
        );
        let registry = gc.adopt_registry(&[&producer], &[ARTIFACT_NAMESPACE])?;
        assert_eq!(registry.namespaces, vec![ARTIFACT_NAMESPACE.to_string()]);
        assert_eq!(registry.producers, vec![TOOL_ARTIFACT_PRODUCER.to_string()]);
    }

    // Stage a real multi-chunk module into the content store.
    let module = multi_chunk_module();
    let module_path = root.join("chunky.wasm");
    fs::write(&module_path, &module)?;
    let sha256 = {
        let mut file = fs::File::open(&module_path)?;
        stream_sha256(&mut file)?
    };
    let staged = stage_artifact_file(&store, &module_path, Some(sha256))?;
    assert!(
        staged.chunks >= 2,
        "the fixture must stage more than one chunk, staged {}",
        staged.chunks
    );

    // The installed unit carries the pointer document; the module bytes stay
    // in the content store as chunks described by the chunk manifest.
    let artifact = ToolArtifactRef::content_store(
        staged.digest.clone(),
        staged.bytes,
        staged.sha256.clone(),
        staged.manifest.clone(),
    );
    let pointer_bytes = ArtifactPointer::new(artifact.clone()).to_bytes()?;
    let unit_artifact_digest = Digest::blake3(&pointer_bytes);
    let name = "fixture.chunky";
    let version = "1.0.0";
    let unit_id = format!("tool.{name}");
    let export = UnitExport {
        schema: shenko_node::UNIT_SCHEMA,
        root_unit_id: unit_id.clone(),
        root_revision: Some(version.to_string()),
        root_artifact_digest: Some(unit_artifact_digest.clone()),
        origin_node: None,
        enabled: true,
        packages: vec![UnitPackage {
            unit: UnitDescriptor {
                unit_id: unit_id.clone(),
                kind: UnitKind::Code,
                revision: version.to_string(),
                artifact_digest: unit_artifact_digest,
                provides: vec![format!("tool:{name}")],
                dependencies: vec![],
                config: serde_json::json!({"tool": {
                    "name": name,
                    "version": version,
                    "abi": "wasi_preview1",
                    "entrypoint": format!("/tools/{name}"),
                    "source": {
                        "uri": "builtin:test",
                        "release": version,
                        "provenance": "test_fixture"
                    },
                    "imports": [
                        "wasi_snapshot_preview1.fd_write",
                        "wasi_snapshot_preview1.proc_exit"
                    ],
                    "artifact": artifact,
                }}),
                applicability: None,
            },
            artifact_base64: B64_STD.encode(&pointer_bytes),
            evidence: vec![],
            resolved_dependencies: None,
        }],
    };
    node.handle_unit_request(UnitRequest::Import {
        export,
        package_root: root.join("tools").to_string_lossy().into_owned(),
        enable: Some(true),
    })?;

    // An unrelated object in the same namespace: no installed tool claims it.
    let orphan = cas
        .seal_object(
            b"an object no installed tool claims",
            "application/octet-stream",
            ARTIFACT_NAMESPACE,
            TOOL_ARTIFACT_PRODUCER,
            1,
        )
        .expect("seal orphan")
        .object
        .digest;

    let manifest = read_chunk_manifest(&store, &staged.manifest)?;
    let chunk_digests: Vec<Digest> = manifest
        .objects
        .iter()
        .map(|object| object.digest.clone())
        .collect();
    assert_eq!(chunk_digests.len(), staged.chunks);

    let producer = ToolArtifactRoots::new(&node, &store);

    // Five distinct declaration failures share this installed tool and its
    // collectable orphan. Every refusal must leave the whole CAS intact.
    // First, a duplicate in the persisted registry cannot be normalized away.
    let registry_path = data.join(CAS_ROOTS_DIR).join(REGISTRY_FILE);
    let registry_bytes = fs::read(&registry_path)?;
    let mut malformed: RootRegistry = serde_json::from_slice(&registry_bytes)?;
    malformed.producers.push(TOOL_ARTIFACT_PRODUCER.into());
    fs::write(&registry_path, serde_json::to_vec(&malformed)?)?;
    declaration_refusal_preserves_objects(&cas, &gc, &[&producer], "sorted and unique")?;
    fs::write(&registry_path, &registry_bytes)?;

    // An extra participant has no namespace overlap; the old planner could
    // silently skip it. Its missing record must not be mistaken for coverage.
    let extra = RecordedRoots::new(data.clone(), "undeclared-retainer", &[]);
    declaration_refusal_preserves_objects(
        &cas, &gc, &[&producer, &extra], "not in the adopted registry",
    )?;

    // A registered producer cannot silently widen the adopted namespace set.
    let expanded = RecordedRoots::new(
        data.clone(), TOOL_ARTIFACT_PRODUCER, &[ARTIFACT_NAMESPACE, "undeclared-namespace"],
    );
    declaration_refusal_preserves_objects(
        &cas, &gc, &[&expanded], "outside the adopted registry",
    )?;

    // A durable record must report the same namespace claims as the actual
    // producer, even when it still contains every real tool digest.
    let mut recorded_roots = producer.enumerate()?;
    let expected_roots = recorded_roots.roots.clone();
    let recorded = RecordedRoots::new(data.clone(), TOOL_ARTIFACT_PRODUCER, &[ARTIFACT_NAMESPACE]);
    recorded_roots.namespaces = vec!["different-namespace".into()];
    write_root_record(&data, &recorded_roots)?;
    declaration_refusal_preserves_objects(
        &cas, &gc, &[&recorded], "enumerated namespaces",
    )?;

    // Deduplicating the record's report would conceal a malformed complete
    // declaration, so an otherwise equal namespace set is also refused.
    recorded_roots.namespaces = vec![ARTIFACT_NAMESPACE.into(), ARTIFACT_NAMESPACE.into()];
    write_root_record(&data, &recorded_roots)?;
    declaration_refusal_preserves_objects(
        &cas, &gc, &[&recorded], "without duplicates",
    )?;

    // A corrected durable declaration is usable again without changing its
    // roots. The original real-producer reclamation and byte checks follow.
    recorded_roots.namespaces = vec![ARTIFACT_NAMESPACE.into()];
    write_root_record(&data, &recorded_roots)?;
    let recorded_plan = gc.plan(&[&recorded])?;
    assert_eq!(recorded_plan.reachable, expected_roots);
    assert!(recorded_plan.candidates.contains(&orphan));
    drop(recorded_plan);

    match gc.reclaim(&[&producer])? {
        GcOutcome::Reclaimed(receipt) => {
            // The receipt names the exact producer set that decided this run.
            assert_eq!(receipt.producers, vec![TOOL_ARTIFACT_PRODUCER.to_string()]);
            // The producer's roots are the chunk manifest and its chunks — the
            // artifact's own identity names no object in the store.
            assert_eq!(receipt.reachable, chunk_digests.len() + 1);
            assert_eq!(receipt.objects_before, staged.chunks + 2);
            for digest in std::iter::once(&staged.manifest.digest).chain(chunk_digests.iter()) {
                assert!(
                    cas.object_exists(digest),
                    "the installed tool's object {digest} must survive reclamation"
                );
                assert!(
                    !receipt.collected.iter().any(|entry| entry.digest == *digest),
                    "the installed tool's object {digest} must not be collected"
                );
                assert!(
                    !receipt.retained_ungoverned.contains(digest),
                    "the installed tool's object {digest} must be a root, not an ungoverned leftover"
                );
            }
            // The unrelated orphan in the same namespace is collectable.
            assert!(
                receipt.collected.iter().any(|entry| entry.digest == orphan),
                "the unrelated orphan must be collected"
            );
            assert!(
                !cas.object_exists(&orphan),
                "the unrelated orphan must be gone from the store"
            );
            // The surviving bytes still reassemble to the installed module.
            let bytes = read_staged_artifact(&store, &staged.manifest, &staged.digest, staged.bytes)?;
            assert_eq!(
                bytes, module,
                "the installed tool's bytes survive reclamation unchanged"
            );
            let inspection = shenko_wasm::inspect_module(&bytes)?;
            assert!(
                inspection.runnable,
                "the surviving bytes are still a real preview1 command"
            );
        }
        GcOutcome::Refused(refusal) => panic!("unexpected refusal: {refusal}"),
    }
    Ok(())
}
