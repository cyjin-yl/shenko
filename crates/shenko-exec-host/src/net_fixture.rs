//! An owned, disposable network fault fixture for real path observations.
//!
//! `awareness.network-change` needs something the retired comparison never had:
//! a path that is genuinely broken while the model's own channel still works.
//! In the retired scenario the symptom arrived *through* the channel the model
//! was asked to doubt, so an arriving message was itself evidence that the
//! channel was fine; nothing was measurable and the model was asked to invent a
//! doubt. This module is the fixture half of the repair, and it is deliberately
//! boring: it owns a small piece of the host's network, it breaks that piece,
//! and it reports what the production `net.probe` measurement actually returned.
//!
//! Three tiers, each with an exact claim:
//!
//! * [`FixtureScope::ProcessOwnedEndpoint`] — a TCP endpoint in this process's
//!   own network namespace whose listener can be stopped and restarted. A
//!   stopped listener produces a real `ECONNREFUSED`; nothing is simulated.
//!   This tier needs no privilege, and it claims nothing about the host's wider
//!   connectivity.
//! * [`FixtureScope::OwnedInterface`] — a disposable veth-backed interface
//!   whose address carries the endpoint. Bringing the link down turns the
//!   failure into a real local-delivery/route failure and bringing it back up
//!   restores the same endpoint. Needs `CAP_NET_ADMIN`; when the host refuses,
//!   the fixture reports that prerequisite instead of pretending to break
//!   anything.
//! * [`FixtureScope::OwnedNamespace`] — a disposable network namespace with its
//!   own route table and resolver configuration. Faults are measured *inside*
//!   that namespace (`ip -n <ns> route get`, `ip netns exec <ns> getent hosts`),
//!   so a route or DNS fact is a kernel lookup in the owned namespace rather
//!   than a line the fixture wrote into a file. Needs `CAP_NET_ADMIN` and, for
//!   the resolver tier, a writable `/etc/netns/<ns>`.
//!
//! What it never does: it never edits the host's routes, resolvers or existing
//! interfaces, it never fabricates a ping/HTTP reply for a path that is down,
//! and it never reports a fault it did not measure. Every command it runs is
//! retained verbatim (argv, status, stdout, stderr) next to the measurement, and
//! a stage that could not be attempted carries its prerequisite instead of a
//! guessed result.
//!
//! Where this sits in the cognitive contract: awareness is a capability, and
//! the awareness text plus a watcher notice is only the harness that currently
//! simulates it. This module is part of that harness. Its receipts are
//! mechanism observations of one attempt — they can serve as the receipt a
//! subject's own reason stops at, and they are the harness's independent
//! measurement of the same path — but they are not content, not an attitude,
//! and carry no confidence. Nothing here writes to the ledger, and no measured
//! failure is turned into a rule about what the model may conclude.
use anyhow::{Context as _, Result, bail, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shenko_broker::request::ProviderBinding;
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::ObjectRef;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::net_probe::{DEFAULT_TIMEOUT_MS, ProbeInput, ProbeObservation};

/// The schema version of every receipt this fixture writes.
pub const FIXTURE_SCHEMA_VERSION: u32 = 1;
/// How much of one command's stdout/stderr is retained.
pub const MAX_COMMAND_BYTES: usize = 8 * 1024;
/// The longest one fixture command may take.
pub const MAX_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// The route the namespace tier blackholes (TEST-NET-2, RFC 5737).
pub const BLACKHOLE_NETWORK: &str = "198.51.100.0/24";
/// One address inside that network, used for the route lookup.
pub const BLACKHOLE_ADDRESS: &str = "198.51.100.9";
/// The unresolvable nameserver the resolver tier points at (TEST-NET-1).
pub const UNREACHABLE_RESOLVER: &str = "192.0.2.1";

/// What one fault of this fixture is allowed to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureScope {
    /// One TCP endpoint in this process's own network namespace.
    ProcessOwnedEndpoint,
    /// One disposable interface whose address carries the endpoint.
    OwnedInterface,
    /// One disposable network namespace and its measured route/resolver path.
    OwnedNamespace,
}

impl FixtureScope {
    /// The exact claim that goes with a fact measured at this scope. It is
    /// written into every fact so a reader cannot mistake a loopback outage for
    /// a statement about the host's connectivity.
    pub fn claim(self) -> &'static str {
        match self {
            Self::ProcessOwnedEndpoint => {
                "one owned TCP endpoint in this process's network namespace; a stopped listener is a real refusal for that address and port only, never a statement about other endpoints, the host, or the wider network"
            }
            Self::OwnedInterface => {
                "one disposable interface address owned by this fixture; a link fault is a real local-delivery/route failure for that address only, never a statement about other interfaces or the host's uplink"
            }
            Self::OwnedNamespace => {
                "the fixture's own disposable network namespace; route and resolver observations are kernel lookups inside that namespace, not packet-level measurements and not statements about the host's namespace"
            }
        }
    }
}

/// The class of fault a caller wants to reason about when it asks whether a
/// fault could also have reached the model's channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultClass {
    /// The owned listener was closed.
    EndpointStopped,
    /// The owned interface carrying the endpoint was taken down.
    LinkDown,
    /// A blackhole route inside the owned namespace.
    NamespaceRoute,
    /// An unreachable resolver inside the owned namespace.
    NamespaceResolver,
}

/// How one capability was admitted, and why it was not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapabilityState {
    /// The fixture measured that it can do this.
    Admitted { detail: String },
    /// The host refused. The string is the real prerequisite, not a guess.
    Rejected { prerequisite: String },
    /// The caller did not ask for this tier.
    NotAttempted,
}

impl CapabilityState {
    pub fn admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }
}

/// One executed command, retained verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandObservation {
    pub argv: Vec<String>,
    pub exit_status: Option<i32>,
    pub succeeded: bool,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
}

/// What this fixture measured about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureCapabilities {
    pub ip: CapabilityState,
    pub endpoint: CapabilityState,
    pub interface: CapabilityState,
    pub namespace: CapabilityState,
    pub namespace_dns: CapabilityState,
    pub getent: CapabilityState,
}

