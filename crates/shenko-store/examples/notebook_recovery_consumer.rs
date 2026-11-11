//! Authored notebook recovery producer/consumer — docs/specs/context-policies.md#spec-12-3-1, docs/specs/nodes.md#spec-5-4.
//!
//! Two real programs, not a fixture: `write` authors a notebook journal
//! (replacement, clear, fork copy) and a pending rollover request in a real
//! store and writes the archive this build produces; `read` imports that
//! archive into a second real store and reads the recovered facts back after
//! reopening the store.
//!
//! The producer declares every authored identity in `<archive>.expect.json`
//! beside the archive, so the consumer compares the recovered rows against the
//! source identities instead of re-deriving them from the archive it reads.
//!
//! Usage:
//!   notebook_recovery_consumer write <dir> <archive>            # peer snapshot
//!   notebook_recovery_consumer write-handover <dir> <archive>   # A/B handover
//!   notebook_recovery_consumer read <dir> <archive> [handover]
//!
//! `read` prints one JSON line. A non-zero exit means the recovered facts did
//! not match the declared identities, the import refused the archive, or the
//! reopened store lost a fact.

use std::path::Path;
use std::sync::Arc;

use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use shenko_store::store::Store;
use shenko_store::ARCHIVE_SCHEMA;
use shenko_types::ids::EntityId;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mode = arguments.next().unwrap_or_default();
    let dir = std::path::PathBuf::from(arguments.next().unwrap_or_default());
    let archive = std::path::PathBuf::from(arguments.next().unwrap_or_default());
    if dir.as_os_str().is_empty() || archive.as_os_str().is_empty() {
        anyhow::bail!("usage: write|write-handover|read <dir> <archive> [handover]");
    }
    std::fs::create_dir_all(&dir)?;
    match mode.as_str() {
        "write" => write(&dir, &archive, false),
        "write-handover" => write(&dir, &archive, true),
        "read" => {
            let handover = arguments.next().is_some_and(|flag| flag == "handover");
            read(&dir, &archive, handover)
        }
        other => anyhow::bail!("unknown mode {other}; expected write, write-handover or read"),
    }
}

fn open(dir: &Path) -> anyhow::Result<Arc<Store>> {
    Ok(Arc::new(Store::open(
        dir.join("shenko.db"),
        dir.join("cas"),
    )?))
}

fn write(dir: &Path, archive: &Path, handover: bool) -> anyhow::Result<()> {
    let store = open(dir)?;
    let scope = EntityId::new();
    let fork_scope = EntityId::new();
    store.append_context_note(scope, None, "first working state", "tool", 1_760_000_000_000, true)?;
    store.append_context_note(scope, Some(1), "", "reset", 1_760_000_000_001, false)?;
    store.append_context_note(scope, Some(2), "state after reset", "tool", 1_760_000_000_002, true)?;
    // A fork copy carries the visible notebook under its own scope identity.
    store.append_context_note(fork_scope, None, "state after reset", "fork", 1_760_000_000_003, true)?;
    let rollover = store.arm_context_rollover(scope, 1_760_000_000_004)?;

    let export = store.export_cognition(&CognitionSnapshotOptions {
        local_boundary_preserved: handover,
        ..Default::default()
    })?;
    let classes = export.manifest.fact_classes.clone();
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(archive)?;
    let receipt = export.write_archive(&mut file)?;
    let expectation = serde_json::json!({
        "scope": scope.to_string(),
        "fork_scope": fork_scope.to_string(),
        "note_revisions": [
            {"revision": 1, "body": "first working state", "source": "tool", "visible": true},
            {"revision": 2, "body": "", "source": "reset", "visible": false},
            {"revision": 3, "body": "state after reset", "source": "tool", "visible": true},
        ],
        "fork_revisions": [
            {"revision": 1, "body": "state after reset", "source": "fork", "visible": true},
        ],
        "rollover_request_id": rollover.request_id.to_string(),
        "rollover_carried": handover,
        "handover": handover,
    });
    std::fs::write(expectation_path(archive), serde_json::to_vec_pretty(&expectation)?)?;
    println!(
        "{}",
        serde_json::json!({
            "mode": if handover { "write-handover" } else { "write" },
            "writer_schema": ARCHIVE_SCHEMA,
            "fact_classes": classes,
            "receipt": receipt,
        })
    );
    Ok(())
}

fn read(dir: &Path, archive: &Path, handover: bool) -> anyhow::Result<()> {
    let expectation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(expectation_path(archive))?)?;
    let scope: EntityId = expectation["scope"].as_str().unwrap_or_default().parse()?;
    let fork_scope: EntityId = expectation["fork_scope"].as_str().unwrap_or_default().parse()?;
    let store = open(dir)?;
    let outcome = store.import_cognition_archive(
        std::fs::File::open(archive)?,
        &CognitionImportOptions {
            peer_id: Some(EntityId::new()),
            local_control_handover: handover,
            ..Default::default()
        },
    )?;
    let imported = format!("{:?}", outcome.ack(EntityId::new()));
    let notes = note_rows(&store, scope)?;
    let fork_notes = note_rows(&store, fork_scope)?;
    let rollover = store
        .context_rollover_request(scope)?
        .map(|stored| stored.request_id.to_string());
    drop(store);

    // Reopen: the recovered facts must be store state, not the import call.
    let reopened = open(dir)?;
    let after_reopen = note_rows(&reopened, scope)?;
    drop(reopened);

    // A peer snapshot carries no local control fact at all, so the consumer
    // requires the absence of the request exactly as the producer declared it.
    let rollover_matches = if expectation["rollover_carried"].as_bool().unwrap_or(false) {
        rollover.as_deref() == expectation["rollover_request_id"].as_str()
    } else {
        rollover.is_none()
    };
    let matches = notes == expectation["note_revisions"]
        && fork_notes == expectation["fork_revisions"]
        && after_reopen == notes
        && rollover_matches;
    println!(
        "{}",
        serde_json::json!({
            "mode": "read",
            "reader_schema": ARCHIVE_SCHEMA,
            "handover": handover,
            "ack": imported,
            "notes": notes,
            "fork_notes": fork_notes,
            "rollover_request_id": rollover,
            "expected": expectation,
            "matches": matches,
        })
    );
    if !matches {
        anyhow::bail!("the recovered authored facts do not match the declared identities");
    }
    Ok(())
}

fn note_rows(store: &Store, scope: EntityId) -> anyhow::Result<serde_json::Value> {
    let mut rows = Vec::new();
    for note in store.context_note_page(scope, None, 32)? {
        rows.push(serde_json::json!({
            "revision": note.revision,
            "body": note.body,
            "source": note.source,
            "visible": note.visible,
        }));
    }
    rows.reverse();
    Ok(serde_json::Value::Array(rows))
}

fn expectation_path(archive: &Path) -> std::path::PathBuf {
    let mut name = archive.as_os_str().to_owned();
    name.push(".expect.json");
    std::path::PathBuf::from(name)
}
