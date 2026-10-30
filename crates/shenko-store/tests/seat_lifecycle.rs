use shenko_store::{LeaseEndReason, SeatLeaseEnd, SeatLocation, SeatStatus, Store};
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
    let acquisition_id = EntityId::new();
    let first = store.acquire_seat_lease(seat.seat_id, task, acquisition_id).unwrap();
    let second = store.acquire_seat_lease(seat.seat_id, task, EntityId::new()).unwrap();
    drop(store);
    let store = Store::open(db, cas).unwrap();
    // Recover the original acquisition by the identity chosen before the
    // attempt. Replaying it after reopen cannot displace the newer writer.
    let replay = store.acquire_seat_lease(seat.seat_id, task, acquisition_id).unwrap();
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
    let contender = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let task_c = EntityId::new();
    // Both discoverers saw no seat. The later admission must recheck inside
    // its write transaction, not rely on this now-stale discovery result.
    assert!(store.seat_at_pane("local", "%2").unwrap().is_none());
    assert!(contender.seat_at_pane("local", "%2").unwrap().is_none());
    let seat_a = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%2")).unwrap();
    let err = contender
        .register_seat(EntityId::new(), task_b, "omp", "1", location("%2"))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_location_occupied");
    assert_eq!(details["claimant_seat_id"], serde_json::json!(seat_a.seat_id.to_string()));
    assert_eq!(contender.seat_at_pane("local", "%2").unwrap().unwrap().seat_id, seat_a.seat_id);
    // Relocation into an occupied pane is refused the same way.
    let seat_c = contender.register_seat(EntityId::new(), task_c, "omp", "1", location("%3")).unwrap();
    let lease_c = contender.acquire_seat_lease(seat_c.seat_id, task_c, EntityId::new()).unwrap();
    let err = contender
        .relocate_seat(seat_c.seat_id, task_c, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    assert_eq!(store.seat(seat_c.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_pane("local", "%3").unwrap().unwrap().seat_id, seat_c.seat_id);
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
    let lease = store.acquire_seat_lease(old.seat_id, task_a, EntityId::new()).unwrap();
    // Relocating under another task is refused and leaves task A's seat alive.
    let err = store
        .relocate_seat(old.seat_id, task_b, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_task_mismatch");
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_pane("local", "%1").unwrap().unwrap().seat_id, old.seat_id);
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
