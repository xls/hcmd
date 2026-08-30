//! Archives are directories.
//!
//! > `Enter` on `foo.zip` enters it; the panel path shows `…/foo.zip#/`.
//!
//! [`ArchiveFs`] is a [`Vfs`] over **one** archive. That is the whole
//! integration: the panel, `ops`, the viewer and the clipboard already work
//! through the trait, so copying out of an archive, viewing a member,
//! `Ctrl+C`-ing a path inside one and walking it for a size total are the code
//! that already exists, unchanged. the design says the trait exists so that
//! "archives, search results and (later) remote filesystems are uniform to the
//! panel"; this milestone is where that claim is either true or it is not.
//!
//! ```text
//!                       VfsPath: [(Local, /a/b.tar.gz), (Archive, /inner/c.zip), (Archive, /x.txt)]
//!                                 └──────────┬────────┘  └──────────┬─────────┘  └───────┬──────┘
//!   ArchiveSession                     the container        materialised to        the member
//!     ├── open()  ────────────────────────────────────────────► Arc<ArchiveFs> (shared, one index)
//!     ├── materialise() ─────────► session temp cache (one file per inner archive, session-lived)
//!     └── temp_root() ───────────► removed on exit
//! ```
//!
//! # The four things this module decides
//!
//! **Detection** is by content first and extension second ([`format::detect`],
//! the design), because "TC users routinely have archives with wrong
//! extensions".
//!
//! **Safety** is [`safety`], enforced twice: a member whose name could escape
//! its destination never enters the index, and a destination is re-validated
//! when it is composed. the design calls Zip Slip "a real vulnerability,
//! not a theoretical one", so the check is this backend's and not the
//! libraries'.
//!
//! **Streaming** is [`index`] plus [`stream`]: the index is built on a worker
//! thread and the panel fills as entries arrive; members are read through an
//! OS pipe so a 4 GB member costs one pipe buffer.
//!
//! **Nesting** is [`session`]: an inner archive is extracted to a
//! session-lived temp file, cached, shared between panels, and cleaned up on
//! exit.

pub mod format;
pub mod index;
pub mod rar;
pub mod rewrite;
pub mod safety;
pub mod session;
pub mod sevenz;
pub mod stream;
pub mod tar;
pub mod zip;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::vfs::{
    BackendKind, Capabilities, Entry, EntryKind, READ_DIR_CHANNEL_DEPTH, ReadSeek, Vfs, VfsPath,
};

use format::{ArchiveFormat, FormatId, MemberEdit, MemberSource, NoProgress, WriteModel};
use index::{Index, IndexStatus, Member, MemberKind};
use safety::member_key;

pub use session::{ArchiveKey, ArchiveSession, CachedFile, RewriteGate, RewriteLimits};

/// One open archive, presented as a filesystem.
///
/// Cheap to clone: everything is behind one `Arc`, so two panels looking into
/// the same archive share its index and its temp files rather than building
/// two of each ([`ArchiveSession`] is what makes sure they get the same one).
#[derive(Clone)]
pub struct ArchiveFs {
    inner: Arc<Inner>,
}

struct Inner {
    /// The local file holding the archive's bytes.
    ///
    /// For a nested archive this is the session's extracted copy, **not** what
    /// the user sees. Everything that touches bytes uses this; everything that
    /// speaks to a human uses `display`.
    container: PathBuf,
    /// The session cache entry `container` lives in, for a **nested** archive.
    ///
    /// Held, not merely remembered: `ArchiveSession::make_room` evicts a
    /// cached member only when nothing outside the cache holds it, and
    /// "nothing" is measured with `Arc::strong_count`. Keeping only the
    /// `PathBuf` made that count read `1` while a panel was inside the inner
    /// archive, so a later extraction that crossed the cache budget deleted
    /// the container out from under it and every subsequent read failed with
    /// `ENOENT`. This is the reference that count is looking for.
    ///
    /// `Some` is also what "this archive is nested" means, which is what
    /// [`ArchiveFs::capabilities`] answers `writable: false` from.
    cached: Option<Arc<CachedFile>>,
    /// The archive's own address, as the user sees it: `/a/b.tar.gz`, or
    /// `/a/b.tar.gz#/inner/c.zip` for a nested one.
    display: VfsPath,
    format: &'static dyn ArchiveFormat,
    index: Arc<Index>,
    /// Weak, deliberately: the session owns the archives, so an `Arc` here
    /// would be a cycle and the session's temp directory would outlive the
    /// process that made it.
    session: Weak<ArchiveSession>,
}

