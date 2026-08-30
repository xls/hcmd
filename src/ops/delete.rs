//! Delete, to the trash and permanently.
//!
//! the design settles the split: **`F8` trashes, `Shift+F8` unlinks**. Both
//! arrive here as a [`JobKind::Delete`] whose `trash` flag says which, so the
//! progress dialog, the cancellation path and the failure summary are the same
//! code for both.
//!
//! > Directories are recursed with a single confirmation, not one per entry.
//!
//! The confirmation is the dialog's business - [`confirm_lines`] is the text it
//! should use, so the count named in the prompt and the count the runner acts
//! on cannot drift apart. What this file guarantees is the other half: one job
//! walks the whole selection and never stops to ask.
//!
//! # The three rules this file exists to keep
//!
//! 1. **Contents before their directory, always.** The unlink is an explicit
//!    post-order walk rather than one `remove_dir_all`, so a cancelled delete
//!    leaves a tree that is *smaller* and never one that lost a directory whose
//!    children are still on disk. There is no intermediate state in which an
//!    entry is unreachable but present.
//! 2. **Cancellation is checked between every entry**, so `Esc` over a
//!    half-million-file tree stops in milliseconds rather than at the end.
//! 3. **A failure never aborts the batch**. It also never
//!    cascades: a directory whose child could not be removed is skipped
//!    silently rather than reported a second time as "not empty", because the
//!    real error has already been named.
//!
//! # Nothing shells out
//!
//! "Nothing shells out". The XDG trash is reached through the
//! `trash` crate, which implements the freedesktop.org spec in
//! process - the `.Trash/files` and `.Trash/info` pair, the `trashinfo`
//! records, and the per-mount `.Trash-$uid` fallback. There is no `gio trash`
//! and no `rm`.
//!
//! # When there is no trash to move to
//!
//! A read-only mount, or one where `.Trash-$uid` cannot be created, has nowhere
//! for `F8` to put anything. The crate says so; this file surfaces that message
//! **verbatim** and marks the failure with [`NO_TRASH_HERE`] so the UI can
//! offer a permanent delete ([`permanent_delete_offer`]). What it never does is
//! quietly unlink instead - `F8` and `Shift+F8` are different keys because they
//! are different decisions, and silently promoting one to the other would make
//! the safe key the destructive one.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::walk::{TreeStats, WalkOptions, walk_stats};
use super::{JobContext, JobKind, JobSpec, JobSummary};
use crate::error::Error;
use crate::vfs::{EntryKind, Vfs, VfsPath};

/// Appended to a trash failure when the reason is that the filesystem has no
/// trash directory and none can be created.
///
/// A marker in the message text rather than a field on [`crate::ops::JobFailure`],
/// which carries a `String` and whose shape phase 1 froze. See the "needs"
/// note in this milestone's report: a `JobFailure::kind` would let the UI
/// classify a failure without matching on prose.
pub const NO_TRASH_HERE: &str = " - this filesystem has no trash directory";

/// The `Delete` runner. `trash` is `F8`; `!trash` is `Shift+F8`.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext, trash: bool) {
    debug_assert!(matches!(spec.kind, JobKind::Delete { .. }));

    // `Capabilities` is what the UI consults before offering an
    // operation, and a read-only backend is "refused up front with a clear
    // message rather than failing halfway through". Refusing here as well as in
    // the UI costs one branch and means a job built without a dialog - the
    // permanent-delete offer below builds one - cannot get past it either.
    //
    // **Of the sources**, not of the `Vfs` handle: the handle a job is given is
    // the router, whose own `capabilities` can only answer for the local
    // filesystem - so asking it let `Shift+F8` inside a `.rar` past this guard
    // and turned one up-front refusal into one failure row per marked file.
    // `capabilities_for` asks the backend that will really service the path,
    // which is the difference the design says the trait exists to express.
    if let Some(first) = spec.sources.first()
        && !vfs.capabilities_for(first).writable
    {
        ctx.start(0, 0);
        for source in &spec.sources {
            ctx.fail(
                source,
                "this backend is read-only; nothing can be deleted from it",
            );
        }
        return;
    }

    // Deleting a tree is fast and its size is not what the user is waiting on,
    // but the *count* is what makes a progress dialog mean anything, so the
    // pre-flight is a metadata walk exactly as the copy's is.
    let per_source = preflight(&spec.sources, &spec.options.walk(), ctx);
    let mut totals = TreeStats::ZERO;
    for stats in &per_source {
        totals.add(*stats);
    }
    ctx.start(totals.files, totals.bytes);

    // Everything bound for the trash goes in one operation at the end; see
    // `trash_together`.
    let mut batch: Vec<(usize, &VfsPath)> = Vec::new();

    for (index, source) in spec.sources.iter().enumerate() {
        // a dropped connection stops the batch, because every
        // remaining file would fail identically.
        //
        if ctx.cancelled() || ctx.fatal() {
            break;
        }

        // "`F8` inside an archive deletes from it". A
        // container has no trash to move a member into and no rename that
        // would put one there, so `F8` on something the kernel cannot be
        // handed is the unlink walk - which still goes through
        // [`Vfs::remove`], and so is still the backend doing the removing.
        // The confirmation the user answered says so.
        if trash && source.local_path().is_some() {
            // Collected, not trashed here: see `trash_together` below.
            batch.push((index, source));
        } else {
            unlink_tree(vfs, source, ctx);
        }
    }
    trash_together(&batch, &per_source, ctx);
}

/// Move every trashable source in one operation.
///
/// **One call, not one per file.** The platform trash is a user-facing action
/// and the desktop announces it: on macOS every `delete` is a Finder operation
/// with its own sound, so trashing twenty files played twenty sounds back to
/// back. `delete_all` is the same work announced once, which is what a person
/// asked for when they marked twenty files and pressed one key.
///
/// The cost is that a batch reports one error for the whole set rather than
/// one per path, and a failure has to name the file it happened to. So a batch
/// that fails is retried one at a time: the slow, chatty path is what runs
/// when something has already gone wrong, and it is what makes the report say
/// which file could not be trashed rather than that some file could not.
fn trash_together(batch: &[(usize, &VfsPath)], per_source: &[TreeStats], ctx: &mut JobContext) {
    trash_together_with(batch, per_source, ctx, &|paths| trash::delete_all(paths));
}

