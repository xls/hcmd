//! The byte source the viewer streams through.
//!
//! > No mode and no file size ever loads the whole thing into memory … Memory
//! > is capped and is a function of the window size, not the file size. A 40 GB
//! > file must open as fast as a 4 KB one.
//!
//! # The types say so
//!
//! [`Source`] has exactly one read method, [`Source::read_window`], and it
//! takes a [`WindowLen`]. A `WindowLen` cannot be constructed above
//! [`WindowLen::MAX`] - [`WindowLen::new`] clamps and there is no other
//! constructor - so "read the whole file" is not a call anybody can spell,
//! whatever they pass. That is deliberate: a cap enforced by a `min` inside the
//! function is a rule somebody deletes, and a cap enforced by the type is one
//! nobody can.
//!
//! The one door out is [`Source::from_memory`], which takes bytes that are
//! *already* in memory and never came from a file - the `F1` help page
//! is the only caller. It reads through exactly the same window
//! machinery afterwards.
//!
//! # Seekable and forward-only
//!
//! the design requires both:
//!
//! * A backend with [`crate::vfs::Capabilities::seekable`] hands over a
//!   `Read + Seek` and every window is a `seek` plus a `read`.
//! * A backend that cannot seek - a stream out of a compressed tar, a remote
//!   read - is read forward only, and a **backward seek replays**: the stream
//!   is reopened from the beginning and bytes are discarded, in a fixed-size
//!   buffer, until the wanted offset. Nothing is buffered and nothing is kept.
//!
//! Replay is why [`Source`] holds an [`Opener`] rather than a handle: reopening
//! is the only thing a forward-only stream can do instead of seeking.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::vfs::{ReadSeek, Vfs, VfsPath};

/// The largest byte range one [`Source::read_window`] can return.
///
/// 256 KiB. It has to comfortably cover the widest terminal times the tallest
/// terminal times the longest UTF-8 encoding of a character, plus the
/// read-ahead, and it has to be small enough that holding one per open viewer
/// is not worth thinking about. The number is a *ceiling on memory*, not a
/// target: an ordinary text window asks for a few KB.
pub const MAX_WINDOW: usize = 256 * 1024;

/// How much is discarded per `read` while replaying a forward-only stream.
const REPLAY_BUF: usize = 64 * 1024;

/// A length that is known to fit in one window.
///
/// The only constructor clamps, so no value above [`WindowLen::MAX`] exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowLen(usize);

impl WindowLen {
    /// The ceiling, as a `WindowLen`.
    pub const MAX: Self = Self(MAX_WINDOW);

    /// Clamp `bytes` into a window length.
    ///
    /// Clamping rather than refusing: every caller derives this from a terminal
    /// size or a row count, and an error return there would only be unwrapped.
    /// The caller learns what it actually got from [`Window::len`].
    pub const fn new(bytes: usize) -> Self {
        if bytes > MAX_WINDOW {
            Self::MAX
        } else {
            Self(bytes)
        }
    }

    /// The clamped length.
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for WindowLen {
    fn default() -> Self {
        Self::MAX
    }
}

/// A contiguous run of bytes read out of a [`Source`], and where it came from.
///
/// Never longer than [`MAX_WINDOW`], because the only thing that produces one
/// is [`Source::read_window`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Window {
    at: u64,
    bytes: Vec<u8>,
    hit_eof: bool,
}

impl Window {
    /// The file offset the first byte sits at.
    pub const fn at(&self) -> u64 {
        self.at
    }

    /// The bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// How many bytes came back. Short of what was asked for at end of file.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when the read came back with nothing.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// One past the last byte, as a file offset.
    pub fn end(&self) -> u64 {
        self.at.saturating_add(self.bytes.len() as u64)
    }

    /// True when the read reached the end of the source.
    pub const fn hit_eof(&self) -> bool {
        self.hit_eof
    }

