//! tmux seat 通过本机 sh 或严格校验身份的宿主 OpenSSH 工作；不是 WASM SSH。
pub use crate::executor::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

// 所有非命令参数均按远端 POSIX shell 的单个字面值引用。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// tmux can sanitize literal control characters in format arguments. Keep the
// wire printable: q: escapes shell punctuation (including | and backslash)
// in expanded text fields, and only unescaped | separates fields. Decode it
// as data, never evaluate the output as a shell program.
const PANE_FORMAT: &str = "#{q:session_name}|#{window_index}|#{pane_index}|#{pane_id}|#{q:pane_current_command}|#{q:pane_current_path}|#{pane_active}|#{pane_width}|#{pane_height}|#{pane_pid}|#{pane_dead}";
const TMUX_QUOTED_CHARACTERS: &str = "|&;<>(){}$`\\\"'*?[# =%\n\t";

fn parse_tmux_fields(line: &str) -> Option<Vec<std::borrow::Cow<'_, str>>> {
    use std::borrow::Cow;
    let mut fields = Vec::new();
    let mut start = 0;
    let mut decoded: Option<String> = None;
    let mut chars = line.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\\' => {
                let (_, escaped) = chars.next()?;
                if !TMUX_QUOTED_CHARACTERS.contains(escaped) || escaped.is_control() {
                    return None;
                }
                decoded.get_or_insert_with(|| line[start..index].to_owned()).push(escaped);
            }
            '|' => {
                fields.push(match decoded.take() {
                    Some(value) => Cow::Owned(value),
                    None => Cow::Borrowed(&line[start..index]),
                });
                start = index + 1;
            }
            ch if ch.is_control() => return None,
            ch => {
                if let Some(value) = &mut decoded {
                    value.push(ch);
                }
            }
        }
    }
    fields.push(match decoded {
        Some(value) => Cow::Owned(value),
        None => Cow::Borrowed(&line[start..]),
    });
    Some(fields)
}

// seat 写守卫的远端退出码；与其它 tmux/ssh 退出码区分开。
const EXIT_PANE_MISSING: i32 = 66;
const EXIT_PANE_DEAD: i32 = 67;
const EXIT_PROGRAM_CHANGED: i32 = 68;
const EXIT_PANE_REPLACED: i32 = 69;

/// Tmux pane identity — needed for seat discovery.
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
    let mut parts = parse_tmux_fields(line)?;
    parse_pane_fields(&mut parts)
}

fn parse_pane_fields(parts: &mut [std::borrow::Cow<'_, str>]) -> Option<TmuxPane> {
    // 旧 tmux 可能缺 pane_pid/pane_dead 两列；按 0/false 处理而非丢弃 pane。
    if parts.len() < 9 {
        return None;
    }
    Some(TmuxPane {
        session: std::mem::take(&mut parts[0]).into_owned(),
        window: parts[1].parse().unwrap_or(0),
        pane: parts[2].parse().unwrap_or(0),
        pane_id: std::mem::take(&mut parts[3]).into_owned(),
        current_command: std::mem::take(&mut parts[4]).into_owned(),
        cwd: std::mem::take(&mut parts[5]).into_owned(),
        active: parts[6] == "1",
        width: parts[7].parse().unwrap_or(80),
        height: parts[8].parse().unwrap_or(24),
        pane_pid: parts.get(9).and_then(|s| s.parse().ok()).unwrap_or(0),
        dead: parts.get(10).is_some_and(|s| s.as_ref() == "1"),
    })
}

/// 已知 agent seat 类型；对端程序名按进程子树 argv 识别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatKind {
    ClaudeCode,
    Omp,
    Codex,
    /// 有前台进程但不是已知 adapter（首个非 shell/wrapper 的 argv）。
    Other(String),
}

/// 发现到的 agent seat：pane 身份 + 进程子树证据 + 识别出的 adapter。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSeat {
    pub pane: TmuxPane,
    /// 从 pane_pid 向下收集的子树 argv，含后台/停止任务；顺序确定
    /// （根在前，广度优先、每层按 pid 升序）。`ps` 不可得或 pane_pid
    /// 为 0/不在表中时为空——空证据走 current_command fallback，绝不
    /// 把无关主机进程呈现为该 pane 的现场。
    pub processes: Vec<String>,
    pub kind: SeatKind,
}

