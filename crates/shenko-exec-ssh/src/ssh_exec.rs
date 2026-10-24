//! tmux seat 通过本机 sh 或严格校验身份的宿主 OpenSSH 工作；不是 WASM SSH。
pub use crate::executor::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};

// 所有非命令参数均按远端 POSIX shell 的单个字面值引用。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Tmux pane identity — needed for seat discovery. SPEC §35.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxPane {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    pub pane_id: String, // %N
    pub current_command: String,
    pub cwd: String,
    pub active: bool,
    pub width: u32,
    pub height: u32,
}

/// Tmux observation — screen capture preserving attributes. SPEC §35.5
/// Uses `tmux capture-pane -e` for attribute preservation (color/highlight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxObservation {
    pub pane: TmuxPane,
    /// Raw screen content with escape sequences (from capture-pane -e)
    pub raw_screen: String,
    /// Plain text (stripped of escape sequences)
    pub plain_text: String,
    pub observation_seq: u64,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

/// Structured seat observation returned by TmuxSeat::capture_pane. SPEC §17.5, §35.5
/// Carries raw ANSI output, plain-stripped text, detected question/prompt lines,
/// and a monotonic revision counter for change detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatObservation {
    pub pane: TmuxPane,
    /// Raw screen content with ANSI escape sequences (from capture-pane -e)
    pub raw: String,
    /// Plain text after stripping all ANSI/VT100 escape sequences
    pub plain: String,
    /// Lines that appear to be prompting for user input (questions, confirmations)
    pub pending_questions: Vec<String>,
    /// Monotonically increasing counter; changes when capture_pane is called
    pub revision: u64,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

