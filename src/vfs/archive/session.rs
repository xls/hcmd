//! The session: nesting, the temp cache, and the gates.
//!
//! # Nesting
//!
//! > Nested archives work through the `VfsPath` segment stack; extract the
//! > inner archive to a temp file, cached for the session, and clean up on
//! > exit.
//!
//! `/home/t/a.tar.gz#/inner/b.zip#/file.txt` is three segments. The last one
//! names a member; every segment before it names an archive. The archive for
//! segment *k* is opened from the container that segment *k-1* produced, which
//! makes [`ArchiveSession::open`] a short recursion:
//!
//! ```text
//!   [(Local, /home/t/a.tar.gz)]                     → open the real file
//!   [.., (Archive, /inner/b.zip)]                   → materialise that member
//!                                                     of the archive above,
//!                                                     then open the temp file
//! ```
//!
//! # Where the cache lives, and what bounds it
//!
//! One directory per process under `[archive] temp_dir` (or `$TMPDIR`, or
//! `/tmp`), named `hcmd-archive-<pid>-<nonce>` and created `0700` - an archive
//! may hold anything, so its extracted contents are never world-readable.
//! Removed when the session drops, and [`ArchiveSession::sweep_orphans`]
//! removes the directories of processes that are no longer running, which is
//! the startup sweep the design asks for.
//!
//! The cache is bounded by [`DEFAULT_CACHE_BUDGET`] bytes. Over budget, files
//! that nothing is using any more are deleted, oldest first; a file that is
//! still referenced is never deleted under its user, and if nothing can be
//! freed the extraction is refused with the numbers rather than filling the
//! disk. Bounded by *disk*, never by memory: nothing here is ever read into
//! RAM.
//!
//! # Two panels, one archive
//!
//! Both get the same `Arc<ArchiveFs>` and therefore the same index and the
//! same temp file: [`ArchiveSession::open`] keys on the archive's identity -
//! the outermost file's path, size and mtime, plus the member path of every
//! nested step - so the second panel is a map lookup. Two panels racing to
//! open the same inner archive extract it once, because extraction is
//! single-flight *per key*: they share one slot and the cache is re-checked
//! inside it. Two panels opening two different archives share nothing and
//! wait for nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ArchiveConfig;
use crate::error::{Error, Result};
use crate::vfs::{BackendKind, VfsPath};

use crate::vfs::image::{ImageFs, part as image_part};

use super::ArchiveFs;
use super::index::Member;

/// How many bytes of extracted members and inner archives one session keeps.
///
/// Not a configuration key: the `[archive]` table has three entries
/// and this is not one of them, and inventing settings is how a config file
/// stops being a reference. 2 GiB is enough for the nested archives a person
/// opens in a session and small enough that a forgotten session is not a disk
/// problem.
pub const DEFAULT_CACHE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// How many archives stay open at once.
///
/// Each one costs its index and, if it is nested, a temp file. Beyond this the
/// least recently opened is dropped from the registry - which stops its index
/// build if nothing else is holding it, and costs a rebuild if a panel comes
/// back to it.
pub const MAX_OPEN_ARCHIVES: usize = 16;

/// The prefix every session temp directory carries, so the sweep can recognise
/// one and nothing else.
const TEMP_PREFIX: &str = "hcmd-archive-";

/// The the design rewrite gates, as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewriteLimits {
    /// Warn and let the user cancel above this. `rewrite_warn_size`.
    pub warn: u64,
    /// Refuse outright above this. `rewrite_max_size`.
    pub max: u64,
}

impl Default for RewriteLimits {
    /// the defaults: warn at 256 MiB, refuse at 500 MiB.
    fn default() -> Self {
        Self {
            warn: 256 * 1024 * 1024,
            max: 500 * 1024 * 1024,
        }
    }
}

/// What the gates say about one rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteGate {
    /// Below every threshold: rewrite without asking.
    Proceed,
    /// Between `rewrite_warn_size` and `rewrite_max_size`. The message names
    /// the size and says the whole archive will be rewritten; the design
    /// requires the cancel to be the default button.
    Warn(String),
    /// Refused, with the reason and the numbers.
    Refuse(String),
}

impl RewriteLimits {
    /// Build from the `[archive]` configuration.
    pub fn from_config(config: &ArchiveConfig) -> Self {
        Self {
            warn: config.rewrite_warn_size.bytes(),
            max: config.rewrite_max_size.bytes(),
        }
    }

    /// Which gate a rewrite of `size` bytes falls into, given the free space
    /// on the filesystem holding `temp_dir`.
    ///
    /// The three checks of the design, in its order:
    ///
    /// 1. refuse above `rewrite_max_size`;
    /// 2. refuse when the disk cannot hold `size × 2` plus a tenth - the new
    ///    archive has to exist beside the original - reporting the actual
    ///    numbers;
    /// 3. warn between the two thresholds.
    pub fn gate(&self, size: u64, temp_dir: &Path, what: &str) -> RewriteGate {
        if size > self.max {
            return RewriteGate::Refuse(format!(
                "{what} is {} and rewriting it would mean rewriting all of it; \
                 the limit is {} (archive.rewrite_max_size). Extract it, change it, \
 and repack it deliberately",
                human(size),
                human(self.max),
            ));
        }
        let needed = size
            .saturating_mul(2)
            .saturating_add(size.saturating_div(5));
        if let Some(free) = free_space(temp_dir)
            && free < needed
        {
            return RewriteGate::Refuse(format!(
                "rewriting {what} needs {}, {} free on {}",
                human(needed),
                human(free),
                temp_dir.display(),
            ));
        }
        if size > self.warn {
            return RewriteGate::Warn(format!(
                "{what} is {}; adding to it rewrites the whole archive",
                human(size)
            ));
        }
        RewriteGate::Proceed
    }

    /// The refusal half of [`RewriteLimits::gate`], as a `Result`.
    ///
    /// The backstop [`super::ArchiveFs`] applies when a write arrives without
    /// having been through a dialog. A warning is not a refusal, so it passes
    /// here: only a human can answer a warning.
    pub fn check(&self, size: u64, temp_dir: &Path, what: &impl std::fmt::Display) -> Result<()> {
        match self.gate(size, temp_dir, &what.to_string()) {
            RewriteGate::Refuse(why) => Err(Error::msg(why)),
            RewriteGate::Proceed | RewriteGate::Warn(_) => Ok(()),
        }
    }
}

