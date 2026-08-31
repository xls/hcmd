//! Which backend services a path.
//!
//! Exactly one backend answers for any given [`VfsPath`], decided by that
//! path's innermost segment, and every caller keeps the single `Arc<dyn Vfs>`
//! it already had. That is what makes the "archives are directories"
//! true without the panel, `ops`, the viewer or the clipboard knowing an
//! archive exists.
//!
//! The router also owns the two registries a path can name: the archive
//! session, created on first use, and the virtual listings a `list:` path
//! refers to. Both are here because both are answers to the
//! same question, "what is behind this path".

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{ArchiveConfig, RemoteConfig};
use crate::error::{Error, Result};
use crate::remote::RemoteRegistry;
use crate::vfs::list::{ListingId, MAX_LISTINGS};
use crate::vfs::{ArchiveSession, BackendKind, Capabilities, Entry, ListFs, LocalFs, Vfs, VfsPath};

/// The backend both panels read through: the local filesystem, plus whatever
/// archive a path happens to be inside.
///
/// the claim is that "archives are directories", and the is
/// that the trait is what makes that true without the panel, `ops`, the viewer
/// or the clipboard knowing an archive exists. Both hold only if *something*
/// turns a [`VfsPath`] into the backend that services it. This is that
/// something, and it is deliberately the only such place: every caller keeps
/// the single `Arc<dyn Vfs>` it already had.
///
/// # `kind` and `capabilities` have no honest answer here, and nothing asks
///
/// Those two trait methods take no path, and a router has no answer that is
/// true for every path at once: the backend servicing `/a/b.rar#/` is
/// read-only and the one servicing `/a` is not. They report [`LocalFs`]'s,
/// which is what this field reported before archives existed - a lie by
/// construction that survived because nothing calls them on a router.
/// **Ask [`VfsRouter::known_capabilities`] instead**, which takes the path,
/// never blocks, and reads the one place the answer is kept.
///
/// They cannot simply be deleted from [`Vfs`]: every backend implements the
/// trait and the removal reaches well past this module. Until then the rule is
/// that a path in hand means a path-taking question.
pub struct VfsRouter {
    local: Arc<LocalFs>,
    /// `[archive]`, kept so the session can be created on demand.
    archive: ArchiveConfig,
    /// `[remote]`, kept because the connect task builds every
    /// [`crate::remote::RemoteFs`] with `remote.listing_ttl` and the router is
    /// where that value is already in scope.
    remote_config: RemoteConfig,
    /// The live connections.
    ///
    /// The registry owns the backend and the tab owns the id, so a job that
    /// outlives its tab keeps working: it holds its own `Arc` through this
    /// router.
    remote: Arc<RemoteRegistry>,
    /// Created the first time a path inside an archive is touched.
    ///
    /// Lazily, for two reasons: a session is a `0700` directory under
    /// `$TMPDIR` and [`crate::app::App::headless`] is documented to build an application
    /// out of compiled-in defaults without touching the filesystem, and a
    /// session that is never used is a directory that never needed to exist.
    session: std::sync::OnceLock<Arc<ArchiveSession>>,

    /// The virtual listings this session is showing, by id.
    ///
    /// A `std::sync::Mutex` and not a `RefCell`: the router is `Send + Sync`
    /// behind an `Arc` and is read from worker tasks, so there is no `&mut`
    /// route to it at all. That is the case a mutex is for and the case the
    /// house style rejects a `RefCell` for.
    listings: std::sync::Mutex<HashMap<u64, Arc<ListFs>>>,
    /// Monotonic source for [`ListingId`]. Ids are never reused, so a stale
    /// `list:` path names nothing rather than someone else's results.
    next_listing: std::sync::atomic::AtomicU64,

