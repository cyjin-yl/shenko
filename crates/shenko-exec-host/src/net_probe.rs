//! `net.probe` — one bounded observation of whether *this* node's path to a
//! declared endpoint currently works. A reachability question is answered by
//! this attempt's measured result, never by an assumption carried over from an
//! earlier moment.
//!
//! The probe is deliberately narrow: DNS resolution, TCP connect, and for
//! `http` one status line. It sends no request body, follows no redirect, keeps
//! no cookies, presents no credentials and reads no response body, so it can
//! only ever learn "this path works right now" or why it does not.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    effect::{
        CheckpointContract, EffectContract, EffectContractBuilder, ExternalEffect, GuaranteeBasis,
        IdempotencyGuarantee, LifecycleContract, ResourceAccess,
    },
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskRecord},
};
use std::{
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpStream, ToSocketAddrs as _},
    sync::Arc,
    time::{Duration, Instant},
};

pub const KIND: &str = "net.probe";
pub const CONTRACT: &[u8] = b"shenko.net.probe/v1: one bounded reachability observation of a declared endpoint from this node (DNS, TCP connect, one HTTP status line); no body, no redirect, no credentials, no persistent state; reports only what this attempt measured";

pub const MAX_URL_BYTES: usize = 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;
pub const MAX_TIMEOUT_MS: u64 = 5_000;
const MAX_ADDRESSES: usize = 4;
const MAX_STATUS_LINE_BYTES: usize = 512;
const MAX_NORMALIZED_BYTES: usize = 64 * 1024;

/// What the caller asked to observe. The URL is frozen into S0 and every later
/// read compares against these bytes, so a probe cannot silently retarget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProbeInput {
    pub url: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenProbe {
    pub schema_version: u32,
    pub url: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub timeout_ms: u64,
}

/// One measured reachability fact. Absent fields mean the measurement never
/// reached that stage; they are not zero-length timings.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeObservation {
    pub schema_version: u32,
    pub url: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub reachable: bool,
    pub dns: Stage,
    pub connect: Stage,
    pub http: Option<HttpStage>,
    pub elapsed_ms: u64,
    pub error_kind: Option<String>,
    pub measured_at: chrono::DateTime<chrono::Utc>,
    pub scope: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub ok: bool,
    pub elapsed_ms: u64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpStage {
    pub status: Option<u16>,
    pub elapsed_ms: u64,
    pub detail: Option<String>,
}

