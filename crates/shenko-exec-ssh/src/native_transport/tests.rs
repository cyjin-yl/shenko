//! Self-owned deterministic keys and real SSH packets over bounded in-memory
//! streams only. No socket, command process, key file, agent, model or account.
use super::*;
use crate::{SshBinding, SshTarget};
use russh::{keys::{PrivateKey, ssh_key::private::Ed25519Keypair}, server};
use shenko_store::Store;
use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};
use tokio::{io::{AsyncReadExt, duplex}, task::JoinHandle};

fn key(seed: u8) -> PrivateKey {
    // Deliberately public test-only seeds, never usable production credentials.
    Ed25519Keypair::from_seed(&[seed; 32]).into()
}
fn target(public: &PublicKey, revoked: bool) -> NativeSshTarget {
    let root = std::env::temp_dir().join(format!("shenko-native-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pin = format!("fixture.invalid {}\n", public.to_openssh().unwrap());
    let trust = if revoked { format!("{pin}@revoked {pin}") } else { pin };
    let path = root.join("public-known-hosts");
    std::fs::write(&path, trust).unwrap();
    let result = {
        let store = Store::open(root.join("ledger.db"), root.join("cas")).unwrap();
        let binding = SshBinding::register(&store, "native-fixture".into(), SshTarget {
            host: "fixture.invalid".into(), user: "fixture".into(), port: 22,
            key_path: Some(root.join("never-created-or-read-private-key")),
            known_hosts_path: Some(path), ..SshTarget::default()
        }, "/fixture-receipts".into()).unwrap();
        NativeSshTarget::from_binding(&store, &binding).unwrap()
    };
    std::fs::remove_dir_all(root).unwrap();
    result
}
fn limits() -> NativeCommandLimits {
    NativeCommandLimits { deadline: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(1), stdout_bytes: 256 * 1024, stderr_bytes: 1024 }
}
#[derive(Clone, Copy)]
enum Mode { Duplex, Silent }
struct Peer {
    expected: PublicKey,
    auths: Arc<AtomicUsize>,
    mode: Mode,
    channel: Option<Channel<server::Msg>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, user: &str, verified: server::VerifiedPublicKey<'_>)
        -> Result<server::Auth, Self::Error>
    {
        self.auths.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "fixture" && verified.public_key.key_data() == self.expected.key_data() {
            server::Auth::Accept
        } else { server::Auth::reject() })
    }
    async fn channel_open_session(&mut self, channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle, _: &mut server::Session) -> Result<(), Self::Error>
    {
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }
    async fn exec_request(&mut self, channel: ChannelId, command: &[u8], session: &mut server::Session)
        -> Result<(), Self::Error>
    {
        assert_eq!(command, b"fixture-command"); // bytes observed, never interpreted
        session.channel_success(channel)?;
        if matches!(self.mode, Mode::Silent) { return Ok(()); }
        let channel = self.channel.take().unwrap();
        let worker = tokio::spawn(async move {
            let (mut read, write) = channel.split();
            // More than the 64 KiB SSH window and 1 KiB duplex buffer. A client
            // that waits for all stdin to finish before draining stdout stalls.
            // Drain stdin concurrently: a full server-side inbound queue must
            // not stall the protocol loop needed by this stdout writer.
            let send = async {
                write.data(&vec![b'o'; 128 * 1024][..]).await.unwrap();
            };
            let receive = async {
                let mut input = Vec::new();
                while let Some(msg) = read.wait().await {
                    match msg {
                        ChannelMsg::Data { data } => input.extend_from_slice(&data),
                        ChannelMsg::Eof => break,
                        ChannelMsg::Close => panic!("client closed before fixture input EOF"),
                        _ => {}
                    }
                }
                assert_eq!(input, vec![b'i'; 128 * 1024]);
            };
            tokio::join!(send, receive);
            write.extended_data(1, &b"stderr-after-input"[..]).await.unwrap();
            write.exit_status(255).await.unwrap();
            write.eof().await.unwrap();
            write.close().await.unwrap();
        });
        self.workers.lock().unwrap().push(worker);
        Ok(())
    }
}
struct Running {
    server: JoinHandle<Result<(), russh::Error>>,
    handle: server::Handle,
    auths: Arc<AtomicUsize>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl Running {
    async fn finish(self) {
        // Stream closure may surface as EOF/IO error: neither is a remote
        // process-stop guarantee. Require that the session actually terminates.
        let _ = timeout(Duration::from_secs(2), self.server).await.unwrap().unwrap();
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers { timeout(Duration::from_secs(2), worker).await.unwrap().unwrap(); }
    }
}
async fn connect(target: NativeSshTarget, mode: Mode)
    -> (Result<NativeSshConnection, NativeSshError>, Running)
{
    connect_with_host(target, mode, key(11), russh::Preferred::default()).await
}
async fn connect_with_host(target: NativeSshTarget, mode: Mode, host_key: PrivateKey,
    preferred: russh::Preferred) -> (Result<NativeSshConnection, NativeSshError>, Running)
{
    let (client_io, server_io) = duplex(1024);
    let auths = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(Mutex::new(vec![]));
    let peer = Peer { expected: key(22).public_key().clone(), auths: auths.clone(),
        mode, channel: None, workers: workers.clone() };
    let config = Arc::new(server::Config {
        keys: vec![host_key], preferred, window_size: 64 * 1024, maximum_packet_size: 32 * 1024,
        channel_buffer_size: 4, event_buffer_size: 4,
        auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(10)), ..Default::default()
    });
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let running = server::run_stream(config, server_io, peer).await?;
        let _ = sender.send(running.handle());
        running.await
    });
    let connection = NativeSshConnection::connect_stream(target, client_io, Duration::from_secs(5)).await;
    let handle = timeout(Duration::from_secs(2), receiver).await.unwrap().unwrap();
    (connection, Running { server, handle, auths, workers })
}
async fn authenticate(connection: NativeSshConnection) -> NativeSshSession {
    connection.authenticate_private_key(PrivateKeyWithHashAlg::new(Arc::new(key(22)), None)).await.unwrap()
}

