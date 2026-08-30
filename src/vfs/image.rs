//! Disk images are directories.
//!
//! > A `.iso` and a `.img` are containers you browse into, which is what
//! > already means by a directory.
//!
//! [`ImageFs`] is a [`Vfs`] over **one** thing an image can present: either
//! its partition table, or one filesystem on it. That is the whole
//! integration - the panel, `ops`, the viewer and the clipboard already work
//! through the trait, so copying a kernel out of a rescue image, viewing a
//! config file on a card dump and walking a partition for a size total are
//! the code that already exists, unchanged.
//!
//! ```text
//!   VfsPath: [(Local, /a/backup.img), (Image, /2), (Image, /boot/vmlinuz)]
//!             └──────────┬─────────┘  └────┬───┘  └────────┬───────────┘
//!                   the container      the partition    the member
//!
//!   VfsPath: [(Local, /a/ubuntu.iso), (Image, /casper/vmlinuz)]
//!             └──────────┬─────────┘  └────────────┬───────┘
//!                   the container        no table, so the first segment
//!                                        is the filesystem's root
//! ```
//!
//! # The four things this module decides
//!
//! **Detection** is by content ([`format::detect`]), because
//! "`.img` says nothing at all about what is inside".
//!
//! **The partition is a segment**, not a directory inside one, because the
//! path after it belongs to a different filesystem and that is what the
//! segment stack is for.
//!
//! **Safety** is `archive::safety`, unforked: a member whose name could
//! escape never enters the index. The names in an ISO or a FAT image are
//! chosen by whoever wrote the image and are exactly as untrusted as a zip
//! member's.
//!
//! **Read-only** is the feature. There is no write path to disable.

pub mod block;
pub mod ext;
pub mod fat;
pub mod format;
pub mod iso;
pub mod part;
pub mod squashfs;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::vfs::archive::index::{Index, IndexStatus, Member, MemberKind};
use crate::vfs::archive::session::{ArchiveSession, CachedFile};
use crate::vfs::archive::{index, safety, stream};
use crate::vfs::{
    BackendKind, Capabilities, Entry, READ_DIR_CHANNEL_DEPTH, ReadSeek, Vfs, VfsPath,
};

use block::Region;
use format::{FsId, Shape, VolumeFormat};
use part::Table;

/// One thing an image presents, as a filesystem.
///
/// Cheap to clone: everything is behind one `Arc`, so two panels looking into
/// the same partition share its index rather than building two.
#[derive(Clone)]
pub struct ImageFs {
    inner: Arc<Inner>,
}

/// Which of the two things this backend is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewKind {
    /// The image's partition table. One row per partition; no index, because
    /// there is nothing to stream.
    Table,
    /// One filesystem: the whole image when it has no table, or one partition
    /// of it.
    Volume(FsId),
}

struct Inner {
    /// The local file holding the image's bytes.
    ///
    /// For an image that came off a remote or out of an archive this is the
    /// session's copy, **not** what the user sees. Everything that touches
    /// bytes uses this; everything that speaks to a human uses `display`.
    container: PathBuf,
    /// The session cache entry `container` lives in, for an image that is not
    /// itself a local file.
    ///
    /// Held, not merely remembered: `ArchiveSession::make_room` evicts a
    /// cached file only when nothing outside the cache holds it, and
    /// "nothing" is measured with `Arc::strong_count`.
    cached: Option<Arc<CachedFile>>,
    /// The image's own address, as the user sees it.
    display: VfsPath,
    /// Which partition this is, from 1. `None` for the table view and for an
    /// image with no table.
    partition: Option<usize>,
    view: ViewKind,
    /// The bytes this backend may read: the whole container, or one
    /// partition's window.
    region: Region,
    /// The table, for the table view.
    table: Option<Table>,
    /// The reader, for a volume whose filesystem this program can read.
    format: Option<&'static dyn VolumeFormat>,
    /// The listing, for a volume. `None` for the table view, which has
    /// nothing to stream.
    index: Option<Arc<Index>>,
    /// Weak, deliberately: the session owns the images, so an `Arc` here would
    /// be a cycle and the session's temp directory would outlive the process
    /// that made it.
    session: Weak<ArchiveSession>,
}

