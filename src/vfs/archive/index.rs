//! The archive index: what listing an archive costs, and what is kept.
//!
//!
//! > Reading is streaming where the format allows and lazily indexed where it
//! > does not. A 4 GB `.tar.gz` must not be decompressed in full to list it;
//! > build the index in the background and populate the panel as entries
//! > appear.
//!
//! This is the same shape as v0.1's streaming `read_dir` and v0.4's viewer
//! index, and for the same reason: the first frame must not wait for the last
//! byte.
//!
//! # What is cached, and how big it gets
//!
//! **Metadata only, never member contents.** One [`Member`] per entry: its
//! normalised path, its kind, size, mode, owner, mtime, and a [`Locator`] -
//! the few bytes that let the format find the member again. Nothing that was
//! decompressed to produce it is kept. Decompressing a 40 GB `.tar.gz` to list
//! it costs 40 GB of *throughput* and a few tens of megabytes of *memory*.
//!
//! The bound is [`MAX_MEMBERS`] entries and [`MAX_NAME_BYTES`] of member names,
//! whichever comes first; past either the index stops, [`IndexStatus`] becomes
//! [`IndexStatus::Truncated`], and every listing of that archive ends with an
//! error row saying so. A truncated index is never silently presented as a
//! complete one - the same rule the design sets for an unreadable directory.
//!
//! # A format with no central directory
//!
//! A compressed tar has none, so **listing it is reading it**: the index is
//! built by decompressing the whole stream once. The panel fills as entries
//! arrive, `..` is there from the first frame, and leaving is never blocked.
//! `stat` on a member that has not been reached yet blocks until it is reached
//! or the index completes - see [`Index::stat_blocking`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::vfs::{Entry, EntryKind};

use super::safety::{Unsafe, normalize_member};

/// The most entries one archive index will hold.
///
/// A million members is far beyond any archive a person browses and still only
/// a few hundred megabytes of index in the worst case. Past it the archive is
/// listed as far as it got and said to be truncated.
pub const MAX_MEMBERS: usize = 1_000_000;

/// The most member-name bytes one archive index will hold: 64 MiB.
///
/// The other half of the bound, because a million four-kilobyte names is not
/// the same amount of memory as a million eight-byte ones. A zip bomb built
/// out of names rather than data hits this one.
pub const MAX_NAME_BYTES: usize = 64 * 1024 * 1024;

/// How many members are added between wakeups of the listeners.
///
/// Small enough that a panel fills visibly, large enough that indexing a
/// 200 000-entry tar is not 200 000 channel wakeups.
pub const INDEX_NOTIFY_BATCH: usize = 128;

/// How the format finds a member again once the index has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locator {
    /// The member's position in the container's own order: a zip central
    /// directory index, a 7z file index, a rar header ordinal.
    Ordinal(usize),
    /// The member's data offset within the (decompressed) stream, and its
    /// length. This is what a tar has instead of a directory: for a plain
    /// `.tar` it is a file offset and reading is a seek; for a compressed one
    /// it is an offset into the decompressed stream and reading is a skip.
    Offset {
        /// Where the member's bytes start.
        data: u64,
        /// How many of them there are.
        len: u64,
    },
    /// Nothing to read: a directory, or a parent this backend synthesised.
    None,
}

/// What a member is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberKind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symbolic link, carrying the target exactly as the archive stored it.
    /// It is validated at extraction time by
    /// [`super::safety::safe_link_target`], never followed here.
    Symlink(String),
    /// A hard link to another member, carrying the target it names.
    Hardlink(String),
    /// A device node, fifo, socket or anything else a tar can carry.
    Other,
}

/// One entry in an archive, as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The normalised path: `/`-separated, no leading or trailing separator,
    /// no `.` or `..` components ([`normalize_member`]).
    pub path: String,
    /// What it is.
    pub kind: MemberKind,
    /// Uncompressed size in bytes, as the container claims. **A claim, not a
    /// measurement**: a corrupt or hostile header can say anything, so nothing
    /// allocates from this number.
    pub size: u64,
    /// Modification time, when the format records one.
    pub mtime: Option<SystemTime>,
    /// POSIX mode bits, `0` when the format has no concept of them.
    pub mode: u32,
    /// Owning uid, `0` when unknown.
    pub uid: u32,
    /// Owning gid, `0` when unknown.
    pub gid: u32,
    /// How to read it.
    pub locator: Locator,
    /// True for a directory this backend invented because members below it
    /// existed and it did not. It has no mode, no mtime and no owner, because
    /// the archive never stored any.
    pub synthetic: bool,
}

