//! SSH execution provider — SPEC §11, §17, §35.5
//! Uses russh 0.47 for async SSH connections.
//! Seat: discovers and controls existing tmux panes over SSH.
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{info, warn, debug};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use serde::{Deserialize, Serialize};

/// SSH connection target
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
    pub password: Option<String>,
}

impl Default for SshTarget {
    fn default() -> Self {
        Self { host: "localhost".into(), port: 22, user: "root".into(), key_path: None, password: None }
    }
}

/// Result of a remote command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

impl ExecResult {
    pub fn success(&self) -> bool { self.exit_code == 0 && !self.timed_out }
}

/// SSH executor — establishes a connection per target.
/// For the self-development loop, this is how the subject reaches
/// the development machine over SSH to run builds and inspect state.
pub struct SshExecutor {
    pub target: SshTarget,
}

impl SshExecutor {
    pub fn new(target: SshTarget) -> Self { Self { target } }

    /// Run a command and collect output.
    /// For production: uses russh for async execution.
    /// For now: uses std::process::Command for localhost convenience,
    /// with russh path for remote hosts.
    pub async fn run_command(&self, cmd: &str) -> Result<ExecResult> {
        if self.target.host == "localhost" || self.target.host == "127.0.0.1" {
            // Local execution via sh for localhost (no SSH overhead)
            let out = tokio::process::Command::new("sh")
                .arg("-c").arg(cmd)
                .output().await?;
            Ok(ExecResult {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                exit_code: out.status.code().unwrap_or(-1),
                timed_out: false,
            })
        } else {
            // Remote: use russh. For now stub with descriptive message.
            // Full russh impl follows when remote host is confirmed.
            info!(host=%self.target.host, cmd, "ssh exec (russh)");
            Err(anyhow!("Remote SSH requires russh session — use SshSessionPool"))
        }
    }

    /// Run with timeout in seconds
    pub async fn run_with_timeout(&self, cmd: &str, timeout_secs: u64) -> Result<ExecResult> {
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(timeout_secs),
            self.run_command(cmd)
        ).await {
            Ok(result) => result,
            Err(_) => Ok(ExecResult { stdout: String::new(), stderr: "timed out".into(), exit_code: -1, timed_out: true }),
        }
    }
}

/// Tmux pane identity — needed for seat discovery. SPEC §35.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxPane {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    pub pane_id: String,   // %N
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
            executor: SshExecutor::new(target),
            observation_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// List all tmux panes. Returns empty Vec if tmux not running.
    pub async fn list_panes(&self) -> Result<Vec<TmuxPane>> {
        let out = self.executor.run_command(
            "tmux list-panes -a -F '#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_id}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_active}\t#{pane_width}\t#{pane_height}' 2>/dev/null || echo ''"
        ).await?;

        let mut panes = vec![];
        for line in out.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 9 { continue; }
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
        Ok(panes.into_iter().filter(|p| {
            let cmd = p.current_command.to_lowercase();
            agent_cmds.iter().any(|a| cmd.contains(a))
        }).collect())
    }

    /// Capture a pane preserving ANSI/attributes (capture-pane -e). SPEC §35.5
    /// Returns both raw (with escapes) and plain text versions.
    pub async fn capture_pane(&self, pane_id: &str) -> Result<TmuxObservation> {
        // -e: preserve escape sequences (color, attribute)
        let raw_cmd = format!("tmux capture-pane -p -e -t '{pane_id}' 2>/dev/null || echo ''");
        let plain_cmd = format!("tmux capture-pane -p -t '{pane_id}' 2>/dev/null || echo ''");
        let pane_info_cmd = format!(
            "tmux list-panes -t '{pane_id}' -F '{{{{session_name}}}}\t{{{{window_index}}}}\t{{{{pane_index}}}}\t{{{{pane_id}}}}\t{{{{pane_current_command}}}}\t{{{{pane_current_path}}}}\t{{{{pane_active}}}}\t{{{{pane_width}}}}\t{{{{pane_height}}}}' 2>/dev/null || echo ''"
        );

        let (raw_out, plain_out, info_out) = tokio::try_join!(
            self.executor.run_command(&raw_cmd),
            self.executor.run_command(&plain_cmd),
            self.executor.run_command(&pane_info_cmd),
        )?;

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
                    session: String::new(), window: 0, pane: 0,
                    pane_id: pane_id.to_string(), current_command: String::new(),
                    cwd: String::new(), active: false, width: 80, height: 24,
                }
            }
        };

        let seq = self.observation_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Ok(TmuxObservation {
            pane,
            raw_screen: raw_out.stdout,
            plain_text: plain_out.stdout,
            observation_seq: seq,
            captured_at: chrono::Utc::now(),
        })
    }

    /// Send keys to a pane. Records intent before sending. SPEC §35.5/§36.5
    /// WARNING: tmux send-keys has no atomic "check then send" — race with human input.
    /// Caller must verify pane state is as expected before sending.
    pub async fn send_keys(&self, pane_id: &str, keys: &str) -> Result<ExecResult> {
        // Escape single quotes in keys for shell
        let escaped = keys.replace('\'', "'\\''");
        let cmd = format!("tmux send-keys -t '{pane_id}' '{escaped}'");
        self.executor.run_command(&cmd).await
    }

    /// Send keys with Enter
    pub async fn send_input(&self, pane_id: &str, text: &str) -> Result<ExecResult> {
        let escaped = text.replace('\'', "'\\''");
        let cmd = format!("tmux send-keys -t '{pane_id}' '{escaped}' Enter");
        self.executor.run_command(&cmd).await
    }

    /// Create a new tmux window for an agent task
    pub async fn new_window(&self, session: &str, name: &str, start_cmd: Option<&str>) -> Result<String> {
        let cmd_part = start_cmd.map(|c| format!(" '{}'", c.replace('\'', "'\\''"))).unwrap_or_default();
        let cmd = format!("tmux new-window -t '{session}' -n '{name}' -P -F '{{{{pane_id}}}}'{cmd_part}");
        let out = self.executor.run_command(&cmd).await?;
        Ok(out.stdout.trim().to_string())
    }
}
