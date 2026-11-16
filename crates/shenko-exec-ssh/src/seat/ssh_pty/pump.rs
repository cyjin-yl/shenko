//! The one physical PTY owner: bounded admission, same-FIFO flush receipts,
//! cancellation, and retained close/join. No Store or lease authority is granted.

use crate::native_transport::{
    NativePtyEvent, NativePtyObservation, NativePtySession, NativePtyStopHandle,
    NativePtyWriteInput, NativePtyWriteReceipt, NativePtyWriteState, NativeSshError,
};
use crate::seat::{AnswerConfirmation, AnswerTransport, SeatAdapterId, SeatResponse};
use crate::terminal::TerminalScreen;
use anyhow::{Context, Result};
use russh::Sig;
use shenko_types::{EntityId, object::Digest};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as SyncMutex, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const COMMAND_LIMIT: usize = 32;
const CLOSE_GRACE: Duration = Duration::from_millis(500);
const PUMP_JOIN_WAIT: Duration = Duration::from_secs(5);

struct WriteStatus {
    state: NativePtyWriteState,
    error: Option<String>,
    local_stream_closed: bool,
}

struct WriteRecord {
    id: EntityId,
    input: NativePtyWriteInput,
    input_digest: String,
    writer_epoch: u64,
    status: SyncMutex<WriteStatus>,
}

impl WriteRecord {
    fn new(input: NativePtyWriteInput, writer_epoch: u64) -> Self {
        let bytes = match &input {
            NativePtyWriteInput::Data { bytes } => bytes.as_slice(),
            NativePtyWriteInput::SignalInt => b"INT",
        };
        Self {
            id: EntityId::new(), input_digest: Digest::blake3(bytes).0, input, writer_epoch,
            status: SyncMutex::new(WriteStatus {
                state: NativePtyWriteState::Queued, error: None, local_stream_closed: false,
            }),
        }
    }

    fn snapshot(&self) -> NativePtyWriteReceipt {
        let status = self.status.lock().unwrap_or_else(|error| error.into_inner());
        NativePtyWriteReceipt {
            schema: "ssh.pty-write.v1", write_id: self.id, input: self.input.clone(),
            input_digest: self.input_digest.clone(), writer_epoch: self.writer_epoch,
            state: status.state, error: status.error.clone(),
            local_stream_closed: status.local_stream_closed,
        }
    }

    fn refuse(&self, reason: String) {
        let mut status = self.status.lock().unwrap_or_else(|error| error.into_inner());
        status.state = NativePtyWriteState::NotSent;
        status.error = Some(reason);
    }
}

struct Command {
    record: Arc<WriteRecord>,
    completed: oneshot::Sender<()>,
}

#[derive(Default)]
struct Journal {
    stopped: Option<String>,
    outstanding: VecDeque<Arc<WriteRecord>>,
    last_completed: Option<Arc<WriteRecord>>,
}

struct Control {
    journal: SyncMutex<Journal>,
    stop: NativePtyStopHandle,
    closing: watch::Sender<bool>,
    slots: Arc<Semaphore>,
    open: AtomicBool,
}

impl Control {
    fn admit(&self, command: Command, permit: mpsc::Permit<'_, Command>) -> Result<(), String> {
        // Admission and stop share this short local lock. No await, SQLite
        // lock, or external I/O occurs while it is held. An outstanding queue
        // permit cannot enqueue application bytes after stop has won.
        let mut journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(reason) = &journal.stopped { return Err(reason.clone()); }
        journal.outstanding.push_back(Arc::clone(&command.record));
        permit.send(command);
        Ok(())
    }

    fn begin(&self, record: &WriteRecord) -> bool {
        let journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        if journal.stopped.is_some() { return false; }
        let mut status = record.status.lock().unwrap_or_else(|error| error.into_inner());
        if status.state != NativePtyWriteState::Queued { return false; }
        status.state = NativePtyWriteState::Writing;
        true
    }

