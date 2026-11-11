//! Authored notebook recovery — docs/specs/context-policies.md#spec-12-3-1, docs/specs/nodes.md#spec-5-4.
//!
//! The working-note journal and the scope's rollover request are authored
//! facts, not transient material: a declared recovery must preserve the exact
//! revision identities (including a clearing revision and a fork copy) and the
//! committed window that answered a rollover. The rollover request is local
//! control state, so only the declared same-installation handover may carry
//! it; a peer archive can neither arm a rollover nor answer one.

use std::sync::Arc;

use shenko_context_policy::{
    NoteSource, NotesJournal, RolloverJournal, StoreNotesJournal, StoreRolloverJournal,
    MAX_NOTE_BYTES,
};
use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use shenko_store::store::Store;
use shenko_store::{ContextNoteAppend, ContextWindowAppend, StoredContextWindow};
use shenko_types::ids::EntityId;

fn root(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-notebook-{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(dir: &std::path::Path) -> Arc<Store> {
    Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap())
}

fn archive(store: &Store, handover: bool) -> (Vec<u8>, shenko_store::CognitionReceipt, Vec<String>) {
    let export = store
        .export_cognition(&CognitionSnapshotOptions {
            local_boundary_preserved: handover,
            ..Default::default()
        })
        .unwrap();
    let classes = export.manifest.fact_classes.clone();
    let mut bytes = Vec::new();
    let receipt = export.write_archive(&mut bytes).unwrap();
    (bytes, receipt, classes)
}

fn revisions(journal: &StoreNotesJournal, limit: u64) -> Vec<(u64, String, NoteSource, bool)> {
    journal
        .revisions(limit)
        .unwrap()
        .into_iter()
        .map(|note| (note.revision, note.text, note.source, note.visible))
        .collect()
}

#[test]
fn authored_notes_clear_and_fork_survive_peer_recovery_and_reopen() {
    let source_dir = root("peer-source");
    let receiver_dir = root("peer-receiver");
    let source = open(&source_dir);
    let scope = EntityId::new();
    let journal = StoreNotesJournal::new(Arc::clone(&source), scope);
    assert_eq!(journal.replace(0, "first working state", NoteSource::Tool).unwrap().revision, 1);
    let cleared = journal.clear(1).unwrap();
    assert_eq!((cleared.revision, cleared.visible), (2, false));
    assert!(journal.visible().unwrap().is_none(), "a cleared notebook is not shown");
    assert_eq!(
        journal.replace(2, "state after reset", NoteSource::Tool).unwrap().revision,
        3
    );
    // A fork copies the visible notebook once, under its own scope identity.
    let fork_scope = EntityId::new();
    let fork = StoreNotesJournal::new(Arc::clone(&source), fork_scope);
    let adopted = fork.adopt_from(&journal).unwrap().unwrap();
    assert_eq!((adopted.revision, adopted.source), (1, NoteSource::Fork));

    let (bytes, receipt, classes) = archive(&source, false);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);
    assert!(receipt.gaps.is_empty(), "gaps: {:?}", receipt.gaps);
    assert!(classes.iter().any(|class| class == "context_notes"));
    assert!(
        !classes.iter().any(|class| class == "context_rollover_requests"),
        "a peer snapshot must not claim local control facts: {classes:?}"
    );
    let source_fingerprint = source
        .cognition_fingerprint(&CognitionSnapshotOptions::default())
        .unwrap()
        .0;

    let receiver = open(&receiver_dir);
    let outcome = receiver
        .import_cognition_archive(&bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "conflicts: {:?}", outcome.conflicts);
    assert!(outcome.gaps.is_empty(), "gaps: {:?}", outcome.gaps);
    assert!(outcome.ack(EntityId::new()).verified);

    let recovered = StoreNotesJournal::new(Arc::clone(&receiver), scope);
    assert_eq!(
        revisions(&recovered, 10),
        vec![
            (1, "first working state".to_string(), NoteSource::Tool, true),
            (2, String::new(), NoteSource::Reset, false),
            (3, "state after reset".to_string(), NoteSource::Tool, true),
        ],
        "the recovered journal keeps the clear and the exact revision numbers"
    );
    assert_eq!(recovered.visible().unwrap().unwrap().text, "state after reset");
    let recovered_fork = StoreNotesJournal::new(Arc::clone(&receiver), fork_scope);
    assert_eq!(
        revisions(&recovered_fork, 10),
        vec![(1, "state after reset".to_string(), NoteSource::Fork, true)],
        "a fork copy keeps its own scope and its fork provenance"
    );
    assert_eq!(
        receiver
            .cognition_fingerprint(&CognitionSnapshotOptions::default())
            .unwrap()
            .0,
        source_fingerprint,
        "the receiver holds the same recovery content as the sender"
    );

    // Reopen: the journal is durable store state, not an in-memory import.
    drop(recovered);
    drop(recovered_fork);
    drop(receiver);
    let reopened = open(&receiver_dir);
    let reopened_journal = StoreNotesJournal::new(Arc::clone(&reopened), scope);
    let head = reopened_journal.head().unwrap().unwrap();
    assert_eq!((head.revision, head.text.as_str(), head.source), (3, "state after reset", NoteSource::Tool));

    // A replay never duplicates, renumbers or rewrites an authored revision.
    let replay = reopened
        .import_cognition_archive(&bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(replay.conflicts.is_empty(), "conflicts: {:?}", replay.conflicts);
    assert_eq!(replay.inserted, 0);
    assert_eq!(revisions(&reopened_journal, 10).len(), 3);

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

/// Authored note bytes are immutable content (docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-4): recovery
/// copies them exactly — multibyte text at the journal bound and a body that
/// merely looks like a digest reference — and a digest-looking body is a
/// non-blocking external-digest reading, never a reason to drop or rewrite
/// the note.
#[test]
fn note_body_bytes_survive_recovery_byte_for_byte() {
    let source_dir = root("bytes-source");
    let receiver_dir = root("bytes-receiver");
    let source = open(&source_dir);
    let multibyte_scope = EntityId::new();
    let multibyte = "汉".repeat(MAX_NOTE_BYTES / 3);
    assert!(multibyte.len() <= MAX_NOTE_BYTES);
    let journal = StoreNotesJournal::new(Arc::clone(&source), multibyte_scope);
    journal.replace(0, &multibyte, NoteSource::Tool).unwrap();
    let digest_looking_scope = EntityId::new();
    let digest_looking = format!("blake3:{}", "0".repeat(64));
    let digest_journal = StoreNotesJournal::new(Arc::clone(&source), digest_looking_scope);
    digest_journal.replace(0, &digest_looking, NoteSource::Tool).unwrap();
    let spaced = "  leading and trailing whitespace\n\n second line  ";
    let spaced_scope = EntityId::new();
    StoreNotesJournal::new(Arc::clone(&source), spaced_scope)
        .replace(0, spaced, NoteSource::Tool)
        .unwrap();

    let (bytes, receipt, _) = archive(&source, false);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);
    assert!(
        receipt
            .gaps
            .iter()
            .any(|gap| gap.kind == "external_artifact" && !gap.blocking),
        "a digest-looking note body is a non-blocking external digest reading: {:?}",
        receipt.gaps
    );

    let receiver = open(&receiver_dir);
    let outcome = receiver
        .import_cognition_archive(&bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "conflicts: {:?}", outcome.conflicts);
    drop(receiver);

    let reopened = open(&receiver_dir);
    for (scope, expected) in [
        (multibyte_scope, multibyte.as_str()),
        (digest_looking_scope, digest_looking.as_str()),
        (spaced_scope, spaced),
    ] {
        let head = StoreNotesJournal::new(Arc::clone(&reopened), scope)
            .head()
            .unwrap()
            .unwrap();
        assert_eq!(head.text.as_bytes(), expected.as_bytes());
        assert_eq!(head.text.len(), expected.len());
    }

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

#[test]
fn pending_and_answered_rollover_control_survive_the_declared_handover() {
    let source_dir = root("handover-source");
    let receiver_dir = root("handover-receiver");
    let other_dir = root("handover-other");
    let source = open(&source_dir);

    // One scope records the answer to its request; another keeps a pending one.
    let answered_scope = EntityId::new();
    let answered = StoreRolloverJournal::new(Arc::clone(&source), answered_scope);
    let armed = answered.request(1_760_000_000_000).unwrap();
    let window_id = EntityId::new();
    assert_eq!(
        source
            .append_context_window(
                answered_scope,
                None,
                Some(0),
                &StoredContextWindow {
                    window_id,
                    generation: 0,
                    body: "{\"entries\":[]}".into(),
                    derivatives: Vec::new(),
                },
            )
            .unwrap(),
        ContextWindowAppend::Appended
    );
    assert!(answered.consume(window_id).unwrap());
    assert!(answered.pending().unwrap().is_none());

    let pending_scope = EntityId::new();
    let pending = StoreRolloverJournal::new(Arc::clone(&source), pending_scope);
    let pending_request = pending.request(1_760_000_002_000).unwrap();
    let pending_notes = StoreNotesJournal::new(Arc::clone(&source), pending_scope);
    assert_eq!(
        pending_notes.replace(0, "pending scope notebook", NoteSource::Tool).unwrap().revision,
        1
    );

    let (bytes, receipt, classes) = archive(&source, true);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);
    assert!(classes.iter().any(|class| class == "context_rollover_requests"));
    assert!(classes.iter().any(|class| class == "context_notes"));

    let receiver = open(&receiver_dir);
    let outcome = receiver
        .import_cognition_archive(
            &bytes[..],
            &CognitionImportOptions {
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "conflicts: {:?}", outcome.conflicts);
    assert!(outcome.gaps.is_empty(), "gaps: {:?}", outcome.gaps);
    assert!(outcome.ack(EntityId::new()).verified);

    {
        // The answered audit names the exact committed window this scope cut.
        let stored = receiver.context_rollover_request(answered_scope).unwrap().unwrap();
        assert_eq!(stored.request_id, armed.request_id);
        assert_eq!(stored.consumed_window, Some(window_id));
        let windows = receiver.context_windows(answered_scope).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_id, window_id);
        let answered_journal = StoreRolloverJournal::new(Arc::clone(&receiver), answered_scope);
        assert!(answered_journal.pending().unwrap().is_none());

        // The pending request arrives as the local control fact it was, not
        // as a peer claim, and the notebook of that scope arrives with it.
        let recovered_pending = StoreRolloverJournal::new(Arc::clone(&receiver), pending_scope);
        assert_eq!(
            recovered_pending.pending().unwrap().unwrap().request_id,
            pending_request.request_id
        );
        let recovered_notes = StoreNotesJournal::new(Arc::clone(&receiver), pending_scope);
        assert_eq!(
            revisions(&recovered_notes, 10),
            vec![(1, "pending scope notebook".to_string(), NoteSource::Tool, true)]
        );
        drop(recovered_notes);
        drop(recovered_pending);
        drop(answered_journal);
    }

    // Reopen: the control facts are durable, not an import side effect.
    drop(receiver);
    let reopened = open(&receiver_dir);
    assert_eq!(
        reopened
            .context_rollover_request(answered_scope)
            .unwrap()
            .unwrap()
            .consumed_window,
        Some(window_id)
    );
    assert_eq!(
        StoreRolloverJournal::new(Arc::clone(&reopened), pending_scope)
            .pending()
            .unwrap()
            .unwrap()
            .request_id,
        pending_request.request_id
    );

    // A replay of the same handover is idempotent.
    let replay = reopened
        .import_cognition_archive(
            &bytes[..],
            &CognitionImportOptions {
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(replay.conflicts.is_empty(), "conflicts: {:?}", replay.conflicts);
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.updated, 0);

    // A peer snapshot of the same store carries neither the class nor a row,
    // and importing it into another store installs no control at all.
    let (peer_bytes, peer_receipt, peer_classes) = archive(&reopened, false);
    assert!(peer_receipt.complete, "gaps: {:?}", peer_receipt.gaps);
    assert!(!peer_classes.iter().any(|class| class == "context_rollover_requests"));
    let other = open(&other_dir);
    let peer_outcome = other
        .import_cognition_archive(&peer_bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(peer_outcome.conflicts.is_empty(), "conflicts: {:?}", peer_outcome.conflicts);
    assert!(other.context_rollover_request(answered_scope).unwrap().is_none());
    assert!(other.context_rollover_request(pending_scope).unwrap().is_none());
    assert_eq!(
        StoreNotesJournal::new(Arc::clone(&other), pending_scope)
            .head()
            .unwrap()
            .unwrap()
            .revision,
        1
    );

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
    let _ = std::fs::remove_dir_all(&other_dir);
}

#[test]
fn a_handover_archive_is_not_a_peer_archive() {
    let source_dir = root("control-source");
    let receiver_dir = root("control-receiver");
    let source = open(&source_dir);
    let scope = EntityId::new();
    let notes = StoreNotesJournal::new(Arc::clone(&source), scope);
    notes.replace(0, "authored notebook", NoteSource::Tool).unwrap();
    let rollover = StoreRolloverJournal::new(Arc::clone(&source), scope);
    let armed = rollover.request(1_760_000_003_000).unwrap();

    let (handover_bytes, receipt, _) = archive(&source, true);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);

    let receiver = open(&receiver_dir);
    let before = receiver.current_commit_seq().unwrap();
    let error = receiver
        .import_cognition_archive(&handover_bytes[..], &CognitionImportOptions::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("local control fact class"),
        "a peer import must refuse local control facts: {error}"
    );
    assert!(receiver.context_note_head(scope).unwrap().is_none());
    assert!(receiver.context_rollover_request(scope).unwrap().is_none());
    assert_eq!(receiver.current_commit_seq().unwrap(), before);

    // The same store's peer snapshot carries the journal and nothing else.
    let (peer_bytes, peer_receipt, _) = archive(&source, false);
    assert!(peer_receipt.complete, "gaps: {:?}", peer_receipt.gaps);
    let outcome = receiver
        .import_cognition_archive(&peer_bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "conflicts: {:?}", outcome.conflicts);
    assert_eq!(
        StoreNotesJournal::new(Arc::clone(&receiver), scope).head().unwrap().unwrap().revision,
        1
    );
    assert!(receiver.context_rollover_request(scope).unwrap().is_none());
    let _ = armed;

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

#[test]
fn a_divergent_authored_revision_refuses_the_whole_incoming_journal() {
    let source_dir = root("divergent-source");
    let receiver_dir = root("divergent-receiver");
    let source = open(&source_dir);
    let receiver = open(&receiver_dir);
    let scope = EntityId::new();
    let local = StoreNotesJournal::new(Arc::clone(&receiver), scope);
    local.replace(0, "shared first", NoteSource::Tool).unwrap();
    local.replace(1, "local second", NoteSource::Tool).unwrap();
    let peer = StoreNotesJournal::new(Arc::clone(&source), scope);
    peer.replace(0, "shared first", NoteSource::Tool).unwrap();
    peer.replace(1, "peer second", NoteSource::Tool).unwrap();
    peer.replace(2, "peer third", NoteSource::Tool).unwrap();
    assert!(peer.head().unwrap().unwrap().visible);

    let (bytes, receipt, _) = archive(&source, false);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);
    let outcome = receiver
        .import_cognition_archive(&bytes[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(
        outcome.conflicts.iter().any(|conflict| conflict.table == "context_notes"),
        "the divergent revision is reported: {:?}",
        outcome.conflicts
    );
    assert!(!outcome.ack(EntityId::new()).verified);
    assert_eq!(
        revisions(&local, 10),
        vec![
            (1, "shared first".to_string(), NoteSource::Tool, true),
            (2, "local second".to_string(), NoteSource::Tool, true),
        ],
        "the local journal is kept whole; the peer tail never lands on it"
    );
    assert!(local.head().unwrap().unwrap().visible);

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

#[test]
fn a_note_write_after_recovery_still_compares_and_appends_on_the_recovered_head() {
    let source_dir = root("append-source");
    let receiver_dir = root("append-receiver");
    let source = open(&source_dir);
    let scope = EntityId::new();
    let journal = StoreNotesJournal::new(Arc::clone(&source), scope);
    journal.replace(0, "one", NoteSource::Tool).unwrap();
    journal.clear(1).unwrap();
    let (bytes, receipt, _) = archive(&source, false);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);

    let receiver = open(&receiver_dir);
    receiver
        .import_cognition_archive(&bytes[..], &CognitionImportOptions::default())
        .unwrap();
    let recovered = StoreNotesJournal::new(Arc::clone(&receiver), scope);
    // The compare-and-append identity survives: the recovered head is 2 (a
    // clear), a stale writer at 0 is refused, and the next revision is 3.
    assert_eq!(
        recovered.replace(0, "stale writer", NoteSource::Tool),
        Err(shenko_context_policy::NotesError::StaleRevision { expected: 0, current: Some(2) })
    );
    let next = recovered.replace(2, "after recovery", NoteSource::Tool).unwrap();
    assert_eq!(next.revision, 3);
    assert_eq!(recovered.visible().unwrap().unwrap().text, "after recovery");
    assert_eq!(
        receiver
            .append_context_note(scope, Some(99), "beyond head", "tool", 1, true)
            .unwrap(),
        ContextNoteAppend::Stale { current: Some(3) }
    );

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

#[test]
fn a_local_pending_rollover_is_never_discarded_by_a_handover_archive() {
    let source_dir = root("keep-pending-source");
    let receiver_dir = root("keep-pending-receiver");
    let source = open(&source_dir);
    let receiver = open(&receiver_dir);
    let scope = EntityId::new();
    // The receiver already owns a pending request for this scope.
    let local = StoreRolloverJournal::new(Arc::clone(&receiver), scope);
    let local_request = local.request(1_760_000_009_000).unwrap();
    // The source answered a different request of the same scope.
    let peer = StoreRolloverJournal::new(Arc::clone(&source), scope);
    let peer_request = peer.request(1_760_000_001_000).unwrap();
    let window_id = EntityId::new();
    source
        .append_context_window(
            scope,
            None,
            Some(0),
            &StoredContextWindow {
                window_id,
                generation: 0,
                body: "{\"entries\":[]}".into(),
                derivatives: Vec::new(),
            },
        )
        .unwrap();
    assert!(peer.consume(window_id).unwrap());
    assert_ne!(local_request.request_id, peer_request.request_id);

    let (bytes, receipt, _) = archive(&source, true);
    assert!(receipt.complete, "gaps: {:?}", receipt.gaps);
    let outcome = receiver
        .import_cognition_archive(
            &bytes[..],
            &CognitionImportOptions {
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.table == "context_rollover_requests"),
        "the divergence is reported: {:?}",
        outcome.conflicts
    );
    assert!(!outcome.ack(EntityId::new()).verified);
    // The local pending request is kept: an archive cannot silently discard a
    // boundary this installation still owes the subject.
    assert_eq!(
        local.pending().unwrap().unwrap().request_id,
        local_request.request_id
    );

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}

#[test]
fn a_later_handover_advances_the_same_request_to_its_answering_window() {
    let source_dir = root("answer-source");
    let receiver_dir = root("answer-receiver");
    let source = open(&source_dir);
    let scope = EntityId::new();
    let peer = StoreRolloverJournal::new(Arc::clone(&source), scope);
    let request = peer.request(1_760_000_004_000).unwrap();
    let peer_id = EntityId::new();

    let (pending_bytes, pending_receipt, _) = archive(&source, true);
    assert!(pending_receipt.complete, "gaps: {:?}", pending_receipt.gaps);
    let receiver = open(&receiver_dir);
    let first = receiver
        .import_cognition_archive(
            &pending_bytes[..],
            &CognitionImportOptions {
                peer_id: Some(peer_id),
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(first.conflicts.is_empty(), "conflicts: {:?}", first.conflicts);
    let recovered = StoreRolloverJournal::new(Arc::clone(&receiver), scope);
    assert_eq!(
        recovered.pending().unwrap().unwrap().request_id,
        request.request_id
    );

    // The same request gains the committed window that answered it.
    let window_id = EntityId::new();
    source
        .append_context_window(
            scope,
            None,
            Some(0),
            &StoredContextWindow {
                window_id,
                generation: 0,
                body: "{\"entries\":[]}".into(),
                derivatives: Vec::new(),
            },
        )
        .unwrap();
    assert!(peer.consume(window_id).unwrap());
    let (answered_bytes, answered_receipt, _) = archive(&source, true);
    assert!(answered_receipt.complete, "gaps: {:?}", answered_receipt.gaps);
    let second = receiver
        .import_cognition_archive(
            &answered_bytes[..],
            &CognitionImportOptions {
                peer_id: Some(peer_id),
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(second.conflicts.is_empty(), "conflicts: {:?}", second.conflicts);
    assert_eq!(second.updated, 1, "the answered request advanced once");
    let stored = receiver.context_rollover_request(scope).unwrap().unwrap();
    assert_eq!(stored.request_id, request.request_id);
    assert_eq!(stored.consumed_window, Some(window_id));
    assert!(recovered.pending().unwrap().is_none());

    // The answer is final: a replayed pending archive can never rewind it.
    let replay = receiver
        .import_cognition_archive(
            &pending_bytes[..],
            &CognitionImportOptions {
                peer_id: Some(peer_id),
                local_control_handover: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(replay.conflicts.iter().any(|conflict| conflict.table == "context_rollover_requests"));
    assert_eq!(
        receiver.context_rollover_request(scope).unwrap().unwrap().consumed_window,
        Some(window_id)
    );

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&receiver_dir);
}
