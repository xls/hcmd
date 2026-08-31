//! A synthetic listing over a fixed set of paths.
//!
//! This is what makes search results and the `Ctrl+B` branch view work with
//! every normal panel operation for free: the panel is looking at a `ListFs`,
//! and each [`Entry`] carries [`Entry::location`] - the path the row really
//! lives at - so copy, delete and rename address the real file rather than the
//! virtual one it is being shown in.
//!
//! # Two shapes, one backend
//!
//! [`ListFs::new`] is v0.1's: every row is known when the listing is built.
//! [`ListFs::streaming`] is v0.6's: the listing starts empty and is filled by
//! whoever holds its [`ListSink`] while the panel is already showing it
//! ("results stream back over a channel, with a live count").
//! Both are the same type because the panel must not be able to tell them
//! apart: [`Vfs::read_dir`] streams from either, and a search result is a
//! directory listing in every way that matters.
//!
//! # Not a directory tree
//!
//! [`Capabilities::has_directories`] is `false` here, which is the same shape
//! the design needs for an object store: rows are a flat set of addresses,
//! not a hierarchy. There is no `..` row - the design makes `Ctrl+R` / `Esc`
//! the way out of a virtual listing, not navigation.
//!
//! # Two rows can have one name
//!
//! A flat listing can hold two files called `mod.rs` from different
//! directories. That is why [`Entry::location`] exists, why marks are keyed on
//! [`Entry::mark_key`] rather than on the name, and why
//! [`ListFs::row`] looks a row up by its real home.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::{mpsc, watch};

use super::{
    BackendKind, Capabilities, Entry, LatencyClass, READ_DIR_CHANNEL_DEPTH, Vfs, VfsPath,
    unsupported,
};
use crate::error::{Error, Result};

/// How many rows arrive before the watch channel is poked.
///
/// A search of a large tree pushes rows far faster than a terminal can draw
/// them, and waking the reader once per row would spend most of a walk in the
/// scheduler. Sixty-four is the same order as `main`'s own `READ_DIR_BATCH`,
/// so a virtual listing reaches the panel in the same size of step a real
/// directory does.
pub const LIST_NOTIFY_BATCH: usize = 64;

/// The most rows one snapshot copies out of the shared state at a time.
///
/// The walk has no backpressure - [`ListSink::push`] never blocks - and it
/// outruns the panel by two orders of magnitude, so by the time the reader's
/// channel is full the unread tail is the whole result set. Copying that tail
/// in one allocation is a second deep clone of every row still in flight, held
/// for as long as it takes to hand the first of them over: on a 275k-row walk,
/// tens of seconds and tens of megabytes. Capping the copy makes the transient
/// a function of the batch instead of the result set, and costs one extra pass
/// round the loop per batch.
const SNAPSHOT_BATCH: usize = 128;

/// A snapshot of at most [`SNAPSHOT_BATCH`] rows from `cursor`, with the new
/// cursor, the status, and whether more rows are already waiting behind them.
///
/// The bound is the whole point (see [`SNAPSHOT_BATCH`]): a free function
/// rather than a closure inside [`ListFs::read_dir`] so that the property can
/// be asserted directly instead of inferred from what a drain happened to
/// deliver.
///
/// `more` comes from the same locked snapshot as the rows, so a reader that
/// acts on it cannot be acting on a listing that has since changed.
fn batch_from(shared: &Shared, cursor: usize) -> (usize, Vec<Entry>, ListStatus, bool) {
    let state = shared.state();
    let from = cursor.min(state.rows.len());
    let rest = state.rows.get(from..).unwrap_or(&[]);
    let take = rest.len().min(SNAPSHOT_BATCH);
    (
        from.saturating_add(take),
        rest.get(..take).unwrap_or(&[]).to_vec(),
        state.status.clone(),
        rest.len() > take,
    )
}

/// How long the reader waits for that poke before looking anyway.
///
/// The batching above is what keeps a million-row walk cheap; this is what
/// makes the last sixty-three rows of a slow one visible before it ends. A
/// listing that is still filling is the only thing that waits on it, and a
/// tenth of a second is below the threshold at which a human calls a panel
/// stalled.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The most virtual listings one session registers at once.
///
/// A backstop against a route nobody thought of rather than a budget: two
/// panels of nine tabs is eighteen, and a tab forgets its listing whenever it
/// leaves one. Registering past it is an error the search reports, never a
/// silent eviction of a listing a tab is showing.
pub const MAX_LISTINGS: usize = 64;

/// Which registered listing a `list:` path names.
///
/// The panel's path while it is showing search results is `list:/7`, so every
/// route that already works on a `VfsPath` - the tab, the state file's
/// refusal to persist one, `App::vfs_for` - keeps working with
/// no new field to thread through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListingId(pub u64);

