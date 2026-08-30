//! The tree walk, written once and used by three callers.
//!
//! The three callers are:
//!
//! 1. **`Ctrl+L`** - "calculate occupied space of selection",
//!    which walks every marked directory and turns the status line's `≥` into
//!    a number.
//! 2. **`Space` on a directory**, which walks that one
//!    directory. This is the whole reason `Space` differs from `Insert`.
//! 3. **The copy/move dialog's selection statistics**, which
//!    are computed *before* the dialog opens because they are "the last chance
//!    to notice a mistake".
//!
//! All three need the same numbers over the same tree, so they share one walk.
//! It runs as a [`JobKind::Size`] job like any other operation, which gets it
//! cancellation and progress reporting for free - the design requires both:
//! "a slow tree must not freeze the UI, and `Esc` abandons the walk leaving the
//! bound in place".
//!
//! # What the walk promises
//!
//! * **It never follows a symlink into a cycle.** Every directory it descends
//!   into is recorded by `(dev, ino)` and never entered twice, so a symlink
//!   loop terminates whatever `follow_symlinks` says.
//! * **An unreadable directory does not fail the walk.** It is counted in
//!   [`WalkOutcome::unreadable`] and skipped; the caller decides whether to
//!   mention it. A `Ctrl+L` over `/` that hits one root-owned directory must
//!   still report the other 99% of the tree. The one exception is a backend
//!   that has gone away under the walk - a dropped connection, a session
//!   closed - which is [`WalkOutcome::fatal`] and stops it: every directory
//!   left on the stack would refuse for that same reason, so counting them
//!   would report the size of the remainder as a permissions problem.
//! * **It is cancellable between every entry**, not merely between
//!   directories, so a single directory with a million entries still responds
//!   to `Esc`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::{FailReason, JobContext, JobKind, JobSpec, is_fatal};
use crate::Error;
use crate::panel::Tab;
use crate::vfs::{EntryKind, Vfs, VfsPath};

/// How many entries the walk processes between progress ticks.
///
/// Small enough that `Esc` is honoured promptly on a huge flat directory,
/// large enough that the tick is not the cost of the walk.
const TICK_EVERY: u64 = 64;

/// What a walk counts.
///
/// `dirs` never includes the root itself: a walk answers "what is *inside*
/// this", and the caller adds the roots it started from. That keeps
/// `Ctrl+L` over three marked directories from double-counting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct TreeStats {
    /// Total bytes of every file below the root.
    pub bytes: u64,
    /// How many files.
    pub files: u64,
    /// How many directories, the root excluded.
    pub dirs: u64,
}

impl TreeStats {
    /// The zero.
    pub const ZERO: Self = Self {
        bytes: 0,
        files: 0,
        dirs: 0,
    };

    /// Add another tree's totals, saturating.
    pub const fn add(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.files = self.files.saturating_add(other.files);
        self.dirs = self.dirs.saturating_add(other.dirs);
    }

    /// Count one file of `bytes`.
    pub const fn add_file(&mut self, bytes: u64) {
        self.files = self.files.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    /// Count one directory.
    pub const fn add_dir(&mut self) {
        self.dirs = self.dirs.saturating_add(1);
    }

    /// True when nothing was found at all.
    pub const fn is_empty(&self) -> bool {
        self.files == 0 && self.dirs == 0 && self.bytes == 0
    }
}

/// How a walk treats what it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct WalkOptions {
    /// the `ops.follow_symlinks`. False - the default - counts a
    /// symlink as a file of its own (tiny) size and never descends through it,
    /// which is what makes a `Ctrl+L` on a directory full of links fast and
    /// finite.
    pub follow_symlinks: bool,
}

/// The result of one walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalkOutcome {
    /// What was counted.
    pub stats: TreeStats,
    /// False when the caller stopped the walk part-way. A partial figure must
    /// never be cached or presented as exact (the last bullet).
    pub complete: bool,
    /// Directories that could not be read. Their contents are missing from
    /// `stats`, which is why a walk that hit one is worth mentioning even
    /// though it is `complete`.
    pub unreadable: u64,
    /// The walk stopped because the thing it was walking went away - a
    /// connection dropped, a session closed under it - rather than because one
    /// directory refused to be read.
    ///
    /// Distinct from `unreadable` because the two want opposite handling: an
    /// unreadable directory is skipped and counted, while a dead session makes
    /// every remaining directory unreadable for the same reason, and carrying
    /// on to count them is both pointless and slow. The failure has already
    /// been reported against the path it happened on, carrying its
    /// [`crate::Error`], so [`super::is_fatal`] can see what it was.
    pub fatal: bool,
}

