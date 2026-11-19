//! Original first E1 over real pinned OpenSSH, independently of any chosen T2.
//! Store control choices use the existing retained-value fixture; this is not
//! a new quorum/process-election acceptance claim.
#![cfg(unix)]

#[path = "../../../crates/shenko-store/tests/support/canonical_native.rs"]
mod canonical_native;
#[path = "support/process_runtime.rs"]
mod process_runtime;

use anyhow::{Context, Result, ensure};
use canonical_native::{
    NativeT2Authority, NativeT2AuthorityState, committed_candidate_with, draft_dispatch_operation,
    initialize_subject_in_with_route, recorded_result_with_docket,
};
use process_runtime::{
    OwnedProcess, append_json, copy_diagnostics, file_sha256, write_sshd_config,
};
use serde_json::{Value, json};
use shenko_node::{Request, SyncPeer};
use shenko_store::{
    ControlRecoveryOutcome, NativeEvidenceDeliveryOutcome, RetentionLimits, Store,
    cognition::CognitionImportOptions,
};
use shenko_types::{
    authority::{CommitAuthorityGrant, CommitAuthorityStamp},
    evidence::*,
    ids::*,
    native_evidence::*,
    object::Digest,
    operation::*,
    subject::{DelegationIntent, StepDecision},
    task::{DispatchEligibility, ImplementationRef, NativeTaskDefinition},
};
use std::{
    cell::Cell,
    fs,
    io::Write,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::Arc,
    time::{Duration, Instant},
};

const COMMAND_LIMIT: Duration = Duration::from_secs(180);
const JOURNEY_LIMIT: Duration = Duration::from_secs(900);

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn executable(name: &str) -> Result<PathBuf> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").context("PATH is required")?) {
        let path = directory.join(name);
        if path.is_file() && path.metadata()?.permissions().mode() & 0o111 != 0 {
            return Ok(path.canonicalize()?);
        }
    }
    anyhow::bail!("real transport executable {name} is required")
}