/// Free bytes on the filesystem holding `path`.
///
/// "Available space comes from `sysinfo`, matching the temp
/// path against the longest mount point; no new dependency." That match is
/// already written, for the panel's volume line.
fn free_space(path: &Path) -> Option<u64> {
    crate::ui::volume::for_path(path).map(|volume| volume.free)
}

/// Binary units, for a message a human reads.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit = unit.saturating_add(1);
    }
    let suffix = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {suffix}")
    } else {
        format!("{value:.1} {suffix}")
    }
}

/// The identity of one archive, as a cache key.
///
/// Includes the outermost file's size and mtime, so an archive that is
/// replaced on disk is a *different* archive: the old index keeps serving
/// whoever still holds it, and the next open builds a new one rather than
/// reading through stale offsets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArchiveKey(String);

impl std::fmt::Display for ArchiveKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one cache entry is filed under: the archive, and the member inside it.
///
/// The member is empty for an entry that is a whole archive rather than
/// something inside one, which is what a remote fetch caches.
type CacheKey = (ArchiveKey, String);

/// One running extraction, held by whoever is doing it.
///
/// The lock is the single flight; there is nothing to carry inside it, because
/// the result is handed over through the cache rather than through the slot.
type Flight = Arc<Mutex<()>>;

/// One extracted file in the session cache.
///
/// Deleted when the last holder drops it *and* the cache has decided to evict
/// it; while anything holds an `Arc<CachedFile>` the file stays.
#[derive(Debug)]
pub struct CachedFile {
    path: PathBuf,
    bytes: u64,
}

impl CachedFile {
    /// Where the extracted bytes are.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How much disk it is using.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Disk the cache has promised to an extraction that has not produced it yet.
///
/// Extractions run in parallel, so a budget check that counted only finished
/// files would let two of them each find room for itself and jointly pass
/// [`DEFAULT_CACHE_BUDGET`]. The charge is taken up front against the size the
/// header or the remote `stat` claimed, settled against what was actually
/// written, and refunded if nothing was.
struct Charge<'a> {
    session: &'a ArchiveSession,
    bytes: u64,
}

impl Charge<'_> {
    /// Replace the claim with what the extraction really wrote.
    fn settle(mut self, written: u64) {
        {
            let mut state = self.session.lock();
            state.cache_bytes = state
                .cache_bytes
                .saturating_sub(self.bytes)
                .saturating_add(written);
        }
        // Settled, so the refund below has nothing left to give back.
        self.bytes = 0;
    }
}

impl Drop for Charge<'_> {
    /// An extraction that failed, or was never run because the size could not
    /// be had, gives its promise back.
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut state = self.session.lock();
        state.cache_bytes = state.cache_bytes.saturating_sub(self.bytes);
    }
}

#[derive(Default)]
struct State {
    archives: HashMap<ArchiveKey, Arc<ArchiveFs>>,
    /// Least-recently-opened first.
    order: Vec<ArchiveKey>,
    /// The open disk images, keyed by container **and**
    /// partition so that partition 1 and partition 2 of one image are two
    /// entries with two indexes. A second map with the same rules, sharing
    /// nothing mutable with the first.
    images: HashMap<ArchiveKey, Arc<ImageFs>>,
    /// Least-recently-opened first, capped separately at
    /// [`MAX_OPEN_ARCHIVES`].
    image_order: Vec<ArchiveKey>,
    cached: HashMap<CacheKey, Arc<CachedFile>>,
    /// Insertion order of `cached`, for eviction.
    cache_order: Vec<CacheKey>,
    cache_bytes: u64,
}

/// Everything an archive needs that outlives one archive: the temp directory,
/// the open-archive registry, and the extracted-member cache.
///
/// One per application run. `App` holds it; [`ArchiveFs`] holds a `Weak` back
/// to it, so dropping the session cleans up even while archives are open.
pub struct ArchiveSession {
    root: PathBuf,
    state: Mutex<State>,
    /// The extractions and fetches that are running right now, keyed the way
    /// the cache is.
    ///
    /// Single-flight, not a queue. Two panels opening the same inner archive
    /// must extract it once, which is a statement about one key; a single
    /// global lock said it by making *every* extraction wait for every other,
    /// so one panel's whole network transfer stood in front of another
    /// panel's unrelated one. A slot per key says the same thing and lets
    /// distinct keys run at once.
    ///
    /// A slot is dropped as soon as nobody holds it any more, so this map is
    /// the size of what is in flight rather than of everything the session has
    /// ever extracted.
    inflight: Mutex<HashMap<CacheKey, Flight>>,
    limits: RewriteLimits,
    budget: u64,
    nonce: AtomicU64,
    /// The live remote connections, so an archive that sits on one can be
    /// opened.
    ///
    /// Set once by the router that owns both. `None` in a session built
    /// without one - a test, or the sweep - where a remote container is simply
    /// unreachable rather than a panic.
    remotes: std::sync::OnceLock<Arc<crate::remote::RemoteRegistry>>,
}

impl std::fmt::Debug for ArchiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveSession")
            .field("root", &self.root)
            .field("open", &self.open_count())
            .field("cache_bytes", &self.cache_bytes())
            .finish()
    }
}

impl ArchiveSession {
    /// Create a session under `[archive] temp_dir` (empty means `$TMPDIR`).
    pub fn new(config: &ArchiveConfig) -> Result<Arc<Self>> {
        let base = if config.temp_dir.trim().is_empty() {
            std::env::temp_dir()
        } else {
            PathBuf::from(config.temp_dir.trim())
        };
        Self::in_dir(base, RewriteLimits::from_config(config))
    }

    /// Create a session with an explicit base directory. Tests, and the sweep.
    pub fn in_dir(base: impl Into<PathBuf>, limits: RewriteLimits) -> Result<Arc<Self>> {
        let base = base.into();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = base.join(format!(
            "{TEMP_PREFIX}{pid}-{nanos:x}",
            pid = std::process::id()
        ));
        std::fs::create_dir_all(&root).map_err(|e| Error::io(&root, e))?;
        // An archive can hold anything; its extracted contents are this user's
        // business and nobody else's.
        let _ = std::fs::set_permissions(
            &root,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        );
        Ok(Arc::new(Self {
            root,
            state: Mutex::new(State::default()),
            inflight: Mutex::new(HashMap::new()),
            limits,
            budget: DEFAULT_CACHE_BUDGET,
            nonce: AtomicU64::new(0),
            remotes: std::sync::OnceLock::new(),
        }))
    }