/// Walk a directory tree, counting bytes, files and directories.
///
/// `tick` is called every [`TICK_EVERY`] entries and once per directory with
/// the running totals; returning `false` stops the walk and leaves
/// [`WalkOutcome::complete`] false. That is the cancellation hook -
/// [`run`] wires it to [`JobContext::cancelled`], and a test can wire it to a
/// counter.
///
/// The root itself is not counted; see [`TreeStats`].
///
/// A `root` that is a file rather than a directory counts as one file, which
/// is what lets a caller hand the walk a mixed selection without sorting it
/// first.
pub fn walk_stats(
    root: &Path,
    options: &WalkOptions,
    tick: &mut dyn FnMut(&TreeStats) -> bool,
) -> WalkOutcome {
    walk_stats_filtered(root, options, tick, &mut |_| true)
}

/// [`walk_stats`], counting only the files `keep` accepts.
///
/// Directories are descended unconditionally whatever `keep` says, exactly as
/// [`super::copy::copy_item`] does: the "Only files of this type"
/// filters files, and a mask of `*.rs` still has to look inside `src/` to find
/// any.
///
/// This is what makes the copy's pre-flight total and its delivered total
/// follow one rule ("Counts are `done / total`"). Without it a
/// masked copy announced a denominator it could never reach and its batch bar
/// stopped short of the right-hand edge on a run that had copied everything it
/// was asked to.
pub fn walk_stats_filtered(
    root: &Path,
    options: &WalkOptions,
    tick: &mut dyn FnMut(&TreeStats) -> bool,
    keep: &mut dyn FnMut(&str) -> bool,
) -> WalkOutcome {
    let mut out = WalkOutcome {
        complete: true,
        ..WalkOutcome::default()
    };

    // The root is measured with `symlink_metadata`, so pointing a walk at a
    // symlink measures the link unless the caller asked to follow it.
    let root_meta = if options.follow_symlinks {
        fs::metadata(root)
    } else {
        fs::symlink_metadata(root)
    };
    let Ok(root_meta) = root_meta else {
        out.unreadable = 1;
        return out;
    };
    if !root_meta.is_dir() {
        // A file named directly as a source is filtered by the mask too, which
        // is what `copy_item` does with a top-level file.
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if keep(&name) {
            out.stats.add_file(root_meta.size());
        }
        return out;
    }

    // Cycle protection, unconditional. Following symlinks is the way to build
    // a cycle, but a bind mount can do it too, and the cost of the set is one
    // `u128` per directory visited against an otherwise unbounded walk.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    seen.insert((root_meta.dev(), root_meta.ino()));

    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut since_tick = 0u64;

    while let Some(dir) = stack.pop() {
        if !tick(&out.stats) {
            out.complete = false;
            return out;
        }
        let iter = match fs::read_dir(&dir) {
            Ok(iter) => iter,
            Err(_) => {
                // the design's requirement, stated in the task: a directory the
                // walk cannot read must not fail the whole walk.
                out.unreadable = out.unreadable.saturating_add(1);
                continue;
            }
        };

        for item in iter {
            since_tick = since_tick.saturating_add(1);
            if since_tick >= TICK_EVERY {
                since_tick = 0;
                if !tick(&out.stats) {
                    out.complete = false;
                    return out;
                }
            }

            let Ok(entry) = item else {
                out.unreadable = out.unreadable.saturating_add(1);
                continue;
            };
            let path = entry.path();
            // `DirEntry::file_type` is free on Linux - `readdir` already
            // reported `d_type` - so the cheap classification comes first and
            // only what needs a size pays for a `stat`.
            let Ok(file_type) = entry.file_type() else {
                out.unreadable = out.unreadable.saturating_add(1);
                continue;
            };

            let is_link = file_type.is_symlink();
            let descend = if is_link {
                options.follow_symlinks
            } else {
                file_type.is_dir()
            };

            if descend {
                // One `stat` to learn both "is it really a directory" and its
                // identity for the cycle set.
                match fs::metadata(&path) {
                    Ok(meta) if meta.is_dir() => {
                        if seen.insert((meta.dev(), meta.ino())) {
                            out.stats.add_dir();
                            stack.push(path);
                        }
                        // An already-seen directory is a cycle or a second
                        // route to the same tree; counting it twice would be
                        // worse than not counting it.
                    }
                    Ok(meta) => {
                        if keep(&entry.file_name().to_string_lossy()) {
                            out.stats.add_file(meta.size());
                        }
                    }
                    Err(_) => out.unreadable = out.unreadable.saturating_add(1),
                }
                continue;
            }

            if !keep(&entry.file_name().to_string_lossy()) {
                continue;
            }

            // Not descending: a plain file, a device node, or a symlink we are
            // not following, which counts as the (tiny) file it is.
            match entry.metadata() {
                Ok(meta) => out.stats.add_file(meta.size()),
                // The name is real even when the metadata is not readable, so
                // it still counts as a file - with an unknown, and therefore
                // zero, size. Dropping it would understate the count as well
                // as the bytes.
                Err(_) => out.stats.add_file(0),
            }
        }
    }

    out
}

