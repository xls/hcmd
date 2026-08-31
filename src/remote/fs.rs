//! One connected remote, behind the [`Vfs`] trait.
//!
//! the design names the backends `SftpFs` / `FtpFs`. They are here as the two
//! [`RemoteTransport`] implementations, and this is the one adapter over them:
//! the mapping from `Vfs` to a remote protocol is written once, so the two
//! protocols cannot drift apart in the half of the work that is not
//! protocol-specific.
//!
//! **Blocking.** Every method here except [`RemoteFs::read_dir`],
//! [`RemoteFs::capabilities`] and [`RemoteFs::capabilities_for`] blocks, and
//! must be called from the blocking pool only.
//! Every synchronous `Vfs` call in this program already is: `ops::spawn` runs
//! a job under `spawn_blocking`, the viewer opens under `spawn_blocking`, and
//! `search::backend::walk` says so in its own doc comment.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::remote::transport::RemoteTransport;
use crate::remote::{RemoteId, Target};
use crate::vfs::{BackendKind, Capabilities, Entry, ReadSeek, Vfs, VfsPath};

/// One directory's rows and when they were read ("Directory
/// listings are cached with a short TTL").
#[derive(Debug, Default)]
pub(crate) struct ListingCache {
    /// Keyed by the remote-side directory path.
    entries: HashMap<String, (Instant, Arc<Vec<Entry>>)>,
}

impl ListingCache {
    /// The rows for a directory, if they are younger than `ttl`.
    fn get(&self, dir: &str, ttl: Duration, now: Instant) -> Option<Arc<Vec<Entry>>> {
        let (read_at, rows) = self.entries.get(dir)?;
        (now.saturating_duration_since(*read_at) < ttl).then(|| Arc::clone(rows))
    }

    /// Remember one directory's rows.
    fn put(&mut self, dir: &str, rows: Arc<Vec<Entry>>, now: Instant) {
        self.entries.insert(dir.to_string(), (now, rows));
    }

    /// Forget one directory, or all of them.
    fn invalidate(&mut self, dir: Option<&str>) {
        match dir {
            Some(dir) => {
                self.entries.remove(dir);
            }
            None => self.entries.clear(),
        }
    }
}

/// The cache handle, shared with the read task and with a writer whose `flush`
/// has to invalidate the directory it wrote into.
type SharedCache = Arc<std::sync::Mutex<ListingCache>>;

/// Lock the cache, recovering a poisoned lock rather than escalating it.
///
/// What is behind it is a map of listings: losing every cached directory to
/// one unrelated panic would be a worse answer than carrying on, and a
/// panicking thread cannot leave the map half-written.
fn lock(cache: &SharedCache) -> std::sync::MutexGuard<'_, ListingCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One connected remote.
pub struct RemoteFs {
    /// The id the registry gave it. `OnceLock` because the backend exists
    /// before it is registered: [`RemoteFs::new`] is what the connect task
    /// builds, and the id is what the event loop hands back.
    id: std::sync::OnceLock<RemoteId>,
    /// Where it points. Secret-free, so it can be rendered anywhere.
    target: Target,
    /// The protocol underneath.
    transport: Arc<dyn RemoteTransport>,
    /// Fixed when the connection was established, so `capabilities_for` never
    /// does I/O.
    caps: Capabilities,
    /// `remote.listing_ttl`.
    ttl: Duration,
    /// the short-TTL listing cache.
    ///
    /// Behind an `Arc` as well as a `Mutex`, which the contract's field list
    /// does not show, for one mechanical reason: `read_dir`'s task and
    /// `open_write`'s returned writer both outlive the `&self` that made them,
    /// and a `Box<dyn Write + Send>` carries no lifetime to borrow through.
    cache: SharedCache,
    /// Set when the transport reports the connection gone.
    lost: Arc<AtomicBool>,
}