impl std::fmt::Debug for ArchiveFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveFs")
            .field("container", &self.inner.container)
            .field("display", &self.inner.display.to_string())
            .field("format", &self.inner.format.id())
            .field("indexed", &self.inner.index.len())
            .field("status", &self.inner.index.status())
            .finish()
    }
}

impl Drop for Inner {
    /// Closing an archive stops its index build.
    ///
    /// The format's `index` call checks the sink after every member, so a
    /// 40 GB `.tar.gz` that nobody is looking at any more stops decompressing
    /// at the next entry rather than at the last one.
    fn drop(&mut self) {
        self.index.cancel();
        // And whatever the format was holding for this container: an open
        // decoder is memory, and nobody is going to read through it again.
        self.format.release(&self.container);
    }
}

impl ArchiveFs {
    /// Open `container`, detect its format, and start indexing it.
    ///
    /// Returns as soon as the format is known: the index is built on a worker
    /// thread and the panel fills from it. Prefer
    /// [`ArchiveSession::open`], which caches - this is the constructor it
    /// calls, exposed for tests and for a caller that genuinely wants a
    /// private instance.
    pub fn open(
        session: &Arc<ArchiveSession>,
        container: impl Into<PathBuf>,
        display: VfsPath,
    ) -> Result<Self> {
        let container = container.into();
        let format = format::detect(&container)?.backend();
        Ok(Self::with_format(session, container, display, format))
    }

    /// [`ArchiveFs::open`] for a **nested** archive, whose bytes are the
    /// session's extracted copy of a member of the archive above it.
    ///
    /// The `Arc<CachedFile>` is held for as long as this archive is open, and
    /// its presence is what makes this archive read-only: see
    /// [`ArchiveFs::capabilities`].
    pub fn open_nested(
        session: &Arc<ArchiveSession>,
        cached: Arc<CachedFile>,
        display: VfsPath,
    ) -> Result<Self> {
        let container = cached.path().to_path_buf();
        let format = format::detect(&container)?.backend();
        let mut fs = Self::with_format(session, container, display, format);
        // The only mutation of `Inner` after construction, and it happens
        // before the value is shared: `with_format` has just made the `Arc`
        // and nothing else holds it yet.
        if let Some(inner) = Arc::get_mut(&mut fs.inner) {
            inner.cached = Some(cached);
        }
        Ok(fs)
    }

    /// [`ArchiveFs::open`] with the format already decided, for a caller that
    /// has sniffed it or a test that is exercising one.
    pub fn with_format(
        session: &Arc<ArchiveSession>,
        container: impl Into<PathBuf>,
        display: VfsPath,
        format: &'static dyn ArchiveFormat,
    ) -> Self {
        let container = container.into();
        let idx = Arc::new(Index::new());
        let inner = Arc::new(Inner {
            container: container.clone(),
            cached: None,
            display,
            format,
            index: Arc::clone(&idx),
            session: Arc::downgrade(session),
        });
        spawn_index(container, format, idx);
        Self { inner }
    }

    /// Which format this archive turned out to be.
    pub fn format(&self) -> FormatId {
        self.inner.format.id()
    }

    /// How this format accepts writes, for the gates.
    pub fn write_model(&self) -> WriteModel {
        self.inner.format.write_model()
    }

    /// The archive's own address, as the user sees it.
    pub fn display_path(&self) -> &VfsPath {
        &self.inner.display
    }

    /// The local file the bytes are really in - the session's extracted copy,
    /// for a nested archive.
    pub fn container(&self) -> &Path {
        &self.inner.container
    }

    /// The shared index, for a caller that wants to watch it fill.
    pub fn index(&self) -> &Arc<Index> {
        &self.inner.index
    }

    /// Block until the index is complete. Tests and `Alt+F6`, which needs the
    /// whole listing before it can report a total.
    pub fn wait_for_index(&self) -> IndexStatus {
        self.inner.index.wait_until_final()
    }

