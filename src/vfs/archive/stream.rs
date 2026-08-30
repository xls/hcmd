//! Turning "a format that writes a member into a `Write`" into "a `Read` the
//! rest of hcmd already knows what to do with".
//!
//! Every archive library in the table hands out a member as a
//! reader borrowed from the open container (`zip`), an entry borrowed from an
//! iterator (`tar`), or a callback (`sevenz-rust2`). None of them can produce
//! a `Box<dyn Read + Send + 'static>`, and this crate is `#![forbid(unsafe_code)]`
//! so a self-referential wrapper is not available either.
//!
//! The answer is a worker thread and an OS pipe. The format writes; the caller
//! reads; the kernel provides the backpressure, so a 4 GB member costs one
//! pipe buffer of memory and not a byte more ("reading is
//! streaming where the format allows"). Dropping the reader closes the pipe,
//! the worker's next write fails, and it stops - which is exactly the
//! cancellation the panel and the viewer already rely on for a directory
//! listing.
//!
//! A plain `std::thread` rather than `tokio::task::spawn_blocking`: an
//! archive member is opened from job threads and from the viewer as often as
//! from the runtime, [`crate::vfs::Vfs::open_read`] is synchronous, and a
//! reader that is never drained would otherwise hold a blocking-pool slot for
//! as long as the user looks at the file.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// The reading end of a member stream.
///
/// Reports the worker's failure *after* the bytes that did arrive: a member
/// whose container is corrupt half way through yields its first half and then
/// an error, which is what lets the viewer show what there was.
pub struct MemberReader {
    pipe: std::io::PipeReader,
    failure: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for MemberReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemberReader")
    }
}

impl Read for MemberReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.pipe.read(buf)?;
        if read == 0 {
            // EOF. The writer has gone, so whatever it recorded is final.
            if let Some(message) = self
                .failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Err(std::io::Error::other(message));
            }
        }
        Ok(read)
    }
}

/// Records a failure for the reader unless the worker got to the end of its
/// work.
///
/// The pipe gives the reader an EOF whichever way the worker leaves, so "the
/// member ended" and "the thread died" look identical from the reading end
/// unless something says otherwise. This is what says otherwise.
struct FailOnDrop {
    failure: Arc<Mutex<Option<String>>>,
    armed: bool,
}

impl Drop for FailOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(
                "the archive reader stopped unexpectedly part way through this entry".to_string(),
            );
        }
    }
}

/// Run `work` on a worker thread, writing into a pipe, and return the reading
/// end.
///
/// `work` is handed a `&mut dyn Write` and is expected to stream into it. It
/// must treat a write error as "stop": that is the reader having gone away.
pub fn piped<F>(work: F) -> Result<Box<dyn Read + Send>>
where
    F: FnOnce(&mut dyn Write) -> Result<u64> + Send + 'static,
{
    let (pipe, writer) = std::io::pipe().map_err(Error::Bare)?;
    let failure = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&failure);

    std::thread::Builder::new()
        .name("hcmd-archive-member".to_string())
        .spawn(move || {
            let mut writer = writer;
            // Declared after `writer`, so it is dropped before it: an unwind
            // records the failure and only then closes the pipe, keeping the
            // ordering the reader depends on below.
            //
            // Without this, a panic inside `work` would drop the writer, the
            // reader would see a clean EOF with nothing recorded, and a
            // half-written member would be indistinguishable from a complete
            // one - a silent truncation on attacker-controlled input, reported
            // to the caller as success.
            let mut guard = FailOnDrop {
                failure: Arc::clone(&recorder),
                armed: true,
            };
            let outcome = work(&mut writer).and_then(|_| writer.flush().map_err(Error::Bare));
            guard.armed = false;
            if let Err(err) = outcome {
                // A closed pipe is the consumer having stopped reading - the
                // viewer moving on, a copy being cancelled - and is not a
                // failure of the archive. Recording it would turn every
                // cancellation into an error dialog.
                let closed = matches!(
                    &err,
                    Error::Bare(io) | Error::Io { source: io, .. }
                        if io.kind() == std::io::ErrorKind::BrokenPipe
                );
                if !closed {
                    *recorder.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
                }
            }
            // Dropping the writer here is what gives the reader its EOF, and
            // it happens after the failure has been recorded so the reader
            // cannot see EOF before the reason for it.
        })
        .map_err(Error::Bare)?;

    Ok(Box::new(MemberReader { pipe, failure }))
}

