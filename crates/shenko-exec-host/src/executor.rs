use std::process::Stdio;
use tracing::info;

pub struct HostExecutor;
impl HostExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Direct argv execution for trusted child-side callers. No shell, and no
    /// fabricated empty success when process creation or exit fails.
    pub async fn run_command(&self, cmd: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        info!(cmd, "host exec");
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}
impl Default for HostExecutor {
    fn default() -> Self {
        Self::new()
    }
}
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