impl ProbeObservation {
    /// Parse and bound the declared endpoint. Nothing is resolved or contacted
    /// here: this is the frozen identity other steps re-check.
    pub fn freeze(input: &ProbeInput) -> Result<FrozenProbe> {
        let url = input.url.trim();
        ensure!(!url.is_empty(), "probe url must not be empty");
        ensure!(url.len() <= MAX_URL_BYTES, "probe url exceeds {MAX_URL_BYTES} bytes");
        ensure!(
            (100..=MAX_TIMEOUT_MS).contains(&input.timeout_ms),
            "probe timeout_ms must be 100..={MAX_TIMEOUT_MS}"
        );
        let (scheme, rest) = url
            .split_once("://")
            .context("probe url must start with http:// or https://")?;
        let scheme = scheme.to_ascii_lowercase();
        ensure!(scheme == "http" || scheme == "https", "probe supports only http and https");
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        ensure!(!authority.is_empty(), "probe url has no host");
        ensure!(
            !path.contains('?') && !path.contains('#'),
            "probe url must not carry a query or fragment"
        );
        ensure!(
            !authority.contains('@') && !authority.contains('?') && !authority.contains('#'),
            "probe url must not carry credentials, query or fragment"
        );
        let default_port = if scheme == "https" { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && !host.contains(':') => {
                (host.to_string(), port.parse::<u16>().context("probe port is not a valid u16")?)
            }
            _ => (authority.to_string(), default_port),
        };
        ensure!(!port.eq(&0), "probe port must not be zero");
        ensure!(
            host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == ':' || c == '[' || c == ']'),
            "probe host contains characters this observer does not resolve"
        );
        Ok(FrozenProbe {
            schema_version: 1,
            url: url.to_string(),
            scheme,
            host,
            port,
            path: path.to_string(),
            timeout_ms: input.timeout_ms,
        })
    }

    /// Measure once. Blocking DNS/connect/read run on a blocking thread; the
    /// whole attempt is bounded by the declared timeout plus its own stage
    /// budgets, so a black-holed path cannot hold the child open.
    pub fn measure(frozen: &FrozenProbe) -> ProbeObservation {
        let started = Instant::now();
        let mut observation = ProbeObservation {
            schema_version: 1,
            url: frozen.url.clone(),
            scheme: frozen.scheme.clone(),
            host: frozen.host.clone(),
            port: frozen.port,
            reachable: false,
            dns: Stage { ok: false, elapsed_ms: 0, detail: None },
            connect: Stage { ok: false, elapsed_ms: 0, detail: None },
            http: None,
            elapsed_ms: 0,
            error_kind: None,
            measured_at: chrono::Utc::now(),
            scope: "one attempt from this node; no body, redirect, credentials or persistent state; success now does not prove continuity before or after",
        };

        let dns_started = Instant::now();
        let budget = Duration::from_millis(frozen.timeout_ms);
        // The resolver has no timeout of its own: a black-holed nameserver must
        // not outlive the attempt, so resolution runs on its own thread and the
        // probe stops waiting at the declared bound.
        let (resolved_tx, resolved_rx) = std::sync::mpsc::channel();
        let (host, port) = (frozen.host.clone(), frozen.port);
        std::thread::spawn(move || {
            let outcome = (host.as_str(), port)
                .to_socket_addrs()
                .map(|iter| iter.take(MAX_ADDRESSES).collect::<Vec<_>>())
                .map_err(|error| error.to_string());
            let _ = resolved_tx.send(outcome);
        });
        let addresses: Vec<SocketAddr> = match resolved_rx.recv_timeout(budget) {
            Ok(Ok(addresses)) => addresses,
            Ok(Err(error)) => {
                observation.dns = Stage { ok: false, elapsed_ms: elapsed(dns_started), detail: Some(error) };
                observation.error_kind = Some("dns_failed".into());
                observation.elapsed_ms = elapsed(started);
                return observation;
            }
            Err(_) => {
                observation.dns = Stage { ok: false, elapsed_ms: elapsed(dns_started), detail: Some("resolution exceeded the attempt bound".into()) };
                observation.error_kind = Some("dns_timeout".into());
                observation.elapsed_ms = elapsed(started);
                return observation;
            }
        };
        if addresses.is_empty() {
            observation.dns = Stage { ok: false, elapsed_ms: elapsed(dns_started), detail: Some("no addresses".into()) };
            observation.error_kind = Some("dns_empty".into());
            observation.elapsed_ms = elapsed(started);
            return observation;
        }
        observation.dns = Stage {
            ok: true,
            elapsed_ms: elapsed(dns_started),
            detail: Some(addresses.iter().map(|address| address.to_string()).collect::<Vec<_>>().join(",")),
        };

        let connect = connect_any(&addresses, budget);
        match connect {
            Ok((stream, remote, took)) => {
                observation.connect = Stage { ok: true, elapsed_ms: took, detail: Some(remote.to_string()) };
                observation.reachable = true;
                if frozen.scheme == "http" {
                    observation.http = Some(http_status_line(&stream, &frozen.host, &frozen.path, budget));
                    if observation.http.as_ref().is_some_and(|http| http.status.is_none()) {
                        observation.error_kind = Some("http_no_status".into());
                    }
                }
            }
            Err(detail) => {
                observation.connect = Stage { ok: false, elapsed_ms: elapsed(started), detail: Some(detail) };
                observation.error_kind = Some("connect_failed".into());
            }
        }
        observation.elapsed_ms = elapsed(started);
        observation
    }
}

fn connect_any(addresses: &[SocketAddr], budget: Duration) -> Result<(TcpStream, SocketAddr, u64), String> {
    let mut last = String::from("no address attempted");
    for address in addresses {
        let started = Instant::now();
        match TcpStream::connect_timeout(address, budget) {
            Ok(stream) => return Ok((stream, *address, elapsed(started))),
            Err(error) => last = format!("{address}: {error}"),
        }
    }
    Err(last)
}

/// One `GET` to the declared path, then the status line only. The connection is
/// closed without reading a body; a server that keeps the socket open cannot
/// extend this attempt past the read budget.
fn http_status_line(stream: &TcpStream, host: &str, path: &str, budget: Duration) -> HttpStage {
    let started = Instant::now();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: shenko-net-probe/1\r\n\r\n");
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => return HttpStage { status: None, elapsed_ms: elapsed(started), detail: Some(format!("clone: {error}")) },
    };
    let _ = writer.set_write_timeout(Some(budget));
    if let Err(error) = writer.write_all(request.as_bytes()) {
        return HttpStage { status: None, elapsed_ms: elapsed(started), detail: Some(format!("write: {error}")) };
    }
    let _ = stream.set_read_timeout(Some(budget));
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => return HttpStage { status: None, elapsed_ms: elapsed(started), detail: Some(format!("clone: {error}")) },
    };
    let mut line = Vec::with_capacity(64);
    let mut byte = [0_u8; 1];
    while line.len() < MAX_STATUS_LINE_BYTES {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' { break; }
                line.push(byte[0]);
            }
            Err(error) => {
                return HttpStage { status: None, elapsed_ms: elapsed(started), detail: Some(format!("read: {error}")) };
            }
        }
    }
    let text = String::from_utf8_lossy(&line);
    let status = text
        .trim_end_matches('\r')
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok());
    HttpStage {
        status,
        elapsed_ms: elapsed(started),
        detail: Some(text.trim_end_matches('\r').chars().take(160).collect()),
    }
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub struct NetProbeHandler {
    store: Arc<Store>,
}

