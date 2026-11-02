// Own deterministic parser/Store/Node/child and memory-protocol fixtures only.
// No real helper process, filesystem key, remote account or model is executed.
mod native_ssh_installation {
    use super::*;
    use async_trait::async_trait;
    use shenko_exec_ssh::{NativeSessionOpener, NativeSshInstallation, SshBinding, SshTarget,
        native_transport::{NativeSshConnection, NativeSshError, NativeSshSession, NativeSshTarget}};
    use shenko_subject::NativeTaskHandler;
    use shenko_types::object::Digest;
    use russh::{server, Channel, ChannelId, ChannelMsg, keys::{PrivateKey, PrivateKeyWithHashAlg,
        ssh_key::private::Ed25519Keypair}};
    use std::{sync::{Mutex, atomic::{AtomicUsize, Ordering}}, time::Duration};

    fn key(seed: u8) -> PrivateKey { Ed25519Keypair::from_seed(&[seed; 32]).into() }
    #[derive(Clone, Copy)]
    enum Mode { Complete, LoseFirstReply, CancelAfterAuth, WrongSigner }
    struct MemoryOpener {
        task_id: EntityId, mode: Mode, opens: AtomicUsize,
        commands: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
        jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    }
    struct Peer {
        operation: shenko_types::operation::OperationRecord,
        lose_reply: bool, channel: Option<Channel<server::Msg>>,
        commands: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
        workers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    }
    impl server::Handler for Peer {
        type Error = russh::Error;
        async fn auth_publickey(&mut self, account: &str, verified: server::VerifiedPublicKey<'_>)
            -> std::result::Result<server::Auth, Self::Error> {
            // Two owned identities let the wrong-signer control authenticate:
            // protocol authentication alone must not satisfy installer identity.
            Ok(if account == "fixture" && (verified.public_key.key_data() == key(82).public_key().key_data()
                || verified.public_key.key_data() == key(83).public_key().key_data()) {
                server::Auth::Accept
            } else { server::Auth::reject() })
        }
        async fn channel_open_session(&mut self, channel: Channel<server::Msg>,
            reply: server::ChannelOpenHandle, _: &mut server::Session)
            -> std::result::Result<(), Self::Error> {
            self.channel = Some(channel); reply.accept().await; Ok(())
        }
        async fn exec_request(&mut self, id: ChannelId, command: &[u8], session: &mut server::Session)
            -> std::result::Result<(), Self::Error> {
            let command = std::str::from_utf8(command).unwrap();
            assert!(command.contains(&self.operation.operation_id.to_string()));
            assert!(command.contains(&self.operation.input_digest.0));
            let mode = if command.ends_with("'dispatch'") { "dispatch" } else {
                assert!(command.ends_with("'query'")); "query"
            }.to_owned();
            session.channel_success(id)?;
            let channel = self.channel.take().unwrap();
            let commands = self.commands.clone(); let lose = self.lose_reply;
            let receipt = serde_json::to_vec(&serde_json::json!({
                "schema":1, "operation_id":self.operation.operation_id,
                "seal":self.operation.input_digest.0, "state":"completed", "exit_code":0,
                "stdout_b64":"b3duZWQtZml4dHVyZS1kYXRh", "stderr_b64":"",
                "stdout_truncated":false, "stderr_truncated":false, "sha256":null,
            })).unwrap();
            self.workers.lock().unwrap().push(tokio::spawn(async move {
                let (mut read, write) = channel.split(); let mut input = Vec::new();
                let mut eof = false;
                while let Some(msg) = read.wait().await {
                    match msg { ChannelMsg::Data { data } => input.extend_from_slice(&data),
                        ChannelMsg::Eof => { eof = true; break; },
                        ChannelMsg::Close => panic!("fixture input closed before EOF"), _ => {} }
                }
                assert!(eof);
                if mode == "query" { assert!(input.is_empty(), "recovery replayed action"); }
                else { assert_eq!(serde_json::from_slice::<serde_json::Value>(&input).unwrap(),
                    serde_json::json!({"kind":"download","path":"/owned-fixture"})); }
                commands.lock().unwrap().push((mode, input));
                if !lose { write.data(&receipt[..]).await.unwrap(); write.exit_status(0).await.unwrap(); }
                write.eof().await.unwrap(); write.close().await.unwrap();
            })); Ok(())
        }
    }
    #[async_trait]
    impl NativeSessionOpener for MemoryOpener {
        async fn open(&self, store: &shenko_store::Store, binding: &SshBinding, budget: Duration)
            -> std::result::Result<NativeSshSession, NativeSshError> {
            let task = store.external_task(self.task_id).unwrap();
            let id: EntityId = task.memos[shenko_store::child::NATIVE_OPERATION_ID].as_str().unwrap().parse().unwrap();
            // The actual installer callback MUST see durable T1 before any
            // authentication. It cannot arm or create the operation itself.
            let operation = store.external_operation(id).unwrap();
            assert_eq!(operation.task_id, self.task_id);
            let attempt = store.external_operation_attempt(id).unwrap();
            assert_eq!(attempt.attempt_no, 1);
            assert_eq!(store.external_operation_attempts(id).unwrap().len(), 1);
            let n = self.opens.fetch_add(1, Ordering::SeqCst);
            let workers = Arc::new(Mutex::new(Vec::new()));
            let peer = Peer { operation, lose_reply: matches!(self.mode, Mode::LoseFirstReply) && n == 0,
                channel: None, commands: self.commands.clone(), workers: workers.clone() };
            let config = Arc::new(server::Config { keys: vec![key(81)], window_size: 64 * 1024,
                maximum_packet_size: 32 * 1024, channel_buffer_size: 4, event_buffer_size: 4,
                auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO),
                inactivity_timeout: Some(Duration::from_secs(10)), ..Default::default() });
            let (client_io, server_io) = tokio::io::duplex(1024);
            self.jobs.lock().unwrap().push(tokio::spawn(async move {
                // Clean local cancellation may terminate the SSH session; worker
                // assertions must still be joined, never detached and ignored.
                let _ = server::run_stream(config, server_io, peer).await.unwrap().await;
                let jobs = std::mem::take(&mut *workers.lock().unwrap());
                for job in jobs { job.await.unwrap(); }
            }));
            let connection = NativeSshConnection::connect_stream(
                NativeSshTarget::from_binding(store, binding)?, client_io, budget).await?;
            let signer = if matches!(self.mode, Mode::WrongSigner) { 83 } else { 82 };
            let session = connection.authenticate_private_key(PrivateKeyWithHashAlg::new(
                Arc::new(key(signer)), None)).await?;
            if matches!(self.mode, Mode::CancelAfterAuth) {
                store.request_native_cancellation(self.task_id, EntityId::new(), EntityId::new(),
                    "owned fixture cancellation during opener await", AbortScope::Task).unwrap();
            }
            Ok(session)
        }
    }
    impl MemoryOpener {
        async fn finish(&self) {
            let jobs = std::mem::take(&mut *self.jobs.lock().unwrap());
            for job in jobs { tokio::time::timeout(Duration::from_secs(2), job).await.unwrap().unwrap(); }
        }
    }
    struct Fixture {
        root: PathBuf, store: Arc<shenko_store::Store>, supervisor: shenko_kernel::Supervisor,
        service: Arc<shenko_subject::NativeChildService>, opener: Arc<MemoryOpener>,
        handler: Arc<shenko_exec_host::NativeSshTaskHandler>, task_id: EntityId, decision_id: EntityId,
        binding: SshBinding, node_id: EntityId,
    }
    async fn fixture(mode: Mode) -> Fixture {
        let root = std::env::temp_dir().join(format!("shenko-native-installed-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(open_native_fixture(&root));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: root.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id()).unwrap();
        bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        drop(home); store.start_subject_dispatcher().unwrap();
        let mut node = node_sync::open_node(&root, &store).unwrap();
        let profile = node.capability_profile().unwrap();
        let decision_id = EntityId::new();
        node.record_node_selection(shenko_node::NodeSelection { decision_id, node_id: profile.node_id,
            observed_profile_digest: Digest::blake3(&serde_json::to_vec(&profile).unwrap()),
            reason: "owned fixture explicitly chooses its actual initialized Node".into() }).unwrap();
        let trust = store.cas.seal_object(format!("fixture.invalid {}\n", key(81).public_key().to_openssh().unwrap()).as_bytes(),
            "text/plain", "ssh-public-trust", "openssh-known-hosts", 1).unwrap().object;
        let binding = SshBinding { target_id: "owned-native-target".into(), target: SshTarget {
            host: "fixture.invalid".into(), user: "fixture".into(), port: 22,
            key_path: Some(root.join("never-created-private-key")), ..Default::default()
        }, receipt_root: "/owned-fixture-receipts".into(), known_hosts: trust };
        let task_id = EntityId::new();
        let opener = Arc::new(MemoryOpener { task_id, mode, opens: AtomicUsize::new(0),
            commands: Arc::new(Mutex::new(vec![])), jobs: Mutex::new(vec![]) });
        let installation = NativeSshInstallation::new(&store, profile.node_id, binding.clone(),
            "owned-fixture-revision-1".into(), &key(82).public_key().to_openssh().unwrap(), opener.clone()).unwrap();
        let handler = Arc::new(shenko_exec_host::NativeSshTaskHandler::new(store.clone(), node, vec![installation]).unwrap());
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
        let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
        supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin,
            handler.clone() as Arc<dyn NativeTaskHandler>).await.unwrap();
        Fixture { root, store, supervisor, service, opener, handler, task_id, decision_id, binding, node_id: profile.node_id }
    }
    fn submit(f: &Fixture) {
        let input = serde_json::json!({"target_id":"owned-native-target","decision_id":f.decision_id,
            "action":{"kind":"download","path":"/owned-fixture"},"timeout_secs":5});
        commit_native_fixture_step(&f.store, &format!("[delegate:ssh.native:1] {input}"), &[f.task_id], vec![]);
    }
    async fn first_evidence(f: &Fixture) -> EntityId {
        f.service.run_once().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let task = f.store.external_task(f.task_id).unwrap();
                if let Some(id) = task.memos.get(shenko_store::child::NATIVE_OPERATION_ID).and_then(|v| v.as_str()) {
                    let id = id.parse().unwrap();
                    if !f.store.external_operation_evidence(id).unwrap().is_empty() { break id; }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap()
    }

    #[tokio::test]
    async fn child_dispatch_t1_precedes_signer_and_delivers_real_receipt() {
        let f = fixture(Mode::Complete).await; submit(&f);
        drive_native_until(&f.service, || Ok(f.store.native_task_result(f.task_id)?.is_some())).await.unwrap();
        let result = f.store.native_task_result(f.task_id).unwrap().unwrap();
        assert_eq!(result.outcome, shenko_types::task::TaskOutcome::Succeeded);
        let value = result.result.unwrap();
        assert_eq!(value["stdout_preview_b64"], "b3duZWQtZml4dHVyZS1kYXRh");
        assert_eq!(value["stdout_preview_truncated"], false);
        assert!(value["receipt_ref"].is_object());
        assert_eq!(f.store.external_operation_attempts(result.operation_id).unwrap().len(), 1);
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 1);
        assert_eq!(f.opener.commands.lock().unwrap()[0].0, "dispatch");
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
        assert!(f.service.installed_prompt_sections().is_empty());
    }

    #[tokio::test]
    async fn lost_receipt_reopen_queries_original_without_dispatch_replay() {
        let f = fixture(Mode::LoseFirstReply).await; submit(&f);
        let id = first_evidence(&f).await;
        let before = f.store.external_operation(id).unwrap();
        assert_eq!(before.effect_status, EffectStatus::Unknown);
        assert!(f.store.native_task_result(f.task_id).unwrap().is_none());
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
        let reopened = Arc::new(open_native_fixture(&f.root)); reopened.start_subject_dispatcher().unwrap();
        let node = node_sync::open_node(&f.root, &reopened).unwrap();
        let installation = NativeSshInstallation::new(&reopened, f.node_id, f.binding.clone(),
            "owned-fixture-revision-1".into(), &key(82).public_key().to_openssh().unwrap(), f.opener.clone()).unwrap();
        let handler = shenko_exec_host::NativeSshTaskHandler::new(reopened.clone(), node, vec![installation]).unwrap();
        let task = reopened.external_task(f.task_id).unwrap();
        let observed = handler.reconcile(&task, id).await.unwrap();
        assert_eq!(observed.operation_id, id); assert_eq!(observed.input_digest, before.input_digest);
        assert_eq!(observed.state, OperationState::Resolved); assert_eq!(observed.effect_status, EffectStatus::None);
        assert_eq!(reopened.external_operation_attempts(id).unwrap().len(), 1);
        let calls = f.opener.commands.lock().unwrap().clone();
        assert_eq!(calls.len(), 2); assert_eq!(calls[0].0, "dispatch");
        assert_eq!(calls[1], ("query".into(), Vec::new()));
        f.opener.finish().await;
    }

    #[tokio::test]
    async fn cancellation_while_opening_authenticated_session_never_sends_exec() {
        let f = fixture(Mode::CancelAfterAuth).await; submit(&f);
        let id = first_evidence(&f).await;
        assert_eq!(f.store.external_operation(id).unwrap().effect_status, EffectStatus::Unknown);
        assert!(f.store.external_task(f.task_id).unwrap().abort_request.is_some());
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 1);
        assert!(f.opener.commands.lock().unwrap().is_empty());
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
    }