impl Member {
    /// The name the panel shows: the last component of [`Member::path`].
    pub fn name(&self) -> &str {
        match self.path.rfind('/') {
            Some(cut) => self.path.get(cut.saturating_add(1)..).unwrap_or(&self.path),
            None => &self.path,
        }
    }

    /// The parent's normalised path; `""` for a member at the archive root.
    pub fn parent(&self) -> &str {
        match self.path.rfind('/') {
            Some(cut) => self.path.get(..cut).unwrap_or(""),
            None => "",
        }
    }

    /// The panel row for this member.
    ///
    /// A symlink reports `to_dir` from the index rather than by following the
    /// link: there is no filesystem here to follow it on, and a link that
    /// points outside the archive has no meaning until something is extracted.
    pub fn to_entry(&self, dir_is_known: impl Fn(&str) -> bool) -> Entry {
        let name = self.name().to_string();
        let is_hidden = name.starts_with('.');
        let kind = match &self.kind {
            MemberKind::Dir => EntryKind::Dir,
            MemberKind::File | MemberKind::Hardlink(_) => EntryKind::File,
            MemberKind::Symlink(target) => EntryKind::Symlink {
                to_dir: dir_is_known(target),
            },
            MemberKind::Other => EntryKind::Other,
        };
        Entry {
            name,
            kind,
            size: if matches!(self.kind, MemberKind::Dir) {
                0
            } else {
                self.size
            },
            mtime: self.mtime,
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            is_hidden,
            is_parent: false,
            location: None,
            hit: None,
            git_state: None,
        }
    }
}

/// A member as a format reads it, before this module normalises the name and
/// decides whether it is allowed to exist at all.
#[derive(Debug, Clone)]
pub struct RawMember {
    /// The name exactly as the container stores it, lossily decoded.
    pub name: String,
    /// What it is.
    pub kind: MemberKind,
    /// Claimed uncompressed size.
    pub size: u64,
    /// Modification time, when there is one.
    pub mtime: Option<SystemTime>,
    /// Mode bits, `0` when the format has none.
    pub mode: u32,
    /// uid, `0` when unknown.
    pub uid: u32,
    /// gid, `0` when unknown.
    pub gid: u32,
    /// How to read it again.
    pub locator: Locator,
}

impl RawMember {
    /// A file with everything unknown zeroed.
    pub fn file(name: impl Into<String>, size: u64, locator: Locator) -> Self {
        Self {
            name: name.into(),
            kind: MemberKind::File,
            size,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            locator,
        }
    }

    /// A directory with everything unknown zeroed.
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            kind: MemberKind::Dir,
            ..Self::file(name, 0, Locator::None)
        }
    }
}

/// Where a format sends the members it reads.
///
/// Deliberately the *only* thing a format's `index` method can do: it cannot
/// see the index, cannot decide what is safe, and cannot allocate the storage.
/// Name validation, parent synthesis, duplicate handling and
/// the memory bounds all live on this side, once, for all eight formats.
pub trait IndexSink {
    /// Add one member. Returns `false` when indexing should stop - the archive
    /// was closed, or a bound in this module was reached. A format must honour
    /// that promptly and return `Ok(())`.
    fn push(&mut self, raw: RawMember) -> bool;

    /// Has the archive been closed under us?
    fn cancelled(&self) -> bool;
}

/// How an index build ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum IndexStatus {
    /// Still reading the container.
    #[default]
    Building,
    /// Every member is known.
    Complete,
    /// A bound in this module was reached; the listing is a prefix of the
    /// truth and says so.
    Truncated(&'static str),
    /// The container could not be read to the end. Whatever was read is still
    /// listed - an archive truncated by a failed download is exactly the case
    /// where seeing the first half matters.
    Failed(String),
}

impl IndexStatus {
    /// Is the build finished, however it finished?
    pub fn is_final(&self) -> bool {
        !matches!(self, Self::Building)
    }
}

/// How many hard links one lookup will follow before it gives up.
///
/// A container may name a hard link whose target is another hard link; it may
/// also name one that points at itself. Eight is past anything a real archive
/// contains and is a number rather than a visited-set because the cost of
/// being wrong here is a member that reports no contents, not a hang.
const MAX_HARDLINK_HOPS: usize = 8;

