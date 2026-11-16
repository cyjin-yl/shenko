//! One real CLI bootstrap → tool install → named SSH WASI execution journey.
//! The original authentication, exec, SFTP and refusal assertions remain in
//! this journey. CI supplies the real WASI artifact and local OpenSSH server;
//! a missing prerequisite is a failure. This is CLI installation/execution
//! evidence, not native wasi.exec T1/E1/T2 or real remote deployment evidence.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

#[path = "support/process_runtime.rs"]
mod process_runtime;
#[path = "support/runtime_health.rs"]
mod runtime_health;
use process_runtime::{
    OwnedProcess, append_json, copy_diagnostics, file_sha256, write_sshd_config,
};
use runtime_health::health;

// A named execution cold-verifies its installed artifact and compiles it on
// the controlled engine. The run budget still includes compile/setup; the
// outer budget additionally bounds CLI startup, verification and cleanup.
const SSH_RUN_TIMEOUT_SECS: u64 = 120;
const SSH_CLI_TIMEOUT_SECS: u64 = 180;

struct Fixture {
    root: PathBuf,
    server: Option<OwnedProcess>,
    guest_no: Cell<usize>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut stopped = true;
        if let Some(mut server) = self.server.take()
            && let Err(error) = server.stop(libc::SIGTERM, Duration::from_secs(3))
        {
            stopped = false;
            eprintln!("OpenSSH fixture cleanup failed: {error:#}");
        }
        // Keep public protocol observations even if an assertion or an early
        // `?` fails. Never copy the fixture tree or either private key.
        if let Some(path) = std::env::var_os("SHENKO_SSH_WASI_EVIDENCE") {
            let evidence = PathBuf::from(path);
            if let Some(parent) = evidence.parent() {
                let _ = fs::create_dir_all(parent);
            }
            for (name, extension) in [
                ("sshd.stderr", "sshd.log"),
                ("guest-records.jsonl", "guest-records.jsonl"),
            ] {
                let source = self.logs().join(name);
                if source.is_file()
                    && let Err(error) = fs::copy(source, evidence.with_extension(extension))
                {
                    eprintln!("could not retain {name}: {error}");
                }
            }
            let diagnostics = evidence.with_extension("diagnostics");
            if let Err(error) = copy_diagnostics(&self.logs(), &diagnostics) {
                eprintln!("could not retain CLI diagnostics: {error:#}");
            }
        }
        // Only this fixture's temporary tree, including its private keys.
        if stopped && !self.logs().join("processes.cleanup-failed").exists() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

impl Fixture {
    fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    fn data(&self) -> PathBuf {
        self.root.join("installation")
    }

    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        // No model/account/channel/display/SSH-agent settings reach this
        // installation. Git is local-only and ignores ambient configuration.
        command
            .env_clear()
            .current_dir(&self.root)
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    }

