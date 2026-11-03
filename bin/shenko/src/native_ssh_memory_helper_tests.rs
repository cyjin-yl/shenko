// Separate no-socket integration fixture. Actual SSH bytes and shipped Python
// helper run over owned memory streams; this does NOT exercise ExplicitFileOpener,
// TCP, a login shell, remote hosts, or the blocked private-loopback carrier.
#[cfg(target_os = "linux")]
mod native_ssh_memory_helper {
    use super::*;
    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use russh::{server, Channel, ChannelId, ChannelMsg, keys::{PrivateKey, PrivateKeyWithHashAlg, HashAlg,
        ssh_key::private::Ed25519Keypair}};
    use shenko_exec_ssh::{NativeSessionOpener, NativeSshFileTargetConfig, NativeSshInstallation,
        SshBinding, SshTarget, native_transport::{NativeSshConnection, NativeSshError,
        NativeSshSession, NativeSshTarget}};
    use shenko_subject::NativeTaskHandler;
    use shenko_types::object::Digest;
    use std::{os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
        sync::{Mutex, atomic::{AtomicUsize, Ordering}}, time::Duration};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const HELPER: &str = include_str!("../../../crates/shenko-exec-ssh/src/remote_helper.py");
    const PAYLOAD: &[u8] = b"owned loopback upload: receipt recovery must not rewrite me\n";
    const AFTER: &[u8] = b"owned mutation after receipt; query must leave this intact\n";
    fn key(seed: u8) -> PrivateKey { Ed25519Keypair::from_seed(&[seed; 32]).into() }
    fn quote(value: &str) -> String { format!("'{}'", value.replace('\'', "'\\''")) }