    /// The member at `path`, waiting for the index to reach it if necessary.
    fn member(&self, path: &VfsPath) -> Result<Member> {
        let key = member_key(path.tail())?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{path}: the archive root is a directory"
            )));
        }
        self.inner.index.stat_blocking(&key)
    }

    /// Extract `member` into the session cache and return the file it landed
    /// in. Used for `.rar`, whose library will not stream, and for a nested
    /// archive.
    pub fn materialise(&self, member: &Member) -> Result<Arc<CachedFile>> {
        let session = self
            .inner
            .session
            .upgrade()
            .ok_or_else(|| Error::msg("the archive session has been closed".to_string()))?;
        session.materialise(self, member)
    }

    /// Extract one member to `dest` under the byte caps.
    ///
    /// The session cache's own extraction, kept beside `open_read`'s so that
    /// there is exactly one answer to "how many bytes may this member
    /// produce". A streaming format is guarded as it writes; `.rar` is the one
    /// that will not stream, so `unrar` writes the file and the size it
    /// actually produced is charged afterwards - which still stops the lie
    /// from being cached and handed on, and still deletes what it wrote.
    pub(crate) fn extract_guarded(&self, member: &Member, dest: &Path) -> Result<u64> {
        let mut guard = guard_for(member, container_bytes(&self.inner.container))?;
        match self.inner.format.member_source() {
            MemberSource::Stream => {
                let file = std::fs::File::create(dest).map_err(|e| Error::io(dest, e))?;
                let mut out = safety::GuardedWriter::new(std::io::BufWriter::new(file), guard);
                let written =
                    self.inner
                        .format
                        .read_member(&self.inner.container, member, &mut out)?;
                out.flush().map_err(|e| Error::io(dest, e))?;
                Ok(written)
            }
            MemberSource::Materialise => {
                self.inner
                    .format
                    .extract_member(&self.inner.container, member, dest)?;
                // What the library produced, not what it said it produced, and
                // `symlink_metadata` because the question is what is *at*
                // `dest` rather than what can be reached through it.
                //
                // `unrar` reproduces a member whose attributes say `S_IFLNK`
                // as a real symbolic link, and its own escape check measures
                // the `..` in the target against the depth of the path we
                // handed it rather than against the archive's root - so a
                // crafted `.rar` plants a link to any readable file on the
                // machine in this session's cache, and every later read of the
                // cached "member" reads that file instead. Nothing but a
                // regular file is a member's contents; anything else is
                // removed and refused.
                let meta = std::fs::symlink_metadata(dest).map_err(|e| Error::io(dest, e))?;
                if !meta.file_type().is_file() {
                    if meta.is_dir() {
                        let _ = std::fs::remove_dir_all(dest);
                    } else {
                        let _ = std::fs::remove_file(dest);
                    }
                    return Err(Error::InvalidPath(format!(
                        "{}: refused - the archive reader produced {} where a \
 file's contents were expected",
                        member.path,
                        if meta.file_type().is_symlink() {
                            "a symbolic link"
                        } else if meta.is_dir() {
                            "a directory"
                        } else {
                            "something that is not a regular file"
                        }
                    )));
                }
                let actual = meta.len();
                if let Err(err) = guard.accept(actual) {
                    let _ = std::fs::remove_file(dest);
                    return Err(err);
                }
                Ok(actual)
            }
        }
    }

    /// True when this archive's bytes are the session's extracted copy of a
    /// member of another archive (the nesting).
    pub fn is_nested(&self) -> bool {
        self.inner.cached.is_some()
    }

    /// the up-front refusal for a write that cannot be delivered.
    ///
    /// A nested archive is opened from a **copy**: `session.materialise`
    /// extracted the inner container into the session cache and this backend
    /// reads and writes that file. A write therefore lands on a temp file that
    /// `ArchiveSession::drop` deletes on exit, and the outer archive - the one
    /// the user is actually looking at - is never touched. Nothing propagates
    /// it back, and a `Move` into such a destination would delete the local
    /// source after a copy whose bytes exist only in that temp file
    /// (the "the delete happening only after a successful copy"
    /// is not satisfied by a copy that goes nowhere).
    ///
    /// So it is refused here, before anything is read: "a read-only archive
    /// backend causes `F5` *into* it to be refused up front with a clear
    /// message rather than failing halfway through a copy", and
    /// [`ArchiveFs::capabilities`] reports `writable: false` so the refusal
    /// reaches the UI before the question rather than after it.
    fn refuse_if_nested(&self) -> Result<()> {
        if self.inner.cached.is_none() {
            return Ok(());
        }
        Err(Error::msg(format!(
            "{}: an archive inside another archive is opened from a copy in \
             this session's cache, so writing to it would change nothing in \
 {}. Extract it, change it, and put it back",
            self.inner.display,
            self.inner
                .display
                .parent()
                .map(|p| p.to_string())
                .unwrap_or_else(|| self.inner.display.to_string()),
        )))
    }

    /// The entry for the archive's own root: a directory named after the
    /// container.
    fn root_entry(&self) -> Entry {
        let name = self
            .inner
            .display
            .file_name()
            .unwrap_or_else(|| self.inner.format.id().label().to_string());
        Entry {
            is_hidden: name.starts_with('.'),
            ..Entry::dir(name)
        }
    }
}

