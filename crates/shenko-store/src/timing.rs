//! Opt-in, non-durable wall-time observations of synchronous Store work.
//!
//! These events are diagnostics, never authority or execution receipts. A
//! scope's end means that its stack frame exited, including early error and
//! unwind paths; it does not assert a successful commit or physical send.
use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
    sync::{OnceLock, atomic::{AtomicU64, Ordering}, mpsc::{self, SyncSender}},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use shenko_types::ids::EntityId;

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct Binding {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<EntityId>,
}

impl Binding {
    fn inherit(self, parent: Self) -> Self {
        Self {
            subject_id: self.subject_id.or(parent.subject_id),
            task_id: self.task_id.or(parent.task_id),
            operation_id: self.operation_id.or(parent.operation_id),
            attempt_id: self.attempt_id.or(parent.attempt_id),
            action_id: self.action_id.or(parent.action_id),
        }
    }
}

#[derive(Clone, Copy)]
struct Frame {
    id: u64,
    binding: Binding,
}

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SINK: OnceLock<Option<SyncSender<Message>>> = OnceLock::new();

enum Message {
    Event(tracing::Dispatch, Event),
    #[cfg(test)]
    Flush(SyncSender<()>),
}

fn sink() -> Option<&'static SyncSender<Message>> {
    SINK.get_or_init(|| {
        // A slow diagnostic destination must never block an acquired Store
        // mutex or SQLite transaction. The bounded queue drops observations,
        // not product work; the next delivered event reports the loss count.
        let (sender, receiver) = mpsc::sync_channel::<Message>(512);
        std::thread::Builder::new().name("shenko-timing".into()).spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    Message::Event(dispatch, event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            tracing::dispatcher::with_default(&dispatch, || {
                                tracing::debug!(target: "shenko_timing", timing_json = %json, "store phase");
                            });
                        }
                    }
                    #[cfg(test)]
                    Message::Flush(ack) => { let _ = ack.try_send(()); }
                }
            }
        }).ok().map(|_| sender)
    }).as_ref()
}

/// Cannot cross an await, thread handoff or callback lifetime. Every measured
/// Store operation is synchronous, so nested phases have one unambiguous parent.
#[must_use]
pub(crate) struct Phase {
    observation: Option<Observation>,
    _synchronous: PhantomData<Rc<()>>,
}

struct Observation {
    frame: Frame,
    parent_id: Option<u64>,
    phase: &'static str,
    started: Instant,
    started_unix_us: u64,
}

#[derive(Serialize)]
struct Event {
    schema: u8,
    event: &'static str,
    pid: u32,
    scope_id: u64,
    parent_id: Option<u64>,
    phase: &'static str,
    started_unix_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_us: Option<u64>,
    dropped_before: u64,
    binding: Binding,
}