impl std::fmt::Debug for ImageFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageFs")
            .field("container", &self.inner.container)
            .field("display", &self.inner.display.to_string())
            .field("view", &self.inner.view)
            .field("partition", &self.inner.partition)
            .finish()
    }
}

impl Drop for Inner {
    /// Closing a volume stops its index build.
    fn drop(&mut self) {
        if let Some(index) = self.index.as_ref() {
            index.cancel();
        }
    }
}

impl ImageFs {
    /// Open `container` and start reading it.
    ///
    /// `partition` is `None` for the image's own root namespace and `Some(n)`
    /// for the filesystem on partition `n`. Returns as soon as the shape is
    /// known: a volume's index is built on a worker thread and the panel fills
    /// from it. Prefer `ArchiveSession::open_image`, which
    /// caches; this is the constructor it calls, exposed for tests.
    pub fn open(
        session: &Arc<ArchiveSession>,
        container: impl Into<PathBuf>,
        display: VfsPath,
        partition: Option<usize>,
    ) -> Result<Self> {
        let container = container.into();
        let shape = format::detect(&container)?;
        Self::with_shape(session, container, None, display, partition, shape)
    }

    /// [`ImageFs::open`] for an image whose bytes are the session's copy of
    /// something that is not a local file: an image on a remote, or an image
    /// stored in an archive (the nesting, in the direction
    /// the design builds).
    pub fn open_cached(
        session: &Arc<ArchiveSession>,
        cached: Arc<CachedFile>,
        display: VfsPath,
        partition: Option<usize>,
    ) -> Result<Self> {
        let container = cached.path().to_path_buf();
        let shape = format::detect(&container)?;
        Self::with_shape(session, container, Some(cached), display, partition, shape)
    }

    /// The half both constructors share, once the shape is known.
    fn with_shape(
        session: &Arc<ArchiveSession>,
        container: PathBuf,
        cached: Option<Arc<CachedFile>>,
        display: VfsPath,
        partition: Option<usize>,
        shape: Shape,
    ) -> Result<Self> {
        let whole = Region::whole(&container)?;
        let (region, table, fs) = match (shape, partition) {
            (Shape::Volume(fs), None) => (whole, None, Some(fs)),
            (Shape::Volume(_), Some(number)) => {
                return Err(Error::InvalidPath(format!(
                    "{display}: this image has no partition table, so it has no \
                     partition {number}"
                )));
            }
            (Shape::Partitioned(table), None) => (whole, Some(table), None),
            (Shape::Partitioned(table), Some(number)) => {
                let Some(entry) = table.get(number) else {
                    return Err(missing_partition(&display, &table, number));
                };
                (entry.region.clone(), None, Some(entry.fs))
            }
        };
        // Which of the two this is follows from the filesystem and from
        // nothing else: an image with a partition table has no filesystem at
        // its own level, and a volume is exactly the case where one was found.
        //
        let fs = fs.map(|fs| refine(fs, &region));
        let view = match fs {
            Some(fs) => ViewKind::Volume(fs),
            None => ViewKind::Table,
        };

        // The reader, or none, which is the difference the design draws
        // between a filesystem that is not supported and an image that is
        // damaged: an unsupported volume opens, lists nothing, and says what
        // it is by name.
        let backend = fs.and_then(FsId::backend);
        let index = match view {
            ViewKind::Table => None,
            ViewKind::Volume(fs) => {
                let idx = Arc::new(Index::new());
                match backend {
                    Some(backend) => spawn_index(region.clone(), backend, Arc::clone(&idx)),
                    None => idx.finish(IndexStatus::Failed(
                        fs.refusal(&volume_name(&display, partition)).to_string(),
                    )),
                }
                Some(idx)
            }
        };

        Ok(Self {
            inner: Arc::new(Inner {
                container,
                cached,
                display,
                partition,
                view,
                region,
                table,
                format: backend,
                index,
                session: Arc::downgrade(session),
            }),
        })
    }