/// Make sure an index build always ends in a final status, however it ends.
///
/// Held by the indexing thread for the length of the build. Its `Drop` runs on
/// the ordinary path (where the status is already final and this does nothing)
/// and on an unwind (where it is the only thing that will), so no waiter is
/// ever left blocked on a build that is no longer running.
struct FinishOnDrop {
    index: Arc<Index>,
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.index.finish(IndexStatus::Failed(
            "the archive reader stopped unexpectedly; the listing is incomplete".to_string(),
        ));
    }
}

/// Start the index build on its own thread.
///
/// A plain thread rather than the tokio blocking pool: this may run for as
/// long as it takes to decompress the container, and a blocking-pool slot held
/// for minutes starves everything else that needs one (`read_dir`, `stat`,
/// every file operation).
fn spawn_index(container: PathBuf, format: &'static dyn ArchiveFormat, idx: Arc<Index>) {
    let reporter = Arc::clone(&idx);
    let spawned = std::thread::Builder::new()
        .name("hcmd-archive-index".to_string())
        .spawn(move || {
            // Everything that waits on this index - `stat_blocking`,
            // `wait_until_final`, and the `read_dir` task that streams the
            // listing - loops until the status is final. If this thread were
            // ever to end without setting one, every one of them would block
            // for the life of the process: the panel would sit on its `..` row
            // and never come back, with no error to show.
            //
            // A returned `Err` is handled below. This guard is for the way out
            // that no `Result` describes: a panic. Archive data is
            // attacker-controlled and the whole crate is
            // written not to panic on it, but "the indexer is correct" is not
            // something a hung panel can be made to depend on. `finish` keeps
            // the first final status it is given, so on the ordinary path this
            // drop is a no-op.
            let guard = FinishOnDrop {
                index: Arc::clone(&idx),
            };
            let outcome = {
                let mut sink = index::Builder::new(Arc::clone(&idx), format.backslash_separators());
                format.index(&container, &mut sink)
            };
            let status = match outcome {
                Ok(()) if idx.cancelled() => IndexStatus::Failed(
                    "the archive was closed while it was being read".to_string(),
                ),
                Ok(()) => IndexStatus::Complete,
                Err(err) => IndexStatus::Failed(err.to_string()),
            };
            idx.finish(status);
            drop(guard);
        });
    if let Err(err) = spawned {
        // No thread means no listing, and a panel that waits forever is worse
        // than one that says why.
        reporter.finish(IndexStatus::Failed(format!(
            "could not start the archive reader: {err}"
        )));
    }
}

impl Vfs for ArchiveFs {
    fn kind(&self) -> BackendKind {
        BackendKind::Archive
    }

