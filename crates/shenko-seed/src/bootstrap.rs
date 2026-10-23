use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tracing::info;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedConfig { pub data_dir: PathBuf, pub agent_name: Option<String>, pub napcat_ws_url: Option<String>, pub model: String }
impl Default for SeedConfig {
    fn default() -> Self {
        Self { data_dir: dirs_next::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("shenko"), agent_name: None, napcat_ws_url: None, model: "claude-opus-5-5".to_string() }
    }
}
pub struct Bootstrap { pub config: SeedConfig }
impl Bootstrap {
    pub fn new(config: SeedConfig) -> Self { Self { config } }
    pub fn validate(&self) -> anyhow::Result<()> { info!("bootstrap validate"); Ok(()) }
}