/// Strip ANSI/VT100 escape sequences from raw terminal output. SPEC §17.5
///
/// Handles CSI sequences (`ESC [ ... letter`), OSC sequences (`ESC ] ... BEL/ST`),
/// two-char ESC sequences, and charset designations. Preserves newlines and
/// all non-escape content verbatim.
pub fn parse_ansi_to_plain(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some(&'[') => {
                // CSI: ESC [ <params> <final>   final = ASCII letter or '@'
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '@' {
                        break;
                    }
                }
            }
            Some(&']') => {
                // OSC: ESC ] <payload> BEL  or  ESC ] <payload> ST (ESC \)
                chars.next();
                loop {
                    match chars.next() {
                        None | Some('\x07') => break,
                        Some('\x1b') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(&'(') | Some(&')') => {
                // Charset designation: ESC ( X  or  ESC ) X
                chars.next();
                chars.next();
            }
            Some(_) => {
                // Two-char escape (e.g. ESC M, ESC 7, ESC 8) — skip second char
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Return lines from plain text that look like interactive prompts. SPEC §17.5
///
/// A line qualifies if it ends with '?' (after trimming) or contains any of
/// the standard confirmation/prompt patterns used by CLI tools.
pub fn extract_question_lines(plain: &str) -> Vec<String> {
    const PATTERNS: &[&str] = &[
        "Do you want",
        "Would you like",
        "Press Enter",
        "Y/n",
        "(y/N)",
        "(Y/n)",
        "(y/n)",
        "[Y/n]",
        "[y/N]",
        "[y/n]",
    ];
    plain
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() {
                return false;
            }
            t.ends_with('?') || PATTERNS.iter().any(|p| line.contains(p))
        })
        .map(|l| l.to_string())
        .collect()
}

/// Tmux seat adapter — discovers and controls tmux panes. SPEC §35.5
/// This is the first required seat path: "已存在的 tmux 前台 TUI"
pub struct TmuxSeat {
    executor: SshExecutor,
    observation_seq: std::sync::atomic::AtomicU64,
}

impl TmuxSeat {
    pub fn local() -> Self {
        Self {
            executor: SshExecutor::new(SshTarget::default()),
            observation_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn remote(target: SshTarget) -> Self {
        Self {
            executor: SshExecutor::over_ssh(target),
            observation_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// List all tmux panes. Returns empty Vec if tmux not running.
    pub async fn list_panes(&self) -> Result<Vec<TmuxPane>> {
        let out = self.executor.run_command(
            "tmux list-panes -a -F '#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_id}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_active}\t#{pane_width}\t#{pane_height}' 2>/dev/null || echo ''"
        ).await?;

        out.require_success()?;
        let mut panes = vec![];
        for line in out.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 9 {
                continue;
            }
            panes.push(TmuxPane {
                session: parts[0].to_string(),
                window: parts[1].parse().unwrap_or(0),
                pane: parts[2].parse().unwrap_or(0),
                pane_id: parts[3].to_string(),
                current_command: parts[4].to_string(),
                cwd: parts[5].to_string(),
                active: parts[6] == "1",
                width: parts[7].parse().unwrap_or(80),
                height: parts[8].parse().unwrap_or(24),
            });
        }
        Ok(panes)
    }

    /// Discover panes running Claude Code, OMP, or other agent seats.
    pub async fn discover_agent_seats(&self) -> Result<Vec<TmuxPane>> {
        let panes = self.list_panes().await?;
        let agent_cmds = ["claude", "omp", "python", "node", "cargo", "bash", "zsh"];
        Ok(panes
            .into_iter()
            .filter(|p| {
                let cmd = p.current_command.to_lowercase();
                agent_cmds.iter().any(|a| cmd.contains(a))
            })
            .collect())
    }

    /// Capture a pane preserving ANSI/attributes (capture-pane -e). SPEC §17.5, §35.5
    /// Returns a SeatObservation with raw ANSI output, plain-stripped text,
    /// and any detected question/prompt lines.
    pub async fn capture_pane(&self, pane_id: &str) -> Result<SeatObservation> {
        let pane_target = shell_quote(pane_id);
        // -e: preserve escape sequences (color, attribute); second call gives tmux-stripped plain
        let raw_cmd = format!("tmux capture-pane -p -e -t {pane_target} 2>/dev/null || echo ''");
        let pane_info_cmd = format!(
            "tmux list-panes -t {pane_target} -F '#{{session_name}}\t#{{window_index}}\t#{{pane_index}}\t#{{pane_id}}\t#{{pane_current_command}}\t#{{pane_current_path}}\t#{{pane_active}}\t#{{pane_width}}\t#{{pane_height}}' 2>/dev/null || echo ''"
        );

        let (raw_out, info_out) = tokio::try_join!(
            self.executor.run_command(&raw_cmd),
            self.executor.run_command(&pane_info_cmd),
        )?;

        raw_out.require_success()?;
        info_out.require_success()?;
        let pane = {
            let line = info_out.stdout.lines().next().unwrap_or("");
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 9 {
                TmuxPane {
                    session: parts[0].to_string(),
                    window: parts[1].parse().unwrap_or(0),
                    pane: parts[2].parse().unwrap_or(0),
                    pane_id: parts[3].to_string(),
                    current_command: parts[4].to_string(),
                    cwd: parts[5].to_string(),
                    active: parts[6] == "1",
                    width: parts[7].parse().unwrap_or(80),
                    height: parts[8].parse().unwrap_or(24),
                }
            } else {
                TmuxPane {
                    session: String::new(),
                    window: 0,
                    pane: 0,
                    pane_id: pane_id.to_string(),
                    current_command: String::new(),
                    cwd: String::new(),
                    active: false,
                    width: 80,
                    height: 24,
                }
            }
        };

        let revision = self
            .observation_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let raw = raw_out.stdout;
        let plain = parse_ansi_to_plain(&raw);
        let pending_questions = extract_question_lines(&plain);

        Ok(SeatObservation {
            pane,
            raw,
            plain,
            pending_questions,
            revision,
            captured_at: chrono::Utc::now(),
        })
    }

    /// 发送按键前，调用方须先持久提交 intent。此适配器本身没有账本。SPEC §35.5/§36.5
    /// WARNING: tmux send-keys has no atomic "check then send" — race with human input.
    /// Caller must verify pane state is as expected before sending.
    pub async fn send_keys(&self, pane_id: &str, keys: &str) -> Result<ExecResult> {
        let pane_target = shell_quote(pane_id);
        let escaped = keys.replace('\'', "'\\''");
        let cmd = format!("tmux send-keys -t {pane_target} '{escaped}'");
        self.executor.run_command(&cmd).await
    }

    /// Send keys with Enter
    pub async fn send_input(&self, pane_id: &str, text: &str) -> Result<ExecResult> {
        let pane_target = shell_quote(pane_id);
        let escaped = text.replace('\'', "'\\''");
        let cmd = format!("tmux send-keys -t {pane_target} '{escaped}' Enter");
        self.executor.run_command(&cmd).await
    }

    /// Create a new tmux window for an agent task
    pub async fn new_window(
        &self,
        session: &str,
        name: &str,
        start_cmd: Option<&str>,
    ) -> Result<String> {
        let session = shell_quote(session);
        let name = shell_quote(name);
        let cmd_part = start_cmd
            .map(|c| format!(" '{}'", c.replace('\'', "'\\''")))
            .unwrap_or_default();
        let cmd = format!("tmux new-window -t {session} -n {name} -P -F '#{{pane_id}}'{cmd_part}");
        let out = self.executor.run_command(&cmd).await?;
        out.require_success()?;
        Ok(out.stdout.trim().to_string())
    }
}
