//! Interactive PTY channel over the native SSH connection — docs/specs/remote-seats.md#spec-17-5.
//!
//! `execute()` is deliberately single-command: it consumes the session. A
//! seat needs the opposite: one authenticated session, one PTY channel that
//! stays open for the seat's lifetime, a live event stream for the terminal
//! state machine, and an explicit close that observes the session end.
//!
//! This module adds that without changing the single-command path: the PTY
//! handshake (pty-req → accept → exec/shell → accept) happens inside
//! `open_pty`, and the returned session yields [`NativePtyEvent`]s until the
//! channel closes. Every event is delivered to the caller; nothing is
//! buffered away behind a screen.

use super::{NativeExit, NativeInterruption, NativeSshConnection, NativeSshError, NativeSshSession};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Sig, client};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

/// What to start on the PTY. `Exec` runs one exact remote command (the
/// caller persists intent before calling); `Shell` starts the remote login
/// shell with no command string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NativePtyProgram {
    Exec { command: Vec<u8> },
    Shell,
}

/// PTY characteristics and program. The caller picks every field; nothing is
/// discovered from the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePtyRequest {
    pub term: String,
    pub cols: u32,
    pub rows: u32,
    pub program: NativePtyProgram,
}

/// One live PTY event. `Output`/`Error` carry raw bytes for the terminal
/// state machine; `Exit` is the remote exit status or signal; `Closed` ends
/// the stream and `recv` returns `None` from then on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePtyEvent {
    Output(Vec<u8>),
    Error(Vec<u8>),
    Exit(NativeExit),
    Eof,
    Closed,
}

/// Close-time observation for one PTY session. Not a proof of application
/// postconditions; the exit status is what the remote reported.
#[derive(Debug, Serialize)]
pub struct NativePtyObservation {
    pub schema: &'static str,
    pub pty_accepted: bool,
    pub program_accepted: bool,
    pub channel_closed: bool,
    pub exit: Option<NativeExit>,
    pub output_bytes: usize,
    pub error_bytes: usize,
    /// Frames that arrived after `Close`. A seat never hides these behind a
    /// buffer: a nonzero count is surfaced as `interruption: Protocol`.
    pub post_close_frames: u64,
    pub interruption: Option<NativeInterruption>,
    pub session_end_observed: bool,
}

/// A live PTY channel holding its SSH session open.
pub struct NativePtySession {
    connection: NativeSshConnection,
    read: ChannelReadHalf,
    write: ChannelWriteHalf<client::Msg>,
    pty_accepted: bool,
    program_accepted: bool,
    exit: Option<NativeExit>,
    output_bytes: usize,
    error_bytes: usize,
    post_close_frames: u64,
    closed: bool,
}

impl NativeSshSession {
    /// Open a PTY channel and start its program. Consumes the single-use
    /// session; the returned value keeps the connection alive until closed.
    pub async fn open_pty(mut self, request: NativePtyRequest, handshake: Duration)
        -> Result<NativePtySession, NativeSshError>
    {
        if request.term.is_empty() || request.term.len() > 64 || request.cols == 0
            || request.cols > 512 || request.rows == 0 || request.rows > 128
            || handshake.is_zero()
        { return Err(NativeSshError::InvalidLimits); }
        if let NativePtyProgram::Exec { command } = &request.program {
            if command.is_empty() || command.len() > 64 * 1024 || command.contains(&0) {
                return Err(NativeSshError::InvalidLimits);
            }
        }
        let deadline = Instant::now().checked_add(handshake).ok_or(NativeSshError::InvalidLimits)?;
        let channel = timeout_at(deadline, self.0.handle.channel_open_session()).await
            .map_err(|_| NativeSshError::Deadline)?
            .map_err(|_| NativeSshError::Transport)?;
        channel.request_pty(true, &request.term, request.cols, request.rows, 0, 0, &[]).await
            .map_err(|_| NativeSshError::Transport)?;
        let (mut read, write) = channel.split();
        await_acceptance(&mut read, deadline).await?;
        let pty_accepted = true;
        match &request.program {
            NativePtyProgram::Exec { command } =>
                channel_exec(&write, command).await?,
            NativePtyProgram::Shell =>
                write.request_shell(true).await.map_err(|_| NativeSshError::Transport)?,
        }
        await_acceptance(&mut read, deadline).await?;
        Ok(NativePtySession {
            connection: self.0,
            read,
            write,
            pty_accepted,
            program_accepted: true,
            exit: None,
            output_bytes: 0,
            error_bytes: 0,
            post_close_frames: 0,
            closed: false,
        })
    }
}

async fn channel_exec(write: &ChannelWriteHalf<client::Msg>, command: &[u8])
    -> Result<(), NativeSshError>
{
    write.exec(true, command.to_vec()).await.map_err(|_| NativeSshError::Transport)
}

/// Wait for exactly one channel request acceptance. `Failure` is the
/// server's real rejection; anything else before acceptance is a protocol
/// violation, not a soft retry.
async fn await_acceptance(read: &mut ChannelReadHalf, deadline: Instant) -> Result<(), NativeSshError> {
    match timeout_at(deadline, read.wait()).await {
        Err(_) => Err(NativeSshError::Deadline),
        Ok(Some(ChannelMsg::Success)) => Ok(()),
        Ok(Some(ChannelMsg::Failure)) => Err(NativeSshError::Connect),
        Ok(Some(_)) => Err(NativeSshError::InvalidBinding),
        Ok(None) => Err(NativeSshError::Transport),
    }
}