    /// The filesystem on partition `number` of this image.
    ///
    /// The table is already read, so this opens no container and probes no
    /// superblock: the partition's window and its claimed filesystem were
    /// decided when the table was, and this is the [`Vfs`] over them. It is
    /// what `ArchiveSession::open_image` calls when a second `Image` segment
    /// selects a partition.
    pub fn partition_view(&self, number: usize) -> Result<Self> {
        let Some(table) = self.inner.table.as_ref() else {
            return Err(Error::InvalidPath(format!(
                "{}: this image has no partition table, so it has no partition \
                 {number}",
                self.inner.display
            )));
        };
        if table.get(number).is_none() {
            return Err(missing_partition(&self.inner.display, table, number));
        }
        let session = self.session().ok_or_else(|| {
            Error::msg("the session that opened this image has been closed".to_string())
        })?;
        Self::with_shape(
            &session,
            self.inner.container.clone(),
            self.inner.cached.clone(),
            self.inner.display.clone(),
            Some(number),
            Shape::Partitioned(table.clone()),
        )
    }

    /// Which of the two things this is showing.
    pub fn view(&self) -> &ViewKind {
        &self.inner.view
    }

    /// The filesystem, for a volume. `None` for the table view.
    pub fn filesystem(&self) -> Option<FsId> {
        match self.inner.view {
            ViewKind::Table => None,
            ViewKind::Volume(fs) => Some(fs),
        }
    }

    /// The table, for the table view. `None` for a volume.
    pub fn table(&self) -> Option<&Table> {
        self.inner.table.as_ref()
    }

    /// Which partition this volume is, from 1. `None` when the image has no
    /// table.
    pub fn partition(&self) -> Option<usize> {
        self.inner.partition
    }

    /// The bytes this backend may read. The whole container for an
    /// unpartitioned image, one partition's window otherwise.
    pub fn region(&self) -> &Region {
        &self.inner.region
    }

    /// The image's own address, as the user sees it.
    pub fn display_path(&self) -> &VfsPath {
        &self.inner.display
    }

    /// The local file the bytes are really in - the session's copy, for an
    /// image that came off a remote or out of an archive.
    pub fn container(&self) -> &Path {
        &self.inner.container
    }

    /// True when this image's bytes are the session's copy of something that
    /// is not a local file.
    pub fn is_cached(&self) -> bool {
        self.inner.cached.is_some()
    }

    /// The shared index, for a caller that wants to watch it fill. `None` for
    /// the table view, which has nothing to stream.
    pub fn index(&self) -> Option<&Arc<Index>> {
        self.inner.index.as_ref()
    }

    /// Block until the index is complete. Tests, and `Alt+F6`, which needs the
    /// whole listing before it can report a total.
    pub fn wait_for_index(&self) -> IndexStatus {
        match self.inner.index.as_ref() {
            Some(index) => index.wait_until_final(),
            // A partition table is read before this backend exists, so there
            // is nothing left to wait for.
            None => IndexStatus::Complete,
        }
    }

    /// The volume label, for a status line. `None` when there is not one.
    pub fn label(&self) -> Option<String> {
        self.inner
            .format
            .and_then(|f| f.volume_label(&self.inner.region))
    }

    /// The session that owns this image's temp files, if it is still alive.
    pub fn session(&self) -> Option<Arc<ArchiveSession>> {
        self.inner.session.upgrade()
    }

    /// The refusal every write path shares.
    ///
    /// One function, so the four methods cannot drift and so the reason is
    /// written once. It is reached before the container is opened, which is
    /// the difference the design draws between a refusal and a failure halfway
    /// through a copy.
    fn refuse_write(&self, path: &VfsPath) -> Error {
        Error::msg(format!(
            "{path}: a disk image is read-only here. Copy the file out, change \
             it, and write the image back with a tool that can"
        ))
    }

