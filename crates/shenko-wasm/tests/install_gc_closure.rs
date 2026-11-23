//! An installation's durable evidence closure survives root-aware reclamation.
//!
//! This is the Store-side counterpart to `tool_artifact_roots`: a real
//! installation Store (`shenko.db` + `cas`), real archived history rows, a
//! real first-E1 receipt, a real adopted root registry, a real exclusive
//! Store producer and a real reclamation. The installation is then reopened
//! and the same question/source/history/receipt facts are read back.
//!
//! The registry is adopted *before* any governed object exists, on purpose. An
//! object created before adoption has an unknown owner and is retained as
//! ungoverned, so only an object created after adoption proves that the
//! ledger's roots — not the adoption epoch — protected it.
//!
//! UNRUN fixture. This file was authored without building or executing it; the
//! integration CI must compile it and run
//! `cargo test -p shenko-wasm --test install_gc_closure` before any runtime
//! result is attributed to it.

use anyhow::{Context, Result, bail, ensure};
use shenko_store::Store;
use shenko_store::cas::ContentStore;
use shenko_store::roots::{CasReadPin, CasWriteFence, CasWriteLease, SqliteWriteLease};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};
use shenko_wasm::gc::{
    CasGc, GcOutcome, STORE_LEDGER_PRODUCER, StoreLedgerRoots,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

/// The one CAS namespace this fixture's installation writes under. A real
/// installation assembly must declare every namespace its Store can seal into;
/// this fixture declares exactly the set it uses, and the registry refuses an
/// uncovered namespace.
const NAMESPACES: &[&str] = &["install-gc-evidence"];

const QUESTION_TEXT: &str = "上次 Hydra auth refresh 并发问题最后改了哪些文件？";
const ANSWER_TEXT: &str = "改了 crates/hydra/src/auth.rs，并补了并发回归测试。";

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!("shenko-install-gc-{tag}-{}", EntityId::new()));
    fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create temp dir {}: {error}", dir.display()));
    dir
}

fn seal(
    cas: &ContentStore,
    bytes: &[u8],
    media_type: &str,
    codec: &str,
) -> Result<ObjectRef> {
    Ok(cas
        .seal_object(bytes, media_type, NAMESPACES[0], codec, 1)?
        .object)
}

/// The persisted fixture: the exact bytes every durable row names, and the
/// rows themselves.
struct Evidence {
    session_id: EntityId,
    session_data: String,
    events: Vec<(i64, String)>,
    receipt: EvidenceReceipt,
    objects: BTreeMap<Digest, Vec<u8>>,
}

fn record_event(record_id: &str, role: &str, text_ref: &ObjectRef) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "record_id": record_id,
        "role": role,
        "text_ref": text_ref,
        "parse_error": serde_json::Value::Null,
    }))?)
}