#[tokio::test]
async fn native_protocol_duplex_and_unambiguous_exit_255() {
    let (connection, running) = connect(target(key(11).public_key(), false), Mode::Duplex).await;
    let session = authenticate(connection.unwrap()).await;
    let observation = session.execute(b"fixture-command", &vec![b'i'; 128 * 1024], limits()).await.unwrap();
    assert_eq!(running.auths.load(Ordering::SeqCst), 1);
    assert_eq!(observation.stdout, vec![b'o'; 128 * 1024]);
    assert_eq!(observation.stderr, b"stderr-after-input");
    assert_eq!(observation.exit, Some(NativeExit::Status { code: 255 }));
    assert!(observation.exec_request_queued && observation.exec_accepted && observation.channel_closed);
    assert!(observation.session_end_observed);
    assert_eq!(observation.interruption, None);
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_wrong_pin_stops_before_authentication() {
    let (connection, running) = connect(target(key(33).public_key(), false), Mode::Duplex).await;
    assert!(matches!(connection, Err(NativeSshError::Connect)));
    assert_eq!(running.auths.load(Ordering::SeqCst), 0);
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_revocation_overrides_matching_pin() {
    let (connection, running) = connect(target(key(11).public_key(), true), Mode::Duplex).await;
    assert!(matches!(connection, Err(NativeSshError::Connect)));
    assert_eq!(running.auths.load(Ordering::SeqCst), 0);
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_refuses_unsolicited_session_without_breaking_outbound_command() {
    let (connection, running) = connect(target(key(11).public_key(), false), Mode::Duplex).await;
    let session = authenticate(connection.unwrap()).await;
    let inbound = timeout(Duration::from_secs(2), running.handle.channel_open_session()).await.unwrap();
    assert!(inbound.is_err());
    let observation = session.execute(b"fixture-command", &vec![b'i'; 128 * 1024], limits()).await.unwrap();
    assert_eq!(observation.interruption, None);
    assert_eq!(observation.exit, Some(NativeExit::Status { code: 255 }));
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_deadline_preserves_unknown_and_observes_local_session_end() {
    let (connection, running) = connect(target(key(11).public_key(), false), Mode::Silent).await;
    let session = authenticate(connection.unwrap()).await;
    let observation = session.execute(b"fixture-command", b"", NativeCommandLimits {
        deadline: Duration::from_millis(100), ..limits()
    }).await.unwrap();
    assert!(observation.exec_accepted);
    assert_eq!(observation.exit, None);
    assert!(!observation.channel_closed);
    assert_eq!(observation.interruption, Some(NativeInterruption::Deadline));
    assert!(observation.session_end_observed); // local protocol end only
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_output_cap_keeps_draining_and_retains_status() {
    let (connection, running) = connect(target(key(11).public_key(), false), Mode::Duplex).await;
    let session = authenticate(connection.unwrap()).await;
    let observation = session.execute(b"fixture-command", &vec![b'i'; 128 * 1024],
        NativeCommandLimits { stdout_bytes: 32, ..limits() }).await.unwrap();
    assert_eq!(observation.stdout, vec![b'o'; 32]);
    assert_eq!(observation.stderr, b"stderr-after-input");
    assert_eq!(observation.exit, Some(NativeExit::Status { code: 255 }));
    assert!(observation.channel_closed && observation.session_end_observed);
    assert_eq!(observation.interruption, Some(NativeInterruption::OutputLimit));
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_cancel_during_kex_closes_owned_stream() {
    let (client_io, mut peer) = duplex(1024);
    let pending = tokio::spawn(NativeSshConnection::connect_stream(
        target(key(11).public_key(), false), client_io, Duration::from_secs(5)));
    // Observe the real client identification, then cancel before any server
    // identification/KEX is supplied. No timing-only sleep to infer startup.
    let mut byte = [0u8; 1];
    loop {
        timeout(Duration::from_secs(2), peer.read_exact(&mut byte)).await.unwrap().unwrap();
        if byte == [b'\n'] { break; }
    }
    pending.abort();
    assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
    let mut remaining = vec![];
    timeout(Duration::from_secs(2), peer.read_to_end(&mut remaining)).await.unwrap().unwrap();
}

fn public_test_only_rsa() -> PrivateKey {
    // Public deterministic fixture, generated for these tests. Never install
    // it as a credential or interpret its PRIVATE KEY label as user material.
    PrivateKey::from_openssh(include_str!("fixtures/public-test-only-rsa")).unwrap()
}
fn rsa_host_preference(hash: Option<HashAlg>) -> russh::Preferred {
    russh::Preferred { key: Cow::Owned(vec![
        russh::keys::Algorithm::Rsa { hash }, russh::keys::Algorithm::Ed25519,
    ]), ..Default::default() }
}

#[tokio::test]
async fn native_protocol_rsa_sha2_host_accepts_raw_ssh_rsa_pin() {
    let host = public_test_only_rsa();
    assert!(matches!(host.public_key().algorithm(), russh::keys::Algorithm::Rsa { hash: None }));
    let pin = target(host.public_key(), false);
    let (connection, running) = connect_with_host(pin, Mode::Duplex, host,
        rsa_host_preference(Some(HashAlg::Sha256))).await;
    let session = authenticate(connection.unwrap()).await;
    let observation = session.execute(b"fixture-command", &vec![b'i'; 128 * 1024], limits()).await.unwrap();
    assert_eq!(observation.interruption, None);
    assert_eq!(observation.exit, Some(NativeExit::Status { code: 255 }));
    assert_eq!(running.auths.load(Ordering::SeqCst), 1);
    running.finish().await;
}

#[tokio::test]
async fn native_protocol_rsa_sha1_only_host_refused_before_authentication() {
    let host = public_test_only_rsa();
    let pin = target(host.public_key(), false);
    let (connection, running) = connect_with_host(pin, Mode::Duplex, host,
        rsa_host_preference(None)).await;
    assert!(matches!(connection, Err(NativeSshError::Connect)));
    assert_eq!(running.auths.load(Ordering::SeqCst), 0);
    running.finish().await;
}
