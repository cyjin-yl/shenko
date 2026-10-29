//! tmux seat 通过本机 sh 或严格校验身份的宿主 OpenSSH 工作；不是 WASM SSH。
pub use crate::executor::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

// 所有非命令参数均按远端 POSIX shell 的单个字面值引用。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// tmux 行格式：pane 身份 + 前台进程 + 尺寸 + 根进程 PID + dead 标志。
const PANE_FORMAT: &str = "#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_id}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_active}\t#{pane_width}\t#{pane_height}\t#{pane_pid}\t#{pane_dead}";

// seat 写守卫的远端退出码；与其它 tmux/ssh 退出码区分开。
const EXIT_PANE_MISSING: i32 = 66;
const EXIT_PANE_DEAD: i32 = 67;
const EXIT_PROGRAM_CHANGED: i32 = 68;

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
    /// tmux 报告的 pane 根进程 PID（通常是 pane 内启动的 shell）。
    #[serde(default)]
    pub pane_pid: u32,
    /// tmux 的 pane_dead 标志：pane 仍在但进程已退出（dead pane）。
    #[serde(default)]
    pub dead: bool,
}

fn parse_pane_line(line: &str) -> Option<TmuxPane> {
    let parts: Vec<&str> = line.split('\t').collect();
    // 旧 tmux 可能缺 pane_pid/pane_dead 两列；按 0/false 处理而非丢弃 pane。
    if parts.len() < 9 {
        return None;
    }
    Some(TmuxPane {
        session: parts[0].to_string(),
        window: parts[1].parse().unwrap_or(0),
        pane: parts[2].parse().unwrap_or(0),
        pane_id: parts[3].to_string(),
        current_command: parts[4].to_string(),
        cwd: parts[5].to_string(),
        active: parts[6] == "1",
        width: parts[7].parse().unwrap_or(80),
        height: parts[8].parse().unwrap_or(24),
        pane_pid: parts.get(9).and_then(|s| s.parse().ok()).unwrap_or(0),
        dead: parts.get(10).is_some_and(|s| *s == "1"),
    })
}

/// 已知 agent seat 类型；对端程序名按进程子树 argv 识别。SPEC §17.1
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatKind {
    ClaudeCode,
    Omp,
    Codex,
    /// 有前台进程但不是已知 adapter（首个非 shell/wrapper 的 argv）。
    Other(String),
}

/// 发现到的 agent seat：pane 身份 + 进程子树证据 + 识别出的 adapter。SPEC §17.1
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSeat {
    pub pane: TmuxPane,
    /// 从 pane_pid 向下收集的进程 argv（根进程在前）。`ps` 不可得时为空。
    pub processes: Vec<String>,
    pub kind: SeatKind,
}

/// seat 写入被守卫拒绝：pane 消失、pane_dead 或前台程序已变化（如退回 shell）。
/// 与传输层 ExecFailure 区分；调用方据此记录 seat 现状而非盲发。SPEC §17.1/§17.5
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatRefusal {
    PaneMissing,
    PaneDead,
    ProgramChanged,
}

impl std::fmt::Display for SeatRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::PaneMissing => "pane 已不存在，旧的 pane_id 不能作为目标",
            Self::PaneDead => "pane 已 dead：进程退出，输入不会到达原 agent",
            Self::ProgramChanged => "pane 前台程序已变化，输入可能落到 shell 或其它程序",
        };
        f.write_str(text)
    }
}
impl std::error::Error for SeatRefusal {}

/// `ps -axo pid=,ppid=,args=` 输出 → (pid → ppid) 表与 (pid → argv) 表。
fn parse_ps_table(stdout: &str) -> (HashMap<u32, u32>, HashMap<u32, String>) {
    let mut ppid_of = HashMap::new();
    let mut args_of = HashMap::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (
            fields.next().and_then(|s| s.parse::<u32>().ok()),
            fields.next().and_then(|s| s.parse::<u32>().ok()),
        ) else {
            continue;
        };
        ppid_of.insert(pid, ppid);
        args_of.insert(pid, fields.collect::<Vec<_>>().join(" "));
    }
    (ppid_of, args_of)
}

