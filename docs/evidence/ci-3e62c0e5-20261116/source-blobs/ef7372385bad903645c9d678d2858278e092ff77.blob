//! Five transport windows, using actual authenticated SSH packets and PTY
//! requests. The stream gate delays/fails a real I/O boundary; it never
//! supplies an application response, channel observation, or receipt.
use super::{key, target};
use crate::native_transport::*;
use crate::native_transport::stream::{CloseGuard, owned};
use crate::seat::{
    AnswerConfirmation, SeatAdapter, SeatAdapterId, SeatHandle, SeatWriteFence,
    ssh_pty::{SSH_PTY_ADAPTER_VERSION, SshPtySeat},
};
use anyhow::{Context, Result};
use russh::{Channel, ChannelId, ChannelMsg, Sig, keys::{PrivateKeyWithHashAlg, PublicKey}, server};
use serde::Serialize;
use shenko_types::{EntityId, RecordRevision, SeatIncarnation, TaskRevision};
use std::{
    future::Future, io, pin::Pin, sync::{Arc, Mutex}, task::{Context as TaskContext, Poll, Waker},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf, duplex},
    sync::{Notify, oneshot}, task::JoinHandle, time::timeout,
};

const WAIT: Duration = Duration::from_secs(5);
const ORIGINAL_ERROR: &str = "fixture original underlying PTY flush error";
const OUTPUT_MARKER: &str = "PTY-FIXTURE-OUTPUT-COMPLETE";

#[derive(Clone, Copy, PartialEq, Eq)]
enum GateMode { Pass, HoldWrite, HoldFlush, FailFlush }

#[derive(Default, Clone, Serialize)]
struct IoObservation {
    write_polls: u64,
    bytes_written: u64,
    flush_polls: u64,
    flushes_completed: u64,
    blocked_writes: u64,
    blocked_flushes: u64,
}

struct GateState { mode: GateMode, observation: IoObservation, wake: Option<Waker> }

#[derive(Clone)]
struct Gate { state: Arc<Mutex<GateState>>, changed: Arc<Notify> }

impl Gate {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(GateState {
                mode: GateMode::Pass, observation: IoObservation::default(), wake: None,
            })), changed: Arc::new(Notify::new()),
        }
    }
    fn set(&self, mode: GateMode) {
        let wake = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.mode = mode;
            state.wake.take()
        };
        if let Some(wake) = wake { wake.wake(); }
    }
    fn observation(&self) -> IoObservation {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).observation.clone()
    }
    async fn blocked(&self, mode: GateMode) {
        loop {
            let changed = self.changed.notified();
            let observed = self.observation();
            if (mode == GateMode::HoldWrite && observed.blocked_writes != 0)
                || (mode == GateMode::HoldFlush && observed.blocked_flushes != 0) { return; }
            changed.await;
        }
    }
}

struct GatedStream { inner: DuplexStream, gate: Gate }

impl AsyncRead for GatedStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>)
        -> Poll<io::Result<()>>
    { Pin::new(&mut self.inner).poll_read(cx, buf) }
}

impl AsyncWrite for GatedStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8])
        -> Poll<io::Result<usize>>
    {
        {
            let mut state = self.gate.state.lock().unwrap_or_else(|error| error.into_inner());
            state.observation.write_polls += 1;
            if state.mode == GateMode::HoldWrite {
                state.observation.blocked_writes += 1;
                state.wake = Some(cx.waker().clone());
                self.gate.changed.notify_one();
                return Poll::Pending;
            }
        }
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(bytes)) = &result {
            self.gate.state.lock().unwrap_or_else(|error| error.into_inner())
                .observation.bytes_written += *bytes as u64;
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        {
            let mut state = self.gate.state.lock().unwrap_or_else(|error| error.into_inner());
            state.observation.flush_polls += 1;
            match state.mode {
                GateMode::HoldFlush => {
                    state.observation.blocked_flushes += 1;
                    state.wake = Some(cx.waker().clone());
                    self.gate.changed.notify_one();
                    return Poll::Pending;
                }
                GateMode::FailFlush => return Poll::Ready(Err(io::Error::other(ORIGINAL_ERROR))),
                _ => {}
            }
        }
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if let Poll::Ready(Ok(())) = &result {
            self.gate.state.lock().unwrap_or_else(|error| error.into_inner())
                .observation.flushes_completed += 1;
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Default, Clone, Serialize)]
struct Received { input: Vec<u8>, signals: Vec<String>, public_key_authentications: u64 }

