//! Directory synchronise: compare two trees and apply the difference.
//!
//! The plan is built once, off the event loop, by walking both trees in
//! lockstep by relative path. It decides nothing the copy and delete engines
//! do not already do - every action here becomes a [`crate::ops::JobKind::Copy`]
//! or a [`crate::ops::JobKind::Delete`] - so this module is the *interface* to
//! that work and not a new way of doing it.
//!
//! # The rule that makes it safe
//!
//! The whole point of a synchronise dialog is that it is a **dry run first**:
//! the plan lists every action it would take and takes none until the user
//! says so, and the applied run does exactly what the plan listed. So the plan
//! is a plain list of [`SyncItem`]s the caller can show, toggle per item, and
//! only then hand to [`SyncPlan::into_jobs`].
//!
//! [`SyncMode::Both`] never deletes. Deletion happens only in the two mirror
//! modes, where the direction is named in the mode itself, so a plan that
//! removes a file is one the user chose a direction that removes files for.
//!
//! # Where the tree is not descended
//!
//! A name that is a directory on **both** sides is not itself an action: the
//! walk descends into it and the files inside are the actions. A name that is
//! a directory on **one** side is a single subtree action - one copy or one
//! delete of the whole thing - and the walk does not descend, because the
//! other side has nothing to descend into. That is what keeps every file-level
//! action's destination directory already present: a file is only ever an
//! action where both its parent directories exist.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use crate::Result;
use crate::ops::compare::{Verdict, verdict};
use crate::ops::{JobKind, JobOptions, JobSpec};
use crate::vfs::{Entry, Vfs, VfsPath};

/// Which way the synchronise runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Copy the newer of each differing pair in whichever direction it is
    /// newer, and copy what exists on only one side to the other. **Never
    /// deletes.** The safe default: a run in this mode only ever adds.
    Both,
    /// Make the right tree identical to the left: copy left over right, and
    /// delete on the right what the left does not have.
    ToRight,
    /// Make the left tree identical to the right.
    ToLeft,
}

/// What a plan does to one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Copy the left side onto the right.
    CopyRight,
    /// Copy the right side onto the left.
    CopyLeft,
    /// Delete the item on the right.
    DeleteRight,
    /// Delete the item on the left.
    DeleteLeft,
    /// Do nothing with this item.
    Skip,
}

impl SyncAction {
    /// Whether this action removes a file. The dialog colours these and the
    /// counts total them separately, because a delete is the expensive
    /// mistake.
    pub const fn deletes(self) -> bool {
        matches!(self, Self::DeleteRight | Self::DeleteLeft)
    }
}

/// What the comparison found for one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// Present on the left, absent on the right.
    LeftOnly,
    /// Present on the right, absent on the left.
    RightOnly,
    /// On both sides and the left is the newer.
    LeftNewer,
    /// On both sides and the right is the newer.
    RightNewer,
    /// On both sides, different, and which is newer cannot be told - equal or
    /// missing mtimes, or one is a file where the other is a directory. The
    /// direction is the user's to choose; nothing here guesses it.
    Conflict,
    /// The same on both sides. Never an action.
    Same,
}

/// One line of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncItem {
    /// The path relative to the two roots, `/`-joined, for display.
    pub rel: String,
    /// A directory that exists on only one side (a whole-subtree action).
    pub is_dir: bool,
    /// What the comparison found.
    pub state: PairState,
    /// What the plan will do. The default from [`default_action`]; the user
    /// may change it with [`SyncPlan::cycle`].
    pub action: SyncAction,
    /// The item's path on the left, whether or not it exists there.
    pub left: VfsPath,
    /// The item's path on the right, whether or not it exists there.
    pub right: VfsPath,
    /// Size on the left, `None` when absent there.
    pub left_size: Option<u64>,
    /// Size on the right, `None` when absent there.
    pub right_size: Option<u64>,
}