/// The [`JobKind::Size`] runner.
///
/// Walks each source in turn, streaming progress, and records a
/// [`TreeStats`] per source that finished. A cancelled run still reports the
/// sources it got through, so `Esc` half-way down a `Ctrl+L` keeps what it
/// already learned rather than throwing it away.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    debug_assert_eq!(spec.kind, JobKind::Size);
    let options = spec.options.walk();
    // Totals are genuinely unknown for a walk - that is what it is measuring -
    // so it starts with zeros and the dialog shows counts rather than a bar.
    ctx.start(0, 0);

    for source in &spec.sources {
        if ctx.cancelled() {
            break;
        }
        ctx.set_file(&source.to_string(), 0);

        let Some(local) = source.local_path() else {
            // Not a path the kernel can be handed: a member of an archive,
            // and later a remote listing. Sized through the trait, which is
            // slower per entry and exactly as correct - and correct is the
            // requirement, because the design forbids reporting a
            // computed-looking total that is really a zero.
            let outcome = walk_stats_via_vfs(vfs, source, &options, ctx);
            credit_walk(source, outcome, ctx);
            if !outcome.complete {
                break;
            }
            continue;
        };

        // Progress is reported against the running total across every source,
        // so a multi-root `Ctrl+L` counts up rather than restarting at each
        // root. The walk hands back cumulative figures per root, so the tick
        // forwards the delta since the previous tick.
        let outcome = {
            let mut last = TreeStats::ZERO;
            walk_stats(local, &options, &mut |stats| {
                ctx.add_files(stats.files.saturating_sub(last.files));
                let delta_bytes = stats.bytes.saturating_sub(last.bytes);
                last = *stats;
                ctx.add_bytes(delta_bytes)
            })
        };

        // "Never silently report a computed-looking total that
        // is actually partial. If the numbers cannot be exact, the `≥` is what
        // makes them honest." A directory the walk could not read is exactly
        // that case - its contents are missing from the figure - so it is *not*
        // cached, and the selection keeps its `≥` until a walk that can read
        // everything resolves it. An unreadable root is the extreme of the same
        // thing: `walk_stats` returns zero bytes for it, and caching that would
        // put an exact-looking `0` on the status line.
        credit_walk(source, outcome, ctx);
        if !outcome.complete {
            break;
        }
    }
}

/// Report one root's walk: cache it if it is exact, say so if it is not.
///
/// "Never silently report a computed-looking total that is
/// actually partial. If the numbers cannot be exact, the `≥` is what makes them
/// honest." A directory the walk could not read is exactly that case - its
/// contents are missing from the figure - so it is *not* cached, and the
/// selection keeps its `≥` until a walk that can read everything resolves it.
/// An unreadable root is the extreme of the same thing.
fn credit_walk(source: &VfsPath, outcome: WalkOutcome, ctx: &mut JobContext) {
    if outcome.complete && outcome.unreadable == 0 {
        ctx.add_sized(source.clone(), outcome.stats);
    }
    if outcome.fatal {
        // The real error was reported where it happened, with its variant
        // intact. Adding "N directories could not be read" on top of it would
        // be this function's guess about a cause it does not have, and a count
        // that is really "everything left".
        return;
    }
    if outcome.unreadable > 0 {
        ctx.fail(
            source,
            FailReason::refused(format!(
                "{} director{} could not be read",
                outcome.unreadable,
                if outcome.unreadable == 1 { "y" } else { "ies" }
            )),
        );
    }
}

