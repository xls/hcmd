//! The three handles an SFTP transfer hands out, and nothing else.
//!
//!
//! All three are the **blocking** halves of a bridge whose other half is a
//! task inside the connection actor. They know nothing about SSH: a reader is
//! a channel of chunks, a writer is a channel of chunks and a channel of
//! acknowledgements, and a seeking reader is a request/response pair. That is
//! deliberate, and it is what lets every one of them be tested here against a
//! hand-driven thread with no server anywhere.
//!
//! # Which channel, and why
//!
//! the design fixes the primitives and the reasoning:
//! the blocking side never touches a tokio receiver, because
//! `blocking_recv` panics when the thread it is on turns out to be a runtime
//! worker, and the house style has no panic paths. So every message that
//! travels **towards** the blocking side arrives on a `std::sync::mpsc`
//! channel, whose `recv` blocks any thread and panics on none, and every
//! message that travels **towards** the actor leaves on a tokio unbounded
//! sender, whose `send` is not `async` and cannot block.
//!
//! Unbounded is only safe with the backpressure written back in, which is
//! where the read-ahead bound of the design actually lives:
//!
//! * reading, a [`tokio::sync::Semaphore`] carries it. The actor takes a
//!   permit before it issues a request and the reader returns one as it
//!   consumes a chunk, so at most `depth` chunks are ever in flight or queued.
//!   `Semaphore::add_permits` and `Semaphore::close` are both callable from a
//!   plain thread, which is what makes this work across the bridge.
//! * writing, the acknowledgement channel carries it. The writer blocks on an
//!   acknowledgement once `depth` chunks are unacknowledged, which is
//!   invariant I11 stated as code rather than as a hope.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::Error;

/// One chunk on its way to a reader, or the failure that ended the stream.
pub(crate) type Chunk = Result<Vec<u8>, Error>;

/// Turn a transport failure into the `io::Error` a `Read` or a `Write` has to
/// return, without losing which failure it was.
///
/// [`io::ErrorKind::ConnectionAborted`] and the boxed [`Error`] are both
/// deliberate: `ops::copy` wraps whatever comes back in `Error::Bare`, and a
/// dropped connection has to stay recognisable through that wrapping for
/// `ops::is_fatal` to stop the batch. The
/// original is recoverable with `err.get_ref().and_then(|e|
/// e.downcast_ref::<crate::error::Error>())`, and the kind alone is enough
/// for a caller that does not want to downcast.
pub(crate) fn as_io(err: Error) -> io::Error {
    match err {
        Error::ConnectionLost(_) => io::Error::new(io::ErrorKind::ConnectionAborted, err),
        other => io::Error::other(other),
    }
}

/// A forward-only reader fed by a pipeline of `SSH_FXP_READ` requests.
///
///
/// "pipeline reads rather than doing one round trip per block -
/// this is the difference between 2 MB/s and saturating the link". The
/// pipelining is in the actor; what is here is the consumer end and the
/// cancellation. Dropping this closes the semaphore, the actor's next
/// `acquire` fails, and the read-ahead stops - the same idiom `read_dir` uses
/// when a panel drops a listing it no longer wants.
///
/// **Blocking.** Call from the blocking pool only.
pub(crate) struct PipelinedReader {
    /// Chunks in file order. Closed by the actor when the file ends.
    chunks: Receiver<Chunk>,
    /// One permit per chunk the actor may have outstanding.
    permits: Arc<Semaphore>,
    /// The chunk being handed out, and how much of it is gone.
    buf: Vec<u8>,
    at: usize,
    /// Set once the stream has ended, so a second `read` does not wait on a
    /// channel nobody is going to write to.
    ended: bool,
}

impl PipelinedReader {
    /// Wrap the consumer end of a pipeline.
    pub(crate) fn new(chunks: Receiver<Chunk>, permits: Arc<Semaphore>) -> Self {
        Self {
            chunks,
            permits,
            buf: Vec::new(),
            at: 0,
            ended: false,
        }
    }
}

