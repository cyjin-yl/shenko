//! Governed generic network transport for WASI preview1 guests.
//!
//! Preview1 has no socket-creation call: the whole socket surface is
//! `sock_accept`/`sock_recv`/`sock_send`/`sock_shutdown` plus `fd_read`/
//! `fd_write` over a descriptor the host already opened. The host therefore
//! opens *target-bound* sockets from an explicit admission before the guest
//! starts and installs them as preopened stream sockets after the directory
//! preopens (see [`crate::p1::P1Host::install_network`]). Guest port code
//! discovers them through the host-owned `SHENKO_NET_<index>` environment
//! descriptors this module encodes.
//!
//! Nothing here shells out and nothing here is a native tool standing in for
//! a WASM one: the transport is a real TCP socket the host created, while the
//! guest program performs the application protocol (HTTP, Git wire, SSH,
//! ...) inside the module. Only endpoints an admission names can be reached,
//! and a guest can neither create, re-point nor widen one.
//!
//! Termination is host-owned: every blocking wait polls in bounded slices and
//! observes the run's [`RunStop`] (cancellation first, then deadline), so a
//! networked guest can never park a run in an uninterruptible host call. A
//! run carrying an admission must carry a deadline or a cancellation handle —
//! [`crate::runtime::WasmRuntime::run_wasi_module`] refuses an unbounded
//! networked run instead of trusting the peer to respond.
//!
//! Restore boundary: the guest's own linear
//! memory, execution stack and interpreter object graph belong to its owned
//! versioned root like any other guest state — networking does not take them
//! out. What no root can capture is the *peer*: a socket is a host OS resource
//! bound to a live connection, so a captured run that used transport is
//! restored with its transport explicitly unrebindable and left unresolved,
//! never silently reconnected and never replayed. Bytes a peer already
//! received are external effects: reconcile and record them, do not claim
//! they were undone.
use crate::runtime::{RunStop, WasiRunOutcome};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// Maximum number of targets one admission may name.
pub const MAX_ADMITTED_TARGETS: usize = 8;
/// Default ceiling on simultaneously open sockets for one run.
pub const DEFAULT_MAX_SOCKETS: usize = 8;
/// Default budget for one admitted connection attempt.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Host-side readiness poll slice. Bounds cancellation/deadline observation
/// latency for a guest blocked inside a socket host call.
pub const IO_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Root of the host-owned guest environment namespace that publishes the
/// installed socket descriptors. A caller may never supply these keys: the
/// host alone decides which descriptors exist, so a guest cannot be handed a
/// forged target list (and cannot be tricked into treating, say, its stdin
/// descriptor as a socket).
pub const GUEST_ENV_ROOT: &str = "SHENKO_NET";
const GUEST_ENV_PREFIX: &str = "SHENKO_NET_";

/// Whether `key` belongs to the host-owned guest network descriptor namespace.
pub fn is_reserved_env_key(key: &str) -> bool {
    key == GUEST_ENV_ROOT || key.starts_with(GUEST_ENV_PREFIX)
}

/// One operation a granted target permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkOperation {
    /// Open an outbound stream to the target's address.
    Connect,
    /// Bind a host listener on the target's address.
    Listen,
    /// Accept inbound streams on a listener the same target granted.
    Accept,
}

impl NetworkOperation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Listen => "listen",
            Self::Accept => "accept",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "connect" => Self::Connect,
            "listen" => Self::Listen,
            "accept" => Self::Accept,
            _ => return None,
        })
    }
}

/// One target an admission names, with the exact operations it permits.
///
/// The shape is the declared-target shape (`host`, `port`, `resolve`,
/// `operations`), so a caller maps its declaration list onto an admission one
/// field at a time instead of inventing a second vocabulary. `host` carries no
/// IPv6 brackets (`::1`, not `[::1]`); `resolve` chooses between host-side
/// name resolution and an IP literal that must parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedTarget {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub resolve: bool,
    pub operations: Vec<NetworkOperation>,
}

