//! The virtual filesystem.
//!
//! Everything that presents as a directory goes through [`Vfs`], so archives,
//! search results and (later) remote filesystems are uniform to the panel.
//!
//! # Deviations from the design, recorded deliberately
//!
//! * `read_dir` is written in the design as
//!   `fn read_dir(&self, path: &VfsPath) -> BoxStream<Result<Entry>>`.
//!   `BoxStream` is `futures::stream::BoxStream`, and `futures` is not on the
//!   the design dependency table. We use
//!   [`tokio::sync::mpsc::Receiver<Result<Entry>>`] instead. The semantics
//!   the design actually requires are all preserved: the listing streams in
//!   incrementally, the panel renders what has arrived so far, and dropping the
//!   receiver cancels the walk (the producing task's `send` fails and it stops).
//! * `stat` is written as `-> Result<Metadata>`. There is no `Metadata` type in
//!   the spec and [`Entry`] already carries every field the panel needs, so
//!   `stat` returns `Result<Entry>`.
//! * Per the design, [`Capabilities`] bakes in no POSIX assumptions: `rename`
//!   is allowed to be non-atomic, `has_directories` may be false (prefixes
//!   only), and `paged_listing` may be true.
//! * The **remote** backends live in [`crate::remote`] and not here, because
//!   the tree names one directory for all of `Ctrl+F`: the connect
//!   dialog, the host book and both protocols. [`crate::remote::RemoteFs`] is
//!   a [`Vfs`] like any other and reaches the panel through the same router.
//!

pub mod archive;
pub mod image;
pub mod list;
pub mod local;
pub mod router;
pub mod users;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::input::Milestone;

pub use archive::{ArchiveFs, ArchiveSession};
pub use image::ImageFs;
pub use list::ListFs;
pub use local::LocalFs;
pub use router::VfsRouter;
pub use users::{group_name, owner_name};

/// Which backend a [`VfsPath`] segment is addressed by.
///
/// v0.1 shipped `Local` and `List`; v0.5 adds `Archive`, and
/// v0.65 adds `Remote`. Adding a variant is a source-compatible
/// change for everything that matches exhaustively on it inside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackendKind {
    /// The real local filesystem.
    Local,
    /// A synthetic listing over a fixed set of paths: search results, `Ctrl+B`
    /// branch view.
    List,
    /// The inside of one archive. "Archives are directories":
    /// a segment of this kind is a path *within* the archive addressed by
    /// every segment above it, so `a.tar.gz#/inner/b.zip#/file.txt` is three
    /// segments and two archives.
    Archive,
    /// One live remote connection.
    ///
    /// The id and not the protocol, because two tabs can be on two different
    /// hosts at once and a path has to say which: `BackendKind::Remote(3)` and
    /// `BackendKind::Remote(4)` are different namespaces, which is what makes
    /// [`VfsPath::starts_with`] answer correctly between them.
    ///
    Remote(crate::remote::RemoteId),
    /// The inside of one disk image. "A `.iso` and a `.img`
    /// are containers you browse into."
    ///
    /// Two segments of this kind can stack, and mean different things when
    /// they do: the first names a **partition** and the second a path on the
    /// filesystem that is on it, which is what the design means by "a
    /// partition is a segment, not a directory inside one". An image with no
    /// partition table has one segment and it is the filesystem's root.
    Image,
}

impl BackendKind {
    /// The stable string id, used in display and in state files.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::List => "list",
            Self::Archive => "archive",
            Self::Remote(_) => "remote",
            Self::Image => "image",
        }
    }

    /// What a backend of this kind can do, without needing an instance.
    ///
    /// [`ListFs`] uses this to delegate: a synthetic listing is exactly as
    /// writable, as seekable and as far away as the backends its rows really
    /// live on.
    pub const fn capabilities(&self) -> Capabilities {
        match self {
            Self::Local => Capabilities::LOCAL,
            Self::List => Capabilities::READ_ONLY_LIST,
            // Conservative, and deliberately so: the honest answer depends on
            // which format the archive turned out to be (`.rar` is read-only,
            // the design), and that needs the open [`ArchiveFs`]. This is
            // the answer for a row in a `ListFs` that happens to live inside an
            // archive, where no instance is at hand - under-promising there
            // costs a refusal the user can retry, over-promising costs a copy
            // that fails halfway.
            Self::Archive => Capabilities::ARCHIVE_UNKNOWN,
            // The same reasoning one line up, for a connection that is not at
            // hand: under-promising costs a refusal the user can retry,
            // over-promising costs a copy that fails halfway.
            Self::Remote(_) => Capabilities::REMOTE_UNKNOWN,
            // Read-only is not conservative here and is not a placeholder for
            // a better answer later: it is the feature.
            // `seekable` is the conservative half - an ISO member is seekable
            // and a FAT member is not, and this is the answer for a path whose
            // backend is not at hand.
            Self::Image => Capabilities::IMAGE,
        }
    }
}