/// seat 写入被守卫拒绝：pane 消失、pane_dead 或前台程序已变化（如退回 shell）。
/// 与传输层 ExecFailure 区分；调用方据此记录 seat 现状而非盲发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatRefusal {
    PaneMissing,
    PaneDead,
    ProgramChanged,
    /// The pane now hosts a different root process: the pane was reused
    /// after the observation, so the answer would land on another program.
    PaneReplaced,
}

impl std::fmt::Display for SeatRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::PaneMissing => "pane 已不存在，旧的 pane_id 不能作为目标",
            Self::PaneDead => "pane 已 dead：进程退出，输入不会到达原 agent",
            Self::ProgramChanged => "pane 前台程序已变化，输入可能落到 shell 或其它程序",
            Self::PaneReplaced => "pane 的根进程已更换：同一个 pane_id 已被另一个程序接管",
        };
        f.write_str(text)
    }
}
impl std::error::Error for SeatRefusal {}

/// `ps -axo pid=,ppid=,pgid=,tpgid=,args=` 的一行记录。
struct ProcRow {
    ppid: u32,
    pgid: u32,
    /// 该进程控制终端的前台进程组；-1/0 表示无前台（不可能是 seat 输入目标）。
    tpgid: i64,
    argv: String,
}

/// `ps -axo pid=,ppid=,pgid=,tpgid=,args=` 输出 → pid → ProcRow。
/// pid/ppid/pgid 缺失或不可解析的行整条丢弃；tpgid 缺失或不可解析记 -1，
/// 该行仍保留子树证据但永远不会被判为前台。
fn parse_ps_table(stdout: &str) -> HashMap<u32, ProcRow> {
    let mut table = HashMap::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid)) = (
            fields.next().and_then(|s| s.parse::<u32>().ok()),
            fields.next().and_then(|s| s.parse::<u32>().ok()),
            fields.next().and_then(|s| s.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let tpgid = fields
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(-1);
        let argv = fields.collect::<Vec<_>>().join(" ");
        table.insert(pid, ProcRow { ppid, pgid, tpgid, argv });
    }
    table
}