    /// What every path this session has actually resolved a backend for turned
    /// out to be able to do.
    ///
    /// This is the one place the answer lives. [`VfsRouter::capabilities_for`]
    /// reads it and [`VfsRouter::read_dir`] writes it, which is what makes the
    /// question answerable from the thread that draws: by the time a panel can
    /// gate a key on a directory, that directory has been listed, and listing
    /// it is what opened the backend that knows.
    ///
    /// An `Arc` because the archive and image arms of
    /// [`VfsRouter::read_dir`] open their backend inside a spawned task, and
    /// that task is where the answer becomes known.
    capabilities: Arc<CapabilityCache>,
}

/// How many answers are remembered before the cache is emptied and refilled.
///
/// The cache is a warm start and never a correctness requirement - a miss
/// resolves the backend, which is what every caller did before it existed - so
/// forgetting everything is always safe. Emptying rather than evicting the
/// oldest keeps the bookkeeping to one comparison; a session that has visited
/// more than this many directories refills the handful it is still looking at
/// on their next read.
const MAX_REMEMBERED_CAPABILITIES: usize = 512;

/// What the backend servicing each path turned out to be able to do.
///
/// The single place that answer is kept. Before this existed the question had
/// five answers that could disagree - a static per-kind guess, the router's
/// (which was the local filesystem's for every path), the honest blocking one,
/// a copy cached on each tab and a fold over the static guesses - and which of
/// them a key was gated on decided whether the key worked. There is now one,
/// it is written wherever a backend is resolved, and every gate reads it.
///
/// A `std::sync::Mutex` and not a `RefCell`: this is shared between the event
/// loop, the job threads and the worker tasks that stream listings, so there
/// is no `&mut` route to it at all. That is the case a mutex is for and the
/// case the house style rejects a `RefCell` for.
#[derive(Debug, Default)]
pub struct CapabilityCache {
    known: std::sync::Mutex<HashMap<VfsPath, Capabilities>>,
}

impl CapabilityCache {
    /// The lock, with a poisoned one recovered rather than escalated.
    ///
    /// What is behind it is a map of plain `Copy` values: a panicking thread
    /// cannot leave it half-written, and answering every later gate
    /// pessimistically because of one unrelated panic would be a worse answer
    /// than carrying on.
    fn known(&self) -> std::sync::MutexGuard<'_, HashMap<VfsPath, Capabilities>> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record what the backend servicing `path` said.
    ///
    /// Called wherever a backend has already been resolved, so remembering
    /// costs nothing that was not already spent. See
    /// [`MAX_REMEMBERED_CAPABILITIES`] for why the cache is emptied rather
    /// than evicted from.
    pub fn remember(&self, path: &VfsPath, caps: Capabilities) {
        let mut map = self.known();
        if map.len() >= MAX_REMEMBERED_CAPABILITIES {
            map.clear();
        }
        map.insert(path.clone(), caps);
    }

    /// The remembered answer, or `None` when nothing has resolved this path.
    pub fn get(&self, path: &VfsPath) -> Option<Capabilities> {
        self.known().get(path).copied()
    }

    /// Forget one path's answer, because what services it has changed.
    ///
    /// A connection that dropped and a listing that was closed both mean the
    /// backend behind a path is not the one that answered, and a remembered
    /// answer would outlive its subject.
    pub fn forget(&self, path: &VfsPath) {
        self.known().remove(path);
    }

    /// Forget every answer for a path and everything under it.
    ///
    /// The unit a backend goes away in is a subtree, not a path: closing a
    /// connection invalidates the answer for `remote:/` and for every
    /// directory on it that a tab has been in.
    pub fn forget_subtree(&self, root: &VfsPath) {
        self.known().retain(|path, _| !path.starts_with(root));
    }
}

impl Drop for VfsRouter {
    /// Stop every walk still filling a listing.
    ///
    /// The process is going away, but a walk holds worker threads and a sink;
    /// cancelling is one atomic store per listing and it means the shutdown
    /// path has no thread still reading the filesystem behind it.
    fn drop(&mut self) {
        for (_, listing) in self.listings().drain() {
            listing.cancel();
        }
    }
}

