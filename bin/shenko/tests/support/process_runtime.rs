//! Bounded owned-process and real OpenSSH support shared by CLI journeys.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Each command owns a process group that receives timeout/failure signals.
/// Waiting for the owned child is bounded, and every observed join is retained
/// beside its output. A leader join is not proof of every descendant's exit.
pub struct OwnedProcess {
    child: Child,
    label: String,
    log: PathBuf,
    joined: bool,
}

impl OwnedProcess {
    pub fn spawn(command: &mut Command, logs: &Path, label: &str) -> Result<Self> {
        Self::spawn_with_input(command, logs, label, None)
    }

    pub fn spawn_with_input(
        command: &mut Command,
        logs: &Path,
        label: &str,
        input: Option<&Path>,
    ) -> Result<Self> {
        let input = match input {
            Some(path) => Stdio::from(fs::File::open(path)?),
            None => Stdio::null(),
        };
        let child = command
            .process_group(0)
            .stdin(input)
            .stdout(fs::File::create(logs.join(format!("{label}.stdout")))?)
            .stderr(fs::File::create(logs.join(format!("{label}.stderr")))?)
            .spawn()
            .with_context(|| format!("spawn {label}"))?;
        let process = Self {
            child,
            label: label.into(),
            log: logs.join("processes.jsonl"),
            joined: false,
        };
        process.record(json!({"event":"spawned"}));
        Ok(process)
    }

    fn record(&self, value: Value) {
        let record = json!({"label":self.label, "pid":self.child.id(), "observation":value});
        if let Err(error) = append_json(&self.log, &record) {
            eprintln!(
                "could not retain {} process observation: {error:#}",
                self.label
            );
        }
    }

    pub fn poll(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if let Some(status) = status
            && !self.joined
        {
            self.joined = true;
            self.record(
                json!({"event":"joined", "status":status.to_string(), "exit_code":status.code()}),
            );
        }
        Ok(status)
    }

    pub fn wait_for(&mut self, timeout: Duration) -> Result<Option<ExitStatus>> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.poll()? {
                return Ok(Some(status));
            }
            if started.elapsed() >= timeout {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn signal(&self, signal: i32) -> Result<()> {
        let group = i32::try_from(self.child.id())?;
        // SAFETY: this is the process group created for this still-owned child.
        if unsafe { libc::kill(-group, signal) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        self.record(json!({"event":"signal_requested", "signal":signal}));
        Ok(())
    }

    pub fn stop(&mut self, signal: i32, grace: Duration) -> Result<(ExitStatus, bool)> {
        if let Some(status) = self.poll()? {
            return Ok((status, false));
        }
        self.signal(signal)?;
        if let Some(status) = self.wait_for(grace)? {
            return Ok((status, false));
        }
        self.signal(libc::SIGKILL)?;
        let status = self
            .wait_for(Duration::from_secs(5))?
            .with_context(|| format!("{} was not joined after SIGKILL", self.label))?;
        Ok((status, true))
    }

    pub fn finish(&mut self, timeout: Duration) -> Result<ExitStatus> {
        if let Some(status) = self.wait_for(timeout)? {
            return Ok(status);
        }
        self.record(json!({"event":"deadline_exceeded", "timeout_secs":timeout.as_secs()}));
        let (status, _) = self.stop(libc::SIGKILL, Duration::ZERO)?;
        anyhow::bail!("{} exceeded {timeout:?}; joined with {status}", self.label)
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.joined
            && let Err(error) = self.stop(libc::SIGKILL, Duration::ZERO)
        {
            self.record(json!({"event":"cleanup_failed", "error":format!("{error:#}")}));
            if let Err(write_error) = fs::write(
                self.log.with_extension("cleanup-failed"),
                format!("{} pid {}: {error:#}\n", self.label, self.child.id()),
            ) {
                eprintln!("could not retain cleanup failure: {write_error}");
            }
            eprintln!("{} cleanup failed: {error:#}", self.label);
        }
    }
}

pub fn append_json(path: &Path, record: &Value) -> Result<()> {
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(log, "{}", serde_json::to_string(record)?)?;
    log.sync_all()?;
    Ok(())
}

pub fn copy_diagnostics(logs: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(logs)? {
        let entry = entry?;
        // Only this explicit public-output directory, never the installation,
        // input files, private keys or private owner-control material.
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub fn health(port: u16) -> Result<Value> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .context("health HTTP headers")?;
    ensure!(response.starts_with(b"HTTP/1.1 200 "), "health HTTP status");
    Ok(serde_json::from_slice(&response[split + 4..])?)
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn write_sshd_config(
    config: &Path,
    root: &Path,
    host_key: &Path,
    authorized_keys: &Path,
    port: u16,
) -> Result<()> {
    // Root-run container fixtures use PAM's account checks so a locked root
    // password does not disable independently authorized public keys.
    // SAFETY: geteuid only observes this process's effective identity.
    let use_pam = if unsafe { libc::geteuid() } == 0 {
        "yes"
    } else {
        "no"
    };
    fs::write(
        config,
        format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM {use_pam}\nStrictModes no\n\
         PermitRootLogin yes\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n",
            host_key.display(),
            root.join("pid").display(),
            authorized_keys.display(),
        ),
    )?;
    Ok(())
}