impl ListingId {
    /// The path a panel sits at while it is showing this listing: `list:/7`.
    pub fn to_path(self) -> VfsPath {
        VfsPath::new(BackendKind::List, format!("/{}", self.0))
    }

    /// The id a `list:` path names, or `None` for anything else.
    ///
    /// Deliberately strict: a `List` segment whose tail is not `/<digits>`
    /// names no listing at all, and answering `None` is what makes
    /// [`crate::panel::Tab::is_virtual`] and the tab's path agree.
    pub fn from_path(path: &VfsPath) -> Option<Self> {
        if path.backend() != BackendKind::List {
            return None;
        }
        let tail = path.tail().to_str()?;
        tail.strip_prefix('/')?.parse::<u64>().ok().map(Self)
    }
}

/// How a streaming listing ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListStatus {
    /// Still being filled.
    Filling,
    /// The producer finished on its own.
    Complete,
    /// `Esc` stopped it. **What was found is kept**.
    Cancelled,
    /// The producer failed. What was found is still kept, and the reason is
    /// shown after the rows that did arrive, exactly as a directory read does
    /// (the design invariant 7).
    Failed(String),
}

impl ListStatus {
    /// True once nothing more will arrive.
    pub fn is_final(&self) -> bool {
        match self {
            Self::Filling => false,
            Self::Complete | Self::Cancelled | Self::Failed(_) => true,
        }
    }
}

/// The rows and how the listing ended, under one lock.
///
/// One lock rather than two, because a reader that took the rows and then the
/// status could see `Complete` after a row it had not read: it would report a
/// finished listing that was short by one hit. Holding both together makes the
/// snapshot consistent by construction.
#[derive(Debug)]
struct State {
    rows: Vec<Entry>,
    status: ListStatus,
}

/// Everything a streaming listing and its sink share.
///
/// A `std::sync::Mutex` and not a `RefCell`: the sink is cloned into
/// `ignore`'s parallel walker, one visitor per thread, so this is genuinely
/// shared across threads and there is no `&mut` route to it. That is the case
/// a mutex is for and the case the house style rejects a `RefCell` for.
#[derive(Debug)]
struct Shared {
    state: std::sync::Mutex<State>,
    /// The row count, readable without taking the lock: the live
    /// count is read once per frame per panel and must never contend with the
    /// walk.
    len: AtomicUsize,
    /// `Esc`. Separate from the status so a producer can check
    /// it without a lock at every entry.
    cancelled: AtomicBool,
    notify: watch::Sender<u64>,
}

impl Shared {
    fn new(rows: Vec<Entry>, status: ListStatus) -> Self {
        let len = rows.len();
        Self {
            state: std::sync::Mutex::new(State { rows, status }),
            len: AtomicUsize::new(len),
            cancelled: AtomicBool::new(false),
            notify: watch::channel(0).0,
        }
    }

    /// The lock, with a poisoned one recovered rather than escalated.
    ///
    /// A panic in a producer thread must not take the panel's rows with it:
    /// what is behind the lock is a `Vec` and an enum, neither of which can be
    /// left half-written, and the rule is that what was found is
    /// reported rather than thrown away.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Poke the readers with the current length.
    fn poke(&self, len: usize) {
        let _ = self.notify.send(len as u64);
    }

    /// Close the listing. The first final status wins.
    fn close(&self, status: ListStatus) {
        let len = {
            let mut state = self.state();
            if state.status.is_final() {
                return;
            }
            state.status = status;
            state.rows.len()
        };
        self.poke(len);
    }
}

/// The producer's end of a streaming listing.
///
/// Cloneable and `Send + Sync`, because `ignore`'s parallel walker hands each
/// thread its own visitor and every one of them pushes into the same listing.
#[derive(Debug, Clone)]
pub struct ListSink {
    shared: Arc<Shared>,
}

impl ListSink {
    /// Append one row.
    ///
    /// `false` once the listing is closed or cancelled, which is what stops a
    /// walk that checks nothing else: there is exactly one cancellation flag
    /// in the search path and it is this one.
    pub fn push(&self, entry: Entry) -> bool {
        if self.shared.cancelled.load(Ordering::SeqCst) {
            return false;
        }
        let len = {
            let mut state = self.shared.state();
            if state.status.is_final() {
                return false;
            }
            state.rows.push(entry);
            state.rows.len()
        };
        self.shared.len.store(len, Ordering::SeqCst);
        if len == 1 || len.is_multiple_of(LIST_NOTIFY_BATCH) {
            self.shared.poke(len);
        }
        true
    }

    /// How many rows have been pushed.
    pub fn len(&self) -> usize {
        self.shared.len.load(Ordering::SeqCst)
    }