impl Read for PipelinedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(rest) = self.buf.get(self.at..)
                && !rest.is_empty()
            {
                let n = rest.len().min(out.len());
                if let (Some(dst), Some(src)) = (out.get_mut(..n), rest.get(..n)) {
                    dst.copy_from_slice(src);
                }
                self.at = self.at.saturating_add(n);
                return Ok(n);
            }
            if self.ended {
                return Ok(0);
            }
            // The chunk is spent. Returning its permit here and not on arrival
            // is what makes the bound a bound on *unconsumed* data.
            match self.chunks.recv() {
                // The actor finished the file and dropped its sender.
                Err(_) => {
                    self.ended = true;
                    return Ok(0);
                }
                Ok(Ok(chunk)) => {
                    if chunk.is_empty() {
                        self.ended = true;
                        return Ok(0);
                    }
                    self.buf = chunk;
                    self.at = 0;
                    self.permits.add_permits(1);
                }
                Ok(Err(err)) => {
                    self.ended = true;
                    return Err(as_io(err));
                }
            }
        }
    }
}

impl Drop for PipelinedReader {
    /// Stop the read-ahead.
    ///
    /// Closing the semaphore rather than dropping a channel, because the actor
    /// may be parked in `acquire` with nothing to send and no way to notice a
    /// dropped receiver; `close` wakes it with an error and it stops there.
    fn drop(&mut self) {
        self.permits.close();
    }
}

/// What a writer sends to its half of the actor.
pub(crate) enum WriteMsg {
    /// One chunk, to be written at the next offset.
    Chunk(Vec<u8>),
    /// Drain every outstanding write, close the handle, and answer here.
    Flush(SyncSender<Result<(), Error>>),
}

/// A writer whose `flush` is the commit.
///
/// `write` returns once the chunk has been accepted into a queue bounded at
/// `depth`, so the byte count `ops::copy` keeps can lead the wire by at most
/// `depth` chunks and by exactly nothing once `flush` has returned. That is
/// invariant I11, and the bound is enforced by this type refusing to hand out
/// a `depth + 1`th unacknowledged chunk rather than by anybody remembering to
/// check.
///
/// **Blocking.** Call from the blocking pool only.
pub(crate) struct PipelinedWriter {
    /// Chunks and the flush marker, towards the actor.
    tx: UnboundedSender<WriteMsg>,
    /// One acknowledgement per chunk the server has answered for.
    acks: Receiver<Result<(), Error>>,
    /// Chunks sent and not yet acknowledged.
    outstanding: usize,
    /// How many of those are allowed at once.
    depth: usize,
    /// True once `flush` has closed the handle. A second `flush` is a no-op
    /// and a `write` after it is an error, because the file is committed and
    /// reopening it silently would write to a different handle at a
    /// different offset.
    committed: bool,
    /// The remote path, for the error text.
    path: String,
    /// The connection, for [`Error::ConnectionLost`].
    authority: String,
}

impl PipelinedWriter {
    /// Wrap the producer end of a write pipeline.
    pub(crate) fn new(
        tx: UnboundedSender<WriteMsg>,
        acks: Receiver<Result<(), Error>>,
        depth: usize,
        path: String,
        authority: String,
    ) -> Self {
        Self {
            tx,
            acks,
            outstanding: 0,
            depth: depth.max(1),
            committed: false,
            path,
            authority,
        }
    }

    /// Wait for one acknowledgement, and report what it said.
    fn take_ack(&mut self) -> io::Result<()> {
        match self.acks.recv() {
            Ok(Ok(())) => {
                self.outstanding = self.outstanding.saturating_sub(1);
                Ok(())
            }
            Ok(Err(err)) => {
                self.outstanding = self.outstanding.saturating_sub(1);
                Err(as_io(err))
            }
            // The actor is gone, which is the connection being gone.
            Err(_) => {
                self.outstanding = 0;
                Err(as_io(Error::ConnectionLost(self.authority.clone())))
            }
        }
    }
}