    /// The index, for a volume; a refusal for the table view.
    fn volume_index(&self, path: &VfsPath) -> Result<&Arc<Index>> {
        self.inner
            .index
            .as_ref()
            .ok_or_else(|| Error::InvalidPath(format!("{path}: a partition table has no files")))
    }

    /// The reader, for a volume whose filesystem this program can read.
    fn volume_format(&self, path: &VfsPath) -> Result<&'static dyn VolumeFormat> {
        match (self.inner.format, self.inner.view.clone()) {
            (Some(format), _) => Ok(format),
            (None, ViewKind::Volume(fs)) => {
                Err(fs.refusal(&volume_name(&self.inner.display, self.inner.partition)))
            }
            (None, ViewKind::Table) => Err(Error::InvalidPath(format!(
                "{path}: a partition table has no files"
            ))),
        }
    }

    /// The member at `path`, waiting for the index to reach it if necessary.
    fn member(&self, path: &VfsPath) -> Result<Member> {
        let key = safety::member_key(path.tail())?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{path}: the volume root is a directory"
            )));
        }
        self.volume_index(path)?.stat_blocking(&key)
    }

    /// The entry for whatever this backend's own root is: a directory named
    /// after the container, or after the partition.
    fn root_entry(&self) -> Entry {
        let name = match self.inner.partition {
            Some(number) => number.to_string(),
            None => self
                .inner
                .display
                .file_name()
                .unwrap_or_else(|| "image".to_string()),
        };
        Entry {
            is_hidden: name.starts_with('.'),
            ..Entry::dir(name)
        }
    }

    /// How big the container is, for the ratio half of `archive::guard_for`'s
    /// check.
    fn container_bytes(&self) -> u64 {
        std::fs::metadata(&self.inner.container)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// The table view's answer for a path below its root.
    ///
    /// Nothing navigates here - a partition row's `Entry::location` sends
    /// `Enter` straight to the filesystem's root - but a path typed into
    /// `Ctrl+G` can reach it and deserves the answer rather than an empty
    /// panel.
    fn partition_row(&self, path: &VfsPath) -> Result<Entry> {
        let Some(table) = self.inner.table.as_ref() else {
            return Err(Error::NotFound(path.to_string()));
        };
        let number = part::partition_number(path.tail())?;
        match table.get(number) {
            // The table's own address, not the image's: `PartitionEntry::row`
            // replaces the innermost `Image` tail with the number and pushes
            // the filesystem's root after it.
            Some(entry) => Ok(entry.row(&path.segment_root())),
            None => Err(missing_partition(path, table, number)),
        }
    }
}

/// The filesystem a message names, refined by the volume itself where the
/// superblock could not be exact (the design, rule 2).
///
/// One case, and it is FAT: the boot sector cannot tell FAT12 from FAT16,
/// because the difference is the number of clusters the geometry works out to
/// and only opening the volume computes it. So a floppy image is called FAT12
/// in the status line and in a refusal, rather than FAT16 because that is the
/// tag both of them carry.
///
/// A refinement that cannot be made keeps the sniffed answer rather than
/// failing. The volume is opened again a moment later by the index build, and
/// that is where a damaged FAT is reported; failing here would turn every
/// unreadable FAT into an image that would not open at all, which is the
/// distinction the design keeps apart.
fn refine(fs: FsId, region: &Region) -> FsId {
    match fs {
        FsId::Fat12 | FsId::Fat16 | FsId::Fat32 => fat::fat_id(region).unwrap_or(fs),
        FsId::Iso9660
        | FsId::ExFat
        | FsId::Ntfs
        | FsId::Ext2
        | FsId::Ext3
        | FsId::Ext4
        | FsId::HfsPlus
        | FsId::Apfs
        | FsId::Squashfs
        | FsId::LinuxSwap
        | FsId::Unknown => fs,
    }
}