type Worker = JoinHandle<Result<()>>;

struct Peer {
    expected: PublicKey,
    channel: Option<Channel<server::Msg>>,
    channel_id: Arc<Mutex<Option<ChannelId>>>,
    received: Arc<Mutex<Received>>,
    changed: Arc<Notify>,
    workers: Arc<Mutex<Vec<Worker>>>,
    output: bool,
}

impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, user: &str, verified: server::VerifiedPublicKey<'_>)
        -> Result<server::Auth, Self::Error>
    {
        self.received.lock().unwrap_or_else(|error| error.into_inner()).public_key_authentications += 1;
        Ok(if user == "fixture" && verified.public_key.key_data() == self.expected.key_data() {
            server::Auth::Accept
        } else { server::Auth::reject() })
    }
    async fn channel_open_session(&mut self, channel: Channel<server::Msg>, reply: server::ChannelOpenHandle,
        _: &mut server::Session) -> Result<(), Self::Error>
    {
        *self.channel_id.lock().unwrap_or_else(|error| error.into_inner()) = Some(channel.id());
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(&mut self, channel: ChannelId, _term: &str, _cols: u32, _rows: u32,
        _pixels_x: u32, _pixels_y: u32, _modes: &[(russh::Pty, u32)], session: &mut server::Session)
        -> Result<(), Self::Error>
    { session.channel_success(channel) }

    async fn exec_request(&mut self, channel: ChannelId, command: &[u8], session: &mut server::Session)
        -> Result<(), Self::Error>
    {
        if command != b"fixture-command" { return Err(russh::Error::Disconnect); }
        session.channel_success(channel)?;
        let channel = self.channel.take().ok_or(russh::Error::Disconnect)?;
        let received = Arc::clone(&self.received);
        let changed = Arc::clone(&self.changed);
        let output = self.output;
        let worker = tokio::spawn(async move {
            let (mut read, write) = channel.split();
            let receive = async {
                while let Some(message) = read.wait().await {
                    match message {
                        ChannelMsg::Data { data } => {
                            received.lock().unwrap_or_else(|error| error.into_inner()).input.extend_from_slice(&data);
                            changed.notify_one();
                        }
                        ChannelMsg::Close => break,
                        _ => {}
                    }
                }
            };
            let send = async {
                if output {
                    let mut bytes = vec![b'o'; 128 * 1024];
                    let ending = format!("\r\n{OUTPUT_MARKER}\r\n");
                    let start = bytes.len() - ending.len();
                    bytes[start..].copy_from_slice(ending.as_bytes());
                    write.data(&bytes[..]).await?;
                }
                Ok::<(), russh::Error>(())
            };
            let ((), sent) = tokio::join!(receive, send);
            sent?;
            Ok(())
        });
        self.workers.lock().unwrap_or_else(|error| error.into_inner()).push(worker);
        Ok(())
    }
    async fn signal(&mut self, _channel: ChannelId, signal: Sig, _: &mut server::Session)
        -> Result<(), Self::Error>
    {
        self.received.lock().unwrap_or_else(|error| error.into_inner()).signals.push(format!("{signal:?}"));
        self.changed.notify_one();
        Ok(())
    }
}

enum ProtocolEnd {
    SetupFailed(String),
    Joined { result: String, join_failed: bool },
}

struct ProtocolOwner {
    observer: Option<JoinHandle<ProtocolEnd>>,
    close: CloseGuard,
}

impl Drop for ProtocolOwner {
    fn drop(&mut self) {
        self.close.close();
        // Last-resort observer abort is not a protocol-session join receipt.
        // Any such missing receipt remains an explicit cleanup failure below.
        if let Some(observer) = &self.observer { observer.abort(); }
    }
}

struct WorkerOwners(Vec<Worker>);

impl Drop for WorkerOwners {
    fn drop(&mut self) { for worker in &self.0 { worker.abort(); } }
}

#[derive(Debug, Default, Serialize)]
struct Cleanup {
    protocol_end_observed: bool,
    observer_results: Vec<String>,
    worker_results: Vec<String>,
    failures: Vec<String>,
}