impl NetProbeHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "5c71e2a4-8f0b-4b3e-9d62-7a1c4e8b0f25"
            .parse()
            .expect("fixed net.probe plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(bytes, "application/json", "native-net-probes", schema, 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenProbe> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "net_probe_frozen_input_mismatch"
        );
        let frozen: FrozenProbe = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_net_probe_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        frozen: &FrozenProbe,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "network.reachability.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_observation".into(),
            extra: serde_json::json!({"host": frozen.host, "port": frozen.port, "scheme": frozen.scheme}),
        };
        let resource = self.seal(&bindings, "net-probe-binding")?;
        let acceptance = self.seal(
            &"one measured reachability observation of the frozen endpoint, or an explicit measured failure",
            "net-probe-acceptance",
        )?;
        let retry = self.seal(
            &"re-measure only under the original armed attempt; a new moment is a new observation, not a replay",
            "net-probe-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef { digest: contract_digest.clone(), locator: "builtin:net-probe-v1".into() },
            symbol: "net.probe.execute".into(),
            schema: SchemaRef { name: "net-probe-observation-v1".into(), major: 1, digest: contract_digest.clone() },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "net.probe.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    fn observe(&self, operation: &OperationRecord, attempt: &AttemptRecord, frozen: &FrozenProbe) -> Result<OperationRecord> {
        let observation = ProbeObservation::measure(frozen);
        let bytes = serde_json::to_vec(&observation)?;
        let raw = self.seal_bytes(&bytes, "net-probe-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({"schema_version": 1, "status": "failed",
                    "error": {"code": "normalized_result_too_large", "message": "the exact observation is sealed in raw evidence"}}),
                "net-probe-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "net.probe.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("net-probe:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: Some(shenko_types::evidence::ObservationLevel::PostconditionVerified),
            normalized_ref: Some(normalized),
            observed_at: Some(observation.measured_at),
            received_commit: operation.prepared_commit,
        };
        // A measured negative result is still a successful observation: the
        // child succeeded at finding out. Callers distinguish reachability from
        // the operation outcome rather than treating "unreachable" as a fault.
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(OperationOutcome::Succeeded),
            EffectStatus::None,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == KIND && invocation.task.definition_version == 1,
            "net_probe_task_identity_mismatch"
        );
        let input: ProbeInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = ProbeObservation::freeze(&input)?;
        let input_ref = self.seal(&frozen, "net-probe-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, &frozen, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)? {
            return Ok(existing);
        }
        let handler = Self { store: Arc::clone(&self.store) };
        let measured = tokio::task::spawn_blocking(move || handler.observe(&operation, &attempt, &frozen))
            .await
            .context("net probe worker join failure")?;
        measured
    }
}

