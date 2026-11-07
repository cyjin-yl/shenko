//! Shenko-owned WASI preview1 host boundary (SPEC §8.3, A08).
//!
//! Wasmtime-wasi 45 can only preopen directories by pathname
//! (`WasiCtxBuilder::preopened_dir`), which re-resolves the path at run time;
//! postchecking a pathname is not confinement (A08). This module owns the
//! preview1 boundary instead: the WITX interface (copied from
//! wasmtime-wasi 45.0.3, Apache-2.0 WITH LLVM-exception, see
//! `witx/p1/`) is implemented over our own host state, and every filesystem
//! resolution proceeds from the [`crate::mount::OwnedDir`] handles admitted
//! at run start — no host pathname is re-resolved for the guest.
//!
//! Path resolution is per-component with `O_NOFOLLOW` discipline (via
//! cap-std `FollowSymlinks::No` on single-component opens), so symlinks the
//! guest creates can never redirect a resolution outside the admitted
//! handles. Copy-on-write mounts see the union view `overlay ∪ base −
//! tombstones`; direct mounts are the same engine with a single layer.

use crate::fsx;
use crate::mount::{MountBacking, MountBinding};
use crate::net::{AdmittedSocket, NetIoError, NetworkAdmission, NetworkStart};
use crate::runtime::{RunStop, WasiRunOutcome};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wiggle::{GuestError, GuestMemory, GuestPtr, GuestType};

/// Host-side failure value for the preview1 trap type.
///
/// The generated preview1 error takes a wasmtime error, and `anyhow::Error`
/// has no conversion into one (trait coherence forbids it), so this module
/// shadows the `anyhow!` macro with one that produces the value the trap
/// constructor expects. Every `types::Error::trap(anyhow!(..))` site in this
/// file therefore carries a wasmtime error, never an `anyhow::Error`.
macro_rules! anyhow {
    ($($arg:tt)*) => {
        wasmtime::Error::from_anyhow(::anyhow::anyhow!($($arg)*))
    };
}

wiggle::from_witx!({
    witx: ["witx/p1/wasi_snapshot_preview1.witx"],
    errors: { errno => trappable Error },
});

impl wiggle::GuestErrorType for types::Errno {
    fn success() -> Self {
        Self::Success
    }
}

/// Guest memory access failures map to errno or traps following the same
/// discipline as the upstream preview1 host (wasi docs: misaligned or
/// out-of-bounds pointers trap; invalid flags/enums are `inval`).
impl From<GuestError> for types::Error {
    fn from(err: GuestError) -> Self {
        use wiggle::GuestError::*;
        match err {
            InvalidFlagValue { .. } | InvalidEnumValue { .. } => types::Errno::Inval.into(),
            PtrOverflow { .. } | PtrOutOfBounds { .. } | PtrNotAligned { .. } => {
                types::Error::trap(err.into())
            }
            InvalidUtf8 { .. } => types::Errno::Ilseq.into(),
            TryFromIntError { .. } => types::Errno::Overflow.into(),
            SliceLengthsDiffer { .. } => types::Errno::Fault.into(),
            InFunc { err, .. } => types::Error::from(*err),
        }
    }
}

pub(crate) mod sync {
    wiggle::wasmtime_integration!({
        witx: ["witx/p1/wasi_snapshot_preview1.witx"],
        target: super,
        errors: { errno => trappable Error },
    });
}

/// Add the Shenko preview1 host functions to a linker whose store data is
/// carried by the store (the controlled store wraps [`P1Host`]).
pub fn add_to_linker_sync<T: 'static>(
    linker: &mut wasmtime::Linker<T>,
    get_cx: impl Fn(&mut T) -> &mut P1Host + Send + Sync + Copy + 'static,
) -> wasmtime::Result<()> {
    sync::add_wasi_snapshot_preview1_to_linker(linker, get_cx)
}

/// Convert a host I/O error into the preview1 errno with the same meaning.
fn errno_from_io(error: std::io::Error) -> types::Errno {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return types::Errno::Loop;
    }
    // The capability layer reports a final symlink and a raced entry with
    // sentinel messages; they keep their POSIX meaning here.
    match error.to_string().as_str() {
        fsx::ELOOP => return types::Errno::Loop,
        fsx::EAGAIN => return types::Errno::Again,
        _ => {}
    }
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => types::Errno::Noent,
        ErrorKind::PermissionDenied => types::Errno::Acces,
        ErrorKind::AlreadyExists => types::Errno::Exist,
        ErrorKind::InvalidInput | ErrorKind::InvalidData => types::Errno::Inval,
        ErrorKind::NotADirectory => types::Errno::Notdir,
        ErrorKind::IsADirectory => types::Errno::Isdir,
        ErrorKind::DirectoryNotEmpty => types::Errno::Notempty,
        _ => types::Errno::Io,
    }
}

fn io_err<T>(error: std::io::Error) -> Result<T, types::Error> {
    Err(errno_from_io(error).into())
}

/// Map a transport failure onto its preview1 errno.
///
/// A committed run stop is reported as the POSIX errno that describes it
/// (`canceled`/`timedout`) *and* remains committed on the run, so guest code
/// that ignores the errno still traps at its next epoch checkpoint instead of
/// running on after the caller asked to stop.
fn socket_error(error: NetIoError) -> types::Error {
    match error {
        NetIoError::Stopped(WasiRunOutcome::DeadlineExceeded) => types::Errno::Timedout.into(),
        NetIoError::Stopped(_) => types::Errno::Canceled.into(),
        NetIoError::WouldBlock => types::Errno::Again.into(),
        NetIoError::Io(error) => errno_from_io(error).into(),
    }
}

fn net_err<T>(error: NetIoError) -> Result<T, types::Error> {
    Err(socket_error(error))
}

fn fault<T>(message: impl std::fmt::Display) -> Result<T, types::Error> {
    Err(types::Error::trap(anyhow!("{message}")))
}

fn read_path(memory: &GuestMemory<'_>, ptr: GuestPtr<str>) -> Result<String, types::Error> {
    let path = memory
        .as_cow_str(ptr)
        .map_err(|e| types::Error::trap(anyhow!("path pointer: {e}")))?;
    let path = path.as_ref();
    if path.contains('\0') {
        return Err(types::Errno::Inval.into());
    }
    if path.len() > 4096 {
        return Err(types::Errno::Nametoolong.into());
    }
    Ok(path.to_string())
}

/// Synchronous byte streams backing guest stdio. `run_wasi_module` adapts
/// the caller's `StdinStream`/`StdoutStream` into these for the worker
/// thread; every adapter is a plain blocking `Read`/`Write`.
pub struct StdioStreams {
    pub stdin: Box<dyn Read + Send + Sync>,
    pub stdout: Box<dyn Write + Send + Sync>,
    pub stderr: Box<dyn Write + Send + Sync>,
}

impl StdioStreams {
    /// Build blocking streams from the upstream asynchronous WASI streams.
    ///
    /// The preview1 host functions in this module are synchronous, so each
    /// operation is driven on wasmtime-wasi's ambient runtime and blocks the
    /// calling (guest) thread until it completes. The guest thread must not
    /// itself be a tokio worker: the runtime already guarantees that by
    /// running the guest on its own thread.
    pub fn from_async(
        stdin: Box<dyn tokio::io::AsyncRead + Send + Sync>,
        stdout: Box<dyn tokio::io::AsyncWrite + Send + Sync>,
        stderr: Box<dyn tokio::io::AsyncWrite + Send + Sync>,
    ) -> Self {
        Self {
            stdin: Box::new(StreamReader { stream: Box::into_pin(stdin) }),
            stdout: Box::new(StreamWriter { stream: Box::into_pin(stdout) }),
            stderr: Box::new(StreamWriter { stream: Box::into_pin(stderr) }),
        }
    }
}

/// Blocking reader over an upstream asynchronous WASI input stream.
struct StreamReader {
    stream: Pin<Box<dyn tokio::io::AsyncRead + Send + Sync>>,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let stream = &mut self.stream;
        wasmtime_wasi::runtime::in_tokio(async move {
            tokio::io::AsyncReadExt::read(stream, buf).await
        })
    }
}

/// Blocking writer over an upstream asynchronous WASI output stream.
struct StreamWriter {
    stream: Pin<Box<dyn tokio::io::AsyncWrite + Send + Sync>>,
}

impl Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let stream = &mut self.stream;
        wasmtime_wasi::runtime::in_tokio(async move {
            tokio::io::AsyncWriteExt::write(stream, buf).await
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let stream = &mut self.stream;
        wasmtime_wasi::runtime::in_tokio(async move {
            tokio::io::AsyncWriteExt::flush(stream).await
        })
    }
}

/// A directory view at one mount-relative path: the overlay layer (direct
/// mounts use it as their only layer) plus, for COW mounts, the readonly
/// base layer. Either layer may be absent at this path; at least one exists.
pub(crate) struct DirHandle {
    mount: usize,
    rel: PathBuf,
    overlay: Option<cap_std::fs::Dir>,
    base: Option<cap_std::fs::Dir>,
}

impl DirHandle {
    fn layer_dirs(&self) -> (Option<&cap_std::fs::Dir>, Option<&cap_std::fs::Dir>) {
        (self.overlay.as_ref(), self.base.as_ref())
    }
}

/// An open file. `rel` names the mount-relative path (used for COW
/// copy-up); stdio-backed files carry no path.
struct FileHandle {
    mount: usize,
    rel: Option<PathBuf>,
    file: std::fs::File,
    cursor: u64,
    append: bool,
    readable: bool,
    writable: bool,
}

enum FdEntry {
    /// A replay descriptor has no fresh OS handle. Reads come exclusively
    /// from the bound transcript; unsupported metadata/effects trap.
    Recorded { identity: String, cursor: u64, directory: bool },
    Stdin(Box<dyn Read + Send + Sync>),
    Stdout(Box<dyn Write + Send + Sync>),
    Stderr(Box<dyn Write + Send + Sync>),
    File(FileHandle),
    Dir(DirHandle),
    /// A target-bound host socket. Preview1 has no socket-creation call, so
    /// the host opened this descriptor from the run's network admission
    /// before the guest existed; the guest can only use it, never create,
    /// re-point or widen it.
    Socket(SocketHandle),
}

/// One installed host socket.
///
/// `nonblocking` is the guest-visible flag. The host descriptor itself is
/// always in non-blocking mode — the host waits in bounded poll slices and
/// emulates blocking reads so cancellation and the deadline stay observable —
/// while the guest sees exactly the mode it asked for.
struct SocketHandle {
    socket: AdmittedSocket,
    nonblocking: bool,
}

impl SocketHandle {
    fn stream(&self) -> Result<&TcpStream, types::Error> {
        match &self.socket {
            AdmittedSocket::Stream(stream) => Ok(stream),
            AdmittedSocket::Listener(_) => Err(types::Errno::Notconn.into()),
        }
    }
}

/// The complete preview1 host state for one run.
pub struct P1Host {
    replay_io: Option<crate::replay_io::ReplayIo>,
    args: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    fds: Vec<Option<FdEntry>>,
    mounts: Vec<MountBinding>,
    monotonic_epoch: Instant,
    /// Required by the generated integration: hostcall fuel bookkeeping. We
    /// record it (upstream parity) but do not meter operations on it.
    hostcall_fuel: usize,
    /// The run's admitted transport: its stop cell bounds every socket call,
    /// and its admission bounds how many sockets may exist.
    net: Option<NetworkState>,
}

struct NetworkState {
    admission: NetworkAdmission,
    stop: RunStop,
}

/// Bounds for guest-supplied buffers and symlink chasing.
const MAX_SYMLINK_DEPTH: usize = 40;
const MAX_IOV_BYTES: usize = 64 * 1024 * 1024;
const MAX_TREE_DEPTH: usize = 64;
const MAX_TREE_ENTRIES: usize = 200_000;

impl P1Host {
    /// Build the host state: stdio at fds 0-2, admitted mounts as preopen
    /// dir fds from 3, in declaration order.
    pub fn new(
        mounts: Vec<MountBinding>,
        stdio: StdioStreams,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Self> {
        let mut fds: Vec<Option<FdEntry>> = Vec::new();
        fds.push(Some(FdEntry::Stdin(stdio.stdin)));
        fds.push(Some(FdEntry::Stdout(stdio.stdout)));
        fds.push(Some(FdEntry::Stderr(stdio.stderr)));
        for (index, binding) in mounts.iter().enumerate() {
            let (overlay, base) = match &binding.backing {
                MountBacking::Direct { root, .. } => (
                    Some(root.dir().try_clone().context("clone mount handle")?),
                    None,
                ),
                MountBacking::CopyOnWrite { base, store, .. } => (
                    Some(
                        store
                            .overlay_dir()
                            .try_clone()
                            .context("clone overlay handle")?,
                    ),
                    Some(base.dir().try_clone().context("clone base handle")?),
                ),
            };
            fds.push(Some(FdEntry::Dir(DirHandle {
                mount: index,
                rel: PathBuf::new(),
                overlay,
                base,
            })));
        }
        Ok(Self {
            replay_io: None,
            args: args.into_iter().map(String::into_bytes).collect(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.into_bytes(), v.into_bytes()))
                .collect(),
            fds,
            mounts,
            monotonic_epoch: Instant::now(),
            hostcall_fuel: 0,
            net: None,
        })
    }

    fn binding(&self, mount: usize) -> Result<&MountBinding, types::Error> {
        self.mounts
            .get(mount)
            .ok_or_else(|| types::Error::trap(anyhow!("fd names unknown mount {mount}")))
    }

    pub fn set_replay_io(&mut self, io: Option<crate::replay_io::ReplayIo>) {
        self.replay_io = io;
    }

