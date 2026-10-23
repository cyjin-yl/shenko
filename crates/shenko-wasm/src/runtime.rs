use tracing::info;
pub struct WasmRuntime { pub engine_version: String }
impl WasmRuntime { pub fn new() -> anyhow::Result<Self> { info!("WASM runtime init (wasmtime-45)"); Ok(Self { engine_version: "wasmtime-45".to_string() }) } }
impl Default for WasmRuntime { fn default() -> Self { Self { engine_version: "wasmtime-45".to_string() } } }
