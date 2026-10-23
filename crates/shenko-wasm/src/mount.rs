use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec { pub source: PathBuf, pub guest_path: String, pub writable: bool, pub persistent: bool, pub versioned: bool }