    #[tokio::test]
    async fn authenticated_other_identity_is_not_installed_signer() {
        let f = fixture(Mode::WrongSigner).await; submit(&f);
        let id = first_evidence(&f).await;
        assert_eq!(f.store.external_operation(id).unwrap().effect_status, EffectStatus::Unknown);
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 1);
        assert!(f.opener.commands.lock().unwrap().is_empty());
        let evidence = f.store.external_operation_evidence(id).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&f.store.cas.read_object(&evidence[0].raw_ref.digest).unwrap()).unwrap();
        assert_eq!(raw[2], "native SSH installation identity mismatch");
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
    }

    async fn startup_file_signer_failure_after_t1(fifo: bool) {
        let f = fixture(Mode::Complete).await;
        // Withdraw the memory provider before exercising actual startup file
        // construction on the same real runtime. No secret file is created.
        f.supervisor.stop().await.unwrap();
        let trust = f.root.join("owned-public-trust");
        std::fs::write(&trust, format!("fixture.invalid {}\n", key(81).public_key().to_openssh().unwrap())).unwrap();
        let config_path = f.root.join("owned-installation.json");
        let selected_key = f.root.join("never-created-private-key");
        std::fs::write(&config_path, serde_json::to_vec(&serde_json::json!({
            "schema":"shenko.ssh.file-installation.v1", "targets":[{
                "node_id":f.node_id,"target_id":"owned-native-target","revision":"owned-file-1",
                "host":"fixture.invalid","port":22,"account":"fixture",
                "private_key_file":selected_key,
                "public_key_openssh":key(82).public_key().to_openssh().unwrap(),
                "known_hosts_file":trust,"receipt_root":"/owned-fixture-receipts"
            }]})).unwrap()).unwrap();
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        supervisor.spawn(shenko_subject::NativeChildPlugin, (f.store.clone(), RuntimeGeneration::default())).await.unwrap();
        let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
        assert!(service.installed_prompt_sections().is_empty());
        install_native_ssh(&supervisor, f.store.clone(), node_sync::open_node(&f.root, &f.store).unwrap(), &config_path).await.unwrap();
        let prompt = service.installed_prompt_sections().join("\n");
        assert!(prompt.contains("owned-native-target"));
        assert!(!prompt.contains("fixture.invalid")); assert!(!prompt.contains("never-created-private-key"));
        assert!(!selected_key.exists());
        // The real startup installer has frozen this exact file reference.
        // Only the new negative case now substitutes an owned FIFO, with no
        // writer. The real opener must reject it rather than block or fallback.
        #[cfg(unix)]
        if fifo {
            use std::os::unix::ffi::OsStrExt as _;
            let name = std::ffi::CString::new(selected_key.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        #[cfg(not(unix))]
        assert!(!fifo, "FIFO fixture is Unix-only");
        submit(&f);
        service.run_once().await.unwrap();
        let id = tokio::time::timeout(Duration::from_secs(10), async { loop {
            let task = f.store.external_task(f.task_id).unwrap();
            if let Some(id) = task.memos.get(shenko_store::child::NATIVE_OPERATION_ID).and_then(|v| v.as_str()) {
                let id = id.parse().unwrap();
                if !f.store.external_operation_evidence(id).unwrap().is_empty() { break id; }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.unwrap();
        let op = f.store.external_operation(id).unwrap(); assert_eq!(op.effect_status, EffectStatus::Unknown);
        let evidence = f.store.external_operation_evidence(id).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&f.store.cas.read_object(&evidence[0].raw_ref.digest).unwrap()).unwrap();
        assert_eq!(raw[2], "selected native SSH credential unavailable");
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 0);
        assert!(f.store.native_task_result(f.task_id).unwrap().is_none());
        assert_eq!(f.store.external_operation_attempts(id).unwrap().len(), 1);
        assert_eq!(f.store.external_operation_attempt(id).unwrap().attempt_no, 1);
        if !fifo { assert!(!selected_key.exists()); }
        #[cfg(unix)]
        if fifo {
            use std::os::unix::fs::FileTypeExt as _;
            assert!(std::fs::symlink_metadata(&selected_key).unwrap().file_type().is_fifo());
        }
        supervisor.stop().await.unwrap();
    }

    #[tokio::test]
    async fn startup_file_installation_diagnoses_missing_key_after_t1_without_fallback() {
        startup_file_signer_failure_after_t1(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_file_installation_rejects_fifo_signer_after_t1_without_fallback() {
        startup_file_signer_failure_after_t1(true).await;
    }
    // Real S1 + Store claim, with the same public NativeTaskInvocation contract
    // used by the installed registry. Only these negative tests detach fields.
    fn claimed_invocation(f: &Fixture) -> shenko_subject::NativeTaskInvocation {
        submit(f);
        let descriptor = f.handler.descriptor();
        let claim = f.store.claim_native_task(f.task_id, descriptor.implementation_ref.clone(), false).unwrap().unwrap();
        // Prove the real Store claim bound the installed implementation before
        // either negative case detaches only its intended identity/context.
        assert_eq!(claim.task.kind, descriptor.kind);
        assert_eq!(claim.task.definition_version, descriptor.definition_version);
        assert_eq!(claim.task.implementation_ref.plugin_id, descriptor.implementation_ref.plugin_id);
        assert_eq!(claim.task.implementation_ref.artifact_digest, descriptor.implementation_ref.artifact_digest);
        let context = shenko_types::agent::CallContext {
            caller_instance_id: EntityId::new(), role_revision: descriptor.role_revision,
            task_id: Some(f.task_id), expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id), controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(), authority_refs: descriptor.authority_refs,
            trace_id: EntityId::new(),
        };
        let authority = shenko_types::operation::AuthorityRef { delegation_id: f.task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim.task.created_by_step_id.unwrap_or(claim.invocation.invocation_id) };
        shenko_subject::NativeTaskInvocation { context, task_invocation: claim.invocation,
            task: claim.task, operation_id: claim.operation_id, authority,
            input: serde_json::json!({"target_id":"owned-native-target","decision_id":f.decision_id,
                "action":{"kind":"download","path":"/owned-fixture"},"timeout_secs":5}) }
    }

    #[tokio::test]
    async fn same_agent_other_branch_and_origin_is_not_the_installed_subject() {
        let f = fixture(Mode::Complete).await;
        let invocation = claimed_invocation(&f); let id = invocation.operation_id;
        let installed = f.store.installation_binding().unwrap().subject.unwrap();
        let mut other = shenko_node::Node::open(&f.root.join("other-owned-node-branch")).unwrap();
        other.handle(shenko_node::Request::Initialize { node_id: f.node_id,
            identity: Some(shenko_node::Identity { agent_id: installed.agent_id,
                branch_id: EntityId::new(), origin_proof: Digest::blake3(b"other-owned-fixture-origin") }) }).unwrap();
        // Give the detached branch otherwise valid current choice evidence so
        // an absent identity guard cannot pass merely as SelectionMissing.
        let profile = other.capability_profile().unwrap();
        other.record_node_selection(shenko_node::NodeSelection { decision_id: f.decision_id,
            node_id: f.node_id, observed_profile_digest: Digest::blake3(&serde_json::to_vec(&profile).unwrap()),
            reason: "owned detached branch otherwise has valid choice evidence".into() }).unwrap();
        let installation = NativeSshInstallation::new(&f.store, f.node_id, f.binding.clone(),
            "owned-fixture-revision-1".into(), &key(82).public_key().to_openssh().unwrap(), f.opener.clone()).unwrap();
        let detached = shenko_exec_host::NativeSshTaskHandler::new(f.store.clone(), other, vec![installation]).unwrap();
        assert_eq!(detached.execute(invocation).await.unwrap_err().to_string(), "ssh_node_subject_binding_stale");
        assert!(f.store.external_operation(id).is_err());
        assert!(f.store.external_operation_attempts(id).unwrap().is_empty());
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 0);
        assert!(f.opener.commands.lock().unwrap().is_empty());
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
    }

    #[tokio::test]
    async fn detached_task_snapshot_and_call_context_cannot_reach_t1() {
        let f = fixture(Mode::Complete).await;
        let mut invocation = claimed_invocation(&f); let id = invocation.operation_id;
        // One detached invocation has both a stale task snapshot and a context
        // for another invocation. This is one combined negative case, not a
        // claim of independently isolating every individual context predicate.
        invocation.task.task_revision = TaskRevision(invocation.task.task_revision.0 + 1);
        invocation.context.invocation_id = Some(EntityId::new());
        assert_eq!(f.handler.execute(invocation).await.unwrap_err().to_string(), "ssh_trusted_invocation_mismatch");
        assert!(f.store.external_operation(id).is_err());
        assert!(f.store.external_operation_attempts(id).unwrap().is_empty());
        assert_eq!(f.opener.opens.load(Ordering::SeqCst), 0);
        assert!(f.opener.commands.lock().unwrap().is_empty());
        f.supervisor.stop().await.unwrap(); f.opener.finish().await;
    }

}
