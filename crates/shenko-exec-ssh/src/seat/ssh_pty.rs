//! Remote SSH seat adapter over a live PTY.
//!
//! A managed agent running on a remote host has no tmux control mode and no
//! JSONL RPC. What it has is a terminal. This adapter owns the PTY session
//! from `native_transport::open_pty` through one single-owner pump task:
//! the pump selects between incoming channel events (fed into a
//! [`TerminalScreen`]) and writer commands, so `respond`/`submit`/`interrupt`
//! never queue behind a held session lock.
//!
//! Question inference uses exactly the same narrow rules as the tmux path:
//! a question is reported only when the screen shows one, options are the
//! ones actually visible, and the selection comes only from a preserved
//! highlight or a cursor marker — never invented. Answers are typed through
//! [`map_answer_to_terminal_input`](crate::seat::tmux::map_answer_to_terminal_input),
//! including its refusal to blind-type y/N.

use crate::native_transport::{NativePtyEvent, NativePtyObservation, NativePtySession};
use crate::seat::tmux::{ensure_terminal_question, infer_questions, map_answer_to_terminal_input};
use crate::seat::{
    AnswerConfirmation, AnswerTransport, PreflightRefusalKind, SeatAdapter, SeatAdapterId,
    SeatAnswer, SeatEffectPlan, SeatHandle, SeatObservation, SeatPreflight,
    SeatQuestionCandidate, SeatResponse, SeatWriteFence,
};
use crate::terminal::{TerminalMode, TerminalScreen};
use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::Sig;
use shenko_store::SeatCapability;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

/// Adapter version bound into durable seat records.
pub const SSH_PTY_ADAPTER_VERSION: &str = "ssh-pty-v1";

const CLOSE_GRACE: Duration = Duration::from_millis(500);
const PUMP_JOIN_WAIT: Duration = Duration::from_secs(5);

/// Writer-originated commands handled by the single-owner pump.
enum PtyCommand {
    Send(Vec<u8>),
    Resize(u32, u32),
    Signal(Sig),
    Eof,
}

/// One live remote seat: a screen everyone reads, and a command channel
/// that serializes every input effect through the pump that owns the PTY.
pub struct SshPtySeat {
    handle: SeatHandle,
    screen: Arc<Mutex<TerminalScreen>>,
    commands: mpsc::Sender<PtyCommand>,
    stream_open: Arc<AtomicBool>,
    closing: watch::Sender<bool>,
    pump: Mutex<PumpOwner>,
}

struct PumpOwner {
    handle: Option<JoinHandle<NativePtyObservation>>,
    observation: Option<NativePtyObservation>,
    failure: Option<String>,
}

impl Drop for PumpOwner {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            // Abrupt owner loss never attests a successful protocol join.
            handle.abort();
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("PTY close failed: {reason}; remote input/outcome remains unknown")]
pub struct PtyCloseError {
    pub reason: String,
    pub pump_joined: bool,
    pub observation: Option<NativePtyObservation>,
}

impl SshPtySeat {
    /// Start pumping `session` into a fresh screen. The pump owns the
    /// session; every writer effect is a command ordered through
    /// `commands`, so a slow or dead remote can never wedge a caller
    /// behind a lock the pump holds.
    pub fn spawn(session: NativePtySession, handle: SeatHandle, columns: u16, rows: u16) -> Self {
        let (commands, mut command_rx) = mpsc::channel::<PtyCommand>(32);
        let screen = Arc::new(Mutex::new(TerminalScreen::new(
            columns,
            rows,
            TerminalMode::Stream,
        )));
        let pump_screen = Arc::clone(&screen);
        let stream_open = Arc::new(AtomicBool::new(true));
        let pump_open = Arc::clone(&stream_open);
        let (closing, mut close_rx) = watch::channel(false);
        let pump = tokio::spawn(async move {
            let mut session = session;
            loop {
                tokio::select! {
                    biased;
                    _ = close_rx.changed() => break,
                    _event = session.recv() => {
                        match _event {
                            Some(NativePtyEvent::Output(bytes)) |
                            Some(NativePtyEvent::Error(bytes)) => {
                                pump_screen.lock().await.feed(&bytes);
                            }
                            Some(NativePtyEvent::Exit(_))
                            | Some(NativePtyEvent::Eof)
                            | Some(NativePtyEvent::Closed)
                            | None => break,
                        }
                    }
                    _command = command_rx.recv() => {
                        let Some(command) = _command else { break; };
                        // Shutdown must also interrupt a writer blocked on
                        // SSH backpressure. Dropping this original write does
                        // not turn its queued/partial input into acceptance.
                        tokio::select! {
                            biased;
                            _ = close_rx.changed() => break,
                            _result = async {
                                match command {
                                    PtyCommand::Send(bytes) => session.send(&bytes).await,
                                    PtyCommand::Resize(cols, rows) => session.resize(cols, rows).await,
                                    PtyCommand::Signal(signal) => session.signal(signal).await,
                                    PtyCommand::Eof => session.eof().await,
                                }
                            } => {
                                if _result.is_err() { break; }
                            }
                        }
                    }
                }
            }
            pump_open.store(false, Ordering::SeqCst);
            session.close(CLOSE_GRACE).await
        });
        Self {
            handle,
            screen,
            commands,
            stream_open,
            closing,
            pump: Mutex::new(PumpOwner {
                handle: Some(pump), observation: None, failure: None,
            }),
        }
    }

