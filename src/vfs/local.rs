//! The local filesystem backend.
//!
//! Everything here is deliberately POSIX-shaped, but nothing here leaks POSIX
//! assumptions into the [`Vfs`] trait itself - and the
//! module docs of [`super`].

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use super::{
    BackendKind, Capabilities, Entry, EntryKind, READ_DIR_BATCH, READ_DIR_CHANNEL_DEPTH, Vfs,
    VfsPath,
};
use crate::error::{Error, Result};

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalFs;

impl LocalFs {
    /// There is no state; this exists so call sites read the same as the other
    /// backends'.
    pub const fn new() -> Self {
        Self
    }

    /// Resolve a [`VfsPath`] to something the kernel understands, or explain
    /// why it cannot be (the design depends on this refusal).
    fn resolve(path: &VfsPath) -> Result<&Path> {
        path.local_path()
            .ok_or_else(|| Error::InvalidPath(format!("{path} is not on the local filesystem")))
    }
}

/// Hidden is a leading dot on Linux, tested on the raw bytes rather than on the
/// lossy name so a non-UTF-8 filename is classified correctly.
fn hidden(raw: &OsStr) -> bool {
    matches!(raw.as_bytes().first(), Some(b'.'))
}

/// Whether a symlink resolves to a directory.
///
/// This follows the link exactly far enough to answer that one question and no
/// further: `fs::metadata` is a single `stat(2)`, and the kernel is what bounds
/// the chain - a symlink loop comes back as `ELOOP` rather than spinning here.
/// A broken link answers `false` and stays a `Symlink` in the listing.
fn symlink_to_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// Classify from a `FileType` that cost nothing - on Linux `readdir` already
/// reported `d_type`, so this needs no syscall in the common case.
fn kind_from_type(ty: fs::FileType, path: &Path) -> EntryKind {
    if ty.is_symlink() {
        EntryKind::Symlink {
            to_dir: symlink_to_dir(path),
        }
    } else if ty.is_dir() {
        EntryKind::Dir
    } else if ty.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// One [`Entry`] from a name and the metadata already in hand.
///
/// Factored out of [`entry_from`] and [`entry_from_path`] so that a search
/// building a row from `ignore`'s own metadata builds the same
/// row a listing would, rather than a second, subtly different one.
/// It is also what keeps a search of a million
/// files at one `stat` per file rather than two.
///
/// The kind comes from the metadata alone, so a symlink is reported with
/// `to_dir: false`: answering that question needs the link's *target*, and
/// metadata does not carry it. A caller holding the path refines it, which
/// [`entry_from`], [`entry_from_path`] and `crate::search::walk` all do.
///
/// [`Entry::location`] is `None` here for the same reason: a name and its
/// metadata do not say where the file lives.
pub fn entry_from_metadata(name: impl Into<String>, meta: &fs::Metadata) -> Entry {
    let name = name.into();
    let is_hidden = name.starts_with('.');
    let ty = meta.file_type();
    let kind = if ty.is_symlink() {
        EntryKind::Symlink { to_dir: false }
    } else if ty.is_dir() {
        EntryKind::Dir
    } else if ty.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Entry {
        name,
        kind,
        size: meta.size(),
        mtime: meta.modified().ok(),
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        is_hidden,
        is_parent: false,
        location: None,
        hit: None,
        git_state: None,
    }
}

/// Turn a directory entry into an [`Entry`].
///
/// `dir` is the [`VfsPath`] being listed; it is only consulted for a name that
/// is not valid UTF-8, where the lossy [`Entry::name`] would not round-trip
/// back to a real file. Such an entry carries an explicit
/// [`Entry::location`] holding the byte-exact path, so it renders lossily but
/// operates correctly (the design;).
///
/// Metadata is read with `lstat` semantics, so a symlink renders as a symlink.
/// A file whose metadata cannot be read at all - a race with a delete, a
/// permission quirk - still appears in the listing, with its size and times
/// unknown rather than being dropped (`stat` may lag the name
/// column).
fn entry_from(dir: &VfsPath, dir_entry: &fs::DirEntry) -> Entry {
    let raw = dir_entry.file_name();
    let name = raw.to_string_lossy();
    // `to_string_lossy` borrows when the bytes were already valid UTF-8, so an
    // owned `Cow` is exactly the "this name did not survive decoding" signal.
    let lossy = matches!(name, Cow::Owned(_));
    let path = dir_entry.path();

    // `DirEntry::metadata` does not traverse the link on Unix.
    let meta = dir_entry.metadata().ok();
    let kind = match (dir_entry.file_type().ok(), meta.as_ref()) {
        (Some(ty), _) => kind_from_type(ty, &path),
        (None, Some(m)) => kind_from_type(m.file_type(), &path),
        (None, None) => EntryKind::Other,
    };

    // Built through the one factoring, so a search row and a listing row are
    // the same row. What the metadata alone cannot say is filled in after:
    // the kind resolves a symlink's target, `is_hidden` is tested on the raw
    // bytes rather than on the lossy name, and a name that did not survive
    // decoding carries its byte-exact path.
    let mut entry = match meta.as_ref() {
        Some(meta) => entry_from_metadata(name.into_owned(), meta),
        None => Entry {
            size: 0,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            ..Entry::file(name.into_owned())
        },
    };
    entry.kind = kind;
    entry.is_hidden = hidden(&raw);
    entry.location = lossy.then(|| dir.join(&raw));
    entry
}

/// Metadata for a path that already exists, as an [`Entry`].
///
/// `symlink_metadata`, so a symlink is reported as itself; the target is
/// resolved only far enough to fill in `to_dir`.
fn entry_from_path(path: &Path) -> Result<Entry> {
    let meta = fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
    let raw = path.file_name().unwrap_or(path.as_os_str());
    let name = raw.to_string_lossy();
    let lossy = matches!(name, Cow::Owned(_));
    let mut entry = entry_from_metadata(name.into_owned(), &meta);
    // The same three things the metadata alone cannot say - see
    // [`entry_from_metadata`].
    entry.kind = kind_from_type(meta.file_type(), path);
    entry.is_hidden = hidden(raw);
    entry.location = lossy.then(|| VfsPath::local(path));
    Ok(entry)
}

impl Vfs for LocalFs {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }

    fn read_dir(&self, path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let resolved: Result<PathBuf> = Self::resolve(path).map(Path::to_path_buf);
        let dir_path = path.clone();

        // Directory reads are blocking syscalls, so they belong on the blocking
        // pool, never on a runtime worker (the UI thread never
        // blocks on I/O).
        tokio::task::spawn_blocking(move || {
            let dir = match resolved {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };

            // The `..` row, synthesised by the backend because only the backend
            // knows where its own namespace ends. Omitted at the filesystem
            // root, which has nowhere to go.
            // `VfsPath::parent` rather than `Path::parent`, so the row appears
            // exactly when there is somewhere for it to go.

            // Only now attempt the read. The `..` row above is deliberately
            // sent *first*, so it survives this failing: it is navigation, not
            // content - where the parent is does not depend on being allowed to
            // read the child. Emitting it afterwards left a panel that could
            // not be read with no row to escape by (`/boot` is `drwx------`),
            // which is a trap, not a permission error.
            let iter = match fs::read_dir(&dir) {
                Ok(iter) => iter,
                Err(err) => {
                    // A directory the user cannot read is an error the panel
                    // reports, never an empty listing indistinguishable from a
                    // genuinely empty directory.
                    let _ = tx.blocking_send(Err(Error::io(&dir, err)));
                    return;
                }
            };

            let mut sent = 0usize;
            for item in iter {
                let message = match item {
                    Ok(de) => Ok(entry_from(&dir_path, &de)),
                    Err(err) => Err(Error::io(&dir, err)),
                };
                // A send error means the receiver was dropped, which is how the
                // panel cancels a listing it no longer wants.
                // It is also what unblocks this thread when the channel is full.
                if tx.blocking_send(message).is_err() {
                    return;
                }
                sent += 1;
                if sent.is_multiple_of(READ_DIR_BATCH) {
                    std::thread::yield_now();
                }
            }
        });

        rx
    }

    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        entry_from_path(Self::resolve(path)?)
    }

    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let p = Self::resolve(path)?;
        let file = fs::File::open(p).map_err(|e| Error::io(p, e))?;
        Ok(Box::new(file))
    }

    /// Open for reading with random access.
    ///
    /// The same `File` as [`LocalFs::open_read`]; a `File` is already
    /// `Read + Seek`, which is what makes the local backend the one the viewer
    /// never has to replay on.
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn super::ReadSeek + Send>> {
        let p = Self::resolve(path)?;
        let file = fs::File::open(p).map_err(|e| Error::io(p, e))?;
        Ok(Box::new(file))
    }

    /// Open for writing, truncating.
    ///
    /// Deliberately **not** `create_new`: the trait's contract is "open for
    /// writing, truncating", and every decision about whether an existing
    /// destination may be replaced belongs to the conflict policy,
    /// which has already been taken by the time a job reaches
    /// this call. A backend that silently refused would move that policy into
    /// the wrong layer.
    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        let p = Self::resolve(path)?;
        let file = fs::File::create(p).map_err(|e| Error::io(p, e))?;
        Ok(Box::new(file))
    }

    /// Create one directory.
    ///
    /// One level only, and an error when the parent is missing - the trait
    /// method is `create_dir`, not `create_dir_all`. Creating a chain is
    /// [`crate::ops::mkdir`]'s job, and it does it one level at a time so a
    /// failure names the level that actually failed.
    fn create_dir(&self, path: &VfsPath) -> Result<()> {
        let p = Self::resolve(path)?;
        fs::create_dir(p).map_err(|e| Error::io(p, e))
    }

    /// Remove a file, a symlink, or a whole directory tree.
    ///
    /// **A directory recurses.** the design puts it plainly - "Directories
    /// are recursed with a single confirmation, not one per entry" - and a
    /// backend that removed only empty directories would push the recursion
    /// into every caller.
    ///
    /// A symlink is removed as itself: `fs::remove_dir_all` does not follow
    /// links, so `Shift+F8` on a link into `/` unlinks the link and leaves the
    /// filesystem alone. That is checked by a test, because the alternative is
    /// catastrophic rather than merely wrong.
    fn remove(&self, path: &VfsPath) -> Result<()> {
        let p = Self::resolve(path)?;
        let meta = fs::symlink_metadata(p).map_err(|e| Error::io(p, e))?;
        if meta.is_dir() {
            fs::remove_dir_all(p).map_err(|e| Error::io(p, e))
        } else {
            fs::remove_file(p).map_err(|e| Error::io(p, e))
        }
    }

    /// Rename, atomically on this backend.
    ///
    /// `fs::rename` is atomic within a filesystem, which is what
    /// `Capabilities::LOCAL.atomic_rename` reports. Callers must still consult
    /// that flag rather than assuming it: the design explicitly allows a
    /// backend's rename to be a copy-then-delete, and an object store's will
    /// be. Across filesystems this fails with `EXDEV` and the caller degrades
    /// to copy-then-delete, which is exactly what
    /// [`crate::ops::copy`] does.
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let a = Self::resolve(from)?;
        let b = Self::resolve(to)?;
        fs::rename(a, b).map_err(|e| Error::io(a, e))
    }

    /// `readlink(2)`, refusing a target this process cannot represent.
    ///
    /// A link target is bytes on Linux and need not be UTF-8; a copy that
    /// cannot name it says so rather than writing a lossily decoded target,
    /// which would point somewhere else.
    fn read_link(&self, path: &VfsPath) -> Result<String> {
        let at = Self::resolve(path)?;
        let target = fs::read_link(at).map_err(|e| Error::io(at, e))?;
        target
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| Error::InvalidPath(format!("{path}: the link target is not UTF-8")))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::LOCAL
    }

    /// `std::os::unix::fs::symlink`, and nothing resolved on the way.
    ///
    /// The target is written as it was given. A symbolic link is allowed to
    /// point at something that does not exist yet, and a relative one means
    /// whatever it means from where the link sits; resolving it here would
    /// turn both of those into something else.
    fn symlink(&self, target: &str, link: &VfsPath) -> Result<()> {
        let path = Self::resolve(link)?;
        std::os::unix::fs::symlink(target, path).map_err(|e| Error::io(path, e))
    }

    fn hard_link(&self, target: &VfsPath, link: &VfsPath) -> Result<()> {
        let from = Self::resolve(target)?;
        let to = Self::resolve(link)?;
        std::fs::hard_link(from, to).map_err(|e| Error::io(to, e))
    }

    fn set_mode(&self, path: &VfsPath, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let target = Self::resolve(path)?;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::io(target, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::LatencyClass;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A throwaway directory under `std::env::temp_dir()`, removed on drop.
    /// Built by hand rather than with `tempfile`, which is not on the dependency
    /// table.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-vfs-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn vfs_path(&self) -> VfsPath {
            VfsPath::local(&self.root)
        }

        fn file(&self, name: impl AsRef<OsStr>, contents: &str) -> PathBuf {
            let p = self.root.join(name.as_ref());
            fs::write(&p, contents).expect("write temp file");
            p
        }

        fn dir(&self, name: &str) -> PathBuf {
            let p = self.root.join(name);
            fs::create_dir_all(&p).expect("create temp dir");
            p
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            // Best effort: a test that chmod'ed a directory to 000 restores it
            // first, and a leaked temp dir must never fail a test run.
            let _ = fs::set_permissions(&self.root, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Drain a listing to completion, led by the `..` the read path prepends,
    /// so what these tests see is what a panel sees. Panics only in test code.
    async fn collect(fs_impl: &LocalFs, path: &VfsPath) -> Vec<Result<Entry>> {
        let mut rx = fs_impl.read_dir(path);
        let mut out: Vec<Result<Entry>> = fs_impl.parent_row(path).map(Ok).into_iter().collect();
        while let Some(item) = rx.recv().await {
            out.push(item);
        }
        out
    }

    async fn collect_ok(fs_impl: &LocalFs, path: &VfsPath) -> HashMap<String, Entry> {
        collect(fs_impl, path)
            .await
            .into_iter()
            .filter_map(std::result::Result::ok)
            .map(|e| (e.name.clone(), e))
            .collect()
    }

    #[tokio::test]
    async fn lists_files_dirs_and_the_parent_row() {
        let t = TempTree::new("list");
        t.file("alpha.txt", "hello");
        t.dir("beta");
        let fs_impl = LocalFs::new();

        // The backend answers with its content. The `..` is navigation and
        // belongs to the read path, which prepends it for every backend alike -
        // this one no longer has a copy of that rule to get wrong.
        let parent = fs_impl.parent_row(&t.vfs_path()).expect("a way out");
        assert!(parent.is_parent && parent.is_dir());
        assert_eq!(parent.name, "..");

        let mut rx = fs_impl.read_dir(&t.vfs_path());
        let mut rest = HashMap::new();
        while let Some(item) = rx.recv().await {
            let e = item.expect("no error");
            rest.insert(e.name.clone(), e);
        }
        assert_eq!(rest.len(), 2, "got {:?}", rest.keys().collect::<Vec<_>>());

        let alpha = rest.get("alpha.txt").expect("alpha.txt");
        assert_eq!(alpha.kind, EntryKind::File);
        assert_eq!(alpha.size, 5);
        assert!(alpha.mtime.is_some());
        assert_ne!(alpha.mode, 0);
        let root_uid = fs::metadata(t.path()).expect("stat the tree").uid();
        assert_eq!(alpha.uid, root_uid);
        assert!(!alpha.is_hidden);
        assert_eq!(alpha.location, None, "a UTF-8 name needs no explicit home");

        let beta = rest.get("beta").expect("beta");
        assert_eq!(beta.kind, EntryKind::Dir);
        assert!(beta.is_dir());
    }

    #[tokio::test]
    async fn the_filesystem_root_has_no_parent_row() {
        let fs_impl = LocalFs::new();
        let mut rx = fs_impl.read_dir(&VfsPath::local_root());
        let first = rx.recv().await.expect("`/` is never empty");
        let first = first.expect("`/` is readable");
        assert!(!first.is_parent, "`/` has nowhere to go up to");
    }

    #[tokio::test]
    async fn hidden_is_a_leading_dot() {
        let t = TempTree::new("hidden");
        t.file(".bashrc", "");
        t.file("visible", "");
        t.dir(".config");
        let by_name = collect_ok(&LocalFs::new(), &t.vfs_path()).await;

        assert!(by_name.get(".bashrc").expect(".bashrc").is_hidden);
        assert!(by_name.get(".config").expect(".config").is_hidden);
        assert!(!by_name.get("visible").expect("visible").is_hidden);
        assert!(
            !by_name.get("..").expect("..").is_hidden,
            "`..` is never hidden"
        );
    }

    #[tokio::test]
    async fn a_broken_symlink_still_lists() {
        let t = TempTree::new("broken");
        let link = t.path().join("dangling");
        std::os::unix::fs::symlink(t.path().join("nothing-here"), &link).expect("symlink");
        let good = t.dir("real");
        std::os::unix::fs::symlink(&good, t.path().join("to-dir")).expect("symlink");

        let by_name = collect_ok(&LocalFs::new(), &t.vfs_path()).await;
        let dangling = by_name.get("dangling").expect("the broken link is listed");
        assert_eq!(dangling.kind, EntryKind::Symlink { to_dir: false });
        assert!(dangling.is_symlink());
        assert!(!dangling.is_dir(), "a broken link is not a directory");

        let to_dir = by_name.get("to-dir").expect("the good link is listed");
        assert_eq!(to_dir.kind, EntryKind::Symlink { to_dir: true });
        assert!(to_dir.is_dir(), "Enter descends a link to a directory");
    }

    #[tokio::test]
    async fn a_symlink_loop_terminates_instead_of_hanging() {
        let t = TempTree::new("loop");
        let a = t.path().join("a");
        let b = t.path().join("b");
        std::os::unix::fs::symlink(&b, &a).expect("a -> b");
        std::os::unix::fs::symlink(&a, &b).expect("b -> a");

        let by_name = collect_ok(&LocalFs::new(), &t.vfs_path()).await;
        // The kernel answers ELOOP; we report "not a directory" and move on.
        assert_eq!(
            by_name.get("a").expect("a").kind,
            EntryKind::Symlink { to_dir: false }
        );
    }

    /// Only where a name may be arbitrary bytes.
    ///
    /// APFS and HFS+ validate names as UTF-8 and refuse the write with
    /// `EILSEQ`, so on macOS this cannot get as far as the behaviour it is
    /// about. The behaviour is still right and still worth pinning where the
    /// filesystem allows the condition to exist.
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
    #[tokio::test]
    async fn a_non_utf8_name_renders_lossily_and_keeps_its_real_path() {
        let t = TempTree::new("nonutf8");
        let raw = OsStr::from_bytes(b"bad-\xff-name.txt");
        let real = t.file(raw, "x");

        let entries = collect(&LocalFs::new(), &t.vfs_path()).await;
        let entry = entries
            .into_iter()
            .filter_map(std::result::Result::ok)
            .find(|e| !e.is_parent)
            .expect("the entry is listed, not silently dropped");

        assert!(
            entry.name.contains('\u{fffd}'),
            "rendered lossily: {:?}",
            entry.name
        );
        let location = entry.location.as_ref().expect("a byte-exact home");
        assert_eq!(location.local_path(), Some(real.as_path()));
        // And that path really opens.
        assert!(LocalFs::new().open_read(location).is_ok());
    }

    #[tokio::test]
    async fn an_unreadable_directory_reports_an_error() {
        let t = TempTree::new("noperm");
        let locked = t.dir("locked");
        t.file("sibling", "");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");

        let out = collect(&LocalFs::new(), &VfsPath::local(&locked)).await;
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));

        if fs::read_dir(t.path().join("locked")).is_ok() && out.iter().all(|r| r.is_ok()) {
            // Running as root, where mode 000 means nothing. Nothing to assert.
            return;
        }
        assert!(
            out.iter().any(std::result::Result::is_err),
            "an unreadable directory must not look like an empty one"
        );
        // ...and it must still offer the way out. Where the parent is does not
        // depend on being allowed to read the child, and the read path asks
        // for it before the read is even attempted - so the answer here cannot
        // depend on the listing having worked. Without it a panel on `/boot`
        // (`drwx------`) has no row to select and no way back except a key the
        // user has to already know.
        assert!(
            LocalFs::new()
                .parent_row(&VfsPath::local(&locked))
                .is_some(),
            "an unreadable directory still has a way out"
        );
    }

    #[tokio::test]
    async fn a_path_that_is_not_local_is_refused() {
        // Straight at the backend: what it refuses is its own business, and
        // the `..` the read path adds would only be noise here.
        let nested = VfsPath::local("/a.tar").with_segment(BackendKind::List, "/x");
        let fs_impl = LocalFs::new();
        let mut rx = fs_impl.read_dir(&nested);
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item);
        }
        assert_eq!(out.len(), 1);
        assert!(matches!(out.first(), Some(Err(Error::InvalidPath(_)))));
    }

    #[test]
    fn dropping_the_receiver_cancels_the_walk() {
        // The property under test is that a producer *blocked on a full
        // channel* is released and stops when the receiver goes away. With a
        // single blocking thread, a walk that did not stop would starve the
        // next one forever.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");

        rt.block_on(async {
            let t = TempTree::new("cancel");
            let count = READ_DIR_CHANNEL_DEPTH * 8;
            for i in 0..count {
                t.file(format!("f{i:04}"), "");
            }
            let fs_impl = LocalFs::new();

            let mut rx = fs_impl.read_dir(&t.vfs_path());
            let first = rx.recv().await.expect("at least one");
            assert!(first.is_ok());
            // The producer is now wedged against a full channel.
            drop(rx);

            // A second listing needs the one blocking thread. It only gets it
            // if the first walk noticed the drop and returned.
            let second = tokio::time::timeout(Duration::from_secs(10), async {
                let mut rx = fs_impl.read_dir(&t.vfs_path());
                let mut n = 0usize;
                while let Some(item) = rx.recv().await {
                    if item.is_ok() {
                        n += 1;
                    }
                }
                n
            })
            .await
            .expect("the cancelled walk released the blocking thread");

            assert_eq!(second, count, "every file the first walk saw");
        });
    }

    #[test]
    fn stat_does_not_follow_the_link() {
        let t = TempTree::new("stat");
        let target = t.file("target", "abcdef");
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let fs_impl = LocalFs::new();

        let via_link = fs_impl.stat(&VfsPath::local(&link)).expect("stat the link");
        assert_eq!(via_link.name, "link");
        assert!(
            via_link.is_symlink(),
            "stat reports the link, not the target"
        );

        let direct = fs_impl.stat(&VfsPath::local(&target)).expect("stat target");
        assert_eq!(direct.kind, EntryKind::File);
        assert_eq!(direct.size, 6);

        let missing = t.path().join("no-such-file");
        assert!(fs_impl.stat(&VfsPath::local(missing)).is_err());
    }

    #[test]
    fn open_write_creates_and_truncates() {
        use std::io::Write as _;

        let t = TempTree::new("openwrite");
        let fs_impl = LocalFs::new();
        let p = VfsPath::local(t.path().join("x.txt"));

        let mut w = fs_impl.open_write(&p).expect("create");
        w.write_all(b"first pass").expect("write");
        drop(w);
        assert_eq!(
            fs::read(t.path().join("x.txt")).expect("read"),
            b"first pass"
        );

        // Truncating, not appending: the trait says so and a copy relies on it.
        let mut w = fs_impl.open_write(&p).expect("reopen");
        w.write_all(b"second").expect("write");
        drop(w);
        assert_eq!(fs::read(t.path().join("x.txt")).expect("read"), b"second");
    }

    #[test]
    fn create_dir_is_one_level_only() {
        let t = TempTree::new("mkdir");
        let fs_impl = LocalFs::new();

        let one = VfsPath::local(t.path().join("one"));
        fs_impl.create_dir(&one).expect("one level");
        assert!(t.path().join("one").is_dir());

        // A missing parent is an error, not a silent `create_dir_all`.
        let deep = VfsPath::local(t.path().join("missing/deep"));
        assert!(fs_impl.create_dir(&deep).is_err());
        assert!(!t.path().join("missing").exists());

        // And a second call over an existing directory reports it.
        assert!(fs_impl.create_dir(&one).is_err());
    }

    #[test]
    fn remove_recurses_into_a_directory() {
        let t = TempTree::new("remove");
        let fs_impl = LocalFs::new();
        fs::create_dir_all(t.path().join("tree/a/b")).expect("mkdir");
        fs::write(t.path().join("tree/a/b/c.txt"), b"deep").expect("write");

        fs_impl
            .remove(&VfsPath::local(t.path().join("tree")))
            .expect("recursive remove");
        assert!(!t.path().join("tree").exists());

        // A plain file too, and a missing path is an error rather than a
        // silent success.
        let f = t.file("solo", "x");
        fs_impl.remove(&VfsPath::local(&f)).expect("remove file");
        assert!(fs_impl.remove(&VfsPath::local(&f)).is_err());
    }

    #[test]
    fn remove_unlinks_a_symlink_and_never_walks_through_it() {
        let t = TempTree::new("removelink");
        let fs_impl = LocalFs::new();
        let real = t.dir("real");
        fs::write(real.join("precious.txt"), b"keep").expect("write");
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        fs_impl
            .remove(&VfsPath::local(&link))
            .expect("unlink the link");
        assert!(!link.exists(), "the link is gone");
        assert!(
            real.join("precious.txt").exists(),
            "the target survived: a recursive remove must never follow a link"
        );
    }

    #[test]
    fn rename_moves_within_the_filesystem() {
        let t = TempTree::new("rename");
        let fs_impl = LocalFs::new();
        let from = t.file("from.txt", "payload");
        let to = t.path().join("to.txt");

        fs_impl
            .rename(&VfsPath::local(&from), &VfsPath::local(&to))
            .expect("rename");
        assert!(!from.exists());
        assert_eq!(fs::read(&to).expect("read"), b"payload");
        assert!(
            fs_impl.capabilities().atomic_rename,
            "LocalFs advertises what it delivers"
        );
    }

    #[test]
    fn a_write_off_the_local_filesystem_is_refused_by_path_rather_than_attempted() {
        let fs_impl = LocalFs::new();
        let nested =
            VfsPath::local("/tmp/a.tar").with_segment(crate::vfs::BackendKind::List, "/inner");
        assert!(fs_impl.open_write(&nested).is_err());
        assert!(fs_impl.create_dir(&nested).is_err());
        assert!(fs_impl.remove(&nested).is_err());
        assert!(fs_impl.rename(&nested, &VfsPath::local("/tmp/x")).is_err());
    }

    #[test]
    fn capabilities_are_the_local_set() {
        let c = LocalFs::new().capabilities();
        assert!(c.writable);
        assert!(c.seekable);
        assert!(c.random_access);
        assert!(c.has_directories);
        assert!(c.atomic_rename);
        assert!(!c.paged_listing);
        assert!(c.can_execute);
        assert_eq!(c.latency, LatencyClass::Local);
        assert_eq!(LocalFs::new().kind(), BackendKind::Local);
    }
}
