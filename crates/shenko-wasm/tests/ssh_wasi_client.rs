//! Real SSH authentication, exec and SFTP. This requires the separately built
//! WASI artifact and an installed OpenSSH server; the client is never a host
//! executable. CI builds the artifact and invokes this ignored journey
//! explicitly, so a missing artifact cannot become a passing smoke result.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shenko_wasm::{
    MemoryInputPipe, MemoryOutputPipe, MountMode, MountSpec, NetworkAdmission, WasiCommand,
    WasiRunOutcome, WasmRuntime,
};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture {
    root: PathBuf,
    server: Option<Child>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
        // Keep public protocol observations even if an assertion or an early
        // `?` fails. Never copy the fixture tree or either private key.
        if let Some(path) = std::env::var_os("SHENKO_SSH_WASI_EVIDENCE") {
            let evidence = PathBuf::from(path);
            if let Some(parent) = evidence.parent() {
                let _ = fs::create_dir_all(parent);
            }
            for name in ["sshd.log", "guest-records.jsonl"] {
                let source = self.root.join(name);
                if source.is_file()
                    && let Err(error) = fs::copy(source, evidence.with_extension(name))
                {
                    eprintln!("could not retain {name}: {error}");
                }
            }
        }
        // Only this fixture's temporary tree, including its private keys.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn guest(module: &[u8], home: &Path, port: Option<u16>, request: Value) -> Result<Value> {
    let mut runtime = WasmRuntime::with_mounts(vec![MountSpec::new(
        home.to_path_buf(),
        "/home",
        MountMode::ReadWrite,
        true,
        false,
    )])?;
    let stdout = MemoryOutputPipe::new(256 * 1024);
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let mut command = WasiCommand::new(
        MemoryInputPipe::new(serde_json::to_vec(&request)?),
        stdout.clone(),
        stderr.clone(),
    );
    command.arguments = vec!["ssh".into()];
    command.network = port
        .map(|port| NetworkAdmission::for_installed_target("127.0.0.1", port))
        .transpose()?;
    command.control.deadline = Some(Duration::from_secs(30));
    let run = runtime.run_wasi_module(module, command);
    let record = json!({
        "action": request["action"],
        "outcome": run.as_ref().map(|outcome| format!("{outcome:?}")).map_err(|error| format!("{error:#}")),
        "stdout": String::from_utf8_lossy(&stdout.contents()),
        "stderr": String::from_utf8_lossy(&stderr.contents()),
    });
    use std::io::Write;
    let mut log = fs::OpenOptions::new().create(true).append(true).open(
        home.parent()
            .context("fixture home has no parent")?
            .join("guest-records.jsonl"),
    )?;
    writeln!(log, "{}", serde_json::to_string(&record)?)?;
    log.sync_all()?;
    let outcome = run?;
    let code = match outcome {
        WasiRunOutcome::Exited(code) => code,
        other => anyhow::bail!(
            "guest did not exit: {other:?}; stderr={}",
            String::from_utf8_lossy(&stderr.contents())
        ),
    };
    Ok(json!({
        "action": request["action"], "exit_code": code,
        "stdout": String::from_utf8_lossy(&stdout.contents()),
        "stderr": String::from_utf8_lossy(&stderr.contents()),
    }))
}

#[test]
#[ignore = "build ssh-wasi and set SHENKO_SSH_WASI_ARTIFACT; requires sshd and ssh-keygen"]
fn ssh_wasi_auth_exec_sftp_and_refusal_journey() -> Result<()> {
    let artifact = std::env::var_os("SHENKO_SSH_WASI_ARTIFACT")
        .context("SHENKO_SSH_WASI_ARTIFACT is required")?;
    let module = fs::read(&artifact)?;
    let root = std::env::temp_dir().join(format!(
        "shenko-ssh-client-{}",
        shenko_types::ids::EntityId::new()
    ));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let mut fixture = Fixture { root, server: None };
    let home = fixture.root.join("home");
    fs::create_dir(&home)?;
    let mut records = vec![];
    let generated = guest(
        &module,
        &home,
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
    ensure!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&host_key)
            .status()?
            .success(),
        "fixture host key generation failed"
    );
    let host_public_key = fs::read_to_string(host_key.with_extension("pub"))?;
    fs::write(fixture.root.join("authorized_keys"), &user_public_key)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let config = fixture.root.join("sshd_config");
    // Root-run container fixtures use PAM's account checks so a locked root
    // password does not disable independently authorized public keys. The
    // unprivileged fixture needs no privileged PAM session setup.
    // SAFETY: geteuid only observes this process's effective identity.
    let use_pam = if unsafe { libc::geteuid() } == 0 {
        "yes"
    } else {
        "no"
    };
    fs::write(
        &config,
        format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM {use_pam}\nStrictModes no\n\
         PermitRootLogin yes\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n",
            host_key.display(),
            fixture.root.join("pid").display(),
            fixture.root.join("authorized_keys").display(),
        ),
    )?;
    let server_log = fixture.root.join("sshd.log");
    let log = fs::File::create(&server_log)?;
    let sshd = std::env::var_os("SHENKO_SSHD").unwrap_or_else(|| "/usr/sbin/sshd".into());
    fixture.server = Some(
        Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(config)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?,
    );
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
            .try_wait()?
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
    let user = Command::new("id").arg("-un").output()?;
    ensure!(user.status.success(), "cannot identify fixture account");
    let binding = json!({
        "host":"127.0.0.1", "port":port,
        "user":String::from_utf8(user.stdout)?.trim(),
        "private_key":"/home/id_ed25519", "host_public_key":host_public_key.trim(),
    });
    let executed = guest(
        &module,
        &home,
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
    let uploaded = guest(
        &module,
        &home,
        Some(port),
        json!({
            "action":"put", "connection":binding, "local":"/home/upload.bin",
            "remote":remote, "overwrite":false
        }),
    )?;
    assert_eq!(uploaded["exit_code"], 0, "{uploaded}");
    assert_eq!(fs::read(&remote)?, payload, "SFTP remote bytes differ");
    records.push(uploaded);
    let downloaded = guest(
        &module,
        &home,
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
    let refused = guest(
        &module,
        &home,
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
    let no_network = guest(
        &module,
        &home,
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

    let evidence = json!({
        "kind":"real_ssh_wasi_openssh_journey", "artifact_sha256":format!("{:x}", Sha256::digest(&module)),
        "payload_sha256":format!("{:x}", Sha256::digest(&payload)), "payload_bytes":payload.len(),
        "client":"WASI module", "server":"local OpenSSH", "records":records,
        "server_log":fs::read_to_string(&server_log)?,
        "scope":"keygen, pinned public-key authentication, exec status/streams, SFTP put/get, key and grant refusal; no long-run claim"
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