/// The default action for a state under a mode.
///
/// Pure and total, so the whole direction policy is one function the tests
/// pin. The one rule that matters most reads straight off it: no arm of
/// [`SyncMode::Both`] returns a deleting action.
pub const fn default_action(state: PairState, mode: SyncMode) -> SyncAction {
    match (mode, state) {
        (_, PairState::Same) => SyncAction::Skip,
        (SyncMode::Both, PairState::LeftOnly | PairState::LeftNewer) => SyncAction::CopyRight,
        (SyncMode::Both, PairState::RightOnly | PairState::RightNewer) => SyncAction::CopyLeft,
        (SyncMode::Both, PairState::Conflict) => SyncAction::Skip,
        (SyncMode::ToRight, PairState::RightOnly) => SyncAction::DeleteRight,
        (SyncMode::ToRight, _) => SyncAction::CopyRight,
        (SyncMode::ToLeft, PairState::LeftOnly) => SyncAction::DeleteLeft,
        (SyncMode::ToLeft, _) => SyncAction::CopyLeft,
    }
}

/// The actions the user may cycle an item through, in order, starting from its
/// default. Every state offers `Skip`; the rest are the moves that make sense
/// for it, so a name present on only the left is never offered "copy left".
fn choices(state: PairState) -> &'static [SyncAction] {
    use SyncAction::{CopyLeft, CopyRight, DeleteLeft, DeleteRight, Skip};
    match state {
        PairState::Same => &[Skip],
        PairState::LeftOnly => &[CopyRight, DeleteLeft, Skip],
        PairState::RightOnly => &[CopyLeft, DeleteRight, Skip],
        PairState::LeftNewer => &[CopyRight, CopyLeft, Skip],
        PairState::RightNewer => &[CopyLeft, CopyRight, Skip],
        PairState::Conflict => &[Skip, CopyRight, CopyLeft],
    }
}

/// How many of each kind a plan will do, for the dialog's footer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncCounts {
    /// Copies onto the right.
    pub copy_right: usize,
    /// Copies onto the left.
    pub copy_left: usize,
    /// Deletions, either side.
    pub delete: usize,
    /// Items the plan leaves alone.
    pub skip: usize,
}

/// A built plan: the items and the mode they were defaulted under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    items: Vec<SyncItem>,
    mode: SyncMode,
}

impl SyncPlan {
    /// The items, in walk order, for the dialog to draw.
    pub fn items(&self) -> &[SyncItem] {
        &self.items
    }

    /// The mode the plan is currently defaulted under.
    pub const fn mode(&self) -> SyncMode {
        self.mode
    }

    /// Whether the plan would do nothing - every item is a `Skip`.
    pub fn is_empty(&self) -> bool {
        self.items.iter().all(|i| i.action == SyncAction::Skip)
    }

    /// Re-default every item under a new mode.
    ///
    /// A mode change resets per-item choices to that mode's defaults rather
    /// than trying to carry them across: the direction is a global promise,
    /// and a leftover override that contradicts it is the kind of surprise
    /// this feature exists to avoid.
    pub fn set_mode(&mut self, mode: SyncMode) {
        self.mode = mode;
        for item in &mut self.items {
            item.action = default_action(item.state, mode);
        }
    }

    /// Advance one item to its next allowed action, wrapping.
    pub fn cycle(&mut self, index: usize) {
        let Some(item) = self.items.get_mut(index) else {
            return;
        };
        let options = choices(item.state);
        let at = options.iter().position(|a| *a == item.action).unwrap_or(0);
        if let Some(next) = options.get((at + 1) % options.len()) {
            item.action = *next;
        }
    }

    /// The tally the footer shows.
    pub fn counts(&self) -> SyncCounts {
        let mut c = SyncCounts::default();
        for item in &self.items {
            match item.action {
                SyncAction::CopyRight => c.copy_right += 1,
                SyncAction::CopyLeft => c.copy_left += 1,
                SyncAction::DeleteRight | SyncAction::DeleteLeft => c.delete += 1,
                SyncAction::Skip => c.skip += 1,
            }
        }
        c
    }