    fn stop_locked(&self, journal: &mut Journal, reason: &str, force_stream: bool) {
        let writing = journal.outstanding.iter().any(|record| {
            record.status.lock().unwrap_or_else(|error| error.into_inner()).state
                == NativePtyWriteState::Writing
        });
        let stream_closed = force_stream || writing;
        if stream_closed {
            // Serialize with the original OwnedStream's actual nonblocking
            // I/O poll. Once this returns no queued application command can
            // perform another inner write, even before this task is polled.
            self.stop.stop_writes();
        }
        if journal.stopped.is_none() { journal.stopped = Some(reason.into()); }
        self.slots.close();
        self.open.store(false, Ordering::SeqCst);
        for record in &journal.outstanding {
            let mut status = record.status.lock().unwrap_or_else(|error| error.into_inner());
            match status.state {
                NativePtyWriteState::Queued => status.state = NativePtyWriteState::NotSent,
                NativePtyWriteState::Writing => status.state = NativePtyWriteState::Unknown,
                _ => continue,
            }
            status.error = Some(reason.into());
            status.local_stream_closed = stream_closed;
        }
        self.closing.send_replace(true);
    }

    fn stop(&self, reason: &str, force_stream: bool) {
        let mut journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        self.stop_locked(&mut journal, reason, force_stream);
    }

    fn finish(&self, record: &WriteRecord, result: Result<(), NativeSshError>) {
        let mut journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        match result {
            Ok(()) => {
                let mut status = record.status.lock().unwrap_or_else(|error| error.into_inner());
                // A concurrent cancellation can already have settled Unknown.
                // A later-polled successful future never erases that fact.
                if status.state == NativePtyWriteState::Writing {
                    status.state = NativePtyWriteState::LocallyFlushed;
                }
            }
            Err(error) => {
                let original = format!("{error:#}");
                self.stop_locked(&mut journal, &original, true);
                let mut status = record.status.lock().unwrap_or_else(|error| error.into_inner());
                // Preserve a separately observed original transport error even
                // if cancellation settled the same command immediately before it.
                if status.error.as_deref() != Some(original.as_str()) {
                    status.error = Some(match status.error.take() {
                        Some(previous) => format!("{previous}; {original}"),
                        None => original,
                    });
                }
            }
        }
    }

    fn acknowledged(&self, record: &Arc<WriteRecord>) {
        let mut journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        if journal.stopped.is_none() {
            journal.outstanding.retain(|other| other.id != record.id);
            journal.last_completed = Some(Arc::clone(record));
        }
    }

    fn snapshots(&self) -> Vec<NativePtyWriteReceipt> {
        let journal = self.journal.lock().unwrap_or_else(|error| error.into_inner());
        journal.last_completed.iter().chain(journal.outstanding.iter())
            .map(|record| record.snapshot()).collect()
    }
}

struct PendingWrite {
    control: Arc<Control>,
    armed: bool,
    // Capacity includes replies whose caller has not resumed yet, not just
    // messages still in the mpsc queue. Retention is at most 32 plus last.
    _slot: OwnedSemaphorePermit,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        if self.armed {
            self.control.stop("original PTY write caller was cancelled; input must not be replayed", true);
        }
    }
}

pub(super) struct Writer {
    commands: mpsc::Sender<Command>,
    slots: Arc<Semaphore>,
    control: Arc<Control>,
}

impl Writer {
    pub(super) fn is_open(&self) -> bool { self.control.open.load(Ordering::SeqCst) }

    pub(super) fn request_close(&self) { self.control.stop("PTY owner is closing", false); }

    pub(super) fn observations(&self) -> Vec<NativePtyWriteReceipt> { self.control.snapshots() }