/// What tiers to attempt. Tiers are attempted, never assumed.
#[derive(Debug, Clone)]
pub struct FixtureConfig {
    pub interface: bool,
    pub namespace: bool,
    /// A hostname this environment's resolver actually resolves. It is used
    /// only to measure the owned namespace's resolver path; `None` leaves the
    /// resolver tier unsupported rather than inventing a name that happens to
    /// fail for an unrelated reason.
    pub resolvable_name: Option<String>,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self { interface: true, namespace: true, resolvable_name: None }
    }
}

impl FixtureConfig {
    /// The tier that needs no privilege.
    pub fn endpoint_only() -> Self {
        Self { interface: false, namespace: false, resolvable_name: None }
    }
}

/// Whether the owned endpoint is listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointMode {
    Serving,
    Stopped,
}

/// Whether the owned interface is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkMode {
    Up,
    Down,
}

/// One production `net.probe` measurement of a declared target, with the exact
/// scope it is allowed to claim.
///
/// This is a measurement of one attempt, not a cognitive record. It does not
/// become a piece of content, it carries no attitude and no confidence, and the
/// fixture never writes it into the ledger: the observation a subject records
/// is its own, and this receipt is only the harness's independent measurement of
/// the same path.
#[derive(Debug, Clone, Serialize)]
pub struct PathObservation {
    pub schema_version: u32,
    pub fixture_id: String,
    /// The caller's own label for the moment this was measured.
    pub condition: String,
    pub scope: FixtureScope,
    pub claim: String,
    pub endpoint_mode: EndpointMode,
    pub link_mode: Option<LinkMode>,
    pub target: String,
    /// The production observation. Fields the attempt never reached stay
    /// absent inside it; nothing here is filled in by the fixture.
    pub observation: ProbeObservation,
}

impl PathObservation {
    pub fn reachable(&self) -> bool {
        self.observation.reachable
    }
}

/// What kind of lookup produced one namespace observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceObservationKind {
    /// A kernel route lookup inside the owned namespace.
    RouteLookup,
    /// A resolver query executed inside the owned namespace.
    ResolverQuery,
}

/// One measured fault or restoration inside the owned namespace.
#[derive(Debug, Clone, Serialize)]
pub struct NamespaceObservation {
    pub schema_version: u32,
    pub fixture_id: String,
    pub condition: String,
    pub scope: FixtureScope,
    pub observation_kind: NamespaceObservationKind,
    pub claim: String,
    /// True only when the lookup itself showed a usable path. A withdrawn
    /// blackhole that leaves the namespace without a route is *not* a recovery
    /// to reachability, and says so here.
    pub reachable: bool,
    /// The classification this fixture derived, plus why.
    pub verdict: String,
    /// The measurement command, retained verbatim.
    pub command: CommandObservation,
    pub measured_at: chrono::DateTime<chrono::Utc>,
}

/// The complete retained record of one fixture run.
#[derive(Debug, Clone, Serialize)]
pub struct FixtureReceipt {
    pub schema_version: u32,
    pub fixture_id: String,
    pub endpoint: String,
    pub capabilities: FixtureCapabilities,
    pub observations: Vec<PathObservation>,
    pub namespace_observations: Vec<NamespaceObservation>,
    pub commands: Vec<CommandObservation>,
    pub claims: Vec<String>,
    /// What this receipt does not establish, stated by the fixture itself.
    pub unproved: Vec<String>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

struct ServingEndpoint {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

struct EndpointState {
    bind: SocketAddr,
    mode: EndpointMode,
    serving: Option<ServingEndpoint>,
    /// Connections this endpoint served over its whole lifetime, so the count
    /// survives a stop and a restart instead of resetting to zero.
    served: Arc<AtomicU64>,
}

struct InterfaceLease {
    name: String,
    address: Ipv4Addr,
    mode: LinkMode,
}

struct NamespaceLease {
    name: String,
    resolv_path: PathBuf,
    resolv_dir: PathBuf,
    /// True only when this fixture created `resolv_dir` and may remove it.
    own_resolv_dir: bool,
    /// The host's own resolver configuration captured at admission. It is the
    /// only thing the resolver tier ever restores; nothing is invented.
    host_resolv: Option<String>,
    /// Whether the namespace resolver was pointed at the unreachable server.
    resolver_broken: bool,
    /// Whether the fixture's blackhole route is currently installed.
    route_broken: bool,
}

/// An owned, disposable network fixture.
///
/// Construction attempts the requested tiers in order and records what the host
/// actually allowed. Every method that changes the fixture returns the retained
/// command observations for that change; every method that measures returns a
/// typed fact carrying its own claim.
pub struct OwnedNetworkFixture {
    id: String,
    root: PathBuf,
    commands: Mutex<Vec<CommandObservation>>,
    endpoint: Mutex<EndpointState>,
    capabilities: FixtureCapabilities,
    /// The admitted interface whose link this fixture may take down. It is a
    /// lease: `set_interface` mutates the recorded mode, so the lease sits
    /// behind the same lock as every other owned resource.
    interface: Mutex<Option<InterfaceLease>>,
    namespace: Mutex<Option<NamespaceLease>>,
    /// The hostname supplied for the resolver tier, if any. `None` is why the
    /// tier can never be admitted: a failure without a name that resolves
    /// proves nothing about resolvers.
    config_name: Option<String>,
    cleaned: bool,
}

impl OwnedNetworkFixture {
    /// Create the fixture below `root`.
    ///
    /// `root` keeps the retained command captures, so the fixture's evidence
    /// survives the process. Nothing outside `root`, the fixture's own
    /// interface and the fixture's own namespace is touched.
    pub fn open(root: impl AsRef<Path>, config: FixtureConfig) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("commands"))
            .with_context(|| format!("create fixture command capture directory under {}", root.display()))?;
        let id = format!("fixture-{}", EntityId::new());
        let short = short_hex(&id);
        let mut fixture = Self {
            id,
            root,
            commands: Mutex::new(Vec::new()),
            endpoint: Mutex::new(EndpointState {
                bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                mode: EndpointMode::Stopped,
                serving: None,
                served: Arc::new(AtomicU64::new(0)),
            }),
            capabilities: FixtureCapabilities {
                ip: CapabilityState::NotAttempted,
                endpoint: CapabilityState::NotAttempted,
                interface: CapabilityState::NotAttempted,
                namespace: CapabilityState::NotAttempted,
                namespace_dns: CapabilityState::NotAttempted,
                getent: CapabilityState::NotAttempted,
            },
            interface: Mutex::new(None),
            namespace: Mutex::new(None),
            config_name: config.resolvable_name.clone(),
            cleaned: false,
        };
        if config.namespace {
            fixture.capabilities.getent = fixture.detect_getent();
        }
        if config.interface || config.namespace {
            fixture.capabilities.ip = fixture.detect_ip();
        }
        if config.interface {
            match fixture.admit_interface(&short) {
                Ok(lease) => {
                    let address = lease.address;
                    fixture.capabilities.interface = CapabilityState::Admitted {
                        detail: format!(
                            "disposable link {} carries {}; the endpoint binds to that address",
                            lease.name,
                            address
                        ),
                    };
                    *fixture.interface.lock() = Some(lease);
                    fixture.endpoint.lock().bind = SocketAddr::new(IpAddr::V4(address), 0);
                }
                Err(prerequisite) => {
                    fixture.capabilities.interface = CapabilityState::Rejected { prerequisite };
                }
            }
        }
        if config.namespace {
            match fixture.admit_namespace(&short) {
                Ok(mut lease) => {
                    fixture.capabilities.namespace = CapabilityState::Admitted {
                        detail: format!(
                            "disposable namespace {} measured with `ip -n {}` lookups",
                            lease.name, lease.name
                        ),
                    };
                    fixture.capabilities.namespace_dns =
                        fixture.admit_namespace_dns(&mut lease);
                    *fixture.namespace.lock() = Some(lease);
                }
                Err(prerequisite) => {
                    let dependent = format!(
                        "the namespace tier itself was not admitted: {prerequisite}"
                    );
                    fixture.capabilities.namespace = CapabilityState::Rejected { prerequisite };
                    fixture.capabilities.namespace_dns = CapabilityState::Rejected {
                        prerequisite: dependent,
                    };
                }
            }
        }
        Ok(fixture)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn capabilities(&self) -> &FixtureCapabilities {
        &self.capabilities
    }