/// 收集 root 的进程子树 pid：广度优先、每层按 pid 升序，输出顺序确定。
/// root 为 0（旧 tmux 缺 pane_pid 列）或不在 ps 表中时返回空证据——
/// 绝不从 init/无关主机进程向下误走整棵进程树。
fn process_subtree(root: u32, table: &HashMap<u32, ProcRow>) -> Vec<u32> {
    if root == 0 || !table.contains_key(&root) {
        return Vec::new();
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, row) in table {
        children.entry(row.ppid).or_default().push(pid);
    }
    let mut out = Vec::new();
    let mut queue = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    while let Some(pid) = queue.pop_front() {
        out.push(pid);
        if let Some(kids) = children.get_mut(&pid) {
            kids.sort_unstable();
            for &kid in kids.iter() {
                if seen.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
    }
    out
}

/// 只取 pane 根进程控制终端的前台组；子进程自己的另一个 PTY 不是输入目标。
/// 后台与停止任务共享子树但不是前台组，收不到 pane 键盘输入——例如前台
/// bash 下被 `Ctrl-Z` 停止的 agent 不得让该 pane 归类为 agent seat。
fn foreground_group_argv(table: &HashMap<u32, ProcRow>, subtree: &[u32]) -> Vec<String> {
    let Some(foreground) = subtree.first().and_then(|pid| table.get(pid))
        .map(|row| row.tpgid).filter(|group| *group > 0)
    else {
        return Vec::new();
    };
    subtree
        .iter()
        .filter_map(|pid| table.get(pid))
        .filter(|row| row.tpgid == foreground && i64::from(row.pgid) == foreground)
        .map(|row| row.argv.clone())
        .collect()
}

fn classify_pane_foreground(
    table: &HashMap<u32, ProcRow>,
    subtree: &[u32],
    current_command: &String,
) -> Option<SeatKind> {
    if subtree.is_empty() {
        classify_seat(std::slice::from_ref(current_command))
    } else {
        // A positively observed foreground shell overrides older tmux metadata.
        classify_seat(&foreground_group_argv(table, subtree))
    }
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

/// 取（前台进程组）argv 列表中第一个非 shell/wrapper 的程序名并归类已知 adapter。
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
fn guarded_send_command(
    pane_id: &str,
    expected_command: &str,
    expected_pid: Option<u32>,
    send_args: &str,
) -> String {
    let pane = shell_quote(pane_id);
    // Identity and numeric columns cannot contain |. Keep the raw command
    // last so cut preserves every | inside its complete value. Compare it as
    // shell-quoted data, without depending on a tmux version's q: escape set.
    let info = "tmux list-panes -a -F '#{pane_id}|#{pane_dead}|#{pane_pid}|#{pane_current_command}'";
    let match_line = format!("$({info} | awk -F '|' -v id={pane} '$1 == id {{print; exit}}')");
    // The pane's root process is the target-side incarnation fence: a pane
    // that was reused by another program after the observation is refused
    // on the far side, not only in the local pre-check.
    let pid_guard = match expected_pid {
        Some(pid) if pid > 0 => format!(
            "[ \"$(printf '%s' \"$line\" | cut -d '|' -f3)\" = {expected} ] || {{ echo seat:replaced >&2; exit {EXIT_PANE_REPLACED}; }}; ",
            expected = shell_quote(&pid.to_string()),
        ),
        _ => String::new(),
    };
    format!(
        "line={match_line}; \
         [ -n \"$line\" ] || {{ echo seat:missing >&2; exit {EXIT_PANE_MISSING}; }}; \
         [ \"$(printf '%s' \"$line\" | cut -d '|' -f2)\" = \"0\" ] || {{ echo seat:dead >&2; exit {EXIT_PANE_DEAD}; }}; \
         [ \"$(printf '%s' \"$line\" | cut -d '|' -f4-)\" = {expected} ] || {{ echo seat:changed >&2; exit {EXIT_PROGRAM_CHANGED}; }}; \
         {pid_guard}\
         tmux send-keys -t {pane} {send_args}",
        expected = shell_quote(expected_command),
    )
}

/// ExecResult → Ok(())，或把守卫退出码/失败映射为 SeatRefusal / ExecFailure。
fn refusal_or_success(result: &ExecResult) -> Result<()> {
    // A reserved shell exit code proves a guard refusal only when the
    // executor actually observed that exit without lost I/O or timeout.
    // Preserve legacy results with no outcome/diagnostic, but never let a
    // new uncertainty become a no-send fact merely because its code matches.
    if result.timed_out
        || !matches!(result.outcome, None | Some(ExecOutcome::Exited))
        || result.diagnostic.is_some()
    {
        return Err(ExecFailure { result: result.clone() }.into());
    }
    match result.exit_code {
        EXIT_PANE_MISSING => Err(SeatRefusal::PaneMissing.into()),
        EXIT_PANE_DEAD => Err(SeatRefusal::PaneDead.into()),
        EXIT_PROGRAM_CHANGED => Err(SeatRefusal::ProgramChanged.into()),
        EXIT_PANE_REPLACED => Err(SeatRefusal::PaneReplaced.into()),
        _ => result.require_success(),
    }
}

/// Tmux observation — screen capture preserving attributes.
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

/// Structured seat observation returned by TmuxSeat::capture_pane.
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

/// Strip ANSI/VT100 escape sequences from raw terminal output.
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

/// Return lines from plain text that look like interactive prompts.
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

fn parse_local_identity_line(
    stdout: &str,
    expected_pane: &str,
) -> Result<(TmuxPane, u32, String)> {
    let line = stdout.strip_suffix('\n').unwrap_or(stdout);
    let mut fields = parse_tmux_fields(line)
        .ok_or_else(|| anyhow::anyhow!("local tmux identity response has invalid field quoting"))?;
    anyhow::ensure!(fields.len() == 13 && !line.contains('\n'),
        "local tmux identity response is incomplete or ambiguous");
    let pane = parse_pane_fields(&mut fields)
        .ok_or_else(|| anyhow::anyhow!("local tmux pane identity unavailable"))?;
    anyhow::ensure!(pane.pane_id == expected_pane && pane.pane_pid > 0 && fields[10] == "0",
        "local tmux pane is missing, dead or replaced");
    anyhow::ensure!(!pane.session.is_empty(), "local tmux session unavailable");
    fields[1].parse::<u32>()?;
    fields[2].parse::<u32>()?;
    let server_pid: u32 = fields[11].parse()?;
    anyhow::ensure!(server_pid > 0 && !fields[12].is_empty(),
        "local tmux server identity unavailable");
    Ok((pane, server_pid, fields[12].to_owned()))
}

/// Linux stat field 22, after the *last* closing comm parenthesis. The comm
/// field may itself contain spaces and parentheses. Never use argv or a
/// rounded wall-clock start time as a replacement for this kernel token.
fn linux_process_start_ticks(stat: &str, expected_pid: u32) -> Result<u64> {
    let (pid, _) = stat.split_once(" (")
        .ok_or_else(|| anyhow::anyhow!("process stat has no pid/comm boundary"))?;
    anyhow::ensure!(pid.parse::<u32>()? == expected_pid && expected_pid > 0,
        "process stat pid mismatch");
    let (_, tail) = stat.rsplit_once(") ")
        .ok_or_else(|| anyhow::anyhow!("process stat has no comm/state boundary"))?;
    let mut fields = tail.split_whitespace();
    let state = fields.next().ok_or_else(|| anyhow::anyhow!("process state unavailable"))?;
    anyhow::ensure!(state != "Z" && state != "X" && state != "x", "process has exited");
    let ticks: u64 = fields.nth(18)
        .ok_or_else(|| anyhow::anyhow!("process start token unavailable"))?.parse()?;
    anyhow::ensure!(ticks > 0, "process start token unavailable");
    Ok(ticks)
}

#[cfg(test)]
mod local_incarnation_tests {
    use super::*;

    #[test]
    fn process_start_uses_kernel_field_with_parentheses_in_comm() -> Result<()> {
        let mut tail = vec!["0"; 22];
        tail[0] = "S";
        tail[19] = "987654321";
        let stat = format!("42 (agent ) with (spaces)) {}", tail.join(" "));
        assert_eq!(linux_process_start_ticks(&stat, 42)?, 987654321);
        assert!(linux_process_start_ticks(&stat, 43).is_err());
        assert!(linux_process_start_ticks("42 (agent) S 1 2", 42).is_err());
        tail[0] = "Z";
        assert!(linux_process_start_ticks(&format!("42 (agent) {}", tail.join(" ")), 42).is_err());
        Ok(())
    }

    #[test]
    fn local_observation_refuses_missing_dead_and_ambiguous_server_identity() -> Result<()> {
        let valid = "owned|0|1|%4|omp|/project|1|80|24|41|0|20|/owned/tmux.sock\n";
        let (pane, server, socket) = parse_local_identity_line(valid, "%4")?;
        assert_eq!(pane.pane_pid, 41);
        assert_eq!(server, 20);
        assert_eq!(socket, "/owned/tmux.sock");
        assert!(parse_local_identity_line(valid, "%5").is_err());
        assert!(parse_local_identity_line(&valid.replace("|0|20|", "|1|20|"), "%4").is_err());
        assert!(parse_local_identity_line(&valid.replace("|20|", "|0|"), "%4").is_err());
        assert!(parse_local_identity_line(&format!("{valid}{valid}"), "%4").is_err());
        assert!(parse_local_identity_line("owned|0|1|%4|omp|/project|1|80|24", "%4").is_err());
        Ok(())
    }

    #[test]
    fn printable_wire_preserves_quoted_command_cwd_and_socket_identity() -> Result<()> {
        // Literal tmux q: output: pipes, backslashes, spaces, apostrophes,
        // format-looking text, shell substitutions and Unicode stay data.
        let line = r"owner\ seat|0|1|%4|agent\|cli\\v2|/project/字\ space/\'\$\(touch\ nope\)\;\#\{pane_id\}\|x\\y|1|80|24|41|0|20|/owned/socket\ name\|x\\y";
        let (pane, server, socket) = parse_local_identity_line(line, "%4")?;
        assert_eq!(pane.session, "owner seat");
        assert_eq!(pane.current_command, "agent|cli\\v2");
        assert_eq!(pane.cwd, "/project/字 space/'$(touch nope);#{pane_id}|x\\y");
        assert_eq!(server, 20);
        assert_eq!(socket, "/owned/socket name|x\\y");
        assert!(parse_local_identity_line(&format!("{line}\\"), "%4").is_err());
        assert!(parse_local_identity_line(&line.replace("owner\\ seat", "owner\\zseat"), "%4").is_err());
        // Control-bearing data is unavailable identity, never normalized
        // into a different path or mistaken for another wire record.
        assert!(parse_local_identity_line(&line.replace("owner\\ seat", "owner\tseat"), "%4").is_err());
        assert!(parse_local_identity_line(&line.replace("owner\\ seat", "owner\nseat"), "%4").is_err());
        assert!(!PANE_FORMAT.contains(char::is_control));
        Ok(())
    }

    #[test]
    fn guard_keeps_complete_command_after_fixed_identity_columns() {
        let command = guarded_send_command("%4", "agent|cli\\v2 '$(nope)", Some(41), "'literal'");
        assert!(command.contains("#{pane_id}|#{pane_dead}|#{pane_pid}|#{pane_current_command}"));
        assert!(command.contains("awk -F '|'"));
        assert!(command.contains("cut -d '|' -f2"));
        assert!(command.contains("cut -d '|' -f3"));
        assert!(command.contains("cut -d '|' -f4-"));
        assert!(command.contains(&shell_quote("agent|cli\\v2 '$(nope)")));
        assert!(!command.contains('\t'));
        assert!(command.ends_with("tmux send-keys -t '%4' 'literal'"));
    }
}

fn read_local_process_identity(
    pane: &TmuxPane,
    server_pid: u32,
    socket_path: String,
) -> Result<shenko_store::LocalTmuxIdentity> {
    let identity = shenko_store::LocalTmuxIdentity {
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned(),
        pane_pid: pane.pane_pid,
        pane_start_ticks: linux_process_start_ticks(
            &std::fs::read_to_string(format!("/proc/{}/stat", pane.pane_pid))?, pane.pane_pid)?,
        server_pid,
        server_start_ticks: linux_process_start_ticks(
            &std::fs::read_to_string(format!("/proc/{server_pid}/stat"))?, server_pid)?,
        socket_path,
    };
    identity.validate()?;
    Ok(identity)
}

/// Tmux seat adapter — discovers and controls tmux panes.
/// This is the first required seat path: "已存在的 tmux 前台 TUI"
pub struct TmuxSeat {
    executor: SshExecutor,
    local: bool,
    observation_seq: std::sync::atomic::AtomicU64,
}

impl TmuxSeat {
    pub fn local() -> Self {
        Self {
            executor: SshExecutor::new(SshTarget::default()),
            local: true,
            observation_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn remote(target: SshTarget) -> Self {
        Self {
            executor: SshExecutor::over_ssh(target),
            local: false,
            observation_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Observe local pane/server identity without changing tmux state. Two
    /// matching tmux reads bracket two matching /proc reads; failure is
    /// unavailable evidence, never a command-name fallback. Remote discovery
    /// retains its existing contract and never reads this host's /proc.
    pub(crate) async fn local_pane_identity(
        &self,
        pane_id: &str,
    ) -> Result<(TmuxPane, shenko_store::LocalTmuxIdentity)> {
        anyhow::ensure!(self.local, "local identity requires the local tmux transport");
        anyhow::ensure!(pane_id.strip_prefix('%').is_some_and(|id|
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())),
            "invalid exact tmux pane id");
        let command = format!(
            "tmux display-message -p -t {} '{}|#{{pid}}|#{{q:socket_path}}'",
            shell_quote(pane_id), PANE_FORMAT,
        );
        let first = self.executor.run_command(&command).await?;
        first.require_success()?;
        let (pane, server_pid, socket_path) = parse_local_identity_line(&first.stdout, pane_id)?;
        let identity = read_local_process_identity(&pane, server_pid, socket_path)?;
        let second = self.executor.run_command(&command).await?;
        second.require_success()?;
        anyhow::ensure!(first.stdout == second.stdout,
            "local tmux locator changed during identity observation");
        anyhow::ensure!(
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() == identity.boot_id
                && linux_process_start_ticks(&std::fs::read_to_string(
                    format!("/proc/{}/stat", pane.pane_pid))?, pane.pane_pid)? == identity.pane_start_ticks
                && linux_process_start_ticks(&std::fs::read_to_string(
                    format!("/proc/{server_pid}/stat"))?, server_pid)? == identity.server_start_ticks,
            "local tmux process incarnation changed during observation");
        Ok((pane, identity))
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

    /// Discover agent seats by the pane's foreground process group.
    ///
    /// Walks `ps` from each pane's pane_pid, collecting the process subtree in
    /// deterministic order (BFS, siblings by pid), then keeps only members of
    /// the pane root's foreground process group (matching the root's `tpgid`): background and
    /// stopped jobs share the subtree but cannot receive pane input — e.g. a
    /// stopped `claude` under a foreground `bash` is never a seat. Bare
    /// shells, wrappers and dead panes are not seats; the first non-shell
    /// foreground program classifies the seat (claude/omp/codex → typed
    /// adapter, anything else → Other). A zero pane_pid or one absent from the
    /// ps table yields empty process evidence (never a walk from init). If
    /// `ps` with pgid/tpgid columns is unavailable on the remote,
    /// classification falls back to pane_current_command with an empty
    /// `processes` evidence list — callers should treat that seat as
    /// unverified.
    pub async fn discover_agent_seats(&self) -> Result<Vec<AgentSeat>> {
        let panes = self.list_panes().await?;
        let ps_out = self
            .executor
            .run_command("ps -axo pid=,ppid=,pgid=,tpgid=,args= 2>/dev/null || echo ''")
            .await?;
        ps_out.require_success()?;
        let table = parse_ps_table(&ps_out.stdout);

        Ok(panes
            .into_iter()
            .filter(|p| !p.dead)
            .filter_map(|pane| {
                let subtree = process_subtree(pane.pane_pid, &table);
                let kind = classify_pane_foreground(&table, &subtree, &pane.current_command)?;
                let processes = subtree
                    .iter()
                    .filter_map(|pid| table.get(pid).map(|row| row.argv.clone()))
                    .collect();
                Some(AgentSeat { pane, processes, kind })
            })
            .collect())
    }

    /// Capture a pane preserving ANSI/attributes (capture-pane -e).
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

    /// 发送按键前，调用方须先持久提交 intent。此适配器本身没有账本。
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
    /// 退回 shell 或旧 pane_id 不得接收为 seat 准备的输入。
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
        expected_pid: Option<u32>,
    ) -> Result<ExecResult> {
        let cmd = guarded_send_command(pane_id, expected_command, expected_pid, &shell_quote(keys));
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
        expected_pid: Option<u32>,
    ) -> Result<ExecResult> {
        let send_args = format!("{} Enter", shell_quote(text));
        let cmd = guarded_send_command(pane_id, expected_command, expected_pid, &send_args);
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

    /// 与 discover_agent_seats 相同的证据收集：root → 子树 argv。
    fn subtree_evidence(root: u32, table: &HashMap<u32, ProcRow>) -> Vec<String> {
        process_subtree(root, table)
            .iter()
            .filter_map(|pid| table.get(pid).map(|r| r.argv.clone()))
            .collect()
    }

    #[test]
    fn discovery_classifies_only_the_foreground_process_group() {
        // 列：pid ppid pgid tpgid args；tpgid 是控制终端的前台进程组。
        let ps = "    1      0     1    -1 /sbin/init\n\
                  200      1   200   201 -bash\n\
                  201    200   201   201 claude\n\
                  300      1   300   300 /bin/zsh\n\
                  301    300   301   300 claude\n\
                  400      1   400   400 -bash\n\
                  401    400   401   400 sudo claude\n\
                  402    401   402   402 claude\n\
                  999      1   999    -1 unrelated daemon --flag\n";
        let table = parse_ps_table(ps);

        // 场景 1：claude 占据前台进程组（pgid 201 == tpgid 201），登录 shell
        // 的 pgid 200 ≠ tpgid 被排除；证据按确定顺序（根在前、同层 pid 升序），
        // 无关主机进程不入子树。
        assert_eq!(process_subtree(200, &table), vec![200, 201]);
        assert_eq!(
            subtree_evidence(200, &table),
            vec!["-bash".to_string(), "claude".to_string()]
        );
        assert_eq!(
            classify_pane_foreground(&table, &process_subtree(200, &table), &"claude".into()),
            Some(SeatKind::ClaudeCode)
        );

        // 场景 2：claude 被 Ctrl-Z 停成后台任务（pgid 301 ≠ tpgid 300），
        // zsh 才是前台——输入会落进 shell；前台组只剩 shell。
        // 即便较早的 tmux 快照仍称 claude，也不能覆盖已观察到的 shell。
        assert_eq!(
            foreground_group_argv(&table, &process_subtree(300, &table)),
            vec!["/bin/zsh".to_string()]
        );
        assert_eq!(
            classify_pane_foreground(&table, &process_subtree(300, &table), &"claude".into()),
            None
        );
        // 场景 3：后台后代在内层 PTY 占前台，不等于占有 pane 的输入终端。
        assert_eq!(
            classify_pane_foreground(&table, &process_subtree(400, &table), &"bash".into()),
            None
        );
    }

    #[test]
    fn zero_pane_pid_yields_empty_evidence_and_fallback() {
        // init 与无关进程存在：未知根绝不允许向下走到它们。
        let table = parse_ps_table("    1      0     1    -1 /sbin/init\n500      1   500   500 vim\n");

        // 场景 4：旧 tmux 缺两列的 9 列行兼容解析为 pane_pid=0/dead=false，
        // 空证据 + current_command fallback 归类，不呈现无关进程为现场。
        let pane = parse_pane_line("s1|0|0|%7|omp|/repo|1|80|24").unwrap();
        assert_eq!((pane.pane_pid, pane.dead), (0, false));
        assert!(process_subtree(pane.pane_pid, &table).is_empty());
        assert_eq!(
            classify_pane_foreground(&table, &[], &pane.current_command),
            Some(SeatKind::Omp)
        );
    }

    #[test]
    fn guarded_send_refuses_stale_panes_and_maps_results() -> Result<()> {
        use anyhow::Context;
        // 场景 5-7：三种守卫退出码各自映射到可下转的 SeatRefusal（参数化，每案独立计数）。
        for (code, refusal) in [
            (EXIT_PANE_MISSING, SeatRefusal::PaneMissing),
            (EXIT_PANE_DEAD, SeatRefusal::PaneDead),
            (EXIT_PROGRAM_CHANGED, SeatRefusal::ProgramChanged),
        ] {
            let err = refusal_or_success(&result(code)).unwrap_err();
            assert_eq!(err.downcast_ref::<SeatRefusal>(), Some(&refusal));
        }
        // 场景 8：普通远端失败保持 ExecFailure，不伪装成 seat 拒绝也不伪装成功。
        let err = refusal_or_success(&result(1)).unwrap_err();
        assert!(err.downcast_ref::<ExecFailure>().is_some());

        // New outcome window: output loss can coexist with a reserved guard
        // exit code. Keep the complete Unknown result, not a typed no-send
        // refusal that would allow the answer to be attempted again.
        let mut uncertain = result(EXIT_PANE_REPLACED);
        uncertain.stdout = "original partial output\n\0字节".into();
        uncertain.stderr = "original capture failure\n".into();
        uncertain.execution_id = Some(uuid::Uuid::new_v4());
        uncertain.outcome = Some(ExecOutcome::Unknown);
        uncertain.diagnostic = Some(ExecDiagnostic {
            reason: ExecUncertainty::OutputUnavailable,
            local_process_reaped: true,
            reconciliation: "original output observation is incomplete; do not resend".into(),
        });
        let expected = serde_json::to_value(&uncertain)?;
        let error = refusal_or_success(&uncertain).err().context("Unknown must remain a failure")?;
        assert!(error.downcast_ref::<SeatRefusal>().is_none());
        let failure = error.downcast_ref::<ExecFailure>().context("retain original ExecFailure")?;
        assert_eq!(serde_json::to_value(&failure.result)?, expected);
        Ok(())
    }
}