    /// True when `[from, to)` lies wholly inside this window.
    pub fn covers(&self, from: u64, to: u64) -> bool {
        from >= self.at && to <= self.end() && from <= to
    }

    /// The bytes for the file range `[from, to)`, clipped to what this window
    /// holds. Never panics on a range that is partly or wholly outside.
    pub fn slice(&self, from: u64, to: u64) -> &[u8] {
        let start = from.saturating_sub(self.at).min(self.bytes.len() as u64);
        let end = to.saturating_sub(self.at).min(self.bytes.len() as u64);
        let (start, end) = (start as usize, end as usize);
        self.bytes.get(start..end.max(start)).unwrap_or(&[])
    }
}

/// How to open - or reopen - the byte stream from its beginning.
///
/// `Fn` and not `FnOnce`, because a forward-only source reopens on every
/// backward seek. Shared so the background index task and the rendering path
/// each hold their own handle onto the same file without either owning the
/// other.
pub type Opener = Arc<dyn Fn() -> Result<Stream> + Send + Sync>;

/// A freshly opened handle, in whichever of the two shapes the backend offers.
pub enum Stream {
    /// Random access. Every window is a seek and a read.
    Seekable(Box<dyn ReadSeek + Send>),
    /// Forward only. A backward seek costs a reopen and a replay.
    Forward(Box<dyn Read + Send>),
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seekable(_) => f.write_str("Stream::Seekable"),
            Self::Forward(_) => f.write_str("Stream::Forward"),
        }
    }
}

/// Build an [`Opener`] over a [`Vfs`] path.
///
/// Prefers [`Vfs::open_seek`] and falls back to [`Vfs::open_read`], so a
/// backend that has not implemented random access is slower and never broken.
pub fn vfs_opener(vfs: Arc<dyn Vfs>, path: VfsPath) -> Opener {
    Arc::new(move || match vfs.open_seek(&path) {
        Ok(handle) => Ok(Stream::Seekable(handle)),
        Err(_) => vfs.open_read(&path).map(Stream::Forward),
    })
}

/// An [`Opener`] over bytes that are already in memory.
///
/// For content that was never a file: the `F1` help page, and the tests.
/// Everything downstream of here is the ordinary window path, which is the
/// point - the help view "uses the same viewer machinery".
pub fn memory_opener(bytes: Arc<Vec<u8>>) -> Opener {
    Arc::new(move || {
        Ok(Stream::Seekable(Box::new(std::io::Cursor::new(
            bytes.as_ref().clone(),
        ))))
    })
}

/// A file being streamed, and the handle currently open on it.
///
/// One `Source` is one cursor. The viewer holds one for rendering and the
/// background index task holds another over the same [`Opener`]; they never
/// share a position and never block each other.
pub struct Source {
    open: Opener,
    stream: Stream,
    /// The offset the open handle will read from next.
    pos: u64,
    /// The size, when the backend reported one. `None` is a stream of unknown
    /// length, which the design says must still open instantly.
    len: Option<u64>,
    /// How many times a backward seek has cost a reopen. Diagnostics only.
    replays: u64,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("seekable", &self.is_seekable())
            .field("pos", &self.pos)
            .field("len", &self.len)
            .field("replays", &self.replays)
            .finish()
    }
}

impl Source {
    /// Open a source. `len` is the size the backend reported, if any.
    pub fn open(open: Opener, len: Option<u64>) -> Result<Self> {
        let stream = open()?;
        Ok(Self {
            open,
            stream,
            pos: 0,
            len,
            replays: 0,
        })
    }

    /// A source over bytes that are already in memory. See [`memory_opener`].
    pub fn from_memory(bytes: Vec<u8>) -> Result<Self> {
        let len = bytes.len() as u64;
        Self::open(memory_opener(Arc::new(bytes)), Some(len))
    }

    /// The opener, so a second cursor can be taken over the same file - which
    /// is exactly what the background index task does.
    pub fn opener(&self) -> Opener {
        Arc::clone(&self.open)
    }