impl AdmittedTarget {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        resolve: bool,
        operations: Vec<NetworkOperation>,
    ) -> Result<Self> {
        let target = Self { host: host.into(), port, resolve, operations };
        target.validate()?;
        Ok(target)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty() && self.host.len() <= 512,
            "NETWORK_TARGET_INVALID: host must be 1..512 bytes"
        );
        ensure!(
            !self.host.chars().any(|c| c.is_control()
                || matches!(c, ',' | ';' | '=' | '|' | '[' | ']')
                || c.is_whitespace()),
            "NETWORK_TARGET_INVALID: host {:?} must not contain control, separator or whitespace \
             characters (IPv6 literals carry no brackets)",
            self.host
        );
        ensure!(self.port != 0, "NETWORK_TARGET_INVALID: port must not be 0");
        ensure!(
            !self.operations.is_empty(),
            "NETWORK_TARGET_INVALID: {}:{} names no operation",
            self.host,
            self.port
        );
        let mut seen: Vec<NetworkOperation> = Vec::with_capacity(self.operations.len());
        for operation in &self.operations {
            ensure!(
                !seen.contains(operation),
                "NETWORK_TARGET_INVALID: {}:{} repeats {}",
                self.host,
                self.port,
                operation.name()
            );
            seen.push(*operation);
        }
        ensure!(
            !(seen.contains(&NetworkOperation::Connect)
                && seen.contains(&NetworkOperation::Listen)),
            "NETWORK_TARGET_INVALID: {}:{} cannot both connect and listen; \
             a listener needs its own target entry",
            self.host,
            self.port
        );
        ensure!(
            !seen.contains(&NetworkOperation::Accept) || seen.contains(&NetworkOperation::Listen),
            "NETWORK_TARGET_INVALID: {}:{} grants accept without listen",
            self.host,
            self.port
        );
        if !self.resolve {
            let literal: IpAddr = self.host.parse().map_err(|_| {
                anyhow::anyhow!(
                    "NETWORK_TARGET_INVALID: {}:{} sets resolve=false but the host is not an IP literal",
                    self.host,
                    self.port
                )
            })?;
            ensure!(
                !literal.is_multicast(),
                "NETWORK_TARGET_INVALID: {} is a multicast address",
                self.host
            );
        }
        Ok(())
    }

    /// Whether this target permits `operation` for the run.
    pub fn permits(&self, operation: NetworkOperation) -> bool {
        self.operations.contains(&operation)
    }

    /// The guest descriptor published for this target: `fd,host,port,ops`.
    fn descriptor(&self, fd: u32) -> String {
        let mut operations: Vec<&str> = self.operations.iter().map(|op| op.name()).collect();
        operations.sort_unstable();
        format!("{fd},{},{},{ops}", self.host, self.port, ops = operations.join("+"))
    }
}

/// The exact network reach granted to one run: which targets, how many
/// sockets, and how long one connection attempt may take.
///
/// An admission is constructed from a granted declaration (or from installed
/// target facts) and is never widened by guest behaviour. It carries no
/// credentials: a credential reaches a guest only through the tool admission
/// that owns it (`SHENKO_TARGET_CREDENTIAL=none|stdin`), never through this
/// transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkAdmission {
    targets: Vec<AdmittedTarget>,
    max_sockets: usize,
    connect_timeout: Duration,
}

impl NetworkAdmission {
    /// Convert an invocation's explicit capability grant without performing
    /// DNS or opening a socket. This can run before T1; the runtime only opens
    /// the exact frozen targets after the attempt is armed. Package
    /// requirements do not create a grant.
    pub fn from_granted_targets(targets: &[shenko_node::NetworkTarget]) -> Result<Option<Self>> {
        if targets.is_empty() {
            return Ok(None);
        }
        let admitted = targets.iter().map(|target| {
            AdmittedTarget::new(
                target.host.clone(), target.port, target.resolve,
                target.operations.iter().map(|operation| match operation {
                    shenko_node::NetworkOperation::Connect {} => NetworkOperation::Connect,
                    shenko_node::NetworkOperation::Listen {} => NetworkOperation::Listen,
                    shenko_node::NetworkOperation::Accept {} => NetworkOperation::Accept,
                }).collect(),
            )
        }).collect::<Result<Vec<_>>>()?;
        Self::new(admitted).map(Some)
    }

