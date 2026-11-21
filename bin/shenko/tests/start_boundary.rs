//! One actual CLI/control journey. The HTTP observer holds the real inference
//! connection through ordinary stop and authority-failure cleanup: admission,
//! owned shutdown and Unknown recovery, never a model-quality success claim.
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

fn command(root: &Path, log_level: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shenko"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .arg("--data-dir")
        .arg(root)
        .arg("--log")
        .arg(log_level);
    command
}

#[test]
fn cli_only_napcat_zero_account_is_refused_before_store_open() -> Result<()> {
    let root = std::env::temp_dir().join(format!("shenko-start-napcat-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let data_dir = root.join("occupied-data");
    fs::write(&data_dir, b"original file")?;
    let output = command(&data_dir, "warn")
        .args(["start", "--napcat-cli-url", "http://127.0.0.1:9", "--napcat-account", "0"])
        .output()?;
    ensure!(!output.status.success(), "zero NapCat account was admitted");
    let stderr = String::from_utf8(output.stderr)?;
    ensure!(stderr.contains("NapCat persistent ingress requires a positive --napcat-account"),
        "startup did not refuse the account before opening the data path: {stderr}");
    ensure!(fs::read(data_dir)? == b"original file", "startup changed the existing data path");
    Ok(())
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

    fn pause(&mut self) -> Result<()> {
        self.signal_group(libc::SIGSTOP)?;
        let pid = i32::try_from(self.0.id()).context("owned process ID is out of range")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut status = 0;
            // Observe this owned child's actual stop, not an assumed delay.
            // WUNTRACED does not reap a stopped child; Child still owns exit.
            let observed = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if observed == pid {
                ensure!(libc::WIFSTOPPED(status), "owned process exited before the teardown barrier");
                return Ok(());
            }
            if observed < 0 { return Err(std::io::Error::last_os_error().into()); }
            ensure!(Instant::now() < deadline, "owned process did not enter its stop barrier");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.signal_group(libc::SIGINT)?;
        let status = self.wait_for(Duration::from_secs(30))?;
        ensure!(
            status.success(),
            "runtime did not stop cleanly: {status}"
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
    closed_inference: mpsc::Receiver<&'static str>,
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
        let (inference_closed, closed_inference) = mpsc::channel();
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
                    if headers.starts_with("GET /v1/models ") {
                        let body = r#"{"data":[{"id":"dispatch-observer","max_model_len":65536}]}"#;
                        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                        if let Err(error) = stream.write_all(response.as_bytes()) {
                            // An ordinary read-only discovery probe has its own shorter deadline.
                            // The held metadata and inference paths still require a peer ending.
                            if !matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset) {
                                return Err(error).context("writing the ordinary model discovery response");
                            }
                            fs::write(logs.join(format!("model-{:03}.reply-error", number - 1)), error.to_string())?;
                        }
                    } else if headers.starts_with("POST /v1/chat/completions ") {
                        sent.send(wire[end + 4..end + 4 + length].to_vec())?;
                        // No response, server timeout or deliberate peer
                        // disconnect can unblock the runtime. Only its owned
                        // Runtime cancellation can close this local HTTP exchange.
                        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                        let deadline = Instant::now() + Duration::from_secs(40);
                        let ending = loop {
                            ensure!(!stopping.load(Ordering::Acquire), "inference observer stopped before the client");
                            ensure!(Instant::now() < deadline, "client did not close held inference HTTP");
                            match stream.read(&mut buffer) {
                                Ok(0) => break "client_eof",
                                Ok(_) => anyhow::bail!("unexpected bytes after the held inference POST"),
                                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut) => {},
                                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break "client_reset",
                                Err(error) => return Err(error.into()),
                            }
                        };
                        fs::write(logs.join(format!("model-{:03}.inference-ending", number - 1)), ending)?;
                        let original = logs.join("inference-cancelled.txt");
                        if !original.exists() { fs::write(original, ending)?; }
                        inference_closed.send(ending)?;
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
            closed_inference,
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
    boundary: Option<EntityId>,
    model: SocketAddr,
    label: &str,
) -> Result<OwnedRuntime> {
    let log = fs::File::create(root.join(format!("{label}.log")))?;
    let mut command = command(data_dir, "info");
    command.arg("start").arg("--port").arg(port.to_string());
    if let Some(boundary) = boundary { command.arg("--pause-at-start").arg(boundary.to_string()); }
    Ok(OwnedRuntime(
        command
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
    // This includes opening the installation and initializing the Node store.
    wait_for_self(runtime, port, Duration::from_secs(120), None)
}

fn wait_for_self(
    runtime: &mut OwnedRuntime,
    port: u16,
    timeout: Duration,
    expected_status: Option<&str>,
) -> Result<Value> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
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
            match stream.read_to_string(&mut response) {
                Ok(_) => {}
                // A single accepted socket can expire while startup is still
                // settling. The original overall observation deadline holds.
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            if let Some((headers, body)) = response.split_once("\r\n\r\n")
                && headers.starts_with("HTTP/1.1 200")
            {
                let value: Value = serde_json::from_str(body)?;
                if value.get("subject_id").and_then(Value::as_str).is_some()
                    && expected_status.is_none_or(|status| value["status"] == status)
                {
                    return Ok(value);
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    match expected_status {
        Some(status) => anyhow::bail!("runtime UI did not report {status} within {timeout:?}"),
        None => anyhow::bail!("runtime UI readiness deadline exceeded"),
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
        command(root, "warn")
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
    let closed = root.join("inference-cancelled.txt");
    let inference_connection_end = if closed.exists() { Some(fs::read_to_string(&closed)?) } else { None };
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "journey_passed":outcome.is_ok(),
            "error":outcome.as_ref().err().map(|error| format!("{error:#}")),
            "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),
            "metadata_connection_end":metadata_connection_end,
            "inference_connection_end":inference_connection_end,
            "remote_completion_claimed":false,
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
    let mut runtime = start(root, &data_dir, port, Some(boundary), observer.address, "first")?;
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
        Some(boundary),
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
        Some(boundary),
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
    // Resume commits before the subject has sealed S0 and passed the broker's
    // durable dispatch fence. Give those writes their own bounded window.
    let body = observer
        .requests
        .recv_timeout(Duration::from_secs(90))
        .context("resumed subject never reached the observed inference POST")
        .with_context(|| runtime_log(root, "restarted"))?;
    let request: Value = serde_json::from_slice(&body)?;
    ensure!(
        request["model"] == "dispatch-observer" && request["stream"] == true,
        "resume did not admit the actual local-provider HTTP request"
    );
    ensure!(observer.closed_inference.try_recv().is_err(), "inference peer closed before SIGINT");
    reopened.stop().with_context(|| runtime_log(root, "restarted"))?;
    observer.closed_inference.recv_timeout(Duration::from_secs(5))
        .with_context(|| runtime_log(root, "restarted"))?;
    drop(reopened);

    let agent: EntityId = first["subject_id"].as_str().context("subject identity")?.parse()?;
    let (request_id, attempt_id, terminal_ref) = {
        let store = Arc::new(shenko_store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?);
        let (_, step) = store.load_subject(agent)?;
        ensure!(step.state == shenko_types::subject::SubjectStepState::AwaitingModel,
            "shutdown manufactured a committed decision for the held generation");
        let request_id = step.active_request_id.context("held request identity")?;
        let attempt = store.subject_request_attempt(request_id)?.context("held request attempt")?;
        let terminal = attempt.terminal.context("shutdown did not retain original Unknown evidence")?;
        ensure!(terminal.outcome["termination"]["kind"] == "unknown");
        let result_ref = terminal.result_ref.context("original Unknown result seal")?;
        let result: shenko_broker::request::InferenceResult = serde_json::from_slice(&store.cas.read_object(&result_ref.digest)?)?;
        ensure!(result.request_id == request_id && result.termination.kind == shenko_broker::request::TerminationKind::Unknown);
        let provider = shenko_broker::local_provider::LocalModelProvider::new(
            format!("http://{}", observer.address), "dispatch-observer".into());
        let (_broker, handle) = shenko_broker::broker::Broker::with_local_provider(provider, 1)?;
        shenko_subject::loop_runner::restore_inference_accounting(&store, agent, &handle)?;
        ensure!(handle.active_count() == 1, "original Unknown debt was absent before any new dispatch");
        fs::write(root.join("inference-shutdown.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "request_id":request_id,"attempt_id":attempt.attempt_id,"termination":result.termination,
            "recovered_compute_debt":handle.active_count(),"remote_stopped":false,"result_ref":result_ref,
        }))?)?;
        (request_id, attempt.attempt_id, result_ref)
    };

    // Reopen the exact uncertain request, with its original identity and
    // physical debt already restored. The UI remains available and blocked;
    // no replacement generation or S1 is invented by process restart.
    let mut reopened = start(root, &data_dir, port, None, observer.address, "unknown-reopened")?;
    ready(&mut reopened, port).with_context(|| runtime_log(root, "unknown-reopened"))?;
    let blocked = wait_for_self(&mut reopened, port, Duration::from_secs(10), Some("blocked"))
        .with_context(|| runtime_log(root, "unknown-reopened"))?;
    ensure!(blocked["subject_id"] == first["subject_id"]);
    ensure!(blocked["blocked_reason"].is_string());
    ensure!(observer.requests.try_recv().is_err(), "unknown generation was replayed");
    reopened
        .stop()
        .with_context(|| runtime_log(root, "unknown-reopened"))?;
    drop(reopened);
    let store = shenko_store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
    let attempt = store.subject_request_attempt(request_id)?.context("original attempt disappeared")?;
    ensure!(attempt.attempt_id == attempt_id && attempt.terminal.and_then(|terminal| terminal.result_ref) == Some(terminal_ref));
    ensure!(observer.requests.try_recv().is_err(), "Unknown shutdown request was resent during reopen");
    drop(store);
    recovery_stop_during_owned_teardown(root, port, &observer)
        .with_context(|| runtime_log(root, "recovery-stopped"))?;
    observer.finish()?;
    drop(observer);
    Ok(())
}

/// Extend the same process journey with failure first, real SIGINT second.
/// The Store transaction only delays physical E1 cleanup; it changes no fact.
fn recovery_stop_during_owned_teardown(root: &Path, port: u16, observer: &Observer) -> Result<()> {
    use std::os::fd::AsRawFd;
    let data_dir = root.join("recovery-installation");
    let label = "recovery-stopped";
    let mut runtime = start(root, &data_dir, port, None, observer.address, label)?;
    let identity = ready(&mut runtime, port)?;
    let agent: EntityId = identity["subject_id"].as_str().context("recovery subject identity")?.parse()?;
    let body = observer.requests.recv_timeout(Duration::from_secs(90))?;
    let request: Value = serde_json::from_slice(&body)?;
    ensure!(request["model"] == "dispatch-observer" && request["stream"] == true);
    let (request_id, attempt_id) = {
        let store = shenko_store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
        let (_, step) = store.load_subject(agent)?;
        let request_id = step.active_request_id.context("original recovery request")?;
        let attempt = store.subject_request_attempt(request_id)?.context("original recovery attempt")?;
        ensure!(attempt.terminal.is_none(), "held generation already had a terminal receipt");
        (request_id, attempt.attempt_id)
    };
    let mut events = Vec::new();
    let origin = Instant::now();
    let mut observed = |event: &str| -> Result<()> {
        events.push(serde_json::json!({"event":event,"elapsed_ms":origin.elapsed().as_millis()}));
        fs::write(root.join("recovery-stop-order.json"), serde_json::to_vec_pretty(&events)?)?;
        Ok(())
    };
    observed("original_inference_post_held")?;
    // Match the production Node→Store lock order. Once both are ours, no
    // control writer can be paused while still owning either SQLite lock.
    let authority = shenko_node::Node::open(&data_dir.join("node"))?.reserve_authority()?;
    let term_before = authority.snapshot().term;
    let mut writer = rusqlite::Connection::open(data_dir.join("shenko.db"))?;
    writer.busy_timeout(Duration::from_secs(5))?;
    let barrier = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    runtime.pause()?;
    drop(authority);
    let mut node = shenko_node::Node::open(&data_dir.join("node"))?;
    node.campaign()?;
    let term_after = node.current_term()?;
    ensure!(term_after > term_before, "real election did not change the old runtime's binding");
    drop(node);
    runtime.signal_group(libc::SIGCONT)?;
    observed("authority_changed_and_process_continued")?;
    // Cancellation reports its original request before synchronous error/E1
    // journaling. That journal is deliberately held here; the Tokio HTTP
    // connection driver need not produce physical EOF before it can progress.
    let request_identity = request_id.to_string();
    let cancellation_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let bytes = fs::read(root.join(format!("{label}.log")))?;
        let log = String::from_utf8_lossy(&bytes);
        ensure!(!log.contains("host shutdown signal observed"),
            "a host signal preceded the controlled authority failure");
        if log.lines().any(|line| line.contains("INFERENCE_OBSERVATION_STOPPED")
            && line.contains(&request_identity)) { break; }
        ensure!(runtime.0.try_wait()?.is_none(), "runtime exited before its failure cancellation was observed");
        ensure!(Instant::now() < cancellation_deadline,
            "original request cancellation was not observed before cleanup SIGINT");
        thread::sleep(Duration::from_millis(10));
    }
    observed("original_request_cancellation_observed_before_sigint")?;
    ensure!(runtime.0.try_wait()?.is_none(), "runtime abandoned its blocked E1 writer");
    runtime.signal_group(libc::SIGINT)?;
    observed("sigint_sent_during_owned_cleanup")?;
    let signal_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let log = fs::read(root.join(format!("{label}.log")))?;
        if String::from_utf8_lossy(&log).contains("host shutdown signal observed") { break; }
        ensure!(runtime.0.try_wait()?.is_none(), "runtime exited before observing the cleanup signal");
        ensure!(Instant::now() < signal_deadline, "host did not observe SIGINT while its writer was held");
        thread::sleep(Duration::from_millis(10));
    }
    observed("host_stop_latched_before_writer_release")?;
    let directory = fs::File::open(&data_dir)?;
    let locked = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ensure!(locked != 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock,
        "installation ownership was released while E1 was still blocked");
    barrier.rollback()?;
    observed("original_e1_writer_released")?;
    observer.closed_inference.recv_timeout(Duration::from_secs(5))
        .context("original HTTP transport did not end after its cleanup writer was released")?;
    observed("original_transport_end_confirmed")?;
    let status = runtime.wait_for(Duration::from_secs(30))?;
    ensure!(!status.success() && status.code().is_some(), "stop erased the original recovery failure: {status}");
    let log = fs::read_to_string(root.join(format!("{label}.log")))?;
    ensure!(log.contains("runtime authority requires reconstruction")
        && log.contains("the reserved Node authority or local writer binding changed"),
        "process did not retain its original typed authority failure");
    ensure!(!log.contains("reconstructing runtime from durable authority")
        && log.matches("Shenko starting").count() == 1,
        "a later incarnation started after the observed cleanup stop");
    ensure!(observer.requests.try_recv().is_err(), "cleanup stop admitted another inference POST");
    ensure!(unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "installation remained locked after all real process workers exited");
    let store = shenko_store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
    let (_, step) = store.load_subject(agent)?;
    ensure!(step.state == shenko_types::subject::SubjectStepState::AwaitingModel
        && step.active_request_id == Some(request_id), "failure/stop manufactured a completed decision");
    let attempt = store.subject_request_attempt(request_id)?.context("original recovery attempt disappeared")?;
    ensure!(attempt.attempt_id == attempt_id);
    let terminal = attempt.terminal.context("original cancelled transport has no terminal receipt")?;
    let result_ref = terminal.result_ref.context("original Unknown result seal")?;
    let result: shenko_broker::request::InferenceResult = serde_json::from_slice(&store.cas.read_object(&result_ref.digest)?)?;
    ensure!(result.request_id == request_id
        && result.termination.kind == shenko_broker::request::TerminationKind::Unknown,
        "the local cleanup signal relabelled the remote outcome");
    fs::write(root.join("recovery-stop-receipt.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "request_id":request_id,"attempt_id":attempt_id,"term_before":term_before,"term_after":term_after,
        "exit_code":status.code(),"termination":result.termination,"result_ref":result_ref,
        "next_incarnation_started":false,"original_owner_held_until_e1_release":true,
        "remote_stopped":false,"model_completion_tested":false,
    }))?)?;
    observed("original_error_and_unknown_preserved_without_reconstruction")?;
    Ok(())
}