/// The mutable half of an index.
#[derive(Debug, Default)]
struct Data {
    members: Vec<Member>,
    by_path: HashMap<String, usize>,
    children: HashMap<String, Vec<usize>>,
    name_bytes: usize,
    /// How many entries were refused by name, and why the
    /// first one was. The count is what the panel reports; the reason is what
    /// makes the report actionable.
    refused: usize,
    first_refusal: Option<String>,
    status: IndexStatus,
    /// Which build this content belongs to. See [`Index::invalidate`].
    epoch: u64,
}

impl Data {
    fn new() -> Self {
        Self {
            status: IndexStatus::Building,
            ..Self::default()
        }
    }

    /// The member at `path`, with a hard link resolved to what it links to.
    ///
    /// A tar hard-link entry carries a header and **no data**: its declared
    /// size is zero and there are no bytes at its position, because the bytes
    /// are the target's. Handing that member out as it stands is what made
    /// `Alt+F6` write a zero-byte file where the archive held a link to real
    /// content, and report the job as complete (the design - a copy either
    /// completes or says why not).
    ///
    /// So the link is resolved **here**, in the backend, and everything above
    /// gets a member that reports the target's size and reads the target's
    /// bytes. `ops` stays what the design wants it to be: a copy engine that
    /// has never heard of a tar. The extracted file is a copy rather than a
    /// second name for one inode - `Vfs` has no `link` - but its contents are
    /// the contents the archive holds, which is the part a user loses when
    /// this is not done.
    fn resolve<'a>(&'a self, member: &'a Member) -> std::borrow::Cow<'a, Member> {
        if !matches!(member.kind, MemberKind::Hardlink(_)) {
            // The overwhelmingly common case, and the one a listing runs once
            // per row: nothing is copied.
            return std::borrow::Cow::Borrowed(member);
        }
        let mut at = member;
        for _ in 0..MAX_HARDLINK_HOPS {
            let MemberKind::Hardlink(target) = &at.kind else {
                break;
            };
            // A tar hard-link target names a member from the archive's root,
            // not from the link's own directory.
            let Ok(key) = normalize_member(target, false) else {
                break;
            };
            let Some(next) = self.by_path.get(&key).and_then(|i| self.members.get(*i)) else {
                break;
            };
            if std::ptr::eq(next, at) {
                break;
            }
            at = next;
            if !matches!(at.kind, MemberKind::Hardlink(_)) {
                return std::borrow::Cow::Owned(Member {
                    path: member.path.clone(),
                    kind: MemberKind::File,
                    size: at.size,
                    mtime: member.mtime,
                    mode: member.mode,
                    uid: member.uid,
                    gid: member.gid,
                    locator: at.locator,
                    synthetic: false,
                });
            }
        }
        std::borrow::Cow::Borrowed(member)
    }
}

/// The index of one archive: shared by every panel, every listing and every
/// read of that archive, built once.
#[derive(Debug)]
pub struct Index {
    data: Mutex<Data>,
    /// Woken whenever members are added or the status becomes final. Serves
    /// [`Index::stat_blocking`], which is synchronous by [`crate::vfs::Vfs`]'s
    /// signature.
    ready: Condvar,
    /// The async half of the same signal, for [`crate::vfs::Vfs::read_dir`].
    /// A `watch` rather than a `Notify` because its version counter makes a
    /// lost wakeup impossible: a listener that reads and then waits cannot
    /// miss an update that landed in between.
    generation: tokio::sync::watch::Sender<u64>,
    cancel: AtomicBool,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// An empty index, waiting for a build.
    pub fn new() -> Self {
        Self {
            data: Mutex::new(Data::new()),
            ready: Condvar::new(),
            generation: tokio::sync::watch::Sender::new(0),
            cancel: AtomicBool::new(false),
        }
    }