/// [`trash_together`] with the trash call itself passed in.
///
/// The split exists so a test can count the calls. "One operation, not one per
/// file" is a claim about how many times the platform is asked, and nothing
/// observable from outside distinguishes one batch of twenty from twenty
/// batches of one: both trash the files. So the seam is here and the test
/// counts through it.
fn trash_together_with(
    batch: &[(usize, &VfsPath)],
    per_source: &[TreeStats],
    ctx: &mut JobContext,
    delete_all: &dyn Fn(&[&Path]) -> std::result::Result<(), trash::Error>,
) {
    if batch.is_empty() {
        return;
    }
    let stats_of = |index: usize| per_source.get(index).copied().unwrap_or(TreeStats::ZERO);
    let paths: Vec<&Path> = batch
        .iter()
        .filter_map(|(_, source)| source.local_path())
        .collect();

    if let Some((_, first)) = batch.first() {
        let bytes = batch.iter().map(|(i, _)| stats_of(*i).bytes).sum();
        ctx.set_file(&first.to_string(), bytes);
    }

    match delete_all(&paths) {
        Ok(()) => {
            for (index, _) in batch {
                credit(stats_of(*index), ctx);
            }
        }
        // The batch said no without saying which. Ask again, singly, so the
        // failure names a file: the ones that can still go do, and the ones
        // that cannot are reported by name.
        Err(_) => {
            for (index, source) in batch {
                if ctx.cancelled() || ctx.fatal() {
                    break;
                }
                trash_one(source, stats_of(*index), ctx);
            }
        }
    }
}

/// Measure each source, so the progress dialog has a denominator and a trashed
/// tree can be credited with what it really contained.
///
/// The walk counts what is *inside* a root, so a
/// directory root adds itself here and a file root was already counted by the
/// walk as the one file it is.
fn preflight(sources: &[VfsPath], options: &WalkOptions, ctx: &mut JobContext) -> Vec<TreeStats> {
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        if ctx.cancelled() {
            out.push(TreeStats::ZERO);
            continue;
        }
        let Some(local) = source.local_path() else {
            out.push(TreeStats::ZERO);
            continue;
        };
        let mut stats = walk_stats(local, options, &mut |_| !ctx.cancelled()).stats;
        if fs::symlink_metadata(local).is_ok_and(|m| m.is_dir()) {
            stats.add_dir();
        }
        out.push(stats);
    }
    out
}

/// `F8`: move one path to the XDG trash.
///
/// The `trash` crate moves the whole tree in one operation, so there is no
/// per-file progress inside it - and no per-file cancellation either. That is
/// a property of the trash protocol (the move is a rename into
/// `~/.local/share/Trash/files` when it can be) rather than a shortcut: there
/// is no half-trashed state to leave behind, so the only honest cancellation
/// point is between sources, which [`run`] checks.
fn trash_one(source: &VfsPath, stats: TreeStats, ctx: &mut JobContext) {
    let Some(local) = source.local_path() else {
        ctx.fail(source, format!("{source} cannot be trashed until v0.5"));
        return;
    };
    ctx.set_file(&source.to_string(), stats.bytes);

    match trash::delete(local) {
        Ok(()) => credit(stats, ctx),
        Err(err) => {
            // The crate's own `Display` is "Error during a `trash` operation:
            // <Debug>", which is not a sentence to put in a status line. The
            // path is already carried by `JobFailure`, so `describe` reduces it
            // to the part that says what went wrong - without paraphrasing it.
            let mut message = describe(&err);
            if trash_unavailable(&err) {
                message.push_str(NO_TRASH_HERE);
            }
            ctx.fail(source, message);
        }
    }
}

/// Account for a whole tree that went in one operation.
fn credit(stats: TreeStats, ctx: &mut JobContext) {
    ctx.add_files(stats.files);
    for _ in 0..stats.dirs {
        ctx.add_dir();
    }
    let _ = ctx.add_bytes(stats.bytes);
}

/// One step of the post-order unlink walk.
enum Step {
    /// Look at this path: recurse into it, or remove it.
    Visit(VfsPath),
    /// Every child of this directory has been dealt with; remove the directory
    /// itself. The `u64` is the failure count at the moment the directory was
    /// visited, so a failure *underneath* it can be recognised and the
    /// inevitable "directory not empty" suppressed.
    Rmdir(VfsPath, u64),
}

/// `Shift+F8`: unlink one path, recursing into a directory contents-first.
///
/// Deliberately **not** a single [`Vfs::remove`] on the root, even though
/// `LocalFs::remove` recurses. A tree of a hundred thousand files has to report
/// progress and has to answer `Esc`, and `remove_dir_all` does neither. Each
/// leaf still goes through [`Vfs::remove`], so the backend remains the thing
/// that removes; only the order is decided here.
///
/// Symlinks are unlinked as themselves and never walked through: the metadata
/// comes from `symlink_metadata`, so a link to `/` is one entry and not a
/// catastrophe.
fn unlink_tree(vfs: &dyn Vfs, root: &VfsPath, ctx: &mut JobContext) {
    let mut stack = vec![Step::Visit(root.clone())];
    let mut failures: u64 = 0;

    while let Some(step) = stack.pop() {
        // Rule 2: between every entry, not merely between roots.
        if ctx.cancelled() || ctx.fatal() {
            return;
        }
        match step {
            Step::Visit(path) => {
                // What it is, and how big. From `lstat` for a path on this
                // machine - a symlink is unlinked as itself, so a link to `/`
                // is one entry rather than a catastrophe - and from the
                // backend for anything else (the `F8` inside an
                // archive).
                let (is_dir, size) = match path.local_path() {
                    Some(local) => match fs::symlink_metadata(local) {
                        Ok(meta) => (meta.is_dir(), meta.len()),
                        Err(err) => {
                            ctx.fail(&path, Error::io(local, err));
                            failures = failures.saturating_add(1);
                            continue;
                        }
                    },
                    None => match vfs.stat(&path) {
                        Ok(entry) => (matches!(entry.kind, EntryKind::Dir), entry.size),
                        Err(err) => {
                            ctx.fail(&path, err);
                            failures = failures.saturating_add(1);
                            continue;
                        }
                    },
                };

                if is_dir {
                    let children = match children_of(vfs, &path) {
                        Ok(children) => children,
                        Err(err) => {
                            // A directory that cannot be listed cannot be emptied, so it
                            // is not removed either - the rule that an unreadable
                            // directory reports its path and its error rather than being
                            // treated as empty.
                            ctx.fail(&path, err);
                            failures = failures.saturating_add(1);
                            continue;
                        }
                    };
                    // Rule 1: the marker goes on the stack *under* the
                    // children, so it can only be reached once every one of
                    // them has been.
                    stack.push(Step::Rmdir(path.clone(), failures));
                    for name in children {
                        stack.push(Step::Visit(path.join(name)));
                    }
                    continue;
                }

                ctx.set_file(&path.to_string(), size);
                match vfs.remove(&path) {
                    Ok(()) => {
                        ctx.add_file();
                        if !ctx.add_bytes(size) {
                            return;
                        }
                    }
                    // A backend that cancels of its own accord has already
                    // stopped; the loop's own check reports it.
                    Err(Error::Cancelled) => return,
                    Err(err) => {
                        ctx.fail(&path, err);
                        failures = failures.saturating_add(1);
                    }
                }
            }
            Step::Rmdir(path, before) => {
                if failures > before {
                    // Something below this directory failed and said so. The
                    // "directory not empty" that would follow is a consequence,
                    // not a second problem, and rule 3 keeps it out of the
                    // summary.
                    continue;
                }
                ctx.set_file(&path.to_string(), 0);
                match vfs.remove(&path) {
                    Ok(()) => ctx.add_dir(),
                    Err(Error::Cancelled) => return,
                    Err(err) => {
                        ctx.fail(&path, err);
                        failures = failures.saturating_add(1);
                    }
                }
            }
        }
    }
}

