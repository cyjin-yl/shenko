//! A real SSH client compiled into WASI preview1. Authentication, key
//! exchange, packet protection and SFTP are executed inside this module.
mod sftp;
mod transport;

use anyhow::{Context, Result, ensure};
use russh::client;
use russh::keys::{
    Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate,
};
use russh::{ChannelMsg, Disconnect};
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Keygen {
        private_key: PathBuf,
        public_key: PathBuf,
    },
    Exec {
        connection: Connection,
        command: String,
    },
    Get {
        connection: Connection,
        remote: String,
        local: PathBuf,
        max_bytes: u64,
    },
    Put {
        connection: Connection,
        local: PathBuf,
        remote: String,
        overwrite: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    host: String,
    port: u16,
    user: String,
    private_key: PathBuf,
    /// Exact previously obtained OpenSSH public key; never trust-on-first-use.
    host_public_key: String,
}

struct PinHostKey(PublicKey);

impl client::Handler for PinHostKey {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(
            matches!(key, PublicKeyOrCertificate::PublicKey { key: observed, .. }
            if observed.key_data() == self.0.key_data()),
        )
    }
}

fn create_new(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new {}", path.display()))?)
}

fn keygen(private_path: &Path, public_path: &Path) -> Result<()> {
    ensure!(
        private_path != public_path,
        "private and public output paths must differ"
    );
    ensure!(!public_path.try_exists()?, "public output already exists");
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let encoded = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
    let mut private = create_new(private_path)?;
    // Mount permissions are enforced by the existing home; private bytes are
    // never emitted on stdout or passed to an external signing process.
    private.write_all(encoded.as_bytes())?;
    private.sync_all()?;
    let mut public = create_new(public_path)?;
    writeln!(public, "{}", key.public_key().to_openssh()?)?;
    public.sync_all()?;
    println!(
        "{}",
        serde_json::json!({"generated": "ed25519", "fingerprint": key.public_key().fingerprint(russh::keys::HashAlg::Sha256).to_string()})
    );
    Ok(())
}

async fn connect(binding: Connection) -> Result<client::Handle<PinHostKey>> {
    let pin = PublicKey::from_openssh(&binding.host_public_key)?;
    let private = russh::keys::load_secret_key(&binding.private_key, None)?;
    ensure!(
        private.algorithm() == Algorithm::Ed25519,
        "this guest supports Ed25519 user keys"
    );
    let transport = transport::Transport::admitted(&binding.host, binding.port)?;
    let config = client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    };
    let mut session = client::connect_stream(Arc::new(config), transport, PinHostKey(pin)).await?;
    let result = session
        .authenticate_publickey(
            binding.user,
            PrivateKeyWithHashAlg::new(Arc::new(private), None),
        )
        .await?;
    ensure!(result.success(), "SSH public-key authentication refused");
    Ok(session)
}

async fn execute(request: Request) -> Result<u32> {
    let (connection, action) = match request {
        Request::Keygen {
            private_key,
            public_key,
        } => {
            keygen(&private_key, &public_key)?;
            return Ok(0);
        }
        Request::Exec {
            connection,
            command,
        } => (connection, Action::Exec(command)),
        Request::Get {
            connection,
            remote,
            local,
            max_bytes,
        } => (connection, Action::Get(remote, local, max_bytes)),
        Request::Put {
            connection,
            local,
            remote,
            overwrite,
        } => (connection, Action::Put(local, remote, overwrite)),
    };
    let session = connect(connection).await?;
    let code = match action {
        Action::Exec(command) => {
            let mut channel = session.channel_open_session().await?;
            channel.exec(true, command).await?;
            let mut code = None;
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => std::io::stdout().write_all(&data)?,
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        std::io::stderr().write_all(&data)?
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    ChannelMsg::Failure => anyhow::bail!("SSH exec request refused"),
                    ChannelMsg::ExitSignal { .. } => {
                        anyhow::bail!("remote command terminated by signal")
                    }
                    _ => {}
                }
            }
            code.context("SSH channel ended without an exit status; outcome unknown")?
        }
        Action::Get(remote, local, max_bytes) => {
            let channel = session.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            sftp::get(channel.into_stream(), &remote, &local, max_bytes).await?;
            0
        }
        Action::Put(local, remote, overwrite) => {
            let channel = session.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            sftp::put(channel.into_stream(), &local, &remote, overwrite).await?;
            0
        }
    };
    session
        .disconnect(Disconnect::ByApplication, "completed", "en")
        .await?;
    Ok(code)
}

enum Action {
    Exec(String),
    Get(String, PathBuf, u64),
    Put(PathBuf, String, bool),
}

fn main() {
    let result = (|| -> Result<u32> {
        let mut input = Vec::new();
        std::io::stdin().take(65_537).read_to_end(&mut input)?;
        ensure!(input.len() <= 65_536, "request exceeds 64 KiB");
        let request = serde_json::from_slice(&input)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?
            .block_on(execute(request))
    })();
    match result {
        Ok(code) => std::process::exit(i32::try_from(code).unwrap_or(255)),
        Err(error) => {
            eprintln!("ssh-wasi: {error:#}");
            std::process::exit(255);
        }
    }
}