/// A stack of `(backend, path)` segments, so nesting is representable and
/// displayable: `/home/t/a.tar.gz#/inner/b.zip#/file.txt`.
///
/// **Non-empty by construction, not by invariant.** The outermost segment is a
/// field of its own and everything nested inside it is the `Vec` beside it, so
/// there is no empty stack for an accessor to invent an answer for.
/// [`VfsPath::backend`] and [`VfsPath::tail`] are total because of it, and that
/// is the point rather than a tidiness: while the stack was one `Vec` those two
/// answered `BackendKind::Local` and `/` for an empty one, so a path that had
/// lost its segments became **the local filesystem root** in a program that
/// routes deletes and moves by path. There is now no way to build the value
/// that had that answer.
///
/// # Filenames and UTF-8
///
/// Segments hold `PathBuf`, so the stored path is byte-exact. [`Entry::name`]
/// is a `String`, so a filename that is not valid UTF-8 is lossy-converted for
/// display and for [`VfsPath::join`]. That is a known limitation of the v0.1
/// contract;.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VfsPath {
    /// The outermost segment. Always present; this is the whole of the
    /// non-empty guarantee.
    head: (BackendKind, PathBuf),
    /// Everything nested inside [`VfsPath::head`], outermost first.
    rest: Vec<(BackendKind, PathBuf)>,
}

impl VfsPath {
    /// A path with a single segment.
    pub fn new(backend: BackendKind, path: impl Into<PathBuf>) -> Self {
        Self {
            head: (backend, path.into()),
            rest: Vec::new(),
        }
    }

    /// A single `Local` segment. The common case.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::new(BackendKind::Local, path)
    }

    /// The local filesystem root, `/`.
    pub fn local_root() -> Self {
        Self::local("/")
    }

    /// Every segment, outermost first.
    ///
    /// Borrowed and collected rather than sliced, because the segments are
    /// deliberately not contiguous: the outermost one lives in a field of its
    /// own, which is what makes the stack non-empty by construction. The
    /// vector is one to three pointers on every path this program builds, and
    /// the callers that only want the innermost segment or the backend have
    /// [`VfsPath::innermost`] and [`VfsPath::backend`], which allocate nothing.
    pub fn segments(&self) -> Vec<&(BackendKind, PathBuf)> {
        std::iter::once(&self.head)
            .chain(self.rest.iter())
            .collect()
    }

    /// How many segments this path has. Never zero.
    pub fn depth(&self) -> usize {
        self.rest.len().saturating_add(1)
    }

    /// The innermost segment: the backend that services operations on this
    /// path, and the path in that backend's own namespace.
    ///
    /// Total, with no fallback, because there is always a segment.
    pub fn innermost(&self) -> &(BackendKind, PathBuf) {
        self.rest.last().unwrap_or(&self.head)
    }

    /// [`VfsPath::innermost`], for the methods that rewrite it in place.
    fn innermost_mut(&mut self) -> &mut (BackendKind, PathBuf) {
        self.rest.last_mut().unwrap_or(&mut self.head)
    }

    /// The `index`th segment from the outside, or `None` past the end.
    fn segment(&self, index: usize) -> Option<&(BackendKind, PathBuf)> {
        match index.checked_sub(1) {
            None => Some(&self.head),
            Some(inner) => self.rest.get(inner),
        }
    }

    /// The backend the innermost segment belongs to - the one that will service
    /// operations on this path.
    pub fn backend(&self) -> BackendKind {
        self.innermost().0
    }

    /// The innermost segment's path, in that backend's own namespace.
    pub fn tail(&self) -> &Path {
        self.innermost().1.as_path()
    }

    /// `Some` only for a path that is exactly one `Local` segment - that is,
    /// something the kernel can be handed. Anything nested (an archive member,
    /// a remote file) returns `None`, which is what the design relies on to
    /// refuse executing a file that has no path.
    pub fn local_path(&self) -> Option<&Path> {
        match (&self.head, self.rest.is_empty()) {
            ((BackendKind::Local, path), true) => Some(path.as_path()),
            _ => None,
        }
    }

    /// True when this path has nowhere further up to go.
    pub fn is_root(&self) -> bool {
        self.rest.is_empty() && self.head.1.parent().is_none()
    }

    /// The innermost path component, lossily decoded.
    ///
    /// `None` at the root of the outermost backend.
    pub fn file_name(&self) -> Option<String> {
        self.tail()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// A short label for a tab or a title: the file name, or the whole path
    /// when there is no file name (the root).
    pub fn display_title(&self) -> String {
        self.file_name().unwrap_or_else(|| self.to_string())
    }

    /// Push a nested segment: entering an archive, connecting a remote.
    pub fn push_segment(&mut self, backend: BackendKind, path: impl Into<PathBuf>) {
        self.rest.push((backend, path.into()));
    }

    /// [`VfsPath::push_segment`], by value.
    pub fn with_segment(mut self, backend: BackendKind, path: impl Into<PathBuf>) -> Self {
        self.push_segment(backend, path);
        self
    }

    /// Pop the innermost segment: leaving an archive, disconnecting a remote.
    ///
    /// Returns the popped `(backend, path)`, or `None` when the path is a
    /// single segment - the outermost one is a field and there is no operation
    /// that removes it, which is how the stack stays non-empty.
    ///
    /// Note that this leaves the outer segment pointing *at the container*
    /// (`/a/b.tar.gz`), not at the directory holding it; [`VfsPath::parent`] is
    /// what walks all the way out.
    pub fn pop_segment(&mut self) -> Option<(BackendKind, PathBuf)> {
        self.rest.pop()
    }

    /// A child of this path within the same segment.
    ///
    /// `name` is treated as a single component; a `name` containing a separator
    /// is joined as written, which is what makes `join` usable for a relative
    /// path typed on the command line.
    ///
    /// **That is safe only for a name this machine chose.** `Path::join` with
    /// an absolute argument discards the base entirely, so a row named
    /// `/etc/cron.d/pwn` handed over by an archive, an image or a server is a
    /// way out of the destination the user picked. A name from any of those
    /// goes through [`PlainName`] and [`VfsPath::join_name`], which cannot be
    /// reached without the check.
    pub fn join(&self, name: impl AsRef<Path>) -> Self {
        let mut next = self.clone();
        let tail = next.innermost_mut();
        tail.1 = tail.1.join(name);
        next
    }

    /// A child of this path named by a [`PlainName`].
    ///
    /// The door for a name that did not come from this machine's own
    /// filesystem. It takes the receipt rather than the string, so "did you
    /// remember the guard" is answered by the signature instead of by a
    /// reviewer.
    pub fn join_name(&self, name: &PlainName) -> Self {
        self.join(name.as_str())
    }

    /// One level up.
    ///
    /// Within a segment this is the parent directory. At the root of a nested
    /// segment it pops the segment and returns the *parent of the container* -
    /// leaving `/a/b.tar.gz#/` lands on `/a`, not on the archive file itself.
    /// `None` only at the root of the outermost backend.
    pub fn parent(&self) -> Option<Self> {
        let (kind, tail) = self.innermost();
        // `Path::new("a").parent()` is `Some("")`, which is not a directory any
        // backend can open. Treat an empty parent of a non-empty path as "no
        // parent" so a relative path cannot walk into nothing.
        let up = tail.parent().filter(|up| !up.as_os_str().is_empty());
        if let Some(up) = up {
            let (kind, up) = (*kind, up.to_path_buf());
            let mut next = self.clone();
            *next.innermost_mut() = (kind, up);
            return Some(next);
        }
        let mut next = self.clone();
        // `None` here is the outermost root, which is where the walk stops.
        next.rest.pop()?;
        next.parent()
    }

    /// Replace the innermost segment's path, keeping its backend and everything
    /// outside it.
    pub fn with_tail(&self, path: impl Into<PathBuf>) -> Self {
        let mut next = self.clone();
        next.innermost_mut().1 = path.into();
        next
    }

    /// The root of the innermost segment's backend.
    pub fn segment_root(&self) -> Self {
        self.with_tail("/")
    }

    /// Is this path `other`, or somewhere beneath it?
    ///
    /// Compared **component by component**, not as text, so `/ab` is not
    /// beneath `/a`. The size cache's subtree invalidation
    /// turns on exactly that distinction, and a `str::starts_with` would get
    /// it wrong.
    ///
    /// Every segment above the one `other` ends at must match **exactly**, and
    /// that last one is compared by components. Entering a container is
    /// descending, so `/a/b.tar#/x` *is* beneath `/a/b.tar`: that is what both
    /// callers need - closing a connection has to forget the cached answer for
    /// every archive opened over it, and `F5` of `/a/b.tar` into
    /// `/a/b.tar#/dir` is a copy loop. What is never beneath anything is a
    /// segment of a different namespace: two connections are two
    /// [`BackendKind::Remote`] ids and neither contains the other, however
    /// alike the paths on them read.
    pub fn starts_with(&self, other: &Self) -> bool {
        if self.depth() < other.depth() {
            return false;
        }
        let last = other.depth().saturating_sub(1);
        for index in 0..last {
            if self.segment(index) != other.segment(index) {
                return false;
            }
        }
        let (Some((kind, path)), Some((their_kind, their_path))) =
            (self.segment(last), other.segment(last))
        else {
            return false;
        };
        kind == their_kind && path.starts_with(their_path)
    }
}