/// Archive a question and its answer through the real Store write paths:
/// sealed CAS objects first, then one raw-session snapshot and one first-E1
/// receipt that name them.
fn publish_evidence(store: &Store) -> Result<Evidence> {
    let session_id = EntityId::new();
    let agent_id = EntityId::new();
    let question_id = EntityId::new();
    let question_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "question_id": question_id,
        "session_id": session_id,
        "text": QUESTION_TEXT,
        "source": {"provider": "codex", "source_instance": "owner-work-history"},
    }))?;
    let question = seal(
        store.cas.as_ref(),
        &question_bytes,
        "application/json",
        "install-gc-question",
    )?;

    let source_bytes = format!("{QUESTION_TEXT}\n{ANSWER_TEXT}\n").into_bytes();
    let source = seal(
        store.cas.as_ref(),
        &source_bytes,
        "application/octet-stream",
        "install-gc-source",
    )?;

    let question_projection = seal(
        store.cas.as_ref(),
        QUESTION_TEXT.as_bytes(),
        "text/plain; charset=utf-8",
        "install-gc-history-projection",
    )?;
    let answer_projection = seal(
        store.cas.as_ref(),
        ANSWER_TEXT.as_bytes(),
        "text/plain; charset=utf-8",
        "install-gc-history-projection",
    )?;

    let session_data = serde_json::to_string(&serde_json::json!({
        "format": "shenko-external-history/1",
        "source": {
            "provider": "codex",
            "source_instance": "owner-work-history",
            "source_key": "session-1",
        },
        "archive": {"raw_ref": source, "record_count": 2},
    }))?;
    let session = RawSession {
        session_id,
        agent_id,
        started_at: String::new(),
        data: session_data.clone(),
    };
    let event_data = vec![
        (
            0i64,
            record_event(&EntityId::new().to_string(), "user", &question_projection)?,
        ),
        (
            1i64,
            record_event(&EntityId::new().to_string(), "assistant", &answer_projection)?,
        ),
    ];
    let events = event_data
        .iter()
        .map(|(seq, data)| RawSessionEvent {
            event_id: EntityId::new(),
            session_id,
            seq: *seq,
            event_type: "external_history_record".into(),
            role: "external_observation".into(),
            data: data.clone(),
        })
        .collect::<Vec<_>>();
    store.commit_raw_session_snapshot(&session, &events, std::slice::from_ref(&source.digest))?;

    let receipt = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: None,
        attempt_id: None,
        source_id: "external-history".into(),
        source_instance_id: "owner-work-history".into(),
        source_event_key: Some("external-history/session-1".into()),
        producer_epoch: Some(1),
        producer_schema_version: Some("1".into()),
        raw_ref: source.clone(),
        decoder_ref: None,
        decode_status: DecodeStatus::Decoded,
        observation_level: None,
        normalized_ref: Some(question.clone()),
        observed_at: None,
        received_commit: CommitRef::new(store.ledger.ledger_id(), LedgerSeq(0)),
    };
    let mut batch = WriteBatch::new();
    batch.evidence_upserts.push(receipt.clone());
    store.commit(batch)?;

    let mut objects = BTreeMap::new();
    objects.insert(question.digest.clone(), question_bytes);
    objects.insert(source.digest.clone(), source_bytes);
    objects.insert(question_projection.digest.clone(), QUESTION_TEXT.as_bytes().to_vec());
    objects.insert(answer_projection.digest.clone(), ANSWER_TEXT.as_bytes().to_vec());

    Ok(Evidence {
        session_id,
        session_data,
        events: event_data,
        receipt,
        objects,
    })
}

/// One orphan is sealed under the same declared namespace but named by no
/// durable row: it is only collectable because it was created after adoption.
fn seal_orphan(store: &Store, label: &str) -> Result<ObjectRef> {
    Ok(store
        .cas
        .seal_object(
            label.as_bytes(),
            "application/octet-stream",
            NAMESPACES[0],
            "install-gc-orphan",
            1,
        )?
        .object)
}

