use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;
use wasmtime::{Engine, Instance, Linker, Module, Store, Val};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

/// Guest path where the persistent WASM home is mounted for WASI commands
/// (SPEC §8.3: every mount declares its source path and guest path; the home
/// mount is the one preopen a WASI session always gets).
pub const HOME_GUEST_PATH: &str = "/home";

/// Real WASM runtime backed by wasmtime-45.
///
/// Holds the engine and an owned store. Modules are compiled by the engine and
/// instantiated against the store; function calls dispatch through the store's
/// context. A `home_dir` records the persistent WASM home so checkpoint/restore
/// can snapshot the marker across sessions and so WASI commands see the same
/// filesystem state under [`HOME_GUEST_PATH`].
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

    /// Run a WASI preview1 command module and return its exit code.
    ///
    /// `wasm_or_wat` may be either a binary module or WAT text. The persistent
    /// home directory (`self.home_dir`) is created if missing and preopened as
    /// the guest's [`HOME_GUEST_PATH`] with read/write access, so anything the
    /// guest writes survives across sessions and stays visible on the host.
    /// The module must export `_start`; a normal return is exit code 0 and a
    /// `proc_exit(n)` trap becomes exit code `n` instead of an error.
    pub fn run_wasi_module(&mut self, wasm_or_wat: &[u8]) -> Result<i32> {
        if self.home_dir.as_os_str().is_empty() {
            return Err(anyhow!(
                "wasm home_dir is not configured; call restore() or set home_dir before running WASI"
            ));
        }
        fs::create_dir_all(&self.home_dir).context("create wasm home dir")?;

        let engine = self.engine.clone();
        let home = self.home_dir.clone();
        let bytes = wasm_or_wat.to_vec();
        // wasmtime-wasi's synchronous shims call `Handle::block_on`, which
        // panics when the calling thread already drives a tokio runtime. Run
        // on a fresh thread with no ambient runtime so the shims fall back to
        // wasmtime-wasi's own runtime; this keeps the API sync-safe from async
        // callers (the Shenko main loop) as well as from plain tests.
        let joined = std::thread::Builder::new()
            .name("shenko-wasi".into())
            .spawn(move || run_wasi_blocking(&engine, &home, &bytes))
            .context("spawn wasi execution thread")?
            .join()
            .map_err(|_| anyhow!("wasi execution thread panicked"))?;
        joined
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

/// Instantiate a WASI preview1 command with the home preopen and run `_start`.
fn run_wasi_blocking(engine: &Engine, home: &Path, wasm_or_wat: &[u8]) -> Result<i32> {
    let module = Module::new(engine, wasm_or_wat)
        .map_err(|e| anyhow!("compile WASI module: {e}"))?;

    let mut builder = WasiCtxBuilder::new();
    // wasmtime 45 has its own `Error` type (not `std::error::Error`), so
    // `anyhow::Context` does not apply here; embed the path in the message.
    builder
        .preopened_dir(home, HOME_GUEST_PATH, DirPerms::all(), FilePerms::all())
        .map_err(|e| anyhow!("preopen {} at {HOME_GUEST_PATH}: {e}", home.display()))?;
    let wasi = builder.build_p1();

    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx)
        .map_err(|e| anyhow!("configure wasi preview1 linker: {e}"))?;

    let mut store = Store::new(engine, wasi);
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| anyhow!("instantiate WASI module: {e}"))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| anyhow!("WASI command must export `_start`: {e}"))?;

    match start.call(&mut store, ()) {
        Ok(()) => Ok(0),
        // `proc_exit` surfaces as an `I32Exit` trap; that is a normal exit,
        // not a runtime failure.
        Err(e) => match e.downcast_ref::<I32Exit>() {
            Some(exit) => Ok(exit.0),
            None => Err(anyhow!("run WASI `_start`: {e:?}")),
        },
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

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("shenko-wasm-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

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

    /// Proves `_start` is actually executed: a clean command exits 0 and
    /// `proc_exit(n)` is reported as exit code `n` rather than an error.
    #[test]
    fn wasi_start_runs_and_reports_exit_code() -> Result<()> {
        let mut rt = WasmRuntime::new()?;
        rt.home_dir = temp_home("start");

        let plain = r#"(module (func (export "_start")))"#;
        assert_eq!(rt.run_wasi_module(plain.as_bytes())?, 0, "empty _start exits 0");

        let exit7 = r#"
            (module
              (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
              ;; real WASI commands export their linear memory; wiggle's host
              ;; glue requires it even when only proc_exit is imported
              (memory (export "memory") 1)
              (func (export "_start") (call $proc_exit (i32.const 7)))
            )
        "#;
        assert_eq!(
            rt.run_wasi_module(exit7.as_bytes())?,
            7,
            "proc_exit(7) surfaces as exit code 7"
        );

        fs::remove_dir_all(&rt.home_dir).ok();
        Ok(())
    }

    /// Proves the persistent home is mounted: fd 3 is a preopen named `/home`,
    /// and the guest can create and write a file there through WASI, which the
    /// host then reads back from the same directory. Each failure exits with a
    /// distinct code so a broken stage is identifiable.
    #[test]
    fn home_dir_preopened_and_guest_writable() -> Result<()> {
        let mut rt = WasmRuntime::new()?;
        let home = temp_home("preopen");
        fs::write(home.join("host-visible.txt"), b"persistent")?;
        rt.home_dir = home.clone();

        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_prestat_get"
                (func $fd_prestat_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_prestat_dir_name"
                (func $fd_prestat_dir_name (param i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit"
                (func $proc_exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 200) "guest-out.txt")
              (data (i32.const 240) "hi\n")
              (func (export "_start")
                ;; fd 3 is the first preopen; a non-zero errno means it is missing.
                (if (i32.ne (call $fd_prestat_get (i32.const 3) (i32.const 0)) (i32.const 0))
                  (then (call $proc_exit (i32.const 41))))
                ;; The u8 tag is at offset 0, so pr_name_len is at offset 4.
                (if (i32.ne (i32.load (i32.const 4)) (i32.const 5))
                  (then (call $proc_exit (i32.const 42))))
                (if (i32.ne (call $fd_prestat_dir_name (i32.const 3) (i32.const 64) (i32.const 5))
                            (i32.const 0))
                  (then (call $proc_exit (i32.const 43))))
                ;; "/home" as little-endian u32 plus the trailing 'e'.
                (if (i32.ne (i32.load (i32.const 64)) (i32.const 0x6d6f682f))
                  (then (call $proc_exit (i32.const 44))))
                (if (i32.ne (i32.load8_u (i32.const 68)) (i32.const 0x65))
                  (then (call $proc_exit (i32.const 45))))
                ;; iovec {buf=240, len=3} at offset 300.
                (i32.store (i32.const 300) (i32.const 240))
                (i32.store (i32.const 304) (i32.const 3))
                ;; path_open(dirfd=3, dirflags=0, path=200 "guest-out.txt" len=13,
                ;;           oflags=CREAT|TRUNC=9, rights_base=FD_WRITE=64,
                ;;           rights_inheriting=0, fdflags=0, opened_fd=310)
                (if (i32.ne
                      (call $path_open
                        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 13)
                        (i32.const 9) (i64.const 64) (i64.const 0)
                        (i32.const 0) (i32.const 312))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 51))))
                ;; fd_write(fd=[310], iovs=300, iovs_len=1, nwritten=320)
                (if (i32.ne
                      (call $fd_write (i32.load (i32.const 312)) (i32.const 300)
                                      (i32.const 1) (i32.const 320))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 52))))
                (if (i32.ne (i32.load (i32.const 320)) (i32.const 3))
                  (then (call $proc_exit (i32.const 53))))
              )
            )
        "#;

        let code = rt.run_wasi_module(wat.as_bytes())?;
        assert_eq!(code, 0, "guest check failed at stage {code}");
        assert_eq!(
            fs::read_to_string(home.join("guest-out.txt"))?,
            "hi\n",
            "guest write should land in the host home directory"
        );
        assert!(home.join("host-visible.txt").exists());
        fs::remove_dir_all(&home).ok();
        Ok(())
    }
}