impl RemoteFs {
    /// A backend over a live transport.
    pub fn new(target: Target, transport: Arc<dyn RemoteTransport>, ttl: Duration) -> Arc<Self> {
        let caps = transport.capabilities();
        Arc::new(Self {
            id: std::sync::OnceLock::new(),
            target,
            transport,
            caps,
            ttl,
            cache: Arc::new(std::sync::Mutex::new(ListingCache::default())),
            lost: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Take the id the registry assigned. Called once, by
    /// [`crate::remote::RemoteRegistry::register`], and ignored afterwards.
    pub(crate) fn adopt(&self, id: RemoteId) {
        let _ = self.id.set(id);
    }

    /// The id its paths name, or `None` before it has been registered.
    pub fn id(&self) -> Option<RemoteId> {
        self.id.get().copied()
    }

    /// Where this connection points.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The transport underneath, for the reconnect path, which replaces one
    /// connection behind the same id.
    pub fn transport(&self) -> &Arc<dyn RemoteTransport> {
        &self.transport
    }

    /// True once the connection has dropped (the disconnected
    /// state). Read by the panel on every frame; **never does I/O**.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || !self.transport.is_live()
    }

    /// The header line for a path on this connection:
    /// `sftp://thorin@nas.local:2222/srv/media`.
    ///
    /// A path this connection does not own falls back to the authority, which
    /// is still true and still secret-free - a header is not the place to
    /// report a routing mistake.
    pub fn header(&self, path: &VfsPath) -> String {
        match self.remote_path(path) {
            Ok(dir) => self.target.url(&dir),
            Err(_) => self.target.authority(),
        }
    }

    /// Drop the cached listing for one directory, which is what `Ctrl+R` does,
    /// and for every directory when `dir` is
    /// `None`.
    pub fn invalidate(&self, dir: Option<&str>) {
        lock(&self.cache).invalidate(dir);
    }

    /// The same, for a [`VfsPath`], which is what the event loop has in hand.
    pub fn invalidate_path(&self, path: &VfsPath) {
        match self.remote_path(path) {
            Ok(dir) => self.invalidate(Some(&dir)),
            Err(_) => self.invalidate(None),
        }
    }

    /// Close the transport. Idempotent; further calls fail with
    /// [`Error::ConnectionLost`].
    pub fn close(&self) {
        self.lost.store(true, Ordering::SeqCst);
        self.transport.close();
    }

    /// The remote-side path a [`VfsPath`] names.
    ///
    /// `Err(InvalidPath)` when the segment is not this connection's or is not
    /// UTF-8: both protocols speak `/`-separated text, and a filename this
    /// program cannot spell is one it must not guess at.
    fn remote_path(&self, path: &VfsPath) -> Result<String> {
        match (path.backend(), self.id()) {
            (BackendKind::Remote(theirs), Some(mine)) if theirs != mine => {
                return Err(Error::InvalidPath(format!(
                    "{path} is not on {}",
                    self.target.authority()
                )));
            }
            (BackendKind::Remote(_), _) => {}
            (
                BackendKind::Local
                | BackendKind::List
                | BackendKind::Archive
                | BackendKind::Image
                | BackendKind::Git,
                _,
            ) => {
                return Err(Error::InvalidPath(format!(
                    "{path} is not a path on {}",
                    self.target.authority()
                )));
            }
        }
        let text = path.tail().to_str().ok_or_else(|| {
            Error::InvalidPath(format!("{path}: a remote path has to be valid UTF-8"))
        })?;
        Ok(normalise(text))
    }

    /// Record a failure that means the connection is gone.
    ///
    /// Every fallible call goes through this, so the disconnected state is set
    /// in one place rather than at fourteen call sites.
    fn note<T>(&self, outcome: Result<T>) -> Result<T> {
        if let Err(Error::ConnectionLost(_)) = &outcome {
            self.lost.store(true, Ordering::SeqCst);
        }
        if !self.transport.is_live() {
            self.lost.store(true, Ordering::SeqCst);
        }
        outcome
    }

    /// The directory a path lives in, remote side, for cache invalidation.
    fn parent_dir(remote: &str) -> String {
        match remote.rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(at) => remote.get(..at).unwrap_or("/").to_string(),
        }
    }
}

/// Normalise a remote path: absolute, `/`-separated, no trailing slash except
/// at the root.
///
/// `VfsPath` stores a `PathBuf` and `Path::join` is happy to produce `//srv`
/// or `/srv/`; a server is not obliged to be as forgiving.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(1));
    out.push('/');
    for part in text.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}

impl Vfs for RemoteFs {
    /// The connection's own namespace. `RemoteId(0)` before registration,
    /// which names no connection anyone can reach - the id is set the moment
    /// the registry accepts the backend and never changes afterwards.
    fn kind(&self) -> BackendKind {
        BackendKind::Remote(self.id().unwrap_or(RemoteId(0)))
    }