impl Write for PipelinedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.committed {
            return Err(io::Error::other(format!(
                "{}: the file was already committed by flush",
                self.path
            )));
        }
        if data.is_empty() {
            return Ok(0);
        }
        // Collect anything the server has already answered for, so a fast link
        // never blocks here at all.
        while let Ok(ack) = self.acks.try_recv() {
            self.outstanding = self.outstanding.saturating_sub(1);
            if let Err(err) = ack {
                return Err(as_io(err));
            }
        }
        while self.outstanding >= self.depth {
            self.take_ack()?;
        }
        if self.tx.send(WriteMsg::Chunk(data.to_vec())).is_err() {
            return Err(as_io(Error::ConnectionLost(self.authority.clone())));
        }
        self.outstanding = self.outstanding.saturating_add(1);
        Ok(data.len())
    }

    /// The commit.
    ///
    /// Returns only once every chunk has been acknowledged and the handle has
    /// been closed, so a failure that the server would otherwise report at
    /// close time is reported here, where `ops::copy` is checking.
    fn flush(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        self.committed = true;
        // Depth 1: this thread parks on `recv` until the one answer arrives,
        // so a deeper channel would buy nothing.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(WriteMsg::Flush(tx)).is_err() {
            return Err(as_io(Error::ConnectionLost(self.authority.clone())));
        }
        match rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(as_io(err)),
            Err(_) => Err(as_io(Error::ConnectionLost(self.authority.clone()))),
        }
    }
}

/// One window of a file, asked for and answered.
pub(crate) struct WindowRequest {
    /// Where the window starts.
    pub(crate) offset: u64,
    /// How many bytes are wanted. The answer may be shorter.
    pub(crate) len: usize,
    /// Where the bytes go back.
    pub(crate) reply: SyncSender<Result<Vec<u8>, Error>>,
}

/// A seeking reader, for the viewer.
///
/// One round trip per window, which is why `Capabilities::SFTP` has
/// `seekable: true` and `random_access: false`: a window can be fetched
/// without reading what is before it, and that is not the same as it being
/// cheap.
///
/// **Blocking.** Call from the blocking pool only.
pub(crate) struct WindowReader {
    /// Requests, towards the actor.
    tx: UnboundedSender<WindowRequest>,
    /// The file's size, so `SeekFrom::End` costs no round trip.
    size: u64,
    /// Where the next read starts.
    pos: u64,
    /// The largest window one request may ask for.
    max: usize,
    /// The connection, for [`Error::ConnectionLost`].
    authority: String,
}

impl WindowReader {
    /// Wrap the requesting end of a window server.
    pub(crate) fn new(
        tx: UnboundedSender<WindowRequest>,
        size: u64,
        max: usize,
        authority: String,
    ) -> Self {
        Self {
            tx,
            size,
            pos: 0,
            max: max.max(1),
            authority,
        }
    }
}