/// Copy exactly `len` bytes from `reader` into `out`, refusing to invent any.
///
/// Archive headers *claim* sizes; this is where the claim meets the stream. A
/// member whose header says 4 GiB and whose data ends after 12 bytes produces
/// an error, not a 4 GiB buffer and not a silent truncation.
pub fn copy_exact(reader: &mut dyn Read, out: &mut dyn Write, len: u64) -> Result<u64> {
    let mut buf = [0u8; 64 * 1024];
    let mut left = len;
    while left > 0 {
        let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
        let slice = buf.get_mut(..want).unwrap_or(&mut []);
        let read = match reader.read(slice) {
            Ok(0) => {
                return Err(Error::msg(format!(
                    "the archive ends {left} bytes before the entry it declares does"
                )));
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Bare(e)),
        };
        out.write_all(buf.get(..read).unwrap_or(&[]))
            .map_err(Error::Bare)?;
        left = left.saturating_sub(read as u64);
    }
    Ok(len)
}

/// Read and discard exactly `len` bytes.
///
/// What "seeking" means on a decompressed stream: a compressed tar has no
/// central directory and no seek, so reaching a member's data means passing
/// over everything before it.
pub fn skip_exact(reader: &mut dyn Read, len: u64) -> Result<()> {
    let mut buf = [0u8; 64 * 1024];
    let mut left = len;
    while left > 0 {
        let want = usize::try_from(left.min(buf.len() as u64)).unwrap_or(buf.len());
        let slice = buf.get_mut(..want).unwrap_or(&mut []);
        match reader.read(slice) {
            Ok(0) => {
                return Err(Error::msg(format!(
                    "the archive ends {left} bytes before the entry it declares does"
                )));
            }
            Ok(n) => left = left.saturating_sub(n as u64),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Bare(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piped_member_streams_through() {
        let mut reader = piped(|out| {
            for _ in 0..1000 {
                out.write_all(&[b'x'; 1024]).map_err(Error::Bare)?;
            }
            Ok(1000 * 1024)
        })
        .expect("spawn");
        let mut got = Vec::new();
        reader.read_to_end(&mut got).expect("read");
        assert_eq!(got.len(), 1000 * 1024, "more than one pipe buffer");
        assert!(got.iter().all(|b| *b == b'x'));
    }

    #[test]
    fn a_worker_failure_arrives_after_the_bytes_that_did() {
        let mut reader = piped(|out| {
            out.write_all(b"first half").map_err(Error::Bare)?;
            Err(Error::msg("the container is corrupt"))
        })
        .expect("spawn");
        let mut got = Vec::new();
        let outcome = reader.read_to_end(&mut got);
        assert_eq!(got, b"first half");
        let err = outcome.expect_err("the failure is reported");
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[test]
    fn dropping_the_reader_stops_the_worker() {
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = piped(move |out| {
            let mut written = 0u64;
            loop {
                match out.write_all(&[0u8; 8192]) {
                    Ok(()) => written = written.saturating_add(8192),
                    Err(e) => {
                        let _ = tx.send(e.kind());
                        return Err(Error::Bare(e));
                    }
                }
                if written > 1024 * 1024 * 64 {
                    let _ = tx.send(std::io::ErrorKind::Other);
                    return Ok(written);
                }
            }
        })
        .expect("spawn");
        drop(reader);
        let kind = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the worker notices");
        assert_eq!(
            kind,
            std::io::ErrorKind::BrokenPipe,
            "the worker stops rather than filling the disk"
        );
    }

    #[test]
    fn a_lying_size_is_an_error_not_a_truncation() {
        let mut src = std::io::Cursor::new(b"twelve bytes".to_vec());
        let mut out = Vec::new();
        let err = copy_exact(&mut src, &mut out, 4096).expect_err("short read");
        assert!(err.to_string().contains("before the entry"), "{err}");
        assert_eq!(out.len(), 12, "what there was is still written");

        let mut src = std::io::Cursor::new(b"twelve bytes".to_vec());
        let mut out = Vec::new();
        assert_eq!(copy_exact(&mut src, &mut out, 6).ok(), Some(6));
        assert_eq!(out, b"twelve");

        let mut src = std::io::Cursor::new(b"twelve bytes".to_vec());
        assert!(skip_exact(&mut src, 7).is_ok());
        let mut rest = Vec::new();
        src.read_to_end(&mut rest).expect("rest");
        assert_eq!(rest, b"bytes");
        assert!(skip_exact(&mut src, 1).is_err());
    }
}
