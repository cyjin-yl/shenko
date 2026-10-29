use crate::mount::{MountMode, MountSpec, validate_mounts};
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use tracing::info;
use wasmtime::{Engine, Instance, Linker, Module, Store, Val};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};
pub use wasmtime_wasi::cli::{InputFile, OutputFile, StdinStream, StdoutStream};
/// Bounded in-memory streams for callers that must cap guest output
/// (Playground backend receipts) without binding the whole crate to a
/// preview-2 path. A full buffer makes the guest's write trap, so the bound
/// is a real contract rather than silent truncation.
pub use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

/// Guest path where the persistent WASM home is mounted for WASI commands
/// (SPEC §8.3: every mount declares its source path and guest path; the home
/// mount is the one preopen a WASI session always gets).
pub const HOME_GUEST_PATH: &str = "/home";

/// Per-invocation preview1 arguments, environment, and standard streams.
/// Arguments include argv[0] only when the caller supplies it. Neither
/// arguments, environment, nor stdio are inherited from the host implicitly.
///
/// The caller owns the policy and lifetime of each stream. For example,
/// `InputFile` and `OutputFile` bind already-open files without buffering the
/// whole output in the runtime. Keep another handle to an output destination
/// if its bytes are needed after a nonzero exit or a trap.
pub struct WasiCommand<I, O, E> {
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    stdin: I,
    stdout: O,
    stderr: E,
}

impl<I, O, E> WasiCommand<I, O, E> {
    /// Explicitly choose all three streams, including any intentional sinks.
    pub fn new(
        stdin: I,
        stdout: O,
        stderr: E,
    ) -> Self {
        Self {
            arguments: Vec::new(),
            environment: Vec::new(),
            stdin,
            stdout,
            stderr,
        }
    }

    fn validate(&self) -> Result<()> {
        for (index, argument) in self.arguments.iter().enumerate() {
            if argument.contains('\0') {
                bail!("WASI_COMMAND_INVALID: argument {index} contains NUL");
            }
        }
        for (index, (key, value)) in self.environment.iter().enumerate() {
            if key.is_empty() || key.contains('\0') || key.contains('=') {
                bail!("WASI_COMMAND_INVALID: environment key {index} is empty or contains NUL or '='");
            }
            if value.contains('\0') {
                bail!("WASI_COMMAND_INVALID: environment value {index} contains NUL");
            }
        }
        Ok(())
    }
}

