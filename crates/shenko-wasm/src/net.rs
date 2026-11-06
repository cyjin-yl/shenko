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
//! Restore boundary (SPEC §51.4, §10.1 via §51.10): the guest's own linear
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
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
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

    /// The addresses this target admits. Name resolution happens here, on the
    /// host, for a target that granted it; an IP literal is used as written.
    fn addresses(&self) -> Result<Vec<SocketAddr>> {
        if self.resolve {
            let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
                .to_socket_addrs()
                .with_context(|| {
                    format!("NETWORK_RESOLVE_FAILED: {}:{}", self.host, self.port)
                })?
                .collect();
            ensure!(
                !addrs.is_empty(),
                "NETWORK_RESOLVE_FAILED: {}:{} resolved to no address",
                self.host,
                self.port
            );
            Ok(addrs)
        } else {
            // Validation already refused `resolve: false` for a name.
            let ip: IpAddr = self.host.parse().map_err(|_| {
                anyhow::anyhow!("NETWORK_TARGET_INVALID: {} is not an IP literal", self.host)
            })?;
            Ok(vec![SocketAddr::new(ip, self.port)])
        }
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
/// Connect attempts are bounded by the admission timeout *and* the live run
/// deadline, and every attempt re-observes cancellation first. Binding a
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
        let addresses = target.addresses()?;
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

/// Why a connect attempt ended: a committed stop is a run outcome, not a
/// transport failure.
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

fn connect_bounded(
    target: &AdmittedTarget,
    addresses: &[SocketAddr],
    connect_timeout: Duration,
    stop: &RunStop,
) -> std::result::Result<TcpStream, StartStop> {
    let mut last_error: Option<anyhow::Error> = None;
    for address in addresses {
        if let Some(outcome) = stop.observe() {
            return Err(StartStop::Stopped(outcome));
        }
        let budget = match stop.remaining() {
            Some(remaining) => remaining.min(connect_timeout),
            None if stop.has_deadline() => match stop.observe() {
                Some(outcome) => return Err(StartStop::Stopped(outcome)),
                None => continue,
            },
            None => connect_timeout,
        };
        // `connect_timeout` rejects a zero duration; a deadline that is about
        // to elapse still gets one attempt slice so the stop observation (not
        // a fabricated timeout) decides the outcome.
        let budget = budget.max(Duration::from_millis(1));
        match TcpStream::connect_timeout(address, budget) {
            Ok(stream) => {
                stream.set_nonblocking(true).with_context(|| {
                    format!(
                        "NETWORK_START_INVALID: {}:{} accepted a connection but cannot \
                         be put in host-driven non-blocking mode",
                        target.host, target.port
                    )
                })?;
                return Ok(stream);
            }
            Err(error) => {
                if let Some(outcome) = stop.observe() {
                    return Err(StartStop::Stopped(outcome));
                }
                last_error = Some(anyhow::anyhow!(
                    "NETWORK_CONNECT_FAILED: {}:{} ({address}): {error}",
                    target.host,
                    target.port
                ));
            }
        }
    }
    Err(StartStop::Failed(last_error.unwrap_or_else(|| {
        anyhow::anyhow!(
            "NETWORK_CONNECT_FAILED: {}:{} has no address to try",
            target.host,
            target.port
        )
    })))
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
    stream: &TcpStream,
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
    stream: &TcpStream,
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