impl fmt::Display for VfsPath {
    /// `/a/b.tar.gz#/inner/c.txt`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.head.1.display())?;
        for (_, path) in &self.rest {
            write!(f, "#{}", path.display())?;
        }
        Ok(())
    }
}

impl Default for VfsPath {
    fn default() -> Self {
        Self::local_root()
    }
}

/// What an [`Entry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symbolic link, carrying whether it resolves to a directory - the panel
    /// needs that to decide whether `Enter` descends.
    Symlink {
        /// True when the link target is a directory.
        to_dir: bool,
    },
    /// Socket, fifo, device node, or anything else.
    Other,
}

/// How much of a matching line is kept on the row.
///
/// A row carries the matched line only so the panel and the viewer can show
/// it; a line longer than this is a minified bundle or a generated table, and
/// keeping all of it on every row of a million-hit search is memory spent on
/// something no column is wide enough to render.
pub const MAX_HIT_LINE: usize = 512;

/// Where in a file a content search matched.
///
/// Carried on the row so that `Enter` "opens the viewer at the matching line
/// with the hit already highlighted" without a second lookup. Filled in only by
/// the search engine; `None` on every row of every real directory listing,
/// which is what the `Box` on [`Entry::hit`] is for - `Entry` grows by one
/// pointer rather than by this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHit {
    /// The byte offset of the start of the matching line, **in the stream the
    /// searcher read**.
    ///
    /// A position in the file when [`ContentHit::decoded`] is false, which is
    /// what the viewer seeks to (position is a byte offset).
    /// A position in the decoded text when it is true, which is not, and is
    /// then good only for a message.
    pub offset: u64,
    /// True when [`ContentHit::offset`] counts bytes of a **transcoded**
    /// stream rather than of the file.
    ///
    /// UTF-16, windows-1252 and CP437 are all searched after decoding
    /// (the independent charsets), and the decoded stream's byte
    /// offsets are not the file's: a UTF-16LE hit reports roughly half the
    /// file offset. [`ContentHit::line`] survives decoding exactly, so it is
    /// what the "opens the viewer at the matching line" is served
    /// from for those three.
    pub decoded: bool,
    /// 1-based line number, when the searcher counted lines.
    pub line: Option<u64>,
    /// The matched text of the line, already decoded, cropped to
    /// [`MAX_HIT_LINE`] characters. For the preview and for nothing else.
    pub line_text: String,
    /// Which charset matched (the independent checkboxes).
    pub charset: &'static str,
}

