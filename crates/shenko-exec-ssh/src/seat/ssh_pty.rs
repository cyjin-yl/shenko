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

mod pump;
pub use pump::PtyCloseError;

use crate::native_transport::{
    NativePtyObservation, NativePtySession, NativePtyWriteInput, NativePtyWriteReceipt,
};
use crate::seat::tmux::{ensure_terminal_question, infer_questions, map_answer_to_terminal_input};
use crate::seat::{
    AnswerTransport, PreflightRefusalKind, SeatAdapter, SeatAdapterId,
    SeatAnswer, SeatEffectPlan, SeatHandle, SeatObservation, SeatPreflight,
    SeatQuestionCandidate, SeatResponse, SeatWriteFence,
};
use crate::terminal::{TerminalMode, TerminalScreen};
use anyhow::{Context, Result};
use async_trait::async_trait;
use shenko_store::SeatCapability;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Adapter version bound into durable seat records.
pub const SSH_PTY_ADAPTER_VERSION: &str = "ssh-pty-v1";

/// One live remote seat: observation and the physical writer belong to the
/// same pump. Input is acknowledged only at the local SSH flush boundary.
pub struct SshPtySeat {
    handle: SeatHandle,
    screen: Arc<Mutex<TerminalScreen>>,
    writer: pump::Writer,
    pump: Mutex<pump::Owner>,
}

impl SshPtySeat {
    pub fn spawn(session: NativePtySession, handle: SeatHandle, columns: u16, rows: u16) -> Self {
        let screen = Arc::new(Mutex::new(TerminalScreen::new(columns, rows, TerminalMode::Stream)));
        let (writer, owner) = pump::spawn(session, Arc::clone(&screen));
        Self { handle, screen, writer, pump: Mutex::new(owner) }
    }

    pub fn screen(&self) -> Arc<Mutex<TerminalScreen>> { Arc::clone(&self.screen) }

    pub fn is_stream_open(&self) -> bool { self.writer.is_open() }

    /// At most the 32 outstanding original commands plus the last completed
    /// command. This is local evidence, not a new durable Store authority.
    pub fn write_observations(&self) -> Vec<NativePtyWriteReceipt> { self.writer.observations() }

    /// Stop admission and retain the original pump handle until its join is
    /// observed. Cancelling a closer leaves ownership and prior errors intact.
    pub async fn close(&self) -> Result<NativePtyObservation> {
        self.writer.request_close();
        self.pump.lock().await.close().await
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
        self.writer.write(NativePtyWriteInput::Data {
            bytes: plan.payload.as_bytes().to_vec(),
        }, fence.lease_epoch).await
    }

    async fn submit(
        &self,
        _seat: &SeatHandle,
        text: &str,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.writer.write(NativePtyWriteInput::Data { bytes }, fence.lease_epoch).await
    }

    async fn interrupt(&self, _seat: &SeatHandle, fence: &SeatWriteFence) -> Result<SeatResponse> {
        self.writer.write(NativePtyWriteInput::SignalInt, fence.lease_epoch).await
    }
}
