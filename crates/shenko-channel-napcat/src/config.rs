use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NapCatConfig { pub ws_url: String, pub access_token: Option<String>, pub bot_qq: u64, pub admin_qq: Vec<u64>, pub reconnect_interval_ms: u64 }
impl Default for NapCatConfig { fn default() -> Self { Self { ws_url: "ws://127.0.0.1:3001".to_string(), access_token: None, bot_qq: 0, admin_qq: vec![], reconnect_interval_ms: 5000 } } }
