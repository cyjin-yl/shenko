//! tmux seat adapter — the "existing foreground TUI" path (SPEC §35.5).
//!
//! This adapter observes an already running tmux pane and answers what it
//! sees. It is deliberately the *compatibility* path: it reconstructs the
//! screen from `tmux capture-pane -p -e` (which keeps the escape sequences
//! that carry foreground/background/reverse attributes) and infers a
//! question from that screen.
//!
//! What it will not do:
//!
//! * It will not pick an option by position. A selected option is reported
//!   only when the screen actually marked it — by a preserved highlight
//!   attribute, or by the cursor glyph the TUI drew in front of it.
//! * It will not send a bare `y`/`n` to an unidentified prompt. A yes/no
//!   keystroke is only sent when the observed prompt really was a yes/no
//!   prompt and the decision was an allow/deny.
//! * It will not claim the answer was applied. tmux offers no atomic
//!   "check the question and type the answer" operation, so a write proves
//!   transport only; the remaining race with an interleaved human is
//!   reported, not hidden.

use crate::{
    seat::{
        AnswerConfirmation, AnswerTransport, Confidence, ProcessIdentity, QuestionKind,
        QuestionSourceKind, SeatAdapter, SeatAdapterId, SeatAnswer, SeatCapability, SeatEffectPlan,
        PreflightRefusalKind, SeatHandle, SeatObservation, SeatOption, SeatPreflight,
        SeatQuestionCandidate, SeatResponse, SeatWriteFence, inferred_question_id,
    },
    ssh_exec::{SeatRefusal, TmuxSeat},
    terminal::{ScreenGrid, TerminalMode, TerminalScreen},
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};

/// Adapter version bound into durable seat records.
pub const TMUX_ADAPTER_VERSION: &str = "tmux-capture-v1";

/// Markers a TUI draws in front of the option it currently has selected.
const CURSOR_MARKERS: &[&str] = &["❯", "›", "➤"];

/// A tmux pane adapter bound to one host (local, or an SSH target).
pub struct TmuxSeatAdapter {
    seat: TmuxSeat,
    observation_seq: AtomicU64,
}

impl TmuxSeatAdapter {
    pub fn local() -> Self {
        Self {
            seat: TmuxSeat::local(),
            observation_seq: AtomicU64::new(0),
        }
    }

