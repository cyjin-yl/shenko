use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;
use wasmtime::{Engine, Instance, Module, Store, Val};

/// Real WASM runtime backed by wasmtime-45.
///
/// Holds the engine and an owned store. Modules are compiled by the engine and
/// instantiated against the store; function calls dispatch through the store's
/// context. A `home_dir` records the persistent WASM home so checkpoint/restore
/// can snapshot the marker across sessions.
pub struct WasmRuntime {
    pub engine: Engine,
    pub store: Option<Store<()>>,
    pub home_dir: PathBuf,
}

impl WasmRuntime {
    /// Create a runtime with a default-config engine and a fresh empty store.
    pub fn new() -> Result<Self> {
        info!("WASM runtime init (wasmtime-45)");
        let engine = Engine::default();
        let store = Store::new(&engine, ());
        Ok(Self {
            engine,
            store: Some(store),
            home_dir: PathBuf::new(),
        })
    }

    /// Compile and instantiate a WAT module against the owned store.
    pub fn instantiate_module(&mut self, wat: &str) -> Result<Instance> {
        let store = self
            .store
            .as_mut()
            .context("wasm store not initialized")?;
        let module = Module::new(&self.engine, wat.as_bytes())
            .map_err(|e| anyhow!("compile WAT module: {e}"))?;
        let instance = Instance::new(store, &module, &[])
            .map_err(|e| anyhow!("instantiate module: {e}"))?;
        Ok(instance)
    }

    /// Call an exported function by name on the given instance.
    pub fn call_func(
        &mut self,
        instance: &Instance,
        name: &str,
        args: &[Val],
    ) -> Result<Vec<Val>> {
        let store = self
            .store
            .as_mut()
            .context("wasm store not initialized")?;
        let func = instance
            .get_func(&mut *store, name)
            .ok_or_else(|| anyhow!("exported function `{}` not found", name))?;
        let result_count = func.ty(&*store).results().len();
        // wasmtime 45 uses the out-parameter call ABI: allocate a slot per
        // declared result, then let the engine overwrite each slot with the
        // produced value (initial values are placeholders, never read).
        let mut results = vec![Val::I32(0); result_count];
        func.call(store, args, &mut results)
            .map_err(|e| anyhow!("call function: {e}"))?;
        Ok(results)
    }

    /// Write a snapshot marker into the WASM home directory.
    pub fn checkpoint(&self, home_dir: &Path) -> Result<()> {
        fs::create_dir_all(home_dir).context("create wasm home dir")?;
        let marker = home_dir.join(".wasm_checkpoint");
        let body = format!("wasmtime-45\n{}\n", chrono::Utc::now().to_rfc3339());
        fs::write(&marker, body).context("write checkpoint marker")?;
        info!(home_dir = %home_dir.display(), "WASM checkpoint written");
        Ok(())
    }

    /// Read the snapshot marker from the WASM home directory and adopt it.
    pub fn restore(&mut self, home_dir: &Path) -> Result<()> {
        let marker = home_dir.join(".wasm_checkpoint");
        let contents = fs::read_to_string(&marker)
            .with_context(|| format!("read checkpoint marker at {}", marker.display()))?;
        let mut lines = contents.lines();
        let version = lines.next().unwrap_or("");
        if !version.starts_with("wasmtime-") {
            return Err(anyhow!("checkpoint marker version mismatch: {}", version));
        }
        self.home_dir = home_dir.to_path_buf();
        info!(home_dir = %home_dir.display(), "WASM checkpoint restored");
        Ok(())
    }
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new().expect("WASM runtime init failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_instantiate() -> Result<()> {
        let mut rt = WasmRuntime::new()?;
        // A module that exports an identity function over i32.
        let wat = r#"
            (module
              (func (export "identity") (param i32) (result i32)
                local.get 0)
            )
        "#;
        let instance = rt.instantiate_module(wat)?;
        let results = rt.call_func(&instance, "identity", &[Val::I32(42)])?;
        assert_eq!(results.len(), 1, "identity should return one value");
        match results[0] {
            Val::I32(v) => assert_eq!(v, 42, "identity(42) should be 42"),
            ref other => panic!("expected i32 result, got {:?}", other),
        }
        Ok(())
    }
}