    pub fn screen(&self) -> Arc<Mutex<TerminalScreen>> {
        Arc::clone(&self.screen)
    }

    pub fn is_stream_open(&self) -> bool {
        self.stream_open.load(Ordering::SeqCst)
    }

    /// Stop admission, interrupt the original pump and retain its handle
    /// until its join is observed. A cancelled closer leaves this owner intact.
    /// Remote process exit and application effects remain separate observations.
    pub async fn close(&self) -> Result<NativePtyObservation> {
        self.closing.send_replace(true);
        self.stream_open.store(false, Ordering::SeqCst);
        let mut owner = self.pump.lock().await;
        if owner.handle.is_some() {
            let first_join = {
                let handle = owner.handle.as_mut().context("PTY pump owner disappeared")?;
                tokio::time::timeout(PUMP_JOIN_WAIT, &mut *handle).await
            };
            let joined = match first_join {
                Ok(result) => Some(result),
                Err(_) => {
                    // Retain the failed stop barrier before abort/join can
                    // suspend again. Cancelling this closer cannot erase it.
                    owner.failure = Some(match owner.failure.take() {
                        Some(previous) => format!("{previous}; PTY pump exceeded its stop budget"),
                        None => "PTY pump exceeded its stop budget".into(),
                    });
                    let handle = owner.handle.as_mut().context("PTY pump owner disappeared")?;
                    handle.abort();
                    tokio::time::timeout(PUMP_JOIN_WAIT, &mut *handle).await.ok()
                }
            };
            if let Some(result) = joined {
                // The completed handle is removed only after its result is
                // retained, never before an await that a caller can cancel.
                match result {
                    Ok(observation) => owner.observation = Some(observation),
                    Err(error) => {
                        let detail = format!("PTY pump join: {error}");
                        owner.failure = Some(match owner.failure.take() {
                            Some(previous) => format!("{previous}; {detail}"),
                            None => detail,
                        });
                    }
                }
                drop(owner.handle.take());
            }
        }
        let reason = owner.failure.clone().or_else(|| {
            match owner.observation.as_ref() {
                Some(observation) if observation.session_end_observed => None,
                Some(_) => Some("local SSH session end was not observed".into()),
                None => Some("PTY pump has no close observation".into()),
            }
        });
        if let Some(reason) = reason {
            owner.failure = Some(reason.clone());
            return Err(PtyCloseError {
                reason,
                pump_joined: owner.handle.is_none(),
                observation: owner.observation.clone(),
            }.into());
        }
        owner.observation.clone().context("PTY close observation disappeared")
    }

}

#[async_trait]
impl SeatAdapter for SshPtySeat {
    fn adapter_id(&self) -> SeatAdapterId {
        SeatAdapterId::SshPty
    }

    fn adapter_version(&self) -> &str {
        SSH_PTY_ADAPTER_VERSION
    }

    fn capability(&self) -> SeatCapability {
        SeatCapability {
            structured_questions: false,
            structured_answers: false,
            screen_observation: true,
            steer: true,
            interrupt: true,
            resume: false,
            notes: vec![
                "问题从 PTY 终端画面推断，规则与 tmux seat 完全相同".into(),
                "回答通过 PTY 输入映射键入，绝不盲打 y/N".into(),
            ],
        }
    }