    /// True before the first row.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `Esc`. Idempotent.
    ///
    /// The rows already pushed stay: "`Esc` stops the walk and keeps what was
    /// found". A listing that had already finished keeps the status it
    /// finished with, because there was nothing left to stop.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.close(ListStatus::Cancelled);
    }

    /// Whether [`ListSink::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst)
    }

    /// Close the listing.
    ///
    /// The first call wins; a `finish` after a `cancel` leaves the status
    /// [`ListStatus::Cancelled`], because the user's answer outranks the
    /// producer's report of how far it got.
    pub fn finish(&self, status: ListStatus) {
        self.shared.close(status);
    }
}

/// A listing built from rows someone else produced, or filled as they are
/// found.
#[derive(Debug, Clone)]
pub struct ListFs {
    /// A human-readable description of where the listing came from, shown in
    /// the panel header: `[search: *.rs "TODO" in ~/dev]`.
    label: String,
    shared: Arc<Shared>,
    /// Derived once at construction - from the rows' real homes for a fixed
    /// listing, from the search roots for a streaming one - rather than
    /// recomputed per frame.
    caps: Capabilities,
}

impl ListFs {
    /// Build a listing from rows that already carry their [`Entry::location`].
    ///
    /// An entry with no `location` is still listed, but it contributes nothing
    /// to the delegated capabilities - there is no backend to ask.
    pub fn new(label: impl Into<String>, entries: Vec<Entry>) -> Self {
        let caps = delegated_capabilities(&entries);
        Self {
            label: label.into(),
            shared: Arc::new(Shared::new(entries, ListStatus::Complete)),
            caps,
        }
    }

    /// An empty listing that will be filled as results are found.
    ///
    ///
    /// `roots` decides the capabilities up front, by the same delegation
    /// [`ListFs::new`] performs over its rows: a listing over local roots is
    /// writable and near, one over an archive root is not. It cannot be
    /// derived from the rows here, because the rows have not arrived yet and
    /// the panel needs an answer on the first frame - and under-promising is
    /// the safe direction. `has_directories` and `atomic_rename`
    /// stay false whatever the roots say, for the reasons
    /// [`delegated_capabilities`] gives.
    pub fn streaming(label: impl Into<String>, roots: &[VfsPath]) -> (Arc<Self>, ListSink) {
        let shared = Arc::new(Shared::new(Vec::new(), ListStatus::Filling));
        let listing = Arc::new(Self {
            label: label.into(),
            shared: Arc::clone(&shared),
            caps: capabilities_of_roots(roots),
        });
        (listing, ListSink { shared })
    }

    /// Build a listing straight from a set of addresses.
    ///
    /// Each path is `stat`ed through its own backend so the size, date and
    /// attribute columns are populated; a path that cannot be `stat`ed is still
    /// listed, with its metadata unknown, because the design explicitly
    /// allows `stat` to lag the name column and a hit that vanished between
    /// being found and being shown must not disappear silently.
    ///
    /// Every entry gets `location` set to the path it came from, which is the
    /// whole point: operations reach through the virtual listing to the real
    /// file.
    ///
    /// This `stat`s synchronously. Call it from the task that produced the
    /// paths, not from the render path. A search does not use it: the walker
    /// already holds each hit's metadata and builds its own row from it, so
    /// this is for the caller that has only addresses.
    pub fn from_paths(label: impl Into<String>, paths: Vec<VfsPath>) -> Self {
        let entries = paths.into_iter().map(entry_for).collect();
        Self::new(label, entries)
    }

    /// The header label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The rows found so far, copied out.
    ///
    /// A snapshot rather than a borrow, because a streaming listing's rows are
    /// behind the lock the walker is pushing into. Callers that want to render
    /// go through [`Vfs::read_dir`], which streams.
    pub fn entries(&self) -> Vec<Entry> {
        self.shared.state().rows.clone()
    }

    /// How it ended, or [`ListStatus::Filling`].
    pub fn status(&self) -> ListStatus {
        self.shared.state().status.clone()
    }

    /// How many rows there are now. The live count the design asks for, and
    /// an atomic read, so a panel may ask on every frame.
    pub fn len(&self) -> usize {
        self.shared.len.load(Ordering::SeqCst)
    }

    /// True before the first row arrives.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A snapshot from `cursor` onwards, with the new cursor and the status.
    ///
    /// This is what [`Vfs::read_dir`] streams from; the shape is
    /// `archive::Index::children_from`'s and for the same reason - a reader
    /// that is behind must be able to catch up without re-sending what it
    /// already has.
    pub fn snapshot_from(&self, cursor: usize) -> (usize, Vec<Entry>, ListStatus) {
        let state = self.shared.state();
        let from = cursor.min(state.rows.len());
        let batch = state.rows.get(from..).unwrap_or(&[]).to_vec();
        (state.rows.len(), batch, state.status.clone())
    }

