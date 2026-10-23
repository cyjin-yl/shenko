use tracing::info;
pub struct HostExecutor;
impl HostExecutor {
    pub fn new() -> Self { Self }
    pub async fn run_command(&self, cmd: &str, _args: &[&str]) -> anyhow::Result<CommandOutput> { info!(cmd, "host exec (stub)"); Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: 0 }) }
}
impl Default for HostExecutor { fn default() -> Self { Self::new() } }
pub struct CommandOutput { pub stdout: String, pub stderr: String, pub exit_code: i32 }