async fn observe_protocol(owner: &mut ProtocolOwner, report: &mut Cleanup) {
    if let Some(observer) = owner.observer.as_mut() {
        match timeout(WAIT, &mut *observer).await {
            Ok(Ok(ProtocolEnd::Joined { result, join_failed })) => {
                // The original observer has actually awaited RunningSession.
                // A normal Err is an observed protocol end, not remote success.
                report.protocol_end_observed = true;
                report.observer_results.push(format!("original running session completed: {result}"));
                if join_failed { report.failures.push(format!("original protocol task join failed: {result}")); }
                drop(owner.observer.take());
            }
            Ok(Ok(ProtocolEnd::SetupFailed(error))) => {
                report.observer_results.push(format!("original server did not create a running session: {error}"));
                report.failures.push(format!("server protocol setup failed: {error}"));
                drop(owner.observer.take());
            }
            Ok(Err(error)) => {
                report.failures.push(format!("server observer joined without a protocol result: {error}"));
                report.observer_results.push(format!("observer join: {error}"));
                drop(owner.observer.take());
            }
            Err(error) => {
                report.failures.push(format!("server protocol end not observed within stop budget: {error}"));
                report.observer_results.push("original session join remains unknown".into());
            }
        }
    }
}

async fn cleanup(owner: &mut ProtocolOwner, workers: &Arc<Mutex<Vec<Worker>>>) -> Cleanup {
    // Stop the actual server stream before awaiting its observer. Aborting the
    // outer task alone would only drop RunningSession's inner JoinHandle.
    owner.close.close();
    let mut report = Cleanup::default();
    observe_protocol(owner, &mut report).await;
    let mut workers = WorkerOwners(std::mem::take(&mut *workers.lock().unwrap_or_else(|error| error.into_inner())));
    for worker in &mut workers.0 {
        match timeout(WAIT, &mut *worker).await {
            Ok(Ok(Ok(()))) => report.worker_results.push("original reader/output worker joined".into()),
            Ok(result) => {
                report.worker_results.push(format!("original reader/output worker result: {result:?}"));
                report.failures.push(format!("peer reader/output worker: {result:?}"));
            }
            Err(error) => {
                worker.abort();
                let joined = timeout(WAIT, &mut *worker).await;
                report.worker_results.push(format!("original worker stop timeout; abort join={joined:?}"));
                report.failures.push(format!("peer worker timeout: {error}; abort join={joined:?}"));
            }
        }
    }
    // If draining/stopping a reader was needed, still await the same original
    // session. A prior timeout is retained even if this later join completes.
    if owner.observer.is_some() { observe_protocol(owner, &mut report).await; }
    report
}

struct Fixture {
    protocol: ProtocolOwner,
    handle: server::Handle,
    channel_id: Arc<Mutex<Option<ChannelId>>>,
    received: Arc<Mutex<Received>>,
    changed: Arc<Notify>,
    workers: Arc<Mutex<Vec<Worker>>>,
    gate: Gate,
}