impl NativePtySession {
    /// Next live event, or `None` once the channel has closed.
    pub async fn recv(&mut self) -> Option<NativePtyEvent> {
        if self.closed { return None; }
        loop {
            let message = self.read.wait().await?;
            let event = match message {
                ChannelMsg::Data { data } => {
                    self.output_bytes += data.len();
                    Some(NativePtyEvent::Output(data.to_vec()))
                }
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    self.error_bytes += data.len();
                    Some(NativePtyEvent::Error(data.to_vec()))
                }
                ChannelMsg::ExtendedData { .. } => return Some(NativePtyEvent::Error(vec![])),
                ChannelMsg::ExitStatus { exit_status } => {
                    if self.exit.is_some() {
                        self.post_close_frames += 1;
                        continue;
                    }
                    self.exit = Some(NativeExit::Status { code: exit_status });
                    Some(NativePtyEvent::Exit(NativeExit::Status { code: exit_status }))
                }
                ChannelMsg::ExitSignal { signal_name, core_dumped, .. } => {
                    if self.exit.is_some() {
                        self.post_close_frames += 1;
                        continue;
                    }
                    let exit = NativeExit::Signal {
                        name: format!("{signal_name:?}"),
                        core_dumped,
                    };
                    self.exit = Some(exit.clone());
                    Some(NativePtyEvent::Exit(exit))
                }
                ChannelMsg::Eof => Some(NativePtyEvent::Eof),
                ChannelMsg::Close => {
                    self.closed = true;
                    Some(NativePtyEvent::Closed)
                }
                _ => None,
            };
            if let Some(event) = event { return Some(event); }
        }
    }

    /// Write bytes to the PTY (keys, pasted input, answer input).
    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), NativeSshError> {
        if self.closed { return Err(NativeSshError::Transport); }
        self.write.data(bytes).await.map_err(|_| NativeSshError::Transport)
    }

    /// Resize the PTY after a terminal size change.
    pub async fn resize(&mut self, cols: u32, rows: u32) -> Result<(), NativeSshError> {
        if cols == 0 || cols > 512 || rows == 0 || rows > 128 {
            return Err(NativeSshError::InvalidLimits);
        }
        self.write.window_change(cols, rows, 0, 0).await.map_err(|_| NativeSshError::Transport)
    }

    /// Signal the remote process group (Ctrl-C is `Sig::INT`).
    pub async fn signal(&mut self, signal: Sig) -> Result<(), NativeSshError> {
        self.write.signal(signal).await.map_err(|_| NativeSshError::Transport)
    }

    /// Half-close: we will send no more input. The channel stays readable
    /// until the remote closes it.
    pub async fn eof(&mut self) -> Result<(), NativeSshError> {
        self.write.eof().await.map_err(|_| NativeSshError::Transport)
    }

    /// Close the channel, drain anything already in flight within `grace`,
    /// then close the owned stream and observe the session end. Frames that
    /// arrive after `Close` are counted, never dropped silently.
    pub async fn close(&mut self, grace: Duration) -> NativePtyObservation {
        let _ = self.write.close().await;
        let drain_deadline = Instant::now() + grace;
        while Instant::now() < drain_deadline {
            match timeout(drain_deadline - Instant::now(), self.read.wait()).await {
                Ok(Some(message)) => match message {
                    ChannelMsg::Close => { self.closed = true; break; }
                    ChannelMsg::Data { data } => { self.output_bytes += data.len(); self.post_close_frames += 1; }
                    ChannelMsg::ExtendedData { data, .. } => { self.error_bytes += data.len(); self.post_close_frames += 1; }
                    ChannelMsg::ExitStatus { exit_status } => {
                        if self.exit.is_none() { self.exit = Some(NativeExit::Status { code: exit_status }); }
                    }
                    ChannelMsg::ExitSignal { signal_name, core_dumped, .. } => {
                        if self.exit.is_none() {
                            self.exit = Some(NativeExit::Signal {
                                name: format!("{signal_name:?}"),
                                core_dumped,
                            });
                        }
                    }
                    _ => {}
                },
                _ => break,
            }
        }
        // Do not rely on Handle::drop; close the owned stream and explicitly
        // observe the session's end, same contract as the single-command path.
        self.connection.close.close();
        let session_end_observed = timeout(grace, &mut self.connection.handle).await.is_ok();
        let interruption = if self.post_close_frames > 0 {
            Some(NativeInterruption::Protocol)
        } else if !self.closed {
            Some(NativeInterruption::Transport)
        } else {
            None
        };
        NativePtyObservation {
            schema: "ssh.native-pty.v1",
            pty_accepted: self.pty_accepted,
            program_accepted: self.program_accepted,
            channel_closed: self.closed,
            exit: self.exit.clone(),
            output_bytes: self.output_bytes,
            error_bytes: self.error_bytes,
            post_close_frames: self.post_close_frames,
            interruption,
            session_end_observed,
        }
    }
}
