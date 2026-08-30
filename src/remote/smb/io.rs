//! The two handles an SMB transfer hands out, and nothing else.
//!
//! Both are the **blocking** halves of a bridge whose other half is a task
//! spawned by [`super::actor`]. They know nothing about SMB: a reader is a
//! request/response pair, a writer is a queue of chunks with an
//! acknowledgement each. That is what lets both be tested here against a
//! hand-driven thread with no server anywhere.
//!
//! # Which channel, and why
//!
//! The same rule the SFTP backend follows, for the same reason: every message
//! travelling **towards** the blocking side arrives on a `std::sync::mpsc`
//! channel, whose `recv` blocks any thread and panics on none, and every
//! message travelling **towards** the actor leaves on a tokio unbounded
//! sender, whose `send` is neither `async` nor blocking nor able to panic.
//! `blocking_recv` panics when the thread it is on turns out to be a runtime
//! worker, and this house has no panic paths.
//!
//! Unbounded is safe here because the backpressure is written back in: the
//! calling thread has at most one request in flight, because it parks on the
//! reply to each one before it sends the next.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use tokio::sync::mpsc::UnboundedSender;

use crate::error::{Error, Result};

/// Turn a transport failure into the `io::Error` a `Read` or a `Write` has to
/// return, without losing which failure it was.
///
/// [`io::ErrorKind::ConnectionAborted`] and the boxed [`Error`] are both
/// deliberate: `ops::copy` wraps whatever comes back, and a dropped connection
/// has to stay recognisable through that wrapping for `ops::is_fatal` to stop
/// the batch.
pub(crate) fn as_io(err: Error) -> io::Error {
    match err {
        Error::ConnectionLost(_) => io::Error::new(io::ErrorKind::ConnectionAborted, err),
        other => io::Error::other(other),
    }
}

/// One positioned read, and where its answer goes.
pub(crate) struct ReadRequest {
    /// Where in the file to start.
    pub(crate) at: u64,
    /// How many bytes to ask the server for.
    pub(crate) len: u64,
    /// The answer. A depth-one `SyncSender`, because the thread that sent the
    /// request is parked on the matching `recv`.
    pub(crate) reply: SyncSender<Result<Vec<u8>>>,
}

/// A random-access reader over one open SMB file.
///
/// SMB2 READ carries an offset, so this is the only reader the backend needs:
/// the viewer's seeking and a copy's straight-through read are the same
/// handle. Dropping it drops the request sender, which is what ends the task
/// holding the file open.
///
/// **Blocking.** Call from the blocking pool only.
pub(crate) struct SmbReader {
    /// Where a request goes.
    requests: UnboundedSender<ReadRequest>,
    /// The file's size, learned when it was opened, so a seek to the end costs
    /// no round trip.
    size: u64,
    /// The read position.
    at: u64,
    /// For the message a lost connection produces. Never a secret.
    authority: String,
}

impl SmbReader {
    /// Wrap the request end of an open file.
    pub(crate) fn new(
        requests: UnboundedSender<ReadRequest>,
        size: u64,
        authority: String,
    ) -> Self {
        Self {
            requests,
            size,
            at: 0,
            authority,
        }
    }
}

impl Read for SmbReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.at >= self.size {
            return Ok(0);
        }
        let want = (self.size.saturating_sub(self.at)).min(out.len() as u64);
        let (reply, answer) = sync_channel(1);
        let request = ReadRequest {
            at: self.at,
            len: want,
            reply,
        };
        if self.requests.send(request).is_err() {
            return Err(as_io(Error::ConnectionLost(self.authority.clone())));
        }
        let bytes = match answer.recv() {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => return Err(as_io(err)),
            // The task went away without answering, which is the connection.
            Err(_) => return Err(as_io(Error::ConnectionLost(self.authority.clone()))),
        };
        let n = bytes.len().min(out.len());
        if let (Some(dst), Some(src)) = (out.get_mut(..n), bytes.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.at = self.at.saturating_add(n as u64);
        Ok(n)
    }
}

impl Seek for SmbReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(delta) => offset(self.at, delta),
            SeekFrom::End(delta) => offset(self.size, delta),
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            ));
        };
        self.at = target;
        Ok(self.at)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.at)
    }
}