/// The names inside a directory, from whichever side of the trait can answer.
///
/// `Vfs::read_dir` streams and sends the `..` row first, because it is
/// navigation rather than content; deleting it would be deleting the parent.
fn children_of(vfs: &dyn Vfs, path: &VfsPath) -> Result<Vec<std::ffi::OsString>, Error> {
    if let Some(local) = path.local_path() {
        return read_children(local);
    }
    let mut rx = vfs.read_dir(path);
    let mut names = Vec::new();
    let mut failure = None;
    while let Some(item) = rx.blocking_recv() {
        match item {
            Ok(entry) if entry.is_parent => {}
            Ok(entry) => names.push(std::ffi::OsString::from(entry.name)),
            Err(err) => failure = Some(err),
        }
    }
    // A directory that could not be listed cannot be emptied, so it is not
    // removed either - the same rule the local side follows.
    match failure {
        Some(err) => Err(err),
        None => Ok(names),
    }
}

/// The names inside a directory, read in one pass.
///
/// Read eagerly rather than held open across the removal of the contents: an
/// open `ReadDir` over a directory being emptied is exactly the case where
/// `readdir` is allowed to skip or repeat entries.
fn read_children(dir: &Path) -> Result<Vec<std::ffi::OsString>, Error> {
    let iter = fs::read_dir(dir).map_err(|e| Error::io(dir, e))?;
    let mut names = Vec::new();
    for item in iter {
        let entry = item.map_err(|e| Error::io(dir, e))?;
        names.push(entry.file_name());
    }
    Ok(names)
}

/// Does this error mean the filesystem simply has nowhere to trash to?
///
/// Two shapes: the trash *directory* could not be created or read
/// (`FileSystem` on a path that is itself a trash folder - a read-only mount,
/// or one where `.Trash-$uid` cannot be made), and `Unknown`, which is what the
/// crate returns when there is no `HOME`/`XDG_DATA_HOME` to find the home trash
/// in and when the mount table cannot be read at all.
///
/// Everything else - the file is gone, the name is not valid text, the target
/// is the filesystem root - is a problem with the item rather than with the
/// trash, and a permanent delete would not help.
fn trash_unavailable(err: &trash::Error) -> bool {
    match err {
        // `FileSystem` is the freedesktop implementation's variant and does
        // not exist on macOS, iOS or Android, where the platform has a trash
        // of its own and no `.Trash-$uid` to fail to create. The gate is the
        // crate's own, copied verbatim so the two cannot drift apart.
        #[cfg(all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        ))]
        trash::Error::FileSystem { path, .. } => is_trash_dir(path),
        trash::Error::Unknown { .. } => true,
        _ => false,
    }
}

/// Is this path inside (or itself) a freedesktop trash directory?
///
/// `.Trash`, `.Trash-1000` and the `Trash` under `XDG_DATA_HOME` are the three
/// spellings the crate can fail on. Only the freedesktop platforms have those
/// spellings, and only they have the error variant that asks this question, so
/// this does not exist elsewhere rather than existing and never being called.
#[cfg(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "ios"),
    not(target_os = "android")
))]
fn is_trash_dir(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name == "Trash" || name.starts_with(".Trash")
    })
}

/// A human-readable line for a `trash` crate error, keeping the crate's own
/// words rather than substituting ours.
fn describe(err: &trash::Error) -> String {
    match err {
        trash::Error::Unknown { description } => description.clone(),
        trash::Error::Os { code, description } => format!("{description} (os error {code})"),
        // The path matters here and the crate's `Display` buries it in a
        // `Debug` dump: on a mount with no trash it is the `.Trash-$uid` that
        // could not be created, which is the whole explanation. Freedesktop
        // only; see `trash_unavailable`.
        #[cfg(all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        ))]
        trash::Error::FileSystem { path, source } => format!("{}: {source}", path.display()),
        trash::Error::TargetedRoot => "the filesystem root cannot be trashed".to_string(),
        trash::Error::CouldNotAccess { target } => format!("cannot access {target}"),
        trash::Error::CanonicalizePath { original } => {
            format!("cannot resolve {}", original.display())
        }
        trash::Error::ConvertOsString { .. } => {
            "the name is not valid text for the trash record".to_string()
        }
        // The restore variants cannot arise from `delete`, but the match has to
        // be total and a wildcard would swallow a future variant silently.
        other => format!("{other}"),
    }
}

/// What a finished `F8` could not trash because there was no trash to move it
/// to (and the "offer a permanent delete" rule).
///
/// The UI turns a non-empty answer into a confirmation - [`confirm_lines`]
/// with `trash: false` - and, if it is accepted, a
/// `JobKind::Delete { trash: false }` over exactly these paths. Empty for every
/// other kind of job and every other kind of failure, so calling it
/// unconditionally on a finished job is correct.
pub fn permanent_delete_offer(summary: &JobSummary) -> Vec<VfsPath> {
    if !matches!(summary.kind, JobKind::Delete { trash: true }) {
        return Vec::new();
    }
    summary
        .failures
        .iter()
        .filter(|f| f.error.ends_with(NO_TRASH_HERE))
        .map(|f| f.path.clone())
        .collect()
}

/// How a pending delete divides: what the trash will take, and what has
/// nowhere to go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashSplit {
    /// Sources on a filesystem with a usable trash.
    pub trashable: Vec<VfsPath>,
    /// Sources with nowhere to be trashed to. `F8` deletes these permanently,
    /// and the confirmation says so before anything happens.
    pub untrashable: Vec<VfsPath>,
}

impl TrashSplit {
    /// Every source, trashable first - the order the prompt describes them in.
    pub fn all(&self) -> Vec<VfsPath> {
        let mut out = self.trashable.clone();
        out.extend(self.untrashable.iter().cloned());
        out
    }

    /// True when the batch is split rather than all one way.
    pub fn is_mixed(&self) -> bool {
        !self.trashable.is_empty() && !self.untrashable.is_empty()
    }
}

