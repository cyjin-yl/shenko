//! The process and reader barrier shared by managed stdio seats.
//!
//! Readers own protocol state only. This owner retains the child, stdin and
//! every reader handle, so the readers cannot keep their own process alive.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use shenko_types::Digest;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin},
    sync::{Mutex, watch},
    task::JoinHandle,
};

const CLOSE_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedWriteState {
    NotSent,
    Unknown,
}

/// A transport observation, never evidence that the remote accepted a prompt.
#[derive(Debug, thiserror::Error)]
#[error("managed seat write {state:?}: {reason} (request {request_id:?}, wire {wire_digest})")]
pub struct ManagedWriteError {
    pub state: ManagedWriteState,
    pub request_id: Option<String>,
    pub wire_digest: Digest,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedWriteIdentity {
    pub request_id: Option<String>,
    pub wire_digest: Digest,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedCloseReceipt {
    pub process_id: Option<u32>,
    pub exit_status: String,
    pub readers_joined: usize,
    pub write_outcome_unknown: bool,
    /// The original write identity survives cancellation of its caller.
    pub unknown_write: Option<ManagedWriteIdentity>,
}

struct Readers(Vec<JoinHandle<Result<()>>>);

impl Drop for Readers {
    fn drop(&mut self) {
        // Abrupt owner loss is not a successful close barrier. Abort retained
        // readers instead of leaving detached tasks holding pipe descriptors.
        for reader in &self.0 {
            reader.abort();
        }
    }
}

pub(super) struct ManagedProcess {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    process_id: Option<u32>,
    readers: Mutex<Readers>,
    reader_count: usize,
    closing: watch::Sender<bool>,
    write_unknown: AtomicBool,
    last_write: Mutex<Option<ManagedWriteIdentity>>,
    close_gate: Mutex<()>,
    close_receipt: Mutex<Option<ManagedCloseReceipt>>,
    close_error: Mutex<Option<String>>,
}

impl ManagedProcess {
    pub(super) fn new(
        child: Child,
        stdin: ChildStdin,
        readers: Vec<JoinHandle<Result<()>>>,
    ) -> Self {
        let process_id = child.id();
        let reader_count = readers.len();
        let (closing, _) = watch::channel(false);
        Self {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(child),
            process_id,
            readers: Mutex::new(Readers(readers)),
            reader_count,
            closing,
            write_unknown: AtomicBool::new(false),
            last_write: Mutex::new(None),
            close_gate: Mutex::new(()),
            close_receipt: Mutex::new(None),
            close_error: Mutex::new(None),
        }
    }

    pub(super) fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    pub(super) async fn transport_live(&self) -> Result<bool> {
        Ok(self.child.lock().await.try_wait()?.is_none())
    }

    pub(super) async fn write_frame(&self, frame: &Value) -> Result<()> {
        let mut wire = serde_json::to_vec(frame)?;
        wire.push(b'\n');
        let request_id = frame.get("id").or_else(|| frame.get("request_id"))
            .or_else(|| frame.get("response").and_then(|response| response.get("request_id")))
            .and_then(Value::as_str).map(str::to_owned);
        let wire_digest = Digest::blake3(&wire);
        let failure = |state, reason: String| ManagedWriteError {
            state, request_id: request_id.clone(), wire_digest: wire_digest.clone(), reason,
        };
        let mut closing = self.closing.subscribe();
        if *closing.borrow() {
            return Err(failure(ManagedWriteState::NotSent, "owner is closing".into()).into());
        }
        let mut guard = tokio::select! {
            biased;
            _ = closing.changed() => return Err(failure(ManagedWriteState::NotSent, "owner closed before writer admission".into()).into()),
            _guard = self.stdin.lock() => _guard,
        };
        if *closing.borrow() || guard.is_none() || self.write_unknown.load(Ordering::Acquire) {
            return Err(failure(ManagedWriteState::NotSent, "stdin is closed or an earlier write is unknown".into()).into());
        }
        let stdin = guard.as_mut().context("managed stdin disappeared after admission")?;
        // A cancelled future may have written a prefix. Keep this latch set
        // unless this exact write and flush finish; another frame must not be
        // appended to a potentially partial JSON line.
        *self.last_write.lock().await = Some(ManagedWriteIdentity {
            request_id: request_id.clone(),
            wire_digest: wire_digest.clone(),
        });
        self.write_unknown.store(true, Ordering::Release);
        let result = tokio::select! {
            biased;
            _ = closing.changed() => Err(failure(ManagedWriteState::Unknown, "owner closed during the original write".into())),
            _result = async {
                stdin.write_all(&wire).await?;
                stdin.flush().await
            } => _result.map_err(|error| failure(ManagedWriteState::Unknown, error.to_string())),
        };
        match result {
            Ok(()) => {
                self.write_unknown.store(false, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                *guard = None;
                Err(error.into())
            }
        }
    }

    pub(super) async fn close(&self) -> Result<ManagedCloseReceipt> {
        self.closing.send_replace(true);
        let _gate = self.close_gate.lock().await;
        // Keep every observed failure in the owner before the next await.
        // A cancelled closer must not discard an error after popping the
        // reader that produced it, then let a retry attest a clean barrier.
        let mut close_error = self.close_error.lock().await;
        if let Some(receipt) = self.close_receipt.lock().await.as_ref() {
            if let Some(error) = close_error.as_ref() {
                anyhow::bail!("{error}");
            }
            return Ok(receipt.clone());
        }
        // A retry may finish previously unjoined resources; it cannot erase
        // an earlier failed barrier and claim that the original close passed.
        match tokio::time::timeout(CLOSE_WAIT, self.stdin.lock()).await {
            Ok(mut stdin) => *stdin = None,
            Err(_) => retain_close_error(&mut close_error,
                "managed stdin writer did not release after close".into()),
        }
        let mut exit_status = None;
        {
            let mut child = self.child.lock().await;
            match child.try_wait() {
                Ok(Some(status)) => exit_status = Some(status.to_string()),
                Ok(None) => {
                    if let Err(error) = child.start_kill() {
                        retain_close_error(&mut close_error, format!("managed child stop: {error}"));
                    }
                    match tokio::time::timeout(CLOSE_WAIT, child.wait()).await {
                        Ok(Ok(status)) => exit_status = Some(status.to_string()),
                        Ok(Err(error)) => retain_close_error(&mut close_error,
                            format!("managed child join: {error}")),
                        Err(_) => retain_close_error(&mut close_error,
                            "managed child did not join within the close budget".into()),
                    }
                }
                Err(error) => retain_close_error(&mut close_error, format!("managed child status: {error}")),
            }
        }
        let mut readers = self.readers.lock().await;
        while let Some(reader) = readers.0.last_mut() {
            match tokio::time::timeout(CLOSE_WAIT, &mut *reader).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => retain_close_error(&mut close_error,
                    format!("managed reader: {error:#}")),
                Ok(Err(error)) => retain_close_error(&mut close_error,
                    format!("managed reader join: {error}")),
                Err(_) => {
                    retain_close_error(&mut close_error,
                        "managed reader did not finish after child stop".into());
                    reader.abort();
                    if tokio::time::timeout(CLOSE_WAIT, &mut *reader).await.is_err() {
                        retain_close_error(&mut close_error,
                            "managed reader abort remained unjoined".into());
                        break;
                    }
                }
            }
            drop(readers.0.pop());
        }
        if let Some(error) = close_error.as_ref() {
            anyhow::bail!("{error}");
        }
        ensure!(readers.0.is_empty(), "managed readers remain unjoined");
        let write_outcome_unknown = self.write_unknown.load(Ordering::Acquire);
        let receipt = ManagedCloseReceipt {
            process_id: self.process_id,
            exit_status: exit_status.context("managed child exit was not observed")?,
            readers_joined: self.reader_count,
            write_outcome_unknown,
            unknown_write: if write_outcome_unknown {
                self.last_write.lock().await.clone()
            } else {
                None
            },
        };
        *self.close_receipt.lock().await = Some(receipt.clone());
        Ok(receipt)
    }
}

fn retain_close_error(stored: &mut Option<String>, error: String) {
    match stored {
        Some(previous) => {
            previous.push_str("; ");
            previous.push_str(&error);
        }
        None => *stored = Some(error),
    }
}