/// One row in a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name, lossily decoded. Never a path.
    pub name: String,
    /// What it is.
    pub kind: EntryKind,
    /// Size in bytes. Zero and meaningless for directories, which render
    /// `<DIR>`.
    pub size: u64,
    /// Modification time, `None` when the backend does not report one.
    pub mtime: Option<SystemTime>,
    /// POSIX mode bits. `0` when the backend has no concept of them.
    pub mode: u32,
    /// Owning uid.
    pub uid: u32,
    /// Owning gid.
    pub gid: u32,
    /// Hidden by the platform's convention - a leading dot on Unix.
    pub is_hidden: bool,
    /// True for the synthetic `..` row, which never sorts, never marks and
    /// never counts in the panel status line.
    pub is_parent: bool,
    /// Where the entry really lives.
    ///
    /// `None` for a normal listing, where the location is the directory being
    /// read. `Some` for a `ListFs` listing (search results, `Ctrl+B` branch
    /// view), where every row comes from a different directory
    /// and operations have to address the real home rather than the virtual
    /// one.
    pub location: Option<VfsPath>,
    /// Where a content search matched, for a search-result row.
    ///
    /// `None` for every row of every real directory listing. Boxed because a
    /// hit is the rare case and an `Entry` is the common one: a listing of a
    /// million files pays one pointer per row rather than four more fields.
    pub hit: Option<Box<ContentHit>>,
}

impl Entry {
    /// A plain file entry with everything unknown zeroed.
    pub fn file(name: impl Into<String>) -> Self {
        let name = name.into();
        let is_hidden = name.starts_with('.');
        Self {
            name,
            kind: EntryKind::File,
            size: 0,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            is_hidden,
            is_parent: false,
            location: None,
            hit: None,
        }
    }

    /// A plain directory entry.
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Dir,
            ..Self::file(name)
        }
    }

    /// The synthetic `..` row.
    pub fn parent_entry() -> Self {
        Self {
            name: "..".to_string(),
            kind: EntryKind::Dir,
            size: 0,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            is_hidden: false,
            is_parent: true,
            location: None,
            hit: None,
        }
    }

    /// True for a directory, or a symlink that resolves to one. This is what
    /// decides whether `Enter` descends.
    pub fn is_dir(&self) -> bool {
        matches!(
            self.kind,
            EntryKind::Dir | EntryKind::Symlink { to_dir: true }
        )
    }

    /// The key this row is **marked** under.
    ///
    /// Its real home when it has one, its name otherwise. A flat virtual
    /// listing can hold two rows called `mod.rs` from different directories -
    /// this module's own documentation says so in as many words - so a name is
    /// not an identity there and marking one would mark both, which would make
    /// `F8` delete a file the user did not mark. Every mark operation in the
    /// crate goes through this; on an ordinary directory listing, where
    /// `location` is `None`, it is the name and nothing changes.
    pub fn mark_key(&self) -> std::borrow::Cow<'_, str> {
        match self.location.as_ref() {
            Some(path) => std::borrow::Cow::Owned(path.to_string()),
            None => std::borrow::Cow::Borrowed(self.name.as_str()),
        }
    }

    /// True for any symlink, whatever it points at.
    pub fn is_symlink(&self) -> bool {
        matches!(self.kind, EntryKind::Symlink { .. })
    }

    /// Executability comes from the mode bits only, never from the extension.
    ///
    pub fn is_executable(&self) -> bool {
        !self.is_dir() && !self.is_parent && self.mode & 0o111 != 0
    }

    /// Split the name into `(stem, extension)` for the `name` and `ext` columns.
    /// Case is preserved and the dot is not included.
    ///
    /// Directories, `..` and dotfiles with no second dot have an empty
    /// extension, so `.bashrc` renders whole in the name column.
    pub fn split_name(&self) -> (&str, &str) {
        if self.is_dir() || self.is_parent {
            return (self.name.as_str(), "");
        }
        match self.name.rfind('.') {
            Some(0) | None => (self.name.as_str(), ""),
            Some(idx) => {
                let (stem, rest) = self.name.split_at(idx);
                (stem, rest.get(1..).unwrap_or(""))
            }
        }
    }

    /// The extension alone, for the `ext` column.
    pub fn extension(&self) -> &str {
        self.split_name().1
    }
}

/// How far away a backend is, so the UI knows whether a listing deserves a
/// progress indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LatencyClass {
    /// Same machine. Listings are effectively instant.
    #[default]
    Local,
    /// Over a network. Show progress; expect a listing to arrive in pages.
    Network,
}

/// What the UI consults before offering an operation.
///
/// None of these fields assume POSIX. `atomic_rename` is allowed to be false,
/// `has_directories` is allowed to be false for a prefix-only store, and
/// `paged_listing` is allowed to be true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Anything at all can be written.
    pub writable: bool,
    /// Reads can seek. False for a stream-only backend, which is what forces
    /// the viewer into its streaming mode.
    pub seekable: bool,
    /// Random access is cheap, not merely possible.
    pub random_access: bool,
    /// Real directories exist. False for an object store, where "directories"
    /// are prefixes.
    pub has_directories: bool,
    /// Rename is a single atomic operation rather than copy-then-delete.
    pub atomic_rename: bool,
    /// Listings arrive in pages and the panel must tolerate that.
    pub paged_listing: bool,
    /// A file here can be handed to the kernel to execute.
    pub can_execute: bool,
    /// Latency class.
    pub latency: LatencyClass,
}