    fn cli(&self) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_shenko"));
        command.arg("--data-dir").arg(self.data()).args([
            "--log",
            "info,shenko_wasm::runtime=debug,shenko::tool_exec=debug",
        ]);
        command
    }

    fn run(&self, command: &mut Command, label: &str, timeout: Duration) -> Result<ExitStatus> {
        OwnedProcess::spawn(command, &self.logs(), label)?.finish(timeout)
    }

    fn cli_json(&self, command: &mut Command, label: &str) -> Result<Value> {
        let status = self.run(command, label, Duration::from_secs(60))?;
        ensure!(
            status.success(),
            "{label} exited {status}: {}",
            fs::read_to_string(self.logs().join(format!("{label}.stderr")))?
        );
        Ok(serde_json::from_slice(&fs::read(
            self.logs().join(format!("{label}.stdout")),
        )?)?)
    }

    fn bootstrap(&self) -> Result<Value> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let mut command = self.cli();
        command.args(["start", "--port"]).arg(port.to_string());
        let mut process = OwnedProcess::spawn(&mut command, &self.logs(), "bootstrap")?;
        let started = Instant::now();
        // Same real /health and SIGINT lifecycle as the existing CLI consumer
        // in cognition_self_learning/harness.rs; no Store initialization here.
        let health = loop {
            if let Some(status) = process.poll()? {
                anyhow::bail!(
                    "CLI exited before readiness ({status}): {}",
                    fs::read_to_string(self.logs().join("bootstrap.stderr"))?
                );
            }
            if let Ok(value) = health(port)
                && value["status"] == "ok"
            {
                break value;
            }
            ensure!(
                started.elapsed() < Duration::from_secs(90),
                "CLI bootstrap readiness timed out"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let (status, forced) = process.stop(libc::SIGINT, Duration::from_secs(20))?;
        ensure!(
            !forced && status.success(),
            "CLI bootstrap did not stop normally: {status}, forced={forced}"
        );
        Ok(json!({"health":health, "port":port, "exit_code":status.code(), "forced":forced}))
    }

    fn install(&self, module: &[u8]) -> Result<Value> {
        let artifact = self.root.join("install-input.wasm");
        fs::write(&artifact, module)?;
        let sha256 = format!("{:x}", Sha256::digest(module));
        let digest = shenko_types::object::Digest::blake3(module).to_string();
        let mut command = self.cli();
        command
            .args([
                "tool",
                "install",
                "--name",
                "ssh",
                "--version",
                "0.1.0",
                "--abi",
                "wasi_preview1",
                "--license",
                "Apache-2.0",
                "--source-uri",
                "https://github.com/cyjin-yl/shenko",
                "--source-release",
                "0.1.0",
                "--provenance",
                "source_build:crates/shenko-wasm/ports/ssh-wasi/build.py",
                "--requires-fs",
                "/home:rw",
                "--requires-host",
                "clock",
                "--requires-host",
                "random",
            ])
            .arg("--file")
            .arg(&artifact)
            .arg("--source-sha256")
            .arg(&sha256)
            .arg("--expected-digest")
            .arg(&digest);
        let installed = self.cli_json(&mut command, "install")?;
        ensure!(
            module.len() > shenko_node::MAX_ARTIFACT_BYTES,
            "this real SSH artifact must exercise the existing out-of-line install path"
        );
        ensure!(
            installed["installed"]["name"] == "ssh"
                && installed["installed"]["unit_id"] == "tool.ssh"
                && installed["installed"]["version"] == "0.1.0"
                && installed["installed"]["bytes"] == module.len()
                && installed["installed"]["digest"] == digest
                && installed["installed"]["sha256"] == sha256
                && installed["installed"]["abi"] == "wasi_preview1"
                && installed["installed"]["storage"] == "content_store"
                && installed["installed"]["recovery_bound"] == true
                && installed["environment"]["pinned"] == true
                && installed["environment"]["activated"] == true,
            "CLI did not install and activate the exact chunked artifact: {installed}"
        );
        let mut list = self.cli();
        list.args(["tool", "list"]);
        let inventory = self.cli_json(&mut list, "inventory")?;
        let selected = inventory["tools"]
            .as_array()
            .context("installed tool inventory")?
            .iter()
            .find(|tool| tool["name"] == "ssh" && tool["selected"] == true)
            .context("installed SSH was not selected by name")?;
        ensure!(
            selected["unit_id"] == installed["installed"]["unit_id"]
                && selected["version"] == installed["installed"]["version"]
                && selected["abi"] == installed["installed"]["abi"]
                && selected["current"] == true
                && selected["enabled"] == true
                && selected["artifact"]["digest"] == digest
                && selected["artifact"]["sha256"] == sha256
                && selected["artifact"]["bytes"] == module.len()
                && selected["artifact"]["kind"] == "content_store"
                && selected["artifact"]["chunk_manifest"].is_object(),
            "selected SSH artifact differs from the exact installed module: {selected}"
        );
        // The unit digest identifies the small pointer document for an
        // out-of-line artifact, not the WASM module. Bind both identities to
        // real stored bytes and to the environment that name lookup uses.
        let package_path = Path::new(
            selected["materialized_path"]
                .as_str()
                .context("selected SSH unit has no materialized pointer")?,
        );
        ensure!(package_path.starts_with(self.data().join("tools")));
        let package_bytes = fs::read(package_path)?;
        let package_digest = shenko_types::object::Digest::blake3(&package_bytes).to_string();
        let artifact_ref = serde_json::from_value(selected["artifact"].clone())?;
        shenko_wasm::artifact::ArtifactPointer::parse(&package_bytes, &artifact_ref)?;
        let activation = &installed["environment"]["activation"];
        let generation = &activation["generation"];
        let generation_number = generation["generation"]
            .as_u64()
            .context("installed SSH environment has no generation")?;
        let generation_digest = generation["digest"]
            .as_str()
            .context("installed SSH environment has no digest")?;
        let selector_revision = activation["selector"]["revision"]
            .as_u64()
            .context("installed SSH environment has no selector revision")?;
        let member = generation["members"]
            .as_array()
            .context("installed SSH environment members")?
            .iter()
            .find(|member| member["unit_id"] == selected["unit_id"])
            .context("installed SSH unit is absent from its active environment")?;
        let listed_generation = inventory["environment"]["generations"]
            .as_array()
            .context("listed environment generations")?
            .iter()
            .find(|item| item["generation"] == generation_number)
            .context("activated SSH environment is absent from the inventory")?;
        ensure!(
            selected["digest"] == package_digest
                && member["artifact_digest"] == package_digest
                && member["revision"] == selected["version"]
                && member["declarations"] == selected["capabilities"]
                && member["role"] == "root"
                && selected["environment"]["role"] == member["role"]
                && selected["environment"]["generation"] == generation_number
                && selected["environment"]["digest"] == generation_digest
                && selected["environment"]["selector_revision"] == selector_revision
                && activation["activated"] == true
                && activation["selector"]["generation"] == generation_number
                && activation["selector"]["generation_digest"] == generation_digest
                && installed["environment"]["prepared"]["generation"] == *generation
                && inventory["environment"]["pinned"] == true
                && inventory["environment"]["selector"] == activation["selector"]
                && listed_generation == generation,
            "selected SSH unit or active environment differs: {selected}; {activation}"
        );
        // Execution has no path to the fixture's original install input. Each
        // CLI process resolves the selected unit and reassembles verified CAS
        // chunks through the product's actual tool exec path.
        fs::remove_file(&artifact)?;
        ensure!(!artifact.exists(), "original install input still exists");
        Ok(json!({"install":installed, "inventory":inventory, "install_input_removed":true}))
    }

    fn guest(&self, port: Option<u16>, request: Value) -> Result<Value> {
        let number = self.guest_no.get();
        self.guest_no.set(number + 1);
        let label = format!("guest-{number}");
        let input = self.root.join(format!("request-{number}.json"));
        let stdout = self.logs().join(format!("{label}.guest-stdout"));
        let stderr = self.logs().join(format!("{label}.guest-stderr"));
        fs::write(&input, serde_json::to_vec(&request)?)?;
        let mut command = self.cli();
        command
            .args(["tool", "exec", "--tool", "ssh", "--timeout-secs"])
            .arg(SSH_RUN_TIMEOUT_SECS.to_string())
            .arg("--stdin-file")
            .arg(&input)
            .arg("--stdout-file")
            .arg(&stdout)
            .arg("--stderr-file")
            .arg(&stderr);
        if let Some(port) = port {
            command.arg("--network").arg(format!("127.0.0.1:{port}"));
        }
        append_json(
            &self.logs().join("guest-records.jsonl"),
            &json!({
                "event":"command_started", "label":label, "action":request["action"],
                "run_timeout_secs":SSH_RUN_TIMEOUT_SECS, "cli_timeout_secs":SSH_CLI_TIMEOUT_SECS,
            }),
        )?;
        let cli_started = Instant::now();
        let status = self.run(
            &mut command,
            &label,
            Duration::from_secs(SSH_CLI_TIMEOUT_SECS),
        );
        let record = json!({
            "event":"command_observed", "label":label, "action":request["action"],
            "run_timeout_secs":SSH_RUN_TIMEOUT_SECS, "cli_timeout_secs":SSH_CLI_TIMEOUT_SECS,
            "cli_elapsed_secs":cli_started.elapsed().as_secs_f64(),
            "exit_code":status.as_ref().ok().and_then(ExitStatus::code),
            "outcome":match &status { Ok(status) => status.to_string(), Err(error) => format!("{error:#}") },
            "stdout":read_output(&stdout, 256 * 1024)?, "stderr":read_output(&stderr, 64 * 1024)?,
            "cli_stderr":read_output(&self.logs().join(format!("{label}.stderr")), 256 * 1024)?,
        });
        append_json(&self.logs().join("guest-records.jsonl"), &record)?;
        let status = status?;
        ensure!(
            status.code().is_some(),
            "guest CLI did not exit normally: {record}"
        );
        Ok(record)
    }
}