    /// Stop the build. Set when the archive is closed; the sink stops at its
    /// next member and the format returns.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.wake();
    }

    /// Has the build been cancelled?
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn wake(&self) {
        // `send_modify` bumps the version even when the value repeats, which is
        // what the listeners actually key on.
        self.generation.send_modify(|g| *g = g.wrapping_add(1));
        self.ready.notify_all();
    }

    /// Lock the data, recovering from a poisoned mutex.
    ///
    /// A panic while holding this lock would leave the index half-updated, not
    /// unsound: every field is a plain container and the invariants are
    /// re-established by the next insert. Refusing to serve a listing because
    /// some other thread panicked is the worse failure, so the guard is taken
    /// either way.
    fn lock(&self) -> std::sync::MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How the build ended, or [`IndexStatus::Building`].
    pub fn status(&self) -> IndexStatus {
        self.lock().status.clone()
    }

    /// How many members are known so far.
    pub fn len(&self) -> usize {
        self.lock().members.len()
    }

    /// True while no member is known.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Which build the index is currently accepting content from.
    ///
    /// A number, not a handle, because it crosses a thread boundary: the
    /// thread that starts a build is handed the epoch and everything that
    /// thread writes carries it.
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Record the end of a build.
    ///
    /// For an index nothing invalidates. Where a rebuild is possible, use
    /// [`Index::finish_epoch`] with the epoch the build was started for.
    pub fn finish(&self, status: IndexStatus) {
        let epoch = self.epoch();
        self.finish_epoch(epoch, status);
    }

    /// Record the end of the build that was started for `epoch`.
    ///
    /// A build that has been superseded says nothing: its answer is about a
    /// container that no longer exists, and stamping it on the build that
    /// replaced it would either end a listing that is still filling or, when
    /// it ends it as failed, leave the archive permanently unreadable to
    /// everything that waits for a final status.
    pub fn finish_epoch(&self, epoch: u64, status: IndexStatus) {
        {
            let mut data = self.lock();
            if data.epoch != epoch {
                return;
            }
            if !data.status.is_final() {
                data.status = status;
            }
        }
        self.wake();
    }

    /// A snapshot of the children of `dir` from `cursor` onwards.
    ///
    /// Returns the new cursor and the status. The cursor is an index into the
    /// directory's own child list, so a listener resumes exactly where it
    /// stopped without rescanning what it has already sent.
    pub fn children_from(&self, dir: &str, cursor: usize) -> (usize, Vec<Entry>, IndexStatus) {
        let data = self.lock();
        let known = |path: &str| {
            data.by_path
                .get(path)
                .and_then(|i| data.members.get(*i))
                .is_some_and(|m| matches!(m.kind, MemberKind::Dir))
        };
        let Some(indices) = data.children.get(dir) else {
            return (cursor, Vec::new(), data.status.clone());
        };
        let fresh = indices.get(cursor..).unwrap_or(&[]);
        let mut out = Vec::with_capacity(fresh.len());
        for index in fresh {
            if let Some(member) = data.members.get(*index).map(|m| data.resolve(m)) {
                let member = member.as_ref();
                // A symlink's `to_dir` is answered from the index, resolving
                // the target relative to the link's own directory.
                let entry = member.to_entry(|target| {
                    let base = member.parent();
                    let joined = if base.is_empty() {
                        target.to_string()
                    } else {
                        format!("{base}/{target}")
                    };
                    normalize_member(&joined, false).is_ok_and(|p| known(&p))
                });
                out.push(entry);
            }
        }
        (cursor.saturating_add(fresh.len()), out, data.status.clone())
    }

    /// Does this directory exist in the index? The archive root always does.
    pub fn is_dir(&self, path: &str) -> bool {
        if path.is_empty() {
            return true;
        }
        let data = self.lock();
        data.by_path
            .get(path)
            .and_then(|i| data.members.get(*i))
            .is_some_and(|m| matches!(m.kind, MemberKind::Dir))
    }

    /// The member at `path`, if it is known *now*.
    pub fn get(&self, path: &str) -> Option<Member> {
        let data = self.lock();
        let member = data.by_path.get(path).and_then(|i| data.members.get(*i))?;
        Some(data.resolve(member).into_owned())
    }

    /// The member at `path`, waiting for the build if it has not got there yet.
    ///
    /// **Blocking**, because [`crate::vfs::Vfs::stat`] is. On a compressed tar
    /// this can take as long as decompressing the rest of the archive, so it
    /// belongs on a job thread and never on the render path - which already
    /// holds the [`Entry`] it needs from the listing and has no reason to call
    /// this. Returns as soon as the member appears, not when the build ends.
    pub fn stat_blocking(&self, path: &str) -> Result<Member> {
        let mut data = self.lock();
        loop {
            if let Some(member) = data.by_path.get(path).and_then(|i| data.members.get(*i)) {
                return Ok(data.resolve(member).into_owned());
            }
            if self.cancelled() {
                return Err(Error::Cancelled);
            }
            match &data.status {
                IndexStatus::Building => {}
                IndexStatus::Complete | IndexStatus::Truncated(_) => {
                    return Err(Error::NotFound(path.to_string()));
                }
                IndexStatus::Failed(why) => {
                    return Err(Error::msg(format!("{path}: {why}")));
                }
            }
            data = self
                .ready
                .wait(data)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Block until the build is finished, and report how it finished.
    ///
    /// For a caller that genuinely needs the whole listing - `Alt+F6`, which
    /// reports a total before it starts, and the tests. The panel never calls
    /// this: it renders what has arrived.
    pub fn wait_until_final(&self) -> IndexStatus {
        let mut data = self.lock();
        loop {
            if data.status.is_final() {
                return data.status.clone();
            }
            if self.cancelled() {
                return IndexStatus::Failed("the archive was closed".to_string());
            }
            data = self
                .ready
                .wait(data)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Throw the index away because the container has changed under it.
    ///
    /// Every [`Locator`] addresses a position in a specific container, and a
    /// write - in particular a compressed-tar rewrite, which renames a whole
    /// new file over the old one - moves all of them. The
    /// index is emptied and marked as still building so that the next listing
    /// waits for a fresh build rather than reading through stale offsets.
    ///
    /// The caller is responsible for starting that build, and for handing it
    /// the epoch returned here: everything the *old* build still has to say -
    /// the members it is part-way through reading, and the final status it
    /// ends with - is about the container that has just been replaced, and is
    /// dropped on the floor from this moment on. [`super::ArchiveFs`] starts
    /// the new build after a successful write.
    pub fn invalidate(&self) -> u64 {
        let epoch = {
            let mut data = self.lock();
            let next = data.epoch.wrapping_add(1);
            *data = Data::new();
            data.epoch = next;
            next
        };
        self.wake();
        epoch
    }

    /// Subscribe to "something changed". See [`Index::generation`]'s note on
    /// why this cannot lose a wakeup.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// How many entries were refused for an unsafe name, and the first reason.
    pub fn refusals(&self) -> (usize, Option<String>) {
        let data = self.lock();
        (data.refused, data.first_refusal.clone())
    }
}

/// The [`IndexSink`] a format writes into.
///
/// Owns the whole "what does a name mean" policy: normalisation, the refusals,
/// parent synthesis, duplicates, and the memory bounds.
pub struct Builder {
    index: Arc<Index>,
    /// True only for formats that really use `\` as a path separator (RAR).
    backslash_separators: bool,
    limits: Limits,
    since_wake: usize,
    /// The build this sink is filling. Content it produces after the index
    /// has moved on is dropped rather than mixed into the build that
    /// replaced it.
    epoch: u64,
}

/// What bounds one index. Fields rather than constants only so a test can
/// reach the truncation path without building a million-entry archive; every
/// caller outside this module gets [`MAX_MEMBERS`] and [`MAX_NAME_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most members one index will hold.
    pub members: usize,
    /// The most member-name bytes one index will hold.
    pub name_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            members: MAX_MEMBERS,
            name_bytes: MAX_NAME_BYTES,
        }
    }
}