    pub(super) async fn write(&self, input: NativePtyWriteInput, writer_epoch: u64) -> Result<SeatResponse> {
        let record = Arc::new(WriteRecord::new(input, writer_epoch));
        let slot = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(slot) => slot,
            Err(_) => {
                record.refuse("PTY owner stopped before this command was admitted".into());
                return response(&record.snapshot());
            }
        };
        let mut pending = PendingWrite { control: Arc::clone(&self.control), armed: false, _slot: slot };
        let (completed, reply) = oneshot::channel();
        match self.commands.reserve().await {
            Ok(permit) => {
                match self.control.admit(Command { record: Arc::clone(&record), completed }, permit) {
                    Ok(()) => pending.armed = true,
                    Err(reason) => record.refuse(reason),
                }
            }
            Err(_) => record.refuse("PTY pump ended before this command was admitted".into()),
        }
        if pending.armed && reply.await.is_err() {
            self.control.stop("original PTY pump ended without a write receipt; input must not be replayed", true);
        }
        let response = response(&record.snapshot())?;
        self.control.acknowledged(&record);
        pending.armed = false;
        Ok(response)
    }
}

fn response(receipt: &NativePtyWriteReceipt) -> Result<SeatResponse> {
    let confirmation = match receipt.state {
            NativePtyWriteState::LocallyFlushed => AnswerConfirmation::Unconfirmed,
            NativePtyWriteState::NotSent => AnswerConfirmation::NotSent {
                reason: receipt.error.clone().unwrap_or_else(|| "PTY input was not dispatched".into()),
            },
            _ => AnswerConfirmation::Unknown {
                reason: receipt.error.clone().unwrap_or_else(|| "PTY local write/flush was not observed".into()),
            },
    };
    Ok(SeatResponse {
            adapter: SeatAdapterId::SshPty, transport: AnswerTransport::TerminalInput,
            confirmation, detail: Some(serde_json::to_string(receipt)?),
    })
}

struct Pump {
    session: NativePtySession,
    commands: mpsc::Receiver<Command>,
    close_rx: watch::Receiver<bool>,
    screen: Arc<Mutex<TerminalScreen>>,
    control: Arc<Control>,
    active: Option<Command>,
}

async fn wait_for_close(closing: &mut watch::Receiver<bool>) {
    while !*closing.borrow_and_update() {
        if closing.changed().await.is_err() { return; }
    }
}

enum PumpEvent {
    Stop,
    Remote(Option<NativePtyEvent>),
    Command(Option<Command>),
}

enum WriteEvent {
    Stop,
    Remote(Option<NativePtyEvent>),
    Finished(Result<(), NativeSshError>),
}

impl Pump {
    async fn write_active(&mut self, record: &WriteRecord) {
        if !self.control.begin(record) { return; }
        let (receiver, writer) = self.session.split_io();
        let mut writing = Box::pin(async {
            match &record.input {
                NativePtyWriteInput::Data { bytes } => writer.send(bytes).await,
                NativePtyWriteInput::SignalInt => writer.signal(Sig::INT).await,
            }
        });
        loop {
            let event = tokio::select! {
                biased;
                () = wait_for_close(&mut self.close_rx) => WriteEvent::Stop,
                _result = &mut writing => WriteEvent::Finished(_result),
                _event = receiver.recv() => WriteEvent::Remote(_event),
            };
            match event {
                WriteEvent::Stop => break,
                WriteEvent::Finished(result) => {
                    self.control.finish(record, result);
                    break;
                }
                WriteEvent::Remote(Some(NativePtyEvent::Output(bytes) | NativePtyEvent::Error(bytes))) => {
                    self.screen.lock().await.feed(&bytes);
                }
                WriteEvent::Remote(_) => {
                    self.control.stop("PTY remote stream ended before the original write receipt", true);
                    break;
                }
            }
        }
    }

    async fn run(mut self) -> NativePtyObservation {
        loop {
            let event = tokio::select! {
                biased;
                () = wait_for_close(&mut self.close_rx) => PumpEvent::Stop,
                _event = self.session.recv() => PumpEvent::Remote(_event),
                _command = self.commands.recv() => PumpEvent::Command(_command),
            };
            match event {
                PumpEvent::Stop | PumpEvent::Command(None) => break,
                PumpEvent::Remote(Some(NativePtyEvent::Output(bytes) | NativePtyEvent::Error(bytes))) => {
                    self.screen.lock().await.feed(&bytes);
                }
                PumpEvent::Remote(_) => break,
                PumpEvent::Command(Some(command)) => {
                    let record = Arc::clone(&command.record);
                    // The original command/receipt sender remains owned across
                    // every await, including a cancelled or aborted pump.
                    self.active = Some(command);
                    self.write_active(&record).await;
                    if let Some(command) = self.active.take() { let _ = command.completed.send(()); }
                }
            }
        }
        self.control.stop("PTY pump stopped before queued commands were dispatched", false);
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() { let _ = command.completed.send(()); }
        let mut observation = self.session.close(CLOSE_GRACE).await;
        observation.writes = self.control.snapshots();
        observation
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        // This executes even if close/join is cancelled or the task is aborted.
        // Close the actual stream before dropping any queued command senders.
        self.control.stop("PTY physical pump dropped before its close/join completed", true);
        self.commands.close();
    }
}

