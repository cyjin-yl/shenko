//! Shenko's single-command native SSH transport, not another generic SSH SDK.
//!
//! The trusted caller supplies a target from registered CAS trust and an owned,
//! already-authorized stream. This module never creates sockets/processes,
//! reads environment/config/key files, or discovers a signing identity. It
//! does not grant policy/resource admission or install a task handler. T1,
//! owner/generation fences and original-operation recovery remain caller duties.
mod stream;
mod trust;
pub use trust::NativeSshTarget;

use russh::{Channel, ChannelId, ChannelMsg, ChannelOpenFailure, client, keys::{
    HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate,
}};
use serde::Serialize;
use std::{borrow::Cow, sync::{Arc, OnceLock}, time::Duration};
use tokio::{io::{AsyncRead, AsyncWrite}, time::{Instant, timeout, timeout_at}};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NativeSshError {
    #[error("invalid native SSH binding")] InvalidBinding,
    #[error("sealed public trust unavailable")] TrustUnavailable,
    #[error("unsupported sealed public trust syntax")] TrustUnsupported,
    #[error("invalid native SSH limits")] InvalidLimits,
    #[error("native SSH connection or host verification failed")] Connect,
    #[error("native SSH authentication failed")] Authentication,
    #[error("native SSH deadline elapsed")] Deadline,
}

/// Not serializable: a connected authenticated protocol session is not durable
/// authority, a credential handle or a replayable task descriptor.
pub struct NativeSshConnection {
    handle: client::Handle<StrictPeer>,
    close: stream::CloseGuard,
    account: String,
    binding_digest: shenko_types::object::Digest,
    auth_deadline: Instant,
}
pub struct NativeSshSession(NativeSshConnection);

impl NativeSshConnection {
    pub async fn connect_stream<R>(target: NativeSshTarget, io: R, deadline: Duration)
        -> Result<Self, NativeSshError>
    where R: AsyncRead + AsyncWrite + Unpin + Send + 'static {
        let auth_deadline = Instant::now().checked_add(deadline)
            .filter(|_| !deadline.is_zero()).ok_or(NativeSshError::InvalidLimits)?;
        let account = target.account.clone();
        let binding_digest = target.binding_digest.clone();
        // Upstream still advertises legacy ssh-rsa after its SHA-2 choices.
        // Pins bind the key, not the security of its negotiated signature.
        let mut preferred = russh::Preferred::default();
        preferred.key = Cow::Owned(preferred.key.iter()
            .filter(|algorithm| !matches!(algorithm, russh::keys::Algorithm::Rsa { hash: None }))
            .cloned().collect());
        let config = Arc::new(client::Config {
            preferred,
            window_size: 64 * 1024, maximum_packet_size: 32 * 1024,
            channel_buffer_size: 4, inactivity_timeout: Some(deadline),
            keepalive_interval: None, ..Default::default()
        });
        // Guard exists before connect_stream can spawn its internal KEX task.
        // Dropping/timing out this future wakes and closes the wrapped stream.
        let (io, close) = stream::owned(io);
        let initial_host_algorithm = Arc::new(OnceLock::new());
        let peer = StrictPeer { target, initial_host_algorithm: initial_host_algorithm.clone() };
        let handle = timeout_at(auth_deadline, client::connect_stream(config, io, peer))
            .await.map_err(|_| NativeSshError::Deadline)?
            .map_err(|_| NativeSshError::Connect)?;
        // A raw RSA public-key blob is not its negotiated signature algorithm.
        // Check the initial KEX's actual signature before user authentication;
        // this retained initial binding does not attest later rekey signatures.
        let binding = handle.agent_session_binding().ok_or(NativeSshError::Connect)?;
        let signature = russh::keys::ssh_key::Signature::try_from(binding.signature())
            .map_err(|_| NativeSshError::Connect)?;
        if initial_host_algorithm.get() != Some(&signature.algorithm()) {
            return Err(NativeSshError::Connect);
        }
        Ok(Self { handle, close, account, binding_digest, auth_deadline })
    }

    /// Public initial-KEX context for an explicitly selected agent's
    /// bind_session call. No automatic agent discovery/forwarding is performed.
    pub fn agent_session_binding(&self) -> Option<&russh::keys::agent::AgentSessionBinding> {
        self.handle.agent_session_binding()
    }

    /// The installer resolves/decrypts the exact selected key locally. No
    /// private bytes or passphrase is added to logs, policy scope or receipts.
    pub async fn authenticate_private_key(mut self, key: PrivateKeyWithHashAlg)
        -> Result<NativeSshSession, NativeSshError>
    {
        if key.algorithm().is_rsa() && key.hash_alg().is_none() {
            return Err(NativeSshError::Authentication); // no RSA/SHA-1 fallback
        }
        let result = timeout_at(self.auth_deadline,
            self.handle.authenticate_publickey(self.account.clone(), key)).await
            .map_err(|_| NativeSshError::Deadline)?
            .map_err(|_| NativeSshError::Authentication)?;
        if !result.success() { return Err(NativeSshError::Authentication); }
        Ok(NativeSshSession(self))
    }

    /// One exact public identity and signer; no enumeration/fallback. An agent
    /// or hardware signer can implement russh's existing Signer directly.
    pub async fn authenticate_signer<S>(mut self, key: PublicKey, hash: Option<HashAlg>, signer: &mut S)
        -> Result<NativeSshSession, NativeSshError>
    where S: russh::Signer + Send, S::Error: Send {
        if key.algorithm().is_rsa() && hash.is_none() { return Err(NativeSshError::Authentication); }
        let result = timeout_at(self.auth_deadline,
            self.handle.authenticate_publickey_with(self.account.clone(), key, hash, signer)).await
            .map_err(|_| NativeSshError::Deadline)?
            .map_err(|_| NativeSshError::Authentication)?;
        if !result.success() { return Err(NativeSshError::Authentication); }
        Ok(NativeSshSession(self))
    }
}