impl Builder {
    /// A sink writing into `index`, for the build it is accepting now.
    pub fn new(index: Arc<Index>, backslash_separators: bool) -> Self {
        Self::with_limits(index, backslash_separators, Limits::default())
    }

    /// [`Builder::new`] for a build that was started for a known epoch.
    ///
    /// The epoch is taken at the point the build is *started*, not here: a
    /// sink that read it on the worker thread would adopt whatever generation
    /// the index had reached by the time that thread was scheduled, which on a
    /// busy machine can be a later one than the build it belongs to.
    pub fn for_epoch(index: Arc<Index>, backslash_separators: bool, epoch: u64) -> Self {
        Self {
            index,
            backslash_separators,
            limits: Limits::default(),
            since_wake: 0,
            epoch,
        }
    }

    /// [`Builder::new`] with the memory bounds given explicitly.
    pub fn with_limits(index: Arc<Index>, backslash_separators: bool, limits: Limits) -> Self {
        let epoch = index.epoch();
        Self {
            index,
            backslash_separators,
            limits,
            since_wake: 0,
            epoch,
        }
    }

    /// Has the index moved on to a build that is not this one?
    fn superseded(&self) -> bool {
        self.index.epoch() != self.epoch
    }

    /// Wake listeners for whatever has been added since the last wake.
    pub fn flush(&mut self) {
        if self.since_wake > 0 {
            self.since_wake = 0;
            self.index.wake();
        }
    }

