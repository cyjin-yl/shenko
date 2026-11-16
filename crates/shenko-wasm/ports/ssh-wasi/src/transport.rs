//! The SSH protocol owns this stream. The host supplies only an already
//! admitted TCP descriptor; no host SSH process or signing RPC is used.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    fn fd_fdstat_set_flags(fd: u32, flags: u16) -> u16;
    fn sock_shutdown(fd: u32, how: u8) -> u16;
}

pub struct Transport {
    file: File,
    fd: u32,
    read_wait: Option<Pin<Box<Sleep>>>,
    write_wait: Option<Pin<Box<Sleep>>>,
}

impl Transport {
    pub fn admitted(host: &str, port: u16) -> anyhow::Result<Self> {
        for index in 0..8 {
            let Ok(value) = std::env::var(format!("SHENKO_NET_{index}")) else {
                continue;
            };
            let fields: Vec<_> = value.split(',').collect();
            if fields.len() != 4
                || fields[1] != host
                || fields[2].parse::<u16>()? != port
                || !fields[3].split('+').any(|op| op == "connect")
            {
                continue;
            }
            let fd = fields[0].parse::<u32>()?;
            anyhow::ensure!(
                fd > 2 && fd <= i32::MAX as u32,
                "invalid admitted descriptor"
            );
            // SAFETY: the runtime owns this namespace and transfers the
            // descriptor to the guest. The guest takes ownership exactly once.
            let errno = unsafe { fd_fdstat_set_flags(fd, 4) };
            anyhow::ensure!(
                errno == 0,
                "cannot make admitted socket nonblocking: {errno}"
            );
            return Ok(Self {
                file: unsafe { File::from_raw_fd(fd as i32) },
                fd,
                read_wait: None,
                write_wait: None,
            });
        }
        anyhow::bail!("no host-admitted TCP stream matches {host}:{port}")
    }
}

// P1 transport exposes nonblocking I/O. A short real timer yields to russh's
// writer/control tasks rather than blocking the one guest executor on recv.
// The host's invocation deadline and revocation remain the outer bound.
fn ready(wait: &mut Option<Pin<Box<Sleep>>>, cx: &mut Context<'_>) -> bool {
    use std::future::Future;
    if let Some(sleep) = wait {
        if sleep.as_mut().poll(cx).is_pending() {
            return false;
        }
    }
    *wait = None;
    true
}

fn retry(wait: &mut Option<Pin<Box<Sleep>>>, cx: &mut Context<'_>) {
    *wait = Some(Box::pin(tokio::time::sleep(Duration::from_millis(2))));
    let _ = ready(wait, cx);
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !ready(&mut this.read_wait, cx) {
            return Poll::Pending;
        }
        match this.file.read(buf.initialize_unfilled()) {
            Ok(count) => {
                buf.advance(count);
                Poll::Ready(Ok(()))
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                retry(&mut this.read_wait, cx);
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !ready(&mut this.write_wait, cx) {
            return Poll::Pending;
        }
        match this.file.write(bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                retry(&mut this.write_wait, cx);
                Poll::Pending
            }
            result => Poll::Ready(result),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        // SAFETY: this is the same live descriptor owned by self.file.
        let errno = unsafe { sock_shutdown(self.fd, 2) };
        Poll::Ready(if errno == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!("socket shutdown errno {errno}")))
        })
    }
}