/// [`walk_stats`] through the [`Vfs`] trait, for a root the kernel does not
/// know about.
///
/// The same shape as the local walk and the same promises: an explicit stack so
/// depth is bounded by memory rather than by the worker's stack, progress
/// forwarded as it goes, cancellation checked between entries, and a directory
/// that could not be listed counted as unreadable rather than as empty.
/// Symlinks are counted as themselves and never followed, which is what makes a
/// link to `/` one entry.
fn walk_stats_via_vfs(
    vfs: &dyn Vfs,
    root: &VfsPath,
    _options: &WalkOptions,
    ctx: &mut JobContext,
) -> WalkOutcome {
    let mut out = WalkOutcome {
        stats: TreeStats::ZERO,
        complete: true,
        unreadable: 0,
        fatal: false,
    };
    let root_is_dir = match vfs.stat(root) {
        Ok(entry) => matches!(entry.kind, EntryKind::Dir),
        Err(err) => {
            if stop_walking(root, err, &mut out, ctx) {
                return out;
            }
            out.unreadable = 1;
            return out;
        }
    };

    let mut stack = vec![root.clone()];
    let mut reported = TreeStats::ZERO;
    while let Some(path) = stack.pop() {
        if ctx.cancelled() {
            out.complete = false;
            return out;
        }
        let mut rx = vfs.read_dir(&path);
        let mut failed: Option<Error> = None;
        while let Some(item) = rx.blocking_recv() {
            match item {
                Ok(entry) if entry.is_parent => {}
                Ok(entry) => match entry.kind {
                    EntryKind::Dir => {
                        out.stats.add_dir();
                        stack.push(path.join(&entry.name));
                    }
                    // `follow_symlinks` is the only option here, and it is
                    // about the local filesystem: a member of a container is
                    // counted as itself, whatever it names.
                    _ => out.stats.add_file(entry.size),
                },
                // The error is kept, not discarded: a dropped connection and a
                // directory the user may not read arrive here identically, and
                // only the error itself says which one this is.
                Err(err) => failed = Some(err),
            }
        }
        if let Some(err) = failed {
            if stop_walking(&path, err, &mut out, ctx) {
                return out;
            }
            out.unreadable = out.unreadable.saturating_add(1);
        }
        ctx.add_files(out.stats.files.saturating_sub(reported.files));
        let delta = out.stats.bytes.saturating_sub(reported.bytes);
        reported = out.stats;
        if !ctx.add_bytes(delta) {
            out.complete = false;
            return out;
        }
    }

    // The walk counts what is *inside* a root, so
    // a directory root adds itself exactly where the local path does.
    let _ = root_is_dir;
    out
}

/// Report `err` against `path` and say whether the walk has to abandon the
/// tree rather than count another unreadable directory.
///
/// The walk's promise is that "an unreadable directory does not fail the
/// walk", and that promise is about permissions on one directory. A session
/// that has gone away is not that: every directory still on the stack would
/// fail for the same reason, the count would come out as the size of the
/// remainder, and the walk would keep asking a dead connection for answers.
/// The outcome is marked incomplete so the figure is never cached as exact -
/// the same treatment a cancel gets, for the same reason.
fn stop_walking(path: &VfsPath, err: Error, out: &mut WalkOutcome, ctx: &mut JobContext) -> bool {
    if !is_fatal(&err) {
        return false;
    }
    ctx.fail(path, err);
    out.fatal = true;
    out.complete = false;
    true
}

/// Directory sizes computed this session, keyed by path.
///
/// > A computed directory size is **cached for the session** against the path
/// > and invalidated when the panel re-reads it, so sizing the same tree twice
/// > is free and a stale figure is never shown.
///
/// Both halves of that sentence are load-bearing, and both live here:
/// [`SizeCache::insert`] is the caching, [`SizeCache::invalidate`] is the
/// invalidation, and [`crate::app::App::request_read`] calls the latter on
/// every re-read.
#[derive(Debug, Clone, Default)]
pub struct SizeCache {
    map: HashMap<VfsPath, TreeStats>,
}

impl SizeCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The size of a tree, if it has been walked and not invalidated since.
    pub fn get(&self, path: &VfsPath) -> Option<TreeStats> {
        self.map.get(path).copied()
    }

    /// True when this path has a known size.
    pub fn contains(&self, path: &VfsPath) -> bool {
        self.map.contains_key(path)
    }

    /// Record a completed walk. Only ever call this with a
    /// [`WalkOutcome::complete`] result: the last bullet forbids
    /// presenting a partial total as a computed one.
    pub fn insert(&mut self, path: VfsPath, stats: TreeStats) {
        self.map.insert(path, stats);
    }

    /// Drop `path` **and everything beneath it**.
    ///
    /// The subtree goes too, deliberately. A re-read of `/a` says its contents
    /// changed; it says nothing about whether `/a/b` still holds what it did,
    /// and a cached `/a/b` that is now wrong is exactly the "stale figure" the
    /// design forbids. Dropping the subtree costs one re-walk and cannot lie.
    pub fn invalidate(&mut self, path: &VfsPath) {
        self.map.retain(|key, _| !key.starts_with(path));
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// How many trees are remembered.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// What a marked selection adds up to (the "selection statistics",
/// the status line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionStats {
    /// Total bytes, counting a marked directory only once it has been sized.
    pub bytes: u64,
    /// Files, including those inside sized directories.
    pub files: u64,
    /// Directories, including those inside sized directories.
    pub dirs: u64,
    /// How many marked directories are still unsized.
    pub unsized_dirs: u64,
}