    pub fn command_log(&self) -> Vec<CommandObservation> {
        self.commands.lock().clone()
    }

    /// The endpoint's bound address. It exists from `open` onward: the socket is
    /// bound by `set_endpoint(Serving)` but the address is fixed here so the
    /// declared target cannot drift between the broken and recovered states.
    pub fn endpoint_addr(&self) -> SocketAddr {
        self.endpoint.lock().bind
    }

    /// The URL the subject is told about. It names the fixture's own address and
    /// port, so a probe of it is a probe of exactly this endpoint.
    pub fn declared_url(&self) -> String {
        format!("http://{}/probe", self.endpoint_addr())
    }

    pub fn endpoint_mode(&self) -> EndpointMode {
        self.endpoint.lock().mode
    }

    pub fn link_mode(&self) -> Option<LinkMode> {
        self.interface.lock().as_ref().map(|lease| lease.mode)
    }

    /// How many connections the owned endpoint actually served. It stays at
    /// its last value while the listener is stopped: zero after a stop means
    /// the endpoint never accepted anything, not that the count was dropped.
    pub fn served_requests(&self) -> u64 {
        self.endpoint.lock().served.load(Ordering::SeqCst)
    }

    /// Start or stop the owned listener. Stopping closes the socket and joins
    /// the accept loop, so a following measurement really is refused by the
    /// kernel instead of timing out against a half-open backlog.
    pub fn set_endpoint(&self, mode: EndpointMode) -> Result<Vec<CommandObservation>> {
        let mut state = self.endpoint.lock();
        if state.mode == mode {
            return Ok(self.command_log());
        }
        match mode {
            EndpointMode::Stopped => {
                if let Some(mut serving) = state.serving.take() {
                    serving.stop.store(true, Ordering::SeqCst);
                    if let Some(handle) = serving.handle.take() {
                        let _ = handle.join();
                    }
                }
                state.mode = EndpointMode::Stopped;
            }
            EndpointMode::Serving => {
                let listener = TcpListener::bind(state.bind).with_context(|| {
                    format!("bind the owned endpoint at {}", state.bind)
                })?;
                listener.set_nonblocking(true).context("set owned endpoint non-blocking")?;
                let bound = listener.local_addr().context("read the owned endpoint address")?;
                state.bind = bound;
                let stop = Arc::new(AtomicBool::new(false));
                let thread_stop = Arc::clone(&stop);
                let thread_served = Arc::clone(&state.served);
                let handle = thread::Builder::new()
                    .name("shenko-net-fixture-endpoint".into())
                    .spawn(move || serve_owned_endpoint(listener, thread_stop, thread_served))
                    .context("spawn the owned endpoint accept loop")?;
                state.serving = Some(ServingEndpoint {
                    stop,
                    handle: Some(handle),
                });
                state.mode = EndpointMode::Serving;
            }
        }
        Ok(self.command_log())
    }

    /// Bring the owned interface up or down. The failure is a real
    /// local-delivery failure for the endpoint's own address; the model's
    /// channel is not this interface unless the caller proves it is.
    pub fn set_interface(&self, mode: LinkMode) -> Result<Vec<CommandObservation>> {
        let mut guard = self.interface.lock();
        let lease = guard
            .as_mut()
            .context("the fixture has no admitted interface; see capabilities().interface")?;
        if lease.mode == mode {
            drop(guard);
            return Ok(self.command_log());
        }
        let verb = match mode {
            LinkMode::Up => "up",
            LinkMode::Down => "down",
        };
        let observation = self.run(&["ip", "link", "set", &lease.name, verb], MAX_COMMAND_TIMEOUT);
        ensure!(
            observation.succeeded,
            "`ip link set {} {verb}` failed: {}",
            lease.name,
            first_line(&observation.stderr, &observation.stdout)
        );
        lease.mode = mode;
        drop(guard);
        Ok(self.command_log())
    }

