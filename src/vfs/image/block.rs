//! The read-only window an image's bytes are read through.
//!
//! Two types and one promise. A [`Region`] is a byte range of a container
//! file: the whole image, or one partition of it. A [`Reader`] is an open
//! handle on one, and it is the only thing in this module that touches the
//! container.
//!
//! The promise is that the container is opened `O_RDONLY` and that nothing
//! reachable from here can write to it. `fatfs::FileSystem` is bounded on
//! `Read + Write + Seek` and has no read-only constructor (see
//! the design), so [`Reader`] implements `Write` and refuses;
//! the file underneath it could not honour a write if the refusal were
//! removed.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The block a [`Reader`] caches, in bytes.
///
/// FAT reads directory entries 32 bytes at a time and ISO 9660 reads
/// directory records one variable-length record at a time, so an uncached
/// window would turn one directory into hundreds of `pread` calls. One
/// aligned block is the whole cache: the access pattern of both libraries is
/// forward within a sector, and a second block would buy nothing measurable.
pub const BLOCK: u64 = 4096;

/// The most bytes [`sniff_window`] reads to decide what a region is.
///
/// `0x8800`, which is the ISO 9660 primary volume descriptor at `0x8000` plus
/// its `CD001` magic and a rounding. It is the largest window any signature
/// in `am-partitions`'s sniffer needs.
pub const SNIFF_LEN: u64 = 0x8800;

/// A byte range of a container file.
///
/// Cheap to clone and holds no handle: the container is opened per operation,
/// exactly as `ArchiveFormat` opens an archive per operation, because a
/// `fatfs::FileSystem` contains a `RefCell` and is therefore not `Sync` and
/// cannot be held by a `Vfs` (the design requires `Send + Sync`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    container: PathBuf,
    start: u64,
    len: u64,
}

impl Region {
    /// The whole container, whose length is measured now.
    pub fn whole(container: impl Into<PathBuf>) -> Result<Self> {
        let container = container.into();
        let len = std::fs::metadata(&container)
            .map_err(|e| Error::io(&container, e))?
            .len();
        Ok(Self {
            container,
            start: 0,
            len,
        })
    }

    /// A sub-range, refused unless it lies wholly inside the container.
    ///
    /// This is the check that stops a partition table from addressing bytes
    /// that are not there: an MBR entry is a 32-bit LBA and a 32-bit count and
    /// nothing in the format says they have to describe a partition that
    /// exists (the design I3). A truncated download of a disk
    /// image is the ordinary way to meet one.
    pub fn sub(container: impl Into<PathBuf>, start: u64, len: u64) -> Result<Self> {
        let whole = Self::whole(container)?;
        whole.slice(start, len)
    }

    /// The container file these bytes are in.
    pub fn container(&self) -> &Path {
        &self.container
    }

    /// The first byte, as an offset into the container.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// How many bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the region is empty, for clippy and for callers.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True when `[offset, offset + len)` is inside this region, offsets
    /// being relative to the region's own start.
    ///
    /// Saturating throughout: an extent claiming `u64::MAX` bytes answers
    /// `false` rather than wrapping into `true`.
    pub fn contains(&self, offset: u64, len: u64) -> bool {
        match offset.checked_add(len) {
            Some(end) => end <= self.len,
            None => false,
        }
    }

    /// A region inside this one, offsets relative to this one's start.
    pub fn slice(&self, offset: u64, len: u64) -> Result<Self> {
        if len == 0 {
            return Err(Error::InvalidPath(format!(
                "{}: a region of no bytes",
                self.container.display()
            )));
        }
        if !self.contains(offset, len) {
            return Err(Error::InvalidPath(format!(
                "{}: bytes {}..{} are past the end of the file, which is {} long",
                self.container.display(),
                self.start.saturating_add(offset),
                self.start.saturating_add(offset).saturating_add(len),
                self.start.saturating_add(self.len),
            )));
        }
        Ok(Self {
            container: self.container.clone(),
            start: self.start.saturating_add(offset),
            len,
        })
    }

    /// Open it. Positioned at 0, which `fatfs::FileSystem::new` asserts on in
    /// debug builds.
    ///
    /// `std::fs::File::open` and nothing else: the handle is `O_RDONLY`, so a
    /// bug here or in any of the three libraries cannot reach the user's image
    /// (the design I1).
    pub fn open(&self) -> Result<Reader> {
        let file =
            std::fs::File::open(&self.container).map_err(|e| Error::io(&self.container, e))?;
        Ok(Reader {
            file,
            start: self.start,
            len: self.len,
            pos: 0,
            block: Box::new([0u8; BLOCK as usize]),
            block_at: None,
            block_len: 0,
        })
    }
}

/// An open, read-only, block-cached handle on a [`Region`].
///
/// Positions are relative to the region: seeking to 0 seeks to the region's
/// first byte, and a read that would pass its last byte is short rather than
/// leaking the bytes after it. That is what makes one partition unable to
/// read another, and it is enforced here rather than in the two format
/// modules, which is why neither of them ever sees an absolute offset.
#[derive(Debug)]
pub struct Reader {
    file: std::fs::File,
    start: u64,
    len: u64,
    pos: u64,
    block: Box<[u8; BLOCK as usize]>,
    block_at: Option<u64>,
    block_len: usize,
}

