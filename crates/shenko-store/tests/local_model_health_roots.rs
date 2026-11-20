//! Durable local-health ownership across supported opaque state aliases.
use anyhow::Result;
use shenko_store::{Store, roots::{CasWriteFence, SqliteWriteLease, object_closure, referenced_objects}};
use shenko_types::object::{Digest, ObjectRef};

fn seal_observation(store: &Store, label: &str) -> Result<(ObjectRef, ObjectRef)> {
    let original = store.cas.seal_object(label.as_bytes(), "text/plain",
        "model-health", "model-health-observation", 1)?.object;
    let evidence = store.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
        "schema_version": 1, "original_ref": original,
    }))?, "application/json", "model-health", "model-usability-evidence", 1)?.object;
    Ok((original, evidence))
}

fn seal_state(
    store: &Store,
    binding: &Digest,
    previous: Option<&ObjectRef>,
    observation: &ObjectRef,
    media_type: &str,
) -> Result<ObjectRef> {
    Ok(store.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
        "schema_version": 1, "binding_digest": binding,
        "previous_ref": previous, "observation_ref": observation,
        "eligibility": {"fixture": "retained storage evidence, no model usability claim"},
    }))?, media_type, "model-health", "model-health-state", 1)?.object)
}

#[test]
fn opaque_health_predecessors_retain_their_original_observations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("head.db");
    let cas_path = directory.path().join("cas");
    let binding = Digest::blake3(b"opaque health predecessor ownership regression");
    let store = Store::open(database.clone(), cas_path.clone())?;
    let (original0, evidence0) = seal_observation(&store, "first unique original observation")?;
    let state0 = seal_state(&store, &binding, None, &evidence0, "application/octet-stream")?;
    store.record_local_model_health(&binding, None, &state0)?;
    let (original1, evidence1) = seal_observation(&store, "second unique original observation")?;
    let state1 = seal_state(&store, &binding, Some(&state0), &evidence1, "application/octet-stream")?;
    store.record_local_model_health(&binding, Some(&state0), &state1)?;
    let (original2, evidence2) = seal_observation(&store, "current unique original observation")?;
    let state2 = seal_state(&store, &binding, Some(&state1), &evidence2, "application/json")?;
    store.record_local_model_health(&binding, Some(&state1), &state2)?;
    let retained = vec![original0, evidence0, state0, original1, evidence1, state1, original2, evidence2, state2];
    let bytes = retained.iter().map(|object| store.cas.read_object(&object.digest))
        .collect::<Result<Vec<_>, _>>()?;
    let orphan = store.cas.seal_object(b"unrelated unowned object", "text/plain", "test", "raw", 1)?.object;
    let stored_rows: Vec<String> = {
        let connection = rusqlite::Connection::open(&database)?;
        let mut statement = connection.prepare("SELECT data FROM local_model_health ORDER BY binding_digest")?;
        statement.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    drop(store);

    let reopened = Store::open(database.clone(), cas_path.clone())?;
    assert_eq!(reopened.local_model_health(&binding)?, Some(retained[8].clone()));
    // Ordinary opaque references correctly lack JSON authority. This control
    // reproduces the old head-only expansion boundary without faking SQL rows.
    let head_only = object_closure(&reopened.cas, vec![retained[8].clone()]);
    assert!(head_only.missing.is_empty());
    assert!(head_only.digests.contains(&retained[5].digest));
    assert!(!head_only.digests.contains(&retained[3].digest));
    assert!(!head_only.digests.contains(&retained[0].digest));

    let _fence = CasWriteFence::acquire(reopened.cas.base(), 0)?;
    let excluded = SqliteWriteLease::try_acquire(&database)?;
    let revision = excluded.revision()?;
    let roots = referenced_objects(excluded.connection(), &reopened.cas)?;
    assert!(roots.missing.is_empty(), "{:?}", roots.missing);
    for (object, expected) in retained.iter().zip(&bytes) {
        assert!(roots.digests.contains(&object.digest), "lost owned object {}", object.digest);
        assert_eq!(reopened.cas.read_object(&object.digest)?, *expected);
    }
    assert!(!roots.digests.contains(&orphan.digest));
    assert_eq!(excluded.revision()?, revision);
    let mut statement = excluded.connection().prepare("SELECT data FROM local_model_health ORDER BY binding_digest")?;
    let after_rows = statement.query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(after_rows, stored_rows, "enumeration must preserve original aliases and rows");
    Ok(())
}