/// A partition the table did not describe, or described and this backend
/// refused.
///
/// The three states of the design stay apart here: a refusal
/// keeps the sentence `part::read_table` wrote for it, and anything else is
/// simply not there.
fn missing_partition(path: &VfsPath, table: &Table, number: usize) -> Error {
    match table.refusal(number) {
        Some(why) => Error::msg(why.to_string()),
        None => Error::NotFound(format!(
            "{path}: there is no partition {number}; this {} table has {}",
            table.kind.label(),
            table.len()
        )),
    }
}

/// What a message calls this volume: the partition when there is one, the
/// image's own address otherwise.
fn volume_name(display: &VfsPath, partition: Option<usize>) -> String {
    match partition {
        Some(number) => format!("partition {number}"),
        None => display.to_string(),
    }
}

/// Make sure an index build always ends in a final status, however it ends.
///
/// Held by the indexing thread for the length of the build. Its `Drop` runs on
/// the ordinary path (where the status is already final and this does nothing)
/// and on an unwind (where it is the only thing that will), so no waiter is
/// ever left blocked on a build that is no longer running
/// (the design I10).
struct FinishOnDrop {
    index: Arc<Index>,
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.index.finish(IndexStatus::Failed(
            "the image reader stopped unexpectedly; the listing is incomplete".to_string(),
        ));
    }
}

/// Start the index build on its own thread.
///
/// A plain thread rather than the tokio blocking pool: this may run for as
/// long as it takes to walk a filesystem, and a blocking-pool slot held for
/// minutes starves everything else that needs one (`read_dir`, `stat`, every
/// file operation).
fn spawn_index(region: Region, format: &'static dyn VolumeFormat, idx: Arc<Index>) {
    let reporter = Arc::clone(&idx);
    let spawned = std::thread::Builder::new()
        .name("hcmd-image-index".to_string())
        .spawn(move || {
            // Everything that waits on this index loops until the status is
            // final. A thread that ended without setting one would leave the
            // panel on its `..` row for the life of the process, with no error
            // to show. Image data is attacker-controlled and neither
            // `hadris-iso` nor `fatfs` is written to this crate's no-panic
            // rule.
            let guard = FinishOnDrop {
                index: Arc::clone(&idx),
            };
            let outcome = {
                let mut sink = index::Builder::new(Arc::clone(&idx), false);
                format.index(&region, &mut sink)
            };
            let status = match outcome {
                Ok(()) if idx.cancelled() => {
                    IndexStatus::Failed("the image was closed while it was being read".to_string())
                }
                Ok(()) => IndexStatus::Complete,
                Err(err) => IndexStatus::Failed(err.to_string()),
            };
            idx.finish(status);
            drop(guard);
        });
    if let Err(err) = spawned {
        reporter.finish(IndexStatus::Failed(format!(
            "could not start the image reader: {err}"
        )));
    }
}

impl Vfs for ImageFs {
    fn kind(&self) -> BackendKind {
        BackendKind::Image
    }

    /// Stream one directory: the partition list, or one directory of the
    /// filesystem on a partition.
    ///
    /// The `..` row goes first and unconditionally, exactly as `LocalFs` and
    /// `ArchiveFs` do it and for the same reason: it is navigation, not
    /// content, and a panel inside an image that failed to index must still
    /// have the row that gets the user out.
    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let has_parent = path.parent().is_some();