    /// True when windows cost a seek rather than a replay.
    pub const fn is_seekable(&self) -> bool {
        matches!(self.stream, Stream::Seekable(_))
    }

    /// The size the backend reported, if it reported one.
    pub const fn len(&self) -> Option<u64> {
        self.len
    }

    /// True only when the backend reported a size and it was zero.
    pub const fn is_empty(&self) -> bool {
        matches!(self.len, Some(0))
    }

    /// How many backward seeks have cost a reopen (the replay).
    pub const fn replays(&self) -> u64 {
        self.replays
    }

    /// Teach the source a size it did not know - what the background index
    /// learns by reaching the end of a stream nobody could `stat`.
    pub const fn set_len(&mut self, len: u64) {
        self.len = Some(len);
    }

    /// Read at most `len` bytes starting at file offset `at`.
    ///
    /// This is the **only** way to get bytes out of a source, and `len` cannot
    /// exceed [`WindowLen::MAX`]. A short read at end of file is not an error:
    /// the window comes back shorter with [`Window::hit_eof`] set.
    ///
    /// On a forward-only stream a backward move reopens and replays; a forward
    /// move discards. Both discard through a fixed buffer, so the memory cost
    /// is the buffer and never the distance.
    pub fn read_window(&mut self, at: u64, len: WindowLen) -> Result<Window> {
        self.position(at)?;
        let want = len.get();
        let mut bytes = vec![0_u8; want];
        let mut filled = 0_usize;
        let mut hit_eof = false;
        while filled < want {
            let dst = bytes.get_mut(filled..).unwrap_or(&mut []);
            let n = match self.reader().read(dst) {
                Ok(0) => {
                    hit_eof = true;
                    break;
                }
                Ok(n) => n,
                // A signal during a read is not a failure; retry.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::Bare(e)),
            };
            filled = filled.saturating_add(n);
        }
        bytes.truncate(filled);
        self.pos = at.saturating_add(filled as u64);
        if hit_eof {
            // The end is now known even if `stat` never said (
            // an unknown length must not stop the file opening).
            let end = self.pos;
            if self.len.is_none_or(|l| l < end) {
                self.len = Some(end);
            }
        }
        Ok(Window { at, bytes, hit_eof })
    }

    /// Put the open handle at `at`, by seeking or by replaying.
    fn position(&mut self, at: u64) -> Result<()> {
        if self.pos == at {
            return Ok(());
        }
        match &mut self.stream {
            Stream::Seekable(handle) => {
                // Seeking past the end is legal and reads back nothing, which
                // is the behaviour the caller wants for an approximate `End`
                // while the index is still building.
                handle.seek(SeekFrom::Start(at)).map_err(Error::Bare)?;
                self.pos = at;
                Ok(())
            }
            Stream::Forward(_) => {
                if at < self.pos {
                    self.replay()?;
                }
                self.discard(at.saturating_sub(self.pos))
            }
        }
    }

    /// Reopen a forward-only stream from the beginning.
    fn replay(&mut self) -> Result<()> {
        self.stream = (self.open)()?;
        self.pos = 0;
        self.replays = self.replays.saturating_add(1);
        Ok(())
    }

    /// Throw away `n` bytes through a fixed buffer.
    fn discard(&mut self, mut n: u64) -> Result<()> {
        let mut buf = [0_u8; REPLAY_BUF];
        while n > 0 {
            let want = usize::try_from(n).unwrap_or(REPLAY_BUF).min(REPLAY_BUF);
            let dst = buf.get_mut(..want).unwrap_or(&mut []);
            match self.reader().read(dst) {
                Ok(0) => {
                    // The stream ended before the target. Not an error: the
                    // caller gets an empty window and learns the real length.
                    self.len = Some(self.pos);
                    return Ok(());
                }
                Ok(read) => {
                    self.pos = self.pos.saturating_add(read as u64);
                    n = n.saturating_sub(read as u64);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Bare(e)),
            }
        }
        Ok(())
    }