impl std::fmt::Debug for VfsRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsRouter")
            .field("session", &self.session.get().map(|s| s.temp_root()))
            .finish()
    }
}

impl VfsRouter {
    /// A router over the real local filesystem.
    pub fn new(archive: ArchiveConfig, remote: RemoteConfig) -> Self {
        Self {
            local: Arc::new(LocalFs::new()),
            archive,
            remote_config: remote,
            remote: Arc::new(RemoteRegistry::new()),
            session: std::sync::OnceLock::new(),
            listings: std::sync::Mutex::new(HashMap::new()),
            next_listing: std::sync::atomic::AtomicU64::new(0),
            capabilities: Arc::new(CapabilityCache::default()),
        }
    }

    /// The live connections.
    pub fn remotes(&self) -> &Arc<RemoteRegistry> {
        &self.remote
    }

    /// `[remote]`, for the connect task.
    pub fn remote_config(&self) -> &RemoteConfig {
        &self.remote_config
    }

    /// The archive session, created on first use.
    ///
    /// A failure to create it is returned rather than remembered, so a
    /// `$TMPDIR` that was full or missing when the first archive was opened
    /// does not poison every later attempt.
    pub fn session(&self) -> Result<&Arc<ArchiveSession>> {
        if let Some(open) = self.session.get() {
            return Ok(open);
        }
        // the startup sweep. Here rather than in the event loop
        // because this is the moment the temp root is first named, and a sweep
        // that runs before anything of ours is in the directory cannot race
        // with our own session.
        ArchiveSession::sweep_orphans(&archive_temp_base(&self.archive));
        let fresh = ArchiveSession::new(&self.archive)?;
        // A racing caller may have won; theirs is the session, and this one's
        // temp directory is removed by its own `Drop`.
        let session = self.session.get_or_init(|| fresh);
        // an archive can sit on a remote, so the session has to
        // be able to reach the live connections. Set once, ignored afterwards.
        session.attach_remotes(Arc::clone(&self.remote));
        Ok(session)
    }

    /// The session, only if one has been created. Never creates one, so it is
    /// safe to call from anywhere.
    pub fn open_session(&self) -> Option<&Arc<ArchiveSession>> {
        self.session.get()
    }