    /// Admit exactly `targets`. Empty, over-large or self-contradictory
    /// admissions are refused: there is no implicit "any" reach.
    pub fn new(targets: Vec<AdmittedTarget>) -> Result<Self> {
        ensure!(
            !targets.is_empty(),
            "NETWORK_ADMISSION_INVALID: an admission must name at least one target"
        );
        ensure!(
            targets.len() <= MAX_ADMITTED_TARGETS,
            "NETWORK_ADMISSION_INVALID: {} targets exceeds the {MAX_ADMITTED_TARGETS} target ceiling",
            targets.len()
        );
        for target in &targets {
            target.validate()?;
        }
        let max_sockets = targets.len().max(1);
        Ok(Self { targets, max_sockets, connect_timeout: DEFAULT_CONNECT_TIMEOUT })
    }

    /// The admission for one installed tool target: exactly one outbound
    /// stream to `host:port`, no listening.
    ///
    /// This is the constructor the native tool path uses for a target its
    /// admission layer already selected, so no caller-composed endpoint text
    /// can reach the transport. `host` is whatever the installer recorded: an
    /// IP literal is used verbatim, a name is resolved by this host for that
    /// one admitted target (never for guest-supplied text).
    pub fn for_installed_target(host: &str, port: u16) -> Result<Self> {
        let literal = host.parse::<IpAddr>().is_ok();
        Self::new(vec![AdmittedTarget::new(
            host,
            port,
            !literal,
            vec![NetworkOperation::Connect],
        )?])
    }

    /// Raise or lower the ceiling on simultaneously open sockets for this run.
    pub fn with_max_sockets(mut self, max_sockets: usize) -> Result<Self> {
        ensure!(
            max_sockets >= self.targets.len(),
            "NETWORK_ADMISSION_INVALID: max_sockets {max_sockets} is below the {} admitted targets",
            self.targets.len()
        );
        ensure!(
            max_sockets <= MAX_ADMITTED_TARGETS,
            "NETWORK_ADMISSION_INVALID: max_sockets {max_sockets} exceeds the {MAX_ADMITTED_TARGETS} ceiling"
        );
        self.max_sockets = max_sockets;
        Ok(self)
    }

    /// Bound one connection attempt (never above a live run deadline).
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Result<Self> {
        ensure!(
            !connect_timeout.is_zero() && connect_timeout <= Duration::from_secs(120),
            "NETWORK_ADMISSION_INVALID: connect timeout must be in 1ns..=120s"
        );
        self.connect_timeout = connect_timeout;
        Ok(self)
    }

    pub fn targets(&self) -> &[AdmittedTarget] {
        &self.targets
    }