    async fn discover(&self) -> Result<Vec<SeatHandle>> {
        Ok(vec![self.handle.clone()])
    }

    async fn transport_live(&self, _seat: &SeatHandle) -> Result<bool> {
        Ok(self.is_stream_open())
    }

    async fn observe(&self, seat: &SeatHandle) -> Result<SeatObservation> {
        let grid = self.screen.lock().await.grid();
        let questions = infer_questions(&grid, seat);
        let screen_text = grid.trimmed_text();
        let mut unknown = vec![];
        if !self.is_stream_open() {
            unknown.push("PTY 输出流已结束；画面是最终状态，不是实时状态".to_string());
        }
        Ok(SeatObservation {
            seat: seat.clone(),
            observation_revision: grid.revision,
            observed_at: chrono::Utc::now(),
            screen: Some(grid),
            screen_text,
            questions,
            unknown,
            raw_evidence: Vec::new(),
        })
    }

    async fn plan_effect(
        &self,
        _seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        answer: &SeatAnswer,
    ) -> Result<Option<SeatEffectPlan>> {
        ensure_terminal_question(question)?;
        let typed = map_answer_to_terminal_input(question, answer)?;
        let bytes = typed.context("该回答没有对应的终端输入映射；未键入任何内容")?;
        Ok(Some(SeatEffectPlan::new(
            AnswerTransport::TerminalInput,
            bytes.clone(),
            format!("向远程 PTY 键入 {} 字节", bytes.len()),
        )))
    }

    async fn preflight(
        &self,
        seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        _plan: &SeatEffectPlan,
    ) -> Result<SeatPreflight> {
        if !self.is_stream_open() {
            return Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::StateChanged,
                reason: "PTY 输出流已结束；seat 不再是活的远程位置".into(),
            });
        }
        // Re-read the live screen: the decision was made against an older
        // frame, and the remote agent may have moved on since.
        let grid = self.screen.lock().await.grid();
        let live = infer_questions(&grid, seat);
        if !live
            .iter()
            .any(|candidate| candidate.question_id == question.question_id)
        {
            return Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: "远程画面上已没有这个问题：可能已被远端处理，或程序已退出到 shell".into(),
            });
        }
        Ok(SeatPreflight::Ready {
            detail: format!("远程 PTY 仍在运行，问题仍在画面上（revision {}）", grid.revision),
        })
    }

    async fn execute_effect(
        &self,
        _seat: &SeatHandle,
        plan: &SeatEffectPlan,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        anyhow::ensure!(!*self.closing.borrow(), "PTY owner is closing; no new input admitted");
        self.commands
            .send(PtyCommand::Send(plan.payload.as_bytes().to_vec()))
            .await
            .map_err(|_| anyhow::anyhow!("PTY 命令通道已关闭，seat 已结束"))?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::SshPty,
            transport: AnswerTransport::TerminalInput,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "已排队 {} 字节（writer epoch {}）；实际传输和远端接收尚未确认",
                plan.payload.len(),
                fence.lease_epoch
            )),
        })
    }

    async fn submit(
        &self,
        _seat: &SeatHandle,
        text: &str,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        anyhow::ensure!(!*self.closing.borrow(), "PTY owner is closing; no new input admitted");
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.commands
            .send(PtyCommand::Send(bytes))
            .await
            .map_err(|_| anyhow::anyhow!("PTY 命令通道已关闭，seat 已结束"))?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::SshPty,
            transport: AnswerTransport::TerminalInput,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "输入及回车已排队（writer epoch {}）；实际传输和远端接收尚未确认",
                fence.lease_epoch
            )),
        })
    }

    async fn interrupt(&self, _seat: &SeatHandle, fence: &SeatWriteFence) -> Result<SeatResponse> {
        anyhow::ensure!(!*self.closing.borrow(), "PTY owner is closing; no new signal admitted");
        self.commands
            .send(PtyCommand::Signal(Sig::INT))
            .await
            .map_err(|_| anyhow::anyhow!("PTY 命令通道已关闭，seat 已结束"))?;
        Ok(SeatResponse {
            adapter: SeatAdapterId::SshPty,
            transport: AnswerTransport::TerminalInput,
            confirmation: AnswerConfirmation::Unconfirmed,
            detail: Some(format!(
                "SIGINT 已排队发送（writer epoch {}）",
                fence.lease_epoch
            )),
        })
    }
}
