use shenko_store::{LeaseEndReason, SeatLocation, SeatStatus, Store};
use shenko_types::ids::EntityId;

fn location(pane: &str) -> SeatLocation {
    SeatLocation::Tmux {
        host: "local".into(), session: "dev".into(), window: 0, pane: 0,
        pane_id: pane.into(), cwd: "/project".into(), current_command: "omp".into(),
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
    let first = store.acquire_seat_lease(seat.seat_id, task).unwrap();
    let second = store.acquire_seat_lease(seat.seat_id, task).unwrap();
    drop(store);
    let store = Store::open(db, cas).unwrap();
    assert!(store.check_seat_write(first.lease_id, task).is_err());
    assert!(store.check_seat_write(second.lease_id, task).is_ok());
    assert_eq!(store.seat_lease(first.lease_id).unwrap().unwrap().superseded_by, Some(second.lease_id));
    store.end_seat_lease(second.lease_id, LeaseEndReason::HumanTakeover).unwrap();
    assert!(store.check_seat_write(second.lease_id, task).is_err());
}

#[test]
fn relocation_preserves_history_and_invalidates_previous_location_lease() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let old = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(old.seat_id, task).unwrap();
    let new = store.relocate_seat(old.seat_id, task, "omp", "1", location("%2")).unwrap();
    assert_eq!(new.supersedes, Some(old.seat_id));
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Retired);
    assert_eq!(store.seat_successor(old.seat_id).unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.seat_at_pane("local", "%1").unwrap().is_none());
    assert_eq!(store.seat_at_pane("local", "%2").unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task).is_err());
    assert_eq!(store.seat_task_seats(task).unwrap().len(), 2);
}