impl Capabilities {
    /// What a real local filesystem offers.
    pub const LOCAL: Self = Self {
        writable: true,
        seekable: true,
        random_access: true,
        atomic_rename: true,
        has_directories: true,
        paged_listing: false,
        can_execute: true,
        latency: LatencyClass::Local,
    };

    /// What an archive backend offers *before* its format is known.
    /// Read-only, forward-only, nothing executable.
    pub const ARCHIVE_UNKNOWN: Self = Self {
        writable: false,
        seekable: false,
        random_access: false,
        atomic_rename: false,
        has_directories: true,
        paged_listing: false,
        can_execute: false,
        latency: LatencyClass::Local,
    };

    /// What a remote backend offers before its protocol is known.
    ///
    ///
    /// The conservative answer for a path whose connection is not at hand -
    /// under-promising costs a refusal the user can retry, over-promising
    /// costs a copy that fails halfway.
    pub const REMOTE_UNKNOWN: Self = Self {
        writable: false,
        seekable: false,
        random_access: false,
        has_directories: true,
        atomic_rename: false,
        paged_listing: true,
        can_execute: false,
        latency: LatencyClass::Network,
    };

    /// SFTP ("Full filesystem semantics over SSH").
    ///
    /// `seekable` is true because `SSH_FXP_READ` takes an offset, so the
    /// viewer can fetch a window without reading what is before it;
    /// `random_access` is false because the design defines it as "cheap,
    /// not merely possible" and a round trip per window is not cheap.
    pub const SFTP: Self = Self {
        writable: true,
        seekable: true,
        random_access: false,
        has_directories: true,
        atomic_rename: true,
        paged_listing: true,
        can_execute: false,
        latency: LatencyClass::Network,
    };

    /// FTP and FTPS. **No POSIX anything**.
    ///
    /// `seekable` is false: `REST` is optional, servers disagree about it, and
    /// the viewer's forward-only mode is correct rather than merely slower.
    /// `atomic_rename` is true because `RNFR`/`RNTO` is one server-side
    /// operation rather than a copy plus a delete - it may still fail when the
    /// target exists, which `ops::copy` already degrades from.
    pub const FTP: Self = Self {
        writable: true,
        seekable: false,
        random_access: false,
        has_directories: true,
        atomic_rename: true,
        paged_listing: true,
        can_execute: false,
        latency: LatencyClass::Network,
    };

    /// What a disk image offers, whatever is on it.
    ///
    /// Read-only is not conservative and is not a placeholder for a better
    /// answer later: the design is "read-only, and not as a first step
    /// towards writing". `seekable` is the conservative half - an ISO member
    /// is seekable and a FAT member is not, and this is the answer for a path
    /// whose backend is not at hand, where under-promising costs a refusal the
    /// user can retry and over-promising costs a copy that fails halfway.
    ///
    pub const IMAGE: Self = Self {
        writable: false,
        seekable: false,
        random_access: false,
        has_directories: true,
        atomic_rename: false,
        paged_listing: false,
        can_execute: false,
        latency: LatencyClass::Local,
    };

    /// A read-only synthetic listing.
    pub const READ_ONLY_LIST: Self = Self {
        writable: false,
        seekable: true,
        random_access: true,
        atomic_rename: false,
        has_directories: false,
        paged_listing: false,
        can_execute: false,
        latency: LatencyClass::Local,
    };
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::LOCAL
    }
}

/// A handle the viewer can seek within.
///
/// Blanket-implemented, so `std::fs::File` and `std::io::Cursor` are both one
/// already and no backend has to write an adapter. It exists because
/// `Box<dyn Read + Seek>` is not a legal trait object.
pub trait ReadSeek: std::io::Read + std::io::Seek {}

impl<T: std::io::Read + std::io::Seek> ReadSeek for T {}

/// How many entries a backend batches into one channel send. Small enough that
/// the first frame paints promptly, large enough that a big directory does not
/// cost one channel round trip per file.
pub const READ_DIR_BATCH: usize = 128;

/// How long a partly filled batch waits for the rest of itself before the
/// panel is given what has arrived.
///
/// [`READ_DIR_BATCH`] alone assumes a producer that ends: a directory read
/// fills its last batch and closes, so the tail is sent immediately. A search
/// listing does not end for as long as the walk runs, and its rows arrive one
/// at a time - so without this, a walk that found eleven hits in a minute
/// showed none of them for that minute. A tenth of a second is
/// [`crate::vfs::list`]'s own poll interval and is below the threshold at
/// which a human calls a panel stalled.
pub const READ_DIR_FLUSH: std::time::Duration = std::time::Duration::from_millis(100);

/// The channel depth [`Vfs::read_dir`] implementations should use.
pub const READ_DIR_CHANNEL_DEPTH: usize = 64;