    #[derive(Default)]
    struct Observations {
        authenticated: AtomicUsize,
        calls: Mutex<Vec<(String, EntityId, String, usize)>>,
        helper_completions: AtomicUsize,
    }
    struct WorkerGuard(Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>);
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            for job in self.0.lock().unwrap().drain(..) { job.abort(); }
        }
    }
    struct Peer {
        root: PathBuf, task: EntityId, receipts: PathBuf, output: PathBuf,
        lose_reply: bool, channel: Option<Channel<server::Msg>>,
        jobs: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        seen: Arc<Observations>,
    }
    impl Peer {
        fn operation(&self) -> shenko_types::operation::OperationRecord {
            // Open the real persisted ledger independently of the client. T1
            // and its one attempt must already exist before authentication.
            let store = open_native_fixture(&self.root);
            let task = store.external_task(self.task).unwrap();
            let id: EntityId = task.memos[shenko_store::child::NATIVE_OPERATION_ID]
                .as_str().unwrap().parse().unwrap();
            let operation = store.external_operation(id).unwrap();
            assert_eq!(operation.task_id, self.task);
            assert_eq!(store.external_operation_attempts(id).unwrap().len(), 1);
            assert_eq!(store.external_operation_attempt(id).unwrap().attempt_no, 1);
            operation
        }
    }
    impl server::Handler for Peer {
        type Error = russh::Error;
        async fn auth_publickey(&mut self, account: &str, verified: server::VerifiedPublicKey<'_>)
            -> std::result::Result<server::Auth, Self::Error>
        {
            if account != "owned-fixture" || verified.public_key.key_data() != key(92).public_key().key_data() {
                return Ok(server::Auth::reject());
            }
            self.operation();
            self.seen.authenticated.fetch_add(1, Ordering::SeqCst);
            Ok(server::Auth::Accept)
        }
        async fn channel_open_session(&mut self, channel: Channel<server::Msg>, reply: server::ChannelOpenHandle,
            _: &mut server::Session) -> std::result::Result<(), Self::Error>
        {
            assert!(self.channel.is_none()); self.channel = Some(channel); reply.accept().await; Ok(())
        }
        async fn exec_request(&mut self, id: ChannelId, command: &[u8], session: &mut server::Session)
            -> std::result::Result<(), Self::Error>
        {
            let operation = self.operation();
            let mode = if command.ends_with(b"'dispatch'") { "dispatch" } else { "query" };
            // Check the entire actual wire command against the shipped helper
            // bytes and original durable IDs, then execute those same argv
            // directly. This fixture does not test an SSH login shell/parser.
            let expected = format!("python3 -I -c {} {} {} {} {}", quote(HELPER),
                quote(self.receipts.to_str().unwrap()), quote(&operation.operation_id.to_string()),
                quote(&operation.input_digest.0), quote(mode));
            assert!(command == expected.as_bytes(), "unexpected memory-helper command");
            session.channel_success(id)?;
            let channel = self.channel.take().unwrap(); let receipts = self.receipts.clone();
            let output = self.output.clone(); let lose = self.lose_reply;
            let seen = self.seen.clone();
            self.jobs.lock().unwrap().push(tokio::spawn(async move {
                let (mut read, write) = channel.split(); let mut input = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match read.wait().await {
                            Some(ChannelMsg::Data { data }) => {
                                assert!(input.len() + data.len() <= 1024 * 1024); input.extend_from_slice(&data);
                            }
                            Some(ChannelMsg::Eof) => break,
                            Some(ChannelMsg::Close) | None => panic!("input closed before fixture EOF"),
                            _ => {}
                        }
                    }
                }).await.expect("memory-helper input deadline");
                if mode == "query" { assert!(input.is_empty(), "query replayed an action"); }
                else { assert_eq!(serde_json::from_slice::<serde_json::Value>(&input).unwrap(),
                    serde_json::json!({"kind":"upload","path":output,"data_b64":B64.encode(PAYLOAD)})); }
                seen.calls.lock().unwrap().push((mode.into(), operation.operation_id,
                    operation.input_digest.0.clone(), input.len()));
                // No shell, environment credential discovery, arbitrary argv,
                // user command or descendant process: helper only upload/query.
                // -S excludes host distro sitecustomize/.pth startup code. The
                // exact shipped helper and wire command are still checked;
                // remote shell/Python site customization is NOT covered here.
                let mut child = tokio::process::Command::new("/usr/bin/python3")
                    .args(["-I", "-S", "-c", HELPER]).arg(&receipts).arg(operation.operation_id.to_string())
                    .arg(&operation.input_digest.0).arg(mode).env_clear().current_dir(&receipts)
                    .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
                let mut stdin = child.stdin.take().unwrap();
                let stdout = child.stdout.take().unwrap(); let stderr = child.stderr.take().unwrap();
                let collect = async {
                    let send = async { stdin.write_all(&input).await.unwrap(); stdin.shutdown().await.unwrap(); drop(stdin); };
                    let read_out = async { let mut bytes = Vec::new(); stdout.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes).await.unwrap(); bytes };
                    let read_err = async { let mut bytes = Vec::new(); stderr.take(1024 * 1024 + 1).read_to_end(&mut bytes).await.unwrap(); bytes };
                    let (_, out, err, status) = tokio::join!(send, read_out, read_err, child.wait());
                    assert!(out.len() <= 2 * 1024 * 1024 && err.len() <= 1024 * 1024);
                    assert!(status.unwrap().success(), "owned helper failed (body withheld)");
                    assert!(err.is_empty(), "owned helper stderr nonempty (body withheld)"); out
                };
                let out = tokio::time::timeout(Duration::from_secs(5), collect).await.expect("owned helper deadline");
                let receipt: serde_json::Value = serde_json::from_slice(&out).unwrap();
                assert_eq!(receipt["operation_id"], operation.operation_id.to_string());
                assert_eq!(receipt["seal"], operation.input_digest.0);
                assert_eq!(receipt["schema"], 1);
                assert_eq!(receipt["state"], "completed"); assert_eq!(receipt["exit_code"], 0);
                assert_eq!(receipt["stdout_truncated"], false); assert_eq!(receipt["stderr_truncated"], false);
                assert_eq!(receipt["sha256"], "c8bfb6c631c95ffc07af7b194417f086721599b6c98d9970a001ed3ba35dbc71");
                seen.helper_completions.fetch_add(1, Ordering::SeqCst);
                if !lose { write.data(&out[..]).await.unwrap(); write.exit_status(0).await.unwrap(); }
                // Drop only the first reply AFTER the real helper has committed
                // its receipt; do not synthesize a completed JSON response.
                write.eof().await.unwrap(); write.close().await.unwrap();
            })); Ok(())
        }
    }

    struct MemoryOpener {
        root: PathBuf, task: EntityId, receipts: PathBuf, output: PathBuf,
        seen: Arc<Observations>, opens: AtomicUsize,
        jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    }
    impl Drop for MemoryOpener {
        fn drop(&mut self) { for job in self.jobs.lock().unwrap().drain(..) { job.abort(); } }
    }
    #[async_trait]
    impl NativeSessionOpener for MemoryOpener {
        async fn open(&self, store: &shenko_store::Store, binding: &SshBinding, budget: Duration)
            -> std::result::Result<NativeSshSession, NativeSshError>
        {
            // Only this injected transport is synthetic. Admission and both
            // actual SSH peers still consume the persisted original operation.
            let task = store.external_task(self.task).unwrap();
            let id: EntityId = task.memos[shenko_store::child::NATIVE_OPERATION_ID].as_str().unwrap().parse().unwrap();
            assert_eq!(store.external_operation(id).unwrap().task_id, self.task);
            assert_eq!(store.external_operation_attempt(id).unwrap().attempt_no, 1);
            assert_eq!(store.external_operation_attempts(id).unwrap().len(), 1);
            assert_eq!(binding.target.host, "memory-helper.invalid");
            assert_eq!(binding.target.user, "owned-fixture");
            let n = self.opens.fetch_add(1, Ordering::SeqCst);
            let jobs = Arc::new(Mutex::new(vec![]));
            let peer = Peer { root: self.root.clone(), task: self.task, receipts: self.receipts.clone(),
                output: self.output.clone(), lose_reply: n == 0, channel: None,
                jobs: jobs.clone(), seen: self.seen.clone() };
            let config = Arc::new(server::Config { keys: vec![key(91)], window_size: 64 * 1024,
                maximum_packet_size: 32 * 1024, channel_buffer_size: 4, event_buffer_size: 4,
                auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO),
                inactivity_timeout: Some(Duration::from_secs(10)), ..Default::default() });
            let (client_io, server_io) = tokio::io::duplex(1024);
            self.jobs.lock().unwrap().push(tokio::spawn(async move {
                let _workers = WorkerGuard(jobs.clone());
                // Strict-pin failure may close before run_stream completes.
                // Helper worker assertions are always joined, never discarded.
                if let Ok(session) = server::run_stream(config, server_io, peer).await {
                    let _ = tokio::time::timeout(Duration::from_secs(10), session).await.expect("memory SSH session deadline");
                }
                let workers = std::mem::take(&mut *jobs.lock().unwrap());
                for worker in workers { tokio::time::timeout(Duration::from_secs(6), worker).await.unwrap().unwrap(); }
            }));
            let connection = NativeSshConnection::connect_stream(
                NativeSshTarget::from_binding(store, binding)?, client_io, budget).await?;
            connection.authenticate_private_key(PrivateKeyWithHashAlg::new(Arc::new(key(92)), None)).await
        }
    }
    impl MemoryOpener {
        async fn finish(&self) {
            let jobs = std::mem::take(&mut *self.jobs.lock().unwrap());
            for job in jobs { tokio::time::timeout(Duration::from_secs(12), job).await.unwrap().unwrap(); }
        }
    }

    struct Fixture {
        root: PathBuf, store: Arc<shenko_store::Store>, task: EntityId, decision: EntityId,
        output: PathBuf, receipts: PathBuf, binding: SshBinding, revision: String,
        node_id: EntityId, public: String, seen: Arc<Observations>, opener: Arc<MemoryOpener>,
    }
    impl Fixture {
        async fn new(wrong_pin: bool) -> Self {
            // A fail-closed harness marker, not permission or socket isolation.
            assert_eq!(std::env::var("SHENKO_MEMORY_HELPER_FIXTURE").as_deref(), Ok("all-sockets-denied"));
            let root = std::env::temp_dir().join(format!("shenko-own-memory-helper-{}", EntityId::new()));
            std::fs::create_dir(&root).unwrap(); std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            let store = Arc::new(open_native_fixture(&root));
            let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig { data_dir: root.clone(), ..Default::default() })
                .prepare(store.ledger.ledger_id()).unwrap();
            bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap(); drop(home);
            store.start_subject_dispatcher().unwrap();
            let mut node = node_sync::open_node(&root, &store).unwrap(); let profile = node.capability_profile().unwrap();
            let decision = EntityId::new(); node.record_node_selection(shenko_node::NodeSelection { decision_id: decision,
                node_id: profile.node_id, observed_profile_digest: Digest::blake3(&serde_json::to_vec(&profile).unwrap()),
                reason: "owned memory-stream helper capability selected".into() }).unwrap(); drop(node);
            let private = root.join("not-opened-private-key-fifo");
            let trust_file = root.join("not-opened-known-hosts-fifo");
            for path in [&private, &trust_file] {
                let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            let receipts = root.join("receipts"); std::fs::create_dir(&receipts).unwrap();
            std::fs::set_permissions(&receipts, std::fs::Permissions::from_mode(0o700)).unwrap();
            let output = root.join("owned-upload"); let config = root.join("installation.json");
            let public = key(92).public_key().to_openssh().unwrap();
            std::fs::write(&config, serde_json::to_vec(&serde_json::json!({
                "schema":"shenko.ssh.file-installation.v1", "targets":[{"node_id":profile.node_id,
                "target_id":"owned-memory-helper", "revision":"owned-memory-v1", "host":"memory-helper.invalid", "port":22,
                "account":"owned-fixture", "private_key_file":private, "public_key_openssh":format!("{public} private-comment-not-for-output"),
                "known_hosts_file":trust_file, "receipt_root":receipts}]})).unwrap()).unwrap();
            let before_config = std::fs::read(&config).unwrap();
            let cli = std::env::var_os("SHENKO_FIXED_CLI").expect("fixed normal binary missing");
            assert_eq!(PathBuf::from(&cli), PathBuf::from("/fixed-shenko-cli"));
            let cli_data = root.join("cli-must-not-create-store");
            let mut child = tokio::process::Command::new(cli).args(["--data-dir"]).arg(&cli_data)
                .args(["ssh-public-identity", "--installation"]).arg(&config)
                .args(["--target", "owned-memory-helper"]).env_clear()
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
            let stdout = child.stdout.take().unwrap(); let stderr = child.stderr.take().unwrap();
            let observe = async {
                let read_out = async { let mut data = Vec::new(); stdout.take(4097).read_to_end(&mut data).await.unwrap(); data };
                let read_err = async { let mut data = Vec::new(); stderr.take(4097).read_to_end(&mut data).await.unwrap(); data };
                let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
                assert!(status.unwrap().success(), "public CLI failed (body withheld)");
                assert!(out.len() <= 4096 && err.is_empty(), "public CLI output limit/error"); out
            };
            let out = tokio::time::timeout(Duration::from_secs(5), observe).await.expect("public CLI deadline");
            let identity: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(identity, serde_json::json!({"schema":"ssh.configured-public-identity.v1", "status":"configured_unverified",
                "target_id":"owned-memory-helper", "algorithm":"ssh-ed25519", "public_key_openssh":public,
                "fingerprint_sha256":key(92).public_key().fingerprint(HashAlg::Sha256).to_string()}));
            assert_eq!(std::fs::read(&config).unwrap(), before_config); assert!(!cli_data.exists());
            let public = identity["public_key_openssh"].as_str().unwrap().to_owned();
            let mut selected = NativeSshFileTargetConfig::read_installation_file(&config).unwrap();
            assert_eq!(selected.len(), 1); let selected = selected.pop().unwrap();
            assert_eq!(selected.node_id, profile.node_id);
            assert_eq!(selected.target_id, identity["target_id"].as_str().unwrap());
            // Only the public configuration projection feeds this owned native
            // test installer. Do not call install(): its private TCP opener is
            // deliberately outside this no-socket test, and remains unverified.
            let trust = store.cas.seal_object(format!("memory-helper.invalid {}\n",
                key(if wrong_pin { 93 } else { 91 }).public_key().to_openssh().unwrap()).as_bytes(),
                "text/plain", "ssh-public-trust", "openssh-known-hosts", 1).unwrap().object;
            let revision = selected.revision;
            let binding = SshBinding { target_id: selected.target_id, target: SshTarget {
                host: selected.host, user: selected.account, port: selected.port,
                key_path: Some(selected.private_key_file), ..Default::default() }, receipt_root: selected.receipt_root, known_hosts: trust };
            let task = EntityId::new(); let seen = Arc::new(Observations::default());
            let opener = Arc::new(MemoryOpener { root: root.clone(), task, receipts: receipts.clone(),
                output: output.clone(), seen: seen.clone(), opens: AtomicUsize::new(0), jobs: Mutex::new(vec![]) });
            Self { root, store, task, decision, output, receipts, binding, revision,
                node_id: profile.node_id, public, seen, opener }
        }
        async fn install(&self, store: Arc<shenko_store::Store>) -> (shenko_kernel::Supervisor, Arc<shenko_subject::NativeChildService>) {
            let node = node_sync::open_node(&self.root, &store).unwrap();
            let installation = NativeSshInstallation::new(&store, self.node_id, self.binding.clone(),
                self.revision.clone(), &self.public, self.opener.clone()).unwrap();
            let handler = shenko_exec_host::NativeSshTaskHandler::new(store.clone(), node, vec![installation]).unwrap();
            let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
            supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
            let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
            supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, Arc::new(handler) as Arc<dyn NativeTaskHandler>).await.unwrap();
            assert!(service.installed_prompt_sections().iter().any(|s| s.contains("owned-memory-helper")));
            (supervisor, service)
        }
        fn submit(&self) {
            let input = serde_json::json!({"target_id":"owned-memory-helper", "decision_id":self.decision,
                "action":{"kind":"upload","path":self.output,"data_b64":B64.encode(PAYLOAD)}, "timeout_secs":5});
            commit_native_fixture_step(&self.store, &format!("[delegate:ssh.native:1] {input}"), &[self.task], vec![]);
        }
        async fn first_observation(&self, service: &Arc<shenko_subject::NativeChildService>) -> EntityId {
            service.run_once().await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let task = self.store.external_task(self.task).unwrap();
                    if let Some(id) = task.memos.get(shenko_store::child::NATIVE_OPERATION_ID).and_then(|x| x.as_str()) {
                        let id: EntityId = id.parse().unwrap();
                        if !self.store.external_operation_evidence(id).unwrap().is_empty() { break id; }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap()
        }
        async fn finish(&self) { self.opener.finish().await; }
    }
    #[tokio::test]
    #[ignore = "requires fixed no-socket helper/CLI execution approval"]
    async fn configured_identity_actual_helper_receipt_survives_lost_reply_without_replay() {
        let f = Fixture::new(false).await; let (supervisor, service) = f.install(f.store.clone()).await;
        f.submit(); let id = f.first_observation(&service).await;
        let operation = f.store.external_operation(id).unwrap();
        assert_eq!(operation.effect_status, EffectStatus::Unknown);
        assert!(f.store.native_task_result(f.task).unwrap().is_none());
        assert_eq!(std::fs::read(&f.output).unwrap(), PAYLOAD);
        let receipt_file = f.receipts.join(id.to_string()).join("receipt.json");
        let original_receipt = std::fs::read(&receipt_file).unwrap();
        supervisor.stop().await.unwrap(); f.opener.finish().await; assert!(service.installed_prompt_sections().is_empty()); drop(service); drop(supervisor);
        // A replay would overwrite this independent post-effect mutation even
        // if the same bytes/count happened to make the ledger look unchanged.
        std::fs::write(&f.output, AFTER).unwrap();
        let reopened = Arc::new(open_native_fixture(&f.root)); reopened.start_subject_dispatcher().unwrap();
        // New Store and registry handles, same durable ledger. The test peer
        // remains in this test process; this is not a machine/process reboot.
        let (supervisor, service) = f.install(reopened.clone()).await;
        drive_native_until(&service, || Ok(reopened.native_task_result(f.task)?.is_some())).await.unwrap();
        let result = reopened.native_task_result(f.task).unwrap().unwrap();
        assert_eq!(result.operation_id, id); assert_eq!(result.outcome, TaskOutcome::Succeeded);
        let final_operation = reopened.external_operation(id).unwrap();
        assert_eq!(final_operation.input_digest, operation.input_digest);
        assert_eq!(final_operation.effect_status, EffectStatus::Complete);
        assert_eq!(reopened.external_operation_attempts(id).unwrap().len(), 1);
        assert_eq!(std::fs::read(&f.output).unwrap(), AFTER);
        assert_eq!(std::fs::read(&receipt_file).unwrap(), original_receipt);
        let calls = f.seen.calls.lock().unwrap().clone(); assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "dispatch"); assert!(calls[0].3 > 0);
        assert_eq!(calls[1], ("query".into(), id, operation.input_digest.0, 0));
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 2);
        assert_eq!(f.seen.authenticated.load(Ordering::SeqCst), 2);
        assert_eq!(f.seen.helper_completions.load(Ordering::SeqCst), 2);
        supervisor.stop().await.unwrap(); f.finish().await;
    }

    #[tokio::test]
    #[ignore = "requires fixed no-socket helper/CLI execution approval"]
    async fn configured_identity_does_not_authorize_wrong_host_pin_or_helper() {
        let f = Fixture::new(true).await; let (supervisor, service) = f.install(f.store.clone()).await;
        f.submit(); let id = f.first_observation(&service).await;
        assert_eq!(f.store.external_operation(id).unwrap().effect_status, EffectStatus::Unknown);
        assert_eq!(f.store.external_operation_attempts(id).unwrap().len(), 1);
        assert!(f.store.native_task_result(f.task).unwrap().is_none());
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 1);
        let evidence = f.store.external_operation_evidence(id).unwrap();
        assert_eq!(evidence.len(), 1);
        let raw: serde_json::Value = serde_json::from_slice(&f.store.cas.read_object(&evidence[0].raw_ref.digest).unwrap()).unwrap();
        assert_eq!(raw[2], "native SSH connection or host verification failed");
        assert_eq!(f.seen.authenticated.load(Ordering::SeqCst), 0);
        assert!(f.seen.calls.lock().unwrap().is_empty());
        assert_eq!(f.seen.helper_completions.load(Ordering::SeqCst), 0);
        assert!(!f.output.exists()); assert_eq!(std::fs::read_dir(&f.receipts).unwrap().count(), 0);
        supervisor.stop().await.unwrap(); f.finish().await;
    }
}