    /// Stream one directory of the archive, filling as the index does.
    ///
    ///
    /// The `..` row goes first and unconditionally, exactly as [`LocalFs`]
    /// does it and for the same reason: it is navigation, not content, and a
    /// panel inside an archive that failed to index must still have the row
    /// that gets the user out.
    ///
    /// [`LocalFs`]: crate::vfs::LocalFs
    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let key = member_key(path.tail());
        let has_parent = path.parent().is_some();
        let idx = Arc::clone(&self.inner.index);

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
                // A `watch` cannot lose a wakeup: an update between the read
                // above and this await bumps the version and returns at once.
                if updates.changed().await.is_err() {
                    break idx.status();
                }
            };

            // Everything that arrived is already in the panel; what follows is
            // the reason the listing is not the whole truth, if it is not.
            let report = match status {
                IndexStatus::Failed(why) => Some(Error::msg(why)),
                IndexStatus::Truncated(why) => {
                    Some(Error::msg(format!("{why}; this listing is incomplete")))
                }
                IndexStatus::Complete | IndexStatus::Building => {
                    if !key.is_empty() && !idx.is_dir(&key) {
                        // Nothing was listed, and the reason matters: a member
                        // that is a file is a different mistake from one that
                        // is not there at all.
                        Some(match idx.get(&key) {
                            Some(_) => Error::InvalidPath(format!("{key} is not a directory")),
                            None => Error::NotFound(key.clone()),
                        })
                    } else {
                        match idx.refusals() {
                            (0, _) => None,
                            // The recorded reason already says why, so the
                            // generic half below is used only when there is no
                            // recorded reason to show instead.
                            (n, Some(first)) => Some(Error::msg(format!(
                                "{n} entr{} refused as unsafe to extract: {first}",
                                if n == 1 { "y was" } else { "ies were" },
                            ))),
                            (n, None) => Some(Error::msg(format!(
                                "{n} entr{} refused as unsafe to extract \
",
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

    /// Metadata for one member.
    ///
    /// **Blocks** while the index has not reached the member yet, which on a
    /// compressed tar can be as long as decompressing the rest of it. Call it
    /// from a job thread; the render path already holds the [`Entry`] the
    /// listing gave it and has no reason to ask again.
    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        let key = member_key(path.tail())?;
        if key.is_empty() {
            return Ok(self.root_entry());
        }
        let member = self.inner.index.stat_blocking(&key)?;
        let idx = Arc::clone(&self.inner.index);
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

    /// Open a member for reading.
    ///
    /// Streams for every format that can stream, through an OS pipe: a member
    /// larger than memory reads fine and a reader that is dropped stops the
    /// work. A `.rar` member is materialised into the session
    /// cache first, because `unrar` will not stream - see [`rar`].
    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let member = self.member(path)?;
        if matches!(member.kind, MemberKind::Dir) {
            return Err(Error::InvalidPath(format!("{path} is a directory")));
        }
        match self.inner.format.member_source() {
            MemberSource::Stream => {
                let format = self.inner.format;
                let container = self.inner.container.clone();
                let guard = guard_for(&member, container_bytes(&container))?;
                stream::piped(move |out| {
                    // Guarded on the *producing* side, so a member that lies
                    // about its size stops being decompressed rather than
                    // stopping being read: the bytes past the cap are never
                    // written, never buffered and never paid for.
                    //
                    let mut out = safety::GuardedWriter::new(out, guard);
                    let written = format.read_member(&container, &member, &mut out)?;
                    out.flush().map_err(Error::Bare)?;
                    Ok(written)
                })
            }
            MemberSource::Materialise => {
                let cached = self.materialise(&member)?;
                // `O_NOFOLLOW`: see `safety::open_no_follow`. The cache holds
                // what somebody else's library wrote, and reading *through*
                // what it wrote is the whole of the `.rar` symlink escape.
                let file = safety::open_no_follow(cached.path())?;
                Ok(Box::new(file))
            }
        }
    }

    /// Random access, offered only where it is real.
    ///
    /// A streamed member has none: the forward-only viewer mode is
    /// the answer, and `Capabilities::seekable` says so, so the viewer takes
    /// it without having to discover it. A materialised member is a file on
    /// disk and is seekable in the ordinary way.
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        match self.inner.format.member_source() {
            MemberSource::Stream => crate::vfs::unsupported("random-access reading in an archive"),
            MemberSource::Materialise => {
                let member = self.member(path)?;
                let cached = self.materialise(&member)?;
                let file = safety::open_no_follow(cached.path())?;
                Ok(Box::new(file))
            }
        }
    }

    /// Open a member for writing.
    ///
    /// The bytes are buffered into a session temp file and committed into the
    /// archive by **`flush`**, which is the completion signal `ops::copy`
    /// already sends ("the final flush completes before the job can report
    /// done"). Three consequences, all of them wanted:
    ///
    /// * a writer dropped without a flush - a cancelled copy, a failed read -
    ///   changes nothing, so the "no half-written destination"
    ///   holds for an archive member as it does for a file;
    /// * the commit's failure is reported by `flush`, where a caller is
    ///   already checking, rather than by `drop`, where nobody can;
    /// * a second `flush` is a no-op and a write after one is an error, rather
    ///   than a silent second commit.
    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn Write + Send>> {
        self.refuse_if_nested()?;
        if !self.inner.format.write_model().writable() {
            // refused up front, with the reason, rather than
            // halfway through a copy.
            return Err(Error::msg(format!(
                "{}: a {} archive cannot be written to",
                self.inner.display,
                self.inner.format.id()
            )));
        }
        let key = member_key(path.tail())?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{path}: the archive root is a directory"
            )));
        }
        let session = self
            .inner
            .session
            .upgrade()
            .ok_or_else(|| Error::msg("the archive session has been closed".to_string()))?;
        let temp = session.scratch_file(&key)?;
        let file = std::fs::File::create(&temp).map_err(|e| Error::io(&temp, e))?;
        Ok(Box::new(MemberWriter {
            inner: Arc::clone(&self.inner),
            member_path: key,
            temp,
            file: Some(file),
            committed: false,
        }))
    }

    fn create_dir(&self, path: &VfsPath) -> Result<()> {
        let key = member_key(path.tail())?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!("{path} already exists")));
        }
        self.edit(&[MemberEdit::PutDir {
            member_path: key,
            mode: 0o755,
        }])
    }

    fn remove(&self, path: &VfsPath) -> Result<()> {
        let key = member_key(path.tail())?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{path}: an archive is deleted from the outside, not from within"
            )));
        }
        self.edit(&[MemberEdit::Remove { member_path: key }])
    }

    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let from_key = member_key(from.tail())?;
        let to_key = member_key(to.tail())?;
        if from_key.is_empty() || to_key.is_empty() {
            return Err(Error::InvalidPath(format!(
                "{from}: the archive root cannot be renamed from inside it"
            )));
        }
        self.edit(&[MemberEdit::Rename {
            from: from_key,
            to: to_key,
        }])
    }

    /// The format's, with one correction: a **nested** archive is read-only
    /// whatever its format says.
    ///
    /// See [`ArchiveFs::refuse_if_nested`]. the design makes `Capabilities`
    /// "what the UI consults before offering an operation", so a write that
    /// this backend cannot deliver has to be absent from the answer here and
    /// not merely refused later.
    fn capabilities(&self) -> Capabilities {
        let caps = self.inner.format.capabilities();
        if self.inner.cached.is_some() {
            return Capabilities {
                writable: false,
                ..caps
            };
        }
        caps
    }

    /// Where a symlink member points, from the index.
    ///
    /// The target is read from the **index**, which is where each format put
    /// it: a tar's is the header's link-name field, a zip's is the member's
    /// contents. The copy engine cannot know which, and does not have to -
    /// that is what this method is for.
    ///
    /// The target is judged here as well as reported, because the design's
    /// escape rule is this backend's threat model: `a/evil -> /etc` followed
    /// by `a/evil/passwd` is the attack, and refusing to create the link is
    /// the half of it that cannot be tricked.
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
                "{path}: refused - a link target longer than {} bytes \
",
                safety::MAX_LINK_TARGET_BYTES
            )));
        }
        safety::safe_link_target(&member.path, target)?;
        Ok(target.to_string())
    }
}