/// Which of these can actually be trashed?
///
/// > Trash availability is per **filesystem**, not per machine: the
/// > freedesktop layout puts the home trash under `$XDG_DATA_HOME`, and
/// > anything on another mount needs a `.Trash-<uid>` at that mount's root. A
/// > file on a filesystem with neither has nowhere to go.
///
/// So the question is asked per filesystem and the answer is cached per
/// `st_dev` - a selection of four hundred files in one directory costs one
/// probe. Metadata only: nothing is created except, where a `.Trash-<uid>`
/// would have to be made, the same write probe the copy already uses to answer
/// "could I create a file here", which is removed immediately.
pub fn split_by_trash(sources: &[VfsPath]) -> TrashSplit {
    let mut out = TrashSplit::default();
    let mut answers: HashMap<u64, bool> = HashMap::new();
    let home = home_trash();
    for source in sources {
        // **When the answer cannot be established, the answer is "trash it".**
        // A path that cannot be stat'd tells us nothing about a trash - and
        // the design is explicit about which way to fail: "`F8` quietly
        // unlinking because a trash directory happened to be missing is the
        // single worst outcome available here." So an unknown filesystem takes
        // the recoverable route, the delete either works or fails per file, and
        // [`permanent_delete_offer`] is the second chance.
        //
        // A backend that is not the local filesystem is **not** that case, and
        // answering "trash it" for one would be the mistake the paragraph above
        // is guarding against rather than an instance of it. the design says
        // "`F8` inside an archive deletes from it": there is no rename that
        // would put a member of a container in `~/.local/share/Trash/files`,
        // and none of the freedesktop layout applies to a path inside a `.zip`.
        // So it is untrashable, and the confirmation says the delete is
        // permanent - before it happens, which is the whole point of the split.
        let usable = match source.local_path() {
            None => false,
            Some(local) => match fs::symlink_metadata(local).map(|m| m.dev()) {
                Err(_) => true,
                Ok(dev) => *answers
                    .entry(dev)
                    .or_insert_with(|| trash_reachable(local, dev, home.as_ref())),
            },
        };
        if usable {
            out.trashable.push(source.clone());
        } else {
            out.untrashable.push(source.clone());
        }
    }
    out
}

/// Where the home trash is, and which filesystem it is on.
///
/// `$XDG_DATA_HOME/Trash`, falling back to `$HOME/.local/share/Trash` exactly as
/// the freedesktop specification does. The directory itself need not exist yet -
/// the crate creates it - so the writability question is asked of the nearest
/// ancestor that does.
fn home_trash() -> Option<(PathBuf, u64)> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::paths::home_dir().ok()?.join(".local/share"),
    };
    let trash = base.join("Trash");
    let dev = existing_ancestor(&trash).and_then(|p| fs::metadata(p).map(|m| m.dev()).ok())?;
    Some((trash, dev))
}

/// Is there a trash `path` could be moved into?
fn trash_reachable(path: &Path, dev: u64, home: Option<&(PathBuf, u64)>) -> bool {
    // The home trash serves everything on its own filesystem, and only that:
    // the trash protocol moves files rather than copying them.
    if let Some((trash, home_dev)) = home
        && *home_dev == dev
    {
        return can_create_in(trash);
    }
    // Anything else needs `.Trash-<uid>` at the root of its own mount, which is
    // the topmost directory still on the same filesystem.
    let Some(top) = mount_root(path, dev) else {
        return false;
    };
    let Some(uid) = current_uid() else {
        return false;
    };
    can_create_in(&top.join(format!(".Trash-{uid}")))
}

/// The mount point `path` is on: walk up while `st_dev` does not change.
///
/// `statfs`/`getmntent` would answer it directly and both need `libc`, which
/// this milestone is not authorised to add. Walking up is what the device
/// number is for, costs one `stat` per level, and is exactly how the boundary
/// is defined.
fn mount_root(path: &Path, dev: u64) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    let mut cursor = Some(path);
    while let Some(current) = cursor {
        match fs::metadata(current) {
            Ok(meta) if meta.dev() == dev => best = Some(current.to_path_buf()),
            // A parent on another filesystem, or one that cannot be stat'd, is
            // the boundary: what we have is the root.
            _ => break,
        }
        cursor = current.parent();
    }
    best
}

/// Could this process create something at `path`?
///
/// True when it is already a writable directory, and otherwise when its nearest
/// existing ancestor will accept a file - which is what creating
/// `.Trash-<uid>`, or the home trash's own `files/` and `info/`, amounts to.
fn can_create_in(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => write_probe(path),
        // Something that is not a directory is in the way.
        Ok(_) => false,
        Err(_) => existing_ancestor(path).is_some_and(write_probe),
    }
}

/// The nearest ancestor of `path` that exists as a directory.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        if fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
            return Some(dir);
        }
        cursor = dir.parent();
    }
    None
}