    pub fn remote(target: crate::SshTarget) -> Self {
        Self {
            seat: TmuxSeat::remote(target),
            observation_seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl SeatAdapter for TmuxSeatAdapter {
    fn adapter_id(&self) -> SeatAdapterId {
        SeatAdapterId::Tmux
    }

    fn adapter_version(&self) -> &str {
        TMUX_ADAPTER_VERSION
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
                "问题由终端画面推断，保留置信与未知；原生结构化事件优先于它".into(),
                "tmux 没有原子的“核对问题并输入”，写入只证明传输，人工并发竞态仍然存在".into(),
                "画面消失不证明答案被接受".into(),
            ],
        }
    }

    async fn discover(&self) -> Result<Vec<SeatHandle>> {
        let seats = self.seat.discover_agent_seats().await?;
        Ok(seats
            .into_iter()
            .map(|agent| SeatHandle {
                location_id: agent.pane.pane_id.clone(),
                adapter: SeatAdapterId::Tmux,
                adapter_version: TMUX_ADAPTER_VERSION.into(),
                host: "local".into(),
                cwd: Some(agent.pane.cwd.clone()),
                remote_session_id: None,
                current_command: Some(agent.pane.current_command.clone()),
                process_identity: Some(ProcessIdentity {
                    pid: Some(agent.pane.pane_pid),
                    start_token: None,
                    executable: Some(agent.pane.current_command.clone()),
                }),
            })
            .collect())
    }

    async fn observe(&self, seat: &SeatHandle) -> Result<SeatObservation> {
        let capture = self.seat.capture_pane(&seat.location_id).await?;
        let mut screen = TerminalScreen::new(
            capture.pane.width.max(1).min(u32::from(u16::MAX)) as u16,
            capture.pane.height.max(1).min(u32::from(u16::MAX)) as u16,
            TerminalMode::ScreenDump,
        );
        screen.feed_screen_dump(capture.raw.as_bytes());
        let grid = screen.grid();
        let questions = infer_questions(&grid, seat);
        let revision = self.observation_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut unknown = Vec::new();
        if screen.unmodelled_sequences > 0 {
            unknown.push(format!(
                "画面中有 {} 段未被本解析器建模的控制序列，相关行的属性可能不完整",
                screen.unmodelled_sequences
            ));
        }
        if grid.alternate_screen {
            unknown.push("当前处于终端 alternate screen；离开该屏后内容不可恢复".into());
        }
        let mut observed_seat = seat.clone();
        observed_seat.cwd = Some(capture.pane.cwd.clone());
        observed_seat.current_command = Some(capture.pane.current_command.clone());
        observed_seat.process_identity = Some(ProcessIdentity {
            pid: Some(capture.pane.pane_pid),
            start_token: None,
            executable: Some(capture.pane.current_command.clone()),
        });
        Ok(SeatObservation {
            seat: observed_seat,
            observation_revision: revision,
            observed_at: chrono::Utc::now(),
            screen_text: grid.trimmed_text(),
            screen: Some(grid),
            questions,
            unknown,
            raw_evidence: capture.raw.into_bytes(),
        })
    }

    async fn plan_effect(
        &self,
        _seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        answer: &SeatAnswer,
    ) -> Result<Option<SeatEffectPlan>> {
        let Some(input) = map_answer_to_terminal_input(question, answer)? else {
            // The mapping is not justified by what was observed: nothing may
            // be typed, and the caller records the refusal.
            return Ok(None);
        };
        Ok(Some(SeatEffectPlan::new(
            AnswerTransport::TerminalInput,
            input.clone(),
            format!("向 pane 键入 {} 字节", input.len()),
        )))
    }

    async fn preflight(
        &self,
        seat: &SeatHandle,
        question: &SeatQuestionCandidate,
        _plan: &SeatEffectPlan,
    ) -> Result<SeatPreflight> {
        // Re-read the live pane. tmux has no atomic "check the question and
        // type", so the only honest guard is a fresh capture immediately
        // before the write.
        let capture = self.seat.capture_pane(&seat.location_id).await?;
        if capture.pane.dead {
            return Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::StateChanged,
                reason: "pane 已 dead：进程退出，输入不会到达原 agent".into(),
            });
        }
        if let Some(expected) = seat.current_command.as_deref() {
            if !expected.is_empty() && capture.pane.current_command != expected {
                return Ok(SeatPreflight::Refused {
                    kind: PreflightRefusalKind::StateChanged,
                    reason: format!(
                        "pane 前台程序已变化：{} → {}",
                        expected, capture.pane.current_command
                    ),
                });
            }
        }
        let mut screen = TerminalScreen::new(
            capture.pane.width.max(1).min(u32::from(u16::MAX)) as u16,
            capture.pane.height.max(1).min(u32::from(u16::MAX)) as u16,
            TerminalMode::ScreenDump,
        );
        screen.feed_screen_dump(capture.raw.as_bytes());
        let live = infer_questions(&screen.grid(), seat);
        if !live
            .iter()
            .any(|candidate| candidate.question_id == question.question_id)
        {
            return Ok(SeatPreflight::Refused {
                kind: PreflightRefusalKind::QuestionGone,
                reason: "现场画面上已没有这个问题：可能已被人类或远端处理，或问题已改写".into(),
            });
        }
        Ok(SeatPreflight::Ready {
            detail: format!(
                "pane {} 仍是 {}，问题仍在画面上",
                capture.pane.pane_id, capture.pane.current_command
            ),
        })
    }

    async fn execute_effect(
        &self,
        seat: &SeatHandle,
        plan: &SeatEffectPlan,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        let expected = seat
            .current_command
            .clone()
            .ok_or_else(|| anyhow::anyhow!("seat 没有记录前台程序，拒绝写入"))?;
        // The pane's process identity is the target-side incarnation fence:
        // the guarded command re-checks it on the far side, so a pane that
        // was reused by another process cannot receive this answer.
        let expected_pid = seat.process_identity.as_ref().and_then(|identity| identity.pid);
        let result = self
            .seat
            .send_input_verified(&seat.location_id, &plan.payload, &expected, expected_pid)
            .await;
        let (confirmation, detail) = match result {
            Ok(_) => (
                AnswerConfirmation::Unconfirmed,
                Some(
                    format!(
                        "按键已写入 pane（writer epoch {}）；tmux 不提供“核对问题并输入”的原子操作，也没有观察到远端确认",
                        fence.lease_epoch
                    ),
                ),
            ),
            Err(error) => {
                let refusal = error.downcast_ref::<SeatRefusal>().copied();
                match refusal {
                    Some(refusal) => (
                        AnswerConfirmation::NotSent {
                            reason: format!("受管通道拒绝了这次写入：{refusal}"),
                        },
                        None,
                    ),
                    None => (
                        AnswerConfirmation::NotSent {
                            reason: format!("写入失败：{error}"),
                        },
                        None,
                    ),
                }
            }
        };
        Ok(SeatResponse {
            adapter: SeatAdapterId::Tmux,
            transport: AnswerTransport::TerminalInput,
            confirmation,
            detail,
        })
    }

    async fn submit(
        &self,
        seat: &SeatHandle,
        text: &str,
        fence: &SeatWriteFence,
    ) -> Result<SeatResponse> {
        let expected = seat
            .current_command
            .clone()
            .ok_or_else(|| anyhow::anyhow!("seat 没有记录前台程序，拒绝写入"))?;
        let expected_pid = seat.process_identity.as_ref().and_then(|identity| identity.pid);
        let result = self
            .seat
            .send_input_verified(&seat.location_id, text, &expected, expected_pid)
            .await;
        let confirmation = match result {
            Ok(_) => AnswerConfirmation::Unconfirmed,
            Err(error) => AnswerConfirmation::NotSent {
                reason: format!("写入失败：{error}"),
            },
        };
        Ok(SeatResponse {
            adapter: SeatAdapterId::Tmux,
            transport: AnswerTransport::TerminalInput,
            confirmation,
            detail: Some(format!(
                "文本已写入 pane（writer epoch {}）；未观察到远端确认",
                fence.lease_epoch
            )),
        })
    }

    async fn interrupt(&self, seat: &SeatHandle, fence: &SeatWriteFence) -> Result<SeatResponse> {
        let expected = seat
            .current_command
            .clone()
            .ok_or_else(|| anyhow::anyhow!("seat 没有记录前台程序，拒绝写入"))?;
        let expected_pid = seat.process_identity.as_ref().and_then(|identity| identity.pid);
        let result = self
            .seat
            .send_keys_verified(&seat.location_id, "C-c", &expected, expected_pid)
            .await;
        let confirmation = match result {
            Ok(_) => AnswerConfirmation::Unconfirmed,
            Err(error) => AnswerConfirmation::NotSent {
                reason: format!("中断写入失败：{error}"),
            },
        };
        Ok(SeatResponse {
            adapter: SeatAdapterId::Tmux,
            transport: AnswerTransport::TerminalInput,
            confirmation,
            detail: Some(format!(
                "已发送中断按键（writer epoch {}）；未观察到远端确认",
                fence.lease_epoch
            )),
        })
    }
}