pub(super) struct Owner {
    handle: Option<JoinHandle<NativePtyObservation>>,
    observation: Option<NativePtyObservation>,
    failure: Option<String>,
    control: Arc<Control>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.control.stop("PTY owner dropped; original stream is locally closed", true);
        if let Some(handle) = &self.handle { handle.abort(); }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("PTY close failed: {reason}; remote input/outcome remains unknown")]
pub struct PtyCloseError {
    pub reason: String,
    pub pump_joined: bool,
    pub observation: Option<NativePtyObservation>,
    pub writes: Vec<NativePtyWriteReceipt>,
}

impl Owner {
    pub(super) async fn close(&mut self) -> Result<NativePtyObservation> {
        if self.handle.is_some() {
            let first_join = {
                let handle = self.handle.as_mut().context("PTY pump owner disappeared")?;
                tokio::time::timeout(PUMP_JOIN_WAIT, &mut *handle).await
            };
            let joined = match first_join {
                Ok(result) => Some(result),
                Err(_) => {
                    self.failure = Some(match self.failure.take() {
                        Some(previous) => format!("{previous}; PTY pump exceeded its stop budget"),
                        None => "PTY pump exceeded its stop budget".into(),
                    });
                    self.control.stop("PTY pump exceeded its stop budget", true);
                    let handle = self.handle.as_mut().context("PTY pump owner disappeared")?;
                    handle.abort();
                    tokio::time::timeout(PUMP_JOIN_WAIT, &mut *handle).await.ok()
                }
            };
            if let Some(result) = joined {
                match result {
                    Ok(observation) => self.observation = Some(observation),
                    Err(error) => {
                        let detail = format!("PTY pump join: {error}");
                        self.failure = Some(match self.failure.take() {
                            Some(previous) => format!("{previous}; {detail}"),
                            None => detail,
                        });
                    }
                }
                drop(self.handle.take());
            }
        }
        let reason = self.failure.clone().or_else(|| match &self.observation {
            Some(observation) if observation.session_end_observed => None,
            Some(_) => Some("local SSH session end was not observed".into()),
            None => Some("PTY pump has no close observation".into()),
        });
        if let Some(reason) = reason {
            self.failure = Some(reason.clone());
            return Err(PtyCloseError {
                reason, pump_joined: self.handle.is_none(), observation: self.observation.clone(),
                writes: self.control.snapshots(),
            }.into());
        }
        self.observation.clone().context("PTY close observation disappeared")
    }
}

pub(super) fn spawn(session: NativePtySession, screen: Arc<Mutex<TerminalScreen>>) -> (Writer, Owner) {
    let (commands, command_rx) = mpsc::channel(COMMAND_LIMIT);
    let (closing, close_rx) = watch::channel(false);
    let slots = Arc::new(Semaphore::new(COMMAND_LIMIT));
    let control = Arc::new(Control {
        journal: SyncMutex::new(Journal::default()), stop: session.stop_handle(), closing,
        open: AtomicBool::new(true), slots: Arc::clone(&slots),
    });
    let pump = Pump {
        session, commands: command_rx, close_rx, screen, control: Arc::clone(&control), active: None,
    };
    let handle = tokio::spawn(pump.run());
    (Writer { commands, slots, control: Arc::clone(&control) },
        Owner { handle: Some(handle), observation: None, failure: None, control })
}