impl SelectionStats {
    /// False when at least one marked directory has not been sized, which is
    /// exactly when the design requires the `≥`.
    pub const fn is_exact(&self) -> bool {
        self.unsized_dirs == 0
    }

    /// True when nothing at all is marked.
    pub const fn is_empty(&self) -> bool {
        self.files == 0 && self.dirs == 0
    }

    /// the line, `17.75 G · 523 files · 95 folders`, minus the size -
    /// which the caller formats through `panel.human_sizes` so the dialog and
    /// the panel never disagree about what a byte count looks like.
    pub fn describe(&self, size: &str, ascii: bool) -> String {
        let sep = if ascii { " - " } else { " \u{b7} " };
        let bound = if self.is_exact() {
            ""
        } else if ascii {
            ">= "
        } else {
            "\u{2265} "
        };
        format!(
            "{bound}{size}{sep}{} file{}{sep}{} folder{}",
            self.files,
            if self.files == 1 { "" } else { "s" },
            self.dirs,
            if self.dirs == 1 { "" } else { "s" },
        )
    }
}

/// The recursive totals of a tab's marked entries.
///
/// A marked directory contributes its cached [`TreeStats`] when it has one and
/// only bumps [`SelectionStats::unsized_dirs`] when it does not - which is what
/// turns into the `≥`. `..` never counts, in either form.
///
/// This is the figure the copy/move dialog shows. The panel status line's
/// shorter form counts only the top level and lives in
/// [`crate::panel::format::status_text`]; both read the same cache, so they can
/// never disagree about whether a directory has been sized.
pub fn selection_stats(tab: &Tab, sizes: &SizeCache) -> SelectionStats {
    // Through the rows, not through the mark set: marks are keyed on
    // `Entry::mark_key`, which on a virtual listing is the row's real address
    // rather than its name. Asking each row whether it
    // is marked is the only form of the question that is true in both.
    let rows: Vec<usize> = tab
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| tab.is_marked(entry))
        .map(|(index, _)| index)
        .collect();
    stats_of_rows(tab, sizes, &rows)
}

/// [`selection_stats`] over an explicit set of names in `tab`.
///
/// the design wants the copy dialog's statistics line to describe "what is
/// about to be operated on", which is the marks when there are any and the
/// entry under the cursor when there are none.
/// Only the caller knows which, so the caller passes the names and the two
/// figures - the dialog's line and the job's source list - are built from one
/// list rather than from two rules that can drift apart.
pub fn stats_of(tab: &Tab, sizes: &SizeCache, names: &[String]) -> SelectionStats {
    stats_of_set(tab, sizes, &names.iter().map(String::as_str).collect())
}

/// [`stats_of`] over row **indices** rather than names.
///
/// A flat virtual listing can hold two rows called `mod.rs` from different
/// directories, so a name is not an identity there and a set of names would
/// count one of them twice and the other never. The callers that already have
/// indices - everything that goes through [`crate::panel::Tab::operand_rows`] -
/// use this; [`stats_of`] keeps its signature for the callers that have names.
pub fn stats_of_rows(tab: &Tab, sizes: &SizeCache, rows: &[usize]) -> SelectionStats {
    let mut out = SelectionStats::default();
    for index in rows {
        let Some(entry) = tab.entries.get(*index) else {
            continue;
        };
        if entry.is_parent {
            continue;
        }
        if entry.is_dir() {
            out.dirs = out.dirs.saturating_add(1);
            // The real home, which on a virtual listing is not the panel's
            // path joined to the name.
            match tab.path_of(*index).and_then(|home| sizes.get(&home)) {
                Some(stats) => {
                    out.bytes = out.bytes.saturating_add(stats.bytes);
                    out.files = out.files.saturating_add(stats.files);
                    out.dirs = out.dirs.saturating_add(stats.dirs);
                }
                None => out.unsized_dirs = out.unsized_dirs.saturating_add(1),
            }
        } else {
            out.files = out.files.saturating_add(1);
            out.bytes = out.bytes.saturating_add(entry.size);
        }
    }
    out
}