struct Fixture {
    root: PathBuf,
    counter: Cell<u32>,
    originals_captured: Cell<bool>,
    started: Instant,
    server: Option<OwnedProcess>,
    port: u16,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("shenko-native-e1-{}", EntityId::new()));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        for directory in [
            "private",
            "private/bin",
            "private/source",
            "private/receiver",
            "private/parked",
            "private/causal",
            "private/diverged",
            "logs",
        ] {
            fs::create_dir(root.join(directory))?;
        }
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        Ok(Self {
            root,
            counter: Cell::new(0),
            originals_captured: Cell::new(false),
            started: Instant::now(),
            server: None,
            port,
        })
    }
    fn private(&self) -> PathBuf {
        self.root.join("private")
    }
    fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    fn capture_originals(&self) -> Result<()> {
        if self.originals_captured.get() {
            return Ok(());
        }
        let serial = self.counter.get();
        self.counter.set(serial + 1);
        let label = format!("{serial:03}-capture-originals");
        let script = self.logs().join("native_e1_snapshot.py");
        fs::write(&script, include_str!("support/native_e1_snapshot.py"))?;
        let archive = self.logs().join(format!("{serial:03}-native-e1-originals.zip"));
        let status = OwnedProcess::spawn(
            self.command(executable("python3")?)?
                .arg(&script)
                .arg("--root")
                .arg(self.private())
                .arg("--output")
                .arg(&archive)
                .arg("--source-revision")
                .arg(std::env::var("SHENKO_SOURCE_REVISION").unwrap_or_else(|_| "unrecorded".into()))
                .args(["--additional-store", "causal", "--additional-store", "diverged"]),
            &self.logs(),
            &label,
        )?
        // Evidence collection still has a finite opportunity after the
        // journey's deadline, including its panic/error path.
        .finish(Duration::from_secs(30))?;
        append_json(
            &self.logs().join("originals.jsonl"),
            &json!({"status":status.to_string(),"archive":archive.file_name().and_then(|name| name.to_str()),
                "sha256":archive.is_file().then(|| file_sha256(&archive)).transpose()?,
                "scope":"selected original row values and typed CAS closure, not a SQLite backup"}),
        )?;
        ensure!(status.success(), "native E1 original evidence export failed; see capture logs");
        self.originals_captured.set(true);
        Ok(())
    }
    fn data(&self, name: &str) -> PathBuf {
        self.private().join(name)
    }
    fn path(&self) -> Result<std::ffi::OsString> {
        Ok(std::env::join_paths(
            std::iter::once(self.private().join("bin")).chain(std::env::split_paths(
                &std::env::var_os("PATH").context("PATH is required")?,
            )),
        )?)
    }
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Result<Command> {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir(self.private())
            .env("PATH", self.path()?)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        Ok(command)
    }
    fn run(
        &self,
        command: &mut Command,
        label: &str,
        input: Option<&[u8]>,
    ) -> Result<(ExitStatus, Vec<u8>)> {
        let serial = self.counter.get();
        self.counter.set(serial + 1);
        let label = format!("{serial:03}-{label}");
        let input_path = self.private().join("request.json");
        if let Some(input) = input {
            fs::write(&input_path, input)?;
        }
        let remaining = JOURNEY_LIMIT.saturating_sub(self.started.elapsed());
        ensure!(
            !remaining.is_zero(),
            "native E1 journey exceeded its finite bound"
        );
        let status = OwnedProcess::spawn_with_input(
            command,
            &self.logs(),
            &label,
            input.map(|_| input_path.as_path()),
        )?
        .finish(COMMAND_LIMIT.min(remaining))?;
        append_json(
            &self.logs().join("commands.jsonl"),
            &json!({"label":label,"status":status.to_string()}),
        )?;
        Ok((
            status,
            fs::read(self.logs().join(format!("{label}.stdout")))?,
        ))
    }
    fn cli_sync(&self, peer: EntityId) -> Result<(ExitStatus, Value)> {
        self.cli_sync_from("receiver", peer)
    }
    fn cli_sync_from(&self, local: &str, peer: EntityId) -> Result<(ExitStatus, Value)> {
        let mut command = self.command(env!("CARGO_BIN_EXE_shenko"))?;
        command
            .arg("--data-dir")
            .arg(self.data(local))
            .args(["sync", "evidence", "--peer"])
            .arg(peer.to_string());
        let (status, bytes) = self.run(&mut command, "sync-evidence", None)?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        Ok((status, value))
    }
    fn remote_page(&self, request: &Request) -> Result<NativeEvidencePage> {
        self.remote_page_from("source", request)
    }
    fn remote_response(&self, name: &str, request: &Request) -> Result<(ExitStatus, Vec<u8>)> {
        let mut command = self.command(self.private().join("bin/ssh"))?;
        command.args(["-oBatchMode=yes", name]).arg(format!(
            "{} --data-dir {} node",
            quote(&self.private().join("remote")),
            quote(&self.data(name))
        ));
        self.run(
            &mut command,
            &format!("{name}-page"),
            Some(&serde_json::to_vec(request)?),
        )
    }
    fn remote_page_from(&self, name: &str, request: &Request) -> Result<NativeEvidencePage> {
        let (status, bytes) = self.remote_response(name, request)?;
        ensure!(
            status.success(),
            "source page failed; logs {}",
            self.logs().display()
        );
        let response: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(response["outcome"], "manifest");
        Ok(serde_json::from_value(response["page"].clone())?)
    }
    fn prepare_ssh(&mut self) -> Result<()> {
        let ssh = executable("ssh")?;
        let python = executable("python3")?;
        let binary = Path::new(env!("CARGO_BIN_EXE_shenko")).canonicalize()?;
        for name in ["host", "client"] {
            let (status, _) = self.run(
                self.command("ssh-keygen")?
                    .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                    .arg(self.private().join(name)),
                "keygen",
                None,
            )?;
            ensure!(status.success(), "fixture SSH keygen failed");
        }
        fs::copy(
            self.private().join("client.pub"),
            self.private().join("authorized_keys"),
        )?;
        let (status, user) = self.run(self.command("id")?.arg("-un"), "account", None)?;
        ensure!(status.success(), "cannot identify fixture SSH account");
        let user = String::from_utf8(user)?;
        let user = user.trim();
        ensure!(
            !user.is_empty() && !user.contains(char::is_whitespace),
            "invalid fixture account"
        );
        let public = fs::read_to_string(self.private().join("host.pub"))?;
        let mut fields = public.split_whitespace();
        let kind = fields.next().context("host public key kind")?;
        let key = fields.next().context("host public key")?;
        fs::write(
            self.private().join("known_hosts"),
            format!("[127.0.0.1]:{} {kind} {key}\n", self.port),
        )?;
        fs::write(
            self.private().join("ssh_config"),
            format!(
                "Host source receiver\n HostName 127.0.0.1\n Port {}\n User {user}\n IdentityFile {}\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile {}\n GlobalKnownHostsFile /dev/null\n BatchMode yes\n StrictHostKeyChecking yes\n ConnectionAttempts 1\n LogLevel ERROR\n",
                self.port,
                self.private().join("client").display(),
                self.private().join("known_hosts").display()
            ),
        )?;
        fs::write(
            self.private().join("bin/ssh"),
            format!(
                "#!/bin/sh\nexec {} -F {} \"$@\"\n",
                quote(&ssh),
                quote(&self.private().join("ssh_config"))
            ),
        )?;
        fs::set_permissions(
            self.private().join("bin/ssh"),
            fs::Permissions::from_mode(0o700),
        )?;
        // Each installed fault interrupts one authenticated chunk exchange.
        // Every successful invocation execs the actual CLI with identical
        // stdin; no fact, manifest, chunk or response is synthesized here.
        let launcher = format!(
            "#!{}\nimport json, os, pathlib, sys, tempfile\npayload=sys.stdin.buffer.read()\nfault=pathlib.Path({})\nif json.loads(payload).get('command') == 'read_control_object' and fault.exists():\n    fault.unlink()\n    sys.exit(75)\nstream=tempfile.TemporaryFile()\nstream.write(payload)\nstream.seek(0)\nos.dup2(stream.fileno(), 0)\nos.execve({}, [{}, *sys.argv[1:]], {{'PATH': {}, 'LC_ALL': 'C', 'GIT_CONFIG_NOSYSTEM':'1', 'GIT_CONFIG_GLOBAL':'/dev/null'}})\n",
            python.display(),
            serde_json::to_string(&self.private().join("lose-chunk"))?,
            serde_json::to_string(&binary)?,
            serde_json::to_string(&binary)?,
            serde_json::to_string(&self.path()?.to_string_lossy())?
        );
        fs::write(self.private().join("remote"), launcher)?;
        fs::set_permissions(
            self.private().join("remote"),
            fs::Permissions::from_mode(0o700),
        )?;
        let config = self.private().join("sshd_config");
        write_sshd_config(
            &config,
            &self.private(),
            &self.private().join("host"),
            &self.private().join("authorized_keys"),
            self.port,
        )?;
        self.server = Some(OwnedProcess::spawn(
            self.command(
                std::env::var_os("SHENKO_SSHD").unwrap_or_else(|| "/usr/sbin/sshd".into()),
            )?
            .args(["-D", "-e", "-f"])
            .arg(config),
            &self.logs(),
            "sshd",
        )?);
        let start = Instant::now();
        loop {
            ensure!(
                self.server
                    .as_mut()
                    .context("missing sshd")?
                    .poll()?
                    .is_none(),
                "fixture sshd exited"
            );
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(5),
                "fixture sshd did not listen"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        fs::write(
            self.logs().join("transport.json"),
            serde_json::to_vec_pretty(&json!({
            "system_ssh_sha256":file_sha256(&ssh)?,"cli_sha256":file_sha256(&binary)?,
            "remote_launcher_sha256":file_sha256(&self.private().join("remote"))?,
            "scope":"real pinned OpenSSH routes to source and receiver Stores through the actual CLI; Store control grants are retained-value fixtures",
            "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),"port":self.port}))?,
        )?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let cleanup = self
            .server
            .as_mut()
            .map(|server| server.stop(libc::SIGKILL, Duration::ZERO))
            .transpose();
        let clean = cleanup.is_ok() && TcpStream::connect(("127.0.0.1", self.port)).is_err();
        // Capture before deleting the private installation, on failure too.
        // Only the exporter's explicit public subset enters the log directory.
        let originals = self.capture_originals();
        let record = json!({"clean":clean,"joined":cleanup.as_ref().map(|_| true).unwrap_or(false),
            "error":cleanup.err().map(|error| format!("{error:#}")),
            "originals_retained":originals.is_ok(),
            "originals_error":originals.as_ref().err().map(|error| format!("{error:#}"))});
        let _ = fs::write(
            self.logs().join("cleanup.json"),
            serde_json::to_vec(&record).unwrap_or_default(),
        );
        if let Some(destination) = std::env::var_os("SHENKO_NATIVE_E1_EVIDENCE")
            && let Err(error) = copy_diagnostics(&self.logs(), Path::new(&destination))
        {
            eprintln!("native E1 evidence copy failed: {error:#}");
        }
        if clean && originals.is_ok() && !self.logs().join("processes.cleanup-failed").exists() {
            let _ = fs::remove_dir_all(self.private());
        }
        eprintln!(
            "native E1 original observations retained in {}",
            self.logs().display()
        );
    }
}

fn latest(state: &parking_lot::Mutex<NativeT2AuthorityState>) -> Result<CommitAuthorityGrant> {
    let state = state.lock();
    state
        .grants
        .get(&state.last)
        .cloned()
        .context("original retained grant")
}

fn peer(fixture: &Fixture, name: &str, node_id: EntityId) -> SyncPeer {
    SyncPeer {
        node_id,
        ssh_target: name.into(),
        remote_binary: fixture.private().join("remote").to_string_lossy().into_owned(),
        remote_data_dir: fixture.data(name),
        workspaces: vec![],
        remote_workspace_root: None,
    }
}

#[test]
fn original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let source = Arc::new(Store::open(
        fixture.data("source").join("shenko.db"),
        fixture.data("source").join("cas"),
    )?);
    let agent = initialize_subject_in_with_route(&source, b"{}", None)?;
    let state = Arc::new(parking_lot::Mutex::new(NativeT2AuthorityState::default()));
    let head = source.load_subject(agent)?.0;
    source.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "e1-source",
        stamp: Some(CommitAuthorityStamp {
            source_id: "e1-source".into(),
            holder_id: EntityId::new(),
            subject_id: agent,
            branch_id: head.branch_id.clone(),
            revision: Digest::blake3(b"independent E1 source"),
        }),
        state: Arc::clone(&state),
    }))?;
    source.activate_runtime_authority(&head)?;
    let (head, step) = source.load_subject(agent)?;
    let result = recorded_result_with_docket(&source, &head, step, None, &[])?;
    let next = EntityId::new();
    let mut step = committed_candidate_with(&source, result, next, vec![]);
    let mut decision: StepDecision = serde_json::from_slice(
        &source
            .cas
            .read_object(&step.decision_ref.as_ref().context("S1 decision")?.digest)?,
    )?;
    let task_ids = [EntityId::new(), EntityId::new(), EntityId::new()];
    for id in task_ids {
        decision.delegated_task_intents.push(DelegationIntent {
            intent_id: id,
            goal: "Observe one original external result".into(),
            system_prompt: String::new(),
            model: None,
            capabilities: vec![],
            budget_hint: None,
            reply_grant: None,
            definition: Some(NativeTaskDefinition {
                kind: "fixture.e1.append".into(),
                definition_version: 1,
                input: json!({"task":id}),
            }),
        });
    }
    step.decision_ref = Some(
        source
            .cas
            .seal_object(
                &serde_json::to_vec(&decision)?,
                "application/json",
                "test",
                "StepDecision",
                8,
            )?
            .object,
    );
    let mut initial = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut initial)?;
    let receiver = Store::open(
        fixture.data("receiver").join("shenko.db"),
        fixture.data("receiver").join("cas"),
    )?;
    let causal = Store::open(
        fixture.data("causal").join("shenko.db"), fixture.data("causal").join("cas"),
    )?;
    let diverged = Store::open(
        fixture.data("diverged").join("shenko.db"), fixture.data("diverged").join("cas"),
    )?;
    for (store, name) in [(&receiver, "e1-follower"), (&causal, "e1-causal"), (&diverged, "e1-diverged")] {
        store.import_cognition_archive(&initial[..], &CognitionImportOptions::default())?;
        store.attach_commit_authority(Arc::new(NativeT2Authority {
            source: name, stamp: None, state: Arc::clone(&state),
        }))?;
    }
    source.commit_step_and_advance_head(&step, &head, next)?;
    let birth = latest(&state)?;
    for store in [&receiver, &causal, &diverged] {
        assert_eq!(store.recover_control_admission(
            &birth,
            &source.admission_objects(&birth.admission.payload_ref)?,
            false
        )?,
        ControlRecoveryOutcome::RecoveredCanonical
        );
    }
    assert_ne!(source.ledger.ledger_id(), receiver.ledger.ledger_id());
    let parked_root = receiver.external_task(head.subject_root_task_id)?;
    assert!(matches!(
        parked_root.dispatch_eligibility,
        DispatchEligibility::Parked { .. }
    ));
    let implementation = ImplementationRef {
        plugin_id: EntityId::new(),
        artifact_digest: Digest::blake3(b"e1 file append fixture"),
        registry_revision: 1,
    };
    let mut claims = Vec::new();
    for task in &task_ids[..2] {
        let claim = source
            .claim_native_task(*task, implementation.clone(), false)?
            .context("fresh original claim")?;
        let grant = latest(&state)?;
        for store in [&receiver, &causal, &diverged] {
            assert_eq!(store.recover_control_admission(
                &grant,
                &source.admission_objects(&grant.admission.payload_ref)?,
                false
            )?,
            ControlRecoveryOutcome::RecoveredCanonical
            );
        }
        claims.push(claim);
    }
    let mut originals = Vec::new();
    for claim in &claims {
        let (ctx, mut operation, mut attempt) =
            draft_dispatch_operation(&source, &claim.invocation, "first-e1", claim.operation_id)?;
        assert!(
            source
                .arm_native_operation_for_dispatch(&ctx, &mut operation, &mut attempt)?
                .is_none()
        );
        let arm = latest(&state)?;
        for store in [&causal, &diverged] {
            assert_eq!(store.recover_control_admission(
                &arm, &source.admission_objects(&arm.admission.payload_ref)?, false,
            )?, ControlRecoveryOutcome::RecoveredCanonical);
        }
        originals.push((ctx, operation, attempt, arm));
    }
    let mut parked_archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut parked_archive)?;
    let parked = Store::open(
        fixture.data("parked").join("shenko.db"),
        fixture.data("parked").join("cas"),
    )?;
    parked.import_cognition_archive(&parked_archive[..], &CognitionImportOptions::default())?;
    let parked_task = parked.external_task(task_ids[0])?;
    assert!(matches!(
        parked_task.dispatch_eligibility,
        DispatchEligibility::Parked { .. }
    ));
    let mut receipts = Vec::new();
    for (index, (_, operation, attempt, _)) in originals.iter().enumerate() {
        let mut effects = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(fixture.private().join("effects"))?;
        effects.write_all(b"effect\n")?;
        effects.sync_all()?;
        let raw = source
            .cas
            .seal_object(
                if index == 0 {
                    b"effect observed"
                } else {
                    b"connection lost"
                },
                "text/plain",
                "native-e1-fixture",
                "raw",
                1,
            )?
            .object;
        let receipt = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "file-observer".into(),
            source_instance_id: "original-executor".into(),
            source_event_key: Some(attempt.attempt_id.to_string()),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(attempt.implementation_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: (index == 0).then_some(ObservationLevel::PostconditionVerified),
            normalized_ref: None,
            observed_at: Some(chrono::Utc::now()),
            received_commit: attempt.armed_commit,
        };
        source
            .record_external_evidence_if_current(
                operation.operation_id,
                attempt.attempt_id,
                receipt.clone(),
                (index == 0).then_some(OperationOutcome::Succeeded),
                if index == 0 {
                    EffectStatus::Complete
                } else {
                    EffectStatus::Unknown
                },
                shenko_store::external::ExternalEvidenceFold {
                    expected_revision: RecordRevision(0),
                    terminal_content_conflict: false,
                    source_operation_revision: None,
                },
            )?
            .context("first fold")?;
        receipts.push(source.external_evidence(receipt.receipt_id)?);
    }
    // A real source S1 accepts uncertainty. This decision is not part of the
    // E1 seal and must not become a receiver's acknowledgement.
    let docket = source.open_reconciliation_docket(agent, 8)?;
    assert_eq!(docket.len(), 1);
    let source_case = docket[0].case.case_id;
    let (current_head, current_step) = source.load_subject(agent)?;
    let docket_json = docket
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let recorded =
        recorded_result_with_docket(&source, &current_head, current_step, None, &docket_json)?;
    let successor = EntityId::new();
    let mut acknowledgement = committed_candidate_with(&source, recorded, successor, vec![]);
    let mut decision: StepDecision = serde_json::from_slice(
        &source.cas.read_object(
            &acknowledgement
                .decision_ref
                .as_ref()
                .context("ack decision")?
                .digest,
        )?,
    )?;
    // The sealed S0 above carries this case; acknowledging it requires the
    // decision schema that supports X11, while schema 8 remains historical.
    decision.schema_version = shenko_store::channel::RECONCILIATION_DECISION_SCHEMA_VERSION;
    decision
        .unknown_acknowledgements
        .push(ReconciliationAcknowledgement {
            case_id: source_case,
            acknowledgement_id: EntityId::new(),
            reason_refs: vec![receipts[1].receipt_id],
            remaining_impact: "Keep the unknown effect; seek a later independent observation"
                .into(),
        });
    acknowledgement.decision_ref = Some(
        source
            .cas
            .seal_object(
                &serde_json::to_vec(&decision)?,
                "application/json",
                "test",
                "StepDecision",
                decision.schema_version,
            )?
            .object,
    );
    source.commit_step_and_advance_head(&acknowledgement, &current_head, successor)?;
    assert_eq!(
        source
            .reconciliation_case(source_case)?
            .context("source case")?
            .state,
        ReconciliationState::AcknowledgedUnknown
    );
    let mut source_node = shenko::node_sync::open_node(&fixture.data("source"), &source)?;
    let source_id = source_node.local_node_id()?.context("source Node")?;
    let identity = source_node.local_identity()?.context("source identity")?;
    let mut receiver_node = shenko::node_sync::open_node(&fixture.data("receiver"), &receiver)?;
    let receiver_id = receiver_node.local_node_id()?.context("receiver Node")?;
    receiver_node.register_sync_peer(&peer(&fixture, "source", source_id))?;
    source_node.register_sync_peer(&peer(&fixture, "receiver", receiver_id))?;
    for (name, store) in [("causal", &causal), ("diverged", &diverged)] {
        let mut node = shenko::node_sync::open_node(&fixture.data(name), store)?;
        node.register_sync_peer(&peer(&fixture, "source", source_id))?;
        node.register_sync_peer(&peer(&fixture, "receiver", receiver_id))?;
    }
    fixture.prepare_ssh()?;
    let page_request = |cursor| Request::NativeEvidencePage {
        subject_id: agent,
        origin_proof: identity.origin_proof.clone(),
        scope: NATIVE_EVIDENCE_SCOPE.into(),
        cursor,
        page_items: Some(1),
    };
    let page = fixture.remote_page(&page_request(None))?;
    assert_eq!(page.items.len(), 1);
    let next_page = fixture.remote_page(&page_request(Some(
        page.next
            .clone()
            .context("truncated page must provide cursor")?,
    )))?;
    assert_eq!(next_page.cursor.through, page.cursor.through);
    assert_eq!(next_page.items.len(), 1);
    assert!(next_page.next.is_none());
    for original_page in [&page, &next_page] {
        assert_eq!(original_page.source.ledger_id, source.ledger.ledger_id());
        assert!(
            original_page
                .items
                .iter()
                .all(|item| { item.source_commit.ledger_id == original_page.source.ledger_id })
        );
    }
    assert_eq!(
        vec![
            page.items[0].position.receipt_id,
            next_page.items[0].position.receipt_id
        ],
        receipts
            .iter()
            .map(|receipt| receipt.receipt_id)
            .collect::<Vec<_>>()
    );
    fs::write(
        fixture.private().join("lose-chunk"),
        b"one lost authenticated chunk",
    )?;
    let (lost, _) = fixture.cli_sync(source_id)?;
    assert!(
        !lost.success(),
        "lost authenticated chunk must not report an applied E1"
    );
    let connection = rusqlite::Connection::open(fixture.data("receiver").join("shenko.db"))?;
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM native_evidence_deliveries WHERE state='pending'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    assert!(
        receiver
            .external_operation_evidence(originals[0].1.operation_id)?
            .is_empty()
    );
    // Retention observation of the existing interrupted exchange: its durable
    // descriptor is a root even before its CAS bytes or application arrive.
    let pending_roots = receiver.retained_object_snapshot(RetentionLimits::default())?;
    let NativeEvidenceAvailability::Sealed { evidence_ref: pending_ref } = &page.items[0].availability else {
        anyhow::bail!("the interrupted exchange must name its original seal");
    };
    assert!(!pending_roots.complete);
    assert!(pending_roots.references.iter().any(|reference|
        reference.required && reference.digest == pending_ref.digest));
    assert!(pending_roots.gaps.iter().any(|gap|
        gap.kind == "referenced_object_unreadable" && gap.digest.as_ref() == Some(&pending_ref.digest)));
    append_json(&fixture.logs().join("retained-object-observations.jsonl"),
        &json!({"stage":"pending_before_original_chunk","snapshot":pending_roots}))?;
    // A new CLI process retries the durable pending manifest, then stops at
    // the receiver's missing actual T1 rather than accepting peer markers.
    let (blocked, blocked_report) = fixture.cli_sync(source_id)?;
    assert!(!blocked.success(), "{blocked_report}");
    assert_eq!(blocked_report["reconciliation_required"], 2);
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM native_evidence_materializations",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    for (_, _, _, arm) in &originals {
        assert_eq!(
            receiver.recover_control_admission(
                arm,
                &source.admission_objects(&arm.admission.payload_ref)?,
                false
            )?,
            ControlRecoveryOutcome::RecoveredCanonical
        );
    }
    let (applied, applied_report) = fixture.cli_sync(source_id)?;
    assert!(
        applied.success(),
        "{applied_report}; logs {}",
        fixture.logs().display()
    );
    assert_eq!(applied_report["applied"], 2);
    for (index, (_, operation, attempt, _)) in originals.iter().enumerate() {
        assert_eq!(
            receiver.external_evidence(receipts[index].receipt_id)?,
            receipts[index]
        );
        assert_eq!(
            serde_json::to_value(receiver.external_operation(operation.operation_id)?)?,
            serde_json::to_value(source.external_operation(operation.operation_id)?)?
        );
        assert_eq!(
            serde_json::to_value(receiver.external_attempt(attempt.attempt_id)?)?,
            serde_json::to_value(source.external_attempt(attempt.attempt_id)?)?
        );
        assert!(receiver.native_task_result(task_ids[index])?.is_none());
    }
    let receiver_case = receiver
        .reconciliation_cases_for_operation(originals[1].1.operation_id)?
        .pop()
        .context("receiver Unknown case")?;
    assert_ne!(receiver_case.case_id, source_case);
    assert_eq!(receiver_case.state, ReconciliationState::Open);
    assert!(receiver_case.decision_ref.is_none());
    assert_eq!(
        receiver_case.created_commit.ledger_id,
        receiver.ledger.ledger_id()
    );
    let marker: String = connection.query_row(
        "SELECT data FROM native_evidence_materializations WHERE receipt_id=?1",
        [receipts[0].receipt_id.to_string()],
        |row| row.get(0),
    )?;
    let marker: Value = serde_json::from_str(&marker)?;
    assert_eq!(
        marker["logical_result"],
        serde_json::to_value(receipts[0].received_commit)?
    );
    assert_eq!(
        marker["committed_at"]["ledger_id"],
        serde_json::to_value(receiver.ledger.ledger_id())?
    );
    assert_ne!(marker["logical_result"], marker["committed_at"]);
    let NativeEvidenceAvailability::Sealed { evidence_ref } = &page.items[0].availability else {
        anyhow::bail!("source first E1 lacks seal")
    };
    let delivered_objects = receiver_node.admission_objects(evidence_ref)?;
    let parked_delivery = parked.queue_native_evidence_delivery(&NativeEvidenceDelivery {
        source: page.source.clone(),
        item: page.items[0].clone(),
    })?;
    assert!(matches!(
        parked.receive_native_evidence(parked_delivery.delivery_id, &delivered_objects)?,
        NativeEvidenceDeliveryOutcome::ReconciliationRequired { .. }
    ));
    assert_eq!(
        serde_json::to_value(parked.external_task(task_ids[0])?)?,
        serde_json::to_value(parked_task)?
    );
    assert!(
        parked
            .external_operation_evidence(originals[0].1.operation_id)?
            .is_empty()
    );
    let mut malformed = page.items[0].clone();
    let NativeEvidenceAvailability::Sealed { evidence_ref } = &mut malformed.availability else {
        anyhow::bail!("missing original seal")
    };
    evidence_ref.codec.major = 99;
    let refused = receiver.queue_native_evidence_delivery(&NativeEvidenceDelivery {
        source: page.source.clone(),
        item: malformed,
    });
    assert!(
        matches!(&refused, Err(error) if error.to_string().contains("unsupported typed object")),
        "{refused:#?}"
    );
    // The later local observation now has its own successor seal. Replaying
    // the first original must still not rewind that newer projection.
    let mut later = receipts[1].clone();
    later.receipt_id = EntityId::new();
    later.source_event_key = Some("later-local-check".into());
    later.source_instance_id = "receiver-postcondition-reader".into();
    later.raw_ref = receiver
        .cas
        .seal_object(
            b"later postcondition observed",
            "text/plain",
            "native-e1-fixture",
            "raw",
            1,
        )?
        .object;
    let later_id = later.receipt_id;
    receiver.record_external_evidence(
        originals[1].1.operation_id,
        originals[1].2.attempt_id,
        later,
        Some(OperationOutcome::Succeeded),
        EffectStatus::Complete,
    )?;
    let advanced = receiver.external_operation(originals[1].1.operation_id)?;
    let watermark = receiver.current_commit_seq()?;
    let (replayed, replay_report) = fixture.cli_sync(source_id)?;
    assert!(replayed.success(), "{replay_report}");
    assert_eq!(replay_report["already_applied"], 2);
    assert_eq!(receiver.current_commit_seq()?, watermark);
    assert_eq!(
        serde_json::to_value(receiver.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&advanced)?
    );
    assert_eq!(
        receiver
            .reconciliation_case(receiver_case.case_id)?
            .context("local case remains")?
            .state,
        ReconciliationState::Resolved
    );
    assert_eq!(
        source
            .reconciliation_case(source_case)?
            .context("source decision remains")?
            .state,
        ReconciliationState::AcknowledgedUnknown
    );

    // Successor window 1: only B's actual later source E1 is advertised. A's
    // first receipt remains A's fact, even though B really materialized it.
    let later = receiver.external_evidence(later_id)?;
    let successor_page = fixture.remote_page_from("receiver", &page_request(None))?;
    assert_eq!(successor_page.schema_version, NATIVE_EVIDENCE_SCHEMA_VERSION);
    assert_eq!(successor_page.scope, NATIVE_EVIDENCE_SCOPE);
    assert_eq!(successor_page.source.node_id, receiver_id);
    assert_eq!(successor_page.source.ledger_id, receiver.ledger.ledger_id());
    assert_eq!(successor_page.items.len(), 1);
    assert!(successor_page.next.is_none());
    let successor_item = &successor_page.items[0];
    assert_eq!(successor_item.position.receipt_id, later_id);
    assert_eq!(successor_item.source_commit, later.received_commit);
    assert_ne!(later.received_commit.ledger_id, receipts[1].received_commit.ledger_id);
    let NativeEvidenceAvailability::Sealed { evidence_ref: successor_ref } = &successor_item.availability else {
        anyhow::bail!("later definite observation lacks its source seal")
    };
    assert_eq!(successor_ref.codec.major, 3);
    let successor_fact: Value = serde_json::from_slice(&receiver.cas.read_object(&successor_ref.digest)?)?;
    let NativeEvidenceAvailability::Sealed { evidence_ref: first_unknown_ref } = &next_page.items[0].availability else {
        anyhow::bail!("original Unknown lacks its source seal")
    };
    assert_eq!(successor_fact["predecessor_ref"], serde_json::to_value(first_unknown_ref)?);
    assert_eq!(successor_fact["receipt"], serde_json::to_value(&later)?);
    assert_eq!(successor_fact["operation"], serde_json::to_value(&advanced)?);

    // Successor window 2: the old scope must refuse explicitly, even on a
    // valid authenticated route; it cannot report a first-only scan complete.
    let old_request = Request::NativeEvidencePage {
        subject_id: agent,
        origin_proof: identity.origin_proof.clone(),
        scope: "canonical_native_first_observation.v1".into(),
        cursor: None,
        page_items: None,
    };
    let old_scope_serial = fixture.counter.get();
    let (old_scope, _) = fixture.remote_response("receiver", &old_request)?;
    let old_scope_error = fs::read_to_string(fixture.logs().join(
        format!("{old_scope_serial:03}-receiver-page.stderr"),
    ))?;
    assert!(!old_scope.success() && old_scope_error.contains("unsupported native E1 requested scope"),
        "old first-only scope must be explicitly refused: {old_scope}: {old_scope_error}");

    // Successor window 3: a genuine T1 is insufficient without this Store's
    // actual first-E1 application. Closure bytes do not install that receipt.
    let (out_of_order, out_of_order_report) = fixture.cli_sync_from("causal", receiver_id)?;
    assert!(!out_of_order.success(), "{out_of_order_report}");
    assert_eq!(out_of_order_report["reconciliation_required"], 1);
    let (causal_debts, causal_debt_count) = causal.native_evidence_delivery_debts()?;
    assert_eq!(causal_debt_count, 1);
    assert!(causal_debts[0].reason.as_deref().is_some_and(|reason|
        reason.contains("first Unknown's actual local receipt")), "{causal_debts:#?}");
    assert!(causal.external_operation_evidence(originals[1].1.operation_id)?.is_empty());
    assert_eq!(serde_json::to_value(causal.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&originals[1].1)?);
    let (first_arrived, first_arrived_report) = fixture.cli_sync_from("causal", source_id)?;
    assert!(first_arrived.success(), "{first_arrived_report}");
    assert_eq!(first_arrived_report["applied"], 2);
    let (ordered, ordered_report) = fixture.cli_sync_from("causal", receiver_id)?;
    assert!(ordered.success(), "{ordered_report}");
    assert_eq!(ordered_report["applied"], 1);
    assert_eq!(ordered_report["retained_delivery_debts"], 0);
    assert_eq!(causal.external_evidence(later_id)?, later);

    // Successor window 4: another real local definite observation is not
    // replaced by B's equally well-formed but causally competing successor.
    let (diverged_first, diverged_first_report) = fixture.cli_sync_from("diverged", source_id)?;
    assert!(diverged_first.success(), "{diverged_first_report}");
    assert_eq!(diverged_first_report["applied"], 2);
    let mut local_observation = receipts[1].clone();
    local_observation.receipt_id = EntityId::new();
    local_observation.source_instance_id = "other-postcondition-observer".into();
    local_observation.source_event_key = Some("independent-postcondition".into());
    local_observation.observed_at = Some(chrono::Utc::now());
    local_observation.observation_level = Some(ObservationLevel::PostconditionVerified);
    assert_eq!(fs::read(fixture.private().join("effects"))?, b"effect\neffect\n");
    local_observation.raw_ref = diverged.cas.seal_object(
        b"independent observer read two original effects", "text/plain", "native-e1-fixture", "raw", 1,
    )?.object;
    let local_observation_id = local_observation.receipt_id;
    diverged.record_external_evidence(originals[1].1.operation_id, originals[1].2.attempt_id,
        local_observation, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
    let divergent_head = diverged.external_operation(originals[1].1.operation_id)?;
    let divergent_receipt = diverged.external_evidence(local_observation_id)?;
    let (competing, competing_report) = fixture.cli_sync_from("diverged", receiver_id)?;
    assert!(!competing.success(), "{competing_report}");
    assert_eq!(competing_report["reconciliation_required"], 1);
    let (divergent_debts, divergent_debt_count) = diverged.native_evidence_delivery_debts()?;
    assert_eq!(divergent_debt_count, 1);
    assert!(divergent_debts[0].reason.as_deref().is_some_and(|reason|
        reason.contains("cannot overwrite a different or later local observation")), "{divergent_debts:#?}");
    assert_eq!(serde_json::to_value(diverged.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&divergent_head)?);
    assert_eq!(diverged.external_evidence(local_observation_id)?, divergent_receipt);
    assert!(!diverged.external_operation_evidence(originals[1].1.operation_id)?.iter()
        .any(|receipt| receipt.receipt_id == later_id));

    // Successor window 5: the reverse authenticated download can stop before
    // any chunk reply. The original acknowledgement and Unknown stay intact
    // until a new CLI process finishes factual application atomically.
    let source_connection = rusqlite::Connection::open(fixture.data("source").join("shenko.db"))?;
    let source_ack: (String, i64) = source_connection.query_row(
        "SELECT data,updated_commit_seq FROM reconciliation_cases WHERE case_id=?1",
        [source_case.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    fs::write(fixture.private().join("lose-chunk"), b"interrupt reverse successor download")?;
    let (reverse_lost, _) = fixture.cli_sync_from("source", receiver_id)?;
    assert!(!reverse_lost.success(), "interrupted successor transfer cannot report application");
    assert!(!fixture.private().join("lose-chunk").exists(), "authenticated chunk fault was not reached");
    assert_eq!(source.external_operation(originals[1].1.operation_id)?.effect_status, EffectStatus::Unknown);
    assert_eq!(source_connection.query_row(
        "SELECT count(*) FROM native_evidence_deliveries WHERE receipt_id=?1 AND state='pending'",
        [later_id.to_string()], |row| row.get::<_, i64>(0),
    )?, 1);
    let (reverse, reverse_report) = fixture.cli_sync_from("source", receiver_id)?;
    assert!(reverse.success(), "{reverse_report}");
    assert_eq!(reverse_report["applied"], 1);
    assert_eq!(source.external_evidence(later_id)?, later);
    assert_eq!(serde_json::to_value(source.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&advanced)?);
    assert_eq!(serde_json::to_value(source.external_attempt(originals[1].2.attempt_id)?)?,
        serde_json::to_value(receiver.external_attempt(originals[1].2.attempt_id)?)?);
    let original_created: String = connection.query_row(
        "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
        [later_id.to_string()], |row| row.get(0),
    )?;
    let received_created: String = source_connection.query_row(
        "SELECT created_at FROM evidence_receipts WHERE receipt_id=?1",
        [later_id.to_string()], |row| row.get(0),
    )?;
    assert_eq!(received_created, original_created);
    let ack_after: (String, i64) = source_connection.query_row(
        "SELECT data,updated_commit_seq FROM reconciliation_cases WHERE case_id=?1",
        [source_case.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(ack_after, source_ack, "the source's original acknowledgement must stay byte-for-byte");
    let cases = source.reconciliation_cases_for_operation(originals[1].1.operation_id)?;
    assert_eq!(cases.len(), 2);
    let resolution = cases.iter().find(|case| case.state == ReconciliationState::Resolved)
        .context("new local resolution after the unchanged acknowledgement")?;
    assert_ne!(resolution.case_id, source_case);
    assert!(resolution.decision_ref.is_none());
    assert_eq!(resolution.evidence_refs, vec![later_id]);
    assert_eq!(resolution.created_commit.ledger_id, source.ledger.ledger_id());
    let successor_marker: String = source_connection.query_row(
        "SELECT data FROM native_evidence_materializations WHERE receipt_id=?1",
        [later_id.to_string()], |row| row.get(0),
    )?;
    let successor_marker: Value = serde_json::from_str(&successor_marker)?;
    assert_eq!(successor_marker["logical_result"], serde_json::to_value(later.received_commit)?);
    assert_eq!(successor_marker["committed_at"], serde_json::to_value(resolution.created_commit)?);
    assert_ne!(successor_marker["logical_result"], successor_marker["committed_at"]);

    // Successor window 6: a supported codec still cannot name a different
    // first observation. This is malformed typed input, not catch-up debt.
    let NativeEvidenceAvailability::Sealed { evidence_ref: definite_ref } = &page.items[0].availability else {
        anyhow::bail!("definite first observation lacks its source seal")
    };
    let mut forged_fact = successor_fact.clone();
    forged_fact["predecessor_ref"] = serde_json::to_value(definite_ref)?;
    let forged_ref = source.cas.seal_object(&serde_json::to_vec(&forged_fact)?,
        "application/json", "commit-authority", "store-native-evidence-fact", 3)?.object;
    let mut forged_item = successor_item.clone();
    forged_item.availability = NativeEvidenceAvailability::Sealed { evidence_ref: forged_ref };
    let forged = source.queue_native_evidence_delivery(&NativeEvidenceDelivery {
        source: successor_page.source.clone(), item: forged_item,
    })?;
    let malformed_result = source.receive_native_evidence(forged.delivery_id, &[]);
    assert!(matches!(&malformed_result, Err(shenko_types::error::ShenkoError::Other(reason))
        if reason.contains("causal predecessor")), "{malformed_result:#?}");
    assert_eq!(serde_json::to_value(source.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&advanced)?);
    assert_eq!(source.external_evidence(later_id)?, later);
    assert_eq!(serde_json::to_value(source.native_evidence_delivery(forged.delivery_id)?)?["state"], "pending");

    // A lost derived seal keeps the real late E1; the source page exposes a
    // gap and the receiver retains it instead of reporting full replication.
    let third = source
        .claim_native_task(task_ids[2], implementation, false)?
        .context("third original claim")?;
    let (third_ctx, mut third_op, mut third_attempt) = draft_dispatch_operation(
        &source,
        &third.invocation,
        "missing-proof",
        third.operation_id,
    )?;
    assert!(
        source
            .arm_native_operation_for_dispatch(&third_ctx, &mut third_op, &mut third_attempt)?
            .is_none()
    );
    let third_arm = latest(&state)?;
    let envelope: Value = serde_json::from_slice(
        &source
            .cas
            .read_object(&third_arm.admission.payload_ref.digest)?,
    )?;
    let transition: shenko_types::object::ObjectRef =
        serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    // This source is still complete before the existing proof-loss fault.
    // Bind the later Object-origin expectation to the actual arm envelope.
    let before_proof_loss = source.current_commit_seq()?;
    let objects_before_proof_loss = source.cas.object_digests()?;
    let intact = source.retained_object_snapshot(RetentionLimits::default())?;
    assert!(intact.complete, "retention gaps before proof loss: {:?}", intact.gaps);
    assert_eq!(intact.ledger_id, source.ledger.ledger_id());
    assert_eq!(intact.source_through.0, before_proof_loss);
    assert!(intact.verified_objects.contains(&transition.digest));
    assert!(intact.references.iter().any(|reference|
        reference.required && reference.digest == transition.digest
            && matches!(&reference.origin,
                shenko_store::retained_objects::RetentionOrigin::Object { digest }
                    if digest == &third_arm.admission.payload_ref.digest)));
    append_json(&fixture.logs().join("retained-object-observations.jsonl"),
        &json!({"stage":"retained_before_proof_loss","store":"source","snapshot":intact}))?;
    assert_eq!(source.current_commit_seq()?, before_proof_loss);
    assert_eq!(source.cas.object_digests()?, objects_before_proof_loss);
    fs::remove_file(source.cas.verified_object_path(&transition)?)?;
    let mut gap_receipt = receipts[1].clone();
    gap_receipt.receipt_id = EntityId::new();
    gap_receipt.operation_id = Some(third_op.operation_id);
    gap_receipt.attempt_id = Some(third_attempt.attempt_id);
    gap_receipt.source_event_key = Some("missing-derived-proof".into());
    source.record_external_evidence(
        third_op.operation_id,
        third_attempt.attempt_id,
        gap_receipt.clone(),
        None,
        EffectStatus::Unknown,
    )?;
    assert_eq!(
        source
            .native_evidence_seal_debts(third_op.operation_id)?
            .len(),
        1
    );
    let (gap, gap_report) = fixture.cli_sync(source_id)?;
    assert!(!gap.success(), "{gap_report}");
    assert_eq!(gap_report["already_applied"], 2);
    assert_eq!(gap_report["reconciliation_required"], 1);
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM native_evidence_deliveries WHERE state='gap'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    for (ctx, operation, attempt, _) in &originals {
        assert!(
            receiver
                .arm_native_operation_for_dispatch(
                    ctx,
                    &mut operation.clone(),
                    &mut attempt.clone()
                )?
                .is_some()
        );
        assert_eq!(
            receiver.commit_native_dispatch(ctx, operation.operation_id, attempt.attempt_id)?,
            shenko_store::child::NativeDispatchTransition::AlreadyCommitted
        );
    }
    assert_eq!(
        serde_json::to_value(receiver.external_task(head.subject_root_task_id)?)?,
        serde_json::to_value(parked_root)?
    );
    assert_eq!(
        fs::read(fixture.private().join("effects"))?,
        b"effect\neffect\n"
    );
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM native_evidence_materializations",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        3
    );
    // The original two remote applications remain two. The additional local
    // receipt seals this installation's real later observation; it is not a
    // third application or a replayed effect from the original source.
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM native_evidence_materializations WHERE json_extract(data,'$.logical_result.ledger_id')=?1",
            [source.ledger.ledger_id().to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );

    // Successor window 7: a still later local fact is outside codec 3's
    // bounded fold. Replaying B's already applied successor must preserve it
    // and the original acknowledgement, including the pending malformed debt.
    let mut after_resolution = later.clone();
    after_resolution.receipt_id = EntityId::new();
    after_resolution.source_instance_id = "source-post-resolution-reader".into();
    after_resolution.source_event_key = Some("after-original-resolution".into());
    after_resolution.observed_at = Some(chrono::Utc::now());
    after_resolution.raw_ref = source.cas.seal_object(
        b"original two effects remain visible after resolution", "text/plain", "native-e1-fixture", "raw", 1,
    )?.object;
    let after_resolution_id = after_resolution.receipt_id;
    source.record_external_evidence(originals[1].1.operation_id, originals[1].2.attempt_id,
        after_resolution, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
    let newest_head = source.external_operation(originals[1].1.operation_id)?;
    let newest_receipt = source.external_evidence(after_resolution_id)?;
    let replay_watermark = source.current_commit_seq()?;
    let (successor_replayed, successor_replay_report) = fixture.cli_sync_from("source", receiver_id)?;
    assert!(!successor_replayed.success(), "malformed pending debt must remain visible");
    assert_eq!(successor_replay_report["already_applied"], 1);
    assert_eq!(successor_replay_report["applied"], 0);
    assert_eq!(successor_replay_report["retained_delivery_debts"], 1);
    assert_eq!(source.current_commit_seq()?, replay_watermark);
    assert_eq!(serde_json::to_value(source.external_operation(originals[1].1.operation_id)?)?,
        serde_json::to_value(&newest_head)?);
    assert_eq!(source.external_evidence(after_resolution_id)?, newest_receipt);
    let ack_replayed: (String, i64) = source_connection.query_row(
        "SELECT data,updated_commit_seq FROM reconciliation_cases WHERE case_id=?1",
        [source_case.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(ack_replayed, source_ack);
    for original in &receipts {
        assert_eq!(source.external_evidence(original.receipt_id)?, *original);
        assert_eq!(receiver.external_evidence(original.receipt_id)?, *original);
    }
    for store in [source.as_ref(), &receiver, &causal, &diverged] {
        for (ctx, operation, attempt, _) in &originals {
            assert!(store.arm_native_operation_for_dispatch(ctx, &mut operation.clone(), &mut attempt.clone())?.is_some());
            assert_eq!(store.commit_native_dispatch(ctx, operation.operation_id, attempt.attempt_id)?,
                shenko_store::child::NativeDispatchTransition::AlreadyCommitted);
            assert!(store.native_task_result(operation.task_id)?.is_none());
        }
    }
    assert_eq!(fs::read(fixture.private().join("effects"))?, b"effect\neffect\n");
    // Preserve the full first/successor/ack/late-fact audit. The source must
    // report the deliberately removed third-arm proof; the receiver remains
    // complete. These reads do not validate delivery semantics or grant a GC permit.
    for (name, store) in [("source", source.as_ref()), ("receiver", &receiver)] {
        let before = store.current_commit_seq()?;
        let objects = store.cas.object_digests()?;
        let snapshot = store.retained_object_snapshot(RetentionLimits::default())?;
        if name == "source" {
            assert!(!snapshot.complete, "the removed arm proof must remain visible");
            assert_eq!(snapshot.omitted_gap_count, 0);
            assert_eq!(snapshot.gaps.len(), 4, "{:?}", snapshot.gaps);
            for gap in &snapshot.gaps {
                assert_eq!(gap.kind, "referenced_object_unreadable");
                assert_eq!(gap.digest.as_ref(), Some(&transition.digest));
            }
            for (table, column) in [
                ("control_materializations", "data"),
                ("control_prepared_transitions", "transition_ref"),
                ("inbox_events", "data"),
            ] {
                assert_eq!(snapshot.gaps.iter().filter(|gap| matches!(&gap.origin,
                    shenko_store::retained_objects::RetentionOrigin::Row {
                        table: found_table, column: found_column, ..
                    } if found_table == table && found_column == column)).count(),
                    1, "missing proof origin {table}.{column}: {:?}", snapshot.gaps);
            }
            assert_eq!(snapshot.gaps.iter().filter(|gap| matches!(&gap.origin,
                shenko_store::retained_objects::RetentionOrigin::Object { digest }
                    if digest == &third_arm.admission.payload_ref.digest)).count(),
                1, "missing original arm envelope origin: {:?}", snapshot.gaps);
            assert!(!snapshot.verified_objects.contains(&transition.digest));
        } else {
            assert!(snapshot.complete, "retention gaps for {name}: {:?}", snapshot.gaps);
        }
        assert_eq!(snapshot.ledger_id, store.ledger.ledger_id());
        assert_eq!(snapshot.source_through.0, before);
        for receipt in receipts.iter().chain(std::iter::once(&later)) {
            assert!(snapshot.verified_objects.contains(&receipt.raw_ref.digest));
        }
        for item in page.items.iter().chain(&next_page.items).chain(&successor_page.items) {
            if let NativeEvidenceAvailability::Sealed { evidence_ref } = &item.availability {
                assert!(snapshot.verified_objects.contains(&evidence_ref.digest));
            }
        }
        assert!(snapshot.references.iter().any(|reference| matches!(&reference.origin,
            shenko_store::retained_objects::RetentionOrigin::Row { table, .. }
                if table == "native_evidence_materializations")));
        append_json(&fixture.logs().join("retained-object-observations.jsonl"),
            &json!({"stage":"retained_after_successor","store":name,"snapshot":snapshot}))?;
        assert_eq!(store.current_commit_seq()?, before);
        assert_eq!(store.cas.object_digests()?, objects);
    }
    // Exercise the public zero-touch audit command against the existing
    // receiver, without supplying a bootstrap home or opening a new writer.
    let before_audit = receiver.current_commit_seq()?;
    let mut audit_command = fixture.command(env!("CARGO_BIN_EXE_shenko"))?;
    audit_command.arg("--data-dir").arg(fixture.data("receiver"))
        .args(["check", "--retained-objects"]);
    let (audit_status, audit_bytes) = fixture.run(&mut audit_command, "check-retained-objects", None)?;
    let audit: Value = serde_json::from_slice(&audit_bytes)?;
    assert!(audit_status.success(), "{audit}");
    assert_eq!(audit["scope"], "single_committed_database");
    assert_eq!(audit["complete"], true);
    assert_eq!(audit["ledger_id"], serde_json::to_value(receiver.ledger.ledger_id())?);
    assert_eq!(receiver.current_commit_seq()?, before_audit);
    assert_eq!(fs::read(fixture.private().join("effects"))?, b"effect\neffect\n");
    append_json(
        &fixture.logs().join("observations.jsonl"),
        &json!({"applied":applied_report,"replayed":replay_report,"gap":gap_report,
        "source_ledger":source.ledger.ledger_id(),"receiver_ledger":receiver.ledger.ledger_id(),"receipts":receipts,
        "local_case":receiver_case,"physical_effect_count":2,"chosen_t2_count":0,
        "successor":{"page":successor_page,"source_receipt":later,"out_of_order":out_of_order_report,
            "ordered":ordered_report,"competing":competing_report,"reverse":reverse_report,
            "original_ack_data":source_ack.0,"local_resolution":resolution,"materialization":successor_marker,
            "malformed_delivery":forged.delivery_id,"replay_after_local_fact":successor_replay_report,
            "later_local_receipt":newest_receipt,"distinct_new_windows":7}}),
    )?;
    fixture.capture_originals()?;
    Ok(())
}
