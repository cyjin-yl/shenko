use shenko_store::{LeaseEndReason, SeatLeaseEnd, SeatLeaseRequest, SeatLocation, SeatStatus, Store};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

fn lease_request(seat_id: EntityId, holder_task_id: EntityId, lease_id: EntityId) -> SeatLeaseRequest {
    SeatLeaseRequest {
        lease_id,
        seat_id,
        holder_task_id,
        lease_epoch: 1,
        fence_capability: "tmux.send-keys:%1".into(),
    }
}

fn location(pane: &str) -> SeatLocation {
    SeatLocation::Tmux {
        host: "local".into(), session: "dev".into(), window: 0, pane: 0,
        pane_id: pane.into(), cwd: "/project".into(), current_command: "omp".into(),
    }
}

fn protocol_code(err: &anyhow::Error) -> Option<(&str, &serde_json::Value)> {
    match err.downcast_ref::<ShenkoError>() {
        Some(ShenkoError::Protocol(p)) => Some((p.code.as_str(), &p.details)),
        _ => None,
    }
}

#[test]
fn displaced_writer_stays_displaced_after_reopen_and_human_takeover() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let cas = dir.join("cas");
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    let task = EntityId::new();
    let seat = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let acquisition_id = EntityId::new();
    let first = store.acquire_seat_lease(lease_request(seat.seat_id, task, acquisition_id)).unwrap();
    let second = store.acquire_seat_lease(lease_request(seat.seat_id, task, EntityId::new())).unwrap();
    drop(store);
    let store = Store::open(db, cas).unwrap();
    // Recover the original acquisition by the identity chosen before the
    // attempt. Replaying it after reopen cannot displace the newer writer.
    let replay = store.acquire_seat_lease(lease_request(seat.seat_id, task, acquisition_id)).unwrap();
    assert_eq!(replay.lease_id, acquisition_id);
    assert_eq!(replay.created_commit, first.created_commit);
    assert!(!replay.is_current());
    assert_eq!(replay.superseded_by, Some(second.lease_id));
    assert!(store.check_seat_write(first.lease_id, task).is_err());
    assert!(store.check_seat_write(second.lease_id, task).is_ok());
    assert_eq!(store.seat_lease(first.lease_id).unwrap().unwrap().superseded_by, Some(second.lease_id));
    store.end_seat_lease(second.lease_id, SeatLeaseEnd::HumanTakeover).unwrap();
    // The recorded end reason is a real JSON value, not double-encoded text.
    let err = store.check_seat_write(second.lease_id, task).unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_lease_ended");
    assert_eq!(
        serde_json::from_value::<LeaseEndReason>(details["reason"].clone()).unwrap(),
        LeaseEndReason::HumanTakeover,
    );
}

#[test]
fn relocation_preserves_history_and_invalidates_previous_location_lease() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let old = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(lease_request(old.seat_id, task, EntityId::new())).unwrap();
    let new = store.relocate_seat(old.seat_id, task, "omp", "1", location("%2")).unwrap();
    assert_eq!(new.supersedes, Some(old.seat_id));
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Retired);
    assert_eq!(store.seat_successor(old.seat_id).unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.seat_at_location("local", "%1").unwrap().is_none());
    assert_eq!(store.seat_at_location("local", "%2").unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task).is_err());
    assert_eq!(store.seat_task_seats(task).unwrap().len(), 2);
}

#[test]
fn pane_admits_only_one_active_seat_for_registration_and_relocation() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let contender = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let task_c = EntityId::new();
    // Both discoverers saw no seat. The later admission must recheck inside
    // its write transaction, not rely on this now-stale discovery result.
    assert!(store.seat_at_location("local", "%2").unwrap().is_none());
    assert!(contender.seat_at_location("local", "%2").unwrap().is_none());
    let seat_a = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%2")).unwrap();
    let err = contender
        .register_seat(EntityId::new(), task_b, "omp", "1", location("%2"))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_location_occupied");
    assert_eq!(details["claimant_seat_id"], serde_json::json!(seat_a.seat_id.to_string()));
    assert_eq!(contender.seat_at_location("local", "%2").unwrap().unwrap().seat_id, seat_a.seat_id);
    // Relocation into an occupied pane is refused the same way.
    let seat_c = contender.register_seat(EntityId::new(), task_c, "omp", "1", location("%3")).unwrap();
    let lease_c = contender
        .acquire_seat_lease(lease_request(seat_c.seat_id, task_c, EntityId::new()))
        .unwrap();
    let err = contender
        .relocate_seat(seat_c.seat_id, task_c, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    assert_eq!(store.seat(seat_c.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_location("local", "%3").unwrap().unwrap().seat_id, seat_c.seat_id);
    assert!(store.check_seat_write(lease_c.lease_id, task_c).is_ok());
}

#[test]
fn relocation_requires_the_predecessors_own_task() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let old = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(lease_request(old.seat_id, task_a, EntityId::new())).unwrap();
    // Relocating under another task is refused and leaves task A's seat alive.
    let err = store
        .relocate_seat(old.seat_id, task_b, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_task_mismatch");
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_location("local", "%1").unwrap().unwrap().seat_id, old.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task_a).is_ok());
    assert_eq!(store.seat_task_seats(task_a).unwrap().len(), 1);
    assert!(store.seat_task_seats(task_b).unwrap().is_empty());
}