/// One backend.
///
/// See the module docs for the two deliberate deviations from the literal
/// signatures in the design (`read_dir` and `stat`).
pub trait Vfs: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> BackendKind;

    /// Stream the contents of a directory.
    ///
    /// **Deviation from the design**: the spec writes this as `->
    /// BoxStream<Result<Entry>>`, which would mean a direct `futures`
    /// dependency that is not on the table. A tokio channel receiver
    /// preserves what the design actually requires - the listing is
    /// incremental, the panel renders what has arrived, and dropping the
    /// receiver cancels the walk.
    ///
    /// Implementations must not block the caller: spawn, and return the
    /// receiver immediately. Callers must be inside a tokio runtime.
    fn read_dir(&self, path: &VfsPath) -> tokio::sync::mpsc::Receiver<Result<Entry>>;

    /// Metadata for a single path.
    ///
    /// **Deviation from the design**: returns `Entry` rather than an
    /// undefined `Metadata` type.
    fn stat(&self, path: &VfsPath) -> Result<Entry>;

    /// Open for reading.
    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>>;

    /// Open for reading **with random access**.
    ///
    /// The viewer wants a `Read + Seek` handle so a window can be fetched
    /// without reading everything before it. A backend that cannot offer one
    /// says so and the viewer falls back to [`Vfs::open_read`], where the index
    /// is forward-only and a backward seek replays from a checkpoint - the
    /// other half of.
    ///
    /// The default is that refusal, so a backend added later compiles
    /// unchanged and is merely slower until it implements this. Implement it
    /// wherever [`Capabilities::seekable`] is true; the two must agree.
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        let _ = path;
        unsupported("random-access reading on this backend")
    }

    /// Open for writing, truncating.
    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>>;

    /// Create a directory. May fail with [`Error::Unsupported`] on a backend
    /// where `capabilities().has_directories` is false.
    fn create_dir(&self, path: &VfsPath) -> Result<()>;

    /// Remove a file or an empty directory.
    fn remove(&self, path: &VfsPath) -> Result<()>;

    /// Rename. Not required to be atomic - check
    /// [`Capabilities::atomic_rename`] before relying on it.
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()>;

    /// Where a symbolic link points, as the backend records it.
    ///
    /// the design copies a symlink as a link, which means the copy engine
    /// has to be able to ask what it points at. It must **ask**, because the
    /// answer is not the same shape in every backend: a `.zip` and a `.7z`
    /// store the target as the member's contents, a tar stores it in the
    /// header's link-name field and gives the member no contents at all, and
    /// a real filesystem has `readlink(2)`. A copy engine that guesses one of
    /// those conventions is wrong for the others - which is what the design
    /// puts this trait here to prevent.
    ///
    /// The backend is also where the target is **judged**: the design's
    /// rule that an extracted link may not point out of its destination is
    /// the archive's threat model, so an archive refuses such a target here
    /// rather than handing it out for somebody else to remember to check.
    ///
    /// The default is a refusal, so a backend added later compiles unchanged.
    fn read_link(&self, path: &VfsPath) -> Result<String> {
        let _ = path;
        unsupported("reading a symbolic link on this backend")
    }

    /// What this backend can do.
    fn capabilities(&self) -> Capabilities;

    /// What this backend can do **for one path**.
    ///
    /// Identical to [`Vfs::capabilities`] for every backend that is one
    /// filesystem. It exists for the router, which is not: the backend that
    /// services `/a/b.rar#/` is read-only and the one that services `/a` is
    /// not, and the design makes that difference the thing the UI consults
    /// "before offering an operation ... rather than failing halfway through
    /// a copy". A caller that has a path in hand must ask this rather than
    /// [`Vfs::capabilities`], which on a router can only answer for the local
    /// filesystem.
    ///
    /// **May block**, and may open an archive to find out (the design
    /// detects by content). Call it from a job thread or the event loop, never
    /// from `dispatch`.
    fn capabilities_for(&self, _path: &VfsPath) -> Capabilities {
        self.capabilities()
    }
}

/// Whether a listing name that did **not** come from this machine's own
/// filesystem may be used as a path component.
///
/// The name in a directory listing is chosen by whoever answers the listing,
/// and [`VfsPath::join`] joins a name containing a separator as written -
/// `Path::join` with an absolute argument discards the base entirely, and one
/// with `..` in it walks out of the destination. A recursive copy joins every
/// name it is told about onto the destination, so a server that answers
/// `SSH_FXP_READDIR` with `../../.bashrc` or `/etc/cron.d/pwn` would otherwise
/// write outside the directory the user chose. That is Zip Slip in its remote
/// spelling, and it is the same invariant
/// [`crate::vfs::archive::safety::normalize_member`] refuses twice for archive
/// members.
///
/// It lives here beside [`untrusted_mode`] rather than in one backend for the
/// same reason that one does: the rule is about the *provenance* of a name and
/// not about a protocol. Any backend that is not the local filesystem is a
/// place a name can be chosen by somebody who is not the user, and every one
/// of them reaches the disk through the same copy loop.
///
/// A plain name carries no separator and no NUL and is exactly one
/// `Component::Normal`: that rejects the empty name, `.`, `..`, an absolute
/// name, a rooted one, and anything carrying a separator - including
/// `a/.`, which has a separator but only one component - while leaving every
/// ordinary file name alone, a leading dot, spaces and newlines included.
pub fn is_plain_name(name: &str) -> bool {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('/')
        || name.contains(std::path::MAIN_SEPARATOR)
    {
        return false;
    }
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
}

/// A name that has been checked with [`is_plain_name`], carried as proof.
///
/// The check and the use of the checked name are pages apart in every backend
/// that needs it - a listing decodes a row here and a copy joins it onto a
/// destination there - so a `String` that has been checked and one that has not
/// look the same at the place it matters. This is the receipt: it cannot be
/// built without passing the check, and [`VfsPath::join_name`] takes nothing
/// else, so "did you remember the guard" is answered by a signature rather than
/// by reading back to the ingest site.
///
/// Backends that hold their own member paths as strings (the archive and image
/// indexes, whose members are `a/b/c` and not one component) check each
/// component with [`is_plain_name`] as they build it and do not need the
/// receipt; it is [`VfsPath::join`] that this exists to guard.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlainName(String);