impl Reader {
    /// How many bytes this reader can still produce from where it stands.
    pub fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    /// Fill the cache with the aligned block at `base`, clamped at the
    /// region's end.
    ///
    /// The clamp is the whole of the isolation: a partition's last block is
    /// short rather than spilling into whatever follows it, so a filesystem
    /// reader that walks off its own volume reads zero bytes rather than its
    /// neighbour's directory.
    fn fill(&mut self, base: u64) -> std::io::Result<()> {
        let want = BLOCK.min(self.len.saturating_sub(base));
        let want = usize::try_from(want).unwrap_or(0);
        self.block_at = None;
        self.block_len = 0;
        if want == 0 {
            self.block_at = Some(base);
            return Ok(());
        }
        self.file
            .seek(SeekFrom::Start(self.start.saturating_add(base)))?;
        let mut filled = 0usize;
        while filled < want {
            let Some(slice) = self.block.get_mut(filled..want) else {
                break;
            };
            match self.file.read(slice) {
                // Short at the end of the container: the region was measured
                // when it was made and the file may have been truncated since.
                Ok(0) => break,
                Ok(n) => filled = filled.saturating_add(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.block_at = Some(base);
        self.block_len = filled;
        Ok(())
    }
}

impl Read for Reader {
    /// Serves from the cached block when it can and reads one aligned block
    /// when it cannot. Never reads past the region.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.remaining() == 0 {
            return Ok(0);
        }
        let base = self.pos.saturating_sub(self.pos % BLOCK);
        if self.block_at != Some(base) {
            self.fill(base)?;
        }
        let within = usize::try_from(self.pos.saturating_sub(base)).unwrap_or(0);
        let available = self.block_len.saturating_sub(within);
        if available == 0 {
            return Ok(0);
        }
        let cap = usize::try_from(self.remaining()).unwrap_or(usize::MAX);
        let take = buf.len().min(available).min(cap);
        let Some(src) = self.block.get(within..within.saturating_add(take)) else {
            return Ok(0);
        };
        let Some(dst) = buf.get_mut(..take) else {
            return Ok(0);
        };
        dst.copy_from_slice(src);
        self.pos = self.pos.saturating_add(take as u64);
        Ok(take)
    }
}

impl Seek for Reader {
    /// Relative to the region. A seek before its start is `InvalidInput`; a
    /// seek past its end is allowed and reads nothing, which is what
    /// `std::fs::File` does and what both libraries expect.
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target: i128 = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.len).saturating_add(i128::from(n)),
            SeekFrom::Current(n) => i128::from(self.pos).saturating_add(i128::from(n)),
        };
        if target < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a seek before the start of the region",
            ));
        }
        self.pos = u64::try_from(target).unwrap_or(u64::MAX);
        Ok(self.pos)
    }
}

impl Write for Reader {
    /// Always `PermissionDenied`, without touching the file.
    ///
    ///
    /// `fatfs::FileSystem<T>` is bounded on `T: Read + Write + Seek` and 0.3.6
    /// has no read-only constructor, so the bound is satisfied here and the
    /// write is made impossible instead of absent. The file underneath is
    /// `O_RDONLY`, so the kernel would refuse it even if this did not.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = buf;
        Err(refusal())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(refusal())
    }
}

/// The one refusal both write methods answer with.
fn refusal() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "a disk image is opened read-only",
    )
}

/// Read up to [`SNIFF_LEN`] bytes from the start of `region`, short at its
/// end.
///
/// The one allocation in the detection path, and it is bounded by the
/// constant rather than by anything the image says.
pub fn sniff_window(region: &Region) -> Result<Vec<u8>> {
    let want = usize::try_from(SNIFF_LEN.min(region.len())).unwrap_or(0);
    let mut reader = region.open()?;
    let mut out = vec![0u8; want];
    let mut filled = 0usize;
    while filled < want {
        let Some(slice) = out.get_mut(filled..) else {
            break;
        };
        match reader.read(slice) {
            Ok(0) => break,
            Ok(n) => filled = filled.saturating_add(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::io(region.container(), e)),
        }
    }
    out.truncate(filled);
    Ok(out)
}

/// Copy `len` bytes starting at `offset` within `region` into `out`.
///
/// The whole of an ISO member read. Refuses before it starts unless
/// [`Region::contains`] agrees, then hands the work to
/// `archive::stream::copy_exact`, which refuses to invent a byte the file
/// does not have.
pub fn copy_range(region: &Region, offset: u64, len: u64, out: &mut dyn Write) -> Result<u64> {
    if !region.contains(offset, len) {
        return Err(Error::InvalidPath(format!(
            "{}: an extent of {len} bytes at {offset} is not inside this volume",
            region.container().display()
        )));
    }
    let mut reader = region.open()?;
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| Error::io(region.container(), e))?;
    crate::vfs::archive::stream::copy_exact(&mut reader, out, len)
}