impl Fixture {
    async fn open(output: bool) -> Result<(Self, NativePtySession)> {
        let (client_io, server_io) = duplex(1024);
        let (server_io, server_close) = owned(server_io);
        let received = Arc::new(Mutex::new(Received::default()));
        let changed = Arc::new(Notify::new());
        let workers = Arc::new(Mutex::new(Vec::new()));
        let channel_id = Arc::new(Mutex::new(None));
        let gate = Gate::new();
        let peer = Peer {
            expected: key(22).public_key().clone(), channel: None, channel_id: Arc::clone(&channel_id),
            received: Arc::clone(&received), changed: Arc::clone(&changed), workers: Arc::clone(&workers), output,
        };
        let config = Arc::new(server::Config {
            keys: vec![key(11)], window_size: 64 * 1024, maximum_packet_size: 32 * 1024,
            channel_buffer_size: 4, event_buffer_size: 4, auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO), inactivity_timeout: Some(Duration::from_secs(15)),
            ..Default::default()
        });
        let (started, ready) = oneshot::channel();
        let server = tokio::spawn(async move {
            let running = match server::run_stream(config, server_io, peer).await {
                Ok(running) => running,
                Err(error) => return ProtocolEnd::SetupFailed(format!("{error:?}")),
            };
            let _ = started.send(running.handle());
            let result = running.await;
            ProtocolEnd::Joined {
                join_failed: matches!(&result, Err(russh::Error::Join(_))),
                result: format!("{result:?}"),
            }
        });
        let mut protocol = ProtocolOwner { observer: Some(server), close: server_close };
        let opened: Result<_> = async {
            let connection = NativeSshConnection::connect_stream(target(key(11).public_key(), false),
                GatedStream { inner: client_io, gate: gate.clone() }, WAIT).await?;
            let handle = timeout(WAIT, ready).await??;
            let authenticated = connection.authenticate_private_key(
                PrivateKeyWithHashAlg::new(Arc::new(key(22)), None)).await?;
            let session = authenticated.open_pty(NativePtyRequest {
                term: "xterm".into(), cols: 80, rows: 24,
                program: NativePtyProgram::Exec { command: b"fixture-command".to_vec() },
            }, WAIT).await?;
            Ok((handle, session))
        }.await;
        let (handle, session) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let stopped = cleanup(&mut protocol, &workers).await;
                eprintln!("PTY original setup failure: {error:#}");
                eprintln!("PTY setup cleanup: {}", serde_json::to_string(&stopped)?);
                return Err(error.context(format!("fixture setup failed; original cleanup={stopped:?}")));
            }
        };
        Ok((Self { protocol, handle, channel_id, received, changed, workers, gate }, session))
    }

    async fn observed(&self, condition: impl Fn(&Received) -> bool) -> Result<()> {
        timeout(WAIT, async {
            loop {
                let changed = self.changed.notified();
                if condition(&self.received.lock().unwrap_or_else(|error| error.into_inner())) { return; }
                changed.await;
            }
        }).await.context("original peer observation deadline")
    }

    async fn close_channel(&self) -> Result<()> {
        let channel = self.channel_id.lock().unwrap_or_else(|error| error.into_inner())
            .context("fixture channel was not established")?;
        timeout(WAIT, self.handle.close(channel)).await?
            .map_err(|()| anyhow::anyhow!("peer channel close could not be queued"))
    }

    async fn finish(&mut self) -> Result<()> {
        let stopped = cleanup(&mut self.protocol, &self.workers).await;
        eprintln!("PTY original protocol/worker cleanup: {}", serde_json::to_string(&stopped)?);
        eprintln!("PTY original peer data: {}", serde_json::to_string(&*self.received.lock().unwrap_or_else(|error| error.into_inner()))?);
        eprintln!("PTY underlying I/O observation: {}", serde_json::to_string(&self.gate.observation())?);
        anyhow::ensure!(stopped.protocol_end_observed && stopped.failures.is_empty(), "original cleanup failed: {stopped:?}");
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.protocol.close.close();
        for worker in self.workers.lock().unwrap_or_else(|error| error.into_inner()).iter() { worker.abort(); }
    }
}

fn seat_handle() -> SeatHandle {
    SeatHandle {
        location_id: "fixture-pty-flush".into(), adapter: SeatAdapterId::SshPty,
        adapter_version: SSH_PTY_ADAPTER_VERSION.into(), host: "fixture.invalid".into(),
        cwd: None, remote_session_id: None, current_command: None, process_identity: None,
    }
}

fn fence() -> SeatWriteFence {
    SeatWriteFence {
        lease_id: EntityId::new(), lease_epoch: 7, fence_capability: "fixture-transport-attribution-only".into(),
        seat_id: EntityId::new(), seat_incarnation: SeatIncarnation(1), task_id: EntityId::new(),
        task_revision: TaskRevision(1), question_id: EntityId::new(), question_revision: 1,
        question_record_revision: RecordRevision(1),
    }
}

async fn reach_gate<F: Future>(future: Pin<&mut F>, gate: &Gate, mode: GateMode) -> Result<()>
where F::Output: std::fmt::Debug
{
    timeout(WAIT, async {
        tokio::select! {
            result = future => Err(anyhow::anyhow!("input completed before its held underlying boundary: {result:?}")),
            () = gate.blocked(mode) => Ok(()),
        }
    }).await?
}

async fn close_seat(fixture: &mut Fixture, seat: &SshPtySeat) -> Result<NativePtyObservation> {
    let close = timeout(Duration::from_secs(12), seat.close()).await;
    eprintln!("PTY original retained writes: {}", serde_json::to_string(&seat.write_observations())?);
    eprintln!("PTY original close result: {close:?}");
    let stopped = fixture.finish().await;
    let observation = close??;
    stopped?;
    anyhow::ensure!(observation.session_end_observed);
    Ok(observation)
}