impl Read for WindowReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let len = out.len().min(self.max);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let request = WindowRequest {
            offset: self.pos,
            len,
            reply: tx,
        };
        if self.tx.send(request).is_err() {
            return Err(as_io(Error::ConnectionLost(self.authority.clone())));
        }
        let data = match rx.recv() {
            Ok(Ok(data)) => data,
            Ok(Err(err)) => return Err(as_io(err)),
            Err(_) => return Err(as_io(Error::ConnectionLost(self.authority.clone()))),
        };
        let n = data.len().min(out.len());
        if let (Some(dst), Some(src)) = (out.get_mut(..n), data.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl Seek for WindowReader {
    /// Arithmetic only. Nothing here talks to the server, because SFTP has no
    /// seek: an offset is an argument to the next read.
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let next = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => offset(self.size, delta),
            SeekFrom::Current(delta) => offset(self.pos, delta),
        };
        match next {
            Some(pos) => {
                self.pos = pos;
                Ok(pos)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            )),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

/// `base + delta`, or `None` when that would be before the start of the file.
///
/// Written out rather than cast through `i64`, because a file longer than
/// `i64::MAX` is not the thing that should decide whether a seek is legal.
fn offset(base: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        base.checked_add(delta.unsigned_abs())
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// A hand-built pipeline: the chunks a fake actor would have sent.
    fn reader_over(chunks: Vec<Chunk>) -> (PipelinedReader, Arc<Semaphore>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let permits = Arc::new(Semaphore::new(4));
        for chunk in chunks {
            let _ = tx.send(chunk);
        }
        drop(tx);
        (PipelinedReader::new(rx, Arc::clone(&permits)), permits)
    }

    #[test]
    fn a_reader_hands_out_every_byte_in_order() {
        let (mut reader, _permits) = reader_over(vec![
            Ok(b"hello ".to_vec()),
            Ok(b"remote ".to_vec()),
            Ok(b"world".to_vec()),
        ]);
        let mut out = String::new();
        reader.read_to_string(&mut out).expect("read");
        assert_eq!(out, "hello remote world");
    }

    #[test]
    fn a_reader_stops_at_the_end_and_stays_stopped() {
        let (mut reader, _permits) = reader_over(vec![Ok(b"ab".to_vec())]);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).expect("first"), 2);
        assert_eq!(reader.read(&mut buf).expect("eof"), 0);
        assert_eq!(reader.read(&mut buf).expect("still eof"), 0);
    }

    #[test]
    fn a_short_output_buffer_takes_the_chunk_a_piece_at_a_time() {
        let (mut reader, _permits) = reader_over(vec![Ok(b"abcdef".to_vec())]);
        let mut buf = [0u8; 4];
        assert_eq!(reader.read(&mut buf).expect("first"), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(reader.read(&mut buf).expect("second"), 2);
        assert_eq!(buf.get(..2), Some(&b"ef"[..]));
    }

    #[test]
    fn a_dropped_connection_mid_read_surfaces_as_connection_aborted() {
        // I14: the error has to stay recognisable through `Error::Bare`.
        let (mut reader, _permits) = reader_over(vec![
            Ok(b"partial".to_vec()),
            Err(Error::ConnectionLost("sftp://t@h:22".to_string())),
        ]);
        let mut buf = [0u8; 16];
        assert_eq!(reader.read(&mut buf).expect("the rows that arrived"), 7);
        let err = reader.read(&mut buf).expect_err("then the failure");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        let inner = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<Error>())
            .expect("the crate error survives the wrapping");
        assert!(matches!(inner, Error::ConnectionLost(a) if a == "sftp://t@h:22"));
    }

    #[test]
    fn consuming_a_chunk_returns_its_permit_and_dropping_the_reader_closes_the_gate() {
        let (mut reader, permits) = reader_over(vec![Ok(b"ab".to_vec()), Ok(b"cd".to_vec())]);
        let before = permits.available_permits();
        let mut buf = [0u8; 2];
        let _ = reader.read(&mut buf).expect("first");
        assert_eq!(
            permits.available_permits(),
            before + 1,
            "a consumed chunk lets the actor issue one more read"
        );
        drop(reader);
        assert!(
            permits.is_closed(),
            "a dropped reader stops the read-ahead (the design, the read_dir idiom)"
        );
    }

    /// A fake actor-side writer: acknowledges every chunk, and answers the
    /// flush once the queue is empty.
    fn writer_pair(depth: usize) -> (PipelinedWriter, thread::JoinHandle<Vec<u8>>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut written = Vec::new();
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    WriteMsg::Chunk(data) => {
                        written.extend_from_slice(&data);
                        let _ = ack_tx.send(Ok(()));
                    }
                    WriteMsg::Flush(reply) => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                }
            }
            written
        });
        (
            PipelinedWriter::new(
                tx,
                ack_rx,
                depth,
                "/srv/out".to_string(),
                "sftp://t@h:22".to_string(),
            ),
            worker,
        )
    }

    #[test]
    fn a_writer_delivers_every_byte_and_flush_is_the_commit() {
        let (mut writer, worker) = writer_pair(4);
        writer.write_all(b"one").expect("one");
        writer.write_all(b"two").expect("two");
        writer.flush().expect("commit");
        let got = worker.join().expect("worker");
        assert_eq!(got, b"onetwo".to_vec());
    }

    #[test]
    fn a_second_flush_is_a_no_op_and_a_write_after_it_is_refused() {
        let (mut writer, worker) = writer_pair(4);
        writer.write_all(b"x").expect("x");
        writer.flush().expect("commit");
        writer.flush().expect("idempotent");
        let err = writer.write(b"y").expect_err("committed");
        assert!(err.to_string().contains("already committed"));
        let _ = worker.join();
    }

    #[test]
    fn the_writer_never_lets_more_than_depth_chunks_go_unacknowledged() {
        // I11 stated as a test: with the fake actor refusing to acknowledge,
        // the depth+1th write must block rather than race ahead.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let mut writer = PipelinedWriter::new(
            tx,
            ack_rx,
            2,
            "/srv/out".to_string(),
            "sftp://t@h:22".to_string(),
        );
        assert_eq!(writer.write(b"a").expect("1"), 1);
        assert_eq!(writer.write(b"b").expect("2"), 1);
        // Two are outstanding and none has been acknowledged, so the third
        // must wait. Acknowledge from another thread and it goes through.
        let released = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(20));
            let _ = ack_tx.send(Ok(()));
            let mut seen = 0usize;
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    WriteMsg::Chunk(_) => seen += 1,
                    WriteMsg::Flush(reply) => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                }
            }
            seen
        });
        assert_eq!(writer.write(b"c").expect("3, after an ack"), 1);
        drop(writer);
        let seen = released.join().expect("worker");
        assert_eq!(seen, 3);
    }

    #[test]
    fn a_writer_whose_actor_is_gone_reports_the_connection_and_not_a_send_error() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WriteMsg>();
        drop(rx);
        let (_ack_tx, ack_rx) = std::sync::mpsc::channel();
        let mut writer = PipelinedWriter::new(
            tx,
            ack_rx,
            4,
            "/srv/out".to_string(),
            "sftp://t@h:22".to_string(),
        );
        let err = writer.write(b"x").expect_err("no actor");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }

    /// A fake window server over an in-memory file.
    fn window_reader(body: &'static [u8], max: usize) -> (WindowReader, thread::JoinHandle<()>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WindowRequest>();
        let worker = thread::spawn(move || {
            while let Some(req) = rx.blocking_recv() {
                let start = usize::try_from(req.offset)
                    .unwrap_or(usize::MAX)
                    .min(body.len());
                let end = start.saturating_add(req.len).min(body.len());
                let slice = body.get(start..end).unwrap_or(&[]).to_vec();
                let _ = req.reply.send(Ok(slice));
            }
        });
        (
            WindowReader::new(tx, body.len() as u64, max, "sftp://t@h:22".to_string()),
            worker,
        )
    }

    #[test]
    fn a_window_reader_reads_and_seeks_without_replaying() {
        let (mut reader, worker) = window_reader(b"0123456789", 4);
        let mut buf = [0u8; 4];
        assert_eq!(reader.read(&mut buf).expect("head"), 4);
        assert_eq!(&buf, b"0123");
        assert_eq!(reader.seek(SeekFrom::Start(8)).expect("seek"), 8);
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).expect("tail");
        assert_eq!(tail, b"89".to_vec());
        assert_eq!(reader.seek(SeekFrom::End(-3)).expect("from the end"), 7);
        let mut last = [0u8; 3];
        reader.read_exact(&mut last).expect("last three");
        assert_eq!(&last, b"789");
        drop(reader);
        worker.join().expect("worker");
    }

    #[test]
    fn a_window_reader_refuses_a_seek_before_the_start() {
        let (mut reader, worker) = window_reader(b"abc", 8);
        let err = reader.seek(SeekFrom::Current(-1)).expect_err("before zero");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        drop(reader);
        worker.join().expect("worker");
    }

    #[test]
    fn a_window_reader_whose_actor_is_gone_says_the_connection_went() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WindowRequest>();
        drop(rx);
        let mut reader = WindowReader::new(tx, 10, 8, "sftp://t@h:22".to_string());
        let mut buf = [0u8; 4];
        let err = reader.read(&mut buf).expect_err("no actor");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn offsets_saturate_rather_than_wrapping() {
        assert_eq!(offset(10, -4), Some(6));
        assert_eq!(offset(10, 4), Some(14));
        assert_eq!(offset(3, -4), None);
        assert_eq!(offset(u64::MAX, 1), None);
    }
}