    /// Insert `member`, synthesising any parent directories it needs.
    ///
    /// Returns false when a bound was reached; the status is set here so the
    /// caller only has to stop.
    fn insert(data: &mut Data, member: Member, limits: Limits) -> bool {
        if data.members.len() >= limits.members {
            data.status = IndexStatus::Truncated("the archive has more entries than hcmd indexes");
            return false;
        }
        if data.name_bytes >= limits.name_bytes {
            data.status =
                IndexStatus::Truncated("the archive's entry names exceed hcmd's index budget");
            return false;
        }

        let parent = member.parent().to_string();
        if !parent.is_empty() && !data.by_path.contains_key(&parent) {
            let synthetic = Member {
                path: parent.clone(),
                kind: MemberKind::Dir,
                size: 0,
                mtime: None,
                mode: 0,
                uid: 0,
                gid: 0,
                locator: Locator::None,
                synthetic: true,
            };
            if !Self::insert(data, synthetic, limits) {
                return false;
            }
        }

        match data.by_path.get(&member.path).copied() {
            Some(at) => {
                // Three cases, and the difference between them is what the
                // user is or is not told (errors are never silent: a listing
                // is never quietly short).
                //
                // 1. A real entry **replaces the placeholder** that stood for
                //    it - unless the placeholder is a directory with members
                //    below it, in which case replacing it would orphan them:
                //    they would stay in `children` under a path that is no
                //    longer a directory, so nothing would ever walk them and
                //    they would vanish from the listing and from every
                //    extraction. The directory stands and the colliding entry
                //    is refused.
                // 2. A later entry of the **same kind** replaces an earlier
                //    one - zip permits duplicate names and every tool takes
                //    the last - but the earlier one is *gone*, so it is
                //    counted. It may not even be a duplicate the container
                //    holds: two distinct non-UTF-8 names can both decode to
                //    the same replacement characters, and the collision is
                //    then one this program manufactured.
                // 3. A *different* kind at the same path is a container
                //    disagreeing with itself; the first answer stands.
                let Some(existing) = data.members.get_mut(at) else {
                    return true;
                };
                let same_kind =
                    std::mem::discriminant(&existing.kind) == std::mem::discriminant(&member.kind);
                let placeholder = existing.synthetic;
                let has_children = data
                    .children
                    .get(&member.path)
                    .is_some_and(|kids| !kids.is_empty());
                if placeholder && !same_kind && has_children {
                    let path = member.path.clone();
                    data.refused = data.refused.saturating_add(1);
                    data.first_refusal.get_or_insert_with(|| {
                        format!(
                            "{path}: refused - the archive holds both this \
                             entry and members inside a directory of the same \
 name"
                        )
                    });
                } else if placeholder || same_kind {
                    let displaced = !placeholder;
                    let path = std::mem::take(&mut existing.path);
                    let name = path.clone();
                    *existing = Member { path, ..member };
                    if displaced {
                        data.refused = data.refused.saturating_add(1);
                        data.first_refusal.get_or_insert_with(|| {
                            format!(
                                "{name}: two entries share this name; only the \
 last one is listed"
                            )
                        });
                    }
                } else {
                    data.refused = data.refused.saturating_add(1);
                    data.first_refusal.get_or_insert_with(|| {
                        format!(
                            "{}: two entries of different kinds share this name",
                            member.path
                        )
                    });
                }
                true
            }
            None => {
                let at = data.members.len();
                data.name_bytes = data.name_bytes.saturating_add(member.path.len());
                data.by_path.insert(member.path.clone(), at);
                data.children.entry(parent).or_default().push(at);
                // Deliberately no empty child list for `member` itself: a
                // directory with no children is answered by `by_path` in
                // `is_dir`, and a `HashMap` entry per member would double the
                // index's memory for nothing.
                data.members.push(member);
                true
            }
        }
    }
}

impl IndexSink for Builder {
    fn push(&mut self, raw: RawMember) -> bool {
        if self.index.cancelled() {
            return false;
        }
        // The container this is reading has been replaced. Nothing it still
        // has to say is about the file the index now describes, and there is
        // no point reading the rest of it either.
        if self.superseded() {
            return false;
        }

        let path = match normalize_member(&raw.name, self.backslash_separators) {
            Ok(path) => path,
            // `.`, `./` and `/` name the archive's own root, and a tar written
            // by `tar -czf x.tgz .` opens with exactly that member. The root is
            // already the listing the panel is looking at, so there is nothing
            // to add and nothing to warn about: dropping it silently is what
            // stops the refusal counter - which exists for Zip Slip
            // - from crying wolf on the single most ordinary tar there is.
            // Only a *directory* is let through this way. A file with no name
            // at all is malformed rather than root, cannot be extracted to
            // anything, and keeps its refusal.
            Err(Unsafe::Empty) if matches!(raw.kind, MemberKind::Dir) => {
                return !self.index.cancelled();
            }
            Err(why) => {
                // refused, and refused *here*, so the member
                // never exists in the index and therefore can never be listed,
                // opened or extracted. Counted, so the panel can say so.
                let mut data = self.index.lock();
                data.refused = data.refused.saturating_add(1);
                let name = raw.name.clone();
                data.first_refusal
                    .get_or_insert_with(|| format!("{name}: rejected - {}", why.reason()));
                drop(data);
                return !self.index.cancelled();
            }
        };
        let member = Member {
            path,
            kind: raw.kind,
            size: raw.size,
            mtime: raw.mtime,
            mode: raw.mode,
            uid: raw.uid,
            gid: raw.gid,
            locator: raw.locator,
            synthetic: false,
        };

        let keep_going = {
            let mut data = self.index.lock();
            Builder::insert(&mut data, member, self.limits)
        };

        self.since_wake = self.since_wake.saturating_add(1);
        if self.since_wake >= INDEX_NOTIFY_BATCH {
            self.flush();
        }
        keep_going && !self.index.cancelled()
    }

