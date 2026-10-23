use serde::{Deserialize, Serialize};
use tracing::info;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTarget { pub host: String, pub port: u16, pub user: String, pub key_path: Option<std::path::PathBuf> }
pub struct SshExecutor { pub target: SshTarget }
impl SshExecutor {
    pub fn new(target: SshTarget) -> Self { Self { target } }
    pub async fn run_command(&self, cmd: &str) -> anyhow::Result<String> { info!(host=%self.target.host, cmd, "ssh exec (stub)"); Ok(String::new()) }
}