    /// Open the run's admitted sockets and install them as guest descriptors.
    ///
    /// Called after [`P1Host::new`] and before the guest is instantiated, so
    /// descriptor numbers are deterministic: the sockets follow every
    /// directory preopen in admission order and are never recycled from a
    /// hole. Their indices are published in the host-owned
    /// `SHENKO_NET_<index>` environment namespace, which a caller cannot
    /// supply itself ([`crate::runtime::WasiCommand::validate`] refuses it).
    ///
    /// Opening happens here, not lazily on first guest use: a target that
    /// cannot be reached fails the run start instead of turning into a
    /// confusing guest `ENOTCONN` later. A stop observed while connecting
    /// returns [`NetworkStart::Stopped`] with no guest code run.
    pub fn install_network(
        &mut self,
        admission: NetworkAdmission,
        stop: RunStop,
    ) -> Result<NetworkStart> {
        let opened = match crate::net::open_admitted(&admission, &stop)? {
            crate::net::OpenOutcome::Opened(opened) => opened,
            crate::net::OpenOutcome::Stopped(outcome) => {
                return Ok(NetworkStart::Stopped(outcome));
            }
        };
        let mut fds = Vec::with_capacity(opened.len());
        for (_target, socket) in opened {
            let fd = self.fds.len() as u32;
            self.fds
                .push(Some(FdEntry::Socket(SocketHandle { socket, nonblocking: false })));
            fds.push(fd);
        }
        for (key, value) in admission.guest_environment(&fds)? {
            self.env.push((key.into_bytes(), value.into_bytes()));
        }
        self.net = Some(NetworkState { admission, stop });
        Ok(NetworkStart::Installed)
    }

    /// The guest socket entry for `fd`, plus the run stop cell it must honour.
    ///
    /// Socket I/O is refused outright when a replay transcript is bound: a
    /// live peer is not a deterministic input, and silently mixing the two
    /// would make a recorded run unrepeatable.
    fn socket_entry(&self, fd: types::Fd) -> Result<(&SocketHandle, RunStop), types::Error> {
        if self.replay_io.is_some() {
            return Err(types::Errno::Notsup.into());
        }
        let stop = match &self.net {
            Some(net) => net.stop.clone(),
            None => return Err(types::Errno::Notsock.into()),
        };
        match self.fd(fd)? {
            FdEntry::Socket(handle) => Ok((handle, stop)),
            _ => Err(types::Errno::Notsock.into()),
        }
    }

    /// Whether `fd` is an installed host socket.
    fn is_socket(&self, fd: types::Fd) -> Result<bool, types::Error> {
        Ok(matches!(self.fd(fd)?, FdEntry::Socket(_)))
    }

    /// The run stop cell that bounds every socket call.
    fn socket_stop(&self) -> Result<RunStop, types::Error> {
        match &self.net {
            Some(net) => Ok(net.stop.clone()),
            None => Err(types::Errno::Notsock.into()),
        }
    }

    /// How many sockets exist right now (the admission caps the count).
    fn open_socket_count(&self) -> usize {
        self.fds
            .iter()
            .filter(|slot| matches!(slot, Some(FdEntry::Socket(_))))
            .count()
    }

    fn replay_guard(&self, operation: &str) -> Result<(), types::Error> {
        if let Some(io) = &self.replay_io {
            io.reject_unrecorded(operation).map_err(|error|
                types::Error::trap(anyhow!("{error:#}")))?;
        }
        Ok(())
    }

    fn record_open(&self, request: &[u8], fd: types::Fd) -> Result<(), types::Error> {
        let Some(io) = &self.replay_io else { return Ok(()); };
        let (identity, directory) = match self.fds.get(u32::from(fd) as usize).and_then(Option::as_ref) {
            Some(FdEntry::File(file)) => (format!("{:?}:{:?}", file.mount, file.rel), false),
            Some(FdEntry::Dir(dir)) => (format!("{:?}:{:?}", dir.mount, dir.rel), true),
            _ => return Err(types::Errno::Badf.into()),
        };
        io.input("path_open", request, 64 * 1024,
            || Ok(serde_json::to_vec(&(u32::from(fd), identity, directory))?))
            .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
        Ok(())
    }

    fn read_input(&mut self, fd: types::Fd, size: usize, offset: Option<u64>) -> Result<Vec<u8>, types::Error> {
        if size > MAX_IOV_BYTES { return Err(types::Errno::Inval.into()); }
        let identity = match self.fds.get(u32::from(fd) as usize).and_then(Option::as_ref)
            .ok_or(types::Errno::Badf)? {
            FdEntry::File(file) => format!("{:?}:{:?}:{}", file.mount, file.rel, offset.unwrap_or(file.cursor)),
            FdEntry::Recorded { identity, cursor, directory: false } => format!("{identity}:{}", offset.unwrap_or(*cursor)),
            FdEntry::Stdin(_) => "stdin".into(),
            _ => return Err(types::Errno::Badf.into()),
        };
        let request = format!("{fd:?}:{size}:{offset:?}:{identity}");
        let io = self.replay_io.clone();
        let replaying = io.as_ref().is_some_and(|io| io.is_replay());
        let mut supply = || -> Result<Vec<u8>, types::Error> {
            let mut bytes = vec![0; size];
            let n = match self.fd_mut(fd)? {
                FdEntry::Stdin(reader) if offset.is_none() => reader.read(&mut bytes).map_err(errno_from_io)?,
                FdEntry::File(file) => {
                    if !file.readable { return Err(types::Errno::Perm.into()); }
                    let n = Self::read_file_at(&mut file.file, offset.unwrap_or(file.cursor), &mut bytes).map_err(errno_from_io)?;
                    if offset.is_none() { file.cursor = file.cursor.saturating_add(n as u64); }
                    n
                },
                _ => return Err(types::Errno::Spipe.into()),
            };
            bytes.truncate(n);
            Ok(bytes)
        };
        let bytes = if let Some(io) = io {
            io.input(if offset.is_some() { "fd_pread" } else { "fd_read" }, request.as_bytes(), size as u64,
                || supply().map_err(|error| ::anyhow::anyhow!("{error:?}")))
                .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?
        } else { supply()? };
        if replaying && offset.is_none() {
            if let Some(Some(FdEntry::File(file))) = self.fds.get_mut(u32::from(fd) as usize) {
                file.cursor = file.cursor.saturating_add(bytes.len() as u64);
            }
            if let Some(Some(FdEntry::Recorded { cursor, .. })) = self.fds.get_mut(u32::from(fd) as usize) {
                *cursor = cursor.saturating_add(bytes.len() as u64);
            }
        }
        Ok(bytes)
    }
    /// Mutable access to a COW mount's store (tombstone updates).
    fn cow_store(&mut self, mount: usize) -> Result<&mut crate::cow::CowMountStore, types::Error> {
        match &mut self
            .mounts
            .get_mut(mount)
            .ok_or_else(|| types::Error::trap(anyhow!("fd names unknown mount {mount}")))?
            .backing
        {
            MountBacking::CopyOnWrite { store, .. } => Ok(store),
            _ => Err(types::Error::trap(anyhow!(
                "mount {mount} is not copy-on-write"
            ))),
        }
    }

    fn mount_writable(&self, mount: usize) -> Result<bool, types::Error> {
        Ok(self.binding(mount)?.backing.writable())
    }

    /// Whether the mount at `mount` is a managed copy-on-write mount.
    fn is_cow(&self, mount: usize) -> Result<bool, types::Error> {
        Ok(matches!(
            self.binding(mount)?.backing,
            MountBacking::CopyOnWrite { .. }
        ))
    }

    /// Duplicate the direct layer handle of a mount. The duplicate lets the
    /// caller keep using the host without holding a borrow of the binding
    /// across the mutable operations that follow.
    fn direct_layer(&self, mount: usize) -> Result<Option<cap_std::fs::Dir>, types::Error> {
        let binding = self.binding(mount)?;
        match &binding.backing {
            MountBacking::Direct { root, .. } => Ok(Some(
                root.dir()
                    .try_clone()
                    .map_err(|e| types::Error::trap(anyhow!("clone direct layer: {e}")))?,
            )),
            MountBacking::CopyOnWrite { .. } => Ok(None),
        }
    }

    fn ensure_writable(&self, mount: usize) -> Result<(), types::Error> {
        if self.mount_writable(mount)? {
            Ok(())
        } else {
            Err(types::Errno::Perm.into())
        }
    }

    /// Look up an fd entry.
    fn fd(&self, fd: types::Fd) -> Result<&FdEntry, types::Error> {
        let index = u32::from(fd) as usize;
        match self.fds.get(index).and_then(Option::as_ref) {
            Some(entry) => Ok(entry),
            None => Err(types::Errno::Badf.into()),
        }
    }

    fn fd_mut(&mut self, fd: types::Fd) -> Result<&mut FdEntry, types::Error> {
        self.replay_guard("descriptor_effect")?;
        let index = u32::from(fd) as usize;
        match self.fds.get_mut(index).and_then(Option::as_mut) {
            Some(entry) => Ok(entry),
            None => Err(types::Errno::Badf.into()),
        }
    }

    fn alloc_fd(&mut self, entry: FdEntry) -> types::Fd {
        for (index, slot) in self.fds.iter_mut().enumerate().skip(3) {
            if slot.is_none() {
                *slot = Some(entry);
                return types::Fd::from(index as u32);
            }
        }
        self.fds.push(Some(entry));
        types::Fd::from((self.fds.len() - 1) as u32)
    }

    fn dir_handle(&self, fd: types::Fd) -> Result<&DirHandle, types::Error> {
        self.replay_guard("unrecorded_path_observation_or_effect")?;
        match self.fd(fd)? {
            FdEntry::Dir(handle) => Ok(handle),
            _ => Err(types::Errno::Notdir.into()),
        }
    }

    fn file_handle_mut(&mut self, fd: types::Fd) -> Result<&mut FileHandle, types::Error> {
        match self.fd_mut(fd)? {
            FdEntry::File(handle) => Ok(handle),
            _ => Err(types::Errno::Badf.into()),
        }
    }

    fn is_dir_fd(&self, fd: types::Fd) -> bool {
        matches!(self.fd(fd), Ok(FdEntry::Dir(_)))
    }

    // ------------------------------------------------------------------
    // Path resolution
    // ------------------------------------------------------------------