impl PlainName {
    /// The receipt for `name`, or `None` when it is not one ordinary
    /// component.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        is_plain_name(&name).then_some(Self(name))
    }

    /// The checked name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The checked name, by value.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for PlainName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for PlainName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for PlainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The mode bits an entry that did **not** come from this machine's own
/// filesystem may be given when it is written to one.
///
/// **setuid, setgid and the sticky bit are dropped.** They are the second
/// oldest trick after Zip Slip: an archive that unpacks a setuid-root shell is
/// a machine given away, and no ordinary unpack needs them.
/// Everything else the source asked for is honoured, because the design
/// asks for mode to be preserved.
///
/// It lives here rather than in `vfs::archive` because the rule is about the
/// *provenance* of a mode and not about archives: any backend that is not the
/// local filesystem is a place a mode can be written by somebody who is not
/// the user, and every one of them reaches the disk through the same copy
/// loop. [`crate::vfs::archive::safety::safe_mode`] is this function plus the
/// "the archive recorded no mode at all" answer.
pub fn untrusted_mode(mode: u32) -> u32 {
    mode & 0o0777
}

/// Convenience for an operation a backend can never perform, whatever the
/// milestone - writing into a synthetic listing, say.
pub(crate) fn unsupported<T>(what: &'static str) -> Result<T> {
    Err(Error::Unsupported(what))
}