    pub fn max_sockets(&self) -> usize {
        self.max_sockets
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The guest environment descriptors for `fds`, in admission order.
    ///
    /// One key per installed socket: `SHENKO_NET_<index>=<fd>,<host>,<port>,
    /// <ops>`. Values never contain whitespace, so a guest shim can parse them
    /// without shell-like quoting rules.
    pub fn guest_environment(&self, fds: &[u32]) -> Result<Vec<(String, String)>> {
        ensure!(
            fds.len() == self.targets.len(),
            "NETWORK_INSTALL_INVALID: {} descriptors for {} admitted targets",
            fds.len(),
            self.targets.len()
        );
        Ok(self
            .targets
            .iter()
            .zip(fds)
            .enumerate()
            .map(|(index, (target, fd))| {
                (format!("{GUEST_ENV_PREFIX}{index}"), target.descriptor(*fd))
            })
            .collect())
    }
}

/// A host socket opened for one admitted target. Dropping it closes the
/// socket, so the run's transport never outlives the run.
pub(crate) enum AdmittedSocket {
    Stream(TcpStream),
    Listener(TcpListener),
}

/// What opening an admission produced before the guest existed.
pub(crate) enum OpenOutcome {
    Opened(Vec<(AdmittedTarget, AdmittedSocket)>),
    Stopped(WasiRunOutcome),
}

/// What a run start did with its admitted sockets.
///
/// Sockets are opened before the guest is instantiated, so a stop observed
/// while connecting ends the run with no guest code executed.
pub enum NetworkStart {
    /// Every admitted socket is installed as a guest descriptor.
    Installed,
    /// The run's cancellation or deadline was observed before any guest code
    /// ran; the outcome is the run's committed result.
    Stopped(WasiRunOutcome),
}

/// Why a socket host call ended without a normal result.
#[derive(Debug)]
pub(crate) enum NetIoError {
    /// The run's cancellation or deadline was observed and committed.
    Stopped(WasiRunOutcome),
    /// A non-blocking descriptor was not ready (the guest asked for `EAGAIN`).
    WouldBlock,
    /// The socket itself failed.
    Io(io::Error),
}

impl From<io::Error> for NetIoError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Open every socket the admission demands, in admission order.
///
/// DNS resolution and connect attempts are bounded by the admission timeout
/// *and* the live run deadline, and every attempt observes cancellation first. Binding a
/// listener is a host-side effect, so it happens only for a target that
/// explicitly granted `listen`.
pub(crate) fn open_admitted(
    admission: &NetworkAdmission,
    stop: &RunStop,
) -> Result<OpenOutcome> {
    let mut opened = Vec::with_capacity(admission.targets().len());
    for target in admission.targets() {
        if let Some(outcome) = stop.observe() {
            return Ok(OpenOutcome::Stopped(outcome));
        }
        let addresses = match resolve_bounded(target, admission.connect_timeout(), stop) {
            Ok(addresses) => addresses,
            Err(StartStop::Stopped(outcome)) => return Ok(OpenOutcome::Stopped(outcome)),
            Err(StartStop::Failed(error)) => return Err(error),
        };
        let socket = if target.permits(NetworkOperation::Connect) {
            match connect_bounded(target, &addresses, admission.connect_timeout(), stop) {
                Ok(stream) => AdmittedSocket::Stream(stream),
                Err(StartStop::Stopped(outcome)) => return Ok(OpenOutcome::Stopped(outcome)),
                Err(StartStop::Failed(error)) => return Err(error),
            }
        } else if target.permits(NetworkOperation::Listen) {
            AdmittedSocket::Listener(listen_bounded(&addresses)?)
        } else {
            bail!(
                "NETWORK_TARGET_INVALID: {}:{} grants no operation that opens a socket",
                target.host,
                target.port
            );
        };
        opened.push((target.clone(), socket));
    }
    Ok(OpenOutcome::Opened(opened))
}

/// Why resolution or a connect attempt ended: a committed stop is a run outcome, not a
/// transport failure.
#[derive(Debug)]
enum StartStop {
    Stopped(WasiRunOutcome),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for StartStop {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl From<std::io::Error> for StartStop {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(anyhow::Error::new(error))
    }
}

/// Resolve only a granted name, with no uninterruptible OS `getaddrinfo`
/// call. Literal addresses never consult DNS or the system resolver config.
fn resolve_bounded(
    target: &AdmittedTarget,
    resolve_timeout: Duration,
    stop: &RunStop,
) -> std::result::Result<Vec<SocketAddr>, StartStop> {
    if let Some(outcome) = stop.observe() {
        return Err(StartStop::Stopped(outcome));
    }
    if let Ok(ip) = target.host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, target.port)]);
    }
    if !target.resolve {
        return Err(anyhow::anyhow!(
            "NETWORK_TARGET_INVALID: {} is not an IP literal",
            target.host
        )
        .into());
    }
    #[cfg(any(unix, windows))]
    {
        resolve_with(target, resolve_timeout, stop, || {
            hickory_resolver::TokioResolver::builder_tokio()
                .context("read system DNS configuration")?
                .build()
                .context("build bounded DNS resolver")
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = resolve_timeout;
        Err(anyhow::anyhow!(
            "NETWORK_RESOLVE_UNSUPPORTED: bounded system DNS is unavailable on this platform"
        )
        .into())
    }
}

#[cfg(any(unix, windows))]
fn resolve_with(
    target: &AdmittedTarget,
    resolve_timeout: Duration,
    stop: &RunStop,
    build_resolver: impl FnOnce() -> Result<hickory_resolver::TokioResolver> + Send,
) -> std::result::Result<Vec<SocketAddr>, StartStop> {
    with_network_executor("dns", |runtime| {
        runtime.block_on(async {
            if let Some(outcome) = stop.observe() {
                return Err(StartStop::Stopped(outcome));
            }
            let resolver = build_resolver().with_context(|| {
                format!("NETWORK_RESOLVE_FAILED: {}:{}", target.host, target.port)
            })?;
            let lookup = wait_bounded(
                async {
                    resolver
                        .lookup_ip(target.host.as_str())
                        .await
                        .with_context(|| {
                            format!("NETWORK_RESOLVE_FAILED: {}:{}", target.host, target.port)
                        })
                },
                resolve_timeout,
                stop,
                format!("NETWORK_RESOLVE_TIMEOUT: {}:{}", target.host, target.port),
            )
            .await?;
            let addresses: Vec<_> = lookup
                .iter()
                .map(|ip| SocketAddr::new(ip, target.port))
                .collect();
            if addresses.is_empty() {
                return Err(anyhow::anyhow!(
                    "NETWORK_RESOLVE_FAILED: {}:{} resolved to no address",
                    target.host,
                    target.port
                )
                .into());
            }
            Ok(addresses)
        })
    })
}

/// A synchronous WASI host call may itself be invoked from a Tokio task.
/// Give network startup an owned current-thread executor instead of nesting
/// block_on on that executor. Join the scoped thread before returning; runtime
/// drop cancels remaining DNS work, including retransmission. No detached
/// blocking resolver or pending connect is left behind after a stop.
fn with_network_executor<T: Send>(
    operation: &str,
    run: impl FnOnce(&tokio::runtime::Runtime) -> std::result::Result<T, StartStop> + Send,
) -> std::result::Result<T, StartStop> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name(format!("shenko-wasi-{operation}"))
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("NETWORK_EXECUTOR_FAILED: create bounded network executor")?;
                run(&runtime)
            })?
            .join()
            .map_err(|_| {
                anyhow::anyhow!("NETWORK_EXECUTOR_FAILED: {operation} executor panicked")
            })?
    })
}