    /// One production `net.probe` measurement of a declared URL.
    ///
    /// This is the same `freeze`/`measure` pair the native `net.probe`
    /// definition runs, so an observation here and a subject's delegated
    /// observation are the same kind of measurement.
    pub fn observe_url(
        &self,
        condition: &str,
        url: &str,
        scope: FixtureScope,
    ) -> Result<PathObservation> {
        let frozen = ProbeObservation::freeze(&ProbeInput {
            url: url.to_string(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })?;
        let observation = ProbeObservation::measure(&frozen);
        Ok(PathObservation {
            schema_version: FIXTURE_SCHEMA_VERSION,
            fixture_id: self.id.clone(),
            condition: condition.to_string(),
            scope,
            claim: scope.claim().to_string(),
            endpoint_mode: self.endpoint_mode(),
            link_mode: self.link_mode(),
            target: url.to_string(),
            observation,
        })
    }

    /// Measure the fixture's own declared endpoint at its current state.
    pub fn observe_endpoint(&self, condition: &str) -> Result<PathObservation> {
        let scope = if self.interface.lock().is_some() {
            FixtureScope::OwnedInterface
        } else {
            FixtureScope::ProcessOwnedEndpoint
        };
        self.observe_url(condition, &self.declared_url(), scope)
    }

    /// A URL whose name is reserved never to resolve (RFC 2606 `.invalid`). This
    /// measures the resolver's real negative answer for that name; it is not a
    /// broken resolver and the fixture never claims it is.
    pub fn unresolvable_url(&self) -> String {
        format!("http://{}.invalid:80/probe", short_hex(&self.id))
    }

    /// Measure the reserved-name resolver fact. Failure-only: a name reserved
    /// never to resolve has no recovery to a reachable path, and none is
    /// claimed.
    pub fn observe_unresolvable_name(&self) -> Result<PathObservation> {
        self.observe_url(
            "reserved-name-unresolvable",
            &self.unresolvable_url(),
            FixtureScope::ProcessOwnedEndpoint,
        )
    }

    /// Install the fixture's blackhole route inside its own namespace and
    /// measure the kernel's route lookup for one address inside it.
    pub fn break_namespace_route(&self) -> Result<NamespaceObservation> {
        let mut guard = self.namespace.lock();
        let namespace = guard
            .as_mut()
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        let added = self.run(
            &["ip", "-n", &namespace.name, "route", "add", "blackhole", BLACKHOLE_NETWORK],
            MAX_COMMAND_TIMEOUT,
        );
        ensure!(
            added.succeeded,
            "`ip -n {} route add blackhole {BLACKHOLE_NETWORK}` failed: {}",
            namespace.name,
            first_line(&added.stderr, &added.stdout)
        );
        namespace.route_broken = true;
        drop(guard);
        let fact = self.measure_namespace_route("namespace-route-blackhole")?;
        ensure!(!fact.reachable, "the installed blackhole route still reported a reachable path");
        Ok(fact)
    }

    /// Withdraw the fixture's blackhole route and measure the lookup again.
    ///
    /// The namespace has no other route to that network, so the lookup after the
    /// withdrawal reports "unreachable" as well. That is recorded as what it is:
    /// the fixture's own entry is gone, and the namespace is still isolated.
    pub fn restore_namespace_route(&self) -> Result<NamespaceObservation> {
        let mut guard = self.namespace.lock();
        let namespace = guard
            .as_mut()
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        let removed = self.run(
            &["ip", "-n", &namespace.name, "route", "del", "blackhole", BLACKHOLE_NETWORK],
            MAX_COMMAND_TIMEOUT,
        );
        ensure!(
            removed.succeeded,
            "`ip -n {} route del blackhole {BLACKHOLE_NETWORK}` failed: {}",
            namespace.name,
            first_line(&removed.stderr, &removed.stdout)
        );
        namespace.route_broken = false;
        drop(guard);
        self.measure_namespace_route("namespace-route-withdrawn")
    }

    /// Point the owned namespace's resolver at an unreachable nameserver
    /// (TEST-NET-1) and measure a real resolver query inside that namespace.
    ///
    /// Requires the caller to have supplied [`FixtureConfig::resolvable_name`]:
    /// without a name this environment actually resolves, a failure proves
    /// nothing about resolvers.
    pub fn break_namespace_dns(&self) -> Result<NamespaceObservation> {
        ensure!(
            self.capabilities.namespace_dns.admitted(),
            "the fixture's namespace resolver tier is not admitted: {}",
            capability_reason(&self.capabilities.namespace_dns)
        );
        let mut guard = self.namespace.lock();
        let namespace = guard
            .as_mut()
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        fs::write(
            &namespace.resolv_path,
            format!("nameserver {UNREACHABLE_RESOLVER}\noptions timeout:1 attempts:1\n"),
        )
        .with_context(|| format!("write {}", namespace.resolv_path.display()))?;
        namespace.resolver_broken = true;
        drop(guard);
        self.measure_namespace_resolver("namespace-resolver-unreachable")
    }

    /// Restore the owned namespace's resolver to the host configuration this
    /// fixture captured when it was admitted, and measure the same query again.
    pub fn restore_namespace_dns(&self) -> Result<NamespaceObservation> {
        ensure!(
            self.capabilities.namespace_dns.admitted(),
            "the fixture's namespace resolver tier is not admitted: {}",
            capability_reason(&self.capabilities.namespace_dns)
        );
        let mut guard = self.namespace.lock();
        let namespace = guard
            .as_mut()
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        let host_resolv = namespace
            .host_resolv
            .as_ref()
            .context("the fixture captured no host resolver configuration to restore")?;
        fs::write(&namespace.resolv_path, host_resolv)
            .with_context(|| format!("write {}", namespace.resolv_path.display()))?;
        namespace.resolver_broken = false;
        drop(guard);
        self.measure_namespace_resolver("namespace-resolver-host-default")
    }

    /// Whether `fault` on this fixture could also have taken down a channel to
    /// `wire`. This is the structural half of the distinctness check; the
    /// measured half is [`ChannelObservation::measure`] connecting while the
    /// fault is active.
    ///
    /// The structural comparison is deliberately literal: a host name is not
    /// resolved here, so a name that resolves to the fixture's own address is
    /// caught by the measured half instead of by a guess.
    pub fn fault_reaches_wire(&self, wire: &ChannelOrigin, fault: FaultClass) -> bool {
        match fault {
            // Closing one listener closes that socket only. Another port, or
            // another address, is untouched even when the wire shares this
            // host's interfaces and resolver.
            FaultClass::EndpointStopped => {
                let endpoint = self.endpoint_addr();
                wire.port == endpoint.port()
                    && wire.host.parse::<IpAddr>().is_ok_and(|ip| ip == endpoint.ip())
            }
            // A link fault is confined to the fixture's own interface. If the
            // model channel is that same address, it is not distinct.
            FaultClass::LinkDown => self
                .interface
                .lock()
                .as_ref()
                .is_some_and(|lease| lease.address.to_string() == wire.host),
            // Namespace faults are confined to a namespace this process is not
            // in, and the channel is opened from this process.
            FaultClass::NamespaceRoute | FaultClass::NamespaceResolver => false,
        }
    }

    /// Assemble the retained receipt from the observations the caller measured.
    pub fn receipt(
        &self,
        observations: Vec<PathObservation>,
        namespace_observations: Vec<NamespaceObservation>,
    ) -> FixtureReceipt {
        let mut claims = vec![
            FixtureScope::ProcessOwnedEndpoint.claim().to_string(),
            FixtureScope::OwnedInterface.claim().to_string(),
            FixtureScope::OwnedNamespace.claim().to_string(),
            "these are mechanism observations of single attempts; they are not content, not an attitude, carry no confidence, and do not demonstrate an awareness capability".to_string(),
        ];
        if !self.capabilities.interface.admitted() {
            claims.push(format!(
                "interface tier not admitted: {}",
                capability_reason(&self.capabilities.interface)
            ));
        }
        if !self.capabilities.namespace.admitted() {
            claims.push(format!(
                "namespace tier not admitted: {}",
                capability_reason(&self.capabilities.namespace)
            ));
        }
        if !self.capabilities.namespace_dns.admitted() {
            claims.push(format!(
                "namespace resolver tier not admitted: {}",
                capability_reason(&self.capabilities.namespace_dns)
            ));
        }
        claims.sort();
        claims.dedup();
        FixtureReceipt {
            schema_version: FIXTURE_SCHEMA_VERSION,
            fixture_id: self.id.clone(),
            endpoint: self.declared_url(),
            capabilities: self.capabilities.clone(),
            observations,
            namespace_observations,
            commands: self.command_log(),
            claims,
            unproved: vec![
                "a fault at this fixture's scope is not evidence about any other endpoint, interface, namespace or the host's uplink".into(),
                "the reserved `.invalid` name measures one negative resolver answer, not a broken resolver".into(),
                "a withdrawn blackhole route leaves the owned namespace without a route to that network; it is not a recovery to reachability".into(),
                "no packet-level capture was taken; route and resolver observations are kernel lookups inside the owned namespace".into(),
            ],
            observed_at: chrono::Utc::now(),
        }
    }

    /// Seal the receipt into the installation's CAS so the exact observations
    /// are immutable provenance next to the trial that used them, and can serve
    /// as the receipt terminal of a reason that stops there.
    pub fn seal(store: &Store, receipt: &FixtureReceipt) -> Result<ObjectRef> {
        let bytes = serde_json::to_vec(receipt).context("serialize the network fixture receipt")?;
        Ok(store
            .cas
            .seal_object(&bytes, "application/json", "network-fixture", "network-fixture-receipt", 1)
            .map_err(|error| {
                store.close_external_writer();
                error
            })?
            .object)
    }

    /// Remove everything this fixture created and return the commands it ran.
    pub fn cleanup(mut self) -> Result<Vec<CommandObservation>> {
        self.remove_owned_resources();
        self.cleaned = true;
        Ok(self.command_log())
    }

    fn remove_owned_resources(&self) {
        if let Some(mut serving) = self.endpoint.lock().serving.take() {
            serving.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = serving.handle.take() {
                let _ = handle.join();
            }
        }
        // The route and the resolver configuration are withdrawn before the
        // namespace goes away, so a partially removed netns never keeps the
        // fixture's blackhole or its unreachable nameserver alive.
        let namespace = self.namespace.lock().take();
        if let Some(namespace) = namespace {
            if namespace.route_broken {
                let _ = self.run(
                    &["ip", "-n", &namespace.name, "route", "del", "blackhole", BLACKHOLE_NETWORK],
                    MAX_COMMAND_TIMEOUT,
                );
            }
            if namespace.resolver_broken {
                if let Some(host_resolv) = &namespace.host_resolv {
                    let _ = fs::write(&namespace.resolv_path, host_resolv);
                }
            }
            let _ = self.run(&["ip", "netns", "del", &namespace.name], MAX_COMMAND_TIMEOUT);
            if namespace.own_resolv_dir {
                let _ = fs::remove_file(&namespace.resolv_path);
                let _ = fs::remove_dir(&namespace.resolv_dir);
            }
        }
        // Deleting one end of the pair deletes both; the peer name is only ever
        // used at creation.
        let interface = self.interface.lock().take();
        if let Some(interface) = interface {
            let _ = self.run(&["ip", "link", "del", &interface.name], MAX_COMMAND_TIMEOUT);
        }
    }

    fn measure_namespace_route(&self, condition: &str) -> Result<NamespaceObservation> {
        let name = self
            .namespace
            .lock()
            .as_ref()
            .map(|namespace| namespace.name.clone())
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        let command = self.run(
            &["ip", "-n", &name, "route", "get", BLACKHOLE_ADDRESS],
            Duration::from_secs(3),
        );
        let output = format!("{}{}", command.stdout, command.stderr);
        let blackholed = output.contains("blackhole");
        let reachable = command.succeeded && !blackholed;
        let verdict = if blackholed {
            "the owned namespace resolves this address to the fixture's blackhole route".to_string()
        } else if reachable {
            "the owned namespace resolves this address to a usable route".to_string()
        } else {
            format!(
                "the owned namespace has no usable route to this address: {}",
                first_line(&command.stderr, &command.stdout)
            )
        };
        Ok(NamespaceObservation {
            schema_version: FIXTURE_SCHEMA_VERSION,
            fixture_id: self.id.clone(),
            condition: condition.to_string(),
            scope: FixtureScope::OwnedNamespace,
            observation_kind: NamespaceObservationKind::RouteLookup,
            claim: FixtureScope::OwnedNamespace.claim().to_string(),
            reachable,
            verdict,
            command,
            measured_at: chrono::Utc::now(),
        })
    }

    fn measure_namespace_resolver(&self, condition: &str) -> Result<NamespaceObservation> {
        let namespace = self
            .namespace
            .lock()
            .as_ref()
            .map(|namespace| namespace.name.clone())
            .context("the fixture has no admitted namespace; see capabilities().namespace")?;
        let host_name = self
            .resolvable_name()
            .context("no resolvable hostname was supplied to measure the namespace resolver")?;
        let command = self.run(
            &["ip", "netns", "exec", &namespace, "getent", "hosts", host_name],
            MAX_COMMAND_TIMEOUT,
        );
        let reachable = command.succeeded && !command.stdout.trim().is_empty();
        let verdict = if reachable {
            format!(
                "a resolver query executed inside the owned namespace answered for {host_name}"
            )
        } else {
            format!(
                "a resolver query executed inside the owned namespace produced no answer for {host_name}: {}",
                first_line(&command.stderr, &command.stdout)
            )
        };
        Ok(NamespaceObservation {
            schema_version: FIXTURE_SCHEMA_VERSION,
            fixture_id: self.id.clone(),
            condition: condition.to_string(),
            scope: FixtureScope::OwnedNamespace,
            observation_kind: NamespaceObservationKind::ResolverQuery,
            claim: FixtureScope::OwnedNamespace.claim().to_string(),
            reachable,
            verdict,
            command,
            measured_at: chrono::Utc::now(),
        })
    }

    fn resolvable_name(&self) -> Option<&str> {
        self.config_name.as_deref()
    }

    fn detect_getent(&self) -> CapabilityState {
        let observation = self.run(&["getent", "--version"], Duration::from_secs(2));
        if observation.timed_out {
            return CapabilityState::Rejected {
                prerequisite: "`getent` did not answer within 2s".into(),
            };
        }
        if observation.succeeded {
            CapabilityState::Admitted {
                detail: first_line(&observation.stdout, &observation.stderr),
            }
        } else {
            // A resolver query the fixture cannot run is not a measured
            // negative answer about any name.
            CapabilityState::Rejected {
                prerequisite: format!(
                    "`getent` is required to observe resolver answers and did not succeed: {}",
                    first_line(&observation.stderr, &observation.stdout)
                ),
            }
        }
    }

    fn detect_ip(&self) -> CapabilityState {
        let observation = self.run(&["ip", "-V"], Duration::from_secs(3));
        if observation.succeeded {
            CapabilityState::Admitted {
                detail: first_line(&observation.stdout, &observation.stderr),
            }
        } else {
            CapabilityState::Rejected {
                prerequisite: format!(
                    "iproute2 `ip` is required for interface/namespace faults and did not answer: {}",
                    first_line(&observation.stderr, &observation.stdout)
                ),
            }
        }
    }

    fn admit_interface(&self, short: &str) -> std::result::Result<InterfaceLease, String> {
        match &self.capabilities.ip {
            CapabilityState::Admitted { .. } => {}
            other => {
                return Err(format!(
                    "CAP_NET_ADMIN and iproute2 are required for an owned interface: {}",
                    capability_reason(other)
                ));
            }
        }
        let name = format!("shkfx{short}");
        let peer = format!("shkfxp{short}");
        let address = owned_interface_address(short)?;
        let created = self.run(
            &["ip", "link", "add", &name, "type", "veth", "peer", "name", &peer],
            MAX_COMMAND_TIMEOUT,
        );
        if !created.succeeded {
            return Err(format!(
                "`ip link add {name} type veth` was refused: {}",
                first_line(&created.stderr, &created.stdout)
            ));
        }
        let addressed = self.run(
            &["ip", "addr", "add", &format!("{address}/32"), "dev", &name],
            MAX_COMMAND_TIMEOUT,
        );
        if !addressed.succeeded {
            let _ = self.run(&["ip", "link", "del", &name], MAX_COMMAND_TIMEOUT);
            return Err(format!(
                "`ip addr add {address}/32 dev {name}` was refused: {}",
                first_line(&addressed.stderr, &addressed.stdout)
            ));
        }
        let brought_up = self.run(&["ip", "link", "set", &name, "up"], MAX_COMMAND_TIMEOUT);
        if !brought_up.succeeded {
            let _ = self.run(&["ip", "link", "del", &name], MAX_COMMAND_TIMEOUT);
            return Err(format!(
                "`ip link set {name} up` was refused: {}",
                first_line(&brought_up.stderr, &brought_up.stdout)
            ));
        }
        Ok(InterfaceLease { name, address, mode: LinkMode::Up })
    }

    fn admit_namespace(&self, short: &str) -> std::result::Result<NamespaceLease, String> {
        match &self.capabilities.ip {
            CapabilityState::Admitted { .. } => {}
            other => {
                return Err(format!(
                    "CAP_NET_ADMIN and iproute2 are required for an owned namespace: {}",
                    capability_reason(other)
                ));
            }
        }
        let name = format!("shkns{short}");
        let created = self.run(&["ip", "netns", "add", &name], MAX_COMMAND_TIMEOUT);
        if !created.succeeded {
            return Err(format!(
                "`ip netns add {name}` was refused: {}",
                first_line(&created.stderr, &created.stdout)
            ));
        }
        let loopback = self.run(&["ip", "-n", &name, "link", "set", "lo", "up"], MAX_COMMAND_TIMEOUT);
        if !loopback.succeeded {
            let _ = self.run(&["ip", "netns", "del", &name], MAX_COMMAND_TIMEOUT);
            return Err(format!(
                "`ip -n {name} link set lo up` was refused: {}",
                first_line(&loopback.stderr, &loopback.stdout)
            ));
        }
        let resolv_dir = PathBuf::from(format!("/etc/netns/{name}"));
        let resolv_path = resolv_dir.join("resolv.conf");
        let own_resolv_dir = match fs::create_dir_all(&resolv_dir) {
            Ok(()) => true,
            Err(_) => false,
        };
        let lease = NamespaceLease {
            name,
            resolv_path,
            resolv_dir,
            own_resolv_dir,
            host_resolv: None,
            resolver_broken: false,
            route_broken: false,
        };
        Ok(lease)
    }

    /// Whether the resolver tier can be measured at all, decided by measuring
    /// it before any fault.
    ///
    /// One query against the host's own nameservers has to answer *first*. If
    /// it does not, the owned namespace has no usable resolver path, and the
    /// tier is rejected with that measurement as its prerequisite: a fixture
    /// that cannot reach a nameserver must not claim it broke one.
    fn admit_namespace_dns(&self, lease: &mut NamespaceLease) -> CapabilityState {
        if !self.capabilities.getent.admitted() {
            return CapabilityState::Rejected {
                prerequisite: format!(
                    "the resolver tier needs `getent` to observe answers: {}",
                    capability_reason(&self.capabilities.getent)
                ),
            };
        }
        let Some(name) = self.resolvable_name() else {
            return CapabilityState::Rejected {
                prerequisite: "no resolvable hostname was supplied; a failure against a name this host does not resolve proves nothing about resolvers".into(),
            };
        };
        let host_resolv = match host_resolver_configuration() {
            Ok(configuration) => configuration,
            Err(prerequisite) => return CapabilityState::Rejected { prerequisite },
        };
        if !lease.own_resolv_dir {
            return CapabilityState::Rejected {
                prerequisite: format!(
                    "the fixture cannot create {} to give the owned namespace its own resolver configuration",
                    lease.resolv_dir.display()
                ),
            };
        }
        if let Err(error) = fs::write(&lease.resolv_path, &host_resolv) {
            return CapabilityState::Rejected {
                prerequisite: format!("write {}: {error}", lease.resolv_path.display()),
            };
        }
        let baseline = self.run(
            &["ip", "netns", "exec", &lease.name, "getent", "hosts", name],
            MAX_COMMAND_TIMEOUT,
        );
        if baseline.succeeded && !baseline.stdout.trim().is_empty() {
            // The captured configuration is exactly the one this baseline was
            // measured with, so restoring it later is a real restoration.
            lease.host_resolv = Some(host_resolv);
            CapabilityState::Admitted {
                detail: format!(
                    "a resolver query inside {} answered for {name} before any fault: {}",
                    lease.name,
                    first_line(&baseline.stdout, &baseline.stderr)
                ),
            }
        } else {
            CapabilityState::Rejected {
                prerequisite: format!(
                    "a resolver query inside the owned namespace produced no answer for {name} before any fault, so a resolver fault cannot be isolated: {}",
                    first_line(&baseline.stderr, &baseline.stdout)
                ),
            }
        }
    }

    fn run(&self, argv: &[&str], timeout: Duration) -> CommandObservation {
        let observation = run_bounded(argv, timeout, &self.root.join("commands"));
        self.commands.lock().push(observation.clone());
        observation
    }
}

impl Drop for OwnedNetworkFixture {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        self.remove_owned_resources();
    }
}

/// The sealed model-channel origin, parsed by the production `net.probe`
/// parser so the fixture and the subject read one URL dialect.
///
/// This is the address the model's requests are sent to. A network-change
/// observation is only about a *different* path when this origin is measured
/// reachable while the fixture's target is not.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelOrigin {
    /// Scheme + host + port exactly as the binding declares them.
    pub origin: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl ChannelOrigin {
    /// Derive the origin from the sealed binding. The offline stub has no wire
    /// and is refused rather than measured against nothing.
    pub fn from_binding(binding: &ProviderBinding) -> Result<Self> {
        let origin = match binding {
            ProviderBinding::Local { base_url, .. }
            | ProviderBinding::Anthropic { base_url, .. } => base_url.clone(),
            ProviderBinding::Profile { profile } => profile.endpoint_origin.clone(),
            ProviderBinding::Stub { .. } => {
                bail!(
                    "the broker binding is the offline stub; a channel distinctness observation needs a live wire"
                )
            }
        };
        let declared = format!("{}/", origin.trim().trim_end_matches('/'));
        let frozen = ProbeObservation::freeze(&ProbeInput {
            url: declared,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
        .map_err(|error| error.context(format!("parse the sealed channel origin {origin}")))?;
        Ok(Self {
            origin,
            scheme: frozen.scheme,
            host: frozen.host,
            port: frozen.port,
        })
    }
}

/// One bounded TCP connect to the model channel's origin.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelObservation {
    pub origin: ChannelOrigin,
    pub connected: bool,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub claim: &'static str,
}

impl ChannelObservation {
    /// Open and drop one connection to the declared origin.
    ///
    /// It sends no request, presents no credential and learns nothing about the
    /// peer beyond whether the path currently accepts a connection.
    pub fn measure(origin: &ChannelOrigin, timeout: Duration) -> Self {
        let started = Instant::now();
        let mut observation = Self {
            origin: origin.clone(),
            connected: false,
            error: None,
            elapsed_ms: 0,
            claim: "one accepted TCP connection to the declared channel origin; no request was sent, no credential was presented, and success now does not prove continuity before or after",
        };
        let addresses: Vec<SocketAddr> =
            match (origin.host.as_str(), origin.port).to_socket_addrs_bounded(timeout) {
                Ok(addresses) => addresses,
                Err(error) => {
                    observation.error = Some(error);
                    observation.elapsed_ms = started.elapsed().as_millis() as u64;
                    return observation;
                }
            };
        for address in addresses {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    drop(stream);
                    observation.connected = true;
                    break;
                }
                Err(error) => observation.error = Some(error.to_string()),
            }
        }
        observation.elapsed_ms = started.elapsed().as_millis() as u64;
        observation
    }
}