#[async_trait]
impl NativeTaskHandler for NetProbeHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: KIND.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-net-probe/1".into(),
            authority_refs: vec!["network.reachability.observe/v1".into()],
            prompt_section: Some(
                "net.probe/v1 观察本节点此刻到某个端点的通路是否真的可用：输入 {\"url\":\"http://host:port/path\",\"timeout_ms\":2000}。只做 DNS、TCP 连接，http 再读一行状态码；不读响应体、不跟随重定向、不带任何凭据或 Cookie，也不保留连接。结果是这一次尝试的测量（reachable、dns/connect/http 的阶段耗时、error_kind），不是对过去或未来的保证。症状与假设冲突时先观察，再解释；不要用早先的成功代替此刻的检查。"
                    .into(),
            ),
        }
    }

    /// A probe touches the network but leaves nothing behind. It is not reset,
    /// restore, replay, cancel or compensation: the declaration says the
    /// attempt may be repeated (a new observation) and has no durable effect.
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        vec![EffectContractBuilder::new(
            descriptor.implementation_ref.plugin_id,
            descriptor.implementation(),
            "net.probe.v1",
            descriptor.definition_version,
        )
        .resource("network_reachability", "declared_endpoint", ResourceAccess::Read)
        .action(shenko_types::effect::EffectActionKind::Cancel)
        .idempotency(IdempotencyGuarantee::BestEffort)
        .external_effect(ExternalEffect {
            kind: "network_probe".into(),
            description: "Sends one HTTP status-line request to an explicitly declared endpoint; no body, no redirect, no credentials, no persistent state".into(),
            reversible: true,
            compensation_possible: false,
        })
        .checkpoint(CheckpointContract {
            capturable: vec!["the measured observation in this installation's CAS".into()],
            excluded: vec!["network state before and after the attempt".into()],
            coordinated: false,
            consistency: "one bounded measurement".into(),
        })
        .lifecycle(LifecycleContract {
            on_upgrade: "an in-flight probe is not repeated; its observation stays queryable".into(),
            on_uninstall: "the definition is withdrawn; measured observations stay readable".into(),
            on_crash: "an armed probe with no observation stays unknown; reconcile re-measures once under the same attempt".into(),
            receipt_readable_after_removal: true,
        })
        .basis(GuaranteeBasis::ClaimOnly)
        .build()]
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: "native.net.probe".into(),
            version: "1".into(),
            description: "Observe whether this node can currently reach a declared endpoint, with per-stage measured timings".into(),
            usage: Some("delegate:net.probe:1 {\"url\":\"http://127.0.0.1:8000/v1/models\",\"timeout_ms\":2000}".into()),
        }]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "net.probe.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight || attempt.dispatch_stage != AttemptDispatchStage::Armed {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    /// A real HTTP/1.1 responder on the loopback interface; each connection is
    /// answered once and closed, exactly like a bounded probe expects.
    fn serve(status_line: &'static str) -> (u16, std::thread::JoinHandle<()>) {
        serve_many(status_line, 1)
    }

    /// Answer exactly `connections` requests, then stop. A fixture thread that
    /// waits for a connection the test never makes would hang the join, not
    /// fail it, so the count is explicit.
    fn serve_many(status_line: &'static str, connections: usize) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let handle = std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(format!("{status_line}\r\n\r\n").as_bytes());
            }
        });
        (port, handle)
    }

    fn input(url: String, timeout_ms: u64) -> ProbeInput {
        ProbeInput { url, timeout_ms }
    }

    #[test]
    fn freeze_rejects_urls_this_observer_cannot_honour() {
        for bad in [
            "ftp://example.com/x",
            "http://user:pass@example.com/",
            "http://example.com:0/",
            "http://example.com/?q=1",
            "not a url",
            "",
        ] {
            assert!(
                ProbeObservation::freeze(&input(bad.into(), 1_000)).is_err(),
                "accepted unsupported url: {bad}"
            );
        }
        assert!(ProbeObservation::freeze(&input("http://example.com/".into(), 99)).is_err());
        assert!(ProbeObservation::freeze(&input("http://example.com/".into(), MAX_TIMEOUT_MS + 1)).is_err());
        let frozen = ProbeObservation::freeze(&input("https://example.com/a/b".into(), 1_000)).unwrap();
        assert_eq!((frozen.scheme.as_str(), frozen.host.as_str(), frozen.port, frozen.path.as_str()),
            ("https", "example.com", 443, "/a/b"));
        let frozen = ProbeObservation::freeze(&input("http://127.0.0.1:8080".into(), 1_000)).unwrap();
        assert_eq!((frozen.port, frozen.path.as_str()), (8080, "/"));
    }

    #[test]
    fn measure_reports_a_real_http_status() {
        let (port, server) = serve("HTTP/1.1 204 No Content");
        let frozen = ProbeObservation::freeze(&input(format!("http://127.0.0.1:{port}/health"), 2_000)).unwrap();
        let observation = ProbeObservation::measure(&frozen);
        assert!(observation.dns.ok, "loopback resolution failed");
        assert!(observation.connect.ok, "loopback connect failed: {observation:?}");
        assert!(observation.reachable);
        assert_eq!(observation.http.as_ref().and_then(|http| http.status), Some(204));
        assert!(observation.error_kind.is_none());
        server.join().expect("fixture server");
    }

    #[test]
    fn measure_reports_an_unreachable_port_as_a_measured_fact() {
        // Bind then release, so the port is real but nothing listens on it.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let frozen = ProbeObservation::freeze(&input(format!("http://127.0.0.1:{port}/"), 500)).unwrap();
        let observation = ProbeObservation::measure(&frozen);
        assert!(observation.dns.ok);
        assert!(!observation.connect.ok);
        assert!(!observation.reachable);
        assert_eq!(observation.error_kind.as_deref(), Some("connect_failed"));
        assert!(observation.http.is_none());
    }

    #[test]
    fn measure_keeps_dns_failure_distinct_from_connection_failure() {
        let frozen = ProbeObservation::freeze(&input("http://shenko-probe.invalid/".into(), 500)).unwrap();
        let observation = ProbeObservation::measure(&frozen);
        assert!(!observation.dns.ok);
        assert!(!observation.reachable);
        assert_eq!(observation.error_kind.as_deref(), Some("dns_failed"));
        assert!(!observation.connect.ok);
    }
}