/// Poll one future throughout the wait. In particular a connection is never
/// restarted at each stop-observation slice: dropping this one future closes
/// its pending socket when cancellation or either deadline wins.
async fn wait_bounded<T>(
    future: impl std::future::Future<Output = Result<T>>,
    timeout: Duration,
    stop: &RunStop,
    timeout_message: String,
) -> std::result::Result<T, StartStop> {
    tokio::pin!(future);
    let budget = tokio::time::sleep(timeout);
    tokio::pin!(budget);
    let mut poll = tokio::time::interval(IO_POLL_INTERVAL);
    loop {
        if let Some(outcome) = stop.observe() {
            return Err(StartStop::Stopped(outcome));
        }
        tokio::select! {
            biased;
            _ = poll.tick() => {}
            _ = &mut budget => {
                if let Some(outcome) = stop.observe() {
                    return Err(StartStop::Stopped(outcome));
                }
                return Err(anyhow::anyhow!(timeout_message).into());
            }
            _completed = &mut future => {
                if let Some(outcome) = stop.observe() {
                    return Err(StartStop::Stopped(outcome));
                }
                return _completed.map_err(StartStop::Failed);
            }
        }
    }
}

fn connect_bounded(
    target: &AdmittedTarget,
    addresses: &[SocketAddr],
    connect_timeout: Duration,
    stop: &RunStop,
) -> std::result::Result<TcpStream, StartStop> {
    with_network_executor("connect", |runtime| {
        runtime.block_on(async {
            let mut last_error: Option<anyhow::Error> = None;
            for address in addresses {
                let connecting = async {
                    let stream = tokio::net::TcpStream::connect(*address)
                        .await
                        .with_context(|| {
                            format!(
                                "NETWORK_CONNECT_FAILED: {}:{} ({address})",
                                target.host, target.port
                            )
                        })?;
                    // Tokio's conversion preserves the connected socket and its
                    // non-blocking mode; the guest's host I/O owns it from here.
                    stream
                        .into_std()
                        .context("NETWORK_START_INVALID: retain connected socket")
                };
                match wait_bounded(
                    connecting,
                    connect_timeout,
                    stop,
                    format!(
                        "NETWORK_CONNECT_TIMEOUT: {}:{} ({address})",
                        target.host, target.port
                    ),
                )
                .await
                {
                    Ok(stream) => return Ok(stream),
                    Err(StartStop::Stopped(outcome)) => return Err(StartStop::Stopped(outcome)),
                    Err(StartStop::Failed(error)) => last_error = Some(error),
                }
            }
            Err(StartStop::Failed(last_error.unwrap_or_else(|| {
                anyhow::anyhow!(
                    "NETWORK_CONNECT_FAILED: {}:{} has no address to try",
                    target.host,
                    target.port
                )
            })))
        })
    })
}

