//! One actual CLI/control journey. The HTTP observer closes the first real
//! inference connection without generating an answer: this checks admission
//! order and restart control, not model quality or a completed model task.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use shenko_types::ids::EntityId;

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shenko"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .arg("--data-dir")
        .arg(root)
        .arg("--log")
        .arg("warn");
    command
}

struct OwnedRuntime(Child);

impl OwnedRuntime {
    fn signal_group(&mut self, signal: i32) -> Result<()> {
        if self.0.try_wait()?.is_none() {
            let pid = i32::try_from(self.0.id()).context("owned process ID is out of range")?;
            // Include descendants still belonging to this owned process group.
            if unsafe { libc::kill(-pid, signal) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "owned process exit deadline exceeded"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.signal_group(libc::SIGINT)?;
        let status = self.wait_for(Duration::from_secs(30))?;
        ensure!(
            status.success(),
            "paused runtime did not stop cleanly: {status}"
        );
        Ok(())
    }

    fn kill_and_join(&mut self) -> Result<()> {
        self.signal_group(libc::SIGKILL)?;
        self.wait_for(Duration::from_secs(5))?;
        Ok(())
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.kill_and_join() {
            eprintln!("owned runtime cleanup was not confirmed: {error:#}");
        }
    }
}

struct Observer {
    address: SocketAddr,
    requests: mpsc::Receiver<Vec<u8>>,
    hold_next_metadata: Arc<AtomicBool>,
    held_metadata: mpsc::Receiver<()>,
    closed_metadata: mpsc::Receiver<&'static str>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl Observer {
    fn new(logs: &Path) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let (sent, requests) = mpsc::channel();
        let (held, held_metadata) = mpsc::channel();
        let (closed, closed_metadata) = mpsc::channel();
        let hold_next_metadata = Arc::new(AtomicBool::new(false));
        let holding = Arc::clone(&hold_next_metadata);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let logs = logs.to_path_buf();
        let worker = thread::spawn(move || -> Result<()> {
            let mut number = 0;
            while !stopping.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut wire = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer)?;
                    ensure!(count > 0, "client closed before sending its HTTP request");
                    wire.extend_from_slice(&buffer[..count]);
                    let Some(end) = wire.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = std::str::from_utf8(&wire[..end])?;
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then_some(value.trim())
                        })
                        .unwrap_or("0")
                        .parse::<usize>()?;
                    if wire.len() < end + 4 + length {
                        continue;
                    }
                    fs::write(logs.join(format!("model-{number:03}.request")), &wire)?;
                    number += 1;
                    if headers.starts_with("GET /v1/models ")
                        && holding.swap(false, Ordering::AcqRel)
                    {
                        // The owner observes the complete real GET before
                        // SIGINT. Keep this connection open without replying:
                        // success must come from runtime cancellation, not
                        // from a fixture response or a dropped listener.
                        held.send(())?;
                        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                        let deadline = Instant::now() + Duration::from_secs(40);
                        let ending = loop {
                            ensure!(
                                !stopping.load(Ordering::Acquire),
                                "metadata observer stopped before the client"
                            );
                            ensure!(
                                Instant::now() < deadline,
                                "client did not cancel pending metadata HTTP"
                            );
                            match stream.read(&mut buffer) {
                                Ok(0) => break "client_eof",
                                Ok(_) => {
                                    anyhow::bail!("unexpected bytes after the held metadata GET")
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) => {}
                                Err(error)
                                    if error.kind() == std::io::ErrorKind::ConnectionReset =>
                                {
                                    break "client_reset";
                                }
                                Err(error) => return Err(error.into()),
                            }
                        };
                        fs::write(logs.join("metadata-cancelled.txt"), ending)?;
                        closed.send(ending)?;
                        break;
                    }
                    if headers.starts_with("POST /v1/chat/completions ") {
                        sent.send(wire[end + 4..end + 4 + length].to_vec())?;
                        // No generated or terminal model answer is supplied.
                    } else {
                        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")?;
                    }
                    break;
                }
            }
            Ok(())
        });
        Ok(Self {
            address,
            requests,
            hold_next_metadata,
            held_metadata,
            closed_metadata,
            stop,
            worker: Some(worker),
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("HTTP observer panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("HTTP observer cleanup failed: {error:#}");
        }
    }
}

fn start(
    root: &Path,
    data_dir: &Path,
    port: u16,
    boundary: EntityId,
    model: SocketAddr,
    label: &str,
) -> Result<OwnedRuntime> {
    let log = fs::File::create(root.join(format!("{label}.log")))?;
    Ok(OwnedRuntime(
        command(data_dir)
            .arg("start")
            .arg("--port")
            .arg(port.to_string())
            .arg("--pause-at-start")
            .arg(boundary.to_string())
            .env("SHENKO_USE_LOCAL_MODEL", "1")
            .env("SHENKO_LOCAL_MODEL_URL", format!("http://{model}"))
            .env("SHENKO_LOCAL_MODEL", "dispatch-observer")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()?,
    ))
}

fn ready(runtime: &mut OwnedRuntime, port: u16) -> Result<Value> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        ensure!(
            runtime.0.try_wait()?.is_none(),
            "runtime exited before its real UI was ready"
        );
        if let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            stream.write_all(
                b"GET /api/self HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
            )?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            if let Some((headers, body)) = response.split_once("\r\n\r\n")
                && headers.starts_with("HTTP/1.1 200")
            {
                let value: Value = serde_json::from_str(body)?;
                if value.get("subject_id").and_then(Value::as_str).is_some() {
                    return Ok(value);
                }
            }
        }
        ensure!(
            Instant::now() < deadline,
            "runtime UI readiness deadline exceeded"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn control(root: &Path, operation: &str, boundary: EntityId) -> Result<Value> {
    let prefix = root
        .parent()
        .context("installation has no parent directory")?
        .join(format!("control-{operation}-{}", EntityId::new()));
    let stdout = prefix.with_extension("stdout");
    let stderr = prefix.with_extension("stderr");
    // Files keep the actual output without an unbounded pipe reader or a
    // full-pipe deadlock while this owner observes the process deadline.
    let mut process = OwnedRuntime(
        command(root)
            .arg("runtime")
            .arg(operation)
            .arg(boundary.to_string())
            .stdout(Stdio::from(fs::File::create(&stdout)?))
            .stderr(Stdio::from(fs::File::create(&stderr)?))
            .process_group(0)
            .spawn()?,
    );
    let status = match process.wait_for(Duration::from_secs(30)) {
        Ok(status) => status,
        Err(error) => {
            let cleanup = process.kill_and_join();
            return Err(error.context(format!(
                "runtime {operation} deadline; cleanup={cleanup:?}; stderr={}",
                fs::read_to_string(&stderr).unwrap_or_default()
            )));
        }
    };
    ensure!(
        status.success(),
        "runtime {operation} failed: {}",
        fs::read_to_string(&stderr)?
    );
    serde_json::from_slice(&fs::read(&stdout)?)
        .context("runtime control did not return its durable record")
}

fn runtime_log(root: &Path, label: &str) -> String {
    let path = root.join(format!("{label}.log"));
    match fs::read_to_string(&path) {
        Ok(log) => format!("{}:\n{log}", path.display()),
        Err(error) => format!("could not read {}: {error}", path.display()),
    }
}

#[test]
fn public_start_pause_survives_process_reopen_and_resume_alone_admits_inference() -> Result<()> {
    let root: PathBuf =
        std::env::temp_dir().join(format!("shenko-start-boundary-{}", EntityId::new()));
    fs::create_dir(&root)?;
    // Run the journey's existing process/observer cleanup before export. A
    // failure does not certify cleanup; only actual socket endings are named.
    // Copy only top-level diagnostic files, never the installation.
    let outcome = journey(&root);
    let retained = retain_observation(&root, &outcome);
    match (outcome, retained) {
        (Ok(()), Ok(())) => {
            fs::remove_dir_all(root)?;
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(retention)) => Err(retention),
        (Err(error), Err(retention)) => {
            Err(error.context(format!("diagnostic retention also failed: {retention:#}")))
        }
    }
}

fn retain_observation(root: &Path, outcome: &Result<()>) -> Result<()> {
    let closed = root.join("metadata-cancelled.txt");
    let metadata_connection_end = if closed.exists() {
        Some(fs::read_to_string(&closed)?)
    } else {
        None
    };
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "journey_passed":outcome.is_ok(),
            "error":outcome.as_ref().err().map(|error| format!("{error:#}")),
            "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),
            "metadata_connection_end":metadata_connection_end,
            "model_completion_tested":false,
        }))?,
    )?;
    if let Some(destination) = std::env::var_os("SHENKO_START_BOUNDARY_EVIDENCE") {
        let destination = PathBuf::from(destination);
        fs::create_dir_all(&destination)?;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::copy(entry.path(), destination.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

fn journey(root: &Path) -> Result<()> {
    let data_dir = root.join("installation");
    let mut observer = Observer::new(root)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let boundary = EntityId::new();
    let mut runtime = start(root, &data_dir, port, boundary, observer.address, "first")?;
    let first = ready(&mut runtime, port).with_context(|| runtime_log(root, "first"))?;
    let paused = control(&data_dir, "status", boundary)?;
    ensure!(paused["state"] == "paused" && paused["boundary_at"].is_object());
    let requested_seq = paused["requested_at"]["seq"]
        .as_u64()
        .context("missing request sequence")?;
    let boundary_seq = paused["boundary_at"]["seq"]
        .as_u64()
        .context("missing boundary sequence")?;
    ensure!(
        paused["requested_at"]["ledger_id"] == paused["boundary_at"]["ledger_id"]
            && requested_seq < boundary_seq,
        "startup did not retain the original ordered request/boundary ledger facts"
    );
    ensure!(
        observer.requests.try_recv().is_err(),
        "startup admitted an idle model request before resume"
    );
    runtime.stop().with_context(|| runtime_log(root, "first"))?;
    drop(runtime);

    // The same installed subject and durable pause must also stop cleanly
    // while the real provider's read-only metadata connection is still held.
    observer.hold_next_metadata.store(true, Ordering::Release);
    let mut discovering = start(
        root,
        &data_dir,
        port,
        boundary,
        observer.address,
        "metadata-stopped",
    )?;
    observer
        .held_metadata
        .recv_timeout(Duration::from_secs(30))
        .with_context(|| runtime_log(root, "metadata-stopped"))?;
    ensure!(
        discovering.0.try_wait()?.is_none(),
        "runtime exited before metadata cancellation"
    );
    ensure!(
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))
            .is_err(),
        "UI started before the held metadata observation completed"
    );
    ensure!(
        observer.closed_metadata.try_recv().is_err(),
        "metadata peer closed before SIGINT"
    );
    discovering
        .stop()
        .with_context(|| runtime_log(root, "metadata-stopped"))?;
    observer
        .closed_metadata
        .recv_timeout(Duration::from_secs(5))
        .with_context(|| runtime_log(root, "metadata-stopped"))?;
    ensure!(
        observer.requests.try_recv().is_err(),
        "metadata cancellation admitted inference"
    );
    let still_paused = control(&data_dir, "status", boundary)?;
    ensure!(
        still_paused["request_id"] == paused["request_id"] && still_paused["state"] == "paused"
    );
    drop(discovering);

    let mut reopened = start(
        root,
        &data_dir,
        port,
        boundary,
        observer.address,
        "restarted",
    )?;
    ensure!(
        ready(&mut reopened, port).with_context(|| runtime_log(root, "restarted"))?["subject_id"]
            == first["subject_id"]
    );
    let rebound = control(&data_dir, "status", boundary)?;
    ensure!(rebound["request_id"] == paused["request_id"] && rebound["state"] == "paused");
    ensure!(
        observer.requests.try_recv().is_err(),
        "reopen admitted inference while the original pause was held"
    );
    let resumed = control(&data_dir, "resume", boundary)?;
    ensure!(resumed["state"] == "resumed" && resumed["closed_at"].is_object());
    let body = observer
        .requests
        .recv_timeout(Duration::from_secs(30))
        .with_context(|| runtime_log(root, "restarted"))?;
    let request: Value = serde_json::from_slice(&body)?;
    ensure!(
        request["model"] == "dispatch-observer" && request["stream"] == true,
        "resume did not admit the actual local-provider HTTP request"
    );
    reopened
        .kill_and_join()
        .with_context(|| runtime_log(root, "restarted"))?;
    drop(reopened);
    observer.finish()?;
    drop(observer);
    Ok(())
}