    /// The registry lock, with a poisoned one recovered rather than escalated.
    ///
    /// What is behind it is a map of `Arc`s: a panicking thread cannot leave
    /// it half-written, and losing every open search result to one unrelated
    /// panic would be a worse answer than carrying on.
    fn listings(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<ListFs>>> {
        self.listings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a virtual listing and hand back the id its `list:` path names.
    ///
    ///
    /// `Err` past [`MAX_LISTINGS`], which `App` makes unreachable by
    /// forgetting a listing whenever a tab leaves one. Never a silent
    /// eviction: the evicted one would be the results a panel is showing.
    pub fn register_listing(&self, listing: Arc<ListFs>) -> Result<ListingId> {
        let mut map = self.listings();
        if map.len() >= MAX_LISTINGS {
            return Err(Error::msg(format!(
                "{MAX_LISTINGS} virtual listings are already open; leave one with Ctrl+R first"
            )));
        }
        // `fetch_add` returns the previous value, so the first id is 1 and
        // `list:/0` is never a listing anyone registered.
        let id = ListingId(
            self.next_listing
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                .saturating_add(1),
        );
        // The listing knows what it can do the moment it is built - a search
        // over local roots is writable, one over an archive root is not - and
        // that is long before its first row arrives. Recorded here so the
        // panel showing a *filling* listing is gated on the real answer:
        // seeding it pessimistically and upgrading it only when the walk
        // finished is what refused `F6` out of a search result for as long as
        // the search ran, which is exactly the window in which acting on the
        // rows is the point.
        self.capabilities
            .remember(&id.to_path(), listing.capabilities());
        map.insert(id.0, listing);
        Ok(id)
    }

    /// The listing an id names, or `None` once it has been forgotten.
    pub fn listing(&self, id: ListingId) -> Option<Arc<ListFs>> {
        self.listings().get(&id.0).map(Arc::clone)
    }

    /// Forget one, cancelling it first so its walk stops.
    ///
    /// Cancelling *is* the forgetting: there is one cancellation flag in the
    /// search path and it is the listing's own, so no caller has to remember
    /// to stop the walk separately.
    pub fn forget_listing(&self, id: ListingId) {
        let listing = self.listings().remove(&id.0);
        if let Some(listing) = listing {
            listing.cancel();
        }
        // A remembered answer must not outlive the backend it describes. Ids
        // are never reused, so this is tidiness rather than correctness for
        // *this* id - but leaving it would grow the cache by one entry per
        // search for the life of the session.
        self.capabilities.forget_subtree(&id.to_path());
    }

    /// How many are registered. Zero once every tab has left its results.
    pub fn listing_count(&self) -> usize {
        self.listings().len()
    }

    /// The one place a capability answer is kept.
    pub fn capability_cache(&self) -> &Arc<CapabilityCache> {
        &self.capabilities
    }

    /// What the backend servicing `path` can do, **without ever blocking**.
    ///
    /// This is the question `dispatch` asks, and `dispatch` may touch no
    /// filesystem. A path the panel has listed is a
    /// hit and the answer is the honest one - `.rar` reports read-only and a
    /// search over local files reports writable. A path nothing has resolved
    /// yet falls back to [`BackendKind::capabilities`], the conservative
    /// path-free answer, which is what every such caller used before this
    /// cache existed: under-promising costs a refusal the user can retry,
    /// over-promising costs a copy that fails halfway.
    pub fn known_capabilities(&self, path: &VfsPath) -> Capabilities {
        self.capabilities
            .get(path)
            .unwrap_or_else(|| path.backend().capabilities())
    }

    /// Resolve `path`'s backend, ask it, and remember the answer.
    ///
    /// The blocking half of [`VfsRouter::capabilities_for`], kept separate so
    /// the reading and the recording are one act and cannot drift, and public
    /// so a caller that has just made a backend exist - a connection coming
    /// up - can fill the cache at the moment the answer is free rather than
    /// leaving the first gate to under-promise.
    ///
    /// **May block**, for whatever `path`'s backend has to do to answer.
    pub fn resolve_capabilities(&self, path: &VfsPath) -> Capabilities {
        let caps = match self.backend_for(path) {
            Ok(backend) => backend.capabilities_for(path),
            // A backend that cannot be opened is not remembered: the next
            // caller should try again rather than inherit a failure that may
            // have been a full `$TMPDIR` or a connection that has since come
            // back.
            Err(_) => return path.backend().capabilities(),
        };
        self.capabilities.remember(path, caps);
        caps
    }

    /// The backend that services `path`.
    ///
    /// This is the whole of the routing rule: a path whose innermost segment
    /// is [`BackendKind::Archive`] belongs to the archive named by every
    /// segment above it, one that is [`BackendKind::List`] belongs to the
    /// registered listing it names, and everything else is local.
    ///
    /// The `List` arm is new in v0.6 and the wildcard that used to swallow it
    /// is gone: routing a `list:` path to [`LocalFs`] listed `/` instead of
    /// the search results, which had never been reached only because nothing
    /// produced such a path. It is now an explicit arm per backend so a fourth
    /// one cannot slip through the same way.
    pub fn backend_for(&self, path: &VfsPath) -> Result<Arc<dyn Vfs>> {
        match path.backend() {
            BackendKind::Archive => {
                let fs = self.session()?.open(path)?;
                Ok(fs as Arc<dyn Vfs>)
            }
            BackendKind::List => {
                let id = ListingId::from_path(path);
                id.and_then(|id| self.listing(id))
                    .map(|fs| fs as Arc<dyn Vfs>)
                    .ok_or_else(|| Error::msg(format!("{path}: that listing has been closed")))
            }
            BackendKind::Local => Ok(Arc::clone(&self.local) as Arc<dyn Vfs>),
            // The history of the repository the path's outer segment names.
            // Rebuilt per call rather than cached: gix discovers the repo from
            // the directory each time, and holding an open handle would pin a
            // `.git` a panel may have left.
            BackendKind::Git => {
                let fs = crate::vfs::git::GitFs::open(path.clone())?;
                Ok(Arc::new(fs) as Arc<dyn Vfs>)
            }
            // An explicit arm per backend as v0.6 made it: a connection that
            // has been closed names nothing, and the message says so rather
            // than listing `/`.
            BackendKind::Remote(id) => crate::remote::backend_for(&self.remote, id, path),
            // The session resolves the stack and hands back the open backend,
            // whose `Capabilities` are the honest ones for what turned out to
            // be on it.
            BackendKind::Image => {
                let fs = self.session()?.open_image(path)?;
                Ok(fs as Arc<dyn Vfs>)
            }
        }
    }
}

/// Where [`ArchiveSession`] will put its temp directory, for the startup sweep.
///
/// The same rule `ArchiveSession::new` applies to `[archive] temp_dir`: empty
/// means `$TMPDIR`.
fn archive_temp_base(config: &ArchiveConfig) -> std::path::PathBuf {
    if config.temp_dir.trim().is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(config.temp_dir.trim())
    }
}

/// A receiver that carries one failure and nothing else.
///
/// Used where a listing could not even be started. It is deliberately *not*
/// preceded by a `..` row: [`crate::app::App::apply_vfs_event`] tells "the archive would
/// not open" from "the archive opened and its index failed" by whether
/// anything arrived at all, and the second case has to keep its `..` so the
/// user can get out.
fn failed_listing(err: Error) -> tokio::sync::mpsc::Receiver<Result<Entry>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let _ = tx.send(Err(err)).await;
    });
    rx
}