fn listen_bounded(addresses: &[SocketAddr]) -> Result<TcpListener> {
    let mut last_error: Option<anyhow::Error> = None;
    for address in addresses {
        match TcpListener::bind(address) {
            Ok(listener) => {
                listener.set_nonblocking(true).with_context(|| {
                    format!("NETWORK_LISTEN_FAILED: {address} cannot be put in non-blocking mode")
                })?;
                return Ok(listener);
            }
            Err(error) => {
                last_error = Some(anyhow::anyhow!("NETWORK_LISTEN_FAILED: {address}: {error}"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        anyhow::anyhow!("NETWORK_LISTEN_FAILED: no address to bind")
    }))
}

enum WaitOutcome {
    Ready,
    WouldBlock,
}

/// One readiness probe of a stream. `Some(Ready)`/`Some(WouldBlock)` ends the
/// wait; `None` means the slice timed out with nothing to report, so the
/// caller re-observes the stop conditions.
#[cfg(unix)]
fn probe_stream(
    stream: &TcpStream,
    write: bool,
    block: bool,
) -> io::Result<Option<WaitOutcome>> {
    probe_fd(stream, write, block)
}

/// Non-POSIX fallback: a non-blocking peek distinguishes data/EOF from
/// "nothing yet", and a writable TCP stream is reported ready (a write attempt
/// still returns `WouldBlock` if the peer window is full).
#[cfg(not(unix))]
fn probe_stream(
    stream: &TcpStream,
    write: bool,
    block: bool,
) -> io::Result<Option<WaitOutcome>> {
    if write {
        return Ok(Some(WaitOutcome::Ready));
    }
    let mut byte = [0u8; 1];
    match stream.peek(&mut byte) {
        Ok(_) => Ok(Some(WaitOutcome::Ready)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            if block {
                std::thread::sleep(IO_POLL_INTERVAL);
                Ok(None)
            } else {
                Ok(Some(WaitOutcome::WouldBlock))
            }
        }
        Err(error) => Err(error),
    }
}

/// `poll(2)` a guest descriptor for readiness. `slices` selects the bounded
/// poll slice (blocking guest) or a single zero-timeout probe.
#[cfg(unix)]
fn probe_fd<T: std::os::unix::io::AsRawFd>(
    handle: &T,
    write: bool,
    block: bool,
) -> io::Result<Option<WaitOutcome>> {
    let mut pollfd = libc::pollfd {
        fd: handle.as_raw_fd(),
        events: if write { libc::POLLOUT } else { libc::POLLIN },
        revents: 0,
    };
    let timeout_ms = if block {
        IO_POLL_INTERVAL.as_millis() as libc::c_int
    } else {
        0
    };
    // SAFETY: `poll` reads one initialized `pollfd` describing a live
    // descriptor (the call cannot retain or free it) and writes only its
    // `revents` field.
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if ready == 0 {
        return Ok(if block { None } else { Some(WaitOutcome::WouldBlock) });
    }
    // POLLERR/POLLHUP/POLLNVAL also make the descriptor "ready": the
    // following read/write surfaces the concrete EOF or error, exactly as
    // POSIX readiness semantics promise.
    Ok(Some(WaitOutcome::Ready))
}

/// Wait until `stream` is ready for the requested direction.
///
/// `block` mirrors the guest-visible descriptor flag: a blocking guest waits
/// in bounded poll slices (so cancellation and the deadline stay observable),
/// a non-blocking guest gets a single zero-timeout probe.
fn wait_stream(
    stream: &TcpStream,
    write: bool,
    block: bool,
    stop: &RunStop,
) -> std::result::Result<WaitOutcome, NetIoError> {
    loop {
        if let Some(outcome) = stop.observe() {
            return Err(NetIoError::Stopped(outcome));
        }
        match probe_stream(stream, write, block).map_err(NetIoError::Io)? {
            Some(outcome) => return Ok(outcome),
            None => continue,
        }
    }
}

/// Wait until `listener` has an incoming connection (or the stop fires).
fn wait_accept(
    listener: &TcpListener,
    block: bool,
    stop: &RunStop,
) -> std::result::Result<WaitOutcome, NetIoError> {
    loop {
        if let Some(outcome) = stop.observe() {
            return Err(NetIoError::Stopped(outcome));
        }
        #[cfg(unix)]
        let probed = probe_fd(listener, false, block).map_err(NetIoError::Io)?;
        #[cfg(not(unix))]
        let probed = {
            if block {
                std::thread::sleep(IO_POLL_INTERVAL);
            }
            Some(WaitOutcome::Ready)
        };
        match probed {
            Some(outcome) => return Ok(outcome),
            None => continue,
        }
    }
}

/// Receive up to `buf.len()` bytes from a guest socket.
///
/// `peek` leaves the bytes queued (`sock_recv` `recv_peek`); `waitall` keeps
/// reading until the buffer is full, the peer closes, or the caller's
/// non-blocking mode has nothing more to give.
pub(crate) fn stream_read(
    mut stream: &TcpStream,
    buf: &mut [u8],
    nonblocking: bool,
    peek: bool,
    waitall: bool,
    stop: &RunStop,
) -> std::result::Result<usize, NetIoError> {
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            return Ok(filled);
        }
        match wait_stream(stream, false, !nonblocking, stop)? {
            WaitOutcome::WouldBlock => {
                return if filled == 0 { Err(NetIoError::WouldBlock) } else { Ok(filled) };
            }
            WaitOutcome::Ready => {}
        }
        let result = if peek {
            stream.peek(buf)
        } else {
            stream.read(&mut buf[filled..])
        };
        match result {
            Ok(0) => return Ok(filled),
            Ok(read) => {
                filled += read;
                if !waitall || peek {
                    return Ok(filled);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if nonblocking {
                    return if filled == 0 { Err(NetIoError::WouldBlock) } else { Ok(filled) };
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return if filled == 0 { Err(NetIoError::Io(error)) } else { Ok(filled) };
            }
        }
    }
}

/// Send `buf` to a guest socket, returning the number of bytes accepted.
///
/// A blocking guest is driven to completion in bounded slices; a non-blocking
/// guest gets whatever one readiness window accepts (a partial count is a
/// normal `sock_send` result).
pub(crate) fn stream_write(
    mut stream: &TcpStream,
    buf: &[u8],
    nonblocking: bool,
    stop: &RunStop,
) -> std::result::Result<usize, NetIoError> {
    let mut written = 0usize;
    while written < buf.len() {
        match wait_stream(stream, true, !nonblocking, stop)? {
            WaitOutcome::WouldBlock => {
                return if written == 0 { Err(NetIoError::WouldBlock) } else { Ok(written) };
            }
            WaitOutcome::Ready => {}
        }
        match stream.write(&buf[written..]) {
            Ok(0) => {
                return Err(NetIoError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "socket accepted no bytes",
                )));
            }
            Ok(sent) => written += sent,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if nonblocking {
                    return if written == 0 { Err(NetIoError::WouldBlock) } else { Ok(written) };
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return if written == 0 { Err(NetIoError::Io(error)) } else { Ok(written) };
            }
        }
    }
    Ok(written)
}

