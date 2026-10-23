use tracing::info;
use crate::config::NapCatConfig;
pub struct NapCatClient { pub config: NapCatConfig }
impl NapCatClient {
    pub fn new(config: NapCatConfig) -> Self { Self { config } }
    pub async fn connect(&self) -> anyhow::Result<()> { info!(url=%self.config.ws_url, "NapCat connecting (stub)"); Ok(()) }
}
