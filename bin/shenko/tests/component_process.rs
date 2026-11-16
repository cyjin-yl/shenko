//! One real CLI/SSH star-component journey. No observation or ACK is forged.
//! Seven durable voters are configured over pinned OpenSSH, then only three
//! Start processes remain reachable. A and C have no configured model; B's
//! unprobed binding is paused before consumers. No inference is requested.
#![cfg(unix)]

#[path = "support/process_runtime.rs"]
mod process_runtime;

use anyhow::{Context, Result, ensure};
use process_runtime::{
    OwnedProcess, append_json, copy_diagnostics, file_sha256, health, write_sshd_config,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use shenko_node::{
    ComponentCertificate, ComponentObservation, ControlFrontier, MemberConfiguration,
};
use shenko_types::ids::EntityId;
use std::{
    cell::Cell,
    collections::BTreeSet,
    fs,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

const NODES: [&str; 7] = ["a", "b", "c", "d", "e", "f", "g"];
const RUN_LIMIT: Duration = Duration::from_secs(900);
const COMMAND_LIMIT: Duration = Duration::from_secs(180);

fn quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn executable(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is required for real SSH tools")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() && candidate.metadata()?.permissions().mode() & 0o111 != 0 {
            return Ok(candidate.canonicalize()?);
        }
    }
    anyhow::bail!("required executable {name} was not found")
}

fn port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