impl Vfs for VfsRouter {
    /// The local filesystem's. See the type's own documentation.
    fn kind(&self) -> BackendKind {
        self.local.kind()
    }

    /// Stream a listing from whichever backend owns `path`.
    ///
    /// Opening an archive is **not** done on the caller's thread: detection
    /// reads the container's first block and a nested archive is extracted to
    /// the session cache first, either of which can take long
    /// enough to matter, and `read_dir` is documented to return its receiver
    /// immediately. The open therefore happens inside the task, on the
    /// blocking pool, and the panel shows "reading..." until it finishes -
    /// which is what the design asks of every backend.
    fn read_dir(&self, path: &VfsPath) -> tokio::sync::mpsc::Receiver<Result<Entry>> {
        match path.backend() {
            BackendKind::Local => {
                // The one write that costs a lookup rather than riding on an
                // open that was happening anyway. It is here rather than
                // nowhere so that every arm of this match leaves the cache
                // holding an answer for the directory it just listed - a panel
                // can then gate a key on the directory it is standing in
                // whatever backend that is, which is the property the whole
                // cache exists for.
                self.capabilities
                    .remember(path, self.local.capabilities_for(path));
                return self.local.read_dir(path);
            }
            // A registered listing is a `Vfs` like any other, and streaming
            // from it is what makes the "results stream back over
            // a channel" the ordinary directory-read channel.
            BackendKind::List | BackendKind::Git => {
                return match self.backend_for(path) {
                    Ok(listing) => {
                        self.capabilities
                            .remember(path, listing.capabilities_for(path));
                        listing.read_dir(path)
                    }
                    Err(err) => failed_listing(err),
                };
            }
            // The remote backend already returns its receiver immediately and
            // puts itself on the blocking pool, so the archive arm's extra
            // task below is not needed.
            BackendKind::Remote(_) => {
                return match self.backend_for(path) {
                    Ok(remote) => {
                        // Fixed when the connection was established, so this
                        // is a field read and not a round trip.
                        self.capabilities
                            .remember(path, remote.capabilities_for(path));
                        remote.read_dir(path)
                    }
                    Err(err) => failed_listing(err),
                };
            }
            // A disk image opens the way an archive does and for the same
            // reason: `format::detect` reads the container's first block and
            // an image that is not a local file is copied into the session
            // cache first, either of which can take long enough to matter.
            //
            BackendKind::Image => {
                let session = match self.session() {
                    Ok(session) => Arc::clone(session),
                    Err(err) => return failed_listing(err),
                };
                let (tx, rx) = tokio::sync::mpsc::channel(crate::vfs::READ_DIR_CHANNEL_DEPTH);
                let path = path.clone();
                let cache = Arc::clone(&self.capabilities);
                tokio::spawn(async move {
                    let opened = {
                        let path = path.clone();
                        tokio::task::spawn_blocking(move || session.open_image(&path)).await
                    };
                    let image = match opened {
                        Ok(Ok(image)) => image,
                        Ok(Err(err)) => {
                            let _ = tx.send(Err(err)).await;
                            return;
                        }
                        Err(join) => {
                            let _ = tx.send(Err(Error::msg(format!("{path}: {join}")))).await;
                            return;
                        }
                    };
                    // Opening the image is what learns which filesystem is on
                    // it, and therefore what a member of it can do. Recorded
                    // here because this is the moment it is known and the
                    // backend is in hand; asking again later would open the
                    // image a second time from the thread that draws.
                    cache.remember(&path, image.capabilities_for(&path));
                    // The `Arc` is held for as long as the listing streams:
                    // dropping the last handle to a volume cancels its index
                    // build, and the listing is the thing reading that index.
                    let mut inner = image.read_dir(&path);
                    while let Some(item) = inner.recv().await {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                    drop(image);
                });
                return rx;
            }
            BackendKind::Archive => {}
        }
        let session = match self.session() {
            Ok(session) => Arc::clone(session),
            Err(err) => return failed_listing(err),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(crate::vfs::READ_DIR_CHANNEL_DEPTH);
        let path = path.clone();
        let cache = Arc::clone(&self.capabilities);
        tokio::spawn(async move {
            let opened = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || session.open(&path)).await
            };
            let archive = match opened {
                Ok(Ok(archive)) => archive,
                Ok(Err(err)) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Err(join) => {
                    let _ = tx.send(Err(Error::msg(format!("{path}: {join}")))).await;
                    return;
                }
            };
            // Detection is what learned which format this is, and the format
            // is the whole of the answer: `.rar` is read-only and a nested
            // archive is read-only whatever its format says. Recorded here
            // because this is the moment it is known - asking later, from the
            // thread that draws, is what used to make the question unaskable.
            cache.remember(&path, archive.capabilities_for(&path));
            // The `Arc` is held for as long as the listing streams: dropping
            // the last handle to an archive cancels its index build, and the
            // listing is exactly the thing reading that index.
            let mut inner = archive.read_dir(&path);
            while let Some(item) = inner.recv().await {
                if tx.send(item).await.is_err() {
                    return;
                }
            }
            drop(archive);
        });
        rx
    }

    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        self.backend_for(path)?.stat(path)
    }

    fn open_read(&self, path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        self.backend_for(path)?.open_read(path)
    }

    fn open_seek(&self, path: &VfsPath) -> Result<Box<dyn crate::vfs::ReadSeek + Send>> {
        self.backend_for(path)?.open_seek(path)
    }

    fn open_write(&self, path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        self.backend_for(path)?.open_write(path)
    }

    fn create_dir(&self, path: &VfsPath) -> Result<()> {
        self.backend_for(path)?.create_dir(path)
    }

    fn remove(&self, path: &VfsPath) -> Result<()> {
        self.backend_for(path)?.remove(path)
    }

    /// The servicing backend's answer, because what a symbolic link's target
    /// is - and whether it is allowed - is the backend's to say.
    fn read_link(&self, path: &VfsPath) -> Result<String> {
        self.backend_for(path)?.read_link(path)
    }

    /// A rename is refused across backends: the two sides address different
    /// namespaces, and "rename" would silently mean "copy and delete".
    fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        if from.backend() != to.backend() {
            return Err(Error::InvalidPath(format!(
                "{from} and {to} are on different backends; \
                 copy or move them rather than renaming"
            )));
        }
        self.backend_for(from)?.rename(from, to)
    }

    /// The local filesystem's. See the type's own documentation.
    fn capabilities(&self) -> Capabilities {
        self.local.capabilities()
    }

    /// The servicing backend's, which for an archive is its **format's**:
    /// `.rar` reports `writable: false` and a streamed member reports
    /// `seekable: false`.
    ///
    /// This is the answer the design wants consulted "before offering an
    /// operation", and it is the reason this method exists on the trait at
    /// all: [`Vfs::capabilities`] takes no path, and a router has no answer
    /// that is true for every path at once.
    ///
    /// **Answered from the cache whenever the cache has it**, which is what
    /// makes this callable from a job thread and from the event loop alike:
    /// [`VfsRouter::read_dir`] records the answer for every directory it
    /// streams, so a path a panel is standing in costs a hash lookup rather
    /// than the archive open the honest answer used to need. A miss resolves
    /// the backend and records what it said, so the second caller is a hit
    /// even when the first was not.
    ///
    /// A backend that cannot be opened falls back to
    /// [`BackendKind::capabilities`]'s conservative answer rather than to the
    /// local filesystem's: under-promising costs a refusal the user can retry,
    /// over-promising costs a copy that fails halfway.
    fn capabilities_for(&self, path: &VfsPath) -> Capabilities {
        if let Some(known) = self.capabilities.get(path) {
            return known;
        }
        self.resolve_capabilities(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A router over compiled-in defaults. Builds nothing on the filesystem:
    /// the archive session is created on first use and no test here reaches
    /// for one.
    fn router() -> VfsRouter {
        VfsRouter::new(ArchiveConfig::default(), RemoteConfig::default())
    }

    /// **The cache is the answer, not a hint.**
    ///
    /// Proved by putting something in it that no backend would ever say and
    /// watching `capabilities_for` report it: a local path really does resolve
    /// to [`LocalFs`], so an answer that is not `LOCAL` can only have come from
    /// the cache. That is what makes the method callable from anywhere - a hit
    /// does not touch a backend at all.
    #[test]
    fn a_remembered_answer_is_returned_without_resolving_the_backend() {
        let router = router();
        let path = VfsPath::local("/etc");
        assert_eq!(router.capabilities_for(&path), Capabilities::LOCAL);

        router
            .capability_cache()
            .remember(&path, Capabilities::ARCHIVE_UNKNOWN);
        assert_eq!(
            router.capabilities_for(&path),
            Capabilities::ARCHIVE_UNKNOWN
        );
        assert_eq!(
            router.known_capabilities(&path),
            Capabilities::ARCHIVE_UNKNOWN
        );
    }

    /// A path nothing has resolved gets the conservative path-free answer, and
    /// gets it without blocking. This is what every gate saw before the cache
    /// existed, so a miss is never worse than the old behaviour.
    #[test]
    fn an_unknown_path_falls_back_to_the_conservative_answer() {
        let router = router();
        let inside_an_archive = VfsPath::local("/a/b.rar").with_segment(BackendKind::Archive, "/");
        assert_eq!(
            router.known_capabilities(&inside_an_archive),
            Capabilities::ARCHIVE_UNKNOWN
        );
    }

    /// **Reading a directory is what fills the cache**, which is the property
    /// the whole design rests on: by the time a panel can gate a key on a
    /// directory, that directory has been listed.
    #[tokio::test]
    async fn reading_a_directory_records_what_its_backend_can_do() {
        let router = router();
        let path = VfsPath::local("/");
        assert_eq!(router.capability_cache().get(&path), None, "the premise");

        let mut rows = router.read_dir(&path);
        // Drained so the walk is not left parked on a full channel; what is in
        // the rows is not what is under test.
        while rows.recv().await.is_some() {}

        assert_eq!(
            router.capability_cache().get(&path),
            Some(Capabilities::LOCAL),
            "listing a directory did not record what its backend can do"
        );
    }

    /// **A listing knows what it can do before its first row arrives.**
    ///
    /// This is the search panel's `F6`: the answer is available at
    /// registration, so gating on it does not have to wait for the walk to
    /// finish - which is the window in which acting on the rows is the point.
    #[test]
    fn registering_a_listing_records_its_answer_before_any_row_arrives() {
        let router = router();
        let (listing, sink) = ListFs::streaming("search", &[VfsPath::local("/tmp")]);
        let id = router.register_listing(listing).expect("the first listing");

        let caps = router.known_capabilities(&id.to_path());
        assert!(
            caps.writable,
            "a search over a local root is writable from its first frame"
        );
        assert!(sink.is_empty(), "and no row has arrived to say so");
    }

    /// A remembered answer does not outlive the backend it describes.
    #[test]
    fn forgetting_a_listing_forgets_what_it_could_do() {
        let router = router();
        let (listing, _sink) = ListFs::streaming("search", &[VfsPath::local("/tmp")]);
        let id = router.register_listing(listing).expect("the first listing");
        assert!(router.capability_cache().get(&id.to_path()).is_some());

        router.forget_listing(id);
        assert_eq!(
            router.capability_cache().get(&id.to_path()),
            None,
            "the listing is gone and its answer described it"
        );
    }

    /// The subtree is the unit a backend goes away in: a connection closing
    /// invalidates the root and every directory on it a tab has been in.
    #[test]
    fn forgetting_a_subtree_takes_everything_under_it_and_nothing_beside_it() {
        let cache = CapabilityCache::default();
        let root = VfsPath::local("/srv");
        let under = VfsPath::local("/srv/media");
        // `/srvx` is not under `/srv`, which a string prefix would get wrong.
        let beside = VfsPath::local("/srvx");
        for path in [&root, &under, &beside] {
            cache.remember(path, Capabilities::LOCAL);
        }

        cache.forget_subtree(&root);

        assert_eq!(cache.get(&root), None);
        assert_eq!(cache.get(&under), None);
        assert_eq!(cache.get(&beside), Some(Capabilities::LOCAL));
    }

    /// The cache is emptied rather than grown without bound, and emptying it
    /// is always safe because a miss resolves.
    #[test]
    fn the_cache_stops_growing() {
        let cache = CapabilityCache::default();
        for row in 0..=MAX_REMEMBERED_CAPABILITIES {
            cache.remember(&VfsPath::local(format!("/{row}")), Capabilities::LOCAL);
        }
        assert!(cache.known().len() <= MAX_REMEMBERED_CAPABILITIES);
    }

    /// A backend that could not be opened is **not** remembered: the next
    /// caller should try again rather than inherit a `$TMPDIR` that was full
    /// or a connection that has since come back.
    #[test]
    fn a_backend_that_would_not_open_is_not_remembered() {
        let router = router();
        // No listing with this id was ever registered, so `backend_for` fails.
        let gone = VfsPath::new(BackendKind::List, "/9999");
        assert_eq!(
            router.capabilities_for(&gone),
            Capabilities::READ_ONLY_LIST,
            "the conservative answer for the kind"
        );
        assert_eq!(
            router.capability_cache().get(&gone),
            None,
            "a failure to open is not an answer worth keeping"
        );
    }
}