/// Serve one owned endpoint: accept, answer one status line, close.
///
/// The response exists so the production probe's `http` stage has a real status
/// to read; the fixture never answers for a path that is down, because when the
/// listener is stopped there is no process to answer at all.
fn serve_owned_endpoint(listener: TcpListener, stop: Arc<AtomicBool>, served: Arc<AtomicU64>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                served.fetch_add(1, Ordering::SeqCst);
                answer_status_line(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn answer_status_line(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(_) => return,
    };
    let mut request = [0_u8; 512];
    let _ = reader.read(&mut request);
    let mut writer = stream;
    let _ = writer.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let _ = writer.flush();
}

fn owned_interface_address(short: &str) -> std::result::Result<Ipv4Addr, String> {
    // 198.18.0.0/15 is reserved for benchmarking; a /32 on a disposable link
    // cannot collide with a routed network.
    let first = u8::from_str_radix(short.get(0..2).unwrap_or("01"), 16).unwrap_or(1);
    let second = u8::from_str_radix(short.get(2..4).unwrap_or("02"), 16).unwrap_or(2);
    Ok(Ipv4Addr::new(198, 18, first, second.max(1)))
}

fn capability_reason(state: &CapabilityState) -> String {
    match state {
        CapabilityState::Admitted { detail } => format!("admitted: {detail}"),
        CapabilityState::Rejected { prerequisite } => prerequisite.clone(),
        CapabilityState::NotAttempted => "the caller did not ask for this tier".into(),
    }
}

/// This host's own resolver configuration, as the fixture will hand it to its
/// owned namespace and later restore.
///
/// Only `nameserver` lines are copied: they are the part a resolver query
/// depends on, and copying anything else would change the namespace's search
/// behaviour without a reason.
fn host_resolver_configuration() -> std::result::Result<String, String> {
    let host = fs::read_to_string("/etc/resolv.conf")
        .map_err(|error| format!("read the host resolver configuration: {error}"))?;
    let nameservers: Vec<&str> = host
        .lines()
        .filter(|line| line.trim_start().starts_with("nameserver"))
        .collect();
    if nameservers.is_empty() {
        return Err("the host resolver configuration names no nameserver to copy".into());
    }
    Ok(format!("{}\n", nameservers.join("\n")))
}

fn short_hex(id: &str) -> String {
    let hex: String = id.chars().filter(|character| character.is_ascii_hexdigit()).collect();
    if hex.len() >= 6 { hex[..6].to_string() } else { format!("{hex:0<6}") }
}

fn first_line(primary: &str, fallback: &str) -> String {
    let text = if primary.trim().is_empty() { fallback } else { primary };
    text.lines().next().unwrap_or("").trim().chars().take(240).collect()
}

fn read_bounded(path: &Path) -> String {
    let mut bytes = Vec::new();
    if let Ok(mut file) = fs::File::open(path) {
        let mut buffer = [0_u8; 1024];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let room = MAX_COMMAND_BYTES.saturating_sub(bytes.len());
                    if room == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count.min(room)]);
                }
                Err(_) => break,
            }
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// Run one command with a hard deadline, capturing stdout/stderr into `root`.
///
/// A command that outlives its deadline is killed and reaped; its capture files
/// stay next to the observation so a partial output is never presented as a
/// complete one.
fn run_bounded(argv: &[&str], timeout: Duration, root: &Path) -> CommandObservation {
    let started = Instant::now();
    let mut observation = CommandObservation {
        argv: argv.iter().map(|argument| argument.to_string()).collect(),
        exit_status: None,
        succeeded: false,
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
        elapsed_ms: 0,
    };
    let Some(program) = argv.first() else {
        observation.stderr = "no argv was supplied".into();
        return observation;
    };
    let stem = format!("cmd-{}", EntityId::new());
    let out_path = root.join(format!("{stem}.out"));
    let err_path = root.join(format!("{stem}.err"));
    let out_file = match fs::File::create(&out_path) {
        Ok(file) => file,
        Err(error) => {
            observation.stderr = format!("create {}: {error}", out_path.display());
            return observation;
        }
    };
    let err_file = match fs::File::create(&err_path) {
        Ok(file) => file,
        Err(error) => {
            observation.stderr = format!("create {}: {error}", err_path.display());
            return observation;
        }
    };
    let mut child = match Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            observation.stderr = format!("spawn {program}: {error}");
            observation.elapsed_ms = started.elapsed().as_millis() as u64;
            return observation;
        }
    };
    let deadline = started + timeout;
    let mut exit = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = Some(status);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    observation.timed_out = true;
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                observation.stderr = format!("wait for {program}: {error}");
                break;
            }
        }
    }
    observation.stdout = read_bounded(&out_path);
    let captured_stderr = read_bounded(&err_path);
    if !captured_stderr.is_empty() {
        observation.stderr = captured_stderr;
    }
    observation.exit_status = exit.and_then(|status| status.code());
    observation.succeeded = exit.is_some_and(|status| status.success()) && !observation.timed_out;
    observation.elapsed_ms = started.elapsed().as_millis() as u64;
    observation
}

/// Resolve a host/port for the channel measurement without waiting forever on a
/// nameserver that is not answering.
trait BoundedToSocketAddrs {
    fn to_socket_addrs_bounded(
        &self,
        timeout: Duration,
    ) -> std::result::Result<Vec<SocketAddr>, String>;
}

impl BoundedToSocketAddrs for (&str, u16) {
    fn to_socket_addrs_bounded(
        &self,
        timeout: Duration,
    ) -> std::result::Result<Vec<SocketAddr>, String> {
        use std::net::ToSocketAddrs as _;
        let (host, port) = *self;
        let (sender, receiver) = std::sync::mpsc::channel();
        let owned = (host.to_string(), port);
        thread::spawn(move || {
            let outcome = (owned.0.as_str(), owned.1)
                .to_socket_addrs()
                .map(|iter| iter.take(4).collect::<Vec<_>>())
                .map_err(|error| error.to_string());
            let _ = sender.send(outcome);
        });
        match receiver.recv_timeout(timeout) {
            Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
            Ok(Ok(_)) => Err("no addresses".into()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("resolution exceeded the channel-measurement bound".into()),
        }
    }
}