/// 收集 pane_pid 的进程子树 argv（根在前，按发现顺序去重）。
fn process_subtree(
    pane_pid: u32,
    ppid_of: &HashMap<u32, u32>,
    args_of: &HashMap<u32, String>,
) -> Vec<String> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in ppid_of {
        children.entry(ppid).or_default().push(pid);
    }
    let mut out = Vec::new();
    let mut stack = vec![pane_pid];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(args) = args_of.get(&pid) {
            out.push(args.clone());
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// shell / 终端复用器本身不算 seat 前台程序。
fn is_shell_or_wrapper(argv: &str) -> bool {
    const WRAPPERS: &[&str] = &[
        "sh", "bash", "zsh", "fish", "dash", "ksh", "login", "tmux", "screen", "sudo", "env",
        "xargs", "watch", "sshd",
    ];
    let comm = argv
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    let comm = comm.strip_prefix('-').unwrap_or(comm);
    WRAPPERS.contains(&comm)
}

/// 取进程子树中第一个非 shell/wrapper 的程序名并归类已知 adapter。
fn classify_seat(processes: &[String]) -> Option<SeatKind> {
    let argv = processes.iter().find(|a| !is_shell_or_wrapper(a))?;
    let comm = argv
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    Some(match comm.as_str() {
        "claude" | "claude.exe" => SeatKind::ClaudeCode,
        "omp" | "omp.exe" => SeatKind::Omp,
        "codex" | "codex.exe" => SeatKind::Codex,
        _ => SeatKind::Other(comm),
    })
}

/// 单条远端命令：校验 pane 仍存在、未 dead、前台程序与期望一致后才 send-keys。
/// 退出码区分拒绝原因；keys 为已 shell-quote 的 send-keys 参数串。
fn guarded_send_command(pane_id: &str, expected_command: &str, send_args: &str) -> String {
    let pane = shell_quote(pane_id);
    let info = "tmux list-panes -a -F '#{pane_id}\t#{pane_dead}\t#{pane_current_command}'";
    let match_line = format!("$({info} | awk -F '\\t' -v id={pane} '$1 == id {{print; exit}}')");
    format!(
        "line={match_line}; \
         [ -n \"$line\" ] || {{ echo seat:missing >&2; exit {EXIT_PANE_MISSING}; }}; \
         [ \"$(printf '%s' \"$line\" | cut -f2)\" = \"0\" ] || {{ echo seat:dead >&2; exit {EXIT_PANE_DEAD}; }}; \
         [ \"$(printf '%s' \"$line\" | cut -f3)\" = {expected} ] || {{ echo seat:changed >&2; exit {EXIT_PROGRAM_CHANGED}; }}; \
         tmux send-keys -t {pane} {send_args}",
        expected = shell_quote(expected_command),
    )
}

/// ExecResult → Ok(())，或把守卫退出码/失败映射为 SeatRefusal / ExecFailure。
fn refusal_or_success(result: &ExecResult) -> Result<()> {
    match result.exit_code {
        EXIT_PANE_MISSING => Err(SeatRefusal::PaneMissing.into()),
        EXIT_PANE_DEAD => Err(SeatRefusal::PaneDead.into()),
        EXIT_PROGRAM_CHANGED => Err(SeatRefusal::ProgramChanged.into()),
        _ => result.require_success(),
    }
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
    /// Stable digest of the plain screen; changes when visible content changes.
    /// Callers compare before answering so a stale question revision is detected.
    #[serde(default)]
    pub screen_hash: u64,
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
        let out = self
            .executor
            .run_command(&format!(
                "tmux list-panes -a -F '{PANE_FORMAT}' 2>/dev/null || echo ''"
            ))
            .await?;

        out.require_success()?;
        Ok(out.stdout.lines().filter_map(parse_pane_line).collect())
    }

    /// Discover agent seats by the pane's real process subtree. SPEC §17.1
    ///
    /// Walks `ps` from each pane's pane_pid: bare shells and wrappers are not
    /// seats; the first non-shell program classifies the seat (claude/omp/codex
    /// → typed adapter, anything else → Other). A pane whose process has exited
    /// (`pane_dead`) is never returned as a seat. If `ps` is unavailable on the
    /// remote, classification falls back to pane_current_command with an empty
    /// `processes` evidence list — callers should treat that seat as unverified.
    pub async fn discover_agent_seats(&self) -> Result<Vec<AgentSeat>> {
        let panes = self.list_panes().await?;
        let ps_out = self
            .executor
            .run_command("ps -axo pid=,ppid=,args= 2>/dev/null || echo ''")
            .await?;
        ps_out.require_success()?;
        let (ppid_of, args_of) = parse_ps_table(&ps_out.stdout);

        Ok(panes
            .into_iter()
            .filter(|p| !p.dead)
            .filter_map(|pane| {
                let processes = process_subtree(pane.pane_pid, &ppid_of, &args_of);
                let kind = classify_seat(&processes)
                    .or_else(|| classify_seat(&[pane.current_command.clone()]))?;
                Some(AgentSeat { pane, processes, kind })
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
            "tmux list-panes -t {pane_target} -F '{PANE_FORMAT}' 2>/dev/null || echo ''"
        );

        let (raw_out, info_out) = tokio::try_join!(
            self.executor.run_command(&raw_cmd),
            self.executor.run_command(&pane_info_cmd),
        )?;

        raw_out.require_success()?;
        info_out.require_success()?;
        let pane = info_out
            .stdout
            .lines()
            .find_map(parse_pane_line)
            .unwrap_or_else(|| TmuxPane {
                session: String::new(),
                window: 0,
                pane: 0,
                pane_id: pane_id.to_string(),
                current_command: String::new(),
                cwd: String::new(),
                active: false,
                width: 80,
                height: 24,
                pane_pid: 0,
                dead: false,
            });

        let revision = self
            .observation_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let raw = raw_out.stdout;
        let plain = parse_ansi_to_plain(&raw);
        let pending_questions = extract_question_lines(&plain);
        let screen_hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            plain.hash(&mut h);
            h.finish()
        };

        Ok(SeatObservation {
            pane,
            raw,
            plain,
            pending_questions,
            revision,
            screen_hash,
            captured_at: chrono::Utc::now(),
        })
    }

    /// 发送按键前，调用方须先持久提交 intent。此适配器本身没有账本。SPEC §35.5/§36.5
    /// WARNING: tmux send-keys has no atomic "check then send" — race with human input.
    /// Caller must verify pane state is as expected before sending.
    /// Prefer `send_keys_verified`/`send_input_verified`, which re-check pane
    /// identity and foreground program inside the same remote command.
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

    /// Verified send-keys: refuses unless the pane still exists, is not
    /// pane_dead, and its foreground program still equals `expected_command`.
    /// SPEC §17.1/§17.5 — 退回 shell 或旧 pane_id 不得接收为 seat 准备的输入。
    ///
    /// `expected_command` should come from a fresh SeatObservation /
    /// discover_agent_seats `pane.current_command`. Refusal is a typed
    /// `SeatRefusal` error (downcast_ref), distinct from transport/ExecFailure.
    /// The check and send run in one remote shell program, but tmux offers no
    /// atomic check-then-send — a human or the agent can still interpose in the
    /// gap; this guard removes blind sends, it does not prove delivery.
    pub async fn send_keys_verified(
        &self,
        pane_id: &str,
        keys: &str,
        expected_command: &str,
    ) -> Result<ExecResult> {
        let cmd = guarded_send_command(pane_id, expected_command, &shell_quote(keys));
        let result = self.executor.run_command(&cmd).await?;
        refusal_or_success(&result)?;
        Ok(result)
    }

    /// Verified send-input: same guard as send_keys_verified, then text + Enter.
    pub async fn send_input_verified(
        &self,
        pane_id: &str,
        text: &str,
        expected_command: &str,
    ) -> Result<ExecResult> {
        let send_args = format!("{} Enter", shell_quote(text));
        let cmd = guarded_send_command(pane_id, expected_command, &send_args);
        let result = self.executor.run_command(&cmd).await?;
        refusal_or_success(&result)?;
        Ok(result)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn result(exit_code: i32) -> ExecResult {
        ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code,
            timed_out: false,
            execution_id: None,
            outcome: Some(ExecOutcome::Exited),
            diagnostic: None,
        }
    }

    #[test]
    fn discovery_classifies_from_process_subtree() {
        // pane 200 runs claude under its login shell; pane 300 is a bare shell;
        // pane 400 runs an unknown foreground program; unrelated pids ignored.
        let ps = "    1      0 /sbin/init\n\
                  200      1 -bash\n\
                  201    200 claude\n\
                  300      1 /bin/zsh\n\
                  400      1 -sh\n\
                  401    400 node server.js\n\
                  999      1 unrelated daemon --flag\n";
        let (ppid_of, args_of) = parse_ps_table(ps);

        let procs = process_subtree(200, &ppid_of, &args_of);
        assert_eq!(procs, vec!["-bash".to_string(), "claude".to_string()]);
        assert_eq!(classify_seat(&procs), Some(SeatKind::ClaudeCode));

        // Bare shell subtree: not an agent seat.
        assert_eq!(classify_seat(&process_subtree(300, &ppid_of, &args_of)), None);
        assert_eq!(
            classify_seat(&process_subtree(400, &ppid_of, &args_of)),
            Some(SeatKind::Other("node".into()))
        );

        // Empty evidence (ps unavailable): current_command fallback classifies.
        assert_eq!(classify_seat(&[]), None);
        assert_eq!(classify_seat(&["omp".to_string()]), Some(SeatKind::Omp));
    }

    #[test]
    fn guarded_send_refuses_stale_panes_and_maps_results() {
        let line = "s1\t0\t0\t%7\tnode\t/repo\t1\t80\t24\t4321\t0\n";
        let pane = parse_pane_line(line).unwrap();
        assert_eq!((pane.pane_id.as_str(), pane.pane_pid, pane.dead), ("%7", 4321, false));

        let cmd = guarded_send_command("%7", "node", "'continue' Enter");
        assert!(cmd.contains("pane_dead") && cmd.contains("pane_current_command"));
        assert!(cmd.contains("tmux send-keys -t '%7' 'continue' Enter"));
        assert!(cmd.contains("exit 66") && cmd.contains("exit 67") && cmd.contains("exit 68"));

        for (code, refusal) in [
            (EXIT_PANE_MISSING, SeatRefusal::PaneMissing),
            (EXIT_PANE_DEAD, SeatRefusal::PaneDead),
            (EXIT_PROGRAM_CHANGED, SeatRefusal::ProgramChanged),
        ] {
            let err = refusal_or_success(&result(code)).unwrap_err();
            assert_eq!(err.downcast_ref::<SeatRefusal>(), Some(&refusal));
        }
        // Success passes through; ordinary tmux failure stays an ExecFailure,
        // not a seat refusal and not a fake success.
        refusal_or_success(&result(0)).unwrap();
        let err = refusal_or_success(&result(1)).unwrap_err();
        assert!(err.downcast_ref::<ExecFailure>().is_some());
    }
}