/// Map a decision onto the narrowest terminal input that is justified by
/// what was observed. `Ok(None)` means the mapping is not justified and the
/// adapter must not type anything.
pub fn map_answer_to_terminal_input(
    question: &SeatQuestionCandidate,
    answer: &SeatAnswer,
) -> Result<Option<String>> {
    if question.source_kind != QuestionSourceKind::TerminalInference {
        // A structured question must be answered through its own protocol.
        return Ok(None);
    }
    if let Some(allow) = answer.allow {
        if !is_yes_no_prompt(&question.text) {
            return Ok(None);
        }
        return Ok(Some(if allow { "y".into() } else { "n".into() }));
    }
    if let Some(text) = &answer.text {
        if !text.is_empty() {
            return Ok(Some(text.clone()));
        }
    }
    let Some(option_id) = &answer.option_id else {
        return Ok(None);
    };
    let Some(option) = question
        .options
        .iter()
        .find(|option| &option.option_id == option_id)
    else {
        return Ok(None);
    };
    if let Some(native) = &option.native_value {
        return Ok(Some(native.clone()));
    }
    // The remote agent's own selection is marked on screen: pressing Enter
    // accepts what the remote agent already selected.
    if question.selected_option.as_deref() == Some(option_id.as_str()) {
        return Ok(Some(String::new()));
    }
    Ok(None)
}