/// Real WASM runtime backed by wasmtime-45.
///
/// Holds the engine and an owned store. Modules are compiled by the engine and
/// instantiated against the store; function calls dispatch through the store's
/// context. A configured mount list binds the persistent home and any other
/// explicitly granted host directories. Only file home/metadata recovery is
/// represented here; live module memory, KV and sockets are not checkpoints.
pub struct WasmRuntime {
    pub engine: Engine,
    pub store: Option<Store<()>>,
    pub home_dir: PathBuf,
    pub mounts: Vec<MountSpec>,
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
            mounts: Vec::new(),
        })
    }

    /// Construct the real WASI home from a managed manifest. There is no
    /// implicit host-directory fallback and no silent COW-to-RW downgrade.
    pub fn with_mounts(mounts: Vec<MountSpec>) -> Result<Self> {
        let home_dir = mounts
            .iter()
            .find(|mount| mount.guest_path == HOME_GUEST_PATH)
            .map(|mount| mount.source.clone())
            .ok_or_else(|| anyhow!("WASM_PROFILE_UNAVAILABLE: /home mount is missing"))?;
        validate_mounts(&mounts, &home_dir)?;
        let mut runtime = Self::new()?;
        runtime.home_dir = home_dir;
        runtime.mounts = mounts;
        Ok(runtime)
    }

    /// Compile and instantiate a WAT module against the owned store.
    pub fn instantiate_module(&mut self, wat: &str) -> Result<Instance> {
        let store = self.store.as_mut().context("wasm store not initialized")?;
        let module = Module::new(&self.engine, wat.as_bytes())
            .map_err(|e| anyhow!("compile WAT module: {e}"))?;
        let instance =
            Instance::new(store, &module, &[]).map_err(|e| anyhow!("instantiate module: {e}"))?;
        Ok(instance)
    }

    /// Call an exported function by name on the given instance.
    pub fn call_func(&mut self, instance: &Instance, name: &str, args: &[Val]) -> Result<Vec<Val>> {
        let store = self.store.as_mut().context("wasm store not initialized")?;
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
    /// `wasm_or_wat` may be either a binary module or WAT text. Every preopen
    /// comes from `self.mounts` with its declared RO/RW permissions. Wasmtime
    /// currently reopens the checked pathname, so an ancestor swap can still
    /// redirect a preopen; do not treat validation as a capability binding.
    /// The command supplies exact argv, environment and streams, without a
    /// shell or inherited process state. A normal return is exit code 0 and a
    /// `proc_exit(n)` trap is exit code `n`; other traps are errors, and any
    /// writes already made to caller-supplied streams or mounts remain effects.
    pub fn run_wasi_module<I, O, E>(
        &mut self,
        wasm_or_wat: &[u8],
        command: WasiCommand<I, O, E>,
    ) -> Result<i32>
    where
        I: StdinStream + 'static,
        O: StdoutStream + 'static,
        E: StdoutStream + 'static,
    {
        if self.home_dir.as_os_str().is_empty() {
            return Err(anyhow!(
                "WASM_PROFILE_UNAVAILABLE: call with_mounts before running WASI"
            ));
        }
        command.validate()?;

        let engine = &self.engine;
        let home = &self.home_dir;
        let mounts = &self.mounts;
        // wasmtime-wasi's synchronous shims call `Handle::block_on`, which
        // panics when the calling thread already drives a tokio runtime. Run
        // on a fresh thread with no ambient runtime so the shims fall back to
        // wasmtime-wasi's own runtime; this keeps the API sync-safe from async
        // callers (the Shenko main loop) as well as from plain tests. Scoped
        // borrowing avoids copying the module and mount list for each run.
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("shenko-wasi".into())
                .spawn_scoped(scope, move || {
                    run_wasi_blocking(engine, home, mounts, wasm_or_wat, command)
                })
                .context("spawn wasi execution thread")?
                .join()
                .map_err(|_| anyhow!("wasi execution thread panicked"))?
        })
    }
}