    /// Poked every [`LIST_NOTIFY_BATCH`] rows, on the first row, and once at
    /// the end.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.notify.subscribe()
    }

    /// `Esc`. Equivalent to [`ListSink::cancel`], for a caller that only has
    /// the listing.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.close(ListStatus::Cancelled);
    }

    /// Whether the walk filling this listing was stopped.
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst)
    }

    /// The row addressing `path`, by its [`Entry::location`].
    ///
    /// By location and never by name: two rows in one virtual listing can
    /// share a name.
    pub fn row(&self, path: &VfsPath) -> Option<Entry> {
        self.shared
            .state()
            .rows
            .iter()
            .find(|e| e.location.as_ref() == Some(path))
            .cloned()
    }
}

impl Default for ListFs {
    /// An empty listing. Written out rather than derived because
    /// `Capabilities::default()` is the *local* set, and an empty virtual
    /// listing delegates to nothing and must therefore be read-only.
    fn default() -> Self {
        Self::new(String::new(), Vec::new())
    }
}

/// One row for one address.
fn entry_for(path: VfsPath) -> Entry {
    let stated = match path.backend() {
        // Only `Local` can be reached without a live connection; every other
        // backend needs an instance, so its rows arrive pre-`stat`ed via
        // [`ListFs::new`].
        BackendKind::Local => super::LocalFs::new().stat(&path).ok(),
        // A row inside an archive needs the open `ArchiveFs` to be `stat`ed,
        // and a `ListFs` is built from addresses alone. Such a row arrives
        // pre-`stat`ed through [`ListFs::new`].
        // A remote row is the same case: it needs the live connection, and a
        // `ListFs` is built from addresses alone.
        // A row inside a disk image is the same case again: it needs the
        // open `ImageFs`, and the bytes behind it are read-only whatever the
        // row turns out to be.
        BackendKind::List
        | BackendKind::Archive
        | BackendKind::Remote(_)
        | BackendKind::Image
        | BackendKind::Git => None,
    };
    let mut entry = stated.unwrap_or_else(|| {
        let name = path.file_name().unwrap_or_else(|| path.to_string());
        let mut e = Entry::file(name);
        e.mtime = None;
        e
    });
    entry.location = Some(path);
    entry
}

/// Fold the rows' real backends into one capability set.
///
/// `has_directories` and `atomic_rename` stay false whatever the rows say: the
/// listing itself is flat, and a "rename" across rows that live in different
/// directories is not one filesystem operation. Everything else delegates -
/// a listing of local files is writable and near, a listing of remote ones
/// inherits that backend's latency.
fn delegated_capabilities(entries: &[Entry]) -> Capabilities {
    fold_capabilities(entries.iter().filter_map(|e| e.location.as_ref()))
}

/// The same, for a streaming listing, which has its roots and not its rows.
fn capabilities_of_roots(roots: &[VfsPath]) -> Capabilities {
    fold_capabilities(roots.iter())
}

/// The body of both: the strictest answer over every backend involved.
fn fold_capabilities<'a>(paths: impl Iterator<Item = &'a VfsPath>) -> Capabilities {
    let mut caps = Capabilities::READ_ONLY_LIST;
    let mut seen = false;
    let mut writable = true;
    let mut seekable = true;
    let mut random_access = true;
    let mut can_execute = true;
    let mut paged_listing = false;
    let mut latency = LatencyClass::Local;

    for path in paths {
        seen = true;
        let c = path.backend().capabilities();
        writable &= c.writable;
        seekable &= c.seekable;
        random_access &= c.random_access;
        can_execute &= c.can_execute;
        paged_listing |= c.paged_listing;
        if c.latency == LatencyClass::Network {
            latency = LatencyClass::Network;
        }
    }

    if seen {
        caps.writable = writable;
        caps.seekable = seekable;
        caps.random_access = random_access;
        caps.can_execute = can_execute;
        caps.paged_listing = paged_listing;
        caps.latency = latency;
    }
    caps
}

impl Vfs for ListFs {
    fn kind(&self) -> BackendKind {
        BackendKind::List
    }