/// `base + delta`, or `None` when it would land before byte zero.
///
/// Saturating rather than wrapping: a wrapped offset is a read somewhere else
/// in the file, which is worse than a refusal.
fn offset(base: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        return Some(base.saturating_add(delta.unsigned_abs()));
    }
    base.checked_sub(delta.unsigned_abs())
}

/// What the writing side sends to the task holding the file open.
pub(crate) enum WriteMsg {
    /// Bytes, and where the acknowledgement goes.
    Chunk(Vec<u8>, SyncSender<Result<()>>),
    /// Commit: drain, flush and close. The reply is the commit's verdict.
    Finish(SyncSender<Result<()>>),
    /// Give up without committing, which is what dropping an unflushed writer
    /// means.
    Abort,
}

/// A writer whose `flush` is the commit.
///
/// Each chunk is acknowledged before the next is sent, which is the
/// backpressure that makes the unbounded sender safe. The wire-level
/// pipelining is inside smb2's own writer, so a chunk of the size
/// `ops::chunk_size` picks is already several WRITEs in flight.
///
/// **Blocking.** Call from the blocking pool only.
pub(crate) struct SmbWriter {
    /// Where a chunk goes.
    chunks: UnboundedSender<WriteMsg>,
    /// Set by the commit, after which a write is refused and a second flush
    /// does nothing.
    committed: bool,
    /// For the message a lost connection produces. Never a secret.
    authority: String,
}

impl SmbWriter {
    /// Wrap the sending end of an open file.
    pub(crate) fn new(chunks: UnboundedSender<WriteMsg>, authority: String) -> Self {
        Self {
            chunks,
            committed: false,
            authority,
        }
    }

    /// The failure a vanished task produces.
    fn gone(&self) -> io::Error {
        as_io(Error::ConnectionLost(self.authority.clone()))
    }
}

impl Write for SmbWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.committed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this file has already been committed",
            ));
        }
        let (ack, acked) = sync_channel(1);
        if self
            .chunks
            .send(WriteMsg::Chunk(data.to_vec(), ack))
            .is_err()
        {
            return Err(self.gone());
        }
        match acked.recv() {
            Ok(Ok(())) => Ok(data.len()),
            Ok(Err(err)) => Err(as_io(err)),
            Err(_) => Err(self.gone()),
        }
    }

    /// The commit. A second call is a no-op, so a caller that flushes and then
    /// drops does not commit twice.
    fn flush(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        self.committed = true;
        let (reply, answer) = sync_channel(1);
        if self.chunks.send(WriteMsg::Finish(reply)).is_err() {
            return Err(self.gone());
        }
        match answer.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(as_io(err)),
            Err(_) => Err(self.gone()),
        }
    }
}

impl Drop for SmbWriter {
    /// A writer dropped without a flush wrote nothing anyone asked to keep, so
    /// the handle is closed rather than committed. `ops::copy` flushes before
    /// it drops; a cancelled copy does not, and this is what stops a partial
    /// file being left behind as though it were whole.
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.chunks.send(WriteMsg::Abort);
        }
    }
}

/// The receiving end of a writer, for the task that owns the file.
pub(crate) type WriteQueue = tokio::sync::mpsc::UnboundedReceiver<WriteMsg>;

/// The receiving end of a reader, for the task that owns the file.
pub(crate) type ReadQueue = tokio::sync::mpsc::UnboundedReceiver<ReadRequest>;

/// A reply channel the actor answers a one-shot command on.
pub(crate) type Reply<T> = SyncSender<Result<T>>;

/// Park on a reply, turning both ways of not getting one into the connection.
///
/// Every synchronous call into the actor ends here, so "the actor is gone" is
/// spelled once rather than at a dozen call sites.
pub(crate) fn wait<T>(answer: Receiver<Result<T>>, authority: &str) -> Result<T> {
    match answer.recv() {
        Ok(outcome) => outcome,
        Err(_) => Err(Error::ConnectionLost(authority.to_string())),
    }
}

#[cfg(test)]
#[path = "io_tests.rs"]
mod tests;