#[test]
fn successor_walk_resolves_long_relocation_chains() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let mut seat = store.register_seat(EntityId::new(), task, "omp", "1", location("%0")).unwrap();
    let origin = seat.seat_id;
    for i in 1..70 {
        seat = store
            .relocate_seat(seat.seat_id, task, "omp", "1", location(&format!("%{i}")))
            .unwrap();
    }
    assert_eq!(store.seat_successor(origin).unwrap().unwrap().seat_id, seat.seat_id);
}

fn omp_location(session: &str) -> SeatLocation {
    SeatLocation::OmpRpc {
        host: "local".into(),
        remote_session_id: session.into(),
    }
}

fn claude_location(session: &str) -> SeatLocation {
    SeatLocation::ClaudeSdk {
        host: "local".into(),
        remote_session_id: session.into(),
    }
}

#[test]
fn managed_session_anchor_admits_only_one_active_seat() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let seat_a = store
        .register_seat(EntityId::new(), task_a, "omp_rpc", "omp-rpc-v1", omp_location("omp sess"))
        .unwrap();
    // A different adapter family at the same (host, anchor) is still one
    // location: the claim space is shared.
    let err = store
        .register_seat(
            EntityId::new(),
            task_b,
            "claude_sdk",
            "claude-control-v1",
            claude_location("omp sess"),
        )
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    // The same anchor under a different host is a different location.
    let remote = SeatLocation::ClaudeSdk {
        host: "remote".into(),
        remote_session_id: "omp sess".into(),
    };
    store
        .register_seat(EntityId::new(), task_b, "claude_sdk", "claude-control-v1", remote)
        .unwrap();
    // The lookup resolves both kinds by their anchor.
    assert_eq!(
        store.seat_at_location("local", "omp sess").unwrap().unwrap().seat_id,
        seat_a.seat_id
    );
    // Retiring the incumbent frees the anchor for the next claimant.
    store.retire_seat(seat_a.seat_id).unwrap();
    let seat_b = store
        .register_seat(
            EntityId::new(),
            task_b,
            "claude_sdk",
            "claude-control-v1",
            claude_location("omp sess"),
        )
        .unwrap();
    assert_eq!(
        store.seat_at_location("local", "omp sess").unwrap().unwrap().seat_id,
        seat_b.seat_id
    );
    // Idempotent re-registration of the same facts still returns the record.
    let again = store
        .register_seat(seat_b.seat_id, task_b, "claude_sdk", "claude-control-v1", claude_location("omp sess"))
        .unwrap();
    assert_eq!(again.seat_id, seat_b.seat_id);
}

#[test]
fn incarnation_bump_stales_existing_leases_without_ending_them() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let seat = store
        .register_seat(EntityId::new(), task, "omp", "1", location("%7"))
        .unwrap();
    let lease = store
        .acquire_seat_lease(lease_request(seat.seat_id, task, EntityId::new()))
        .unwrap();
    assert_eq!(seat.incarnation.0, 0);
    let bumped = store.bump_seat_incarnation(seat.seat_id).unwrap();
    assert_eq!(bumped.incarnation.0, 1);
    assert_eq!(bumped.seat_id, seat.seat_id);
    assert!(bumped.record_revision.0 > seat.record_revision.0);
    assert_eq!(bumped.status, SeatStatus::Active);
    // The lease itself is not ended by an observation; the incarnation fence
    // is what refuses the write.
    let current = store.seat_current_lease(seat.seat_id).unwrap().unwrap();
    assert!(current.is_current());
    assert_eq!(current.lease_id, lease.lease_id);
    let err = store.check_seat_write(lease.lease_id, task).unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_changed");
    // The record's supersedes chain is untouched: an in-place bump is not a
    // relocation and mints no successor.
    assert_eq!(
        store.seat_successor(seat.seat_id).unwrap().unwrap().seat_id,
        seat.seat_id
    );
    // A retired record can never be bumped.
    store.retire_seat(seat.seat_id).unwrap();
    assert!(store.bump_seat_incarnation(seat.seat_id).is_err());
}

#[test]
fn active_seat_records_lists_only_live_seats() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let tmux_seat = store
        .register_seat(EntityId::new(), task, "tmux", "1", location("%1"))
        .unwrap();
    let omp_seat = store
        .register_seat(EntityId::new(), task, "omp_rpc", "1", omp_location("s"))
        .unwrap();
    store.retire_seat(tmux_seat.seat_id).unwrap();
    let active = store.active_seat_records().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].seat_id, omp_seat.seat_id);
}

#[test]
fn pre_v2_location_index_is_rebuilt_to_cover_session_anchors() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let cas = dir.join("cas");
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    drop(store);
    // Simulate a store written before the index covered remote_session_id.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_seat_active_location;
             CREATE UNIQUE INDEX idx_seat_active_location ON seat_records(
                 json_extract(data,'$.location.host'), json_extract(data,'$.location.pane_id')
             ) WHERE status='active';",
        )
        .unwrap();
    }
    let _store = Store::open(db.clone(), cas).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='idx_seat_active_location'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("remote_session_id"), "index was not rebuilt: {sql}");
}