    /// Turn the plan into the jobs that carry it out.
    ///
    /// Copies are grouped by destination directory so a directory of changed
    /// files is one job, not one per file; deletions are grouped by side.
    /// `trash` is whether a deletion goes to the trash, from the same config
    /// the delete key reads.
    pub fn into_jobs(&self, trash: bool) -> Vec<JobSpec> {
        let mut copies: BTreeMap<String, (VfsPath, Vec<VfsPath>)> = BTreeMap::new();
        let mut delete_left: Vec<VfsPath> = Vec::new();
        let mut delete_right: Vec<VfsPath> = Vec::new();
        for item in &self.items {
            match item.action {
                SyncAction::CopyRight => group_copy(&mut copies, &item.left, &item.right),
                SyncAction::CopyLeft => group_copy(&mut copies, &item.right, &item.left),
                SyncAction::DeleteRight => delete_right.push(item.right.clone()),
                SyncAction::DeleteLeft => delete_left.push(item.left.clone()),
                SyncAction::Skip => {}
            }
        }
        let mut jobs = Vec::new();
        for (_key, (dest, sources)) in copies {
            jobs.push(JobSpec::new(JobKind::Copy, sources, Some(dest)));
        }
        for sources in [delete_left, delete_right] {
            if !sources.is_empty() {
                jobs.push(deletion(sources, trash));
            }
        }
        jobs
    }
}

/// Add one copy to the group keyed by its destination directory.
fn group_copy(
    copies: &mut BTreeMap<String, (VfsPath, Vec<VfsPath>)>,
    source: &VfsPath,
    target: &VfsPath,
) {
    let Some(dest) = target.parent() else {
        return;
    };
    copies
        .entry(dest.to_string())
        .or_insert_with(|| (dest, Vec::new()))
        .1
        .push(source.clone());
}

/// A grouped deletion job.
fn deletion(sources: Vec<VfsPath>, trash: bool) -> JobSpec {
    JobSpec::new(JobKind::Delete { trash }, sources, None).with_options(JobOptions::default())
}

/// Options for building a plan, mirroring what `compare_lists` reads.
#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    /// The starting mode; the dialog can change it.
    pub mode: SyncMode,
    /// How far apart two mtimes may be and still count as the same instant.
    pub slack: Duration,
    /// Whether a pair the size and mtime cannot separate is read byte for byte
    /// before it is called the same.
    pub contents: bool,
}

/// Walk both trees and build the plan. **Does I/O**, so it runs off the event
/// loop, the same as a size walk or a contents compare.
pub fn plan(
    vfs: &dyn Vfs,
    left_root: &VfsPath,
    right_root: &VfsPath,
    options: PlanOptions,
) -> Result<SyncPlan> {
    let mut items = Vec::new();
    walk(vfs, left_root, right_root, "", &options, &mut items)?;
    Ok(SyncPlan {
        items,
        mode: options.mode,
    })
}

/// Read one directory into a name-keyed map, skipping only `.`/`..`.
///
/// Hidden files are kept: a synchronise that silently left dotfiles behind
/// would not be a mirror. The first read error aborts the plan rather than
/// producing a partial one that looks complete.
fn read_into_map(vfs: &dyn Vfs, dir: &VfsPath) -> Result<BTreeMap<String, Entry>> {
    let mut map = BTreeMap::new();
    let mut rx = vfs.read_dir(dir);
    while let Some(item) = rx.blocking_recv() {
        let entry = item?;
        if entry.is_parent {
            continue;
        }
        map.insert(entry.name.clone(), entry);
    }
    Ok(map)
}

/// Recurse one directory pair, appending items in a stable order.
fn walk(
    vfs: &dyn Vfs,
    left_dir: &VfsPath,
    right_dir: &VfsPath,
    rel: &str,
    options: &PlanOptions,
    out: &mut Vec<SyncItem>,
) -> Result<()> {
    let left = read_into_map(vfs, left_dir)?;
    let right = read_into_map(vfs, right_dir)?;
    let mut names: Vec<&String> = left.keys().chain(right.keys()).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let child_rel = join_rel(rel, name);
        classify(
            vfs,
            (left.get(name), right.get(name)),
            (left_dir.join(name), right_dir.join(name)),
            child_rel,
            options,
            out,
        )?;
    }
    Ok(())
}