/// Convenience for an operation a backend *will* perform, but not in this
/// milestone.
///
/// Deliberately an `Err` and never a `todo!()`: a `todo!()` is a panic, and the
/// panel has to be able to report this in its status line the same way
/// `Action::not_implemented_message` does. The wording matches - "not
/// implemented until v0.5" - so the two paths read identically to a user, and
/// both name the milestone that will bring the feature rather than the one it
/// is missing from.
// Unused in v0.2: `LocalFs` implements every method of the trait, so nothing
// reaches for this until the first backend that does not - the archive VFS in
// v0.5. Deleting it and writing it again there would lose the reasoning above,
// which is the part worth keeping.
#[allow(
    dead_code,
    reason = "nothing reaches for this until a backend that does not implement \
              every method; the reasoning above is what is worth keeping"
)]
pub(crate) fn not_implemented_until<T>(milestone: Milestone, what: &str) -> Result<T> {
    Err(Error::msg(format!(
        "{what}: not implemented until {}",
        milestone.label()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_nests_with_hash() {
        let p = VfsPath::local("/a/b.tar.gz").with_segment(BackendKind::List, "/inner/c.txt");
        assert_eq!(p.to_string(), "/a/b.tar.gz#/inner/c.txt");
    }

    /// what a backend that is not this machine may call a file.
    ///
    /// The rule exists because [`VfsPath::join`] joins a name containing a
    /// separator as written, and `Path::join` with an absolute argument
    /// discards the base entirely - so a listing name is the shortest route
    /// out of a copy's destination there is.
    #[test]
    fn a_plain_name_is_one_component_and_nothing_clever() {
        for ok in [
            "a.txt",
            ".bashrc",
            "....",
            "a file with spaces",
            "a-b_c.tar.gz",
            "with\nnewline",
            "naïve",
        ] {
            assert!(is_plain_name(ok), "{ok:?} is an ordinary file name");
        }
        for bad in [
            "",
            ".",
            "..",
            "../../.bashrc",
            "/etc/cron.d/pwn",
            "sub/dir",
            "a/.",
            "trailing/",
            "with\u{0}nul",
        ] {
            assert!(!is_plain_name(bad), "{bad:?} is a path and not a name");
        }
    }

    #[test]
    fn local_path_only_for_a_single_local_segment() {
        assert!(VfsPath::local("/etc").local_path().is_some());
        let nested = VfsPath::local("/a.tar").with_segment(BackendKind::List, "/x");
        assert!(nested.local_path().is_none());
    }

    #[test]
    fn parent_walks_up_then_pops_the_segment() {
        let p = VfsPath::local("/a/b.tar.gz").with_segment(BackendKind::List, "/inner/c");
        let up = p.parent().expect("inner parent");
        assert_eq!(up.to_string(), "/a/b.tar.gz#/inner");
        let up = up.parent().expect("segment root");
        assert_eq!(up.to_string(), "/a/b.tar.gz#/");
        // Popping the segment lands on the directory holding the container.
        let up = up.parent().expect("out of the archive");
        assert_eq!(up.to_string(), "/a");
    }

    #[test]
    fn starts_with_compares_components_not_text() {
        let a = VfsPath::local("/a");
        assert!(VfsPath::local("/a").starts_with(&a));
        assert!(VfsPath::local("/a/b/c").starts_with(&a));
        assert!(
            !VfsPath::local("/ab").starts_with(&a),
            "/ab is not under /a"
        );
        assert!(!VfsPath::local("/").starts_with(&a));

        // Entering a container is descending: what is inside the archive is
        // beneath the archive, which is what `forget_subtree` and the copy
        // loop check both turn on.
        let outer = VfsPath::local("/a/b.tar");
        let inner = outer.clone().with_segment(BackendKind::List, "/x");
        assert!(inner.starts_with(&outer));
        assert!(!outer.starts_with(&inner));
        assert!(
            inner.starts_with(&outer.clone().with_segment(BackendKind::List, "/")),
            "and it is beneath the archive root"
        );
    }

    /// The case the doc on [`VfsPath::starts_with`] makes a claim about, which
    /// the code once contradicted: two namespaces, however alike the paths on
    /// them read.
    #[test]
    fn no_path_is_beneath_a_segment_of_another_namespace() {
        let one = VfsPath::new(BackendKind::Remote(crate::remote::RemoteId(1)), "/srv");
        let two = VfsPath::new(BackendKind::Remote(crate::remote::RemoteId(2)), "/srv/logs");
        assert!(
            !two.starts_with(&one),
            "a different connection, not a child"
        );
        assert!(!one.starts_with(&two));

        // The same shape one level in: the outer segments must match exactly,
        // so an archive on one connection holds nothing on the other.
        let here = VfsPath::new(BackendKind::Remote(crate::remote::RemoteId(1)), "/a.tar")
            .with_segment(BackendKind::Archive, "/x");
        let there = VfsPath::new(BackendKind::Remote(crate::remote::RemoteId(2)), "/a.tar");
        assert!(!here.starts_with(&there));

        // And a segment of a different *kind* at the same depth is not a
        // parent either, even at the same path.
        let listed = VfsPath::local("/a.tar").with_segment(BackendKind::List, "/x");
        let archived = VfsPath::local("/a.tar").with_segment(BackendKind::Archive, "/x");
        assert!(!listed.starts_with(&archived));
        assert!(!archived.starts_with(&listed));
    }

    /// The stack is non-empty by construction, so the two accessors that route
    /// operations answer about a segment that is really there rather than
    /// falling back to the local filesystem root.
    #[test]
    fn a_path_cannot_be_emptied_of_its_segments() {
        let mut p = VfsPath::new(BackendKind::List, "/results");
        assert_eq!(p.depth(), 1);
        assert!(
            p.pop_segment().is_none(),
            "the outermost segment is a field"
        );
        assert_eq!(p.depth(), 1);
        // Not `Local` and not `/`: the head is the only answer there is.
        assert_eq!(p.backend(), BackendKind::List);
        assert_eq!(p.tail(), Path::new("/results"));
        assert_eq!(
            p.innermost(),
            &(BackendKind::List, PathBuf::from("/results"))
        );
    }

    /// A name that did not come from this machine reaches [`VfsPath::join`]
    /// only through the receipt, and the receipt is the check.
    #[test]
    fn a_plain_name_is_the_only_way_to_join_a_name_from_elsewhere() {
        for bad in ["/etc/cron.d/pwn", "../../.bashrc", "..", ".", "", "a/b"] {
            assert!(PlainName::new(bad).is_none(), "{bad:?} is not a name");
        }
        let name = PlainName::new("hosts").expect("an ordinary name");
        assert_eq!(name.as_str(), "hosts");
        assert_eq!(name.to_string(), "hosts");
        assert_eq!(
            VfsPath::local("/etc").join_name(&name),
            VfsPath::local("/etc/hosts")
        );
        assert_eq!(
            PlainName::new("hosts").map(PlainName::into_string),
            Some("hosts".to_string())
        );
    }

    #[test]
    fn root_has_no_parent() {
        let root = VfsPath::local_root();
        assert!(root.is_root());
        assert!(root.parent().is_none());
    }

    #[test]
    fn push_and_pop_are_symmetric() {
        let mut p = VfsPath::local("/a/b.tar.gz");
        assert!(p.pop_segment().is_none(), "the stack never empties");
        p.push_segment(BackendKind::List, "/inner");
        assert_eq!(p.segments().len(), 2);
        assert_eq!(p.backend(), BackendKind::List);
        assert_eq!(p.tail(), Path::new("/inner"));
        let popped = p.pop_segment().expect("one to pop");
        assert_eq!(popped, (BackendKind::List, PathBuf::from("/inner")));
        assert_eq!(p, VfsPath::local("/a/b.tar.gz"));
        assert!(
            p.local_path().is_some(),
            "back to an addressable local path"
        );
    }

    #[test]
    fn join_and_file_name_round_trip() {
        let p = VfsPath::local("/etc").join("hosts");
        assert_eq!(p.to_string(), "/etc/hosts");
        assert_eq!(p.file_name().as_deref(), Some("hosts"));
        assert_eq!(p.parent(), Some(VfsPath::local("/etc")));
        assert_eq!(VfsPath::local_root().file_name(), None);
    }

    #[test]
    fn a_relative_path_does_not_walk_into_an_empty_parent() {
        let p = VfsPath::local("a");
        assert_eq!(p.parent(), None);
    }

    #[test]
    fn file_name_is_lossy_but_the_path_stays_byte_exact() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let raw = OsStr::from_bytes(b"caf\xffe");
        let p = VfsPath::local("/tmp").join(raw);
        // Lossy for display...
        let shown = p.file_name().expect("a name");
        assert!(shown.contains('\u{fffd}'), "rendered lossily: {shown:?}");
        // ...but the stored path still round-trips to the real bytes, which is
        // what operations use.
        assert_eq!(p.tail().file_name(), Some(raw));
        let expected = Path::new("/tmp").join(raw);
        assert_eq!(p.local_path(), Some(expected.as_path()));
    }

    #[test]
    fn split_name_keeps_dotfiles_whole() {
        assert_eq!(Entry::file(".bashrc").split_name(), (".bashrc", ""));
        assert_eq!(Entry::file("a.tar.gz").split_name(), ("a.tar", "gz"));
        assert_eq!(Entry::file("noext").split_name(), ("noext", ""));
        assert_eq!(Entry::dir("dir.d").split_name(), ("dir.d", ""));
    }
}