    /// Normalize `path` (a WASI relative path) onto `base`, rejecting NULs,
    /// escapes above the mount root and empty components. `.`/`..` resolve
    /// lexically, exactly like the preview1 adapters.
    fn normalize(base: &Path, path: &str) -> Result<PathBuf, types::Error> {
        let mut out = base.to_path_buf();
        if path.is_empty() {
            return Err(types::Errno::Noent.into());
        }
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if !out.pop() {
                        return Err(types::Errno::Notdir.into());
                    }
                }
                name => out.push(name),
            }
        }
        Ok(out)
    }

    /// Effective (COW-aware) symlink_metadata of `rel` under a mount:
    /// overlay wins; tombstones hide the base.
    fn effective_symlink_metadata(
        &self,
        mount: usize,
        rel: &Path,
    ) -> Result<Option<cap_std::fs::Metadata>, types::Error> {
        let binding = self.binding(mount)?;
        let (overlay_root, base_root) = layer_roots(binding);
        let tombstones = mount_tombstones(binding);
        if let Some(dir) = overlay_root {
            if let Ok(meta) = dir.symlink_metadata(rel) {
                return Ok(Some(meta));
            }
        }
        if let Some(ts) = tombstones {
            if ts.is_removed(rel) {
                return Ok(None);
            }
        }
        if let Some(dir) = base_root {
            if let Ok(meta) = dir.symlink_metadata(rel) {
                return Ok(Some(meta));
            }
        }
        Ok(None)
    }

    /// Open one directory layer child with no-follow discipline.
    fn open_layer_dir(
        dir: &cap_std::fs::Dir,
        name: &OsString,
    ) -> Result<Option<cap_std::fs::Dir>, types::Error> {
        match fsx::open_component(dir, Path::new(name)) {
            Ok(Some(file)) => Ok(Some(cap_std::fs::Dir::from_std_file(file))),
            Ok(None) => Ok(None),
            Err(error) => Err(errno_from_io(error).into()),
        }
    }

    /// Open one file layer child with explicit open options, no-follow.
    fn open_layer_file(
        dir: &cap_std::fs::Dir,
        name: &OsString,
        options: &cap_std::fs::OpenOptions,
    ) -> Result<Option<std::fs::File>, types::Error> {
        match fsx::open_component_with(dir, Path::new(name), options) {
            Ok(Some(file)) => Ok(Some(file)),
            Ok(None) => Ok(None),
            Err(error) => Err(errno_from_io(error).into()),
        }
    }

    /// Resolve (dirfd, path) into (parent dir handle, final component name),
    /// following symlinks per `follow` with a bounded depth. Every component
    /// is pinned through the no-follow union layers, so guest-created
    /// symlinks cannot redirect resolution outside the admitted mount.
    fn resolve_parent(
        &self,
        dirfd: types::Fd,
        path: &str,
        follow: bool,
    ) -> Result<(DirHandle, OsString), types::Error> {
        let mut handle = self.dir_handle(dirfd)?.clone();
        let full = Self::normalize(&handle.rel, path)?;
        let mut queue: std::collections::VecDeque<OsString> = full
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect();
        if queue.is_empty() {
            return Err(types::Errno::Noent.into());
        }

        let mut depth = 0usize;
        // Walk intermediate components; symlink targets are spliced back
        // into the front of the queue.
        while queue.len() > 1 {
            let name = queue.pop_front().expect("len checked");
            if name == ".." {
                handle = self.parent_handle(&handle)?;
                continue;
            }
            let (next, splice) = self.step(&handle, &name, &mut depth)?;
            handle = next;
            if let Some(target) = splice {
                for (at, comp) in target.into_iter().enumerate() {
                    queue.insert(at, comp);
                }
                if queue.len() > 4096 {
                    return Err(types::Errno::Nametoolong.into());
                }
            }
        }

        let mut final_name = queue.pop_front().expect("len checked");
        if follow {
            loop {
                let rel = handle.rel.join(&final_name);
                let is_link = self
                    .effective_symlink_metadata(handle.mount, &rel)
                    .map(|meta| meta.map(|m| m.file_type().is_symlink()).unwrap_or(false))?;
                if !is_link {
                    break;
                }
                let (next, splice) = self.step(&handle, &final_name, &mut depth)?;
                handle = next;
                let mut target: std::collections::VecDeque<OsString> = splice
                    .expect("step splices symlink targets")
                    .into_iter()
                    .collect();
                while target.len() > 1 {
                    let name = target.pop_front().expect("len checked");
                    if name == ".." {
                        handle = self.parent_handle(&handle)?;
                        continue;
                    }
                    let (next, splice) = self.step(&handle, &name, &mut depth)?;
                    handle = next;
                    if let Some(more) = splice {
                        for (at, comp) in more.into_iter().enumerate() {
                            target.insert(at, comp);
                        }
                    }
                }
                final_name = target.pop_front().expect("len checked");
            }
        }
        Ok((handle, final_name))
    }

    /// The union dir handle one level above `handle` (symlink targets may
    /// walk above the link's directory). Escaping the mount root fails.
    fn parent_handle(&self, handle: &DirHandle) -> Result<DirHandle, types::Error> {
        let rel = handle
            .rel
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or(types::Errno::Notdir)?;
        self.dir_handle_at(handle.mount, &rel)
    }

    /// Build the union dir handle at `rel` within a mount: both layers
    /// opened independently (either may be absent unless tombstoned).
    fn dir_handle_at(&self, mount: usize, rel: &Path) -> Result<DirHandle, types::Error> {
        let binding = self.binding(mount)?;
        let (overlay_root, base_root) = layer_roots(binding);
        let tombstones = mount_tombstones(binding);
        let base_hidden = tombstones
            .map(|ts| ts.is_removed(rel))
            .unwrap_or(false);
        let open_at = |root: &cap_std::fs::Dir| -> Result<Option<cap_std::fs::Dir>, types::Error> {
            if rel.as_os_str().is_empty() {
                return root
                    .try_clone()
                    .map(Some)
                    .map_err(|e| types::Error::trap(anyhow!("clone layer: {e}")));
            }
            let mut dir = root
                .try_clone()
                .map_err(|e| types::Error::trap(anyhow!("clone layer: {e}")))?;
            for component in rel.components() {
                let name = match component {
                    std::path::Component::Normal(name) => name,
                    _ => continue,
                };
                let file = match fsx::open_component(&dir, Path::new(name)) {
                    Ok(Some(file)) => file,
                    Ok(None) => return Ok(None),
                    Err(error) => return Err(errno_from_io(error).into()),
                };
                dir = cap_std::fs::Dir::from_std_file(file);
            }
            Ok(Some(dir))
        };
        let overlay = match overlay_root {
            Some(root) => open_at(root)?,
            None => None,
        };
        let base = if base_hidden {
            None
        } else {
            match base_root {
                Some(root) => open_at(root)?,
                None => None,
            }
        };
        if overlay.is_none() && base.is_none() {
            return Err(types::Errno::Noent.into());
        }
        Ok(DirHandle {
            mount,
            rel: rel.to_path_buf(),
            overlay,
            base,
        })
    }

    /// Open the child directory of `parent` named `name` in the effective
    /// view. The overlay wins: when it holds the name, the base layer is
    /// consulted only for directory merges (a tombstoned base is hidden).
    fn open_dir_child(&self, parent: &DirHandle, name: &OsString) -> Result<DirHandle, types::Error> {
        let child_rel = parent.rel.join(name);
        let binding = self.binding(parent.mount)?;
        let (overlay_dir, base_dir) = parent.layer_dirs();
        let tombstones = mount_tombstones(binding);
        let base_hidden = tombstones
            .map(|ts| ts.is_removed(&child_rel))
            .unwrap_or(false);
        let overlay_meta = overlay_dir
            .and_then(|d| d.symlink_metadata(name).ok());
        let overlay = match &overlay_meta {
            Some(meta) if meta.is_dir() => Some(
                Self::open_layer_dir(overlay_dir.expect("overlay layer checked"), name)?
                    .expect("dir presence checked"),
            ),
            Some(_) => return Err(types::Errno::Notdir.into()),
            None => None,
        };
        let base = if base_hidden {
            None
        } else {
            match base_dir.and_then(|d| d.symlink_metadata(name).ok()) {
                Some(meta) if meta.is_dir() => Some(
                    Self::open_layer_dir(base_dir.expect("base layer checked"), name)?
                        .expect("dir presence checked"),
                ),
                _ => None,
            }
        };
        if overlay.is_none() && base.is_none() {
            return Err(types::Errno::Noent.into());
        }
        Ok(DirHandle {
            mount: parent.mount,
            rel: child_rel,
            overlay,
            base,
        })
    }

    /// Resolve one path component below a dir handle in the effective view.
    /// Returns the child dir handle when the child is a directory, the
    /// unchanged parent with a symlink target to splice when the child is a
    /// symlink, or the unchanged parent when the child is a regular file.
    fn step(
        &self,
        handle: &DirHandle,
        name: &OsString,
        depth: &mut usize,
    ) -> Result<(DirHandle, Option<Vec<OsString>>), types::Error> {
        let child_rel = handle.rel.join(name);
        let binding = self.binding(handle.mount)?;
        let (overlay_dir, base_dir) = handle.layer_dirs();
        let tombstones = mount_tombstones(binding);
        let base_hidden = tombstones
            .map(|ts| ts.is_removed(&child_rel))
            .unwrap_or(false);
        let overlay_meta = overlay_dir.and_then(|d| d.symlink_metadata(name).ok());
        let base_meta = if base_hidden {
            None
        } else {
            base_dir.and_then(|d| d.symlink_metadata(name).ok())
        };
        let meta = overlay_meta
            .clone()
            .or(base_meta)
            .ok_or(types::Errno::Noent)?;

        if meta.file_type().is_symlink() {
            *depth += 1;
            if *depth > MAX_SYMLINK_DEPTH {
                return Err(types::Errno::Loop.into());
            }
            let target = (if overlay_meta.is_some() {
                overlay_dir
            } else {
                base_dir
            })
            .expect("symlink layer present")
            .read_link(name)
            .map_err(|e| {
                types::Error::trap(anyhow!("read_link {:?}: {e}", child_rel.display()))
            })?;
            if target.is_absolute() {
                // Absolute host paths would escape the admitted capability.
                return Err(types::Errno::Perm.into());
            }
            let comps: Vec<OsString> = target
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(n) => Some(n.to_os_string()),
                    _ => None,
                })
                .collect();
            // The link target resolves below the link's own parent.
            return Ok((handle.clone(), Some(comps)));
        }

        if meta.is_dir() {
            return Ok((self.open_dir_child(handle, name)?, None));
        }
        // Regular (or special) file: the caller opens the child by name
        // through the effective layers; resolution stops at this parent.
        Ok((handle.clone(), None))
    }

    /// Open a regular file in the effective view (overlay wins; tombstones
    /// hide the base). `write` requests copy the base content up into the
    /// overlay for COW mounts before opening.
    fn open_effective_file(
        &mut self,
        parent: &DirHandle,
        name: &OsString,
        create: bool,
        create_new: bool,
        truncate: bool,
        read: bool,
        write: bool,
        append: bool,
    ) -> Result<std::fs::File, types::Error> {
        let rel = parent.rel.join(name);
        let mount = parent.mount;
        let is_cow = matches!(
            self.binding(mount)?.backing,
            MountBacking::CopyOnWrite { .. }
        );
        // Effective presence is computed from the layers without keeping a
        // borrow of `self` alive across the mutable operations below.
        let (overlay_has, base_has, base_is_dir) = {
            let binding = self.binding(mount)?;
            let (overlay_dir, base_dir) = parent.layer_dirs();
            let tombstones = mount_tombstones(binding);
            let overlay_meta = overlay_dir.and_then(|d| d.symlink_metadata(name).ok());
            let base_meta = base_dir.and_then(|d| d.symlink_metadata(name).ok());
            let hidden = tombstones.map(|ts| ts.is_removed(&rel)).unwrap_or(false);
            let overlay_has = overlay_meta.is_some();
            let base_has = !overlay_has && !hidden && base_meta.is_some();
            let base_is_dir = base_meta.map(|m| m.is_dir()).unwrap_or(false);
            (overlay_has, base_has, base_is_dir)
        };

        if create_new && (overlay_has || base_has) {
            return Err(types::Errno::Exist.into());
        }
        if !overlay_has && !base_has && !create {
            return Err(types::Errno::Noent.into());
        }
        if write || append {
            self.ensure_writable(mount)?;
            if base_is_dir && !overlay_has {
                return Err(types::Errno::Isdir.into());
            }
        }

        let mut options = cap_std::fs::OpenOptions::new();
        options.read(read).write(write).append(append).truncate(truncate);
        if create {
            options.create(true);
        }
        if create_new {
            options.create_new(true);
        }

        if !is_cow {
            // `name` is relative to the resolved parent, not the mount root.
            let dir = parent.overlay.as_ref().ok_or(types::Errno::Noent)?;
            return Self::open_layer_file(dir, name, &options)?
                .ok_or_else(|| types::Error::trap(anyhow!("open vanished mid-resolution")));
        }

        if !overlay_has {
            if !write && !append && !create {
                // Pure read of a base file: no copy-up.
                let base = parent.base.as_ref().ok_or(types::Errno::Noent)?;
                return Self::open_layer_file(base, name, &options)?.ok_or_else(|| {
                    types::Error::trap(anyhow!("open vanished mid-resolution"))
                });
            }
            // Materialize the parent chain and copy the base bytes up.
            self.materialize_dirs(mount, &rel)?;
        }
        // Reuse the pinned parent when it already has an overlay. Otherwise
        // open the newly materialized parent through the mount capability.
        let materialized_parent = if parent.overlay.is_none() {
            Some(self.dir_handle_at(mount, &parent.rel)?)
        } else {
            None
        };
        let overlay = parent.overlay.as_ref()
            .or_else(|| materialized_parent.as_ref().and_then(|parent| parent.overlay.as_ref()))
            .ok_or(types::Errno::Noent)?;
        if !overlay_has && base_has {
            let base = parent.base.as_ref().ok_or(types::Errno::Noent)?;
            Self::copy_up(base, overlay, name)?;
        }
        Self::open_layer_file(overlay, name, &options)?
            .ok_or_else(|| types::Error::trap(anyhow!("open vanished mid-resolution")))
    }

    /// Create any missing ancestor directories of `rel` in the overlay
    /// (clearing tombstones for recreated paths) — confined to the overlay
    /// root handle.
    fn materialize_dirs(&mut self, mount: usize, rel: &Path) -> Result<(), types::Error> {
        let ancestors: Vec<PathBuf> = rel
            .parent()
            .map(|parent| {
                let mut v = Vec::new();
                let mut p = parent;
                while !p.as_os_str().is_empty() {
                    v.push(p.to_path_buf());
                    let Some(parent) = p.parent() else {
                        break;
                    };
                    p = parent;
                }
                v.reverse();
                v
            })
            .unwrap_or_default();
        let mut cleared = false;
        {
            let store = self.cow_store(mount)?;
            for ancestor in &ancestors {
                let _ = store.overlay_dir().create_dir(ancestor);
                if store.tombstones.is_removed(ancestor) {
                    store.tombstones.clear_below(ancestor);
                    cleared = true;
                }
            }
        }
        if cleared {
            self.cow_store(mount)?.persist_tombstones().map_err(|e| {
                types::Error::trap(anyhow!("persist tombstones: {e}"))
            })?;
        }
        Ok(())
    }

    /// Copy one base file into the overlay (bytes + permissions).
    fn copy_up(
        base_dir: &cap_std::fs::Dir,
        overlay_dir: &cap_std::fs::Dir,
        name: &OsString,
    ) -> Result<(), types::Error> {
        let mut read_options = cap_std::fs::OpenOptions::new();
        read_options.read(true);
        let src = Self::open_layer_file(base_dir, name, &read_options)?
            .ok_or(types::Errno::Noent)?;
        let mut dst_options = cap_std::fs::OpenOptions::new();
        dst_options.write(true).create(true).truncate(true);
        let mut dst = Self::open_layer_file(overlay_dir, name, &dst_options)?
            .ok_or_else(|| types::Error::trap(anyhow!("copy-up open failed")))?;
        let mut reader = std::io::BufReader::new(src);
        std::io::copy(&mut reader, &mut dst).map_err(|e| {
            types::Error::trap(anyhow!("copy-up {:?}: {e}", name))
        })?;
        if let Ok(meta) = base_dir.symlink_metadata(name) {
            use std::os::unix::fs::PermissionsExt;
            let mode = cap_std::fs::PermissionsExt::mode(&meta.permissions());
            let _ = dst.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        Ok(())
    }

    /// Recursively move the effective subtree rooted at `src` into the
    /// overlay at `dest_rel` (used by cross-directory renames whose source
    /// lives partly in the base layer). Depth and entry count are bounded.
    fn move_tree_to_overlay(
        &mut self,
        mount: usize,
        src: &DirHandle,
        dest_rel: &Path,
        depth: usize,
    ) -> Result<(), types::Error> {
        if depth > MAX_TREE_DEPTH {
            return Err(types::Error::trap(anyhow!(
                "rename tree deeper than {MAX_TREE_DEPTH}"
            )));
        }
        let created = self.cow_store(mount)?.overlay_dir().create_dir(dest_rel);
        if created.as_ref().err().map(|e| e.kind()) != Some(std::io::ErrorKind::AlreadyExists) {
            created.map_err(errno_from_io)?;
        }
        let ts_ref = match &self.binding(mount)?.backing {
            MountBacking::CopyOnWrite { store, .. } => Some(&store.tombstones),
            _ => None,
        };
        let children = effective_children(src, ts_ref)?;
        if children.len() > MAX_TREE_ENTRIES {
            return Err(types::Error::trap(anyhow!(
                "rename tree wider than {MAX_TREE_ENTRIES}"
            )));
        }
        for (name, meta, _in_overlay) in children {
            let child_src_rel = src.rel.join(&name);
            let child_dest_rel = dest_rel.join(&name);
            let file_type = meta.file_type();
            if file_type.is_dir() {
                let child = self.open_dir_child(src, &name)?;
                self.move_tree_to_overlay(mount, &child, &child_dest_rel, depth + 1)?;
            } else if file_type.is_symlink() {
                let target = src
                    .overlay
                    .as_ref()
                    .and_then(|d| d.read_link(&name).ok())
                    .or_else(|| src.base.as_ref().and_then(|d| d.read_link(&name).ok()))
                    .ok_or(types::Errno::Noent)?;
                self.cow_store(mount)?
                    .overlay_dir()
                    .symlink(target.as_os_str(), &child_dest_rel)
                    .map_err(errno_from_io)?;
            } else {
                // Regular file: a writable open copies it up at the source
                // path; rename within the overlay to the destination.
                let _ = self.open_effective_file(
                    src,
                    &name,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )?;
                let overlay = self.cow_store(mount)?.overlay_dir();
                overlay
                    .rename(&child_src_rel, overlay, &child_dest_rel)
                    .map_err(errno_from_io)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // stdio helpers
    // ------------------------------------------------------------------

    fn read_iovs(
        memory: &GuestMemory<'_>,
        iovs: types::IovecArray,
    ) -> Result<Vec<u8>, types::Error> {
        let count = iovs.len();
        let mut out = Vec::new();
        for index in 0..count {
            let iov = memory
                .read::<types::Iovec>(iovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            let len = iov.buf_len as usize;
            if out.len().saturating_add(len) > MAX_IOV_BYTES {
                return Err(types::Errno::Inval.into());
            }
            let slice = iov.buf.as_array(iov.buf_len);
            let bytes = memory.to_vec(slice).map_err(|e| {
                types::Error::trap(anyhow!("iov buffer: {e}"))
            })?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    fn write_iovs(
        memory: &mut GuestMemory<'_>,
        iovs: types::IovecArray,
        data: &[u8],
    ) -> Result<types::Size, types::Error> {
        let count = iovs.len();
        let mut written = 0usize;
        for index in 0..count {
            if written >= data.len() {
                break;
            }
            let iov = memory
                .read::<types::Iovec>(iovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            let take = (iov.buf_len as usize).min(data.len() - written);
            if take == 0 {
                continue;
            }
            let slice = iov.buf.as_array(take as u32);
            memory
                .copy_from_slice(&data[written..written + take], slice)
                .map_err(|e| types::Error::trap(anyhow!("iov write: {e}")))?;
            written += take;
        }
        Ok(written as types::Size)
    }

    fn write_ciovs(
        memory: &mut GuestMemory<'_>,
        ciovs: types::CiovecArray,
    ) -> Result<Vec<u8>, types::Error> {
        let count = ciovs.len();
        let mut out = Vec::new();
        for index in 0..count {
            let iov = memory
                .read::<types::Ciovec>(ciovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("ciovec: {e}")))?;
            let len = iov.buf_len as usize;
            if out.len().saturating_add(len) > MAX_IOV_BYTES {
                return Err(types::Errno::Inval.into());
            }
            let slice = iov.buf.as_array(iov.buf_len);
            let bytes = memory.to_vec(slice).map_err(|e| {
                types::Error::trap(anyhow!("ciov buffer: {e}"))
            })?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    fn read_file_at(file: &mut std::fs::File, cursor: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        file.seek(SeekFrom::Start(cursor))?;
        file.read(buf)
    }

    fn write_file_at(
        file: &mut std::fs::File,
        cursor: u64,
        append: bool,
        data: &[u8],
    ) -> std::io::Result<usize> {
        if append {
            file.seek(SeekFrom::End(0))?;
        } else {
            file.seek(SeekFrom::Start(cursor))?;
        }
        file.write(data)
    }

    fn fd_rights(&self, fd: types::Fd) -> Result<types::Rights, types::Error> {
        const READ_DIR: types::Rights = types::Rights::FD_READ
            .union(types::Rights::FD_READDIR)
            .union(types::Rights::FD_FILESTAT_GET)
            .union(types::Rights::PATH_OPEN)
            .union(types::Rights::PATH_FILESTAT_GET)
            .union(types::Rights::PATH_READLINK);
        const WRITE_DIR: types::Rights = types::Rights::PATH_CREATE_DIRECTORY
            .union(types::Rights::PATH_CREATE_FILE)
            .union(types::Rights::PATH_FILESTAT_SET_TIMES)
            .union(types::Rights::PATH_LINK_SOURCE)
            .union(types::Rights::PATH_LINK_TARGET)
            .union(types::Rights::PATH_RENAME_SOURCE)
            .union(types::Rights::PATH_RENAME_TARGET)
            .union(types::Rights::PATH_UNLINK_FILE)
            .union(types::Rights::FD_FILESTAT_SET_TIMES);
        match self.fd(fd)? {
            FdEntry::Stdin(_) => Ok(types::Rights::FD_READ),
            FdEntry::Recorded { directory: false, .. } => Ok(types::Rights::FD_FILESTAT_GET
                .union(types::Rights::FD_SYNC).union(types::Rights::FD_READ)),
            FdEntry::Recorded { directory: true, .. } => {
                self.replay_guard("fdstat: recorded directory rights")?;
                Err(types::Error::trap(anyhow!("REPLAY_IO_UNOBSERVED: directory rights")))
            },
            FdEntry::Stdout(_) | FdEntry::Stderr(_) => Ok(types::Rights::FD_WRITE),
            FdEntry::File(handle) => {
                let mut rights = types::Rights::FD_FILESTAT_GET.union(types::Rights::FD_SYNC);
                if handle.readable {
                    rights |= types::Rights::FD_READ;
                }
                if handle.writable {
                    rights |= types::Rights::FD_WRITE
                        | types::Rights::FD_SEEK
                        | types::Rights::FD_TELL
                        | types::Rights::FD_FILESTAT_SET_SIZE;
                }
                Ok(rights)
            }
            FdEntry::Dir(handle) => {
                let mut rights = READ_DIR;
                if self.mount_writable(handle.mount)? {
                    rights |= WRITE_DIR;
                }
                Ok(rights)
            }
            FdEntry::Socket(handle) => Ok(match &handle.socket {
                // A connected stream: read/write payload, change blocking
                // mode, poll for readiness and shut one direction down. It is
                // not a path, cannot be re-pointed, and cannot accept.
                AdmittedSocket::Stream(_) => types::Rights::FD_READ
                    .union(types::Rights::FD_WRITE)
                    .union(types::Rights::FD_FDSTAT_SET_FLAGS)
                    .union(types::Rights::POLL_FD_READWRITE)
                    .union(types::Rights::SOCK_SHUTDOWN),
                // A listener can only change flags, poll and accept inbound
                // streams; it never reads or writes payload itself.
                AdmittedSocket::Listener(_) => types::Rights::FD_FDSTAT_SET_FLAGS
                    .union(types::Rights::POLL_FD_READWRITE)
                    .union(types::Rights::SOCK_ACCEPT),
            }),
        }
    }

    fn filetype_of(meta: &cap_std::fs::Metadata) -> types::Filetype {
        let ty = meta.file_type();
        if ty.is_dir() {
            types::Filetype::Directory
        } else if ty.is_file() {
            types::Filetype::RegularFile
        } else if ty.is_symlink() {
            types::Filetype::SymbolicLink
        } else {
            types::Filetype::Unknown
        }
    }

    fn filestat_of(meta: &cap_std::fs::Metadata) -> types::Filestat {
        use cap_std::fs::MetadataExt;
        types::Filestat {
            dev: meta.dev(),
            ino: meta.ino(),
            filetype: Self::filetype_of(meta),
            nlink: meta.nlink(),
            size: meta.size(),
            atim: Self::nanos(meta.atime(), meta.atime_nsec()),
            mtim: Self::nanos(meta.mtime(), meta.mtime_nsec()),
            ctim: Self::nanos(meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// Nanoseconds since the Unix epoch from a seconds/nanoseconds pair.
    fn nanos(seconds: i64, nanos: i64) -> u64 {
        let seconds = seconds.max(0) as u64;
        let nanos = nanos.max(0) as u64;
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(nanos)
    }

    /// Create a directory below an already-resolved parent. Shared by
    /// path_create_directory and CREAT|DIRECTORY path_open.
    fn create_directory_at(
        &mut self,
        parent: &DirHandle,
        name: &OsString,
        may_exist: bool,
    ) -> Result<(), types::Error> {
        self.ensure_writable(parent.mount)?;
        let rel = parent.rel.join(name);
        if self.is_cow(parent.mount)? {
            if let Some(meta) = self.effective_symlink_metadata(parent.mount, &rel)? {
                return if may_exist && meta.is_dir() {
                    Ok(())
                } else {
                    Err(types::Errno::Exist.into())
                };
            }
            self.materialize_dirs(parent.mount, &rel)?;
            {
                let store = self.cow_store(parent.mount)?;
                let result = store.overlay_dir().create_dir(&rel);
                let already = result.as_ref().err().map(|e| e.kind())
                    == Some(std::io::ErrorKind::AlreadyExists);
                if !(may_exist && already) {
                    result.map_err(errno_from_io)?;
                }
                store.tombstones.clear_below(&rel);
            }
            self.cow_store(parent.mount)?
                .persist_tombstones()
                .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            return Ok(());
        }
        let root = self
            .direct_layer(parent.mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        let result = root.create_dir(&rel);
        if may_exist
            && result.as_ref().err().map(|e| e.kind())
                == Some(std::io::ErrorKind::AlreadyExists)
        {
            return Ok(());
        }
        result.map_err(errno_from_io)?;
        Ok(())
    }
}

impl Clone for DirHandle {
    fn clone(&self) -> Self {
        Self {
            mount: self.mount,
            rel: self.rel.clone(),
            overlay: self
                .overlay
                .as_ref()
                .and_then(|d| d.try_clone().ok()),
            base: self.base.as_ref().and_then(|d| d.try_clone().ok()),
        }
    }
}

/// The root handles of a binding's resolution layers.
///
/// These handles sit at the mount root, so they address mount-relative
/// paths. The handles stored in [`DirHandle`] are opened children and sit at
/// `DirHandle::rel` instead; child lookups use those.
fn layer_roots(binding: &MountBinding) -> (Option<&cap_std::fs::Dir>, Option<&cap_std::fs::Dir>) {
    match &binding.backing {
        MountBacking::Direct { root, .. } => (Some(root.dir()), None),
        MountBacking::CopyOnWrite { base, store, .. } => {
            (Some(store.overlay_dir()), Some(base.dir()))
        }
    }
}

/// The durable tombstone set of a mount, if it is a copy-on-write mount.
fn mount_tombstones(binding: &MountBinding) -> Option<&crate::cow::Tombstones> {
    match &binding.backing {
        MountBacking::CopyOnWrite { store, .. } => Some(&store.tombstones),
        MountBacking::Direct { .. } => None,
    }
}

/// Effective children of a directory handle: overlay ∪ base − tombstones,
/// overlay winning name conflicts, sorted for stable readdir cookies.
fn effective_children(
    handle: &DirHandle,
    tombstones: Option<&crate::cow::Tombstones>,
) -> Result<Vec<(OsString, cap_std::fs::Metadata, bool)>, types::Error> {
    let mut map: std::collections::BTreeMap<OsString, (cap_std::fs::Metadata, bool)> =
        std::collections::BTreeMap::new();
    if let Some(base) = handle.base.as_ref() {
        for entry in read_dir_entries(base, &handle.rel)? {
            let (name, meta) = entry;
            if let Some(ts) = tombstones {
                if ts.is_removed(&handle.rel.join(&name)) {
                    continue;
                }
            }
            map.entry(name).or_insert((meta, false));
        }
    }
    if let Some(overlay) = handle.overlay.as_ref() {
        for (name, meta) in read_dir_entries(overlay, &handle.rel)? {
            map.insert(name, (meta, true));
        }
    }
    Ok(map
        .into_iter()
        .map(|(name, (meta, in_overlay))| (name, meta, in_overlay))
        .collect())
}

fn read_dir_entries(
    dir: &cap_std::fs::Dir,
    rel: &Path,
) -> Result<Vec<(OsString, cap_std::fs::Metadata)>, types::Error> {
    let mut out = Vec::new();
    // The layer handle is already positioned at `rel`; listing it directly
    // avoids re-resolving (and re-following) the path beneath the layer.
    let read = dir.entries();
    let entries = match read {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return io_err(e),
    };
    for entry in entries {
        let entry = entry.map_err(|e| {
            types::Error::trap(anyhow!("read_dir {}: {e}", rel.display()))
        })?;
        let name = entry.file_name();
        let meta = entry
            .metadata()
            .map_err(|e| types::Error::trap(anyhow!("dir entry metadata: {e}")))?;
        out.push((name, meta));
    }
    Ok(out)
}

impl wasi_snapshot_preview1::WasiSnapshotPreview1 for P1Host {
    fn set_hostcall_fuel(&mut self, fuel: usize) {
        self.hostcall_fuel = fuel;
    }

    fn args_get(
        &mut self,
        memory: &mut GuestMemory<'_>,
        argv: GuestPtr<GuestPtr<u8>>,
        argv_buf: GuestPtr<u8>,
    ) -> Result<(), types::Error> {
        let mut offset = 0usize;
        for (index, arg) in self.args.clone().iter().enumerate() {
            let ptr = argv_buf.add(offset as u32).map_err(|e| {
                types::Error::trap(anyhow!("argv ptr: {e}"))
            })?;
            memory
                .write(argv.add(index as u32).map_err(|e| {
                    types::Error::trap(anyhow!("argv slot: {e}"))
                })?, ptr)
                .map_err(|e| types::Error::trap(anyhow!("argv slot write: {e}")))?;
            let region = ptr.as_array(arg.len() as u32 + 1);
            let mut bytes = arg.clone();
            bytes.push(0);
            memory
                .copy_from_slice(&bytes, region)
                .map_err(|e| types::Error::trap(anyhow!("argv copy: {e}")))?;
            offset += bytes.len();
        }
        Ok(())
    }

    fn args_sizes_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
    ) -> Result<(types::Size, types::Size), types::Error> {
        let count = self.args.len() as types::Size;
        let size = self
            .args
            .iter()
            .map(|arg| arg.len() as types::Size + 1)
            .sum();
        Ok((count, size))
    }

    fn environ_get(
        &mut self,
        memory: &mut GuestMemory<'_>,
        environ: GuestPtr<GuestPtr<u8>>,
        environ_buf: GuestPtr<u8>,
    ) -> Result<(), types::Error> {
        let mut offset = 0usize;
        let env = self.env.clone();
        for (index, (key, value)) in env.iter().enumerate() {
            let mut bytes = key.clone();
            bytes.push(b'=');
            bytes.extend_from_slice(value);
            bytes.push(0);
            let ptr = environ_buf.add(offset as u32).map_err(|e| {
                types::Error::trap(anyhow!("environ ptr: {e}"))
            })?;
            memory
                .write(environ.add(index as u32).map_err(|e| {
                    types::Error::trap(anyhow!("environ slot: {e}"))
                })?, ptr)
                .map_err(|e| types::Error::trap(anyhow!("environ slot write: {e}")))?;
            let region = ptr.as_array(bytes.len() as u32);
            memory
                .copy_from_slice(&bytes, region)
                .map_err(|e| types::Error::trap(anyhow!("environ copy: {e}")))?;
            offset += bytes.len();
        }
        Ok(())
    }

    fn environ_sizes_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
    ) -> Result<(types::Size, types::Size), types::Error> {
        let count = self.env.len() as types::Size;
        let size = self
            .env
            .iter()
            .map(|(key, value)| {
                (key.len() + value.len() + 2) as types::Size
            })
            .sum();
        Ok((count, size))
    }

    fn clock_res_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        id: types::Clockid,
    ) -> Result<types::Timestamp, types::Error> {
        match id {
            types::Clockid::Realtime | types::Clockid::Monotonic => Ok(1_000_000),
            _ => Err(types::Errno::Inval.into()),
        }
    }

    fn clock_time_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        id: types::Clockid,
        _precision: types::Timestamp,
    ) -> Result<types::Timestamp, types::Error> {
        let supply = || match id {
            types::Clockid::Realtime => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .map_err(|e| types::Error::trap(anyhow!("clock: {e}"))),
            types::Clockid::Monotonic => {
                Ok(self.monotonic_epoch.elapsed().as_nanos() as u64)
            }
            _ => Err(types::Errno::Inval.into()),
        };
        if let Some(io) = &self.replay_io {
            let request = format!("{id:?}:{_precision}");
            let bytes = io.input("clock_time_get", request.as_bytes(), 8,
                || supply().map(|value| value.to_le_bytes().to_vec())
                    .map_err(|error| ::anyhow::anyhow!("{error:?}")))
                .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
            let value: [u8; 8] = bytes.try_into().map_err(|_| types::Error::trap(anyhow!("REPLAY_IO_CORRUPT: clock length")))?;
            Ok(u64::from_le_bytes(value))
        } else { supply() }
    }

    fn fd_advise(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        _fd: types::Fd,
        _offset: types::Filesize,
        _len: types::Filesize,
        _advice: types::Advice,
    ) -> Result<(), types::Error> {
        // No-op: the host filesystem needs no application hints.
        Ok(())
    }

    fn fd_allocate(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        offset: types::Filesize,
        len: types::Filesize,
    ) -> Result<(), types::Error> {
        let handle = self.file_handle_mut(fd)?;
        if !handle.writable {
            return Err(types::Errno::Perm.into());
        }
        let current = handle
            .file
            .metadata()
            .map_err(|e| types::Error::trap(anyhow!("fd_allocate metadata: {e}")))?
            .len();
        let want = offset.saturating_add(len);
        if want > current {
            handle.file.set_len(want).map_err(|e| {
                types::Error::trap(anyhow!("fd_allocate set_len: {e}"))
            })?;
        }
        Ok(())
    }

    fn fd_close(&mut self, _memory: &mut GuestMemory<'_>, fd: types::Fd) -> Result<(), types::Error> {
        let index = u32::from(fd) as usize;
        match self.fds.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(types::Errno::Badf.into()),
        }
    }

    fn fd_datasync(&mut self, memory: &mut GuestMemory<'_>, fd: types::Fd) -> Result<(), types::Error> {
        self.fd_sync(memory, fd)
    }

    fn fd_fdstat_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
    ) -> Result<types::Fdstat, types::Error> {
        let filetype = match self.fd(fd)? {
            FdEntry::Stdin(_) | FdEntry::Stdout(_) | FdEntry::Stderr(_) => {
                types::Filetype::CharacterDevice
            }
            FdEntry::File(_) | FdEntry::Recorded { directory: false, .. } => types::Filetype::RegularFile,
            FdEntry::Recorded { directory: true, .. } => types::Filetype::Directory,
            FdEntry::Dir(_) => types::Filetype::Directory,
            // Preview1 has one byte-stream socket filetype; a listening
            // socket is reported the same way (it is only ever accepted on,
            // which its rights express).
            FdEntry::Socket(_) => types::Filetype::SocketStream,
        };
        let fs_flags = match self.fd(fd)? {
            FdEntry::File(handle) if handle.append => types::Fdflags::APPEND,
            FdEntry::Socket(handle) if handle.nonblocking => types::Fdflags::NONBLOCK,
            _ => types::Fdflags::empty(),
        };
        let rights = self.fd_rights(fd)?;
        Ok(types::Fdstat {
            fs_filetype: filetype,
            fs_flags,
            fs_rights_base: rights,
            fs_rights_inheriting: rights,
        })
    }

    fn fd_fdstat_set_flags(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        flags: types::Fdflags,
    ) -> Result<(), types::Error> {
        // For files only APPEND is representable (regular files and our stdio
        // adapters are always ready, so NONBLOCK/SYNC are dropped); a socket
        // honours NONBLOCK and refuses the modes it cannot express.
        match self.fd_mut(fd)? {
            FdEntry::File(handle) => {
                handle.append = flags.contains(types::Fdflags::APPEND);
                Ok(())
            }
            // A socket has no append or synchronised-I/O mode; only NONBLOCK
            // is representable, and it changes whether guest calls block.
            FdEntry::Socket(handle) => {
                if flags.contains(types::Fdflags::APPEND)
                    || flags.intersects(
                        types::Fdflags::SYNC | types::Fdflags::RSYNC | types::Fdflags::DSYNC,
                    )
                {
                    return Err(types::Errno::Inval.into());
                }
                handle.nonblocking = flags.contains(types::Fdflags::NONBLOCK);
                Ok(())
            }
            _ => Err(types::Errno::Badf.into()),
        }
    }

    fn fd_fdstat_set_rights(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        _fd: types::Fd,
        _fs_rights_base: types::Rights,
        _fs_rights_inheriting: types::Rights,
    ) -> Result<(), types::Error> {
        // Rights are derived from the mount admission, not negotiable.
        Ok(())
    }

    fn fd_filestat_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
    ) -> Result<types::Filestat, types::Error> {
        self.replay_guard("fd_filestat_get")?;
        match self.fd(fd)? {
            FdEntry::Recorded { .. } => Err(types::Error::trap(anyhow!("REPLAY_IO_UNOBSERVED: fd_filestat_get"))),
            // A connected socket has no filesystem identity, and preview1
            // exposes no socket-stat call; refuse instead of inventing a size
            // or inode.
            FdEntry::Socket(_) => Err(types::Errno::Notsup.into()),
            FdEntry::File(handle) => {
                let meta = fsx::file_metadata(&handle.file).map_err(|e| {
                    types::Error::trap(anyhow!("fd_filestat_get: {e}"))
                })?;
                Ok(Self::filestat_of(&meta))
            }
            FdEntry::Dir(handle) => {
                let layer = handle
                    .overlay
                    .as_ref()
                    .or(handle.base.as_ref())
                    .expect("dir handle has a layer");
                let meta = fsx::dir_cap_metadata(layer)
                    .map_err(|e| types::Error::trap(anyhow!("dir metadata: {e}")))?;
                Ok(Self::filestat_of(&meta))
            }
            FdEntry::Stdin(_) | FdEntry::Stdout(_) | FdEntry::Stderr(_) => Ok(types::Filestat {
                dev: 0,
                ino: 0,
                filetype: types::Filetype::CharacterDevice,
                nlink: 0,
                size: 0,
                atim: 0,
                mtim: 0,
                ctim: 0,
            }),
        }
    }

    fn fd_filestat_set_size(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        size: types::Filesize,
    ) -> Result<(), types::Error> {
        let handle = self.file_handle_mut(fd)?;
        if !handle.writable {
            return Err(types::Errno::Perm.into());
        }
        handle.file.set_len(size).map_err(|e| {
            types::Error::trap(anyhow!("fd_filestat_set_size: {e}"))
        })?;
        Ok(())
    }

    fn fd_filestat_set_times(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        atim: types::Timestamp,
        mtim: types::Timestamp,
        fst_flags: types::Fstflags,
    ) -> Result<(), types::Error> {
        if fst_flags.contains(types::Fstflags::ATIM_NOW) && fst_flags.contains(types::Fstflags::ATIM)
        {
            return Err(types::Errno::Inval.into());
        }
        if fst_flags.contains(types::Fstflags::MTIM_NOW) && fst_flags.contains(types::Fstflags::MTIM)
        {
            return Err(types::Errno::Inval.into());
        }
        let (file, writable) = match self.fd_mut(fd)? {
            FdEntry::File(handle) => (&mut handle.file, handle.writable),
            _ => return Err(types::Errno::Badf.into()),
        };
        if !writable {
            return Err(types::Errno::Perm.into());
        }
        let now = SystemTime::now();
        let meta = file
            .metadata()
            .map_err(|e| types::Error::trap(anyhow!("metadata: {e}")))?;
        let mut times = std::fs::FileTimes::new();
        if fst_flags.contains(types::Fstflags::ATIM) {
            times = times.set_accessed(std::time::UNIX_EPOCH + duration_nanos(atim));
        } else if fst_flags.contains(types::Fstflags::ATIM_NOW) {
            times = times.set_accessed(now);
        } else if let Ok(atime) = meta.accessed() {
            times = times.set_accessed(atime);
        }
        if fst_flags.contains(types::Fstflags::MTIM) {
            times = times.set_modified(std::time::UNIX_EPOCH + duration_nanos(mtim));
        } else if fst_flags.contains(types::Fstflags::MTIM_NOW) {
            times = times.set_modified(now);
        } else if let Ok(mtime) = meta.modified() {
            times = times.set_modified(mtime);
        }
        file.set_times(times)
            .map_err(|e| types::Error::trap(anyhow!("set_times: {e}")))?;
        Ok(())
    }

    fn fd_read(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        iovs: types::IovecArray,
    ) -> Result<types::Size, types::Error> {
        if self.is_socket(fd)? {
            return self.socket_fd_read(memory, fd, iovs);
        }
        let count = iovs.len();
        if count == 0 {
            return Ok(0);
        }
        let mut total = 0usize;
        for index in 0..count {
            let iov = memory
                .read::<types::Iovec>(iovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            if iov.buf_len == 0 {
                continue;
            }
            let bytes = self.read_input(fd, iov.buf_len as usize, None)?;
            let read_now = bytes.len();
            memory.copy_from_slice(&bytes, iov.buf.as_array(read_now as types::Size))
                .map_err(|e| types::Error::trap(anyhow!("iov copy: {e}")))?;
            total += read_now;
            if read_now < iov.buf_len as usize {
                break;
            }
        }
        Ok(total as types::Size)
    }

    fn fd_pread(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        iovs: types::IovecArray,
        offset: types::Filesize,
    ) -> Result<types::Size, types::Error> {
        // A socket has no file offset; this is ESPIPE, not a bad descriptor.
        if self.is_socket(fd)? {
            return Err(types::Errno::Spipe.into());
        }
        let count = iovs.len();
        let mut total = 0usize;
        for index in 0..count {
            let iov = memory
                .read::<types::Iovec>(iovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            if iov.buf_len == 0 {
                continue;
            }
            let bytes = self.read_input(fd, iov.buf_len as usize, Some(offset.saturating_add(total as u64)))?;
            let n = bytes.len();
            memory.copy_from_slice(&bytes, iov.buf.as_array(n as types::Size))
                .map_err(|e| types::Error::trap(anyhow!("iov copy: {e}")))?;
            total += n;
            if n < iov.buf_len as usize {
                break;
            }
        }
        Ok(total as types::Size)
    }

    fn fd_write(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ciovs: types::CiovecArray,
    ) -> Result<types::Size, types::Error> {
        // Sockets are checked before the replay branch: a live peer is not a
        // recordable filesystem effect, and `socket_entry` refuses a socket
        // under a bound transcript outright.
        if self.is_socket(fd)? {
            return self.socket_fd_write(memory, fd, ciovs);
        }
        let data = Self::write_ciovs(memory, ciovs)?;
        if data.is_empty() {
            return Ok(0);
        }
        if let Some(io) = self.replay_io.clone() {
            let fd_number = u32::from(fd);
            if fd_number > 2 {
                io.reject_unrecorded("fd_write: filesystem effect")
                    .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
            } else {
                let request = format!("{fd_number}:{}", blake3::hash(&data).to_hex());
                let count = io.input("fd_write_receipt", request.as_bytes(), 8, || {
                    let entry = self.fds.get_mut(fd_number as usize).and_then(Option::as_mut)
                        .ok_or_else(|| ::anyhow::anyhow!("bad output descriptor"))?;
                    let n = match entry {
                        FdEntry::Stdout(writer) | FdEntry::Stderr(writer) => writer.write(&data)?,
                        _ => ::anyhow::bail!("bad output descriptor"),
                    };
                    Ok((n as u64).to_le_bytes().to_vec())
                }).map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
                let count: [u8; 8] = count.try_into().map_err(|_| types::Error::trap(anyhow!("REPLAY_IO_CORRUPT: write receipt")))?;
                let n = usize::try_from(u64::from_le_bytes(count)).map_err(|_| types::Errno::Overflow)?;
                if n > data.len() { return Err(types::Error::trap(anyhow!("REPLAY_IO_CORRUPT: write size"))); }
                if io.is_replay() {
                    // Only the new invocation's explicitly owned stdout/err
                    // artifact is materialized; no original filesystem write
                    // or arbitrary descriptor effect is repeated.
                    match self.fds.get_mut(fd_number as usize).and_then(Option::as_mut) {
                        Some(FdEntry::Stdout(writer)) | Some(FdEntry::Stderr(writer)) =>
                            writer.write_all(&data[..n]).map_err(errno_from_io)?,
                        _ => return Err(types::Errno::Badf.into()),
                    }
                }
                return Ok(n as types::Size);
            }
        }
        let n = match self.fd_mut(fd)? {
            FdEntry::Stdout(writer) | FdEntry::Stderr(writer) => {
                writer.write(&data).map_err(errno_from_io)?
            }
            FdEntry::File(handle) => {
                if !handle.writable {
                    return Err(types::Errno::Perm.into());
                }
                let n = Self::write_file_at(&mut handle.file, handle.cursor, handle.append, &data)
                    .map_err(errno_from_io)?;
                if !handle.append {
                    handle.cursor = handle.cursor.saturating_add(n as u64);
                }
                n
            }
            _ => return Err(types::Errno::Badf.into()),
        };
        Ok(n as types::Size)
    }

    fn fd_pwrite(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ciovs: types::CiovecArray,
        offset: types::Filesize,
    ) -> Result<types::Size, types::Error> {
        if self.is_socket(fd)? {
            return Err(types::Errno::Spipe.into());
        }
        let data = Self::write_ciovs(memory, ciovs)?;
        let n = match self.fd_mut(fd)? {
            FdEntry::File(handle) => {
                if !handle.writable {
                    return Err(types::Errno::Perm.into());
                }
                Self::write_file_at(&mut handle.file, offset, false, &data).map_err(errno_from_io)?
            }
            _ => return Err(types::Errno::Spipe.into()),
        };
        Ok(n as types::Size)
    }

    fn fd_prestat_get(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
    ) -> Result<types::Prestat, types::Error> {
        match self.fd(fd)? {
            FdEntry::Dir(handle) => {
                let guest_path = &self.binding(handle.mount)?.spec.guest_path;
                Ok(types::Prestat::Dir(types::PrestatDir {
                    pr_name_len: guest_path
                        .len()
                        .try_into()
                        .map_err(|_| types::Error::trap(anyhow!("preopen name too long")))?,
                }))
            }
            _ => Err(types::Errno::Badf.into()),
        }
    }

    fn fd_prestat_dir_name(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        path: GuestPtr<u8>,
        path_max_len: types::Size,
    ) -> Result<(), types::Error> {
        let guest_path = match self.fd(fd)? {
            FdEntry::Dir(handle) => self.binding(handle.mount)?.spec.guest_path.clone(),
            _ => return Err(types::Errno::Badf.into()),
        };
        if (path_max_len as usize) < guest_path.len() {
            return Err(types::Errno::Nametoolong.into());
        }
        let region = path.as_array(guest_path.len() as u32);
        memory
            .copy_from_slice(guest_path.as_bytes(), region)
            .map_err(|e| types::Error::trap(anyhow!("prestat name copy: {e}")))?;
        Ok(())
    }

    fn fd_renumber(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        from_fd: types::Fd,
        to_fd: types::Fd,
    ) -> Result<(), types::Error> {
        if from_fd == to_fd {
            return Ok(());
        }
        let from = self
            .fds
            .get_mut(u32::from(from_fd) as usize)
            .and_then(Option::take)
            .ok_or(types::Errno::Badf)?;
        let slot = self
            .fds
            .get_mut(u32::from(to_fd) as usize)
            .ok_or(types::Errno::Badf)?;
        *slot = Some(from);
        Ok(())
    }

    fn fd_seek(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        offset: types::Filedelta,
        whence: types::Whence,
    ) -> Result<types::Filesize, types::Error> {
        match self.fd_mut(fd)? {
            FdEntry::File(handle) => {
                let base = match whence {
                    types::Whence::Set => 0i128,
                    types::Whence::Cur => handle.cursor as i128,
                    types::Whence::End => handle
                        .file
                        .metadata()
                        .map_err(|e| types::Error::trap(anyhow!("seek metadata: {e}")))?
                        .len() as i128,
                };
                let pos = base + offset as i128;
                if pos < 0 {
                    return Err(types::Errno::Inval.into());
                }
                handle.cursor = pos as u64;
                handle
                    .file
                    .seek(SeekFrom::Start(handle.cursor))
                    .map_err(|e| types::Error::trap(anyhow!("seek: {e}")))?;
                Ok(handle.cursor)
            }
            _ => Err(types::Errno::Spipe.into()),
        }
    }

    fn fd_sync(&mut self, _memory: &mut GuestMemory<'_>, fd: types::Fd) -> Result<(), types::Error> {
        match self.fd_mut(fd)? {
            FdEntry::File(handle) => handle
                .file
                .sync_all()
                .map_err(|e| types::Error::trap(anyhow!("fd_sync: {e}")))?,
            FdEntry::Dir(_) => {}
            _ => return Err(types::Errno::Badf.into()),
        }
        Ok(())
    }

    fn fd_tell(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
    ) -> Result<types::Filesize, types::Error> {
        match self.fd(fd)? {
            FdEntry::File(handle) => Ok(handle.cursor),
            _ => Err(types::Errno::Spipe.into()),
        }
    }

    fn fd_readdir(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        buf: GuestPtr<u8>,
        buf_len: types::Size,
        cookie: types::Dircookie,
    ) -> Result<types::Size, types::Error> {
        let handle = match self.fd(fd)? {
            FdEntry::Dir(handle) => handle.clone(),
            _ => return Err(types::Errno::Notdir.into()),
        };
        let binding = self.binding(handle.mount)?;
        let tombstones = match &binding.backing {
            MountBacking::CopyOnWrite { store, .. } => Some(&store.tombstones),
            _ => None,
        };
        let children = effective_children(&handle, tombstones)?;
        let mut written = 0usize;
        let mut index = cookie as usize;
        while index < children.len() {
            let (name, meta, in_overlay) = &children[index];
            let filetype = Self::filetype_of(meta);
            // Stable pseudo-inode: hash of (layer, mount, rel path).
            let identity = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                (in_overlay, handle.mount, handle.rel.join(name)).hash(&mut hasher);
                hasher.finish()
            };
            let dirent_size = types::Dirent::guest_size() as usize;
            let name_bytes = name.as_encoded_bytes();
            let entry_len = dirent_size + name_bytes.len();
            if written + entry_len > buf_len as usize {
                break;
            }
            let dirent = types::Dirent {
                d_next: (index + 1) as u64,
                d_ino: identity,
                d_namlen: name_bytes
                    .len()
                    .try_into()
                    .map_err(|_| types::Error::trap(anyhow!("name too long")))?,
                d_type: filetype,
            };
            let head = buf.add(written as u32).map_err(|e| {
                types::Error::trap(anyhow!("dirent ptr: {e}"))
            })?;
            // Preview1 directory entries are packed byte records. A name
            // can leave the next header unaligned, so typed guest writes
            // would impose an invalid alignment restriction.
            let mut header = [0u8; 24];
            debug_assert_eq!(header.len(), dirent_size);
            header[0..8].copy_from_slice(&dirent.d_next.to_le_bytes());
            header[8..16].copy_from_slice(&dirent.d_ino.to_le_bytes());
            header[16..20].copy_from_slice(&dirent.d_namlen.to_le_bytes());
            header[20] = dirent.d_type.into();
            memory
                .copy_from_slice(&header, head.as_array(header.len() as u32))
                .map_err(|e| types::Error::trap(anyhow!("dirent write: {e}")))?;
            let name_region = buf
                .add(written as u32 + dirent_size as u32)
                .map(|p| p.as_array(name_bytes.len() as u32))
                .map_err(|e| types::Error::trap(anyhow!("name region: {e}")))?;
            memory
                .copy_from_slice(name_bytes, name_region)
                .map_err(|e| types::Error::trap(anyhow!("name copy: {e}")))?;
            written += entry_len;
            index += 1;
        }
        Ok(written as types::Size)
    }

    fn path_create_directory(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let path = read_path(memory, path)?;
        let (parent, name) = self.resolve_parent(dirfd, &path, false)?;
        self.create_directory_at(&parent, &name, false)
    }

    fn path_filestat_get(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        flags: types::Lookupflags,
        path: GuestPtr<str>,
    ) -> Result<types::Filestat, types::Error> {
        let path = read_path(memory, path)?;
        let follow = flags.contains(types::Lookupflags::SYMLINK_FOLLOW);
        let (parent, name) = self.resolve_parent(dirfd, &path, follow)?;
        let rel = parent.rel.join(&name);
        let binding = self.binding(parent.mount)?;
        let (overlay_dir, base_dir) = parent.layer_dirs();
        let tombstones = mount_tombstones(binding);
        if !follow {
            let meta = overlay_dir
                .and_then(|d| d.symlink_metadata(&name).ok())
                .or_else(|| {
                    if tombstones.map(|ts| ts.is_removed(&rel)).unwrap_or(false) {
                        None
                    } else {
                        base_dir.and_then(|d| d.symlink_metadata(&name).ok())
                    }
                });
            return meta
                .map(|m| Self::filestat_of(&m))
                .ok_or_else(|| types::Errno::Noent.into());
        }
        // resolve_parent already followed the final symlink when asked; a
        // remaining symlink here means the !follow branch above already
        // answered, so this is the followed target.
        let meta = self.effective_symlink_metadata(parent.mount, &rel)?;
        meta.map(|m| Self::filestat_of(&m))
            .ok_or_else(|| types::Errno::Noent.into())
    }

    fn path_filestat_set_times(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        flags: types::Lookupflags,
        path: GuestPtr<str>,
        atim: types::Timestamp,
        mtim: types::Timestamp,
        fst_flags: types::Fstflags,
    ) -> Result<(), types::Error> {
        if fst_flags.contains(types::Fstflags::ATIM_NOW) && fst_flags.contains(types::Fstflags::ATIM)
        {
            return Err(types::Errno::Inval.into());
        }
        if fst_flags.contains(types::Fstflags::MTIM_NOW) && fst_flags.contains(types::Fstflags::MTIM)
        {
            return Err(types::Errno::Inval.into());
        }
        let path = read_path(memory, path)?;
        let follow = flags.contains(types::Lookupflags::SYMLINK_FOLLOW);
        let (parent, name) = self.resolve_parent(dirfd, &path, follow)?;
        self.ensure_writable(parent.mount)?;
        // Times are applied through a writable open of the effective file
        // (copy-up for COW happens inside open_effective_file).
        let file = self.open_effective_file(
            &parent,
            &name,
            false,
            false,
            false,
            false,
            true,
            false,
        )?;
        let mut file = file;
        let now = SystemTime::now();
        let mut times = std::fs::FileTimes::new();
        if fst_flags.contains(types::Fstflags::ATIM) {
            times = times.set_accessed(UNIX_EPOCH + duration_nanos(atim));
        } else if fst_flags.contains(types::Fstflags::ATIM_NOW) {
            times = times.set_accessed(now);
        }
        if fst_flags.contains(types::Fstflags::MTIM) {
            times = times.set_modified(UNIX_EPOCH + duration_nanos(mtim));
        } else if fst_flags.contains(types::Fstflags::MTIM_NOW) {
            times = times.set_modified(now);
        }
        file.set_times(times)
            .map_err(|e| types::Error::trap(anyhow!("path_filestat_set_times: {e}")))?;
        Ok(())
    }

    fn path_link(
        &mut self,
        memory: &mut GuestMemory<'_>,
        src_fd: types::Fd,
        src_flags: types::Lookupflags,
        src_path: GuestPtr<str>,
        target_fd: types::Fd,
        target_path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let src_path = read_path(memory, src_path)?;
        let target_path = read_path(memory, target_path)?;
        let _ = src_flags; // hard links follow the source path in preview1
        let (src_parent, src_name) = self.resolve_parent(src_fd, &src_path, true)?;
        let (target_parent, target_name) = self.resolve_parent(target_fd, &target_path, false)?;
        if src_parent.mount != target_parent.mount {
            return Err(types::Errno::Xdev.into());
        }
        self.ensure_writable(target_parent.mount)?;
        let rel = target_parent.rel.join(&target_name);
        if self.is_cow(target_parent.mount)? {
            self.materialize_dirs(target_parent.mount, &rel)?;
            let src_rel = src_parent.rel.join(&src_name);
            let in_overlay = {
                let store = self.cow_store(target_parent.mount)?;
                store.overlay_dir().symlink_metadata(&src_rel).is_ok()
            };
            if !in_overlay {
                // Source lives only in the base layer: a writable open
                // materializes it into the overlay before linking.
                let _ = self.open_effective_file(
                    &src_parent,
                    &src_name,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )?;
            }
            {
                let store = self.cow_store(target_parent.mount)?;
                let overlay = store.overlay_dir();
                overlay
                    .hard_link(&src_rel, overlay, &rel)
                    .map_err(errno_from_io)?;
            }
            self.cow_store(target_parent.mount)?
                .persist_tombstones()
                .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            return Ok(());
        }
        let root = self
            .direct_layer(target_parent.mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        root.hard_link(src_parent.rel.join(&src_name), &root, &rel)
            .map_err(errno_from_io)?;
        Ok(())
    }

    fn path_open(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        dirflags: types::Lookupflags,
        path: GuestPtr<str>,
        oflags: types::Oflags,
        fs_rights_base: types::Rights,
        _fs_rights_inheriting: types::Rights,
        fdflags: types::Fdflags,
    ) -> Result<types::Fd, types::Error> {
        let path = read_path(memory, path)?;
        let open_request = format!("{dirfd:?}:{dirflags:?}:{path:?}:{oflags:?}:{fs_rights_base:?}:{_fs_rights_inheriting:?}:{fdflags:?}");
        if let Some(io) = self.replay_io.clone().filter(|io| io.is_replay()) {
            if fs_rights_base.contains(types::Rights::FD_WRITE)
                || oflags.contains(types::Oflags::CREAT) || oflags.contains(types::Oflags::TRUNC)
                || fdflags.contains(types::Fdflags::APPEND) {
                io.reject_unrecorded("path_open: writable descriptor")
                    .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
            }
            let bytes = io.input("path_open", open_request.as_bytes(), 64 * 1024,
                || ::anyhow::bail!("REPLAY_IO_INVALID: replay invoked fresh open"))
                .map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
            let (expected_fd, identity, directory): (u32, String, bool) = serde_json::from_slice(&bytes)
                .map_err(|error| types::Error::trap(anyhow!("REPLAY_IO_CORRUPT: {error}")))?;
            let fd = self.alloc_fd(FdEntry::Recorded { identity, cursor: 0, directory });
            if u32::from(fd) != expected_fd {
                return Err(types::Error::trap(anyhow!("REPLAY_IO_ORDER_MISMATCH: opened descriptor")));
            }
            return Ok(fd);
        }
        if fs_rights_base.contains(types::Rights::FD_WRITE)
            || oflags.contains(types::Oflags::CREAT)
            || oflags.contains(types::Oflags::TRUNC)
        {
            // The mount's declared mode is the authority on writability; a
            // readonly mount never upgrades because a right was requested.
            let mount = self.dir_handle(dirfd)?.mount;
            if !self.mount_writable(mount)? {
                return Err(types::Errno::Perm.into());
            }
        }
        let follow = dirflags.contains(types::Lookupflags::SYMLINK_FOLLOW)
            && !oflags.contains(types::Oflags::DIRECTORY);
        let (parent, name) = self.resolve_parent(dirfd, &path, follow)?;

        if oflags.contains(types::Oflags::DIRECTORY) {
            // Open the resolved directory as a new fd. CREAT creates the
            // directory when missing (Exist is not an error here).
            if oflags.contains(types::Oflags::CREAT) {
                let child_rel = parent.rel.join(&name);
                let exists = self
                    .effective_symlink_metadata(parent.mount, &child_rel)?
                    .is_some();
                if !exists {
                    self.create_directory_at(&parent, &name, false)?;
                }
            }
            let handle = self.open_dir_child(&parent, &name)?;
            let fd = self.alloc_fd(FdEntry::Dir(handle));
            self.record_open(open_request.as_bytes(), fd)?;
            return Ok(fd);
        }

        let create = oflags.contains(types::Oflags::CREAT);
        let create_new = oflags.contains(types::Oflags::EXCL);
        let truncate = oflags.contains(types::Oflags::TRUNC);
        let append = fdflags.contains(types::Fdflags::APPEND);
        let readable = true;
        let writable = fs_rights_base.contains(types::Rights::FD_WRITE) || create || truncate || append;

        let file = self.open_effective_file(
            &parent,
            &name,
            create,
            create_new,
            truncate,
            readable,
            writable,
            append,
        )?;
        let rel = parent.rel.join(&name);
        let fd = self.alloc_fd(FdEntry::File(FileHandle {
            mount: parent.mount,
            rel: Some(rel),
            file,
            cursor: 0,
            append,
            readable,
            writable,
        }));
        self.record_open(open_request.as_bytes(), fd)?;
        Ok(fd)
    }

    fn path_readlink(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        path: GuestPtr<str>,
        buf: GuestPtr<u8>,
        buf_len: types::Size,
    ) -> Result<types::Size, types::Error> {
        let path = read_path(memory, path)?;
        let (parent, name) = self.resolve_parent(dirfd, &path, false)?;
        let binding = self.binding(parent.mount)?;
        let (overlay_dir, base_dir) = parent.layer_dirs();
        let tombstones = mount_tombstones(binding);
        let rel = parent.rel.join(&name);
        let target = overlay_dir
            .and_then(|d| d.read_link(&name).ok())
            .or_else(|| {
                if tombstones.map(|ts| ts.is_removed(&rel)).unwrap_or(false) {
                    None
                } else {
                    base_dir.and_then(|d| d.read_link(&name).ok())
                }
            })
            .ok_or(types::Errno::Noent)?;
        let bytes = target.as_os_str().as_encoded_bytes();
        let take = (buf_len as usize).min(bytes.len());
        let region = buf.as_array(take as u32);
        memory
            .copy_from_slice(&bytes[..take], region)
            .map_err(|e| types::Error::trap(anyhow!("readlink copy: {e}")))?;
        Ok(take as types::Size)
    }

    fn path_remove_directory(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let path = read_path(memory, path)?;
        let (parent, name) = self.resolve_parent(dirfd, &path, false)?;
        self.ensure_writable(parent.mount)?;
        let rel = parent.rel.join(&name);
        let binding = self.binding(parent.mount)?;
        let (overlay_dir, base_dir) = parent.layer_dirs();
        let tombstones = mount_tombstones(binding);
        let in_overlay = overlay_dir
            .map(|d| d.symlink_metadata(&name).map(|m| m.is_dir()).unwrap_or(false))
            .unwrap_or(false);
        let in_base = !in_overlay
            && !tombstones.map(|ts| ts.is_removed(&rel)).unwrap_or(false)
            && base_dir
                .map(|d| d.symlink_metadata(&name).map(|m| m.is_dir()).unwrap_or(false))
                .unwrap_or(false);
        if !in_overlay && !in_base {
            return Err(types::Errno::Noent.into());
        }
        // The effective view must be empty.
        let child_handle = DirHandle {
            mount: parent.mount,
            rel: rel.clone(),
            overlay: if in_overlay {
                Self::open_layer_dir(overlay_dir.expect("overlay layer"), &name)?
            } else {
                None
            },
            base: if !in_overlay {
                Self::open_layer_dir(base_dir.expect("base layer"), &name)?
            } else {
                None
            },
        };
        let ts_ref = match &self.binding(parent.mount)?.backing {
            MountBacking::CopyOnWrite { store, .. } => Some(&store.tombstones),
            _ => None,
        };
        if !effective_children(&child_handle, ts_ref)?.is_empty() {
            return Err(types::Errno::Notempty.into());
        }
        if self.is_cow(parent.mount)? {
            {
                let store = self.cow_store(parent.mount)?;
                if in_overlay {
                    store.overlay_dir().remove_dir(&rel).map_err(errno_from_io)?;
                }
                if in_base {
                    store.tombstones.remove(&rel);
                }
            }
            self.cow_store(parent.mount)?
                .persist_tombstones()
                .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            return Ok(());
        }
        let root = self
            .direct_layer(parent.mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        root.remove_dir(&rel).map_err(errno_from_io)?;
        Ok(())
    }

    fn path_rename(
        &mut self,
        memory: &mut GuestMemory<'_>,
        src_fd: types::Fd,
        src_path: GuestPtr<str>,
        dest_fd: types::Fd,
        dest_path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let src_path = read_path(memory, src_path)?;
        let dest_path = read_path(memory, dest_path)?;
        let (src_parent, src_name) = self.resolve_parent(src_fd, &src_path, false)?;
        let (dest_parent, dest_name) = self.resolve_parent(dest_fd, &dest_path, false)?;
        if src_parent.mount != dest_parent.mount {
            return Err(types::Errno::Xdev.into());
        }
        self.ensure_writable(dest_parent.mount)?;
        let mount = dest_parent.mount;
        let src_rel = src_parent.rel.join(&src_name);
        let dest_rel = dest_parent.rel.join(&dest_name);
        if self.is_cow(mount)? {
            self.materialize_dirs(mount, &dest_rel)?;
            let src_meta = self
                .effective_symlink_metadata(mount, &src_rel)?
                .ok_or(types::Errno::Noent)?;
            let (src_in_overlay, src_in_base) = {
                let store = self.cow_store(mount)?;
                let in_overlay = store.overlay_dir().symlink_metadata(&src_rel).is_ok();
                let in_base = !in_overlay
                    && !store.tombstones.is_removed(&src_rel)
                    && src_parent
                        .base
                        .as_ref()
                        .map(|b| b.symlink_metadata(&src_name).is_ok())
                        .unwrap_or(false);
                (in_overlay, in_base)
            };
            let file_type = src_meta.file_type();
            if file_type.is_dir() {
                // Move the whole effective subtree into the overlay at the
                // destination, then hide the source.
                let base_hidden = {
                    let store = self.cow_store(mount)?;
                    store.tombstones.is_removed(&src_rel)
                };
                let src_handle = DirHandle {
                    mount,
                    rel: src_rel.clone(),
                    overlay: src_parent
                        .overlay
                        .as_ref()
                        .map(|d| Self::open_layer_dir(d, &src_name))
                        .transpose()?
                        .flatten(),
                    base: if base_hidden {
                        None
                    } else {
                        src_parent
                            .base
                            .as_ref()
                            .map(|d| Self::open_layer_dir(d, &src_name))
                            .transpose()?
                            .flatten()
                    },
                };
                self.move_tree_to_overlay(mount, &src_handle, &dest_rel, 0)?;
                // Drop the now-empty overlay source directory.
                let overlay_source_is_dir = {
                    let store = self.cow_store(mount)?;
                    store
                        .overlay_dir()
                        .symlink_metadata(&src_name)
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                };
                if overlay_source_is_dir {
                    let _ = self.cow_store(mount)?.overlay_dir().remove_dir(&src_rel);
                }
            } else if file_type.is_symlink() {
                if src_in_overlay {
                    let overlay = self.cow_store(mount)?.overlay_dir();
                    overlay
                        .rename(&src_rel, overlay, &dest_rel)
                        .map_err(errno_from_io)?;
                } else {
                    // Base symlink: recreate the link at the destination.
                    let target = src_parent
                        .base
                        .as_ref()
                        .and_then(|b| b.read_link(&src_name).ok())
                        .ok_or(types::Errno::Noent)?;
                    self.cow_store(mount)?
                        .overlay_dir()
                        .symlink(target.as_os_str(), &dest_rel)
                        .map_err(errno_from_io)?;
                }
            } else {
                // Regular file: materialize at the source path, then
                // rename within the overlay.
                if !src_in_overlay {
                    let _ = self.open_effective_file(
                        &src_parent,
                        &src_name,
                        false,
                        false,
                        false,
                        true,
                        true,
                        false,
                    )?;
                }
                let overlay = self.cow_store(mount)?.overlay_dir();
                overlay
                    .rename(&src_rel, overlay, &dest_rel)
                    .map_err(errno_from_io)?;
            }
            if src_in_base {
                self.cow_store(mount)?.tombstones.remove(&src_rel);
            }
            self.cow_store(mount)?
                .persist_tombstones()
                .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            return Ok(());
        }
        let root = self
            .direct_layer(mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        root.rename(&src_rel, &root, &dest_rel).map_err(errno_from_io)?;
        Ok(())
    }

    fn path_symlink(
        &mut self,
        memory: &mut GuestMemory<'_>,
        src_path: GuestPtr<str>,
        dirfd: types::Fd,
        dest_path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let src_path = read_path(memory, src_path)?;
        let dest_path = read_path(memory, dest_path)?;
        let (parent, name) = self.resolve_parent(dirfd, &dest_path, false)?;
        self.ensure_writable(parent.mount)?;
        let rel = parent.rel.join(&name);
        // Absolute symlink targets are stored verbatim but can never be
        // followed out of the mount (resolution rejects them); relative ones
        // stay confined by the per-component walk.
        if self.is_cow(parent.mount)? {
            self.materialize_dirs(parent.mount, &rel)?;
            {
                let store = self.cow_store(parent.mount)?;
                store
                    .overlay_dir()
                    .symlink(&src_path, &rel)
                    .map_err(errno_from_io)?;
                store.tombstones.clear_below(&rel);
            }
            self.cow_store(parent.mount)?
                .persist_tombstones()
                .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            return Ok(());
        }
        let root = self
            .direct_layer(parent.mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        root.symlink(&src_path, &rel).map_err(errno_from_io)?;
        Ok(())
    }

    fn path_unlink_file(
        &mut self,
        memory: &mut GuestMemory<'_>,
        dirfd: types::Fd,
        path: GuestPtr<str>,
    ) -> Result<(), types::Error> {
        let path = read_path(memory, path)?;
        let (parent, name) = self.resolve_parent(dirfd, &path, false)?;
        self.ensure_writable(parent.mount)?;
        let rel = parent.rel.join(&name);
        let binding = self.binding(parent.mount)?;
        let (overlay_dir, base_dir) = parent.layer_dirs();
        let tombstones = mount_tombstones(binding);
        let meta = overlay_dir
            .and_then(|d| d.symlink_metadata(&name).ok())
            .or_else(|| {
                if tombstones.map(|ts| ts.is_removed(&rel)).unwrap_or(false) {
                    None
                } else {
                    base_dir.and_then(|d| d.symlink_metadata(&name).ok())
                }
            })
            .ok_or(types::Errno::Noent)?;
        if meta.is_dir() {
            return Err(types::Errno::Isdir.into());
        }
        let base_present = base_dir
            .map(|d| d.symlink_metadata(&name).is_ok())
            .unwrap_or(false);
        if self.is_cow(parent.mount)? {
            let mut removed_base = false;
            {
                let store = self.cow_store(parent.mount)?;
                let in_overlay = store.overlay_dir().symlink_metadata(&rel).is_ok();
                if in_overlay {
                    store.overlay_dir().remove_file(&rel).map_err(errno_from_io)?;
                }
                if base_present && !in_overlay {
                    store.tombstones.remove(&rel);
                    removed_base = true;
                }
            }
            if removed_base {
                self.cow_store(parent.mount)?
                    .persist_tombstones()
                    .map_err(|e| types::Error::trap(anyhow!("persist tombstones: {e}")))?;
            }
            return Ok(());
        }
        let root = self
            .direct_layer(parent.mount)?
            .ok_or_else(|| types::Error::trap(anyhow!("direct layer missing")))?;
        root.remove_file(&rel).map_err(errno_from_io)?;
        Ok(())
    }

    fn poll_oneoff(
        &mut self,
        memory: &mut GuestMemory<'_>,
        subs: GuestPtr<types::Subscription>,
        events: GuestPtr<types::Event>,
        nsubscriptions: types::Size,
    ) -> Result<types::Size, types::Error> {
        self.replay_guard("poll_oneoff")?;
        if nsubscriptions == 0 {
            return Err(types::Errno::Inval.into());
        }
        if nsubscriptions > 1024 {
            return Err(types::Errno::Inval.into());
        }
        let mut delivered = 0u32;
        for index in 0..nsubscriptions {
            let sub = memory
                .read::<types::Subscription>(subs.add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("subscription: {e}")))?;
            let (error, type_, nbytes) = match sub.u {
                types::SubscriptionU::Clock(clock) => {
                    let now = self.clock_time_get(
                        memory,
                        clock.id,
                        0,
                    )?;
                    let deadline = if clock
                        .flags
                        .contains(types::Subclockflags::SUBSCRIPTION_CLOCK_ABSTIME)
                    {
                        clock.timeout
                    } else {
                        now.saturating_add(clock.timeout)
                    };
                    if deadline > now {
                        std::thread::sleep(std::time::Duration::from_nanos(deadline - now));
                    }
                    (types::Errno::Success, types::Eventtype::Clock, 0u64)
                }
                types::SubscriptionU::FdRead(fd_read) => {
                    let fd = fd_read.file_descriptor;
                    if self.is_socket(fd)? {
                        // A socket subscription waits for readability (data or
                        // EOF) in bounded slices, so a cancellation or the
                        // deadline ends the poll instead of parking the run.
                        self.socket_readiness(fd, false)?;
                    } else if !matches!(self.fd(fd)?, FdEntry::Stdin(_) | FdEntry::File(_)) {
                        return Err(types::Errno::Badf.into());
                    }
                    // Stdin adapters and regular files report ready; a
                    // blocking read then delivers data or 0 (EOF).
                    (types::Errno::Success, types::Eventtype::FdRead, 1u64)
                }
                types::SubscriptionU::FdWrite(fd_write) => {
                    let fd = fd_write.file_descriptor;
                    if self.is_socket(fd)? {
                        self.socket_readiness(fd, true)?;
                    } else if !matches!(
                        self.fd(fd)?,
                        FdEntry::Stdout(_) | FdEntry::Stderr(_) | FdEntry::File(_)
                    ) {
                        return Err(types::Errno::Badf.into());
                    }
                    (types::Errno::Success, types::Eventtype::FdWrite, 1u64)
                }
            };
            let event = types::Event {
                userdata: sub.userdata,
                error,
                type_,
                fd_readwrite: types::EventFdReadwrite {
                    nbytes,
                    flags: types::Eventrwflags::empty(),
                },
            };
            memory
                .write(events.add(index)?, event)
                .map_err(|e| types::Error::trap(anyhow!("event write: {e}")))?;
            delivered += 1;
        }
        Ok(delivered)
    }

    fn proc_exit(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        status: types::Exitcode,
    ) -> wasmtime::Error {
        // Preview1 reserves the top of the status range for the host.
        if status >= 126 {
            return wasmtime::Error::msg("exit with invalid exit status outside of [0..126)");
        }
        wasmtime::Error::from(wasmtime_wasi::I32Exit(status as i32))
    }

    fn proc_raise(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        _sig: types::Signal,
    ) -> Result<(), types::Error> {
        Err(types::Errno::Nosys.into())
    }

    fn sched_yield(&mut self, _memory: &mut GuestMemory<'_>) -> Result<(), types::Error> {
        std::thread::yield_now();
        Ok(())
    }

    fn random_get(
        &mut self,
        memory: &mut GuestMemory<'_>,
        buf: GuestPtr<u8>,
        buf_len: types::Size,
    ) -> Result<(), types::Error> {
        if buf_len as usize > MAX_IOV_BYTES { return Err(types::Errno::Inval.into()); }
        let supply = || -> ::anyhow::Result<Vec<u8>> {
            let mut bytes = vec![0u8; buf_len as usize];
            getrandom::getrandom(&mut bytes).map_err(|error| ::anyhow::anyhow!("random_get: {error}"))?;
            Ok(bytes)
        };
        let bytes = if let Some(io) = &self.replay_io {
            io.input("random_get", &buf_len.to_le_bytes(), buf_len as u64, supply)
        } else { supply() }.map_err(|error| types::Error::trap(anyhow!("{error:#}")))?;
        let region = buf
            .as_array(buf_len);
        memory
            .copy_from_slice(&bytes, region)
            .map_err(|e| types::Error::trap(anyhow!("random copy: {e}")))?;
        Ok(())
    }


    fn sock_accept(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        flags: types::Fdflags,
    ) -> Result<types::Fd, types::Error> {
        if flags.intersects(
            types::Fdflags::APPEND
                | types::Fdflags::SYNC
                | types::Fdflags::RSYNC
                | types::Fdflags::DSYNC,
        ) {
            return Err(types::Errno::Inval.into());
        }
        let nonblocking = flags.contains(types::Fdflags::NONBLOCK);
        let stop = self.socket_stop()?;
        let max_sockets = match &self.net {
            Some(net) => net.admission.max_sockets(),
            None => return Err(types::Errno::Notsock.into()),
        };
        if self.open_socket_count() >= max_sockets {
            return Err(types::Errno::Mfile.into());
        }
        let accepted = match self.fd(fd)? {
            FdEntry::Socket(SocketHandle {
                socket: AdmittedSocket::Listener(listener),
                nonblocking: listener_nonblocking,
            }) => crate::net::listener_accept(listener, *listener_nonblocking, &stop),
            // A connected stream never accepts; only a granted listener does.
            FdEntry::Socket(SocketHandle { socket: AdmittedSocket::Stream(_), .. }) => {
                return Err(types::Errno::Inval.into());
            }
            _ => return Err(types::Errno::Notsock.into()),
        };
        let stream = match accepted {
            Ok(stream) => stream,
            Err(error) => return net_err(error),
        };
        Ok(self.alloc_fd(FdEntry::Socket(SocketHandle {
            socket: AdmittedSocket::Stream(stream),
            nonblocking,
        })))
    }

    fn sock_recv(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ri_data: types::IovecArray,
        ri_flags: types::Riflags,
    ) -> Result<(types::Size, types::Roflags), types::Error> {
        // wiggle already rejected any flag bit outside the two defined ones.
        let peek = ri_flags.contains(types::Riflags::RECV_PEEK);
        let waitall = ri_flags.contains(types::Riflags::RECV_WAITALL);
        let total = self.socket_scatter_recv(memory, fd, ri_data, peek, waitall)?;
        Ok((total, types::Roflags::empty()))
    }

    fn sock_send(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        si_data: types::CiovecArray,
        si_flags: types::Siflags,
    ) -> Result<types::Size, types::Error> {
        // `siflags` defines no flag: anything but zero is invalid input, not a
        // silently ignored hint.
        if si_flags != 0 {
            return Err(types::Errno::Inval.into());
        }
        self.socket_send_bytes(memory, fd, si_data)
    }

    fn sock_shutdown(
        &mut self,
        _memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        how: types::Sdflags,
    ) -> Result<(), types::Error> {
        let mode = match (how.contains(types::Sdflags::RD), how.contains(types::Sdflags::WR)) {
            (true, true) => Shutdown::Both,
            (true, false) => Shutdown::Read,
            (false, true) => Shutdown::Write,
            (false, false) => return Err(types::Errno::Inval.into()),
        };
        let (handle, _stop) = self.socket_entry(fd)?;
        let stream = handle.stream()?;
        match crate::net::shutdown_stream(stream, mode) {
            Ok(()) => Ok(()),
            Err(error) => Err(socket_error(error)),
        }
    }
}

fn duration_nanos(nanos: u64) -> std::time::Duration {
    std::time::Duration::from_nanos(nanos)
}

/// Preview1 socket surface the generated `WasiSnapshotPreview1` trait does not
/// declare: `fd_read`/`fd_write` over an installed socket descriptor and
/// `poll_oneoff` readiness. A guest reaches these through the standard calls,
/// never by name.
impl P1Host {
    /// `fd_read` on an installed socket: one guest `read(2)`, scattered across
    /// the descriptor's iovecs in order.
    fn socket_fd_read(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        iovs: types::IovecArray,
    ) -> Result<types::Size, types::Error> {
        let count = iovs.len();
        let mut total = 0usize;
        for index in 0..count {
            let iov = memory
                .read::<types::Iovec>(iovs.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            if iov.buf_len == 0 {
                continue;
            }
            let requested = iov.buf_len as usize;
            if requested > MAX_IOV_BYTES {
                return Err(types::Errno::Inval.into());
            }
            let mut buf = vec![0u8; requested];
            let read = self.socket_recv_into(fd, &mut buf, false, false)?;
            memory
                .copy_from_slice(&buf[..read], iov.buf.as_array(read as types::Size))
                .map_err(|e| types::Error::trap(anyhow!("iov copy: {e}")))?;
            total += read;
            if read < requested {
                break;
            }
        }
        Ok(total as types::Size)
    }

    /// `fd_write` on an installed socket: one guest `write(2)`.
    fn socket_fd_write(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ciovs: types::CiovecArray,
    ) -> Result<types::Size, types::Error> {
        self.socket_send_bytes(memory, fd, ciovs)
    }

    /// Gather the guest civecs and send them on `fd`'s socket.
    fn socket_send_bytes(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ciovs: types::CiovecArray,
    ) -> Result<types::Size, types::Error> {
        let data = Self::write_ciovs(memory, ciovs)?;
        if data.is_empty() {
            return Ok(0);
        }
        let (handle, stop) = self.socket_entry(fd)?;
        let stream = handle.stream()?;
        let sent = match crate::net::stream_write(stream, &data, handle.nonblocking, &stop) {
            Ok(sent) => sent,
            Err(error) => return net_err(error),
        };
        Ok(sent as types::Size)
    }

    /// Receive into `buf`, honouring the guest's peek/waitall flags.
    fn socket_recv_into(
        &mut self,
        fd: types::Fd,
        buf: &mut [u8],
        peek: bool,
        waitall: bool,
    ) -> Result<usize, types::Error> {
        let (handle, stop) = self.socket_entry(fd)?;
        let stream = handle.stream()?;
        match crate::net::stream_read(stream, buf, handle.nonblocking, peek, waitall, &stop) {
            Ok(read) => Ok(read),
            Err(error) => net_err(error),
        }
    }

    /// `sock_recv` over a scatter list. Only byte streams are installed, so no
    /// datagram can be truncated and the truncation flag stays clear.
    fn socket_scatter_recv(
        &mut self,
        memory: &mut GuestMemory<'_>,
        fd: types::Fd,
        ri_data: types::IovecArray,
        peek: bool,
        waitall: bool,
    ) -> Result<types::Size, types::Error> {
        let count = ri_data.len();
        let mut total = 0usize;
        for index in 0..count {
            let iov = memory
                .read::<types::Iovec>(ri_data.as_ptr().add(index)?)
                .map_err(|e| types::Error::trap(anyhow!("iovec: {e}")))?;
            if iov.buf_len == 0 {
                continue;
            }
            let requested = iov.buf_len as usize;
            if requested > MAX_IOV_BYTES {
                return Err(types::Errno::Inval.into());
            }
            let mut buf = vec![0u8; requested];
            // WAITALL completes the whole scatter, so only the final iovec
            // asks the transport to fill to the end; earlier ones are
            // satisfied by whatever the stream delivers.
            let wait = waitall && index + 1 == count;
            let read = self.socket_recv_into(fd, &mut buf, peek, wait)?;
            memory
                .copy_from_slice(&buf[..read], iov.buf.as_array(read as types::Size))
                .map_err(|e| types::Error::trap(anyhow!("iov copy: {e}")))?;
            total += read;
            if peek || read < requested {
                break;
            }
        }
        Ok(total as types::Size)
    }

    /// Wait for socket readiness (`poll_oneoff`), bounded by the run stop so an
    /// uninterruptible poll cannot park the run.
    fn socket_readiness(&self, fd: types::Fd, write: bool) -> Result<(), types::Error> {
        let (handle, stop) = self.socket_entry(fd)?;
        let stream = handle.stream()?;
        match crate::net::wait_stream_ready(stream, write, &stop) {
            Ok(()) => Ok(()),
            Err(error) => Err(socket_error(error)),
        }
    }
}