    /// Remove the temp directories of sessions whose process is gone
    /// ("Sweep orphaned temp files from previous sessions at
    /// startup").
    ///
    /// Only directories with this program's own prefix, and only when
    /// `/proc/<pid>` says the owner is no longer running - a second hcmd
    /// running right now must keep its cache.
    pub fn sweep_orphans(base: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(base) else {
            return 0;
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
                continue;
            };
            let Some((pid, _)) = rest.split_once('-') else {
                continue;
            };
            let Ok(pid) = pid.parse::<u32>() else {
                continue;
            };
            if super::rewrite::pid_is_live(pid) {
                continue;
            }
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                removed = removed.saturating_add(1);
            }
        }
        removed
    }

    /// The session's temp directory.
    pub fn temp_root(&self) -> &Path {
        &self.root
    }

    /// The the design limits this session was configured with.
    pub fn limits(&self) -> RewriteLimits {
        self.limits
    }

    /// How many archives are open.
    pub fn open_count(&self) -> usize {
        self.lock().archives.len()
    }

    /// How much disk the cache is using.
    pub fn cache_bytes(&self) -> u64 {
        self.lock().cache_bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh path in the session directory, for a member being written
    /// (the design keeps the original extension so the editor's own
    /// filetype detection works).
    ///
    /// The last component of a member path is a name from inside an archive,
    /// so it is attacker-controlled. Everything that reaches
    /// here today comes from the index, whose paths [`normalize_member`] has
    /// already refused `..` in - but this is a `pub` function that builds a
    /// path by joining, and `join("..")` walks straight out of the directory
    /// that was just created. So the name is checked here too, and anything
    /// that is not a plain file name becomes `member` rather than being
    /// sanitised into something that only looks safe.
    pub fn scratch_file(&self, member_path: &str) -> Result<PathBuf> {
        let n = self.nonce.fetch_add(1, Ordering::Relaxed);
        let name = member_path.rsplit('/').next().unwrap_or("member");
        let safe: String = name
            .chars()
            .map(|c| if c == '/' || c == '\0' { '_' } else { c })
            .collect();
        // `file_name` is `None` for `.`, `..` and anything with a separator in
        // it, and `Some(safe)` exactly when the string is one ordinary
        // component - which is the only thing that may be joined on.
        let usable = Path::new(&safe)
            .file_name()
            .is_some_and(|component| component == std::ffi::OsStr::new(&safe));
        let dir = self.root.join(format!("scratch-{n:x}"));
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        Ok(dir.join(if usable { safe.as_str() } else { "member" }))
    }

    /// Give the session the registry of live connections, so an archive on a
    /// remote host can be opened.
    ///
    /// Called once, by the router that owns both; a second call is ignored,
    /// which is what makes it safe from any thread.
    pub fn attach_remotes(&self, remotes: Arc<crate::remote::RemoteRegistry>) {
        let _ = self.remotes.set(remotes);
    }

    /// Download an archive that lives on a remote host into the session cache
    /// and hand back the local copy.
    ///
    /// **Copied rather than opened in place**, and that is the decision worth
    /// stating: every archive format here seeks, and `Capabilities::SFTP` says
    /// `random_access` is false because a round trip per window is not cheap.
    /// Reading a `.tar.xz` index over SSH one window at a time is minutes;
    /// fetching it once and opening it locally is one transfer. The cache is
    /// the same one nested archives use, with the same budget and the same
    /// eviction rule.
    ///
    /// **Blocking.** It transfers a file.
    fn materialise_remote(
        &self,
        id: crate::remote::RemoteId,
        tail: &Path,
    ) -> Result<(ArchiveKey, Arc<CachedFile>)> {
        let display = VfsPath::new(BackendKind::Remote(id), tail.to_path_buf());
        let Some(registry) = self.remotes.get() else {
            return Err(Error::InvalidPath(format!(
                "{display}: this session has no connections"
            )));
        };
        let Some(fs) = registry.get(id) else {
            // The variant, not its sentence hand-rolled into an `Error::msg`:
            // a batch extracting out of an archive on a connection that went
            // away under it has to stop, and `ops::is_fatal` can only see that
            // if the classification survives. Spelling the same words as a
            // message made every remaining member fail with this line instead.
            return Err(Error::connection_closed(&display));
        };
        let key = remote_key(id, tail);
        // Two panels opening the same remote archive fetch it once; a panel
        // opening a different one does not queue behind this transfer.
        let cached = self.materialise_once(
            (key.clone(), String::new()),
            // A round trip, so it is asked for only once the cache has said
            // that this transfer is really going to happen.
            || Ok(crate::vfs::Vfs::stat(fs.as_ref(), &display)?.size),
            || {
                let n = self.nonce.fetch_add(1, Ordering::Relaxed);
                let dir = self.root.join(format!("remote-{n:x}"));
                std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
                // The original name, so format sniffing by extension still
                // works.
                let name = display.file_name().unwrap_or_else(|| "archive".to_string());
                let dest = dir.join(name);
                let written = (|| -> Result<u64> {
                    let mut reader = crate::vfs::Vfs::open_read(fs.as_ref(), &display)?;
                    let mut file = std::fs::File::create(&dest).map_err(|e| Error::io(&dest, e))?;
                    std::io::copy(&mut reader, &mut file).map_err(|e| Error::io(&dest, e))
                })()
                .inspect_err(|_| {
                    let _ = std::fs::remove_dir_all(&dir);
                })?;
                Ok((dest, written))
            },
        )?;
        Ok((key, cached))
    }

    /// The backend that services `path`, whose innermost segment must be an
    /// archive segment.
    ///
    /// `path` addresses a member; the archive is everything above it. Two
    /// callers asking for the same archive get the same instance, and
    /// therefore the same index.
    pub fn open(self: &Arc<Self>, path: &VfsPath) -> Result<Arc<ArchiveFs>> {
        let segments = path.segments();
        let Some((last_kind, _)) = segments.last() else {
            return Err(Error::InvalidPath(format!("{path} has no segments")));
        };
        if *last_kind != BackendKind::Archive {
            return Err(Error::InvalidPath(format!(
                "{path} is not inside an archive"
            )));
        }
        let cut = segments.len().saturating_sub(1);
        self.open_container(path, cut)
    }

    /// The archive named by the first `depth` segments of `path`.
    fn open_container(self: &Arc<Self>, path: &VfsPath, depth: usize) -> Result<Arc<ArchiveFs>> {
        let segments = path.segments();
        let head = segments.get(..depth).unwrap_or(&[]);
        let Some((kind, tail)) = head.last() else {
            return Err(Error::InvalidPath(format!("{path}: nothing to open")));
        };

        match kind {
            BackendKind::Local => {
                let key = local_key(tail)?;
                let display = VfsPath::local(tail.clone());
                self.get_or_open(key, tail.clone(), display)
            }
            BackendKind::Archive => {
                // The archive is a member of the archive above it: extract it
                // once, cache it for the session.
                let outer = self.open_container(path, depth.saturating_sub(1))?;
                let member_path = super::safety::member_key(tail)?;
                if member_path.is_empty() {
                    return Err(Error::InvalidPath(format!(
                        "{path}: an archive root is not itself an archive"
                    )));
                }
                let member = outer.index().stat_blocking(&member_path)?;
                let cached = self.materialise(&outer, &member)?;
                let mut display = outer.display_path().clone();
                display.push_segment(BackendKind::Archive, format!("/{member_path}"));
                let key = nested_key(&outer_key(&outer)?, &member_path);
                // The `Arc<CachedFile>` goes **into** the archive, not out of
                // scope: `make_room` evicts an entry only when nothing outside
                // the cache holds it, and dropping this handle here made an
                // inner archive that a panel was standing inside look unheld -
                // so a later extraction that crossed the budget unlinked its
                // container. See `ArchiveFs::open_nested`.
                self.get_or_open_nested(key, cached, display)
            }
            BackendKind::List | BackendKind::Git => Err(Error::InvalidPath(format!(
                "{path}: a virtual listing cannot hold an archive"
            ))),
            // An archive stored **inside** a disk image is the nesting of
            // the design in the direction this milestone does not build,
            // and it is refused by name so the refusal has a location and a
            // message rather than failing as "not an archive".
            //
            BackendKind::Image => Err(Error::InvalidPath(format!(
                "{path}: an archive inside a disk image cannot be opened yet; \
                 copy it out first"
            ))),
            // an archive can sit on a remote. It is fetched into
            // the session cache and opened from there, which is the same
            // mechanism a nested archive already uses and for the same reason
            // (see [`ArchiveSession::materialise_remote`]).
            BackendKind::Remote(id) => {
                let (key, cached) = self.materialise_remote(*id, tail)?;
                let display = VfsPath::new(BackendKind::Remote(*id), tail.clone());
                self.get_or_open_nested(key, cached, display)
            }
        }
    }

    /// The cached instance, or a new one.
    fn get_or_open(
        self: &Arc<Self>,
        key: ArchiveKey,
        container: PathBuf,
        display: VfsPath,
    ) -> Result<Arc<ArchiveFs>> {
        self.get_or_build(key, |session| ArchiveFs::open(session, container, display))
    }

    /// [`ArchiveSession::get_or_open`] for a nested archive, which holds its
    /// cache entry open for as long as it lives.
    fn get_or_open_nested(
        self: &Arc<Self>,
        key: ArchiveKey,
        cached: Arc<CachedFile>,
        display: VfsPath,
    ) -> Result<Arc<ArchiveFs>> {
        self.get_or_build(key, |session| {
            ArchiveFs::open_nested(session, cached, display)
        })
    }

    /// The registry half both of the above share.
    fn get_or_build(
        self: &Arc<Self>,
        key: ArchiveKey,
        build: impl FnOnce(&Arc<Self>) -> Result<ArchiveFs>,
    ) -> Result<Arc<ArchiveFs>> {
        if let Some(open) = self.touch(&key) {
            return Ok(open);
        }
        // Opening detects the format and starts the index thread; doing it
        // outside the lock keeps a slow container off everyone else's path.
        let fresh = Arc::new(build(self)?);
        let mut state = self.lock();
        // Someone may have opened the same archive while this one was being
        // built. Theirs wins, so there is exactly one index per archive.
        if let Some(open) = state.archives.get(&key) {
            return Ok(Arc::clone(open));
        }
        state.archives.insert(key.clone(), Arc::clone(&fresh));
        state.order.retain(|k| k != &key);
        state.order.push(key);
        while state.order.len() > MAX_OPEN_ARCHIVES {
            let evicted = state.order.remove(0);
            state.archives.remove(&evicted);
        }
        Ok(fresh)
    }

    /// Mark an archive as most recently used, returning it if it is open.
    fn touch(&self, key: &ArchiveKey) -> Option<Arc<ArchiveFs>> {
        let mut state = self.lock();
        let found = state.archives.get(key).map(Arc::clone)?;
        state.order.retain(|k| k != key);
        state.order.push(key.clone());
        Some(found)
    }

    /// The backend that services `path`, whose innermost segment must be an
    /// image segment.
    ///
    /// Resolves the stack the way [`ArchiveSession::open`] does, with one
    /// added rule: an `Image` segment above an `Image` segment whose image has
    /// a partition table is a **partition selector**, not a container. It
    /// cannot be both, because an image with a table has no filesystem at its
    /// own level for a member to live on.
    ///
    /// Two callers asking for the same image and partition get the same
    /// instance, and therefore the same index (I15).
    pub fn open_image(self: &Arc<Self>, path: &VfsPath) -> Result<Arc<ImageFs>> {
        let segments = path.segments();
        let Some((last_kind, _)) = segments.last() else {
            return Err(Error::InvalidPath(format!("{path} has no segments")));
        };
        if *last_kind != BackendKind::Image {
            return Err(Error::InvalidPath(format!(
                "{path} is not inside a disk image"
            )));
        }
        let cut = segments.len().saturating_sub(1);
        self.open_image_at(path, cut)
    }

    /// The image named by the first `depth` segments of `path`.
    fn open_image_at(self: &Arc<Self>, path: &VfsPath, depth: usize) -> Result<Arc<ImageFs>> {
        let segments = path.segments();
        let head = segments.get(..depth).unwrap_or(&[]);
        let Some((kind, tail)) = head.last() else {
            return Err(Error::InvalidPath(format!("{path}: nothing to open")));
        };

        match kind {
            BackendKind::Local => {
                let display = VfsPath::local(tail.clone());
                let key = image_key(&display, None)?;
                if let Some(open) = self.touch_image(&key) {
                    return Ok(open);
                }
                let container = tail.clone();
                let fresh = Arc::new(ImageFs::open(self, container, display, None)?);
                Ok(self.register_image(key, fresh))
            }
            // The image is a member of an archive: extract it once, cache it
            // for the session, and open the copy (the nesting, in
            // the direction the design builds).
            BackendKind::Archive => {
                let outer = self.open_container(path, depth.saturating_sub(1))?;
                let member_path = super::safety::member_key(tail)?;
                if member_path.is_empty() {
                    return Err(Error::InvalidPath(format!(
                        "{path}: an archive root is not a disk image"
                    )));
                }
                let mut display = outer.display_path().clone();
                display.push_segment(BackendKind::Archive, format!("/{member_path}"));
                let key = image_key(&display, None)?;
                if let Some(open) = self.touch_image(&key) {
                    return Ok(open);
                }
                let member = outer.index().stat_blocking(&member_path)?;
                let cached = self.materialise(&outer, &member)?;
                let fresh = Arc::new(ImageFs::open_cached(self, cached, display, None)?);
                Ok(self.register_image(key, fresh))
            }
            // An image on a remote is fetched into the session cache and
            // opened from there: the same mechanism, the same budget, the same
            // sweep.
            BackendKind::Remote(id) => {
                let display = VfsPath::new(BackendKind::Remote(*id), tail.clone());
                let key = image_key(&display, None)?;
                if let Some(open) = self.touch_image(&key) {
                    return Ok(open);
                }
                let (_, cached) = self.materialise_remote(*id, tail)?;
                let fresh = Arc::new(ImageFs::open_cached(self, cached, display, None)?);
                Ok(self.register_image(key, fresh))
            }
            // A partition selector, never a container: an image with a table
            // has no filesystem at its own level for a member segment to
            // address, so the rule is total and nothing is left to a heuristic.
            //
            BackendKind::Image => {
                let outer = self.open_image_at(path, depth.saturating_sub(1))?;
                let number = image_part::partition_number(tail)?;
                let key = image_key(outer.display_path(), Some(number))?;
                if let Some(open) = self.touch_image(&key) {
                    return Ok(open);
                }
                let fresh = Arc::new(outer.partition_view(number)?);
                Ok(self.register_image(key, fresh))
            }
            BackendKind::List | BackendKind::Git => Err(Error::InvalidPath(format!(
                "{path}: a virtual listing cannot hold a disk image"
            ))),
        }
    }

    /// Mark an image as most recently used, returning it if it is open.
    fn touch_image(&self, key: &ArchiveKey) -> Option<Arc<ImageFs>> {
        let mut state = self.lock();
        let found = state.images.get(key).map(Arc::clone)?;
        state.image_order.retain(|k| k != key);
        state.image_order.push(key.clone());
        Some(found)
    }

    /// File a freshly opened image, evicting the least recently opened once
    /// the cap is passed.
    ///
    /// Someone may have opened the same image while this one was being built.
    /// Theirs wins, so there is exactly one index per volume (I15).
    fn register_image(&self, key: ArchiveKey, fresh: Arc<ImageFs>) -> Arc<ImageFs> {
        let mut state = self.lock();
        if let Some(open) = state.images.get(&key) {
            return Arc::clone(open);
        }
        state.images.insert(key.clone(), Arc::clone(&fresh));
        state.image_order.retain(|k| k != &key);
        state.image_order.push(key);
        while state.image_order.len() > MAX_OPEN_ARCHIVES {
            let evicted = state.image_order.remove(0);
            state.images.remove(&evicted);
        }
        fresh
    }

    /// Forget an open image, so its index build stops once nothing else holds
    /// it. Called when the last tab leaves one.
    pub fn close_image(&self, key: &ArchiveKey) {
        let mut state = self.lock();
        state.images.remove(key);
        state.image_order.retain(|k| k != key);
    }

    /// How many images are open, for tests and for a status line.
    pub fn open_image_count(&self) -> usize {
        self.lock().images.len()
    }

    /// The registry key of an open image, for [`ArchiveSession::close_image`].
    pub fn image_key_for(image: &ImageFs) -> Result<ArchiveKey> {
        image_key(image.display_path(), image.partition())
    }

    /// The registry key of an open archive, for [`ArchiveSession::close`].
    pub fn key_for(archive: &ArchiveFs) -> Result<ArchiveKey> {
        outer_key(archive)
    }

    /// Forget an open archive, so its index build stops once nothing else
    /// holds it. Called when the last tab leaves an archive.
    pub fn close(&self, key: &ArchiveKey) {
        let mut state = self.lock();
        state.archives.remove(key);
        state.order.retain(|k| k != key);
    }

    /// Extract one member into the session cache, or return the copy already
    /// there.
    pub fn materialise(&self, archive: &ArchiveFs, member: &Member) -> Result<Arc<CachedFile>> {
        let key = (outer_key(archive)?, member.path.clone());
        self.materialise_once(
            key,
            || Ok(member.size),
            || {
                let n = self.nonce.fetch_add(1, Ordering::Relaxed);
                let dir = self.root.join(format!("member-{n:x}"));
                std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
                // The original name, so the editor's filetype detection and
                // the format sniffing of a nested archive both still work.
                let dest = dir.join(member.name());
                // Under the same guard every other read of a member is under
                // (`super::guard_for`): the charge was taken against the size
                // the header *declared*, and a header that lied would
                // otherwise fill the filesystem the session's temp directory
                // is on.
                let written = archive.extract_guarded(member, &dest).inspect_err(|_| {
                    let _ = std::fs::remove_dir_all(&dir);
                })?;
                Ok((dest, written))
            },
        )
    }

    /// Put the entry for `key` in the cache, running `produce` at most once
    /// however many callers ask for it at the same time.
    ///
    /// The cache is checked, the key's own slot is taken, and the cache is
    /// checked again inside it: the second caller of one key waits for the
    /// first and then finds the file, and callers of *different* keys never
    /// meet. `size` is what the entry is expected to cost - a claim, since it
    /// comes from a header or from a remote `stat` - and is asked for only
    /// once the work is really going to happen, because on a remote it is a
    /// round trip.
    fn materialise_once(
        &self,
        key: CacheKey,
        size: impl FnOnce() -> Result<u64>,
        produce: impl FnOnce() -> Result<(PathBuf, u64)>,
    ) -> Result<Arc<CachedFile>> {
        if let Some(hit) = self.lock().cached.get(&key).map(Arc::clone) {
            return Ok(hit);
        }
        let flight = self.flight(&key);
        let outcome = {
            let _slot = flight.lock().unwrap_or_else(|e| e.into_inner());
            self.produce_once(&key, size, produce)
        };
        // This caller lets go of the slot BEFORE asking whether it can be
        // forgotten, so the count `land` sees is the waiters and the map and
        // nothing else. Holding it across the question meant a waiter that
        // woke the instant the lock was released - between the guard dropping
        // and this handle dropping - saw a count one too high, declined to
        // remove the slot, and then nobody ever did: the map kept an entry per
        // archive for the life of the session.
        drop(flight);
        self.land(&key);
        outcome
    }

    /// The body [`ArchiveSession::materialise_once`] runs inside the slot.
    fn produce_once(
        &self,
        key: &CacheKey,
        size: impl FnOnce() -> Result<u64>,
        produce: impl FnOnce() -> Result<(PathBuf, u64)>,
    ) -> Result<Arc<CachedFile>> {
        if let Some(hit) = self.lock().cached.get(key).map(Arc::clone) {
            return Ok(hit);
        }
        let charge = self.charge(size()?)?;
        let (path, bytes) = produce()?;
        charge.settle(bytes);
        let cached = Arc::new(CachedFile { path, bytes });
        let mut state = self.lock();
        state.cached.insert(key.clone(), Arc::clone(&cached));
        state.cache_order.push(key.clone());
        Ok(cached)
    }

    /// The slot for one cache key, created if this is the first caller.
    ///
    /// Whoever wins the slot's lock does the work; everyone else finds the
    /// finished file in the cache when they get in.
    fn flight(&self, key: &CacheKey) -> Flight {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        match inflight.get(key) {
            Some(slot) => Arc::clone(slot),
            None => {
                let slot = Arc::new(Mutex::new(()));
                inflight.insert(key.clone(), Arc::clone(&slot));
                slot
            }
        }
    }

    /// Forget the slot for `key` once the last caller has left it.
    ///
    /// One strong reference is the map's own: nobody holds the slot and nobody
    /// is blocked on it, so whoever comes next makes a fresh one and finds the
    /// cache already warm. More than one means a waiter still needs this exact
    /// lock, so it stays and that waiter forgets it on the way out.
    ///
    /// The caller drops its own handle first, which is what makes one the
    /// right number to compare against.
    fn land(&self, key: &CacheKey) {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if inflight
            .get(key)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            inflight.remove(key);
        }
    }

    /// Take `wanted` bytes of the budget for an extraction that is about to
    /// run, evicting what nothing is using to find them.
    fn charge(&self, wanted: u64) -> Result<Charge<'_>> {
        let mut state = self.lock();
        Self::make_room_in(&mut state, self.budget, wanted)?;
        // Charged before the bytes exist, under the same lock that found the
        // room: two extractions running at once must not each be told there is
        // room for it.
        state.cache_bytes = state.cache_bytes.saturating_add(wanted);
        drop(state);
        Ok(Charge {
            session: self,
            bytes: wanted,
        })
    }

    /// [`ArchiveSession::make_room_in`], for the test that proves an open
    /// nested archive is not evicted out from under itself.
    #[cfg(test)]
    pub(crate) fn make_room_for_test(&self, wanted: u64) -> Result<()> {
        Self::make_room_in(&mut self.lock(), self.budget, wanted)
    }

    /// Free enough of the cache to hold `wanted` more bytes.
    ///
    /// Only entries nothing is using are evicted - a temp file pulled out from
    /// under an open viewer is a crash, not a saving - and when nothing can be
    /// freed the extraction is refused with the numbers rather than filling
    /// the filesystem.
    ///
    /// Takes the state rather than locking it, so that finding the room and
    /// [`ArchiveSession::charge`] taking it are one critical section.
    fn make_room_in(state: &mut State, budget: u64, wanted: u64) -> Result<()> {
        if state.cache_bytes.saturating_add(wanted) <= budget {
            return Ok(());
        }
        let mut keep = Vec::with_capacity(state.cache_order.len());
        for key in std::mem::take(&mut state.cache_order) {
            if state.cache_bytes.saturating_add(wanted) <= budget {
                keep.push(key);
                continue;
            }
            match state.cached.get(&key) {
                // Held only by the cache: nothing is reading it.
                Some(file) if Arc::strong_count(file) == 1 => {
                    let bytes = file.bytes();
                    state.cached.remove(&key);
                    state.cache_bytes = state.cache_bytes.saturating_sub(bytes);
                }
                Some(_) => keep.push(key),
                None => {}
            }
        }
        state.cache_order = keep;
        if state.cache_bytes.saturating_add(wanted) > budget {
            return Err(Error::msg(format!(
                "the archive cache is full: this needs {}, {} of {} is in use by \
                 archives that are still open",
                human(wanted),
                human(state.cache_bytes),
                human(budget),
            )));
        }
        Ok(())
    }
}