#[test]
fn original_pty_writes_observe_flush_cancel_errors_and_channel_loss() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        // 1. The actual first data flush is held. More than both SSH windows
        // flows in both directions once released, and submit's CR is retained.
        // 2. INT is a later same-FIFO command, never an application acceptance.
        let (mut fixture, session) = Fixture::open(true).await?;
        let seat = SshPtySeat::spawn(session, seat_handle(), 80, 24);
        let checked: Result<()> = async {
            let handle = seat_handle();
            let fence = fence();
            let text = "i".repeat(128 * 1024);
            let mut expected = text.as_bytes().to_vec();
            expected.push(b'\r');
            fixture.gate.set(GateMode::HoldFlush);
            let before = fixture.gate.observation();
            let mut pending = Box::pin(seat.submit(&handle, &text, &fence));
            reach_gate(pending.as_mut(), &fixture.gate, GateMode::HoldFlush).await?;
            // Incoming output can cause an earlier real WINDOW_ADJUST flush
            // to reach the same gate. Also observe the original command's
            // own admission to the physical writer before asserting its state.
            timeout(WAIT, async {
                loop {
                    if seat.write_observations().first()
                        .is_some_and(|record| record.state == NativePtyWriteState::Writing) { break; }
                    tokio::task::yield_now().await;
                }
            }).await?;
            let records = seat.write_observations();
            anyhow::ensure!(records.len() == 1 && records[0].state == NativePtyWriteState::Writing);
            anyhow::ensure!(records[0].input == NativePtyWriteInput::Data { bytes: expected.clone() });
            anyhow::ensure!(fixture.gate.observation().flushes_completed == before.flushes_completed);
            fixture.gate.set(GateMode::Pass);
            let response = timeout(WAIT, pending).await??;
            anyhow::ensure!(response.confirmation == AnswerConfirmation::Unconfirmed);
            let detail: serde_json::Value = serde_json::from_str(response.detail.as_deref().context("missing original data receipt")?)?;
            anyhow::ensure!(detail["state"] == "locally_flushed");
            anyhow::ensure!(detail["input"]["bytes"] == serde_json::to_value(&expected)?);
            fixture.observed(|received| received.input == expected).await?;
            // Peer input completion does not prove reverse-direction output
            // completion. Observe the actual last output bytes in the original
            // screen before close can remove the SSH channel and reject late data.
            timeout(WAIT, async {
                loop {
                    if seat.screen().lock().await.grid().trimmed_text().contains(OUTPUT_MARKER) { break; }
                    tokio::task::yield_now().await;
                }
            }).await?;
            eprintln!("PTY first actual flush receipt: {}", response.detail.context("lost data receipt")?);
            let signal = timeout(WAIT, seat.interrupt(&handle, &fence)).await??;
            anyhow::ensure!(signal.confirmation == AnswerConfirmation::Unconfirmed);
            fixture.observed(|received| received.signals == ["INT"] && received.input == expected).await?;
            let receipt = seat.write_observations();
            anyhow::ensure!(receipt.len() == 1 && receipt[0].state == NativePtyWriteState::LocallyFlushed);
            anyhow::ensure!(receipt[0].input == NativePtyWriteInput::SignalInt && receipt[0].writer_epoch == 7);
            anyhow::ensure!(fixture.received.lock().unwrap_or_else(|error| error.into_inner()).public_key_authentications == 1);
            Ok(())
        }.await;
        let closed = close_seat(&mut fixture, &seat).await;
        checked?;
        anyhow::ensure!(closed?.output_bytes == 128 * 1024, "full duplex output was not observed");

        // 3. Cancel the original write at a real blocked I/O poll. A second
        // admitted command never starts; opening the gate cannot revive either.
        let (mut fixture, session) = Fixture::open(false).await?;
        let seat = SshPtySeat::spawn(session, seat_handle(), 80, 24);
        let checked: Result<()> = async {
            let handle = seat_handle();
            let fence = fence();
            fixture.gate.set(GateMode::HoldWrite);
            let mut original = Box::pin(seat.submit(&handle, "original-cancelled", &fence));
            reach_gate(original.as_mut(), &fixture.gate, GateMode::HoldWrite).await?;
            let original_id = seat.write_observations().first().context("original command missing")?.write_id;
            let mut queued = Box::pin(seat.submit(&handle, "queued-never-sent", &fence));
            let pending = std::future::poll_fn(|cx| Poll::Ready(queued.as_mut().poll(cx).is_pending())).await;
            anyhow::ensure!(pending && seat.write_observations().len() == 2);
            let at_cancel = fixture.gate.observation();
            drop(original);
            fixture.gate.set(GateMode::Pass);
            let refused = timeout(WAIT, queued).await??;
            anyhow::ensure!(matches!(refused.confirmation, AnswerConfirmation::NotSent { .. }));
            let records = seat.write_observations();
            let original = records.iter().find(|record| record.write_id == original_id).context("cancelled original lost")?;
            anyhow::ensure!(original.state == NativePtyWriteState::Unknown && original.local_stream_closed);
            anyhow::ensure!(original.input == NativePtyWriteInput::Data { bytes: b"original-cancelled\r".to_vec() });
            let queued = records.iter().find(|record| record.write_id != original_id).context("queued original lost")?;
            anyhow::ensure!(queued.state == NativePtyWriteState::NotSent);
            anyhow::ensure!(queued.input == NativePtyWriteInput::Data { bytes: b"queued-never-sent\r".to_vec() });
            let closed = timeout(Duration::from_secs(12), seat.close()).await??;
            anyhow::ensure!(closed.session_end_observed);
            let after = fixture.gate.observation();
            anyhow::ensure!(after.write_polls == at_cancel.write_polls && after.bytes_written == at_cancel.bytes_written);
            anyhow::ensure!(fixture.received.lock().unwrap_or_else(|error| error.into_inner()).input.is_empty());
            Ok(())
        }.await;
        let closed = close_seat(&mut fixture, &seat).await;
        checked?;
        closed?;

        // 4. The original underlying flush error reaches Unknown with the full
        // input; later admission is refused and does not become a retry.
        let (mut fixture, session) = Fixture::open(false).await?;
        let seat = SshPtySeat::spawn(session, seat_handle(), 80, 24);
        let checked: Result<()> = async {
            let handle = seat_handle();
            let fence = fence();
            fixture.gate.set(GateMode::HoldFlush);
            let mut pending = Box::pin(seat.submit(&handle, "original-flush-error", &fence));
            reach_gate(pending.as_mut(), &fixture.gate, GateMode::HoldFlush).await?;
            fixture.gate.set(GateMode::FailFlush);
            let response = timeout(WAIT, pending).await??;
            anyhow::ensure!(matches!(&response.confirmation, AnswerConfirmation::Unknown { reason } if reason.contains(ORIGINAL_ERROR)));
            let records = seat.write_observations();
            anyhow::ensure!(records.len() == 1 && records[0].state == NativePtyWriteState::Unknown);
            anyhow::ensure!(records[0].input == NativePtyWriteInput::Data { bytes: b"original-flush-error\r".to_vec() });
            anyhow::ensure!(response.detail.as_deref().is_some_and(|detail| detail.contains(ORIGINAL_ERROR)));
            let refused = timeout(WAIT, seat.submit(&handle, "not-a-retry", &fence)).await??;
            anyhow::ensure!(matches!(refused.confirmation, AnswerConfirmation::NotSent { .. }));
            Ok(())
        }.await;
        let closed = close_seat(&mut fixture, &seat).await;
        checked?;
        closed?;

        // 5. A peer closes the channel while the original ciphertext cannot
        // flush. The actual barrier must refuse even after the stream unblocks.
        let (mut fixture, mut session) = Fixture::open(false).await?;
        let checked: Result<()> = async {
            fixture.gate.set(GateMode::HoldWrite);
            let (reader, writer) = session.split_io();
            let mut pending = Box::pin(writer.send(b"original-closed-channel"));
            reach_gate(pending.as_mut(), &fixture.gate, GateMode::HoldWrite).await?;
            fixture.close_channel().await?;
            let observed_close = timeout(WAIT, async {
                while let Some(event) = reader.recv().await {
                    if event == NativePtyEvent::Closed { return true; }
                }
                false
            }).await?;
            anyhow::ensure!(observed_close, "peer channel close was not actually observed");
            fixture.gate.set(GateMode::Pass);
            let result = timeout(WAIT, pending).await?;
            eprintln!("PTY original closed-channel flush result: {result:?}");
            let error = result.err().context("removed channel incorrectly received a successful flush receipt")?;
            anyhow::ensure!(error.to_string().contains("channel closed before its outbound flush boundary"));
            Ok(())
        }.await;
        let close = timeout(Duration::from_secs(12), session.close(Duration::from_millis(500))).await;
        eprintln!("PTY original removed-channel close observation: {close:?}");
        let stopped = fixture.finish().await;
        checked?;
        anyhow::ensure!(close?.session_end_observed);
        stopped?;
        Ok(())
    })
}
