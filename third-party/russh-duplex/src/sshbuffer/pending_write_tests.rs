#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
//! Fixed synthetic writer controls: no socket, credential, sleep or real peer.
//! Clear packet framing is deterministic here; real encryption is covered by
//! the separately selected, existing protocol tests, not by these unit tests.
use super::*;
use std::{io, pin::Pin, task::{Context, Poll}};

struct Sink {
    bytes: Vec<u8>,
    accept_until: usize,
    flush_ready: bool,
    write_zero: bool,
    write_error: bool,
    flush_error: bool,
    writes: usize,
}
impl Sink {
    fn open() -> Self {
        Self { bytes: vec![], accept_until: usize::MAX, flush_ready: true,
            write_zero: false, write_error: false, flush_error: false, writes: 0 }
    }
}
impl AsyncWrite for Sink {
    fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8])
        -> Poll<io::Result<usize>>
    {
        self.writes += 1;
        if self.write_error { return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())); }
        if self.write_zero { return Poll::Ready(Ok(0)); }
        let n = buf.len().min(self.accept_until.saturating_sub(self.bytes.len()));
        if n == 0 { return Poll::Pending; }
        self.bytes.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.flush_error { return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())); }
        if self.flush_ready { Poll::Ready(Ok(())) } else { Poll::Pending }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn assert_terminal(writer: &mut PacketWriter, sink: &mut Sink) {
    assert!(writer.has_pending_write(), "terminal failure must remain visible to the protocol driver");
    sink.write_zero = false;
    sink.write_error = false;
    sink.flush_error = false;
    sink.flush_ready = true;
    sink.accept_until = usize::MAX;
    let output = sink.bytes.clone();
    let sequence = writer.write_buffer.seqn;
    assert!(writer.packet_raw(b"retry").is_err());
    assert!(writer.write_packet(|b| { b.extend_from_slice(b"retry"); Ok(()) }).is_err());
    assert!(writer.packet(|b| { b.extend_from_slice(b"retry"); Ok(()) }).is_err());
    assert!(writer.packet_bytes(|b| { b.extend_from_slice(b"retry"); Ok(()) }).is_err());
    assert!(writer.flush_into(sink).await.is_err());
    assert_eq!(writer.write_buffer.seqn, sequence);
    assert_eq!(sink.bytes, output);
}

#[tokio::test]
async fn canceled_partial_write_then_control_append_is_exactly_once() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(b"first packet").unwrap();
    let mut expected = PacketWriter::clear();
    expected.packet_raw(b"first packet").unwrap();
    expected.packet_raw(b"control reply").unwrap();
    let mut sink = Sink::open();
    sink.accept_until = 3;
    // One poll accepts a prefix then blocks; dropping this future is exactly
    // what selecting a ready inbound event does to the losing flush future.
    {
        let mut flush = std::pin::pin!(writer.flush_into(&mut sink));
        assert!(futures::poll!(&mut flush).is_pending());
    }
    assert_eq!(writer.write_cursor, 3);
    writer.packet_raw(b"control reply").unwrap();
    assert_eq!(writer.write_buffer.bytes, b"first packet".len() + b"control reply".len());
    assert_eq!(writer.write_buffer.seqn, Wrapping(2));
    sink.accept_until = usize::MAX;
    writer.flush_into(&mut sink).await.unwrap();
    assert_eq!(sink.bytes, expected.write_buffer.buffer);
    assert!(!writer.has_pending_write());
}

#[tokio::test]
async fn canceled_pending_flush_does_not_replay_accepted_bytes() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(b"flush pending").unwrap();
    let expected = writer.write_buffer.buffer.clone();
    let mut sink = Sink::open();
    sink.flush_ready = false;
    {
        let mut flush = std::pin::pin!(writer.flush_into(&mut sink));
        assert!(futures::poll!(&mut flush).is_pending());
    }
    assert!(writer.has_pending_write());
    assert_eq!(sink.bytes, expected);
    let writes = sink.writes;
    sink.flush_ready = true;
    writer.flush_into(&mut sink).await.unwrap();
    assert_eq!(sink.bytes, expected);
    assert_eq!(sink.writes, writes);
    assert!(!writer.has_pending_write());
}

#[tokio::test]
async fn write_zero_permanently_ends_packet_writer() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(b"write zero").unwrap();
    let mut sink = Sink::open();
    sink.write_zero = true;
    assert!(matches!(writer.flush_into(&mut sink).await,
        Err(Error::IO(e)) if e.kind() == io::ErrorKind::WriteZero));
    assert_terminal(&mut writer, &mut sink).await;
}

#[tokio::test]
async fn write_error_permanently_ends_packet_writer() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(b"write error").unwrap();
    let mut sink = Sink::open();
    sink.write_error = true;
    assert!(matches!(writer.flush_into(&mut sink).await,
        Err(Error::IO(e)) if e.kind() == io::ErrorKind::BrokenPipe));
    assert_terminal(&mut writer, &mut sink).await;
}

#[tokio::test]
async fn flush_error_permanently_ends_packet_writer() {
    let mut writer = PacketWriter::clear();
    writer.packet_raw(b"flush error").unwrap();
    let mut sink = Sink::open();
    sink.flush_error = true;
    assert!(matches!(writer.flush_into(&mut sink).await,
        Err(Error::IO(e)) if e.kind() == io::ErrorKind::BrokenPipe));
    assert!(!sink.bytes.is_empty());
    assert_terminal(&mut writer, &mut sink).await;
}

#[tokio::test]
async fn exact_encoded_limit_accepts_then_overflow_is_terminal() {
    let mut writer = PacketWriter::clear();
    // Clear framing has a 5-byte prefix + 4-byte padding at this length.
    // This unit intentionally tests the local queue cap, not peer packet limits.
    let payload = vec![0x5a; PacketWriter::MAX_PENDING_WRITE_BYTES - 9];
    writer.packet_raw(&payload).unwrap();
    assert_eq!(writer.write_buffer.buffer.len(), PacketWriter::MAX_PENDING_WRITE_BYTES);
    let sequence = writer.write_buffer.seqn;
    assert!(matches!(writer.packet_raw(b"overflow control"), Err(Error::Pending)));
    assert_eq!(writer.write_buffer.seqn, sequence);
    let mut sink = Sink::open();
    assert_terminal(&mut writer, &mut sink).await;
    assert!(sink.bytes.is_empty());
    // The same admission limit with no prior bytes must also wake the driver's
    // terminal flush arm, even if a Handler caught the admission error.
    let mut empty = PacketWriter::clear();
    let too_large = vec![0x5a; PacketWriter::MAX_PENDING_WRITE_BYTES + 1];
    assert!(matches!(empty.packet_raw(&too_large), Err(Error::Pending)));
    assert!(empty.write_buffer.buffer.is_empty());
    assert_terminal(&mut empty, &mut Sink::open()).await;
}