impl ArchiveFs {
    /// Apply edits to the container, refusing what the format cannot do and
    /// what the design gates.
    ///
    /// The index is invalidated afterwards, because every locator in it may
    /// have moved: a rewrite renames a whole new container over the old one.
    fn edit(&self, edits: &[MemberEdit]) -> Result<()> {
        self.refuse_if_nested()?;
        let model = self.inner.format.write_model();
        if !model.writable() {
            return Err(Error::msg(format!(
                "{}: a {} archive cannot be written to",
                self.inner.display,
                self.inner.format.id()
            )));
        }
        if model.needs_rewrite_gates() {
            // A backstop, not the gate: the design wants the refusal and
            // the warning in front of the *user*, before anything is touched,
            // and that is the dialog's job. This is what stops a caller that
            // skipped it from silently rewriting a gigabyte.
            let limits = self
                .inner
                .session
                .upgrade()
                .map(|s| s.limits())
                .unwrap_or_default();
            let size = std::fs::metadata(&self.inner.container)
                .map(|m| m.len())
                .unwrap_or(0);
            let temp = self
                .inner
                .session
                .upgrade()
                .map(|s| s.temp_root().to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            limits.check(size, &temp, &self.inner.display)?;
        }
        let outcome = self
            .inner
            .format
            .apply(&self.inner.container, edits, &mut NoProgress);
        if outcome.is_ok() {
            // Every locator addressed a position in the container that has
            // just been replaced, so the index is thrown away and rebuilt
            // rather than patched: a patch would have to know what each format
            // did to the layout, which is exactly the knowledge this module
            // does not have.
            self.inner.index.invalidate();
            spawn_index(
                self.inner.container.clone(),
                self.inner.format,
                Arc::clone(&self.inner.index),
            );
        }
        outcome
    }
}

/// The writer [`ArchiveFs::open_write`] hands out.
///
/// Buffers into a session temp file; `flush` is the commit.
struct MemberWriter {
    inner: Arc<Inner>,
    member_path: String,
    temp: PathBuf,
    file: Option<std::fs::File>,
    committed: bool,
}

impl Write for MemberWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.committed {
            return Err(std::io::Error::other(format!(
                "{}: the member has already been committed to the archive",
                self.member_path
            )));
        }
        match self.file.as_mut() {
            Some(file) => file.write(buf),
            None => Err(std::io::Error::other("the member writer is closed")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.committed {
            return Ok(());
        }
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.flush()?;
        let mtime = file.metadata().ok().and_then(|m| m.modified().ok());
        drop(file);

        let edit = MemberEdit::Put {
            member_path: self.member_path.clone(),
            source: self.temp.clone(),
            mode: 0o644,
            mtime,
        };
        let fs = ArchiveFs {
            inner: Arc::clone(&self.inner),
        };
        fs.edit(&[edit]).map_err(std::io::Error::other)?;
        self.committed = true;
        let _ = std::fs::remove_file(&self.temp);
        Ok(())
    }
}

impl Drop for MemberWriter {
    fn drop(&mut self) {
        // No commit, no change, no leftovers. A cancelled copy leaves the
        // archive exactly as it was.
        self.file.take();
        let _ = std::fs::remove_file(&self.temp);
    }
}

/// The [`safety::Guard`] one member's bytes are read under.
///
/// the bomb defence has two halves and this is the one that needs
/// no destination: whatever the member's header declared is a *claim*, and
/// every byte that comes out is charged against it. A `.zip` entry declaring
/// sixteen bytes over a deflate stream that never ends is stopped sixteen bytes
/// in, wherever the read came from - `ops`' copy, the viewer, the session
/// cache.
///
/// A size is declared only for something that has contents. A directory has
/// none and never reaches here; a symlink's "size" is its target's length in
/// some formats and zero in others, so it is left unclaimed rather than guessed
/// at, and only the absolute ceiling applies.
pub(crate) fn guard_for(member: &Member, container_bytes: u64) -> Result<safety::Guard> {
    let declared = match member.kind {
        MemberKind::File | MemberKind::Hardlink(_) => Some(member.size),
        MemberKind::Dir | MemberKind::Symlink(_) | MemberKind::Other => None,
    };
    // The defaults, deliberately: the `[archive]` table has three
    // keys and inventing a fourth would document a file the spec does not
    // define (the design, and `safety::ExtractLimits`).
    let limits = safety::ExtractLimits::default();
    // The **declared** size, before a byte is produced. Charging bytes as they
    // arrive stops a member that lies about being small; it does not stop one
    // that is honest about being enormous, and a 5 MB container whose single
    // member truthfully declares four terabytes is the decompression bomb
    // the design names. Refusing on the claim costs nothing and is the only
    // check that can arrive before the disk starts filling.
    if let Some(declared) = declared
        && let Err(over) = limits.check_declared(declared, container_bytes)
    {
        return Err(Error::InvalidPath(format!(
            "{}: refused - {}",
            member.path,
            over.reason()
        )));
    }
    Ok(safety::Guard::for_member(&member.path, declared, limits))
}

/// How big the container is, for the ratio half of [`guard_for`]'s check.
///
/// `0` when it cannot be measured, which is what
/// [`safety::ExtractLimits::check_declared`] reads as "unknown" and skips the
/// ratio test on rather than guessing.
fn container_bytes(container: &Path) -> u64 {
    std::fs::metadata(container).map(|m| m.len()).unwrap_or(0)
}

/// The entry kind a member of this kind presents as, for callers outside this
/// module that hold a [`Member`] rather than an [`Entry`].
pub fn entry_kind(member: &Member) -> EntryKind {
    match &member.kind {
        MemberKind::Dir => EntryKind::Dir,
        MemberKind::Symlink(_) => EntryKind::Symlink { to_dir: false },
        MemberKind::File | MemberKind::Hardlink(_) => EntryKind::File,
        MemberKind::Other => EntryKind::Other,
    }
}

#[cfg(test)]
mod tests;