    fn cancelled(&self) -> bool {
        self.index.cancelled()
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(names: &[&str]) -> Arc<Index> {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            for (i, name) in names.iter().enumerate() {
                assert!(sink.push(RawMember::file(*name, 10, Locator::Ordinal(i))));
            }
        }
        index.finish(IndexStatus::Complete);
        index
    }

    #[test]
    fn a_finished_build_cannot_speak_for_the_rebuild_that_replaced_it() {
        // The order a write into an archive produces on a loaded machine: the
        // build thread sets its final status, is descheduled before its
        // last line runs, the write commits and invalidates the index in the
        // meantime, and the old thread then finishes what it was doing. What
        // it has to say is about a container that no longer exists.
        let index = Arc::new(Index::new());
        let first = index.epoch();
        index.finish_epoch(first, IndexStatus::Complete);

        let rebuild = index.invalidate();
        index.finish_epoch(
            first,
            IndexStatus::Failed("the archive reader stopped unexpectedly".to_string()),
        );
        assert_eq!(
            index.status(),
            IndexStatus::Building,
            "the rebuild is still running, whatever the build before it says"
        );

        // And a member the old build was still producing is not mixed into
        // the new one either.
        let mut stale = Builder::for_epoch(Arc::clone(&index), false, first);
        assert!(
            !stale.push(RawMember::file("gone.txt", 1, Locator::Ordinal(0))),
            "a superseded sink stops rather than reading the rest"
        );
        assert!(index.get("gone.txt").is_none());

        let mut fresh = Builder::for_epoch(Arc::clone(&index), false, rebuild);
        assert!(fresh.push(RawMember::file("new.txt", 1, Locator::Ordinal(0))));
        drop(fresh);
        index.finish_epoch(rebuild, IndexStatus::Complete);
        assert_eq!(index.status(), IndexStatus::Complete);
        assert!(index.get("new.txt").is_some());
    }

    #[test]
    fn parents_are_synthesised_once() {
        let index = build(&["a/b/c.txt", "a/b/d.txt", "a/e.txt"]);
        assert_eq!(index.len(), 5, "a, a/b, and the three files");
        assert!(index.is_dir("a"));
        assert!(index.is_dir("a/b"));
        assert!(index.is_dir(""), "the root always exists");

        let (cursor, rows, status) = index.children_from("", 0);
        assert_eq!(status, IndexStatus::Complete);
        assert_eq!(cursor, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a");
        assert!(rows[0].is_dir());

        let (_, rows, _) = index.children_from("a/b", 0);
        let mut names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["c.txt", "d.txt"]);
    }