        if let Some(table) = self.inner.table.as_ref() {
            let at_root = safety::member_key(path.tail()).map(|k| k.is_empty());
            let rows = table.rows(path);
            let refused = table.refused.clone();
            // Where the filesystem is, when the tail really names a
            // partition; not found when it names something else, because a
            // table has nothing else in it.
            let elsewhere = match part::partition_number(path.tail()) {
                Ok(number) if table.get(number).is_some() => Error::InvalidPath(format!(
                    "{path}: this is partition {number}; its filesystem is at {path}#/"
                )),
                Ok(number) => missing_partition(path, table, number),
                Err(err) => err,
            };
            tokio::spawn(async move {
                if has_parent && tx.send(Ok(Entry::parent_entry())).await.is_err() {
                    return;
                }
                match at_root {
                    Ok(true) => {}
                    // A partition row is not a directory in the table's own
                    // namespace: its contents live one segment further in.
                    Ok(false) => {
                        let _ = tx.send(Err(elsewhere)).await;
                        return;
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                        return;
                    }
                }
                for row in rows {
                    if tx.send(Ok(row)).await.is_err() {
                        return;
                    }
                }
                // Everything that could be addressed is in the panel; what
                // follows is why the table is not the whole truth, if it is
                // not.
                //
                // One message and not one per refusal: the panel's listing
                // pump stops at the first error it is handed, so a second send
                // would be a reason nobody ever reads.
                if !refused.is_empty() {
                    let _ = tx.send(Err(Error::msg(refused.join("; ")))).await;
                }
            });
            return rx;
        }

        let key = safety::member_key(path.tail());
        let Some(idx) = self.inner.index.as_ref().map(Arc::clone) else {
            tokio::spawn(async move {
                if has_parent {
                    let _ = tx.send(Ok(Entry::parent_entry())).await;
                }
            });
            return rx;
        };

        tokio::spawn(async move {
            if has_parent && tx.send(Ok(Entry::parent_entry())).await.is_err() {
                return;
            }
            let key = match key {
                Ok(key) => key,
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            };

            let mut updates = idx.subscribe();
            let mut cursor = 0usize;
            let status = loop {
                let (next, rows, status) = idx.children_from(&key, cursor);
                cursor = next;
                for row in rows {
                    if tx.send(Ok(row)).await.is_err() {
                        return;
                    }
                }
                if status.is_final() {
                    break status;
                }
                if updates.changed().await.is_err() {
                    break idx.status();
                }
            };

            let report = match status {
                IndexStatus::Failed(why) => Some(Error::msg(why)),
                IndexStatus::Truncated(why) => {
                    Some(Error::msg(format!("{why}; this listing is incomplete")))
                }
                IndexStatus::Complete | IndexStatus::Building => {
                    if !key.is_empty() && !idx.is_dir(&key) {
                        Some(match idx.get(&key) {
                            Some(_) => Error::InvalidPath(format!("{key} is not a directory")),
                            None => Error::NotFound(key.clone()),
                        })
                    } else {
                        match idx.refusals() {
                            (0, _) => None,
                            (n, Some(first)) => Some(Error::msg(format!(
                                "{n} entr{} refused as unsafe to extract: {first}",
                                if n == 1 { "y was" } else { "ies were" },
                            ))),
                            (n, None) => Some(Error::msg(format!(
                                "{n} entr{} refused as unsafe to extract",
                                if n == 1 { "y was" } else { "ies were" },
                            ))),
                        }
                    }
                }
            };
            if let Some(err) = report {
                let _ = tx.send(Err(err)).await;
            }
        });