/// True when the observed prompt really was a yes/no prompt.
pub fn is_yes_no_prompt(text: &str) -> bool {
    const MARKERS: &[&str] = &["(y/N)", "(Y/n)", "(y/n)", "[Y/n]", "[y/N]", "[y/n]", "(y/N/n)"];
    MARKERS.iter().any(|marker| text.contains(marker))
}

/// True when the line reads like something the remote agent is asking.
fn looks_like_question(text: &str) -> bool {
    if text.ends_with('?') || is_yes_no_prompt(text) {
        return true;
    }
    const MARKERS: &[&str] = &[
        "Do you want",
        "Would you like",
        "Proceed",
        "Press Enter",
        "Allow ",
        "Permission",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

/// The byte length of an option marker at the start of `text`, when the line
/// really is an option line.
fn option_marker(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let digits = bytes.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits > 0 && digits <= 2 {
        if let Some(&marker) = bytes.get(digits) {
            if (marker == b'.' || marker == b')') && bytes.get(digits + 1) == Some(&b' ') {
                return Some(digits + 2);
            }
        }
    }
    for marker in ["❯", "➜", "»", "▸", "▶", "•", "◦", "*", "-", ">"] {
        if text.starts_with(marker) {
            let rest = &text[marker.len()..];
            if rest.starts_with(' ') {
                return Some(marker.len() + 1);
            }
        }
    }
    if text.starts_with('[') {
        if let Some(close) = text.find(']') {
            if close == 2 && text.as_bytes().get(3) == Some(&b' ') {
                return Some(4);
            }
        }
    }
    None
}

/// Infer the open questions from a reconstructed screen.
///
/// The inference is deliberately narrow: it reports a question only when the
/// screen shows one, it names the options it actually saw, and it states
/// what it could not determine. It never invents a selection.
pub fn infer_questions(grid: &ScreenGrid, seat: &SeatHandle) -> Vec<SeatQuestionCandidate> {
    let mut questions = Vec::new();
    let rows: Vec<usize> = (0..grid.lines.len())
        .filter(|row| !grid.lines[*row].text.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return questions;
    }
    let start = rows.len().saturating_sub(16);
    for (offset, &row) in rows[start..].iter().enumerate() {
        let text = grid.lines[row].text.trim().to_string();
        if !looks_like_question(&text) {
            continue;
        }
        let mut options: Vec<SeatOption> = Vec::new();
        let mut highlighted: Option<String> = None;
        let mut cursor_marked: Option<String> = None;
        for &next in rows[start + offset + 1..].iter() {
            let raw = &grid.lines[next].text;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let marker = option_marker(trimmed);
            let highlight = grid.highlighted_range(next);
            if marker.is_none() && highlight.is_none() {
                break;
            }
            let leading = raw.len() - raw.trim_start().len();
            let (label, label_start) = match marker {
                Some(marker) => {
                    let label = trimmed[marker..].trim().to_string();
                    let start_char = raw[..leading + marker].chars().count();
                    (label, start_char)
                }
                None => {
                    let start_char = raw[..leading].chars().count();
                    (trimmed.to_string(), start_char)
                }
            };
            if label.is_empty() {
                break;
            }
            let option_id = format!("opt-{}", options.len() + 1);
            let marker_text = marker.map(|marker| &trimmed[..marker]).unwrap_or("");
            let numeric = marker_text.trim_end().trim_end_matches(['.', ')']);
            // A numbered option can be selected by its number; that mapping
            // is justified by the screen itself.
            let native_value = if !numeric.is_empty() && numeric.chars().all(|c| c.is_ascii_digit())
            {
                Some(numeric.to_string())
            } else {
                None
            };
            if cursor_marked.is_none() && CURSOR_MARKERS.iter().any(|m| trimmed.starts_with(m)) {
                cursor_marked = Some(option_id.clone());
            }
            options.push(SeatOption {
                option_id: option_id.clone(),
                label: label.clone(),
                meaning: None,
                native_value,
            });
            if let Some((start_char, end_char)) = highlight {
                let label_end = label_start + label.chars().count();
                if label_start < end_char && start_char < label_end {
                    highlighted = Some(option_id.clone());
                }
            }
            if options.len() >= 8 {
                break;
            }
        }
        let question_kind = if is_yes_no_prompt(&text) {
            QuestionKind::ToolPermission
        } else {
            QuestionKind::Planning
        };
        let mut unknown = vec!["问题类型由终端形状推断，原程序没有声明其语义".to_string()];
        let (selected, confidence) = if let Some(option_id) = highlighted {
            unknown.push("选中项依据画面保留的高亮属性判断；高亮本身不代表答案已被接受".to_string());
            (
                Some(option_id),
                Confidence::medium("终端画面显示提问、选项，并保留了一处高亮属性"),
            )
        } else if let Some(option_id) = cursor_marked {
            unknown.push("选中项依据选项行前的光标标记推断，屏幕未保留高亮属性".to_string());
            (
                Some(option_id),
                Confidence::medium("终端画面显示提问、选项，并观察到一处光标标记"),
            )
        } else {
            unknown.push("选中项未知：画面没有保留可判定的高亮属性，也没有光标标记".to_string());
            (
                None,
                Confidence::medium("终端画面显示提问与选项，但没有可判定的选中状态"),
            )
        };
        let confidence = if options.is_empty() {
            unknown.push("画面上只看到提问，没有可识别的选项".to_string());
            Confidence::low("终端画面只有一行提问文字")
        } else {
            confidence
        };
        let question_id = inferred_question_id(seat, &text, &options);
        questions.push(SeatQuestionCandidate {
            question_id,
            source_kind: QuestionSourceKind::TerminalInference,
            question_kind,
            native_request_id: None,
            native_tool_use_id: None,
            native_payload: None,
            title: None,
            text,
            options,
            selected_option: selected,
            confidence,
            unknown,
            observed_screen_revision: grid.revision,
            expires_at_ms: None,
        });
    }
    questions
}

/// Ensure a tmux adapter's answer was produced for a terminal question.
pub fn ensure_terminal_question(question: &SeatQuestionCandidate) -> Result<()> {
    ensure!(
        question.source_kind == QuestionSourceKind::TerminalInference,
        "structured questions must be answered through their own protocol"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat::ConfidenceLevel;
    use crate::terminal::{CellStyle, TerminalColor};

    fn handle() -> SeatHandle {
        SeatHandle {
            location_id: "%3".into(),
            adapter: SeatAdapterId::Tmux,
            adapter_version: TMUX_ADAPTER_VERSION.into(),
            host: "local".into(),
            cwd: Some("/project".into()),
            remote_session_id: None,
            current_command: Some("claude".into()),
            process_identity: None,
        }
    }

    fn screen(lines: &[(&str, bool)]) -> ScreenGrid {
        let mut terminal = TerminalScreen::new(40, lines.len() as u16, TerminalMode::ScreenDump);
        let mut bytes = Vec::new();
        for (line, highlight) in lines {
            if *highlight {
                bytes.extend_from_slice(b"\x1b[7m");
            }
            bytes.extend_from_slice(line.as_bytes());
            if *highlight {
                bytes.extend_from_slice(b"\x1b[0m");
            }
            bytes.push(b'\n');
        }
        terminal.feed_screen_dump(&bytes);
        terminal.grid()
    }

    #[test]
    fn a_question_with_highlighted_option_is_reported() {
        let grid = screen(&[
            ("Do you want to continue?", false),
            ("  yes", true),
            ("  no", false),
        ]);
        let questions = infer_questions(&grid, &handle());
        assert_eq!(questions.len(), 1);
        let question = &questions[0];
        assert_eq!(question.source_kind, QuestionSourceKind::TerminalInference);
        assert_eq!(question.options.len(), 1);
        assert_eq!(question.selected_option.as_deref(), Some("opt-1"));
        assert_eq!(question.confidence.level, ConfidenceLevel::Medium);
    }

    #[test]
    fn selection_requires_a_visible_marker_not_option_position() {
        for (first, expected) in [("  ❯ yes", Some("opt-1")), ("  - yes", None)] {
            let grid = screen(&[("Do you want to continue?", false), (first, false), ("  - no", false)]);
            let question = &infer_questions(&grid, &handle())[0];
            assert_eq!(question.options.len(), 2);
            assert_eq!(question.selected_option.as_deref(), expected);
        }
    }

    #[test]
    fn numbered_options_map_to_their_number() {
        let grid = screen(&[("Pick one?", false), ("  1. alpha", false), ("  2) beta", false)]);
        let question = &infer_questions(&grid, &handle())[0];
        assert_eq!(question.options[0].native_value.as_deref(), Some("1"));
        assert_eq!(question.options[1].native_value.as_deref(), Some("2"));
        let answer = SeatAnswer {
            option_id: Some("opt-2".into()),
            text: None,
            allow: None,
            reason: "fixture".into(),
        };
        let mapped = map_answer_to_terminal_input(question, &answer).unwrap();
        assert_eq!(mapped.as_deref(), Some("2"));
    }

    #[test]
    fn a_yes_no_prompt_accepts_only_allow_or_deny() {
        let grid = screen(&[("Continue? (y/N)", false)]);
        let question = &infer_questions(&grid, &handle())[0];
        let allow = SeatAnswer {
            option_id: None,
            text: None,
            allow: Some(true),
            reason: "fixture".into(),
        };
        assert_eq!(
            map_answer_to_terminal_input(question, &allow)
                .unwrap()
                .as_deref(),
            Some("y")
        );
        let deny = SeatAnswer {
            option_id: None,
            text: None,
            allow: Some(false),
            reason: "fixture".into(),
        };
        assert_eq!(
            map_answer_to_terminal_input(question, &deny)
                .unwrap()
                .as_deref(),
            Some("n")
        );
    }

    #[test]
    fn an_unmapped_decision_sends_nothing() {
        let grid = screen(&[("Pick one?", false), ("  - alpha", false), ("  - beta", false)]);
        let question = &infer_questions(&grid, &handle())[0];
        assert_eq!(question.options[0].label, "alpha");
        assert_eq!(question.options[0].native_value, None);
        assert_eq!(question.selected_option, None);
        let answer = SeatAnswer {
            option_id: Some("opt-1".into()),
            text: None,
            allow: None,
            reason: "fixture".into(),
        };
        assert_eq!(map_answer_to_terminal_input(question, &answer).unwrap(), None);
    }

    #[test]
    fn reverse_video_is_the_only_selection_marker() {
        let style = CellStyle {
            reverse: true,
            ..CellStyle::default()
        };
        assert!(style.marks_selection());
        let bold = CellStyle {
            bold: true,
            ..CellStyle::default()
        };
        assert!(!bold.marks_selection());
        let coloured = CellStyle {
            foreground: TerminalColor::Indexed(3),
            ..CellStyle::default()
        };
        assert!(!coloured.marks_selection());
    }
}