struct Fixture {
    root: PathBuf,
    original_path: std::ffi::OsString,
    number: Cell<u32>,
    started: Instant,
    servers: Vec<Option<OwnedProcess>>,
    residents: Vec<Option<OwnedProcess>>,
    ssh_ports: Vec<u16>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root =
            std::env::temp_dir().join(format!("shenko-component-process-{}", EntityId::new()));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        for directory in ["private", "private/bin", "logs"] {
            fs::create_dir(root.join(directory))?;
        }
        for name in NODES {
            fs::create_dir(root.join("private").join(name))?;
        }
        Ok(Self {
            root,
            original_path: std::env::var_os("PATH").context("PATH is required")?,
            number: Cell::new(0),
            started: Instant::now(),
            servers: (0..NODES.len()).map(|_| None).collect(),
            residents: (0..3).map(|_| None).collect(),
            ssh_ports: Vec::new(),
        })
    }

    fn private(&self) -> PathBuf {
        self.root.join("private")
    }
    fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    fn node_root(&self, index: usize) -> PathBuf {
        self.private().join(NODES[index])
    }
    fn data(&self, index: usize) -> PathBuf {
        self.node_root(index).join("installation")
    }
    fn remote_binary(&self) -> PathBuf {
        self.private().join("shenko")
    }

    fn path(&self) -> Result<std::ffi::OsString> {
        let paths = std::iter::once(self.private().join("bin"))
            .chain(std::env::split_paths(&self.original_path));
        Ok(std::env::join_paths(paths)?)
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

    fn cli(&self, index: usize) -> Result<Command> {
        let mut command = self.command(env!("CARGO_BIN_EXE_shenko"))?;
        command
            .arg("--data-dir")
            .arg(self.data(index))
            .args(["--log", "info"]);
        Ok(command)
    }

    fn label(&self, prefix: &str) -> String {
        let number = self.number.get();
        self.number.set(number + 1);
        format!("{number:04}-{prefix}")
    }

    fn run(&self, command: &mut Command, label: &str, input: Option<&Path>) -> Result<ExitStatus> {
        let remaining = RUN_LIMIT.saturating_sub(self.started.elapsed());
        ensure!(
            !remaining.is_zero(),
            "component journey exceeded its 900 second bound"
        );
        OwnedProcess::spawn_with_input(command, &self.logs(), label, input)?
            .finish(COMMAND_LIMIT.min(remaining))
    }

    fn invoke(
        &self,
        index: usize,
        args: &[&str],
        input: Option<&Value>,
    ) -> Result<(ExitStatus, Value)> {
        let label = self.label(&format!("{}-{}", NODES[index], args.join("-")));
        // Names in argv may contain slashes; the filename is just an ordinal
        // and Node label. The full public argv remains in the observation.
        let label = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(100)
            .collect::<String>();
        let input_path = self.private().join(format!("{label}.stdin"));
        if let Some(input) = input {
            fs::write(&input_path, serde_json::to_vec(input)?)?;
        }
        append_json(
            &self.logs().join("commands.jsonl"),
            &json!({
                "label":label,"node":NODES[index],"args":args,"request":input,
                "command_limit_secs":COMMAND_LIMIT.as_secs(),"journey_limit_secs":RUN_LIMIT.as_secs(),
            }),
        )?;
        let status = self.run(
            self.cli(index)?.args(args),
            &label,
            input.map(|_| input_path.as_path()),
        )?;
        let bytes = fs::read(self.logs().join(format!("{label}.stdout")))?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).with_context(|| format!("decode {label} stdout"))?
        };
        Ok((status, value))
    }

    fn json(&self, index: usize, args: &[&str], input: Option<&Value>) -> Result<Value> {
        let (status, value) = self.invoke(index, args, input)?;
        ensure!(
            status.success(),
            "{} {args:?} failed with {status}; original stderr is retained in {}",
            NODES[index],
            self.logs().display()
        );
        Ok(value)
    }

    fn node(&self, index: usize, request: Value) -> Result<Value> {
        self.json(index, &["node"], Some(&request))
    }

    fn prepare_ssh(&mut self) -> Result<()> {
        let actual_ssh = executable("ssh")?;
        let binary = Path::new(env!("CARGO_BIN_EXE_shenko")).canonicalize()?;
        let config = self.private().join("ssh_config");
        // Only configuration is adapted. These launchers exec the exact
        // system SSH and candidate CLI, forwarding stdin/stdout unchanged.
        // They never construct an Observe response, vote or acknowledgement.
        let ssh_launcher = self.private().join("bin/ssh");
        fs::write(
            &ssh_launcher,
            format!(
                "#!/bin/sh\nexec {} -F {} \"$@\"\n",
                quote(&actual_ssh),
                quote(&config)
            ),
        )?;
        fs::set_permissions(&ssh_launcher, fs::Permissions::from_mode(0o700))?;
        let runtime_path = PathBuf::from(self.path()?);
        fs::write(
            self.remote_binary(),
            format!(
                "#!/bin/sh\nexec /usr/bin/env -i PATH={} LC_ALL=C GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null {} \"$@\"\n",
                quote(&runtime_path),
                quote(&binary),
            ),
        )?;
        fs::set_permissions(self.remote_binary(), fs::Permissions::from_mode(0o700))?;
        let account_label = self.label("account");
        ensure!(
            self.run(self.command("id")?.arg("-un"), &account_label, None)?
                .success(),
            "cannot identify fixture account"
        );
        let user = fs::read_to_string(self.logs().join(format!("{account_label}.stdout")))?;
        let user = user.trim();
        ensure!(
            !user.is_empty() && !user.contains(char::is_whitespace),
            "unexpected fixture account name"
        );
        let mut known_hosts = String::new();
        for (index, name) in NODES.iter().enumerate() {
            for kind in ["host", "client"] {
                let key = self.node_root(index).join(kind);
                let label = self.label(&format!("{name}-{kind}-keygen"));
                ensure!(
                    self.run(
                        self.command("ssh-keygen")?
                            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                            .arg(&key),
                        &label,
                        None
                    )?
                    .success(),
                    "fixture keygen failed"
                );
            }
        }
        self.set_edges(
            &NODES
                .iter()
                .flat_map(|a| NODES.iter().filter(move |b| a != *b).map(move |b| (*a, *b)))
                .collect::<Vec<_>>(),
        )?;
        for (index, name) in NODES.iter().enumerate() {
            let port = port()?;
            self.ssh_ports.push(port);
            let public = fs::read_to_string(self.node_root(index).join("host.pub"))?;
            let mut words = public.split_whitespace();
            let algorithm = words.next().context("host key algorithm")?;
            let key = words.next().context("host public key")?;
            known_hosts.push_str(&format!("[127.0.0.1]:{port} {algorithm} {key}\n"));
            let sshd_config = self.node_root(index).join("sshd_config");
            write_sshd_config(
                &sshd_config,
                &self.node_root(index),
                &self.node_root(index).join("host"),
                &self.node_root(index).join("authorized_keys"),
                port,
            )?;
            let sshd = std::env::var_os("SHENKO_SSHD").unwrap_or_else(|| "/usr/sbin/sshd".into());
            let mut command = self.command(sshd)?;
            command.args(["-D", "-e", "-f"]).arg(sshd_config);
            self.servers[index] = Some(OwnedProcess::spawn(
                &mut command,
                &self.logs(),
                &format!("sshd-{name}"),
            )?);
            let started = Instant::now();
            loop {
                ensure!(
                    self.servers[index]
                        .as_mut()
                        .context("sshd missing")?
                        .poll()?
                        .is_none(),
                    "fixture sshd {name} exited",
                );
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
                ensure!(
                    started.elapsed() < Duration::from_secs(5),
                    "fixture sshd did not listen"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let known_hosts_path = self.private().join("known_hosts");
        fs::write(&known_hosts_path, &known_hosts)?;
        let mut client_config = String::new();
        for (source, source_name) in NODES.iter().enumerate() {
            for (destination, destination_name) in NODES.iter().enumerate() {
                if source == destination {
                    continue;
                }
                client_config.push_str(&format!(
                    "Host component-{source_name}-{destination_name}\n HostName 127.0.0.1\n Port {}\n User {user}\n IdentityFile {}\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile {}\n GlobalKnownHostsFile /dev/null\n BatchMode yes\n StrictHostKeyChecking yes\n ConnectionAttempts 1\n LogLevel ERROR\n\n",
                    self.ssh_ports[destination], self.node_root(source).join("client").display(), known_hosts_path.display(),
                ));
            }
        }
        fs::write(config, client_config)?;
        fs::write(
            self.logs().join("transport.json"),
            serde_json::to_vec_pretty(&json!({
                "system_ssh":actual_ssh,"system_ssh_sha256":file_sha256(&actual_ssh)?,
                "cli_binary":binary,"cli_sha256":file_sha256(&binary)?,
                "ssh_launcher_sha256":file_sha256(&ssh_launcher)?,
                "remote_launcher_sha256":file_sha256(&self.remote_binary())?,
                "known_hosts":known_hosts,"ports":self.ssh_ports,
                "scope":"real pinned OpenSSH; private -F configuration launcher; no fabricated protocol response; one host, not physical partition",
            }))?,
        )?;
        Ok(())
    }

    fn set_edges(&self, edges: &[(&str, &str)]) -> Result<()> {
        for destination in NODES {
            let mut keys = String::new();
            for (source, target) in edges {
                if *target == destination {
                    keys.push_str(&fs::read_to_string(
                        self.private().join(source).join("client.pub"),
                    )?);
                }
            }
            // New authentication opens each exchange. Replacing this private
            // key file never fabricates a transport response or Node state.
            fs::write(
                self.private().join(destination).join("authorized_keys"),
                keys,
            )?;
        }
        append_json(
            &self.logs().join("topology.jsonl"),
            &json!({"edges":edges,"enforcement":"per-source public keys accepted by each actual sshd","elapsed_seconds":self.started.elapsed().as_secs_f64()}),
        )?;
        Ok(())
    }

    fn start(
        &mut self,
        index: usize,
        port: u16,
        boundary: Option<EntityId>,
        model_port: Option<u16>,
    ) -> Result<()> {
        let mut command = self.cli(index)?;
        command.args(["start", "--port"]).arg(port.to_string());
        if let Some(boundary) = boundary {
            command.arg("--pause-at-start").arg(boundary.to_string());
        }
        if let Some(model_port) = model_port {
            command
                .env("SHENKO_USE_LOCAL_MODEL", "1")
                .env("SHENKO_LOCAL_MODEL", "component-unprobed-binding")
                .env(
                    "SHENKO_LOCAL_MODEL_URL",
                    format!("http://127.0.0.1:{model_port}"),
                );
        }
        self.residents[index] = Some(OwnedProcess::spawn(
            &mut command,
            &self.logs(),
            &format!("resident-{}", NODES[index]),
        )?);
        Ok(())
    }

    fn ready(&mut self, index: usize, port: u16) -> Result<Value> {
        let started = Instant::now();
        loop {
            ensure!(
                self.residents[index]
                    .as_mut()
                    .context("resident missing")?
                    .poll()?
                    .is_none(),
                "{} exited before readiness",
                NODES[index]
            );
            if let Ok(health) = health(port)
                && health["status"] == "ok"
            {
                return Ok(health);
            }
            ensure!(
                started.elapsed() < Duration::from_secs(180) && self.started.elapsed() < RUN_LIMIT,
                "{} readiness timed out; original logs retained",
                NODES[index]
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop_resident(&mut self, index: usize) -> Result<()> {
        if let Some(mut process) = self.residents[index].take() {
            let (status, forced) = process.stop(libc::SIGINT, Duration::from_secs(20))?;
            ensure!(
                !forced && status.success(),
                "{} did not stop normally: {status}, forced={forced}",
                NODES[index]
            );
        }
        Ok(())
    }

    fn stop_server(&mut self, index: usize) -> Result<()> {
        if let Some(mut server) = self.servers[index].take() {
            let (_, forced) = server.stop(libc::SIGTERM, Duration::from_secs(3))?;
            ensure!(!forced, "sshd {} required forced cleanup", NODES[index]);
            ensure!(
                TcpStream::connect(("127.0.0.1", self.ssh_ports[index])).is_err(),
                "owned sshd listener remains reachable"
            );
        }
        Ok(())
    }

    fn snapshot(&self, index: usize) -> Result<Value> {
        let conn = Connection::open_with_flags(
            self.data(index).join("shenko.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let tx = conn.unchecked_transaction()?;
        let head: String = tx.query_row("SELECT data FROM subject_branch_heads", [], |row| {
            row.get(0)
        })?;
        let genesis: String =
            tx.query_row("SELECT data FROM subject_genesis", [], |row| row.get(0))?;
        let provisioning: String = tx.query_row(
            "SELECT data FROM local_installation_provisioning WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let count = |table: &str| -> Result<i64> {
            Ok(
                tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?,
            )
        };
        let result = json!({
            "head":serde_json::from_str::<Value>(&head)?,
            "genesis":serde_json::from_str::<Value>(&genesis)?,
            "local_provisioning":serde_json::from_str::<Value>(&provisioning)?,
            "request_attempts":count("subject_request_attempts")?,
            "external_attempts":count("attempts")?,
            "authority_bindings":count("subject_authority_bindings")?,
        });
        tx.commit()?;
        Ok(result)
    }

    fn heartbeat(&self, index: usize) -> Result<Value> {
        let conn = Connection::open_with_flags(
            self.data(index).join("node/node.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let raw: String = conn.query_row(
            "SELECT v FROM meta WHERE k='component_candidate'",
            [],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str(&raw)?)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut errors = Vec::new();
        for (index, name) in NODES[..3].iter().enumerate() {
            if let Err(error) = self.stop_resident(index) {
                errors.push(format!("resident {name}: {error:#}"));
            }
        }
        for (index, name) in NODES.iter().enumerate() {
            if let Err(error) = self.stop_server(index) {
                errors.push(format!("sshd {name}: {error:#}"));
            }
        }
        let cleanup = serde_json::to_vec(&json!({
            "errors":errors,"scope":"owned leaders joined and SSH listeners checked; not a proof about arbitrary escaped descendants",
        })).map_err(anyhow::Error::from).and_then(|bytes|
            fs::write(self.logs().join("cleanup.json"), bytes).map_err(Into::into));
        if let Err(error) = cleanup {
            eprintln!("could not retain component cleanup evidence: {error:#}");
        }
        if let Some(destination) = std::env::var_os("SHENKO_COMPONENT_PROCESS_EVIDENCE")
            && let Err(error) = copy_diagnostics(&self.logs(), Path::new(&destination))
        {
            eprintln!(
                "component evidence copy failed: {error:#}; original logs {}",
                self.logs().display()
            );
        }
        // All keys, input files, owner control and databases are private.
        // Only explicit observations/logs are copied to the evidence output.
        if errors.is_empty() && !self.logs().join("processes.cleanup-failed").exists() {
            let _ = fs::remove_dir_all(self.private());
        }
        eprintln!(
            "component process observations retained in {}",
            self.logs().display()
        );
    }
}

#[test]
fn three_real_start_processes_agree_over_a_pinned_ssh_star() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let outcome = journey(&mut fixture);
    fs::write(
        fixture.logs().join("result.json"),
        serde_json::to_vec_pretty(&json!({
            "passed":outcome.is_ok(),"error":outcome.as_ref().err().map(|error| format!("{error:#}")),
            "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),
            "model_inference_tested":false,"physical_partition_tested":false,
            "foreign_inflight_takeover_tested":false,
        }))?,
    )?;
    outcome
}

fn journey(fixture: &mut Fixture) -> Result<()> {
    fixture.prepare_ssh()?;
    let bootstrap_port = port()?;
    fixture.start(1, bootstrap_port, None, None)?;
    let origin_health = fixture.ready(1, bootstrap_port)?;
    fixture.stop_resident(1)?;
    let origin =
        shenko_store::Store::inspect_installation_binding(&fixture.data(1).join("shenko.db"))?;
    let origin_subject = origin
        .subject
        .as_ref()
        .context("bootstrap subject missing")?;
    let subject_id = origin_subject.agent_id;
    let original_facts = fixture.snapshot(1)?;
    let identity = fixture.json(1, &["sync", "identity"], None)?;
    let identity_path = fixture.private().join("origin-identity.json");
    fs::write(
        &identity_path,
        serde_json::to_vec(&identity["subject_identity"])?,
    )?;
    let mut ids = Vec::new();
    for index in 0..NODES.len() {
        let actual = fixture.json(index, &["sync", "identity"], None)?;
        ids.push(serde_json::from_value::<EntityId>(
            actual["node_id"].clone(),
        )?);
    }
    ensure!(
        ids.iter().copied().collect::<BTreeSet<_>>().len() == NODES.len(),
        "Node identities were copied"
    );
    // Enrollment learns actual identities over pinned SSH. No synthetic
    // member ID or in-process transport closure joins the electorate.
    for (source, source_name) in NODES.iter().enumerate() {
        for (destination, destination_name) in NODES.iter().enumerate() {
            if source == destination {
                continue;
            }
            let alias = format!("component-{source_name}-{destination_name}");
            let enrolled = fixture.json(
                source,
                &[
                    "sync",
                    "enroll",
                    &alias,
                    "--remote-data-dir",
                    &fixture.data(destination).to_string_lossy(),
                    "--remote-binary",
                    &fixture.remote_binary().to_string_lossy(),
                ],
                None,
            )?;
            ensure!(
                enrolled["peer"]["node_id"] == json!(ids[destination]),
                "authenticated enrollment identity differs"
            );
        }
    }
    fixture.json(1, &["sync", "push"], None)?;
    for index in 0..NODES.len() {
        if index == 1 {
            continue;
        }
        let before = shenko_store::Store::inspect_installation_binding(
            &fixture.data(index).join("shenko.db"),
        )?;
        ensure!(
            before.subject.is_none(),
            "plain peer import became an installed subject"
        );
        let home = shenko_store::Store::inspect_home_installation_binding(
            &fixture.data(index).join("shenko.db"),
            &fixture.data(index).join("cas"),
        )?;
        ensure!(
            home.binding == before
                && home.installation_id == before.ledger_id
                && home.installation_id != origin.ledger_id,
            "receiver lost its independently provisioned home identity"
        );
        let imported = fixture.snapshot(index)?;
        ensure!(
            imported["genesis"] == original_facts["genesis"]
                && imported["local_provisioning"] != original_facts["local_provisioning"],
            "peer import rewrote the source genesis or copied local home provisioning"
        );
        fixture.json(
            index,
            &[
                "sync",
                "adopt",
                "--identity-file",
                &identity_path.to_string_lossy(),
                "--trusted-node",
                &ids[1].to_string(),
                "--provenance",
                "pinned OpenSSH origin identity retained by this journey",
                "--reason",
                "adopt the real bootstrap origin before component participation",
            ],
            None,
        )?;
    }
    let added = ids
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, id)| id.to_string())
        .collect::<Vec<_>>();
    let mut args = vec!["sync", "members", "add"];
    args.extend(added.iter().map(String::as_str));
    fixture.json(1, &args, None)?;
    for (index, id) in ids.iter().enumerate() {
        if index != 1 {
            fixture.json(
                1,
                &["sync", "members", "push", "--peer", &id.to_string()],
                None,
            )?;
        }
    }
    let shown = fixture.json(1, &["sync", "members", "show"], None)?;
    let configuration: MemberConfiguration =
        serde_json::from_value(shown["member_configuration"].clone())?;
    let frontier: ControlFrontier =
        serde_json::from_value(fixture.node(1, json!({"command":"control_frontier"}))?)?;
    ensure!(
        configuration.voters() == ids.iter().copied().collect(),
        "actual seven-member electorate differs"
    );
    ensure!(
        !configuration.has_quorum(&ids[..3].iter().copied().collect()),
        "three nodes are not a minority"
    );
    let mut member_observations = Vec::new();
    for index in 0..NODES.len() {
        let shown = fixture.json(index, &["sync", "members", "show"], None)?;
        let retained: MemberConfiguration =
            serde_json::from_value(shown["member_configuration"].clone())?;
        // Bounded Append records the local learning time. It is not part of
        // ConfigurationEntry or its chosen value. Compare every consensus
        // identity field and the complete committed prefix across members;
        // then prove each member retains its own full metadata after reopen.
        ensure!(
            retained.revision == configuration.revision
                && retained.term == configuration.term
                && retained.members == configuration.members
                && retained.joint_new == configuration.joint_new
                && retained.digest == configuration.digest,
            "member configuration was not actually retained"
        );
        let retained_frontier: ControlFrontier =
            serde_json::from_value(fixture.node(index, json!({"command":"control_frontier"}))?)?;
        ensure!(
            retained_frontier == frontier,
            "member retained a different committed control prefix"
        );
        let reopened = fixture.json(index, &["sync", "members", "show"], None)?;
        ensure!(
            shown["member_configuration"] == reopened["member_configuration"],
            "member's complete configuration metadata changed after CLI reopen"
        );
        member_observations.push(json!({
            "node":NODES[index],"configuration":retained,
            "frontier":retained_frontier,"reopened":reopened["member_configuration"],
        }));
    }
    fs::write(
        fixture.logs().join("membership.json"),
        serde_json::to_vec_pretty(
            &json!({"configuration":configuration,"frontier":frontier,"members":member_observations,"node_ids":ids,"origin_health":origin_health,"origin_identity":identity,"origin_ledger":origin.ledger_id}),
        )?,
    )?;

    // The same public failed-election path creates real local Store forks.
    // This establishes local subject binding on distinct ledgers without
    // copying the origin database, inventing a receipt or editing SQL.
    fixture.set_edges(&[])?;
    for index in [0, 2] {
        let selected = fixture.json(index, &["sync", "election", "campaign"], None)?;
        ensure!(
            selected["availability_branch"] == true && selected["fencing_token"].is_null(),
            "isolated election did not select an explicit branch"
        );
        let binding = shenko_store::Store::inspect_installation_binding(
            &fixture.data(index).join("shenko.db"),
        )?;
        let subject = binding
            .subject
            .context("actual fork did not establish local subject binding")?;
        ensure!(
            binding.ledger_id != origin.ledger_id
                && subject.agent_id == subject_id
                && subject.origin_proof == origin_subject.origin_proof
                && subject.branch_id != origin_subject.branch_id,
            "fork lost source identity or reused the origin ledger/branch"
        );
        let snapshot = fixture.snapshot(index)?;
        let local_home = shenko_store::Store::inspect_home_installation_binding(
            &fixture.data(index).join("shenko.db"),
            &fixture.data(index).join("cas"),
        )?;
        ensure!(
            snapshot["head"]["branch_origin"].is_object()
                && snapshot["head"]["committed_at"]["ledger_id"] == json!(binding.ledger_id)
                && snapshot["genesis"] == original_facts["genesis"]
                && local_home.installation_id == binding.ledger_id
                && local_home.installation_id != origin.ledger_id,
            "fork has no local committed provenance"
        );
        fs::write(
            fixture.logs().join(format!("{}-fork.json", NODES[index])),
            serde_json::to_vec_pretty(&snapshot)?,
        )?;
    }
    for index in 3..NODES.len() {
        fixture.stop_server(index)?;
    }
    fixture.set_edges(&[("a", "b"), ("b", "a"), ("b", "c"), ("c", "b")])?;

    // Keep an unused bound socket so this URL cannot accidentally name an
    // ambient model. It supplies no response. B is configured, never probed
    // or called; the real startup pause must prevent every inference attempt.
    let unprobed = TcpListener::bind("127.0.0.1:0")?;
    unprobed.set_nonblocking(true)?;
    let ui_ports = [port()?, port()?, port()?];
    let boundary = EntityId::new();
    fixture.start(0, ui_ports[0], None, None)?;
    fixture.start(2, ui_ports[2], None, None)?;
    fixture.start(
        1,
        ui_ports[1],
        Some(boundary),
        Some(unprobed.local_addr()?.port()),
    )?;
    let coordinator_health = fixture.ready(1, ui_ports[1])?;
    ensure!(
        coordinator_health["subject_id"] == json!(subject_id),
        "coordinator started a different self"
    );
    let paused = fixture.json(1, &["runtime", "status", &boundary.to_string()], None)?;
    ensure!(
        paused["state"] == "paused" && paused["boundary_at"].is_object(),
        "coordinator did not commit its startup boundary"
    );
    let mut observed = Vec::new();
    for index in 0..3 {
        observed.push(serde_json::from_value::<ComponentObservation>(
            fixture.node(index, json!({"command":"component_observe"}))?,
        )?);
    }
    let certificate: ComponentCertificate = observed[1]
        .presence
        .committed
        .clone()
        .context("coordinator certificate missing")?;
    certificate.validate()?;
    ensure!(
        certificate.proposal.coordinator == ids[1] && certificate.acknowledgements.len() == 3,
        "component lacks its actual three-member certificate"
    );
    let wanted = [
        BTreeSet::from([ids[0], ids[1]]),
        BTreeSet::from([ids[0], ids[1], ids[2]]),
        BTreeSet::from([ids[1], ids[2]]),
    ];
    for index in 0..3 {
        ensure!(
            observed[index].complete
                && observed[index]
                    .reachable
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    == wanted[index],
            "actual SSH neighbourhood differs for {}",
            NODES[index]
        );
        ensure!(
            observed[index].presence.node_id == ids[index]
                && observed[index].presence.committed.as_ref() == Some(&certificate),
            "member retained a different certificate"
        );
        ensure!(
            observed[index].presence.runtime_id.is_some()
                && observed[index].presence.candidate.is_some() == (index == 1),
            "configured candidacy differs from the actual Start environment"
        );
        ensure!(
            fixture.residents[index]
                .as_mut()
                .context("resident missing")?
                .poll()?
                .is_none(),
            "resident exited"
        );
        let snapshot = fixture.snapshot(index)?;
        ensure!(
            snapshot["request_attempts"] == 0 && snapshot["external_attempts"] == 0,
            "the coordination journey admitted inference or an external attempt"
        );
        if index == 1 {
            ensure!(
                snapshot["head"]["branch_id"] == json!(certificate.proposal.branch_id)
                    && snapshot["head"]["mode"] == "paused"
                    && snapshot["authority_bindings"] == 1,
                "coordinator did not activate the actual agreed Store branch"
            );
        } else {
            ensure!(
                snapshot["authority_bindings"] == 0
                    && TcpStream::connect(("127.0.0.1", ui_ports[index])).is_err(),
                "follower started selected-runtime consumers"
            );
            let request = json!({"command":"availability_branch","branch_id":certificate.proposal.branch_id,"subject_id":subject_id,"reason":"follower must not acquire the coordinator branch"});
            let (status, _) = fixture.invoke(index, &["node"], Some(&request))?;
            ensure!(!status.success(), "follower opened the coordinator branch");
            ensure!(
                fixture.snapshot(index)? == snapshot,
                "follower refusal changed its Store head or attempts"
            );
        }
        fs::write(
            fixture
                .logs()
                .join(format!("{}-selected-snapshot.json", NODES[index])),
            serde_json::to_vec_pretty(&snapshot)?,
        )?;
    }
    fs::write(
        fixture.logs().join("agreement.json"),
        serde_json::to_vec_pretty(
            &json!({"observations":observed,"certificate":certificate,"paused":paused,"coordinator_health":coordinator_health}),
        )?,
    )?;
    let first = (0..3)
        .map(|index| fixture.heartbeat(index))
        .collect::<Result<Vec<_>>>()?;
    let started = Instant::now();
    let later = loop {
        let later = (0..3)
            .map(|index| fixture.heartbeat(index))
            .collect::<Result<Vec<_>>>()?;
        if later
            .iter()
            .zip(&first)
            .all(|(new, old)| new["observed_at"].as_str() > old["observed_at"].as_str())
        {
            break later;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(15),
            "owned resident heartbeat did not renew"
        );
        for index in 0..3 {
            ensure!(
                fixture.residents[index]
                    .as_mut()
                    .context("resident missing")?
                    .poll()?
                    .is_none(),
                "resident exited while following"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    for index in 0..3 {
        ensure!(
            later[index]["runtime_id"] == first[index]["runtime_id"],
            "renewal replaced the actual runtime"
        );
    }
    ensure!(
        matches!(unprobed.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "the paused unprobed binding was contacted"
    );
    fs::write(
        fixture.logs().join("resident-renewal.json"),
        serde_json::to_vec_pretty(&json!({"before":first,"after":later,"model_contacted":false}))?,
    )?;
    for index in 0..3 {
        fixture.stop_resident(index)?;
    }
    for index in 0..3 {
        fixture.stop_server(index)?;
    }
    Ok(())
}
