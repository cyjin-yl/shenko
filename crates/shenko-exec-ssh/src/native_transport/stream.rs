//! Owned stream cancellation, including a connection future dropped during KEX.
//! Closing this guard stops future I/O through this stream; it does NOT attest
//! that a remote process stopped, or that a background session was joined.
use std::{io, pin::Pin, sync::{Arc, Mutex}, task::{Context, Poll, Waker}};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct State { closed: bool, reader: Option<Waker>, writer: Option<Waker> }
pub(super) struct CloseGuard(Arc<Mutex<State>>);
pub(super) struct OwnedStream<R> { inner: Option<R>, state: Arc<Mutex<State>> }

pub(super) fn owned<R>(inner: R) -> (OwnedStream<R>, CloseGuard) {
    let state = Arc::new(Mutex::new(State::default()));
    (OwnedStream { inner: Some(inner), state: state.clone() }, CloseGuard(state))
}
impl CloseGuard {
    pub(super) fn close(&self) {
        let (read, write) = {
            let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
            s.closed = true;
            (s.reader.take(), s.writer.take())
        };
        if let Some(w) = read { w.wake(); }
        if let Some(w) = write { w.wake(); }
    }
}
impl Drop for CloseGuard { fn drop(&mut self) { self.close(); } }
impl<R> OwnedStream<R> {
    fn poll_io<T>(&mut self, cx: &mut Context<'_>, read: bool,
        poll: impl FnOnce(&mut R, &mut Context<'_>) -> Poll<io::Result<T>>) -> Poll<io::Result<T>>
    {
        let state = self.state.clone();
        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.closed {
            drop(s);
            self.inner.take();
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionAborted, "native SSH stream closed")));
        }
        if read { s.reader = Some(cx.waker().clone()); } else { s.writer = Some(cx.waker().clone()); }
        // Serialize a single nonblocking poll with close(). No await while
        // holding this lock: once close returns, no later inner I/O is polled.
        match self.inner.as_mut() {
            Some(inner) => poll(inner, cx),
            None => Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into())),
        }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for OwnedStream<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, true, |r, cx| Pin::new(r).poll_read(cx, buf))
    }
}
impl<R: AsyncWrite + Unpin> AsyncWrite for OwnedStream<R> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.poll_io(cx, false, |r, cx| Pin::new(r).poll_write(cx, buf))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, false, |r, cx| Pin::new(r).poll_flush(cx))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, false, |r, cx| Pin::new(r).poll_shutdown(cx))
    }
}
