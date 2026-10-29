use shenko_store::{SeatLeaseEnd, SeatLocation, SeatStatus, Store};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

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
    let first = store.acquire_seat_lease(seat.seat_id, task, EntityId::new()).unwrap();
    let second = store.acquire_seat_lease(seat.seat_id, task, EntityId::new()).unwrap();
    drop(store);
    let store = Store::open(db, cas).unwrap();
    assert!(store.check_seat_write(first.lease_id, task).is_err());
    assert!(store.check_seat_write(second.lease_id, task).is_ok());
    assert_eq!(store.seat_lease(first.lease_id).unwrap().unwrap().superseded_by, Some(second.lease_id));
    store.end_seat_lease(second.lease_id, SeatLeaseEnd::HumanTakeover).unwrap();
    // The recorded end reason is a real JSON value, not double-encoded text.
    let err = store.check_seat_write(second.lease_id, task).unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_lease_ended");
    assert_eq!(details["reason"], serde_json::json!("human_takeover"));
}

#[test]
fn relocation_preserves_history_and_invalidates_previous_location_lease() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let old = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(old.seat_id, task, EntityId::new()).unwrap();
    let new = store.relocate_seat(old.seat_id, task, "omp", "1", location("%2")).unwrap();
    assert_eq!(new.supersedes, Some(old.seat_id));
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Retired);
    assert_eq!(store.seat_successor(old.seat_id).unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.seat_at_pane("local", "%1").unwrap().is_none());
    assert_eq!(store.seat_at_pane("local", "%2").unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task).is_err());
    assert_eq!(store.seat_task_seats(task).unwrap().len(), 2);
}

#[test]
fn pane_admits_only_one_active_seat_for_registration_and_relocation() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let task_c = EntityId::new();
    let seat_a = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%1")).unwrap();
    let seat_a2 = store.relocate_seat(seat_a.seat_id, task_a, "omp", "1", location("%2")).unwrap();
    // A second active registration at an occupied pane is refused atomically.
    let err = store
        .register_seat(EntityId::new(), task_b, "omp", "1", location("%2"))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_location_occupied");
    assert_eq!(details["claimant_seat_id"], serde_json::json!(seat_a2.seat_id.to_string()));
    assert_eq!(store.seat_at_pane("local", "%2").unwrap().unwrap().seat_id, seat_a2.seat_id);
    // Relocation into an occupied pane is refused the same way.
    let seat_c = store.register_seat(EntityId::new(), task_c, "omp", "1", location("%3")).unwrap();
    let err = store
        .relocate_seat(seat_c.seat_id, task_c, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    assert_eq!(store.seat(seat_c.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_pane("local", "%3").unwrap().unwrap().seat_id, seat_c.seat_id);
    // Retiring the claimant frees the pane for the next registration.
    store.retire_seat(seat_a2.seat_id).unwrap();
    let seat_b = store.register_seat(EntityId::new(), task_b, "omp", "1", location("%2")).unwrap();
    assert_eq!(store.seat_at_pane("local", "%2").unwrap().unwrap().seat_id, seat_b.seat_id);
}

#[test]
fn caller_named_lease_replays_without_displacing_newer_writer() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let seat = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    // A retried acquisition of the same caller-chosen identity replays the
    // committed lease instead of minting a second writer.
    let attempt = EntityId::new();
    let first = store.acquire_seat_lease(seat.seat_id, task, attempt).unwrap();
    let replay = store.acquire_seat_lease(seat.seat_id, task, attempt).unwrap();
    assert_eq!(replay.lease_id, first.lease_id);
    assert_eq!(replay.created_commit, first.created_commit);
    // After a newer writer takes over, replaying the old attempt still returns
    // the same ended lease — it cannot displace the legitimate holder.
    let newer = store.acquire_seat_lease(seat.seat_id, task, EntityId::new()).unwrap();
    let replayed = store.acquire_seat_lease(seat.seat_id, task, attempt).unwrap();
    assert_eq!(replayed.lease_id, first.lease_id);
    assert!(!replayed.is_current());
    assert_eq!(replayed.superseded_by, Some(newer.lease_id));
    assert!(store.check_seat_write(newer.lease_id, task).is_ok());
}

#[test]
fn relocation_requires_the_predecessors_own_task() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let old = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(old.seat_id, task_a, EntityId::new()).unwrap();
    // Relocating under another task is refused and leaves task A's seat alive.
    let err = store
        .relocate_seat(old.seat_id, task_b, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_task_mismatch");
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_pane("local", "%1").unwrap().unwrap().seat_id, old.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task_a).is_ok());
    // The predecessor's own task can still relocate afterwards.
    let moved = store.relocate_seat(old.seat_id, task_a, "omp", "1", location("%2")).unwrap();
    assert_eq!(moved.task_id, task_a);
    assert_eq!(store.seat_successor(old.seat_id).unwrap().unwrap().seat_id, moved.seat_id);
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