#[test]
fn installation_evidence_closure_survives_reclamation_and_orphans_are_collected_once() -> Result<()> {
    let installation = temp_dir("installation");
    let database = installation.join("shenko.db");
    let cas_directory = installation.join("cas");

    // A real installation Store: the standard `shenko.db` + `cas` layout, so
    // the physical installation domain (not just a bare database) is in play.
    let store = Store::open(database.clone(), cas_directory.clone())?;
    let evidence;
    {
        let gc = CasGc::new(store.cas.as_ref(), &installation);

        // Declare the installation's complete producer set and CAS namespace
        // set before any governed object exists.
        {
            let producer = StoreLedgerRoots::exclusive(&store, &database, NAMESPACES, 0)?;
            assert_eq!(producer.database(), database.as_path());
            let registry = gc.adopt_registry(&[&producer], NAMESPACES)?;
            assert_eq!(registry.producers, vec![STORE_LEDGER_PRODUCER.to_string()]);
            assert_eq!(registry.namespaces, vec![NAMESPACES[0].to_string()]);
        }

        // Seal and publish the evidence under the shared publication lease,
        // exactly as a Store writer must before naming a sealed object.
        let publication = CasWriteLease::acquire(store.cas.base())?;
        evidence = publish_evidence(&store)?;
        let orphans = vec![
            seal_orphan(&store, "unreferenced orphan one")?,
            seal_orphan(&store, "unreferenced orphan two")?,
            seal_orphan(&store, "unreferenced orphan three")?,
        ];
        // A bounded reader that already resolved these bytes: the pin is an
        // exclusion, so reclamation must retain the object while it is live.
        let pinned = seal_orphan(&store, "bytes held by a live bounded reader")?;
        let pin = CasReadPin::acquire(store.cas.as_ref(), [&pinned])?;
        drop(publication);

        let orphan_digests: BTreeSet<Digest> =
            orphans.iter().map(|object| object.digest.clone()).collect();

        // The producer owns the installation's exclusive CAS fence and its
        // SQLite write lease for the whole run. While it is alive, no other
        // writer can seal-and-publish a fresh root, and no second collector
        // can take the database.
        let producer = StoreLedgerRoots::exclusive(&store, &database, NAMESPACES, 0)?;
        ensure!(
            CasWriteFence::acquire(store.cas.base(), 0).is_err(),
            "the exclusive CAS fence must already be held by the Store producer"
        );
        ensure!(
            SqliteWriteLease::try_acquire(&database).is_err(),
            "the exclusive SQLite write lease must already be held by the Store producer"
        );

        // One plan built from the ledger rows: every published evidence object
        // is reachable, the pinned object is reachable through its live pin,
        // and the orphans are exactly the candidates.
        let plan = gc
            .plan(&[&producer])
            .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
        for digest in evidence.objects.keys() {
            ensure!(
                plan.reachable.contains(digest),
                "the ledger's evidence object {digest} is not a root"
            );
        }
        ensure!(
            plan.reachable.contains(&pinned.digest),
            "a live reader pin must be a root"
        );
        for orphan in &orphans {
            ensure!(
                plan.candidates.contains(&orphan.digest),
                "unreferenced object {} is not a candidate",
                orphan.digest
            );
        }
        ensure!(
            !plan.candidates.contains(&pinned.digest),
            "a pinned object must never be a candidate"
        );
        drop(plan);

        let receipt = match gc.reclaim(&[&producer])? {
            GcOutcome::Reclaimed(receipt) => receipt,
            GcOutcome::Refused(refusal) => bail!("a complete declaration was refused: {refusal}"),
        };
        assert_eq!(receipt.producers, vec![STORE_LEDGER_PRODUCER.to_string()]);
        assert_eq!(
            receipt.adopted_at_unix_ms,
            gc.registry()?.context("registry")?.adopted_at_unix_ms
        );
        assert!(receipt.failed.is_empty(), "{:?}", receipt.failed);
        assert!(receipt.retained_ungoverned.is_empty(), "{:?}", receipt.retained_ungoverned);
        assert!(receipt.retained_concurrent.is_empty());
        assert!(
            receipt.reachable > evidence.objects.len(),
            "the plan must retain the evidence closure and the pin"
        );

        // Orphans are collected exactly once: the exact set, no duplicates,
        // and the exact byte total.
        let collected: BTreeSet<Digest> = receipt
            .collected
            .iter()
            .map(|entry| entry.digest.clone())
            .collect();
        assert_eq!(
            collected.len(),
            receipt.collected.len(),
            "an object must not be collected twice in one run"
        );
        assert_eq!(collected, orphan_digests);
        let orphan_bytes = orphans.iter().map(|object| object.bytes).sum::<u64>();
        assert_eq!(receipt.collected_bytes, orphan_bytes);
        for orphan in &orphans {
            ensure!(
                !store.cas.object_exists(&orphan.digest),
                "collected object {} still exists",
                orphan.digest
            );
        }
        for digest in evidence.objects.keys() {
            ensure!(
                store.cas.object_exists(digest),
                "evidence object {digest} was collected"
            );
        }
        ensure!(
            store.cas.object_exists(&pinned.digest),
            "the pinned object was collected while a reader held it"
        );

        // Releasing the producer releases both exclusions.
        drop(producer);
        drop(CasWriteFence::acquire(store.cas.base(), 0)?);
        drop(SqliteWriteLease::try_acquire(&database)?);

        // A second run over the same committed installation collects nothing.
        let producer = StoreLedgerRoots::exclusive(&store, &database, NAMESPACES, 0)?;
        let second = match gc.reclaim(&[&producer])? {
            GcOutcome::Reclaimed(receipt) => receipt,
            GcOutcome::Refused(refusal) => bail!("the retained installation was refused: {refusal}"),
        };
        assert!(
            second.collected.is_empty(),
            "objects were collected twice: {:?}",
            second.collected
        );
        assert_eq!(second.objects_before, receipt.objects_before - orphans.len());
        assert_eq!(second.reachable, receipt.reachable);
        drop(producer);

        // Ending the reader ends the pin's protection: the same object is now
        // unreferenced and collected exactly once.
        drop(pin);
        let producer = StoreLedgerRoots::exclusive(&store, &database, NAMESPACES, 0)?;
        let third = match gc.reclaim(&[&producer])? {
            GcOutcome::Reclaimed(receipt) => receipt,
            GcOutcome::Refused(refusal) => bail!("the unpinned store was refused: {refusal}"),
        };
        assert_eq!(third.collected.len(), 1, "{:?}", third.collected);
        assert_eq!(third.collected[0].digest, pinned.digest);
        assert_eq!(third.collected_bytes, pinned.bytes);
        drop(producer);

        // The final CAS inventory is exactly the original evidence closure.
        let expected: BTreeSet<Digest> = evidence.objects.keys().cloned().collect();
        let inventory: BTreeSet<Digest> = store.cas.object_digests()?.into_iter().collect();
        assert_eq!(inventory, expected);
    }
    drop(store);

    // Reopen the same installation and read the original facts back: the
    // question text, the archived source version, its history records and the
    // first-E1 receipt.
    let reopened = Store::open(database.clone(), cas_directory.clone())?;
    let session = reopened
        .raw_session(evidence.session_id)?
        .context("archived source session was lost")?;
    assert_eq!(session.data, evidence.session_data);

    let events = reopened.session_events_range(evidence.session_id, 0, i64::MAX)?;
    assert_eq!(events.len(), evidence.events.len());
    for (stored, (expected_seq, expected_data)) in events.iter().zip(&evidence.events) {
        assert_eq!(stored.seq, *expected_seq);
        assert_eq!(&stored.data, expected_data);
    }

    let stored_receipt = reopened.external_evidence(evidence.receipt.receipt_id)?;
    assert_eq!(stored_receipt.receipt_id, evidence.receipt.receipt_id);
    assert_eq!(stored_receipt.source_id, evidence.receipt.source_id);
    assert_eq!(stored_receipt.source_instance_id, evidence.receipt.source_instance_id);
    assert_eq!(stored_receipt.source_event_key, evidence.receipt.source_event_key);
    assert_eq!(stored_receipt.decode_status, evidence.receipt.decode_status);
    assert_eq!(stored_receipt.raw_ref, evidence.receipt.raw_ref);
    assert_eq!(stored_receipt.normalized_ref, evidence.receipt.normalized_ref);

    for (digest, bytes) in &evidence.objects {
        assert_eq!(
            &reopened.cas.read_object(digest)?,
            bytes,
            "original bytes of {digest} changed"
        );
        assert!(reopened.cas.object_exists(digest));
    }
    Ok(())
}