        rx
    }

    /// Metadata for one path.
    ///
    /// **Blocks** on a volume while the index has not reached the member yet.
    /// Call it from a job thread; the render path already holds the [`Entry`]
    /// the listing gave it and has no reason to ask again.
    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        let key = safety::member_key(path.tail())?;
        if key.is_empty() {
            return Ok(self.root_entry());
        }
        if self.inner.table.is_some() {
            return self.partition_row(path);
        }
        let idx = self.volume_index(path)?;
        let member = idx.stat_blocking(&key)?;
        let idx = Arc::clone(idx);
        Ok(member.to_entry(|target| {
            let base = member.parent();
            let joined = if base.is_empty() {
                target.to_string()
            } else {
                format!("{base}/{target}")
            };
            safety::normalize_member(&joined, false).is_ok_and(|p| idx.is_dir(&p))
        }))
    }

    /// Open a member for reading, streaming through an OS pipe.
    ///
    /// A member larger than memory reads fine and a reader that is dropped
    /// stops the work. Every byte is charged against what the
    /// filesystem declared, by `archive::guard_for`, which is the archive
    /// backend's bomb defence used unforked (the design I5).
    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let format = self.volume_format(path)?;
        let member = self.member(path)?;
        if matches!(member.kind, MemberKind::Dir) {
            return Err(Error::InvalidPath(format!("{path} is a directory")));
        }
        let guard = crate::vfs::archive::guard_for(&member, self.container_bytes())?;
        let region = self.inner.region.clone();
        stream::piped(move |out| {
            let mut out = safety::GuardedWriter::new(out, guard);
            let written = format.read_member(&region, &member, &mut out)?;
            out.flush().map_err(Error::Bare)?;
            Ok(written)
        })
    }

    /// Random access, offered only where it is real.
    ///
    /// An ISO member with one extent is a contiguous byte range of a local
    /// file and seeking it is a `pread`; a FAT member is not, and the
    /// forward-only viewer mode is the answer there.
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        let format = self.volume_format(path)?;
        let member = self.member(path)?;
        match format.open_member(&self.inner.region, &member)? {
            Some(handle) => Ok(handle),
            None => crate::vfs::unsupported("random-access reading in a disk image"),
        }
    }

    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn Write + Send>> {
        Err(self.refuse_write(path))
    }

    fn create_dir(&self, path: &VfsPath) -> Result<()> {
        Err(self.refuse_write(path))
    }

    fn remove(&self, path: &VfsPath) -> Result<()> {
        Err(self.refuse_write(path))
    }

    fn rename(&self, from: &VfsPath, _to: &VfsPath) -> Result<()> {
        Err(self.refuse_write(from))
    }

    /// Where a symlink member points, from the index.
    ///
    /// The target is judged here as well as reported, because the design's
    /// escape rule is this backend's threat model too: a Rock Ridge `SL` entry
    /// is chosen by whoever wrote the image.
    fn read_link(&self, path: &VfsPath) -> Result<String> {
        let member = self.member(path)?;
        let MemberKind::Symlink(target) = &member.kind else {
            return Err(Error::InvalidPath(format!("{path} is not a symbolic link")));
        };
        let target = target.trim_end_matches('\0');
        if target.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{path}: refused - an empty link target"
            )));
        }
        if target.len() > safety::MAX_LINK_TARGET_BYTES {
            return Err(Error::InvalidPath(format!(
                "{path}: refused - a link target longer than {} bytes",
                safety::MAX_LINK_TARGET_BYTES
            )));
        }
        safety::safe_link_target(&member.path, target)?;
        Ok(target.to_string())
    }

    /// What this backend offers, whatever is on it.
    ///
    /// `writable` is false on every path, so the refusal reaches the
    /// UI **before** the question rather than halfway through a copy.
    fn capabilities(&self) -> Capabilities {
        match self.inner.format {
            Some(format) => Capabilities {
                writable: false,
                ..format.capabilities()
            },
            None => Capabilities::IMAGE,
        }
    }

    /// [`Vfs::capabilities`], refined for one path.
    ///
    /// One case differs: an ISO member with more than one extent is not
    /// seekable, because `open_member` cannot hand out a contiguous window for
    /// it. **May block**, which is the method's documented contract, and
    /// nothing on the render path calls it.
    fn capabilities_for(&self, path: &VfsPath) -> Capabilities {
        let caps = self.capabilities();
        if !caps.seekable {
            return caps;
        }
        let Some(format) = self.inner.format else {
            return caps;
        };
        let Ok(member) = self.member(path) else {
            return caps;
        };
        match format.open_member(&self.inner.region, &member) {
            Ok(Some(_)) => caps,
            Ok(None) | Err(_) => Capabilities {
                seekable: false,
                random_access: false,
                ..caps
            },
        }
    }
}

#[cfg(test)]
mod tests;