    #[test]
    fn a_real_directory_replaces_the_synthetic_one() {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            sink.push(RawMember::file("a/b.txt", 1, Locator::Ordinal(0)));
            let mut dir = RawMember::dir("a");
            dir.mode = 0o755;
            dir.mtime = Some(SystemTime::UNIX_EPOCH);
            sink.push(dir);
        }
        index.finish(IndexStatus::Complete);
        let a = index.get("a").expect("a");
        assert!(!a.synthetic);
        assert_eq!(a.mode, 0o755);
        assert_eq!(index.len(), 2, "no duplicate row for `a`");
        let (_, rows, _) = index.children_from("", 0);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_unsafe_name_never_enters_the_index() {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            for name in ["../../etc/passwd", "/etc/shadow", "ok.txt", "a/../b"] {
                sink.push(RawMember::file(name, 1, Locator::Ordinal(0)));
            }
        }
        index.finish(IndexStatus::Complete);
        assert_eq!(index.len(), 1, "only ok.txt survived");
        assert!(index.get("ok.txt").is_some());
        let (count, first) = index.refusals();
        assert_eq!(count, 3);
        assert!(
            first.is_some_and(|f| f.contains("rejected")),
            "the first refusal says why"
        );
    }

    #[test]
    fn stat_blocking_returns_when_the_build_finishes() {
        let index = Arc::new(Index::new());
        let writer = Arc::clone(&index);
        let handle = std::thread::spawn(move || {
            let mut sink = Builder::new(Arc::clone(&writer), false);
            sink.push(RawMember::file("late.txt", 7, Locator::Ordinal(0)));
            drop(sink);
            writer.finish(IndexStatus::Complete);
        });
        let member = index.stat_blocking("late.txt").expect("the member arrives");
        assert_eq!(member.size, 7);
        assert!(matches!(
            index.stat_blocking("never.txt"),
            Err(Error::NotFound(_))
        ));
        handle.join().expect("the writer thread");
    }

    #[test]
    fn cancelling_stops_the_sink() {
        let index = Arc::new(Index::new());
        let mut sink = Builder::new(Arc::clone(&index), false);
        assert!(sink.push(RawMember::file("a.txt", 1, Locator::Ordinal(0))));
        index.cancel();
        assert!(!sink.push(RawMember::file("b.txt", 1, Locator::Ordinal(1))));
        assert!(sink.cancelled());
    }

    #[test]
    fn a_symlink_to_a_directory_is_reported_as_one() {
        let index = Arc::new(Index::new());
        {
            let mut sink = Builder::new(Arc::clone(&index), false);
            sink.push(RawMember::dir("d"));
            let mut link = RawMember::file("l", 0, Locator::None);
            link.kind = MemberKind::Symlink("d".to_string());
            sink.push(link);
            let mut broken = RawMember::file("b", 0, Locator::None);
            broken.kind = MemberKind::Symlink("nowhere".to_string());
            sink.push(broken);
        }
        index.finish(IndexStatus::Complete);
        let (_, rows, _) = index.children_from("", 0);
        let by_name: HashMap<&str, &Entry> = rows.iter().map(|e| (e.name.as_str(), e)).collect();
        assert!(by_name["l"].is_dir(), "resolves to a directory");
        assert!(by_name["l"].is_symlink());
        assert!(!by_name["b"].is_dir(), "a broken link is not a directory");
    }

    #[test]
    fn a_bounded_index_truncates_rather_than_growing() {
        // the design does not put a number on it; this module does, and the
        // rule is that the listing is a prefix of the truth and says so rather
        // than the process growing until it is killed.
        let index = Arc::new(Index::new());
        let limits = Limits {
            members: 4,
            name_bytes: MAX_NAME_BYTES,
        };
        {
            let mut sink = Builder::with_limits(Arc::clone(&index), false, limits);
            let mut accepted = 0;
            for i in 0..100 {
                if sink.push(RawMember::file(format!("f{i}.txt"), 1, Locator::Ordinal(i))) {
                    accepted += 1;
                }
            }
            assert!(accepted < 100, "the sink stops rather than accepting");
        }
        assert_eq!(index.len(), 4);
        assert!(
            matches!(index.status(), IndexStatus::Truncated(_)),
            "{:?}",
            index.status()
        );

        // And on names, which is the shape a bomb built out of paths has.
        let index = Arc::new(Index::new());
        let limits = Limits {
            members: MAX_MEMBERS,
            name_bytes: 100,
        };
        {
            let mut sink = Builder::with_limits(Arc::clone(&index), false, limits);
            for i in 0..100 {
                sink.push(RawMember::file(
                    format!("{}-{i}", "n".repeat(60)),
                    1,
                    Locator::Ordinal(i),
                ));
            }
        }
        assert!(index.len() < 100);
        assert!(matches!(index.status(), IndexStatus::Truncated(_)));
    }

    #[test]
    fn names_and_parents_split_correctly() {
        let m = Member {
            path: "a/b/c.txt".to_string(),
            kind: MemberKind::File,
            size: 0,
            mtime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            locator: Locator::None,
            synthetic: false,
        };
        assert_eq!(m.name(), "c.txt");
        assert_eq!(m.parent(), "a/b");
        let root = Member {
            path: "top".to_string(),
            ..m
        };
        assert_eq!(root.name(), "top");
        assert_eq!(root.parent(), "");
    }
}