impl Drop for ArchiveSession {
    /// "Clean up on exit".
    ///
    /// The whole directory, so a file whose `CachedFile` leaked goes with it.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The key for an archive that is a real local file.
fn local_key(path: &Path) -> Result<ArchiveKey> {
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(ArchiveKey(format!(
        "local:{}:{}:{mtime}",
        path.display(),
        meta.len()
    )))
}

/// The key for an archive that lives on a remote host.
///
/// The connection id and the remote path, and **not** a size or an mtime: a
/// `stat` over the network costs a round trip on every path through the
/// registry, and the id is already unique per connection and never reused
/// (the design I1). The cost is that an archive replaced on the
/// server during one session keeps its downloaded copy until the connection is
/// closed, which is the same staleness window `remote.listing_ttl` accepts for
/// a listing.
fn remote_key(id: crate::remote::RemoteId, path: &Path) -> ArchiveKey {
    ArchiveKey(format!("remote:{}:{}", id.0, path.display()))
}

/// The key for one disk image, or for one partition of one.
///
/// [`ArchiveKey`] is reused rather than a second opaque key type: it is a
/// string key and a second one would be a second thing to keep unique against
/// the first. The partition is part of the key,
/// so partition 1 and partition 2 of one image are two entries with two
/// indexes, which is what makes a per-partition `IndexStatus` possible.
///
fn image_key(display: &VfsPath, partition: Option<usize>) -> Result<ArchiveKey> {
    let segments = display.segments();
    let Some(((kind, root), rest)) = segments.split_first() else {
        return Err(Error::InvalidPath(format!(
            "{display}: an image has no segments"
        )));
    };
    let mut key = match kind {
        BackendKind::Local => local_key(root)?,
        BackendKind::Remote(id) => remote_key(*id, root),
        BackendKind::List | BackendKind::Archive | BackendKind::Image | BackendKind::Git => {
            return Err(Error::InvalidPath(format!(
                "{display}: a disk image must start at a file"
            )));
        }
    };
    for (kind, tail) in rest {
        match kind {
            BackendKind::Archive => key = nested_key(&key, &super::safety::member_key(tail)?),
            BackendKind::Local
            | BackendKind::List
            | BackendKind::Remote(_)
            | BackendKind::Image
            | BackendKind::Git => {
                return Err(Error::InvalidPath(format!(
                    "{display}: only an archive holds a disk image"
                )));
            }
        }
    }
    Ok(ArchiveKey(match partition {
        Some(number) => format!("image:{key}#{number}"),
        None => format!("image:{key}"),
    }))
}

/// The key for an archive nested inside another.
fn nested_key(outer: &ArchiveKey, member_path: &str) -> ArchiveKey {
    ArchiveKey(format!("{outer}#{member_path}"))
}

/// The key of an already-open archive, rebuilt from what it knows.
fn outer_key(archive: &ArchiveFs) -> Result<ArchiveKey> {
    let segments = archive.display_path().segments();
    let Some(((kind, root), rest)) = segments.split_first() else {
        return Err(Error::InvalidPath(format!(
            "{}: an archive has no segments",
            archive.display_path()
        )));
    };
    let mut key = match kind {
        BackendKind::Local => local_key(root)?,
        // an archive on a remote is keyed by its connection and
        // its remote path, which is what `materialise_remote` filed it under.
        BackendKind::Remote(id) => remote_key(*id, root),
        BackendKind::List | BackendKind::Archive | BackendKind::Image | BackendKind::Git => {
            return Err(Error::InvalidPath(format!(
                "{}: an archive must start at a file",
                archive.display_path()
            )));
        }
    };
    for (kind, tail) in rest {
        if *kind != BackendKind::Archive {
            return Err(Error::InvalidPath(format!(
                "{}: only archives nest",
                archive.display_path()
            )));
        }
        key = nested_key(&key, &super::safety::member_key(tail)?);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_root_is_private_and_swept() {
        let base = std::env::temp_dir().join(format!("hcmd-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");

        let session = ArchiveSession::in_dir(&base, RewriteLimits::default()).expect("session");
        let root = session.temp_root().to_path_buf();
        assert!(root.exists());
        let mode = <std::fs::Metadata as std::os::unix::fs::MetadataExt>::mode(
            &std::fs::metadata(&root).expect("stat"),
        );
        assert_eq!(mode & 0o777, 0o700, "an archive's contents are private");

        // A directory belonging to a pid that cannot be running.
        let orphan = base.join(format!("{TEMP_PREFIX}4294967294-dead"));
        std::fs::create_dir_all(&orphan).expect("orphan");
        let mine = root.clone();
        assert_eq!(ArchiveSession::sweep_orphans(&base), 1);
        assert!(!orphan.exists(), "an orphan is swept");
        assert!(mine.exists(), "a live session's cache is left alone");

        drop(session);
        assert!(!mine.exists(), "the session cleans up on exit");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_rewrite_gates_are_spec_13_3s() {
        let limits = RewriteLimits::default();
        assert_eq!(limits.warn, 256 * 1024 * 1024);
        assert_eq!(limits.max, 500 * 1024 * 1024);

        let tmp = std::env::temp_dir();
        assert_eq!(limits.gate(1024, &tmp, "a.tar.gz"), RewriteGate::Proceed);

        let warn = limits.gate(300 * 1024 * 1024, &tmp, "a.tar.gz");
        match warn {
            RewriteGate::Warn(message) => {
                assert!(message.contains("rewrites the whole archive"), "{message}");
                assert!(message.contains("300.0 MiB"), "{message}");
            }
            other => panic!("expected a warning, got {other:?}"),
        }

        let refuse = limits.gate(900 * 1024 * 1024, &tmp, "a.tar.gz");
        match refuse {
            RewriteGate::Refuse(message) => {
                assert!(message.contains("500.0 MiB"), "{message}");
                assert!(message.contains("Extract it"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_free_space_gate_reports_the_numbers() {
        // A limit high enough that only the disk check can fire, and a size no
        // filesystem has room for twice over.
        let limits = RewriteLimits {
            warn: u64::MAX,
            max: u64::MAX,
        };
        let huge = u64::MAX / 4;
        match limits.gate(huge, &std::env::temp_dir(), "big.tar.zst") {
            RewriteGate::Refuse(message) => {
                assert!(message.contains("needs"), "{message}");
                assert!(message.contains("free on"), "{message}");
            }
            // A machine `sysinfo` cannot enumerate has no numbers to report,
            // and the other two gates still apply.
            other => assert_eq!(other, RewriteGate::Proceed),
        }
    }

    #[test]
    fn human_sizes_read_as_the_spec_writes_them() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(256 * 1024 * 1024), "256.0 MiB");
        assert_eq!(human(1536 * 1024 * 1024), "1.5 GiB");
    }

    /// A scratch file always lands *inside* the scratch directory, whatever the
    /// member is called.
    #[test]
    fn a_scratch_file_cannot_be_named_out_of_its_directory() {
        let base = std::env::temp_dir().join(format!("hcmd-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let session = ArchiveSession::in_dir(&base, RewriteLimits::default()).expect("session");
        let root = session.temp_root().to_path_buf();

        for hostile in ["..", ".", "a/..", "../../etc/passwd", "", "/", "x/./.."] {
            let path = session.scratch_file(hostile).expect("scratch");
            let parent = path.parent().expect("a parent");
            assert!(
                parent.starts_with(&root),
                "{hostile:?} escaped the session root: {}",
                path.display()
            );
            // The file itself is one component inside the scratch directory,
            // so no `..` survived into the path that will be written.
            assert!(
                path.file_name().is_some(),
                "{hostile:?} produced a path with no file name: {}",
                path.display()
            );
            assert!(
                !path.components().any(|c| c.as_os_str() == ".."),
                "{hostile:?} left a `..` in {}",
                path.display()
            );
        }

        // An ordinary name is still kept verbatim - the design wants the
        // extension so the editor's filetype detection works.
        let ordinary = session.scratch_file("dir/notes.txt").expect("scratch");
        assert_eq!(
            ordinary.file_name().and_then(|n| n.to_str()),
            Some("notes.txt")
        );

        drop(session);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A meeting point for two threads, with an answer instead of a hang.
    ///
    /// Not a `Barrier`: a barrier nobody else reaches blocks the test run for
    /// ever, and "the two extractions never overlapped" is precisely what
    /// these tests have to *report*.
    struct Meeting {
        arrived: Mutex<usize>,
        bell: std::sync::Condvar,
    }

    impl Meeting {
        fn new() -> Self {
            Self {
                arrived: Mutex::new(0),
                bell: std::sync::Condvar::new(),
            }
        }

        /// Arrive, and wait up to two seconds for `wanted` arrivals in all.
        /// False means they did not happen at the same time.
        fn meet(&self, wanted: usize) -> bool {
            let limit = std::time::Duration::from_secs(2);
            let start = std::time::Instant::now();
            let mut arrived = self.arrived.lock().unwrap();
            *arrived += 1;
            self.bell.notify_all();
            while *arrived < wanted {
                let left = limit.saturating_sub(start.elapsed());
                if left.is_zero() {
                    return false;
                }
                let (next, _) = self.bell.wait_timeout(arrived, left).unwrap();
                arrived = next;
            }
            true
        }
    }

    /// A session under its own directory, and the directory, so the test can
    /// take it away again.
    fn scratch_session(what: &str) -> (PathBuf, Arc<ArchiveSession>) {
        let base = std::env::temp_dir().join(format!("hcmd-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let session = ArchiveSession::in_dir(&base, RewriteLimits::default()).expect("session");
        (base, session)
    }

    /// The key one member of one archive is cached under.
    fn cache_key(name: &str) -> CacheKey {
        (ArchiveKey(format!("test:{name}")), format!("{name}.txt"))
    }

    /// Two panels opening two *different* archives extract at the same time.
    ///
    /// The whole point of a single-flight per key: with one global extraction
    /// lock the second thread cannot enter its extraction until the first has
    /// finished its own, so the meeting times out and this fails.
    #[test]
    fn two_different_members_extract_at_the_same_time() {
        let (base, session) = scratch_session("flight-apart");
        let meeting = Arc::new(Meeting::new());
        let overlapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let threads: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|name| {
                let session = Arc::clone(&session);
                let meeting = Arc::clone(&meeting);
                let overlapped = Arc::clone(&overlapped);
                std::thread::spawn(move || {
                    session.materialise_once(
                        cache_key(name),
                        || Ok(4),
                        || {
                            if meeting.meet(2) {
                                overlapped.fetch_add(1, Ordering::Relaxed);
                            }
                            let dest = session.temp_root().join(name);
                            std::fs::write(&dest, b"body").expect("write");
                            Ok((dest, 4))
                        },
                    )
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("joined").expect("materialised");
        }

        assert_eq!(
            overlapped.load(Ordering::Relaxed),
            2,
            "two different archives must not queue behind one another"
        );
        drop(session);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Two panels opening the *same* archive extract it once, and get the same
    /// file.
    #[test]
    fn the_same_member_is_extracted_once() {
        let (base, session) = scratch_session("flight-together");
        let inside = Arc::new(Meeting::new());
        let extractions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let first = {
            let session = Arc::clone(&session);
            let inside = Arc::clone(&inside);
            let extractions = Arc::clone(&extractions);
            std::thread::spawn(move || {
                session.materialise_once(
                    cache_key("shared"),
                    || Ok(4),
                    || {
                        extractions.fetch_add(1, Ordering::Relaxed);
                        // Hold the slot until the other caller is on its way
                        // in, so it really does arrive during this extraction.
                        assert!(inside.meet(2), "the second caller never arrived");
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        let dest = session.temp_root().join("shared");
                        std::fs::write(&dest, b"body").expect("write");
                        Ok((dest, 4))
                    },
                )
            })
        };

        assert!(inside.meet(2), "the extraction never started");
        let second = session
            .materialise_once(
                cache_key("shared"),
                || Ok(4),
                || {
                    extractions.fetch_add(1, Ordering::Relaxed);
                    let dest = session.temp_root().join("shared-again");
                    std::fs::write(&dest, b"body").expect("write");
                    Ok((dest, 4))
                },
            )
            .expect("materialised");
        let first = first.join().expect("joined").expect("materialised");

        assert_eq!(
            extractions.load(Ordering::Relaxed),
            1,
            "one key, one extraction"
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "both callers hold the one extracted file"
        );
        // The slot is gone once the last caller has left it, so the map does
        // not grow with every archive the session has ever opened.
        assert!(
            session
                .inflight
                .lock()
                .unwrap()
                .get(&cache_key("shared"))
                .is_none(),
            "the finished slot is forgotten"
        );
        assert_eq!(
            session.cache_bytes(),
            4,
            "the charge settled at what it wrote"
        );

        drop(session);
        let _ = std::fs::remove_dir_all(&base);
    }
}