#[derive(Clone, Copy)]
pub struct NativeCommandLimits {
    pub deadline: Duration,
    pub shutdown_grace: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeExit { Status { code: u32 }, Signal { name: String, core_dumped: bool } }
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeInterruption { Deadline, Transport, Rejected, Protocol, OutputLimit, MissingExit }

/// Transport observation, not a proof of application postconditions or grant.
/// Remote exit255 is a real status here; legacy ssh-process255 is not retyped.
#[derive(Debug, Serialize)]
pub struct NativeCommandObservation {
    pub schema: &'static str,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: Option<NativeExit>,
    pub exec_request_queued: bool,
    pub exec_accepted: bool,
    pub channel_closed: bool,
    pub interruption: Option<NativeInterruption>,
    pub session_end_observed: bool,
}

impl NativeSshSession {
    /// Bind a task to the exact trusted installation facts used for KEX and
    /// account authentication. The caller still owns endpoint stream acquisition
    /// and signer authorization; a matching digest does not create either.
    pub(crate) fn matches_binding(&self, binding: &crate::SshBinding) -> Result<bool, NativeSshError> {
        Ok(self.0.binding_digest == NativeSshTarget::binding_digest(binding)?)
    }

    /// Consume a single-use session. Cancellation closes its owned stream, but
    /// cannot prove the remote action stopped. Persist intent before calling;
    /// any lost/unknown result must query the original operation, never resend.
    pub async fn execute(mut self, command: &[u8], input: &[u8], limits: NativeCommandLimits)
        -> Result<NativeCommandObservation, NativeSshError>
    {
        if command.is_empty() || command.len() > 64 * 1024 || command.contains(&0)
            || input.len() > 1024 * 1024 || limits.stdout_bytes > 2 * 1024 * 1024
            || limits.stderr_bytes > 2 * 1024 * 1024 || limits.deadline.is_zero()
            || limits.shutdown_grace.is_zero() || limits.shutdown_grace > Duration::from_secs(2)
        { return Err(NativeSshError::InvalidLimits); }
        let deadline = Instant::now().checked_add(limits.deadline).ok_or(NativeSshError::InvalidLimits)?;
        let mut observation = NativeCommandObservation {
            schema: "ssh.native-transport.v1", stdout: vec![], stderr: vec![], exit: None,
            exec_request_queued: false, exec_accepted: false, channel_closed: false,
            interruption: None, session_end_observed: false,
        };
        match timeout_at(deadline, exchange(&self.0.handle, command, input, limits, &mut observation)).await {
            Err(_) => observation.interruption = Some(NativeInterruption::Deadline),
            Ok(Err(reason)) => observation.interruption = Some(reason),
            Ok(Ok(())) => {}
        }
        // Do not rely on Handle::drop. Close the owned stream and explicitly
        // observe the session's end within a separate finite cleanup allowance.
        self.0.close.close();
        observation.session_end_observed = timeout(limits.shutdown_grace, &mut self.0.handle).await.is_ok();
        Ok(observation)
    }
}

async fn exchange(handle: &client::Handle<StrictPeer>, command: &[u8], input: &[u8],
    limits: NativeCommandLimits, result: &mut NativeCommandObservation) -> Result<(), NativeInterruption>
{
    let channel = handle.channel_open_session().await.map_err(|_| NativeInterruption::Transport)?;
    channel.exec(true, command.to_vec()).await.map_err(|_| NativeInterruption::Transport)?;
    result.exec_request_queued = true;
    let (mut read, write) = channel.split();
    let (confirmed, confirmation) = tokio::sync::oneshot::channel();
    let send = async {
        confirmation.await.map_err(|_| NativeInterruption::Rejected)?;
        write.data(input).await.map_err(|_| NativeInterruption::Transport)?;
        write.eof().await.map_err(|_| NativeInterruption::Transport)
    };
    let receive = async {
        let mut confirmed = Some(confirmed);
        let mut overflow = false;
        while let Some(message) = read.wait().await {
            match message {
                ChannelMsg::Success if !result.exec_accepted => {
                    result.exec_accepted = true;
                    if let Some(ack) = confirmed.take() { let _ = ack.send(()); }
                }
                ChannelMsg::Failure => return Err(NativeInterruption::Rejected),
                ChannelMsg::Data { data } => overflow |= append(&mut result.stdout, &data, limits.stdout_bytes),
                ChannelMsg::ExtendedData { data, ext: 1 } => overflow |= append(&mut result.stderr, &data, limits.stderr_bytes),
                ChannelMsg::ExtendedData { .. } => return Err(NativeInterruption::Protocol),
                ChannelMsg::ExitStatus { exit_status } => {
                    if result.exit.is_some() { return Err(NativeInterruption::Protocol); }
                    result.exit = Some(NativeExit::Status { code: exit_status });
                }
                ChannelMsg::ExitSignal { signal_name, core_dumped, .. } => {
                    if result.exit.is_some() { return Err(NativeInterruption::Protocol); }
                    result.exit = Some(NativeExit::Signal { name: format!("{signal_name:?}"), core_dumped });
                }
                ChannelMsg::Eof => {} // continue draining; EOF alone is not an exit status
                ChannelMsg::Close => { result.channel_closed = true; break; }
                _ => {}
            }
        }
        if !result.channel_closed { return Err(NativeInterruption::Transport); }
        if !result.exec_accepted { return Err(NativeInterruption::Rejected); }
        if overflow { return Err(NativeInterruption::OutputLimit); }
        if result.exit.is_none() { return Err(NativeInterruption::MissingExit); }
        Ok(())
    };
    // Independent input and output futures: no write-window await inside the
    // receive pump, and no stderr starvation behind a separate output queue.
    tokio::try_join!(receive, send)?;
    Ok(())
}

fn append(output: &mut Vec<u8>, bytes: &[u8], maximum: usize) -> bool {
    let remaining = maximum.saturating_sub(output.len());
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    bytes.len() > remaining
}

struct StrictPeer {
    target: NativeSshTarget,
    initial_host_algorithm: Arc<OnceLock<russh::keys::Algorithm>>,
}
impl client::Handler for StrictPeer {
    type Error = russh::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // Pin trust is independent of signature-algorithm policy. In this
        // callback a raw RSA key can carry hash_alg=None even for RSA/SHA-2.
        Ok(self.target.accepts(key))
    }
    async fn kex_done(&mut self, _: Option<&[u8]>, names: &russh::Names, _: &mut client::Session)
        -> Result<(), Self::Error>
    {
        // Applies to negotiated algorithms on both initial KEX and rekeys.
        // Deliberately do not copy/store the callback's shared-secret argument.
        if matches!(names.key, russh::keys::Algorithm::Rsa { hash: None }) {
            return Err(russh::Error::WrongServerSig);
        }
        let _ = self.initial_host_algorithm.set(names.key.clone());
        Ok(())
    }
    async fn should_accept_unknown_server_channel(&mut self, _: ChannelId, _: &str) -> bool { false }

    async fn server_channel_open_session(&mut self, _: Channel<client::Msg>, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_agent_forward(&mut self, _: Channel<client::Msg>, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_unknown(&mut self, _: Channel<client::Msg>, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_x11(&mut self, _: Channel<client::Msg>, _: &str, _: u32, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_direct_tcpip(&mut self, _: Channel<client::Msg>, _: &str, _: u32, _: &str, _: u32, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_direct_streamlocal(&mut self, _: Channel<client::Msg>, _: &str, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_forwarded_tcpip(&mut self, _: Channel<client::Msg>, _: &str, _: u32, _: &str, _: u32, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
    async fn server_channel_open_forwarded_streamlocal(&mut self, _: Channel<client::Msg>, _: &str, reply: client::ChannelOpenHandle, _: &mut client::Session) -> Result<(), Self::Error> { reject(reply).await }
}
async fn reject(reply: client::ChannelOpenHandle) -> Result<(), russh::Error> {
    reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
    Ok(())
}

#[cfg(test)]
mod tests;