    /// Stream the rows, and keep streaming while they are still arriving.
    ///
    ///
    /// A fixed listing is already `Complete`, so this sends its rows once and
    /// ends - which is exactly what v0.1 did. A streaming one sends what has
    /// arrived, waits, and sends each batch as it lands, ending when the
    /// status is final and sending one `Err` last for
    /// [`ListStatus::Failed`] so the panel keeps the rows and shows the reason.
    ///
    ///
    /// It sends **no `..` row**: a flat listing is not a tree, and the design
    /// makes `Ctrl+R` and `Esc` the way out rather than navigation.
    fn read_dir(&self, _path: &VfsPath) -> mpsc::Receiver<Result<Entry>> {
        let (tx, rx) = mpsc::channel(READ_DIR_CHANNEL_DEPTH);
        let shared = Arc::clone(&self.shared);
        // Subscribed *before* the first snapshot, so a row pushed between the
        // two marks the receiver changed rather than being waited for.
        let mut watch = shared.notify.subscribe();
        tokio::spawn(async move {
            let mut cursor = 0usize;
            loop {
                let (next, batch, status, more) = batch_from(&shared, cursor);
                cursor = next;
                for entry in batch {
                    if tx.send(Ok(entry)).await.is_err() {
                        return;
                    }
                }
                // Rows still waiting are sent before the status is acted on:
                // a `Complete` listing must hand over everything it holds
                // before it ends the stream.
                if more {
                    continue;
                }
                match status {
                    ListStatus::Filling => {}
                    ListStatus::Complete | ListStatus::Cancelled => return,
                    ListStatus::Failed(message) => {
                        let _ = tx.send(Err(Error::msg(message))).await;
                        return;
                    }
                }
                // The poke is batched; the timeout is what makes a trickle
                // visible. A closed sender ends the wait immediately, which
                // cannot happen while this task holds the `Arc` that owns it.
                let _ = tokio::time::timeout(POLL_INTERVAL, watch.changed()).await;
            }
        });
        rx
    }

    /// Look a row up by its real home first, falling back to its name.
    ///
    /// The name fallback is genuinely ambiguous - a flat listing can hold two
    /// files called `mod.rs` from different directories - so the location match
    /// is tried first and is the one callers should rely on.
    fn stat(&self, path: &VfsPath) -> Result<Entry> {
        let state = self.shared.state();
        if let Some(hit) = state
            .rows
            .iter()
            .find(|e| e.location.as_ref() == Some(path))
        {
            return Ok(hit.clone());
        }
        let wanted = path.file_name();
        state
            .rows
            .iter()
            .find(|e| Some(&e.name) == wanted.as_ref())
            .cloned()
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    /// Reading a row means reading the file it points at, which is that
    /// backend's job, not this one's. The panel resolves
    /// [`Entry::location`] before opening anything (see
    /// `Panel::current_path`), so this is only reached by a caller that skipped
    /// that step.
    fn open_read(&self, _path: &VfsPath) -> Result<Box<dyn std::io::Read + Send>> {
        unsupported("reading from a virtual listing")
    }

    fn open_write(&self, _path: &VfsPath) -> Result<Box<dyn std::io::Write + Send>> {
        unsupported("writing to a virtual listing")
    }

    fn create_dir(&self, _path: &VfsPath) -> Result<()> {
        unsupported("creating a directory in a virtual listing")
    }

    fn remove(&self, _path: &VfsPath) -> Result<()> {
        unsupported("removing from a virtual listing")
    }

    fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
        unsupported("renaming in a virtual listing")
    }