    /// Stream one directory.
    ///
    /// Puts itself on the blocking pool with `spawn_blocking` plus
    /// `blocking_send`, which is character-for-character the shape of
    /// `LocalFs::read_dir`: there is therefore exactly one reply flavour and
    /// one code path, and the receiver still comes back immediately.
    ///
    fn read_dir(&self, path: &VfsPath) -> tokio::sync::mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = tokio::sync::mpsc::channel(crate::vfs::READ_DIR_CHANNEL_DEPTH);
        let resolved = self.remote_path(path);
        let has_parent = path.parent().is_some();
        let transport = Arc::clone(&self.transport);
        let cache = Arc::clone(&self.cache);
        let lost = Arc::clone(&self.lost);
        let ttl = self.ttl;

        tokio::task::spawn_blocking(move || {
            // The `..` row goes first and **before** the listing is attempted,
            // for the reason `LocalFs::read_dir` gives: it is navigation, not
            // content, and a directory that cannot be read must still have a
            // row to escape by.
            if has_parent && tx.blocking_send(Ok(Entry::parent_entry())).is_err() {
                return;
            }
            let dir = match resolved {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };
            let now = Instant::now();
            if let Some(rows) = lock(&cache).get(&dir, ttl, now) {
                for entry in rows.iter() {
                    if tx.blocking_send(Ok(entry.clone())).is_err() {
                        return;
                    }
                }
                return;
            }
            let rows = match transport.list(&dir) {
                Ok(rows) => rows,
                Err(err) => {
                    if matches!(err, Error::ConnectionLost(_)) || !transport.is_live() {
                        lost.store(true, Ordering::SeqCst);
                    }
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };
            // The one boundary every remote listing crosses, which is where a
            // name the *server* chose stops being trusted: a row called
            // `../../.bashrc` or `/etc/cron.d/pwn` is joined onto a local
            // destination by `ops::copy` as written, so it is dropped here
            // rather than being carried into the panel, the cache and the copy
            // loop (`crate::vfs::is_plain_name`). Dropping is
            // the honest answer: such a row names no file this backend can
            // address, because `RemoteFs` addresses a child by joining its
            // name onto the directory it came from.
            let rows: Vec<Entry> = rows
                .into_iter()
                .filter(|entry| entry.is_parent || crate::vfs::is_plain_name(&entry.name))
                .collect();
            let rows = Arc::new(rows);
            lock(&cache).put(&dir, Arc::clone(&rows), now);
            let mut sent = 0usize;
            for entry in rows.iter() {
                if tx.blocking_send(Ok(entry.clone())).is_err() {
                    return;
                }
                sent = sent.saturating_add(1);
                if sent.is_multiple_of(crate::vfs::READ_DIR_BATCH) {
                    std::thread::yield_now();
                }
            }
        });

        rx
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        let remote = self.remote_path(path)?;
        self.note(self.transport.stat(&remote))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        let remote = self.remote_path(path)?;
        self.note(self.transport.open_read(&remote))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn ReadSeek + Send>> {
        let remote = self.remote_path(path)?;
        self.note(self.transport.open_seek(&remote))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    ///
    /// The returned writer's `flush` is the commit, and it invalidates the
    /// directory it wrote into so a panel never shows a stale listing of a
    /// directory it has just written to.
    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        let remote = self.remote_path(path)?;
        let inner = self.note(self.transport.open_write(&remote))?;
        Ok(Box::new(CommittingWriter {
            inner,
            dir: Self::parent_dir(&remote),
            cache: Arc::clone(&self.cache),
        }))
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn create_dir(&self, path: &VfsPath) -> Result<()> {
        let remote = self.remote_path(path)?;
        let outcome = self.note(self.transport.create_dir(&remote));
        self.invalidate(Some(&Self::parent_dir(&remote)));
        outcome
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    ///
    /// A `stat` first, because the two protocols each have a separate call for
    /// a file and for a directory and neither will do the other's job.
    fn remove(&self, path: &VfsPath) -> Result<()> {
        let remote = self.remote_path(path)?;
        let entry = self.note(self.transport.stat(&remote))?;
        let outcome = if entry.is_dir() && !entry.is_symlink() {
            self.note(self.transport.remove_dir(&remote))
        } else {
            self.note(self.transport.remove_file(&remote))
        };
        self.invalidate(Some(&Self::parent_dir(&remote)));
        self.invalidate(Some(&remote));
        outcome
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let source = self.remote_path(from)?;
        let dest = self.remote_path(to)?;
        let outcome = self.note(self.transport.rename(&source, &dest));
        self.invalidate(Some(&Self::parent_dir(&source)));
        self.invalidate(Some(&Self::parent_dir(&dest)));
        outcome
    }

    /// **Blocking.** Call from the blocking pool only.
    ///
    fn read_link(&self, path: &VfsPath) -> Result<String> {
        let remote = self.remote_path(path)?;
        self.note(self.transport.read_link(&remote))
    }

    /// The value fixed when the connection was established. **Never does
    /// I/O**.
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    /// The same value for every path on this connection. **Never does I/O**:
    /// `App::apply_vfs_event` calls this on the event loop, and a call that
    /// waited on a socket there would stall the frame.
    fn capabilities_for(&self, _path: &VfsPath) -> Capabilities {
        self.caps
    }
}

impl std::fmt::Debug for RemoteFs {
    /// Target, capabilities and whether it is lost. Never the transport, and
    /// there is no secret anywhere in reach of this type.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFs")
            .field("id", &self.id())
            .field("target", &self.target)
            .field("caps", &self.caps)
            .field("lost", &self.is_lost())
            .finish()
    }
}

/// The writer [`RemoteFs::open_write`] hands out.
///
/// It exists for one line: `flush` is the commit,
/// and the moment it succeeds the directory's
/// cached listing is a lie (I7).
struct CommittingWriter {
    /// The transport's own writer.
    inner: Box<dyn std::io::Write + Send>,
    /// The directory to forget once the commit lands.
    dir: String,
    /// The cache to forget it in.
    cache: SharedCache,
}

impl std::io::Write for CommittingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let outcome = self.inner.flush();
        // Invalidated whichever way the flush went: a partial write leaves the
        // directory changed too, and a stale listing is a stale listing.
        lock(&self.cache).invalidate(Some(&self.dir));
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::transport::FakeTransport;
    use crate::remote::{Protocol, RemoteRegistry};
    use std::io::{Read, Seek, Write};

    fn target() -> Target {
        Target {
            protocol: Protocol::Sftp,
            host: "nas.local".to_string(),
            port: 2222,
            user: "thorin".to_string(),
            dir: Some("/srv".to_string()),
        }
    }

    fn fs_with(transport: FakeTransport) -> Arc<RemoteFs> {
        let fs = RemoteFs::new(
            target(),
            Arc::new(transport) as Arc<dyn RemoteTransport>,
            Duration::from_secs(2),
        );
        fs.adopt(RemoteId(3));
        fs
    }

    fn tree() -> FakeTransport {
        FakeTransport::new()
            .with_dir("/srv")
            .with_dir("/srv/media")
            .with_file("/srv/notes.txt", b"hello remote")
    }

    async fn drain(fs: &Arc<RemoteFs>, path: &VfsPath) -> Vec<Result<Entry>> {
        let mut rx = fs.read_dir(path);
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn a_listing_leads_with_the_parent_row_and_omits_it_at_the_root() {
        let fs = fs_with(tree());
        let rows = drain(&fs, &RemoteId(3).path("/srv")).await;
        let names: Vec<String> = rows
            .iter()
            .filter_map(|r| r.as_ref().ok().map(|e| e.name.clone()))
            .collect();
        assert_eq!(names.first().map(String::as_str), Some(".."));
        assert!(names.contains(&"media".to_string()));
        assert!(names.contains(&"notes.txt".to_string()));

        let root = drain(&fs, &RemoteId(3).root()).await;
        let first = root
            .first()
            .and_then(|r| r.as_ref().ok())
            .map(|e| e.name.clone());
        assert_eq!(
            first.as_deref(),
            Some("srv"),
            "no `..` at the connection root"
        );
    }

    /// a name in a listing is chosen by the server, and this is
    /// the one boundary every remote listing crosses.
    ///
    /// `ops::copy` joins a child's name onto the destination as written, and
    /// `Path::join` with an absolute argument discards the base entirely - so
    /// a row called `../../.bashrc` or `/etc/cron.d/pwn` is Zip Slip in its
    /// remote spelling. Such a row names nothing this backend can address
    /// either, because a child is addressed by joining its name onto the
    /// directory it came from, so it is dropped here.
    #[tokio::test]
    async fn a_listing_name_that_is_a_path_never_leaves_the_backend() {
        let hostile = tree()
            .with_listing_name("/srv", "../../.bashrc", b"PWNED")
            .with_listing_name("/srv", "/etc/cron.d/pwn", b"PWNED")
            .with_listing_name("/srv", "sub/dir", b"PWNED")
            .with_listing_name("/srv", "..", b"PWNED");
        let fs = fs_with(hostile);
        let rows = drain(&fs, &RemoteId(3).path("/srv")).await;
        let names: Vec<String> = rows
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .filter(|e| !e.is_parent)
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(
            names,
            vec!["media".to_string(), "notes.txt".to_string()],
            "only plain file names survive the boundary"
        );
        // The `..` row is still there once, synthesised from the path rather
        // than taken from the server.
        let parents = rows
            .iter()
            .filter(|r| matches!(r, Ok(e) if e.is_parent))
            .count();
        assert_eq!(parents, 1);
    }

    #[tokio::test]
    async fn a_listing_that_fails_still_left_the_parent_row_behind() {
        // I15's half that belongs to the backend: the `..` row is sent before
        // the listing is attempted, so a failure keeps it.
        let fs = fs_with(tree());
        let rows = drain(&fs, &RemoteId(3).path("/nowhere")).await;
        assert!(matches!(rows.first(), Some(Ok(e)) if e.is_parent));
        assert!(matches!(rows.last(), Some(Err(Error::NotFound(_)))));
    }

    #[tokio::test]
    async fn a_cached_listing_is_identical_and_costs_no_second_call() {
        // I6.
        let transport = Arc::new(tree());
        let fs = RemoteFs::new(
            target(),
            Arc::clone(&transport) as Arc<dyn RemoteTransport>,
            Duration::from_secs(60),
        );
        fs.adopt(RemoteId(3));
        let first = drain(&fs, &RemoteId(3).path("/srv")).await;
        let calls = transport.calls();
        let second = drain(&fs, &RemoteId(3).path("/srv")).await;
        assert_eq!(transport.calls(), calls, "served from the cache");
        let one: Vec<&Entry> = first.iter().filter_map(|r| r.as_ref().ok()).collect();
        let two: Vec<&Entry> = second.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(one, two);
    }

    #[tokio::test]
    async fn a_listing_older_than_the_ttl_is_not_served() {
        // I6's other half.
        let transport = Arc::new(tree());
        let fs = RemoteFs::new(
            target(),
            Arc::clone(&transport) as Arc<dyn RemoteTransport>,
            Duration::from_millis(1),
        );
        fs.adopt(RemoteId(3));
        drain(&fs, &RemoteId(3).path("/srv")).await;
        let calls = transport.calls();
        std::thread::sleep(Duration::from_millis(5));
        drain(&fs, &RemoteId(3).path("/srv")).await;
        assert!(transport.calls() > calls, "re-read after the TTL");
    }

    #[tokio::test]
    async fn every_write_invalidates_the_directory_it_changed() {
        // I7.
        let transport = Arc::new(tree());
        let fs = RemoteFs::new(
            target(),
            Arc::clone(&transport) as Arc<dyn RemoteTransport>,
            Duration::from_secs(60),
        );
        fs.adopt(RemoteId(3));

        for change in ["create_dir", "remove", "rename", "write"] {
            drain(&fs, &RemoteId(3).path("/srv")).await;
            let calls = transport.calls();
            match change {
                "create_dir" => fs
                    .create_dir(&RemoteId(3).path("/srv/fresh"))
                    .expect("mkdir"),
                "remove" => fs.remove(&RemoteId(3).path("/srv/fresh")).expect("remove"),
                "rename" => fs
                    .rename(
                        &RemoteId(3).path("/srv/notes.txt"),
                        &RemoteId(3).path("/srv/notes2.txt"),
                    )
                    .expect("rename"),
                _ => {
                    let mut w = fs
                        .open_write(&RemoteId(3).path("/srv/new.bin"))
                        .expect("open");
                    w.write_all(b"bytes").expect("write");
                    w.flush().expect("flush");
                }
            }
            drain(&fs, &RemoteId(3).path("/srv")).await;
            assert!(
                transport.calls() > calls.saturating_add(1),
                "{change} left a stale listing"
            );
        }
    }

    #[tokio::test]
    async fn read_dir_returns_its_receiver_without_waiting_for_the_listing() {
        // I4: the call must come back before the listing does, because the
        // panel draws in between.
        //
        // The bound is the fake's own delay rather than a number picked
        // separately from it. A fixed 100 ms against a 300 ms sleep is a claim
        // about how fast the machine is; "returned before the listing could
        // have finished" is the claim actually being made, and it stays true
        // on a runner three times slower than this one.
        let delay = Duration::from_millis(300);
        let fs = fs_with(tree().with_list_delay(delay));
        let started = Instant::now();
        let rx = fs.read_dir(&RemoteId(3).path("/srv"));
        assert!(
            started.elapsed() < delay,
            "read_dir blocked for {:?}, which is the whole listing",
            started.elapsed()
        );
        drop(rx);
    }

    #[test]
    fn capabilities_for_answers_with_the_transport_refusing_everything() {
        // I5.
        let fs = fs_with(tree().fail_after(0));
        assert_eq!(fs.capabilities_for(&RemoteId(3).root()), Capabilities::SFTP);
        assert_eq!(fs.capabilities(), Capabilities::SFTP);
    }

    #[test]
    fn a_path_from_another_connection_is_refused_rather_than_serviced() {
        // I1 and I2 at the backend: ids are namespaces.
        let fs = fs_with(tree());
        let err = fs
            .stat(&RemoteId(4).path("/srv/notes.txt"))
            .expect_err("not ours");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
        let err = fs
            .stat(&VfsPath::local("/etc/passwd"))
            .expect_err("not ours");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
    }

    #[test]
    fn the_whole_synchronous_surface_works_against_a_fake() {
        let fs = fs_with(tree());
        let notes = RemoteId(3).path("/srv/notes.txt");
        assert_eq!(fs.stat(&notes).expect("stat").size, 12);
        let mut text = String::new();
        fs.open_read(&notes)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        assert_eq!(text, "hello remote");
        let mut seek = fs.open_seek(&notes).expect("seek");
        seek.seek(std::io::SeekFrom::Start(6)).expect("seek");
        let mut rest = String::new();
        seek.read_to_string(&mut rest).expect("read");
        assert_eq!(rest, "remote");
        fs.create_dir(&RemoteId(3).path("/srv/sub")).expect("mkdir");
        fs.rename(
            &RemoteId(3).path("/srv/sub"),
            &RemoteId(3).path("/srv/sub2"),
        )
        .expect("rename");
        fs.remove(&RemoteId(3).path("/srv/sub2")).expect("rmdir");
        assert!(fs.stat(&RemoteId(3).path("/srv/sub2")).is_err());
    }

    #[test]
    fn a_dropped_connection_marks_the_backend_lost() {
        // nothing reconnects by itself, and `is_lost` never does I/O.
        let fs = fs_with(tree().drop_connection_at("/srv/notes.txt"));
        assert!(!fs.is_lost());
        let outcome = fs.open_read(&RemoteId(3).path("/srv/notes.txt"));
        let err = match outcome {
            Ok(_) => panic!("the connection was supposed to have gone"),
            Err(err) => err,
        };
        assert!(matches!(err, Error::ConnectionLost(_)), "{err}");
        assert!(fs.is_lost());
    }

    #[test]
    fn an_ftp_shaped_transport_refuses_to_seek_and_says_so_in_its_capabilities() {
        let fs = RemoteFs::new(
            target(),
            Arc::new(FakeTransport::ftp().with_file("/a.txt", b"x")) as Arc<dyn RemoteTransport>,
            Duration::from_secs(2),
        );
        fs.adopt(RemoteId(5));
        assert!(!fs.capabilities().seekable);
        let err = match fs.open_seek(&RemoteId(5).path("/a.txt")) {
            Ok(_) => panic!("an FTP transport cannot seek"),
            Err(err) => err,
        };
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
    }

    #[test]
    fn the_header_is_the_url_and_carries_no_secret() {
        let fs = fs_with(tree());
        assert_eq!(
            fs.header(&RemoteId(3).path("/srv/media")),
            "sftp://thorin@nas.local:2222/srv/media"
        );
        assert!(!format!("{fs:?}").contains("hunter2"));
    }

    #[test]
    fn paths_are_normalised_before_they_reach_the_wire() {
        assert_eq!(normalise("/srv//media/"), "/srv/media");
        assert_eq!(normalise("/"), "/");
        assert_eq!(normalise(""), "/");
        assert_eq!(RemoteFs::parent_dir("/srv/media"), "/srv");
        assert_eq!(RemoteFs::parent_dir("/srv"), "/");
        assert_eq!(RemoteFs::parent_dir("/"), "/");
    }

    /// I3: every blocking method answers from the blocking pool and from a
    /// plain thread alike, and neither panics.
    ///
    /// The rule the doc comments state is that they are called from the
    /// blocking pool only; this is the half a test can check - that nothing in
    /// them reaches for a runtime that is not there.
    #[tokio::test]
    async fn the_blocking_surface_is_callable_from_a_worker_thread_and_a_plain_one() {
        // Every step asserts its own answer, and the sequence is written to
        // leave the tree as it found it so the second run sees what the first
        // one did.
        fn exercise(fs: &Arc<RemoteFs>) {
            let id = RemoteId(3);
            let notes = id.path("/srv/notes.txt");
            assert_eq!(fs.stat(&notes).expect("stat").size, 12);

            let mut text = String::new();
            fs.open_read(&notes)
                .expect("open_read")
                .read_to_string(&mut text)
                .expect("read");
            assert_eq!(text, "hello remote");

            let mut seek = fs.open_seek(&notes).expect("open_seek");
            assert_eq!(seek.seek(std::io::SeekFrom::Start(6)).expect("seek"), 6);
            let mut rest = String::new();
            seek.read_to_string(&mut rest).expect("read");
            assert_eq!(rest, "remote");

            let written = id.path("/srv/w.bin");
            let mut writer = fs.open_write(&written).expect("open_write");
            writer.write_all(b"bytes").expect("write");
            writer.flush().expect("flush");
            assert_eq!(fs.stat(&written).expect("stat").size, 5);
            fs.remove(&written).expect("remove");

            let made = id.path("/srv/d");
            let renamed = id.path("/srv/e");
            fs.create_dir(&made).expect("create_dir");
            assert!(fs.stat(&made).expect("stat").is_dir());
            fs.rename(&made, &renamed).expect("rename");
            assert!(fs.stat(&made).is_err(), "the old name is gone");
            assert!(fs.stat(&renamed).expect("stat").is_dir());
            fs.remove(&renamed).expect("remove");
            assert!(fs.stat(&renamed).is_err(), "the removed name is gone");

            assert_eq!(
                fs.read_link(&id.path("/srv/link")).expect("read_link"),
                "notes.txt"
            );

            assert_eq!(fs.capabilities(), Capabilities::SFTP);
            assert_eq!(fs.capabilities_for(&id.root()), Capabilities::SFTP);
            assert!(!fs.is_lost(), "nothing here loses the connection");
            assert_eq!(
                fs.header(&id.path("/srv")),
                "sftp://thorin@nas.local:2222/srv"
            );
        }

        let fs = fs_with(tree().with_link("/srv/link", "notes.txt"));
        let on_pool = Arc::clone(&fs);
        tokio::task::spawn_blocking(move || exercise(&on_pool))
            .await
            .expect("the blocking pool");
        let on_thread = Arc::clone(&fs);
        std::thread::spawn(move || exercise(&on_thread))
            .join()
            .expect("a plain thread");
    }

    #[test]
    fn the_registry_never_reuses_an_id_and_refuses_past_the_ceiling() {
        // I1, and `MAX_CONNECTIONS` as a refusal rather than an eviction.
        fn unregistered() -> Arc<RemoteFs> {
            RemoteFs::new(
                target(),
                Arc::new(FakeTransport::new()) as Arc<dyn RemoteTransport>,
                Duration::from_secs(2),
            )
        }
        let registry = RemoteRegistry::new();
        let first = registry.register(unregistered()).expect("register");
        assert_eq!(first, RemoteId(1));
        assert_eq!(registry.get(first).and_then(|fs| fs.id()), Some(first));
        registry.close(first);
        assert!(registry.get(first).is_none());
        let second = registry.register(unregistered()).expect("register");
        assert_ne!(first, second, "ids are never reused");
        assert_eq!(registry.count(), 1);
        assert_eq!(
            registry.authority(second).as_deref(),
            Some("sftp://thorin@nas.local:2222")
        );

        while registry.count() < crate::remote::MAX_CONNECTIONS {
            registry
                .register(unregistered())
                .expect("under the ceiling");
        }
        let refused = registry
            .register(unregistered())
            .expect_err("at the ceiling");
        assert!(refused.to_string().contains("Ctrl+F"), "{refused}");
        assert_eq!(registry.count(), crate::remote::MAX_CONNECTIONS);
        assert_eq!(registry.ids().len(), crate::remote::MAX_CONNECTIONS);
    }
}