/// The one thing this module writes, and it removes it again.
///
/// The mode bits are not an answer on their own - they say nothing about ACLs,
/// supplementary groups, a read-only mount or being root - and `access(2)`
/// needs `libc`. So the probe is the operation, exactly as
/// [`super::copy::probe_writable`] does it for a copy's destination.
fn write_probe(dir: &Path) -> bool {
    let probe = dir.join(format!(
        ".hcmd-trash-probe-{}-{}",
        std::process::id(),
        PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    match fs::File::create(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Keeps two trash probes in the same process off each other's name.
static PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// This process's effective uid, for the `.Trash-<uid>` name.
///
/// `getuid(2)` is not in std and `libc` is not authorised for this milestone.
/// `/proc/self` is owned by the process's own uid on Linux, which is the
/// platform this program targets, so one `stat` answers it; a system with no
/// `/proc` mounted falls back to owning a file, which is the same question
/// asked the long way round.
fn current_uid() -> Option<u32> {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return Some(meta.uid());
    }
    let probe = std::env::temp_dir().join(format!(".hcmd-uid-{}", std::process::id()));
    let file = fs::File::create(&probe).ok()?;
    let uid = file.metadata().map(|m| m.uid()).ok();
    drop(file);
    let _ = fs::remove_file(&probe);
    uid
}

/// The confirmation for a delete whose trash has already been probed.
///
///
/// Three shapes, and the spec dictates each:
///
/// * everything can be trashed - the ordinary prompt, naming the count;
/// * nothing can - *"There is no trash on this filesystem. Deleting
///   permanently."*, verbatim, because the user has to be told before it
///   happens rather than shown forty failures afterwards;
/// * some can and some cannot - "says so, names how many of each, and applies
///   one decision to the batch".
pub fn availability_lines(split: &TrashSplit) -> Vec<String> {
    if split.untrashable.is_empty() {
        return confirm_lines(&split.trashable, true);
    }
    if split.trashable.is_empty() {
        let mut lines =
            vec!["There is no trash on this filesystem. Deleting permanently.".to_string()];
        lines.extend(confirm_lines(&split.untrashable, false));
        return lines;
    }
    let trashed = split.trashable.len();
    let unlinked = split.untrashable.len();
    vec![
        format!(
            "{trashed} item{} go to the trash; {unlinked} {} no trash on {} filesystem and {} deleted permanently.",
            if trashed == 1 { "" } else { "s" },
            if unlinked == 1 { "has" } else { "have" },
            if unlinked == 1 { "its" } else { "their" },
            if unlinked == 1 { "is" } else { "are" },
        ),
        format!(
            "Delete all {} selected item(s)?",
            trashed.saturating_add(unlinked)
        ),
        "A directory is removed with everything inside it.".to_string(),
    ]
}

/// The confirmation the design requires, **naming the count**.
///
/// Lives here rather than in the dialog so the number in the prompt and the
/// number of sources the runner is handed are the same expression. The second
/// line is the other half of the same clause - "directories are recursed with a
/// single confirmation" - said out loud, because a user who marked one folder
/// is agreeing to more than one item.
pub fn confirm_lines(sources: &[VfsPath], trash: bool) -> Vec<String> {
    let verb = if trash {
        "Move to the trash"
    } else {
        "Delete permanently"
    };
    let first = match sources {
        [one] => format!(
            "{verb}: {}?",
            one.file_name().unwrap_or_else(|| one.to_string())
        ),
        _ => format!("{verb}: {} selected items?", sources.len()),
    };
    vec![
        first,
        "A directory is removed with everything inside it.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{JobEvent, JobSpec, JobSummary};
    use crate::vfs::{BackendKind, LocalFs};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-del-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
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

    fn drive(spec: JobSpec) -> JobSummary {
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
        crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);
        summary
    }

    /// Whether this machine has a working freedesktop trash at all, probed
    /// with the `trash` crate directly and never through `ops::delete`.
    ///
    /// A branch decided by the outcome of the code under test is not a skip,
    /// it is a green light: an `F8` that fails for any reason at all would
    /// take it, and every assertion about trashing sits behind it. This probe
    /// trashes and purges a file of its own, so once it has succeeded a
    /// failure from the job is a failure of the job.
    fn trash_is_available(t: &TempTree) -> bool {
        let probe = t.path().join("hcmd-trash-probe.txt");
        if fs::write(&probe, b"probe").is_err() {
            return false;
        }
        if trash::delete(&probe).is_err() {
            let _ = fs::remove_file(&probe);
            return false;
        }
        purge(&probe);
        true
    }

    /// Take one path's records out of the trash again, so a test leaves
    /// nothing behind in the user's own trash.
    ///
    /// `trash::os_limited` is the freedesktop and Windows half of the crate
    /// and does not exist on macOS, where the trash is the platform's and is
    /// not enumerable from here. So there are two bodies rather than a gate at
    /// every call site: on macOS a test's items stay in the user's trash,
    /// which is where a trashed file is supposed to be, and the tests that
    /// need to look at the trash say so themselves.
    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    ))]
    fn purge(original: &Path) {
        if let Ok(items) = trash::os_limited::list() {
            let mine: Vec<trash::TrashItem> = items
                .into_iter()
                .filter(|i| i.original_path() == original)
                .collect();
            let _ = trash::os_limited::purge_all(mine);
        }
    }

    /// See the other [`purge`]. Nothing to do where the trash cannot be read.
    #[cfg(not(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )))]
    fn purge(_original: &Path) {}

    /// Whether this platform lets a test look inside the trash it just used.
    ///
    /// Not a skip: the trashing itself is asserted everywhere. This gates only
    /// the half that reads the trash back, which macOS does not offer.
    const TRASH_IS_ENUMERABLE: bool = cfg!(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    ));

    fn unlink(sources: Vec<VfsPath>) -> JobSummary {
        drive(JobSpec::new(
            JobKind::Delete { trash: false },
            sources,
            None,
        ))
    }

    #[test]
    fn shift_f8_unlinks_a_whole_tree_with_one_job() {
        let t = TempTree::new("unlink");
        fs::create_dir_all(t.path().join("a/b")).expect("mkdir");
        fs::write(t.path().join("a/b/c.txt"), b"x").expect("write");
        let target = t.path().join("a");

        let summary = unlink(vec![VfsPath::local(&target)]);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert!(!target.exists(), "the tree is gone");
        // `a`, `a/b` and `a/b/c.txt`: every entry is counted, not just the root
        // the user marked.
        assert_eq!(summary.files_done, 1);
        assert_eq!(summary.dirs_done, 2);
        assert_eq!(summary.bytes_done, 1);
    }

    #[test]
    fn a_cancelled_recursive_delete_stops_part_way_through_the_tree() {
        // The point of the post-order walk rather than one `remove_dir_all`:
        // the deletion is made of individually cancellable steps. Cancelling
        // mid-flight must therefore leave the tree *partly* deleted - some
        // entries gone, some still there, the root still standing - which a
        // single `remove_dir_all` could never produce, because it is one
        // uninterruptible call.
        //
        // Ordering itself is not separately observable after the fact: a
        // filesystem cannot hold an entry whose parent directory is gone. What
        // is observable is that a directory is never handed to a recursive
        // remove, which is what these counts show.
        let t = TempTree::new("order");
        let root = t.path().join("big");
        for i in 0..5 {
            let inner = root.join(format!("inner{i}"));
            fs::create_dir_all(&inner).expect("mkdir");
            for j in 0..40 {
                fs::write(inner.join(format!("f{j}.txt")), b"xyz").expect("write");
            }
        }
        let untouched = t.path().join("sibling");
        fs::create_dir_all(&untouched).expect("mkdir");
        fs::write(untouched.join("keep.txt"), b"keep").expect("write");

        let (mut ctx, mut rx, _dtx, flag) = JobContext::for_test(JobKind::Delete { trash: false });
        let spec = JobSpec::new(
            JobKind::Delete { trash: false },
            vec![VfsPath::local(&root)],
            None,
        );
        let worker = std::thread::spawn(move || {
            crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
            ctx.finish()
        });

        // Drain the channel - which is also what keeps the worker from
        // blocking on it - and pull the plug once it is demonstrably under way.
        let mut seen = 0u32;
        while let Some(update) = rx.blocking_recv() {
            if matches!(update.event, JobEvent::Progress { .. }) {
                seen = seen.saturating_add(1);
                if seen == 30 {
                    flag.cancel();
                }
            }
            if matches!(update.event, JobEvent::Finished { .. }) {
                break;
            }
        }
        let summary = worker.join().expect("worker");

        assert!(summary.cancelled);
        assert!(summary.failures.is_empty(), "{:?}", summary.failures);
        assert!(root.exists(), "the root outlived the cancel");
        let left: usize = (0..5)
            .map(|i| {
                fs::read_dir(root.join(format!("inner{i}")))
                    .map(Iterator::count)
                    .unwrap_or(0)
            })
            .sum();
        assert!(summary.files_done > 0, "it got started");
        assert!(
            left > 0,
            "and it stopped: a single remove_dir_all would have taken all 200"
        );
        assert!(
            u64::try_from(left).unwrap_or(u64::MAX) + summary.files_done <= 200,
            "nothing was counted twice"
        );
        // Only the named source is walked.
        assert!(untouched.join("keep.txt").exists());
    }

    #[test]
    fn a_cancelled_delete_stops_early_and_leaves_the_rest_alone() {
        let t = TempTree::new("cancel");
        let one = t.path().join("one.txt");
        let two = t.path().join("two.txt");
        fs::write(&one, b"a").expect("write");
        fs::write(&two, b"b").expect("write");

        let spec = JobSpec::new(
            JobKind::Delete { trash: false },
            vec![VfsPath::local(&one), VfsPath::local(&two)],
            None,
        );
        let (mut ctx, rx, _dtx, flag) = JobContext::for_test(spec.kind);
        flag.cancel();
        crate::ops::run(&LocalFs::new(), &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);

        assert!(summary.cancelled);
        assert_eq!(summary.files_done, 0);
        assert!(one.exists() && two.exists(), "nothing was touched");
        assert!(summary.failures.is_empty(), "a cancel is not a failure");
    }

    #[test]
    fn a_missing_path_fails_the_item_and_not_the_batch() {
        let t = TempTree::new("missing");
        let present = t.path().join("here.txt");
        fs::write(&present, b"x").expect("write");

        let summary = unlink(vec![
            VfsPath::local(t.path().join("nope.txt")),
            VfsPath::local(&present),
        ]);
        assert_eq!(summary.failures.len(), 1);
        assert!(!present.exists(), "the second item was still deleted");
    }

    #[test]
    fn a_symlink_is_unlinked_and_its_target_survives() {
        let t = TempTree::new("symlink");
        let real = t.path().join("real.txt");
        fs::write(&real, b"keep me").expect("write");
        let link = t.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let summary = unlink(vec![VfsPath::local(&link)]);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert!(!link.exists());
        assert_eq!(fs::read(&real).expect("read"), b"keep me");
    }

    #[test]
    fn a_symlink_to_a_directory_is_not_walked_through() {
        let t = TempTree::new("dirlink");
        let precious = t.path().join("precious");
        fs::create_dir_all(&precious).expect("mkdir");
        fs::write(precious.join("keep.txt"), b"keep").expect("write");
        let tree = t.path().join("tree");
        fs::create_dir_all(&tree).expect("mkdir");
        std::os::unix::fs::symlink(&precious, tree.join("link")).expect("symlink");

        let summary = unlink(vec![VfsPath::local(&tree)]);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert!(!tree.exists());
        assert!(
            precious.join("keep.txt").exists(),
            "the link was removed, not followed"
        );
    }

    #[test]
    fn an_unremovable_file_keeps_its_directory_and_reports_once() {
        // A directory with no write permission cannot lose its children. The
        // child's failure is the real one; the parent's inevitable "not empty"
        // must not be reported as a second problem.
        use std::os::unix::fs::PermissionsExt as _;

        let t = TempTree::new("locked");
        let locked = t.path().join("locked");
        fs::create_dir_all(&locked).expect("mkdir");
        fs::write(locked.join("stuck.txt"), b"x").expect("write");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");

        let summary = unlink(vec![VfsPath::local(&locked)]);

        // Put the write bit back before any assertion can unwind, or the
        // temp-tree drop cannot clean up either.
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));

        assert_eq!(
            summary.failures.len(),
            1,
            "one failure, not one per level: {:?}",
            summary.failures
        );
        assert!(
            summary
                .failures
                .iter()
                .all(|f| f.path.to_string().ends_with("stuck.txt"))
        );
        assert_eq!(summary.dirs_done, 0, "the directory is still there");
    }

    #[test]
    fn a_read_only_backend_is_refused_up_front() {
        // `Capabilities` refuses the operation before anything is
        // attempted, rather than failing halfway through.
        let t = TempTree::new("readonly");
        let victim = t.path().join("safe.txt");
        fs::write(&victim, b"still here").expect("write");

        let list = crate::vfs::ListFs::new("results", Vec::new());
        let spec = JobSpec::new(
            JobKind::Delete { trash: false },
            vec![VfsPath::local(&victim)],
            None,
        );
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
        crate::ops::run(&list, &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);

        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0].error.contains("read-only"),
            "{:?}",
            summary.failures
        );
        assert!(victim.exists(), "nothing was attempted");
    }

    #[test]
    fn a_path_the_backend_cannot_answer_for_fails_with_the_backends_own_reason() {
        // Until v0.5 this was a milestone refusal: `Shift+F8` on anything that
        // was not a path on this machine reported "not implemented until
        // v0.5". the design brought the other half - "`F8` inside an
        // archive deletes from it" - so the walk now asks the backend, and a
        // backend that cannot answer for the path says so itself. What must
        // never happen either way is a silent success on something nothing
        // removed.
        let nested = VfsPath::local("/a/b.tar").with_segment(BackendKind::List, "/inner");
        let summary = unlink(vec![nested]);
        assert_eq!(summary.failures.len(), 1);
        assert!(
            !summary.failures[0].error.is_empty(),
            "the reason is the backend's, and there is one: {:?}",
            summary.failures
        );
        assert!(
            !summary.failures[0].error.contains("until v0.5"),
            "and it is no longer a milestone: {:?}",
            summary.failures
        );
    }

    #[test]
    fn twenty_files_are_one_call_to_the_platform_and_not_twenty() {
        // The platform announces a trash: on macOS every call is a Finder
        // operation with its own sound, so twenty files played twenty sounds
        // back to back. The fix is one call, and "one call" is not observable
        // from outside - twenty batches of one trash the files just as well -
        // so this counts through the seam instead of looking at the result.
        let t = TempTree::new("trash-batch");
        let mut victims = Vec::new();
        for i in 0..20 {
            let p = t.path().join(format!("hcmd-batch-{i}.txt"));
            fs::write(&p, b"x").expect("write");
            victims.push(VfsPath::local(&p));
        }
        let refs: Vec<(usize, &VfsPath)> = victims.iter().enumerate().collect();
        let stats = vec![TreeStats::ZERO; victims.len()];

        // Atomics rather than `Cell`: interior mutability by `Cell` is banned
        // in this crate, tests included, and a counter is what an atomic is.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let widest = std::sync::atomic::AtomicUsize::new(0);
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Delete { trash: true });
        trash_together_with(&refs, &stats, &mut ctx, &|paths| {
            calls.fetch_add(1, Ordering::Relaxed);
            widest.fetch_max(paths.len(), Ordering::Relaxed);
            Ok(())
        });
        drop(rx);

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "twenty files, one call to the platform"
        );
        assert_eq!(
            widest.load(Ordering::Relaxed),
            20,
            "and all twenty went in it"
        );
    }

    #[test]
    fn a_batch_that_fails_is_retried_singly_so_the_report_names_the_file() {
        // The cost of batching is that one error covers the whole set. That is
        // paid back by asking again one at a time, which is what makes a
        // failure say which file rather than that some file.
        let t = TempTree::new("trash-batch-fail");
        let mut victims = Vec::new();
        for i in 0..3 {
            let p = t.path().join(format!("hcmd-fail-{i}.txt"));
            fs::write(&p, b"x").expect("write");
            victims.push(VfsPath::local(&p));
        }
        let refs: Vec<(usize, &VfsPath)> = victims.iter().enumerate().collect();
        let stats = vec![TreeStats::ZERO; victims.len()];

        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Delete { trash: true });
        trash_together_with(&refs, &stats, &mut ctx, &|_| {
            Err(trash::Error::Unknown {
                description: "the batch refused".to_string(),
            })
        });
        let summary = ctx.finish();
        drop(rx);

        // The retry went through the real `trash_one`, so whatever this
        // machine's trash does, the outcome is per file and not per batch:
        // either all three were trashed singly, or each failure names its own
        // path. What must never happen is one report standing for three files.
        let named = summary.failures.len();
        assert!(
            named == 0 || named == victims.len(),
            "a failed batch is answered per file, got {named} reports for {} files: {summary:?}",
            victims.len()
        );
        for failure in &summary.failures {
            assert!(
                !failure.error.is_empty(),
                "every reported failure says why: {summary:?}"
            );
        }
    }

    #[test]
    fn f8_trashes_and_the_item_can_be_restored() {
        // The whole point of `F8` over `Shift+F8`: the file is recoverable.
        // Uses the real XDG trash, because a hand-rolled one would be testing
        // the mock. Everything it puts there is restored or purged again.
        let t = TempTree::new("trash");
        let victim = t.path().join("hcmd-trash-roundtrip.txt");
        fs::write(&victim, b"recoverable").expect("write");

        // Decided before the job runs, and not by it.
        let available = trash_is_available(&t);
        let summary = drive(JobSpec::new(
            JobKind::Delete { trash: true },
            vec![VfsPath::local(&victim)],
            None,
        ));

        if !available {
            // No trash on this filesystem, or no HOME. The contract for that
            // case is asserted instead, and it is still an assertion: the job
            // has to say so, and the file has to be untouched.
            assert!(victim.exists(), "a failed trash deletes nothing");
            let Some(first) = summary.failures.first() else {
                panic!("a trash that cannot work has to be reported: {summary:?}");
            };
            assert!(
                !first.error.is_empty(),
                "the crate's own words are surfaced"
            );
            return;
        }

        assert!(
            summary.failures.is_empty(),
            "this machine trashes, so the job had to: {:?}",
            summary.failures
        );
        assert!(!victim.exists(), "the file left its original location");
        assert_eq!(summary.files_done, 1);

        // The file is gone from where it was, which is asserted above on every
        // platform. Reading the trash back to prove it is recoverable needs
        // `trash::os_limited`, which macOS does not have: there the item is in
        // the platform's own trash and the user recovers it from there.
        if !TRASH_IS_ENUMERABLE {
            return;
        }
        restore_and_check(&victim);
    }

    /// The second half of the round trip, where the trash can be read.
    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    ))]
    fn restore_and_check(victim: &Path) {
        let items = trash::os_limited::list().expect("list the trash");
        let mine: Vec<trash::TrashItem> = items
            .into_iter()
            .filter(|i| i.original_path() == victim)
            .collect();
        assert_eq!(mine.len(), 1, "exactly one trash record for it");

        trash::os_limited::restore_all(mine).expect("restore");
        assert_eq!(
            fs::read(victim).expect("read the restored file"),
            b"recoverable",
            "restored with its contents"
        );
    }

    /// See the other [`restore_and_check`]. Never called where the trash
    /// cannot be enumerated; present so the call above compiles everywhere.
    #[cfg(not(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )))]
    fn restore_and_check(_victim: &Path) {}

    #[test]
    fn trashing_a_tree_counts_what_was_inside_it() {
        let t = TempTree::new("trashtree");
        let tree = t.path().join("hcmd-trash-tree");
        fs::create_dir_all(tree.join("inner")).expect("mkdir");
        fs::write(tree.join("inner/a.txt"), b"abc").expect("write");
        fs::write(tree.join("b.txt"), b"de").expect("write");

        // Decided before the job runs, and not by it.
        let available = trash_is_available(&t);
        let summary = drive(JobSpec::new(
            JobKind::Delete { trash: true },
            vec![VfsPath::local(&tree)],
            None,
        ));
        if !available {
            assert!(tree.exists(), "a failed trash deletes nothing");
            assert!(
                !summary.failures.is_empty(),
                "a trash that cannot work is reported"
            );
            return;
        }
        assert!(
            summary.failures.is_empty(),
            "this machine trashes, so the job had to: {:?}",
            summary.failures
        );
        assert!(!tree.exists());
        assert_eq!(summary.files_done, 2, "not just the root");
        assert_eq!(summary.dirs_done, 2, "the tree and its inner directory");
        assert_eq!(summary.bytes_done, 5);

        // Clean up: purge it rather than leaving it in the user's trash.
        purge(&tree);
    }

    /// The error this platform reports when the trash itself cannot be used,
    /// and the text a reader should find in the explanation.
    ///
    /// `trash::Error::FileSystem` exists only on the freedesktop backend, so
    /// naming it unconditionally is a compile error on macOS rather than a
    /// failing test - which is why this is a function with one body per
    /// platform rather than a value the test builds inline.
    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    ))]
    fn trash_itself_unusable() -> (trash::Error, String) {
        let path = PathBuf::from("/mnt/ro/.Trash-1000");
        let names = path.display().to_string();
        (
            trash::Error::FileSystem {
                path,
                source: std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem),
            },
            names,
        )
    }

    /// macOS and the BSDs have no `FileSystem` variant; the crate reports the
    /// same condition as `Unknown` with the reason in prose.
    #[cfg(not(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )))]
    fn trash_itself_unusable() -> (trash::Error, String) {
        let reason = "the trash directory /mnt/ro/.Trash-1000 could not be created".to_string();
        (
            trash::Error::Unknown {
                description: reason.clone(),
            },
            reason,
        )
    }

    #[test]
    fn a_missing_trash_is_surfaced_verbatim_and_offers_a_permanent_delete() {
        // The classifier and the offer, driven off the crate's own error
        // values - the condition itself (a read-only mount) cannot be created
        // inside a test without root.
        let (no_trash_dir, names) = trash_itself_unusable();
        assert!(trash_unavailable(&no_trash_dir));
        let described = describe(&no_trash_dir);
        assert!(
            described.contains(&names),
            "the trash directory it could not make is the explanation: {described}"
        );

        let no_home = trash::Error::Unknown {
            description: "Neither the XDG_DATA_HOME nor the HOME environment variable was found"
                .to_string(),
        };
        assert!(trash_unavailable(&no_home));
        assert_eq!(
            describe(&no_home),
            "Neither the XDG_DATA_HOME nor the HOME environment variable was found"
        );

        // A problem with the item, not with the trash: a permanent delete
        // would not help, so it is not offered.
        let gone = trash::Error::CouldNotAccess {
            target: "/tmp/nope".to_string(),
        };
        assert!(!trash_unavailable(&gone));
        assert!(!trash_unavailable(&trash::Error::TargetedRoot));

        // And the offer reads back off a summary.
        let summary = JobSummary {
            kind: JobKind::Delete { trash: true },
            files_done: 0,
            dirs_done: 0,
            bytes_done: 0,
            skipped: 0,
            failures: vec![
                crate::ops::JobFailure {
                    path: VfsPath::local("/mnt/ro/a.txt"),
                    error: format!("{described}{NO_TRASH_HERE}"),
                },
                crate::ops::JobFailure {
                    path: VfsPath::local("/mnt/ro/b.txt"),
                    error: describe(&gone),
                },
            ],
            cancelled: false,
            elapsed: Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        };
        let offer = permanent_delete_offer(&summary);
        assert_eq!(offer, vec![VfsPath::local("/mnt/ro/a.txt")]);

        // Never offered for a `Shift+F8`, which had no trash to fall back from.
        let unlinked = JobSummary {
            kind: JobKind::Delete { trash: false },
            ..summary
        };
        assert!(permanent_delete_offer(&unlinked).is_empty());
    }

    /// where there is no trash, the confirmation "**says so and
    /// changes its own affirmative to `Delete`**", and a mixed selection "names
    /// how many of each, and applies one decision to the batch".
    #[test]
    fn the_confirmation_says_up_front_when_there_is_no_trash() {
        let all_good = TrashSplit {
            trashable: vec![VfsPath::local("/home/t/a.txt")],
            untrashable: Vec::new(),
        };
        let lines = availability_lines(&all_good);
        assert!(lines[0].contains("Move to the trash"), "{lines:?}");

        let none = TrashSplit {
            trashable: Vec::new(),
            untrashable: vec![
                VfsPath::local("/mnt/stick/a.iso"),
                VfsPath::local("/mnt/stick/b.iso"),
            ],
        };
        let lines = availability_lines(&none);
        assert_eq!(
            lines[0], "There is no trash on this filesystem. Deleting permanently.",
            "the own words, before anything happens"
        );
        assert!(lines[1].contains('2'), "and the count: {lines:?}");

        let mixed = TrashSplit {
            trashable: vec![VfsPath::local("/home/t/a.txt")],
            untrashable: vec![VfsPath::local("/mnt/stick/b.iso")],
        };
        assert!(mixed.is_mixed());
        let lines = availability_lines(&mixed);
        assert!(lines[0].contains('1'), "how many of each: {lines:?}");
        assert!(lines[0].contains("no trash"), "{lines:?}");
        assert!(lines[0].contains("permanently"), "{lines:?}");
        assert_eq!(mixed.all().len(), 2, "one decision covers the batch");
    }

    /// The probe fails **towards the trash**, never towards the unlink.
    ///
    /// "`F8` quietly unlinking because a trash directory
    /// happened to be missing is the single worst outcome available here." A
    /// path that cannot even be stat'd says nothing about a trash, so it takes
    /// the recoverable route and any failure comes back as
    /// [`permanent_delete_offer`].
    #[test]
    fn a_path_that_cannot_be_probed_is_treated_as_trashable() {
        let split = split_by_trash(&[VfsPath::local("/nonexistent-hcmd-probe/a.txt")]);
        assert_eq!(split.untrashable.len(), 0, "{split:?}");
        assert_eq!(split.trashable.len(), 1);
    }

    /// The two filesystem questions the probe is made of, asked directly:
    /// "where does this mount begin" and "could I create the trash directory
    /// there" (the per-filesystem rule).
    #[test]
    fn the_probe_finds_the_mount_root_and_asks_whether_it_can_be_written() {
        use std::os::unix::fs::PermissionsExt as _;
        let t = TempTree::new("trash-probe");
        let deep = t.dir("a/b/c");
        let dev = fs::metadata(&deep).expect("meta").dev();

        let root = mount_root(&deep, dev).expect("some ancestor is on this filesystem");
        assert!(deep.starts_with(&root), "{root:?}");
        assert_eq!(
            fs::metadata(&root).expect("meta").dev(),
            dev,
            "and it is on the same filesystem"
        );

        // A trash directory that does not exist yet is judged by whether it
        // could be created.
        assert!(can_create_in(&deep.join(".Trash-1000")));
        assert!(can_create_in(&deep));

        // A directory nobody may write to has nowhere for a trash to go.
        let locked = t.dir("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("chmod");
        let refused = !can_create_in(&locked.join(".Trash-1000"));
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        // Running as root defeats the mode bits; the premise is checked rather
        // than assumed.
        if write_probe(&locked) {
            return;
        }
        assert!(refused, "a mode-555 mount root has no trash to offer");
    }

    #[test]
    fn the_confirmation_names_the_count() {
        // "Shift+F8 unlinks, with a confirmation naming the
        // count. Directories are recursed with a single confirmation."
        let many = vec![
            VfsPath::local("/a/one"),
            VfsPath::local("/a/two"),
            VfsPath::local("/a/three"),
        ];
        let lines = confirm_lines(&many, false);
        assert!(lines[0].contains('3'), "{lines:?}");
        assert!(lines[0].contains("permanently"), "{lines:?}");
        assert!(lines[1].contains("inside it"), "one prompt covers a tree");

        let one = confirm_lines(&[VfsPath::local("/a/report.txt")], true);
        assert!(one[0].contains("report.txt"), "{one:?}");
        assert!(one[0].contains("trash"), "{one:?}");
    }
}