/// Instantiate a WASI preview1 command with the home preopen and run `_start`.
fn run_wasi_blocking<I, O, E>(
    engine: &Engine,
    home: &Path,
    mounts: &[MountSpec],
    wasm_or_wat: &[u8],
    command: WasiCommand<I, O, E>,
) -> Result<i32>
where
    I: StdinStream + 'static,
    O: StdoutStream + 'static,
    E: StdoutStream + 'static,
{
    let canonical_sources = validate_mounts(mounts, home)?;
    let module =
        Module::new(engine, wasm_or_wat).map_err(|e| anyhow!("compile WASI module: {e}"))?;

    let mut builder = WasiCtxBuilder::new();
    builder
        .args(&command.arguments)
        .envs(&command.environment)
        .stdin(command.stdin)
        .stdout(command.stdout)
        .stderr(command.stderr);
    for (mount, source) in mounts.iter().zip(canonical_sources.iter()) {
        let (dir_perms, file_perms) = match mount.mode {
            MountMode::ReadOnly => (DirPerms::READ, FilePerms::READ),
            MountMode::ReadWrite => (DirPerms::all(), FilePerms::all()),
            MountMode::CopyOnWrite => unreachable!("validated COW must fail before preopen"),
        };
        // wasmtime 45 has its own `Error` type (not `std::error::Error`), so
        // `anyhow::Context` does not apply here; embed the binding in the message.
        builder
            .preopened_dir(source, &mount.guest_path, dir_perms, file_perms)
            .map_err(|e| {
                anyhow!(
                    "preopen {} at {} ({:?}): {e}",
                    source.display(),
                    mount.guest_path,
                    mount.mode
                )
            })?;
    }
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
    use std::fs;
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    fn no_stdio() -> WasiCommand<std::io::Empty, std::io::Empty, std::io::Empty> {
        WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty())
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-wasm-{tag}-{}", uuid::Uuid::new_v4()));
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
        let home = temp_home("start");
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        let plain = r#"(module (func (export "_start")))"#;
        assert_eq!(
            rt.run_wasi_module(plain.as_bytes(), no_stdio())?,
            0,
            "empty _start exits 0"
        );

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
            rt.run_wasi_module(exit7.as_bytes(), no_stdio())?,
            7,
            "proc_exit(7) surfaces as exit code 7"
        );

        fs::remove_dir_all(&home).ok();
        Ok(())
    }

    /// Proves the persistent home is mounted: fd 3 is a preopen named `/home`,
    /// and the guest can create and write a file there through WASI, which the
    /// host then reads back from the same directory. Each failure exits with a
    /// distinct code so a broken stage is identifiable.
    #[test]
    fn home_dir_preopened_and_guest_writable() -> Result<()> {
        let home = temp_home("preopen");
        fs::write(home.join("host-visible.txt"), b"persistent")?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

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

        let code = rt.run_wasi_module(wat.as_bytes(), no_stdio())?;
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

    /// One focused mount-semantics scenario: a real guest cannot create a file
    /// through a RO preopen, and requesting unimplemented COW fails instead of
    /// silently becoming direct RW.
    #[test]
    fn read_only_and_cow_mounts_keep_their_semantics() -> Result<()> {
        let home = temp_home("modes-home");
        let read_only = temp_home("modes-ro");
        let mut rt = WasmRuntime::with_mounts(vec![
            MountSpec::new(
                home.clone(),
                HOME_GUEST_PATH,
                MountMode::ReadWrite,
                true,
                false,
            ),
            MountSpec::new(
                read_only.clone(),
                "/read-only",
                MountMode::ReadOnly,
                false,
                false,
            ),
        ])?;
        let write_attempt = r#"
            (module
              (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 200) "blocked.txt")
              (func (export "_start")
                ;; The second preopen is fd 4. A successful create on a RO
                ;; mount is a contract violation; any WASI errno is rejected.
                (if (i32.eq
                      (call $path_open
                        (i32.const 4) (i32.const 0) (i32.const 200) (i32.const 11)
                        (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 300))
                      (i32.const 0))
                  (then (call $proc_exit (i32.const 90))))
              )
            )
        "#;
        assert_eq!(rt.run_wasi_module(write_attempt.as_bytes(), no_stdio())?, 0);
        assert!(!read_only.join("blocked.txt").exists());

        let error = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::CopyOnWrite,
            true,
            true,
        )])
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
        assert!(error.contains("MOUNT_MODE_UNSUPPORTED"), "{error}");
        assert!(error.contains("no direct-RW fallback"), "{error}");
        Ok(())
    }

    #[test]
    fn command_passes_arguments_environment_and_file_streams_to_guest() -> Result<()> {
        let home = temp_home("command-streams").canonicalize()?;
        let input_path = home.join("stdin.bin");
        let output_path = home.join("stdout.bin");
        let error_path = home.join("stderr.bin");
        fs::write(&input_path, b"in\0put")?;
        let mut command = WasiCommand::new(
            InputFile::new(fs::File::open(input_path)?),
            OutputFile::new(fs::File::create(&output_path)?),
            OutputFile::new(fs::File::create(&error_path)?),
        );
        command.arguments = vec!["tool".into(), "a b".into(), "".into(), "你好".into()];
        command.environment = vec![("KEY".into(), "guest=one".into()), ("EMPTY".into(), "".into())];
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;

        // Write the exact WASI argv buffer and then the raw stdin bytes to
        // stdout; write the exact environment buffer to stderr. Lengths come
        // from WASI, not from the fixture, so unexpected inherited entries
        // cannot go unnoticed.
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "args_sizes_get"
                (func $args_sizes_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "args_get"
                (func $args_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_sizes_get"
                (func $environ_sizes_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_get"
                (func $environ_get (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_read"
                (func $fd_read (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start")
                (if (call $args_sizes_get (i32.const 0) (i32.const 4))
                  (then (call $exit (i32.const 20))))
                (if (i32.ne (i32.load (i32.const 0)) (i32.const 4))
                  (then (call $exit (i32.const 21))))
                (if (call $args_get (i32.const 64) (i32.const 256))
                  (then (call $exit (i32.const 22))))
                (if (call $environ_sizes_get (i32.const 8) (i32.const 12))
                  (then (call $exit (i32.const 23))))
                (if (i32.ne (i32.load (i32.const 8)) (i32.const 2))
                  (then (call $exit (i32.const 24))))
                (if (call $environ_get (i32.const 96) (i32.const 512))
                  (then (call $exit (i32.const 25))))
                (i32.store (i32.const 128) (i32.const 256))
                (i32.store (i32.const 132) (i32.load (i32.const 4)))
                (if (call $fd_write (i32.const 1) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 26))))
                (i32.store (i32.const 128) (i32.const 1024))
                (i32.store (i32.const 132) (i32.const 64))
                (if (call $fd_read (i32.const 0) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 27))))
                (i32.store (i32.const 132) (i32.load (i32.const 140)))
                (if (call $fd_write (i32.const 1) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 28))))
                (i32.store (i32.const 128) (i32.const 512))
                (i32.store (i32.const 132) (i32.load (i32.const 12)))
                (if (call $fd_write (i32.const 2) (i32.const 128) (i32.const 1) (i32.const 140))
                  (then (call $exit (i32.const 29))))
                (call $exit (i32.const 7))))
        "#;
        assert_eq!(rt.run_wasi_module(wat.as_bytes(), command)?, 7);
        let mut expected_stdout = "tool\0a b\0\0你好\0".as_bytes().to_vec();
        expected_stdout.extend_from_slice(b"in\0put");
        assert_eq!(fs::read(output_path)?, expected_stdout);
        assert_eq!(fs::read(error_path)?, b"KEY=guest=one\0EMPTY=\0");
        Ok(())
    }

    #[test]
    fn command_stream_effects_remain_visible_after_a_trap() -> Result<()> {
        let home = temp_home("command-trap").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let stdout = MemoryOutputPipe::new(64);
        let stderr = MemoryOutputPipe::new(64);
        let command = WasiCommand::new(std::io::empty(), stdout.clone(), stderr.clone());
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "prefix")
              (data (i32.const 120) "diag")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const 6))
                (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 90))))
                (i32.store (i32.const 0) (i32.const 120))
                (i32.store (i32.const 4) (i32.const 4))
                (if (call $fd_write (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 91))))
                unreachable))
        "#;
        let error = rt.run_wasi_module(wat.as_bytes(), command).unwrap_err();
        assert!(error.to_string().contains("run WASI `_start`"), "{error}");
        assert_eq!(stdout.contents().as_ref(), b"prefix");
        assert_eq!(stderr.contents().as_ref(), b"diag");
        Ok(())
    }

    #[test]
    fn consecutive_commands_do_not_share_process_state() -> Result<()> {
        let home = temp_home("command-isolation").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "args_sizes_get"
                (func $args (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "environ_sizes_get"
                (func $env (param i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start")
                (if (call $args (i32.const 0) (i32.const 4))
                  (then (call $exit (i32.const 90))))
                (if (call $env (i32.const 8) (i32.const 12))
                  (then (call $exit (i32.const 91))))
                (call $exit (i32.add (i32.mul (i32.load (i32.const 0)) (i32.const 10))
                                     (i32.load (i32.const 8))))))
        "#;
        let mut first = no_stdio();
        first.arguments = vec!["one".into(), "two".into()];
        first.environment = vec![("A".into(), "one".into())];
        assert_eq!(rt.run_wasi_module(wat.as_bytes(), first)?, 21);
        assert_eq!(rt.run_wasi_module(wat.as_bytes(), no_stdio())?, 0);
        Ok(())
    }

    #[test]
    fn invalid_command_strings_fail_before_guest_effects() -> Result<()> {
        let home = temp_home("command-invalid").canonicalize()?;
        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home,
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "started")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const 7))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))
        "#;
        let mut reject = |arguments, environment| {
            let stdout = MemoryOutputPipe::new(64);
            let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty());
            command.arguments = arguments;
            command.environment = environment;
            let error = rt.run_wasi_module(wat.as_bytes(), command).unwrap_err();
            assert!(error.to_string().contains("WASI_COMMAND_INVALID"), "{error}");
            assert!(stdout.contents().is_empty());
        };
        reject(vec!["bad\0arg".into()], vec![]);
        reject(vec![], vec![("BAD=KEY".into(), "value".into())]);
        reject(vec![], vec![("KEY".into(), "bad\0value".into())]);
        Ok(())
    }
}