    fn reader(&mut self) -> &mut (dyn Read + Send) {
        match &mut self.stream {
            Stream::Seekable(handle) => handle,
            Stream::Forward(handle) => handle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Read`-only wrapper, so the forward-only path can be exercised without
    /// an archive or a remote host.
    fn forward_opener(bytes: Vec<u8>) -> Opener {
        let shared = Arc::new(bytes);
        Arc::new(move || {
            Ok(Stream::Forward(Box::new(std::io::Cursor::new(
                shared.as_ref().clone(),
            ))))
        })
    }

    #[test]
    fn a_window_length_cannot_be_built_above_the_cap() {
        assert_eq!(WindowLen::new(usize::MAX).get(), MAX_WINDOW);
        assert_eq!(WindowLen::new(10).get(), 10);
        assert_eq!(WindowLen::MAX.get(), MAX_WINDOW);
    }

    #[test]
    fn a_read_is_capped_by_the_type_not_by_the_argument() {
        let mut src = Source::from_memory(vec![b'x'; MAX_WINDOW * 2]).expect("open");
        let w = src
            .read_window(0, WindowLen::new(usize::MAX))
            .expect("read");
        assert_eq!(w.len(), MAX_WINDOW, "the whole file is not askable for");
        assert!(!w.hit_eof());
    }

    #[test]
    fn a_short_read_at_the_end_is_not_an_error() {
        let mut src = Source::from_memory(b"abcdef".to_vec()).expect("open");
        let w = src.read_window(4, WindowLen::new(100)).expect("read");
        assert_eq!(w.bytes(), b"ef");
        assert!(w.hit_eof());
        assert_eq!(w.at(), 4);
        assert_eq!(w.end(), 6);
    }

    #[test]
    fn seeking_past_the_end_reads_nothing_rather_than_failing() {
        let mut src = Source::from_memory(b"abc".to_vec()).expect("open");
        let w = src.read_window(9_000, WindowLen::new(16)).expect("read");
        assert!(w.is_empty());
        assert!(w.hit_eof());
    }

    #[test]
    fn a_forward_only_source_replays_for_a_backward_seek() {
        let data: Vec<u8> = (0..=255_u8).cycle().take(100_000).collect();
        let mut src = Source::open(forward_opener(data.clone()), Some(100_000)).expect("open");
        assert!(!src.is_seekable());

        let w = src.read_window(90_000, WindowLen::new(8)).expect("forward");
        assert_eq!(w.bytes(), data.get(90_000..90_008).expect("slice"));
        assert_eq!(src.replays(), 0, "a forward move never replays");

        let w = src.read_window(10, WindowLen::new(8)).expect("backward");
        assert_eq!(w.bytes(), data.get(10..18).expect("slice"));
        assert_eq!(src.replays(), 1, "a backward move replays exactly once");
    }

    #[test]
    fn a_stream_of_unknown_length_learns_its_own_end() {
        let mut src = Source::open(forward_opener(b"hello".to_vec()), None).expect("open");
        assert_eq!(src.len(), None);
        let w = src.read_window(0, WindowLen::new(64)).expect("read");
        assert!(w.hit_eof());
        assert_eq!(src.len(), Some(5));
    }

    #[test]
    fn slice_clips_rather_than_panicking() {
        let mut src = Source::from_memory(b"0123456789".to_vec()).expect("open");
        let w = src.read_window(2, WindowLen::new(4)).expect("read");
        assert_eq!(w.slice(2, 6), b"2345");
        assert_eq!(w.slice(0, 100), b"2345", "clipped at both ends");
        assert_eq!(w.slice(50, 60), b"");
        assert_eq!(
            w.slice(5, 3),
            b"",
            "an inverted range is empty, not a panic"
        );
        assert!(w.covers(3, 5));
        assert!(!w.covers(3, 99));
    }
}