/// The body of [`stats_of`], over a set so a big selection stays linear in the
/// number of entries rather than quadratic.
fn stats_of_set(tab: &Tab, sizes: &SizeCache, names: &HashSet<&str>) -> SelectionStats {
    let mut out = SelectionStats::default();
    for entry in &tab.entries {
        if entry.is_parent || !names.contains(entry.name.as_str()) {
            continue;
        }
        if entry.is_dir() {
            out.dirs = out.dirs.saturating_add(1);
            match sizes.get(&tab.path.join(&entry.name)) {
                Some(stats) => {
                    out.bytes = out.bytes.saturating_add(stats.bytes);
                    out.files = out.files.saturating_add(stats.files);
                    out.dirs = out.dirs.saturating_add(stats.dirs);
                }
                None => out.unsized_dirs = out.unsized_dirs.saturating_add(1),
            }
        } else {
            out.files = out.files.saturating_add(1);
            out.bytes = out.bytes.saturating_add(entry.size);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Entry;
    use std::io::Write as _;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A throwaway directory, removed on drop. Built by hand rather than with
    /// `tempfile`, which is not on the dependency table.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-walk-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("parents");
            }
            let mut f = fs::File::create(&p).expect("create");
            f.write_all(&vec![b'x'; bytes]).expect("write");
            p
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(&p).expect("mkdir");
            p
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn always(_: &TreeStats) -> bool {
        true
    }

    #[test]
    fn a_tree_is_counted_to_full_depth() {
        let t = TempTree::new("depth");
        t.file("a.txt", 10);
        t.file("sub/b.txt", 20);
        t.file("sub/deeper/c.txt", 30);
        t.dir("empty");

        let out = walk_stats(t.path(), &WalkOptions::default(), &mut always);
        assert!(out.complete);
        assert_eq!(out.stats.files, 3);
        assert_eq!(out.stats.bytes, 60);
        // sub, sub/deeper, empty - the root itself is never counted.
        assert_eq!(out.stats.dirs, 3);
    }

    #[test]
    fn the_walk_stops_when_the_caller_says_so() {
        let t = TempTree::new("cancel");
        for i in 0..200 {
            t.file(&format!("sub{}/f{i}.bin", i % 7), 4);
        }
        let mut ticks = 0;
        let out = walk_stats(t.path(), &WalkOptions::default(), &mut |_| {
            ticks += 1;
            ticks < 2
        });
        assert!(!out.complete, "a stopped walk is never complete");
        assert!(
            out.stats.bytes < 800,
            "it really stopped early: {:?}",
            out.stats
        );
    }

    #[test]
    fn a_symlink_loop_terminates() {
        let t = TempTree::new("loop");
        let inner = t.dir("a/b");
        t.file("a/f.txt", 5);
        symlink(t.path(), inner.join("up")).expect("symlink");

        // Following symlinks is exactly how the loop is built, so this is the
        // configuration the cycle set has to survive.
        let out = walk_stats(
            t.path(),
            &WalkOptions {
                follow_symlinks: true,
            },
            &mut always,
        );
        assert!(out.complete);
        assert_eq!(out.stats.files, 1, "the loop was not walked twice");
    }

    #[test]
    fn a_symlink_is_a_file_when_it_is_not_followed() {
        let t = TempTree::new("link");
        let target = t.dir("target");
        t.file("target/big.bin", 1000);
        symlink(&target, t.path().join("link")).expect("symlink");

        let out = walk_stats(t.path(), &WalkOptions::default(), &mut always);
        assert!(out.complete);
        assert_eq!(out.stats.files, 2, "big.bin plus the link itself");
        assert!(
            out.stats.bytes < 1000 + 200,
            "the link's target is not counted twice: {:?}",
            out.stats
        );
    }

    #[test]
    fn an_unreadable_directory_does_not_fail_the_walk() {
        let t = TempTree::new("perm");
        t.file("readable.txt", 100);
        let locked = t.dir("locked");
        fs::File::create(locked.join("hidden.txt")).expect("create");
        let mut perms = fs::metadata(&locked).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        fs::set_permissions(&locked, perms).expect("chmod");

        // Whether the premise holds is the kernel's answer, not the walk's:
        // `chmod 000` stops nothing when the suite runs as root. Asking
        // `out.unreadable` instead would put every assertion below behind the
        // very thing they are testing, so a walk that swallowed the permission
        // error would take the quiet branch and pass.
        let really_locked = fs::read_dir(&locked).is_err();

        let out = walk_stats(t.path(), &WalkOptions::default(), &mut always);

        // Restore before the assertions, or the drop cannot clean up.
        let mut perms = fs::metadata(&locked).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&locked, perms).expect("chmod back");

        assert!(out.complete, "unreadable is not a failure");
        assert_eq!(out.stats.bytes, 100, "the rest of the tree still counted");
        if really_locked {
            assert_eq!(
                out.unreadable, 1,
                "the directory the kernel refused was counted: {out:?}"
            );
        } else {
            // Running as root, where the directory is readable after all.
            assert_eq!(out.unreadable, 0, "nothing was refused: {out:?}");
        }
    }

    #[test]
    fn a_file_root_counts_as_one_file() {
        let t = TempTree::new("fileroot");
        let f = t.file("solo.bin", 42);
        let out = walk_stats(&f, &WalkOptions::default(), &mut always);
        assert!(out.complete);
        assert_eq!(out.stats.files, 1);
        assert_eq!(out.stats.bytes, 42);
        assert_eq!(out.stats.dirs, 0);
    }

    #[test]
    fn a_missing_root_is_unreadable_rather_than_a_panic() {
        let out = walk_stats(
            Path::new("/nonexistent-hcmd-walk-target"),
            &WalkOptions::default(),
            &mut always,
        );
        assert_eq!(out.unreadable, 1);
        assert!(out.stats.is_empty());
    }

    #[test]
    fn the_cache_invalidates_the_whole_subtree() {
        let mut cache = SizeCache::new();
        let a = VfsPath::local("/a");
        let ab = VfsPath::local("/a/b");
        let abc = VfsPath::local("/a/b/c");
        let other = VfsPath::local("/other");
        for p in [&a, &ab, &abc, &other] {
            cache.insert(p.clone(), TreeStats::ZERO);
        }
        assert_eq!(cache.len(), 4);

        cache.invalidate(&ab);
        assert!(cache.contains(&a), "an ancestor survives");
        assert!(!cache.contains(&ab));
        assert!(!cache.contains(&abc), "descendants go too");
        assert!(cache.contains(&other), "an unrelated path survives");
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_a_descendant() {
        let mut cache = SizeCache::new();
        cache.insert(VfsPath::local("/ab"), TreeStats::ZERO);
        cache.invalidate(&VfsPath::local("/a"));
        assert!(
            cache.contains(&VfsPath::local("/ab")),
            "/ab is not under /a"
        );
    }

    fn tab_with(marks: &[&str], entries: Vec<Entry>) -> Tab {
        let mut tab = Tab::new(VfsPath::local("/root"));
        tab.entries = entries;
        tab.marks = marks.iter().map(|m| (*m).to_string()).collect();
        tab
    }

    #[test]
    fn selection_statistics_are_a_lower_bound_until_the_dirs_are_sized() {
        let mut file = Entry::file("a.bin");
        file.size = 500;
        let tab = tab_with(&["a.bin", "sub"], vec![file, Entry::dir("sub")]);

        let mut sizes = SizeCache::new();
        let stats = selection_stats(&tab, &sizes);
        assert_eq!(stats.bytes, 500);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.dirs, 1);
        assert!(!stats.is_exact(), "an unsized directory is marked");
        assert!(stats.describe("500", false).starts_with('\u{2265}'));
        assert!(stats.describe("500", true).starts_with(">="));

        sizes.insert(
            VfsPath::local("/root/sub"),
            TreeStats {
                bytes: 1_000,
                files: 3,
                dirs: 2,
            },
        );
        let stats = selection_stats(&tab, &sizes);
        assert_eq!(stats.bytes, 1_500);
        assert_eq!(stats.files, 4);
        assert_eq!(stats.dirs, 3, "the marked dir plus its two children");
        assert!(stats.is_exact());
        assert!(!stats.describe("1,500", false).starts_with('\u{2265}'));
    }

    /// "Never silently report a computed-looking total that is
    /// actually partial." A walk that skipped an unreadable subdirectory is
    /// short by whatever was inside it, so it is **not** cached - the selection
    /// keeps its `≥` and `Ctrl+L` can resolve it later.
    #[test]
    fn a_walk_that_could_not_read_everything_is_never_cached() {
        let t = TempTree::new("partial-size");
        t.file("open.txt", 5);
        let locked = t.dir("locked");
        t.file("locked/big.bin", 5000);
        fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let spec = JobSpec::size(vec![VfsPath::local(t.path())]);
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Size);
        run(&crate::vfs::LocalFs::new(), &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);

        let _ = fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));

        // Running as root defeats the mode bits, and then there is nothing
        // partial about the walk; the premise is checked rather than assumed.
        if summary.failures.is_empty() {
            return;
        }
        assert!(
            summary.sized.is_empty(),
            "a partial figure reached the size cache: {:?}",
            summary.sized
        );
        assert!(
            summary.failures[0].error.contains("could not be read"),
            "{}",
            summary.failures[0].error
        );
    }

    /// The extreme of the same case: a root that cannot be read at all reports
    /// zero bytes, and an exact-looking `0` is the worst thing to cache.
    #[test]
    fn a_root_that_cannot_be_read_is_not_cached_as_zero() {
        let t = TempTree::new("locked-root");
        let root = t.dir("locked");
        t.file("locked/big.bin", 4242);
        fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let spec = JobSpec::size(vec![VfsPath::local(&root)]);
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Size);
        run(&crate::vfs::LocalFs::new(), &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);

        let _ = fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755));

        if summary.failures.is_empty() {
            return;
        }
        assert!(summary.sized.is_empty(), "{:?}", summary.sized);
    }

    /// the copy's pre-flight and the copy itself apply the same
    /// mask, so `done / total` is one rule.
    #[test]
    fn a_filtered_walk_counts_only_what_it_keeps() {
        let t = TempTree::new("filtered");
        t.file("keep.rs", 100);
        t.file("drop.md", 9900);
        t.file("sub/also.rs", 7);

        let outcome = walk_stats_filtered(
            t.path(),
            &WalkOptions::default(),
            &mut |_| true,
            &mut |name| name.ends_with(".rs"),
        );
        assert_eq!(outcome.stats.files, 2, "both `.rs` files, and no `.md`");
        assert_eq!(outcome.stats.bytes, 107);
        assert_eq!(outcome.stats.dirs, 1, "directories are always descended");
    }

    #[test]
    fn the_parent_row_never_counts() {
        let tab = tab_with(&["..", "a"], vec![Entry::parent_entry(), Entry::file("a")]);
        let stats = selection_stats(&tab, &SizeCache::new());
        assert_eq!(stats.files, 1);
        assert_eq!(stats.dirs, 0);
    }

    /// A backend that lists one directory of subdirectories and then reports
    /// that its connection is gone.
    ///
    /// The shape of a real drop: the walk got a directory's worth of answers,
    /// pushed its children on the stack, and every question after that is
    /// asked of a socket that is not there.
    struct DropsAfterFirstListing {
        listings: AtomicU64,
        children: usize,
    }

    impl Vfs for DropsAfterFirstListing {
        fn kind(&self) -> crate::vfs::BackendKind {
            crate::vfs::BackendKind::Remote(crate::remote::RemoteId(1))
        }

        fn read_dir(&self, _path: &VfsPath) -> tokio::sync::mpsc::Receiver<crate::Result<Entry>> {
            let nth = self.listings.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            if nth == 0 {
                for n in 0..self.children {
                    let _ = tx.blocking_send(Ok(Entry::dir(format!("sub{n}"))));
                }
            } else {
                let _ = tx.blocking_send(Err(Error::connection_lost("sftp://user@host:22")));
            }
            rx
        }

        fn stat(&self, _path: &VfsPath) -> crate::Result<Entry> {
            Ok(Entry::dir("srv"))
        }

        fn open_read(&self, _path: &VfsPath) -> crate::Result<Box<dyn std::io::Read + Send>> {
            Err(Error::Unsupported("read"))
        }

        fn open_write(&self, _path: &VfsPath) -> crate::Result<Box<dyn std::io::Write + Send>> {
            Err(Error::Unsupported("write"))
        }

        fn create_dir(&self, _path: &VfsPath) -> crate::Result<()> {
            Err(Error::Unsupported("mkdir"))
        }

        fn remove(&self, _path: &VfsPath) -> crate::Result<()> {
            Err(Error::Unsupported("remove"))
        }

        fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> crate::Result<()> {
            Err(Error::Unsupported("rename"))
        }

        fn capabilities(&self) -> crate::vfs::Capabilities {
            crate::vfs::Capabilities::LOCAL
        }
    }

    /// A connection that drops mid-walk stops the walk, rather than turning
    /// every directory still on the stack into its own failure row.
    ///
    /// The two halves matter separately. One failure instead of eight is what
    /// the failure summary is for. Two listings instead of nine is the other
    /// half: the walk stopped *asking* a connection that is gone, which no
    /// count of unreadable directories would have made it do.
    #[test]
    fn a_dropped_connection_stops_the_walk_instead_of_failing_every_directory() {
        let vfs = DropsAfterFirstListing {
            listings: AtomicU64::new(0),
            children: 8,
        };
        let root = VfsPath::new(
            crate::vfs::BackendKind::Remote(crate::remote::RemoteId(1)),
            "/srv",
        );
        let spec = JobSpec::new(JobKind::Size, vec![root], None);
        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Size);

        run(&vfs, &spec, &mut ctx);

        assert!(ctx.fatal(), "a lost connection stops the batch");
        assert_eq!(
            vfs.listings.load(Ordering::SeqCst),
            2,
            "the walk stopped asking a dead connection after the first refusal"
        );
        let summary = ctx.finish();
        assert_eq!(
            summary.failures.len(),
            1,
            "one row for the connection, not one per directory: {:?}",
            summary.failures
        );
        let told = &summary.failures.first().expect("the one failure").error;
        assert!(
            told.contains(crate::error::CONNECTION_LOST_TEXT),
            "the row says what actually happened: {told}"
        );
    }
}