impl Observation {
    fn emit(&self, event: &'static str, elapsed_us: Option<u64>) {
        // Only fixed phase labels, numeric time and typed identities enter the
        // event. Prompts, SQL, errors, endpoint URLs and credentials do not.
        let event = Event {
            schema: 1,
            event,
            pid: std::process::id(),
            scope_id: self.frame.id,
            parent_id: self.parent_id,
            phase: self.phase,
            started_unix_us: self.started_unix_us,
            elapsed_us,
            dropped_before: DROPPED.load(Ordering::Relaxed),
            binding: self.frame.binding,
        };
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        if sink().is_none_or(|sender| sender.try_send(Message::Event(dispatch, event)).is_err()) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Phase {
    pub(crate) fn new(phase: &'static str) -> Self {
        Self::bound(phase, Binding::default())
    }

    pub(crate) fn bound(phase: &'static str, binding: Binding) -> Self {
        // No clock, allocation, identity generation or thread-local work when
        // this target is disabled (the default info log level).
        let observation = tracing::enabled!(target: "shenko_timing", tracing::Level::DEBUG).then(|| {
            let (frame, parent_id) = STACK.with(|stack| {
                let mut stack = stack.borrow_mut();
                let parent = stack.last().copied();
                let frame = Frame {
                    id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                    binding: binding.inherit(parent.map(|parent| parent.binding).unwrap_or_default()),
                };
                stack.push(frame);
                (frame, parent.map(|parent| parent.id))
            });
            let observation = Observation {
                frame,
                parent_id,
                phase,
                started: Instant::now(),
                started_unix_us: SystemTime::now().duration_since(UNIX_EPOCH)
                    .map(|time| micros(time.as_micros())).unwrap_or(0),
            };
            observation.emit("begin", None);
            observation
        });
        Self { observation, _synchronous: PhantomData }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let Some(observation) = &self.observation else { return; };
        let elapsed_us = micros(observation.started.elapsed().as_micros());
        STACK.with(|stack| {
            // Diagnostics must not panic or change the observed operation if a
            // future caller accidentally drops a parent before its child.
            stack.borrow_mut().retain(|frame| frame.id != observation.frame.id);
        });
        observation.emit("end", Some(elapsed_us));
    }
}

fn micros(value: u128) -> u64 {
    value.min(u128::from(u64::MAX)) as u64
}

pub(crate) fn measure<T>(phase: &'static str, work: impl FnOnce() -> T) -> T {
    let _phase = Phase::new(phase);
    work()
}

pub(crate) fn immediate(conn: &mut rusqlite::Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    let _phase = Phase::new("sqlite.begin_immediate");
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::{Event, Metadata, Subscriber, field::{Field, Visit}, span::{Attributes, Id, Record}};

    struct Collector(Arc<Mutex<Vec<serde_json::Value>>>);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    impl Visit for Collector {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "timing_json" {
                self.0.lock().unwrap().push(serde_json::from_str(&format!("{value:?}")).unwrap());
            }
        }
    }

    impl Subscriber for Collector {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool { metadata.target() == "shenko_timing" }
        fn new_span(&self, _: &Attributes<'_>) -> Id { Id::from_u64(1) }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) { event.record(&mut Collector(Arc::clone(&self.0))); }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    #[test]
    fn nested_error_paths_keep_identity_and_retire_the_parent() {
        let _serial = TEST_LOCK.lock().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let task = EntityId::new();
        let action = EntityId::new();
        tracing::subscriber::with_default(Collector(Arc::clone(&events)), || {
            let parent = Phase::bound("native.claim", Binding { task_id: Some(task), ..Default::default() });
            fn refused(action: EntityId) -> Result<(), &'static str> {
                let _child = Phase::bound("canonical.prepare", Binding { action_id: Some(action), ..Default::default() });
                Err("private failure detail")
            }
            let result = refused(action);
            assert_eq!(result, Err("private failure detail"));
            drop(parent);
            let _next = Phase::new("next");
        });
        let (ack, delivered) = mpsc::sync_channel(1);
        sink().unwrap().send(Message::Flush(ack)).unwrap();
        delivered.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(events[0]["event"], "begin");
        assert_eq!(events[1]["parent_id"], events[0]["scope_id"]);
        assert_eq!(events[1]["binding"]["task_id"], task.to_string());
        assert_eq!(events[1]["binding"]["action_id"], action.to_string());
        assert_eq!(events[2]["scope_id"], events[1]["scope_id"]);
        assert_eq!(events[2]["event"], "end");
        assert!(events[2]["elapsed_us"].is_u64());
        assert_eq!(events[3]["scope_id"], events[0]["scope_id"]);
        assert!(events[4]["parent_id"].is_null());
        assert_eq!(events[4]["binding"], serde_json::json!({}));
        assert!(!serde_json::to_string(&*events).unwrap().contains("private failure detail"));
    }

    #[test]
    fn disabled_timing_does_not_create_a_scope() {
        let _serial = TEST_LOCK.lock().unwrap();
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            let phase = Phase::new("disabled");
            assert!(phase.observation.is_none());
            STACK.with(|stack| assert!(stack.borrow().is_empty()));
        });
    }

    #[test]
    fn slow_log_destination_drops_observations_without_blocking_sqlite() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        struct Blocked {
            first: AtomicBool,
            entered: SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Subscriber for Blocked {
            fn enabled(&self, metadata: &Metadata<'_>) -> bool { metadata.target() == "shenko_timing" }
            fn new_span(&self, _: &Attributes<'_>) -> Id { Id::from_u64(1) }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, _: &Event<'_>) {
                if !self.first.swap(true, Ordering::Relaxed) {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        let _serial = TEST_LOCK.lock().unwrap();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let subscriber = Blocked { first: AtomicBool::new(false), entered, release: Mutex::new(blocked) };
        tracing::subscriber::with_default(subscriber, || {
            drop(Phase::new("blocked_output"));
            observed.recv_timeout(Duration::from_secs(5)).unwrap();
            let before = DROPPED.load(Ordering::Relaxed);
            let mut conn = rusqlite::Connection::open_in_memory().unwrap();
            let tx = immediate(&mut conn).unwrap();
            tx.execute_batch("CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(1)").unwrap();
            for _ in 0..600 { drop(Phase::new("while_transaction_open")); }
            tx.commit().unwrap();
            // Reaching this release while the subscriber is still blocked
            // proves that full-queue emission did not wait for that sink.
            assert!(DROPPED.load(Ordering::Relaxed) > before);
            release.send(()).unwrap();
            assert_eq!(conn.query_row("SELECT value FROM sample", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
        });
        let (ack, delivered) = mpsc::sync_channel(1);
        sink().unwrap().send(Message::Flush(ack)).unwrap();
        delivered.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