/// Decide one name: descend into a shared directory, or record an item.
fn classify(
    vfs: &dyn Vfs,
    sides: (Option<&Entry>, Option<&Entry>),
    paths: (VfsPath, VfsPath),
    rel: String,
    options: &PlanOptions,
    out: &mut Vec<SyncItem>,
) -> Result<()> {
    let (left, right) = sides;
    let (left_path, right_path) = paths;
    if let (Some(l), Some(r)) = (left, right)
        && l.is_dir()
        && r.is_dir()
    {
        return walk(vfs, &left_path, &right_path, &rel, options, out);
    }
    let state = pair_state(vfs, left, right, &left_path, &right_path, options);
    out.push(SyncItem {
        rel,
        is_dir: left.or(right).is_some_and(Entry::is_dir),
        state,
        action: default_action(state, options.mode),
        left: left_path,
        right: right_path,
        left_size: left.map(|e| e.size),
        right_size: right.map(|e| e.size),
    });
    Ok(())
}

/// Classify a name that is not a directory on both sides.
fn pair_state(
    vfs: &dyn Vfs,
    left: Option<&Entry>,
    right: Option<&Entry>,
    left_path: &VfsPath,
    right_path: &VfsPath,
    options: &PlanOptions,
) -> PairState {
    match (left, right) {
        (Some(_), None) => PairState::LeftOnly,
        (None, Some(_)) => PairState::RightOnly,
        (None, None) => PairState::Same,
        (Some(l), Some(r)) => {
            let contents = options.contents && !l.is_dir() && !r.is_dir();
            let same = matches!(
                verdict_between(vfs, l, r, left_path, right_path, options.slack, contents),
                Verdict::Same | Verdict::Undecided
            );
            if same {
                PairState::Same
            } else {
                newer_of(l.mtime, r.mtime, options.slack)
            }
        }
    }
}

/// The pair's verdict, reading bytes only when `contents` asked for it and the
/// cheap steps could not separate them.
fn verdict_between(
    vfs: &dyn Vfs,
    left: &Entry,
    right: &Entry,
    left_path: &VfsPath,
    right_path: &VfsPath,
    slack: Duration,
    contents: bool,
) -> Verdict {
    let cheap = verdict(left, right, slack, false);
    if cheap != Verdict::Same || !contents {
        return cheap;
    }
    // No progress meter for a plan-time read: the walk is the slow thing the
    // user is already waiting on, and the tick only ever says "keep going".
    match crate::ops::compare::bytes_differ(vfs, left_path, right_path, &mut |_| true) {
        Ok(true) => Verdict::ContentsDiffer,
        Ok(false) => Verdict::Same,
        // A pair we cannot read is not one we can call different: leaving it
        // `Same` marks nothing, which is the honest answer for an unreadable
        // file, exactly as the contents-compare job does.
        Err(_) => Verdict::Same,
    }
}

/// Which side is newer, or `Conflict` when the mtimes cannot tell.
fn newer_of(left: Option<SystemTime>, right: Option<SystemTime>, slack: Duration) -> PairState {
    match (left, right) {
        (Some(l), Some(r)) => {
            let ahead = l.duration_since(r).unwrap_or(Duration::ZERO);
            let behind = r.duration_since(l).unwrap_or(Duration::ZERO);
            if ahead > slack {
                PairState::LeftNewer
            } else if behind > slack {
                PairState::RightNewer
            } else {
                PairState::Conflict
            }
        }
        _ => PairState::Conflict,
    }
}

/// Join a relative path with a child name using `/`, never a platform
/// separator: the string is for display and for nothing that touches disk.
fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

#[cfg(test)]
impl SyncPlan {
    /// Assemble a plan straight from items, for tests in other modules (the
    /// synchronise dialog's) that cannot reach the private fields and should
    /// not run a real tree walk to get a plan to draw.
    pub(crate) fn from_parts(items: Vec<SyncItem>, mode: SyncMode) -> Self {
        Self { items, mode }
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