/// Wait until a guest socket is ready for `write` (or has readable data / an
/// EOF), bounded by the run stop. `poll_oneoff` uses this so a poll on a
/// socket cannot outlive a cancellation or the deadline.
pub(crate) fn wait_stream_ready(
    stream: &TcpStream,
    write: bool,
    stop: &RunStop,
) -> std::result::Result<(), NetIoError> {
    match wait_stream(stream, write, true, stop)? {
        WaitOutcome::Ready | WaitOutcome::WouldBlock => Ok(()),
    }
}

/// Accept one incoming stream from a guest listener.
pub(crate) fn listener_accept(
    listener: &TcpListener,
    nonblocking: bool,
    stop: &RunStop,
) -> std::result::Result<TcpStream, NetIoError> {
    loop {
        match wait_accept(listener, !nonblocking, stop)? {
            WaitOutcome::WouldBlock => return Err(NetIoError::WouldBlock),
            WaitOutcome::Ready => {}
        }
        match listener.accept() {
            Ok((stream, _peer)) => {
                stream.set_nonblocking(true).map_err(NetIoError::Io)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if nonblocking {
                    return Err(NetIoError::WouldBlock);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(NetIoError::Io(error)),
        }
    }
}

/// Shut down one direction of a guest socket.
pub(crate) fn shutdown_stream(
    stream: &TcpStream,
    how: Shutdown,
) -> std::result::Result<(), NetIoError> {
    stream.shutdown(how).map_err(NetIoError::Io)
}

#[cfg(all(test, any(unix, windows)))]
mod stop_tests {
    use super::*;
    use crate::{
        HOME_GUEST_PATH, MemoryOutputPipe, MountMode, MountSpec, WasiCancellation, WasiCommand,
        WasmRuntime,
    };
    use hickory_resolver::TokioResolver;
    use hickory_resolver::config::{
        ConnectionConfig, LookupIpStrategy, NameServerConfig, ResolverConfig,
    };
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    use std::net::UdpSocket;
    use std::time::Instant;

    fn blackhole_lookup(
        server: SocketAddr,
        target: &AdmittedTarget,
        budget: Duration,
        stop: &RunStop,
    ) -> std::result::Result<Vec<SocketAddr>, StartStop> {
        resolve_with(target, budget, stop, move || {
            let mut connection = ConnectionConfig::udp();
            connection.port = server.port();
            let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
                server.ip(),
                true,
                vec![connection],
            )]);
            let mut builder =
                TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
            builder.options_mut().timeout = Duration::from_secs(30);
            builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4Only;
            Ok(builder.build()?)
        })
    }

    /// Emit a marker, then make a real preview1 clock subscription for 30s.
    /// The marker distinguishes an interrupted host wait from a stop that
    /// only happened during compilation. No network admission is installed.
    const CLOCK_WAT: &str = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write"
        (func $write (param i32 i32 i32 i32) (result i32)))
      (import "wasi_snapshot_preview1" "poll_oneoff"
        (func $poll (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 128) "clock")
      (func (export "_start")
        ;; subscription: userdata, clock tag, monotonic id, relative timeout.
        (i64.store (i32.const 0) (i64.const 42))
        (i32.store (i32.const 8) (i32.const 0))
        (i32.store (i32.const 16) (i32.const 1))
        (i64.store (i32.const 24) (i64.const 30000000000))
        (i32.store (i32.const 112) (i32.const 128))
        (i32.store (i32.const 116) (i32.const 5))
        (drop (call $write (i32.const 1) (i32.const 112) (i32.const 1) (i32.const 120)))
        (drop (call $poll (i32.const 0) (i32.const 48) (i32.const 1) (i32.const 96)))))
    "#;

    #[test]
    fn dns_and_guest_clock_waits_obey_run_stop() -> Result<()> {
        let target = AdmittedTarget::new(
            "bounded-dns.example.",
            443,
            true,
            vec![NetworkOperation::Connect],
        )?;

        // Cancellation is issued only after the DNS peer receives the query.
        // Calling the synchronous host boundary inside another Tokio runtime
        // also guards against nested-executor panics or a deadlocked resolver.
        let dns = UdpSocket::bind("127.0.0.1:0")?;
        dns.set_read_timeout(Some(Duration::from_secs(2)))?;
        let server = dns.local_addr()?;
        let cancel = WasiCancellation::default();
        let request_cancel = cancel.clone();
        let peer = std::thread::spawn(move || -> io::Result<usize> {
            let result = dns.recv_from(&mut [0u8; 512]).map(|(size, _)| size);
            request_cancel.cancel();
            result
        });
        let stop = RunStop::new(Some(cancel), Some(Instant::now() + Duration::from_secs(5)));
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = outer
            .block_on(async { blackhole_lookup(server, &target, Duration::from_secs(10), &stop) });
        let query_bytes = peer.join().map_err(|_| anyhow::anyhow!("DNS peer panicked"))??;
        assert!(query_bytes >= 12);
        assert!(
            matches!(result, Err(StartStop::Stopped(WasiRunOutcome::Cancelled))),
            "{result:?}"
        );

        let home = std::env::temp_dir().join(format!("shenko-clock-stop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home)?;
        let result = (|| -> Result<()> {
            let mut runtime = WasmRuntime::with_mounts(vec![MountSpec::new(
                home.clone(),
                HOME_GUEST_PATH,
                MountMode::ReadOnly,
                false,
                false,
            )])?;
            let output = MemoryOutputPipe::new(64);
            let mut command = WasiCommand::new(io::empty(), output.clone(), io::empty());
            command.control.deadline = Some(Duration::from_secs(3));
            let started = Instant::now();
            let outcome = runtime.run_wasi_module(CLOCK_WAT.as_bytes(), command)?;
            assert_eq!(outcome, WasiRunOutcome::DeadlineExceeded);
            assert_eq!(output.contents().as_ref(), b"clock");
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "clock wait outlived its run deadline"
            );

            Ok(())
        })();
        std::fs::remove_dir_all(home)?;
        result
    }
}