fn read_output(path: &Path, maximum: usize) -> Result<String> {
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(file) => {
            file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        bytes.len() <= maximum,
        "fixture output exceeds {maximum} bytes: {}",
        path.display()
    );
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn ssh_wasi_auth_exec_sftp_and_refusal_journey() -> Result<()> {
    let artifact = std::env::var_os("SHENKO_SSH_WASI_ARTIFACT")
        .context("SHENKO_SSH_WASI_ARTIFACT is required")?;
    let module = fs::read(&artifact).with_context(|| {
        format!(
            "cannot read SSH WASI artifact {}",
            Path::new(&artifact).display()
        )
    })?;
    let root = std::env::temp_dir().join(format!(
        "shenko-ssh-client-{}",
        shenko_types::ids::EntityId::new()
    ));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let mut fixture = Fixture {
        root,
        server: None,
        guest_no: Cell::new(0),
    };
    fs::create_dir(fixture.logs())?;
    let bootstrap = fixture.bootstrap()?;
    let original_binding =
        shenko_store::Store::inspect_installation_binding(&fixture.data().join("shenko.db"))?;
    let subject = original_binding
        .subject
        .as_ref()
        .context("CLI bootstrap subject")?;
    let installation = fixture.install(&module)?;
    let home = fixture.data().join("home");
    ensure!(home.is_dir(), "real CLI did not create its managed home");
    let mut records = vec![];
    let generated = fixture.guest(
        None,
        json!({
            "action":"keygen", "private_key":"/home/id_ed25519", "public_key":"/home/id_ed25519.pub"
        }),
    )?;
    assert_eq!(generated["exit_code"], 0, "{generated}");
    assert_eq!(
        fs::metadata(home.join("id_ed25519"))?.permissions().mode() & 0o077,
        0,
        "a guest-generated private key must not inherit group/world access"
    );
    let user_public_key = fs::read_to_string(home.join("id_ed25519.pub"))?;
    assert!(user_public_key.starts_with("ssh-ed25519 "));
    records.push(generated);

    let host_key = fixture.root.join("host_key");
    let mut keygen = fixture.command("ssh-keygen");
    keygen
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&host_key);
    ensure!(
        fixture
            .run(&mut keygen, "host-keygen", Duration::from_secs(10))?
            .success(),
        "fixture host key generation failed"
    );
    let host_public_key = fs::read_to_string(host_key.with_extension("pub"))?;
    fs::write(fixture.root.join("authorized_keys"), &user_public_key)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let config = fixture.root.join("sshd_config");
    write_sshd_config(
        &config,
        &fixture.root,
        &host_key,
        &fixture.root.join("authorized_keys"),
        port,
    )?;
    let server_log = fixture.logs().join("sshd.stderr");
    let sshd = std::env::var_os("SHENKO_SSHD").unwrap_or_else(|| "/usr/sbin/sshd".into());
    let mut server = fixture.command(sshd);
    server.args(["-D", "-e", "-f"]).arg(config);
    fixture.server = Some(OwnedProcess::spawn(&mut server, &fixture.logs(), "sshd")?);
    let mut listening = false;
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            listening = true;
            break;
        }
        if let Some(status) = fixture
            .server
            .as_mut()
            .context("fixture server missing")?
            .poll()?
        {
            anyhow::bail!(
                "OpenSSH fixture exited {status}: {}",
                fs::read_to_string(&server_log)?
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        listening,
        "OpenSSH fixture did not listen: {}",
        fs::read_to_string(&server_log)?
    );
    let mut user = fixture.command("id");
    user.arg("-un");
    ensure!(
        fixture
            .run(&mut user, "account", Duration::from_secs(5))?
            .success(),
        "cannot identify fixture account"
    );
    let user = fs::read_to_string(fixture.logs().join("account.stdout"))?;
    let binding = json!({
        "host":"127.0.0.1", "port":port,
        "user":user.trim(),
        "private_key":"/home/id_ed25519", "host_public_key":host_public_key.trim(),
    });
    let executed = fixture.guest(
        Some(port),
        json!({
            "action":"exec", "connection":binding,
            "command":"printf 'ssh-wasi-exec\\n'; printf 'ssh-wasi-stderr\\n' >&2; exit 7"
        }),
    )?;
    assert_eq!(
        executed["exit_code"],
        7,
        "{executed}; server={}",
        fs::read_to_string(&server_log)?
    );
    assert_eq!(executed["stdout"], "ssh-wasi-exec\n");
    assert_eq!(executed["stderr"], "ssh-wasi-stderr\n");
    records.push(executed);

    let payload: Vec<u8> = (0..100_003).map(|n| (n % 251) as u8).collect();
    fs::write(home.join("upload.bin"), &payload)?;
    let remote = fixture.root.join("remote.bin");
    let uploaded = fixture.guest(
        Some(port),
        json!({
            "action":"put", "connection":binding, "local":"/home/upload.bin",
            "remote":remote, "overwrite":false
        }),
    )?;
    assert_eq!(uploaded["exit_code"], 0, "{uploaded}");
    assert_eq!(fs::read(&remote)?, payload, "SFTP remote bytes differ");
    records.push(uploaded);
    let downloaded = fixture.guest(
        Some(port),
        json!({
            "action":"get", "connection":binding, "remote":remote,
            "local":"/home/download.bin", "max_bytes":payload.len()
        }),
    )?;
    assert_eq!(downloaded["exit_code"], 0, "{downloaded}");
    assert_eq!(
        fs::read(home.join("download.bin"))?,
        payload,
        "SFTP returned bytes differ"
    );
    records.push(downloaded);

    // The authenticated client must refuse an independently valid but wrong
    // host key before the requested remote command can create this sentinel.
    let sentinel = fixture.root.join("must-not-exist");
    let mut wrong_pin = binding.clone();
    wrong_pin["host_public_key"] = json!(user_public_key.trim());
    let refused = fixture.guest(
        Some(port),
        json!({
            "action":"exec", "connection":wrong_pin,
            "command":format!("printf bad > '{}'", sentinel.to_string_lossy().replace('\'', "'\\''"))
        }),
    )?;
    assert_eq!(refused["exit_code"], 255, "{refused}");
    assert!(
        !sentinel.exists(),
        "wrong host key still executed a command"
    );
    records.push(refused);
    let no_network = fixture.guest(
        None,
        json!({
            "action":"exec", "connection":binding, "command":"printf must-not-run"
        }),
    )?;
    assert_eq!(no_network["exit_code"], 255, "{no_network}");
    assert!(
        no_network["stderr"]
            .as_str()
            .context("missing refusal stderr")?
            .contains("no host-admitted TCP stream")
    );
    records.push(no_network);

    let final_binding =
        shenko_store::Store::inspect_installation_binding(&fixture.data().join("shenko.db"))?;
    ensure!(
        final_binding == original_binding,
        "CLI tool execution changed the installation identity"
    );
    let (server_status, server_forced) = fixture
        .server
        .as_mut()
        .context("fixture server missing")?
        .stop(libc::SIGTERM, Duration::from_secs(3))?;
    ensure!(
        !server_forced,
        "OpenSSH server required a forced stop: {server_status}"
    );
    let evidence = json!({
        "kind":"real_ssh_wasi_openssh_journey", "artifact_sha256":format!("{:x}", Sha256::digest(&module)),
        "payload_sha256":format!("{:x}", Sha256::digest(&payload)), "payload_bytes":payload.len(),
        "client":"installed WASI module through real CLI tool exec", "server":"local OpenSSH", "records":records,
        "bootstrap":bootstrap, "installation":installation,
        "cli_binary_sha256":file_sha256(Path::new(env!("CARGO_BIN_EXE_shenko")))?,
        "installation_identity":{"ledger_id":original_binding.ledger_id, "agent_id":subject.agent_id,
            "branch_id":subject.branch_id, "origin_proof":subject.origin_proof},
        "same_installation_after_execution":true,
        "server_stop":{"status":server_status.to_string(), "forced":server_forced},
        "server_log":fs::read_to_string(&server_log)?,
        "scope":"real CLI bootstrap, install and selected-tool execution; keygen, pinned public-key authentication, exec status/streams, SFTP put/get, key and grant refusal; no native wasi.exec T1/E1/T2, remote deployment or long-run claim"
    });
    if let Some(path) = std::env::var_os("SHENKO_SSH_WASI_EVIDENCE") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(&evidence)?)?;
    }
    eprintln!("{}", serde_json::to_string(&evidence)?);
    Ok(())
}