    /// What the **rows' real homes** can do, not what a `list:` path can.
    ///
    /// The distinction is the whole design of this backend, and it is why the
    /// three refusals above do not contradict a `writable: true` here. A `list:`
    /// path addresses a position in a set of results; the file a row names
    /// lives somewhere else, and `Entry::location` is where. Every operation
    /// the panel offers on a row - `F5`, `F6`, `F8` - reaches through to that
    /// location and is serviced by the row's own backend, so what `writable`
    /// answers is "may these rows be written", which is the question those
    /// keys are gating on.
    ///
    /// `create_dir`, `remove` and `rename` are the operations that address the
    /// listing *itself* rather than a row, and a set of results is not a
    /// namespace anything can be created in, removed from or renamed within.
    /// `has_directories: false` says that for `create_dir`; the other two say
    /// it by refusing. Nothing routes them here in practice - the panel
    /// resolves a row to its location first - and refusing is the answer if
    /// anything ever does.
    ///
    /// One `writable` bool cannot express "these rows may be deleted but this
    /// listing may not be renamed in", which is what makes the pairing above
    /// need a paragraph rather than a field.
    fn capabilities(&self) -> Capabilities {
        self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::EntryKind;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

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
                "hcmd-list-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp tree");
            Self { root }
        }

        fn file(&self, rel: &str, contents: &str) -> VfsPath {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&p, contents).expect("write");
            VfsPath::local(p)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    async fn drain(fs_impl: &ListFs) -> Vec<Entry> {
        let mut rx = fs_impl.read_dir(&VfsPath::new(BackendKind::List, "/"));
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item.expect("a listing never errors"));
        }
        out
    }

    // ---------------------------- the streaming listing ------

    #[test]
    fn a_listing_id_round_trips_through_the_path_a_panel_sits_at() {
        let id = ListingId(7);
        assert_eq!(id.to_path(), VfsPath::new(BackendKind::List, "/7"));
        assert_eq!(ListingId::from_path(&id.to_path()), Some(id));
        // Anything that is not `list:/<digits>` names no listing at all, which
        // is what keeps `Tab::is_virtual` and the tab's path in step.
        assert_eq!(ListingId::from_path(&VfsPath::local("/7")), None);
        assert_eq!(
            ListingId::from_path(&VfsPath::new(BackendKind::List, "/nope")),
            None
        );
        assert_eq!(
            ListingId::from_path(&VfsPath::new(BackendKind::List, "/")),
            None
        );
    }

    #[test]
    fn a_streaming_listing_starts_empty_and_grows() {
        let (listing, sink) = ListFs::streaming("[search: * in /root]", &[VfsPath::local("/root")]);
        assert!(listing.is_empty());
        assert!(sink.is_empty());
        assert_eq!(listing.status(), ListStatus::Filling);
        assert!(!listing.status().is_final());
        // The capabilities are the roots', not the rows': the panel needs an
        // answer on the first frame and the rows have not arrived.
        assert!(listing.capabilities().writable);
        assert!(!listing.capabilities().has_directories);

        for i in 0..3u32 {
            assert!(sink.push(Entry::file(format!("f{i}"))));
        }
        assert_eq!(listing.len(), 3);
        assert_eq!(sink.len(), 3);
        sink.finish(ListStatus::Complete);
        assert_eq!(listing.status(), ListStatus::Complete);
        assert!(listing.status().is_final());
        assert!(
            !sink.push(Entry::file("late")),
            "a closed listing is closed"
        );
        assert_eq!(listing.len(), 3);
    }

    #[test]
    fn esc_stops_the_walk_and_keeps_what_was_found() {
        // at the level the flag actually lives: cancelling keeps
        // every row already pushed, refuses the next one - which is what stops
        // a producer that checks nothing else - and outranks the producer's own
        // report of how far it got.
        let (listing, sink) = ListFs::streaming("[branch: /root]", &[VfsPath::local("/root")]);
        assert!(sink.push(Entry::file("kept.rs")));
        listing.cancel();

        assert!(sink.is_cancelled());
        assert!(listing.is_cancelled());
        assert!(!sink.push(Entry::file("dropped.rs")));
        assert_eq!(listing.len(), 1);
        assert_eq!(listing.entries().len(), 1);
        assert_eq!(listing.status(), ListStatus::Cancelled);

        // "The first call wins": a `finish` afterwards does not turn the
        // user's answer into the producer's.
        sink.finish(ListStatus::Complete);
        assert_eq!(listing.status(), ListStatus::Cancelled);
        listing.cancel();
        assert_eq!(listing.status(), ListStatus::Cancelled, "idempotent");
    }

    #[test]
    fn a_snapshot_resumes_from_where_the_reader_got_to() {
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        for i in 0..4u32 {
            assert!(sink.push(Entry::file(format!("f{i}"))));
        }
        let (cursor, batch, status) = listing.snapshot_from(0);
        assert_eq!(cursor, 4);
        assert_eq!(batch.len(), 4);
        assert_eq!(status, ListStatus::Filling);

        assert!(sink.push(Entry::file("f4")));
        let (next, batch, _) = listing.snapshot_from(cursor);
        assert_eq!(next, 5);
        assert_eq!(batch.len(), 1, "only what the reader has not seen");
        assert_eq!(batch.first().map(|e| e.name.as_str()), Some("f4"));
        // A cursor past the end is not a panic and not a resend.
        let (_, batch, _) = listing.snapshot_from(99);
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn a_read_streams_rows_that_arrive_after_it_started() {
        // "results stream back over a channel, with a live
        // count". The panel is reading before the walk has found anything, and
        // the read ends when the listing does - not when it started.
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        let mut rx = listing.read_dir(&VfsPath::new(BackendKind::List, "/1"));

        assert!(sink.push(Entry::file("first")));
        let first = rx.recv().await.expect("a row arrived");
        assert_eq!(first.expect("a listing row").name, "first");

        assert!(sink.push(Entry::file("second")));
        let second = rx.recv().await.expect("and another");
        assert_eq!(second.expect("a listing row").name, "second");

        sink.finish(ListStatus::Complete);
        assert!(rx.recv().await.is_none(), "the read ends with the listing");
    }

    #[tokio::test]
    async fn a_cancelled_read_ends_with_the_rows_it_had_and_no_error() {
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        assert!(sink.push(Entry::file("kept")));
        let mut rx = listing.read_dir(&VfsPath::new(BackendKind::List, "/1"));
        assert_eq!(rx.recv().await.expect("a row").expect("ok").name, "kept");
        listing.cancel();
        // Cancelling is the user's answer, not a failure: the panel keeps the
        // rows and is told nothing went wrong.
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_failed_listing_delivers_its_rows_and_then_the_reason() {
        // an unreadable thing is reported *after* what did
        // arrive, never rendered as an empty listing.
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        assert!(sink.push(Entry::file("found")));
        sink.finish(ListStatus::Failed("the root vanished".to_string()));

        let mut rx = listing.read_dir(&VfsPath::new(BackendKind::List, "/1"));
        assert_eq!(rx.recv().await.expect("a row").expect("ok").name, "found");
        let err = rx.recv().await.expect("the reason").expect_err("a failure");
        assert!(err.to_string().contains("the root vanished"), "{err}");
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn a_row_is_looked_up_by_its_real_home_and_not_by_its_name() {
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        for dir in ["one", "two"] {
            let mut row = Entry::file("mod.rs");
            row.location = Some(VfsPath::local(format!("/root/{dir}/mod.rs")));
            row.size = if dir == "one" { 3 } else { 6 };
            assert!(sink.push(row));
        }
        let a = VfsPath::local("/root/one/mod.rs");
        let b = VfsPath::local("/root/two/mod.rs");
        assert_eq!(listing.row(&a).map(|e| e.size), Some(3));
        assert_eq!(listing.row(&b).map(|e| e.size), Some(6));
        assert_eq!(listing.row(&VfsPath::local("/root/three/mod.rs")), None);
        // And the two rows key differently for marking, which is the whole
        // reason `Entry::mark_key` exists.
        assert_ne!(
            listing.row(&a).map(|e| e.mark_key().into_owned()),
            listing.row(&b).map(|e| e.mark_key().into_owned())
        );
    }

    #[tokio::test]
    async fn a_listing_built_from_paths_carries_real_locations() {
        let t = TempTree::new("paths");
        let a = t.file("one/deep/mod.rs", "aaa");
        let b = t.file("two/mod.rs", "bbbbbb");

        let fs_impl = ListFs::from_paths("search: mod.rs", vec![a.clone(), b.clone()]);
        assert_eq!(fs_impl.label(), "search: mod.rs");
        assert_eq!(fs_impl.entries().len(), 2);

        let rows = drain(&fs_impl).await;
        assert_eq!(rows.len(), 2, "order is preserved");
        assert_eq!(rows[0].name, "mod.rs");
        assert_eq!(rows[1].name, "mod.rs");
        // Same name, different homes - which is exactly why `location` exists.
        assert_eq!(rows[0].location.as_ref(), Some(&a));
        assert_eq!(rows[1].location.as_ref(), Some(&b));
        assert_eq!(rows[0].size, 3);
        assert_eq!(rows[1].size, 6);
        assert_eq!(rows[0].kind, EntryKind::File);
        assert!(rows[0].mtime.is_some(), "stat populated the date column");
        assert_ne!(rows[0].mode, 0);
        assert!(!rows[0].is_parent, "a virtual listing has no `..` row");
    }

    #[tokio::test]
    async fn a_path_that_cannot_be_stated_is_still_listed() {
        let t = TempTree::new("missing");
        let real = t.file("present.txt", "x");
        let gone = VfsPath::local(t.root.join("vanished.txt"));

        let fs_impl = ListFs::from_paths("search", vec![real, gone.clone()]);
        let rows = drain(&fs_impl).await;
        assert_eq!(rows.len(), 2, "a hit that vanished is not silently dropped");
        let ghost = &rows[1];
        assert_eq!(ghost.name, "vanished.txt");
        assert_eq!(ghost.size, 0, "unknown");
        assert_eq!(ghost.mtime, None, "unknown");
        assert_eq!(ghost.location.as_ref(), Some(&gone));
    }

    #[tokio::test]
    async fn dropping_the_receiver_stops_the_listing() {
        let t = TempTree::new("cancel");
        let paths: Vec<VfsPath> = (0..READ_DIR_CHANNEL_DEPTH * 4)
            .map(|i| t.file(&format!("f{i:04}"), ""))
            .collect();
        let fs_impl = ListFs::from_paths("search", paths);

        let mut rx = fs_impl.read_dir(&VfsPath::new(BackendKind::List, "/"));
        assert!(rx.recv().await.is_some());
        drop(rx);
        // The producer's next `send` fails and it returns; nothing panics and
        // the listing can be read again from the start.
        tokio::task::yield_now().await;
        assert_eq!(drain(&fs_impl).await.len(), READ_DIR_CHANNEL_DEPTH * 4);
    }

    #[test]
    fn stat_prefers_the_real_location_over_the_name() {
        let t = TempTree::new("stat");
        let a = t.file("one/mod.rs", "aaa");
        let b = t.file("two/mod.rs", "bbbbbb");
        let fs_impl = ListFs::from_paths("search", vec![a.clone(), b.clone()]);

        assert_eq!(fs_impl.stat(&a).expect("a").size, 3);
        assert_eq!(fs_impl.stat(&b).expect("b").size, 6);
        // The name fallback still resolves something, ambiguously.
        let by_name = fs_impl
            .stat(&VfsPath::new(BackendKind::List, "/mod.rs"))
            .expect("name fallback");
        assert_eq!(by_name.name, "mod.rs");
        assert!(
            fs_impl
                .stat(&VfsPath::new(BackendKind::List, "/nope"))
                .is_err()
        );
    }

    #[test]
    fn capabilities_delegate_to_the_rows_real_backends() {
        let t = TempTree::new("caps");
        let a = t.file("a.txt", "x");
        let fs_impl = ListFs::from_paths("search", vec![a]);
        let c = fs_impl.capabilities();

        assert_eq!(fs_impl.kind(), BackendKind::List);
        assert!(!c.has_directories, "a flat listing is never a tree");
        assert!(!c.atomic_rename, "rows come from different directories");
        // Delegated from LocalFs.
        assert!(c.writable);
        assert!(c.seekable);
        assert!(c.random_access);
        assert!(c.can_execute);
        assert_eq!(c.latency, LatencyClass::Local);
    }

    #[test]
    fn an_empty_or_homeless_listing_is_read_only() {
        let empty = ListFs::new("empty", Vec::new());
        assert!(!empty.capabilities().writable);
        assert!(!empty.capabilities().has_directories);

        // Rows with no `location` have no backend to delegate to.
        let homeless = ListFs::new("homeless", vec![Entry::file("a"), Entry::dir("b")]);
        assert!(!homeless.capabilities().writable);
        assert_eq!(homeless.entries().len(), 2);
    }

    #[test]
    fn write_operations_are_refused_rather_than_faked() {
        let fs_impl = ListFs::new("l", vec![Entry::file("a")]);
        let p = VfsPath::new(BackendKind::List, "/a");
        assert!(matches!(fs_impl.open_read(&p), Err(Error::Unsupported(_))));
        assert!(matches!(fs_impl.open_write(&p), Err(Error::Unsupported(_))));
        assert!(matches!(fs_impl.create_dir(&p), Err(Error::Unsupported(_))));
        assert!(matches!(fs_impl.remove(&p), Err(Error::Unsupported(_))));
        assert!(matches!(fs_impl.rename(&p, &p), Err(Error::Unsupported(_))));
    }

    #[test]
    fn a_reader_copies_one_batch_at_a_time_and_not_the_whole_tail() {
        // The walk has no backpressure - `ListSink::push` never blocks - and it
        // outruns the panel by two orders of magnitude, so by the time the
        // reader's channel is full the unread tail is the whole result set.
        // Copying that tail in one `to_vec` was a second deep clone of every
        // row still in flight, held for as long as it took to hand the first of
        // them over: on a 275k-row walk, tens of seconds and tens of megabytes
        // for a listing the design puts no bound on.
        let rows = SNAPSHOT_BATCH * 3 + 7;
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        for i in 0..rows {
            assert!(sink.push(Entry::file(format!("f{i:06}"))));
        }
        sink.finish(ListStatus::Complete);

        let (next, batch, status, more) = batch_from(&listing.shared, 0);
        assert_eq!(batch.len(), SNAPSHOT_BATCH, "bounded by the batch");
        assert_eq!(next, SNAPSHOT_BATCH);
        assert_eq!(status, ListStatus::Complete);
        assert!(more, "and it says the rest is waiting");

        // Walked to the end, one bounded copy per pass, and the last one says
        // there is nothing behind it.
        let mut cursor = 0usize;
        let mut seen = 0usize;
        loop {
            let (next, batch, _, more) = batch_from(&listing.shared, cursor);
            assert!(batch.len() <= SNAPSHOT_BATCH);
            cursor = next;
            seen = seen.saturating_add(batch.len());
            if !more {
                break;
            }
        }
        assert_eq!(seen, rows);
        assert_eq!(
            batch_from(&listing.shared, cursor).1.len(),
            0,
            "and a spent cursor copies nothing"
        );
    }

    #[tokio::test]
    async fn a_finished_listing_hands_over_every_row_before_it_ends_the_stream() {
        // The other half of the bound: rows still waiting are sent *before* the
        // status is acted on, so a `Complete` listing bigger than one batch
        // does not lose its tail to the `return` that ends the read.
        let rows = SNAPSHOT_BATCH * 2 + 5;
        let (listing, sink) = ListFs::streaming("l", &[VfsPath::local("/root")]);
        for i in 0..rows {
            assert!(sink.push(Entry::file(format!("f{i:06}"))));
        }
        sink.finish(ListStatus::Complete);

        let drained = drain(&listing).await;
        assert_eq!(drained.len(), rows);
        for (i, entry) in drained.iter().enumerate() {
            assert_eq!(entry.name, format!("f{i:06}"), "and in order");
        }
    }
}
