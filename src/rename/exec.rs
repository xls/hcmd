//! the execution: two phases, a recovery pass, and undo.
//!
//! > **Two-phase**, so swaps and cycles survive: rename everything to unique
//! > temporary names, then to the targets. `a→b` together with `b→a` must work.
//!
//! # Why two phases and not a clever ordering
//!
//! A batch of renames is a permutation, and a permutation with a cycle in it
//! has no ordering that avoids a collision: `a → b` and `b → a` collide
//! whichever goes first. Two phases dissolve every cycle at once, and the cost
//! is one extra rename per file inside the file's own directory, which is a
//! directory-entry update and nothing more.
//!
//! # What an interruption leaves
//!
//! Phase 1 moves each source to `.hcmd-rename-<pid>-<nonce>-<i>` **in its own
//! directory**, so no rename in either phase can cross a device or a backend.
//! Phase 2 moves each temporary name to its target. The **recovery pass** then
//! runs whatever happened - a failure, a cancel, an error halfway - and returns
//! every file still at a temporary name to its original name where that name is
//! free. Where it is not, which is precisely the swap case interrupted halfway,
//! the file stays at its temporary name and the result list names that path in
//! full.
//!
//! Nothing is deleted, nothing is overwritten, and nothing is left somewhere
//! the result list does not say. Every pair that does not end at its target
//! produces **exactly one** failure, which is what makes
//! [`result_lines`] able to say what happened per file from the summary alone.
//!
//! # A rename never overwrites
//!
//! Existence is checked immediately before every rename, in both phases and in
//! the recovery pass. The dialog blocks on the collisions it can prove; this
//! refuses the ones it cannot see, one `ctx.fail` per file, and the rest of the
//! batch continues.

use crate::ops::{JobContext, JobFailure, JobSpec, JobSummary};
use crate::vfs::{Vfs, VfsPath};

/// The prefix every temporary name carries, so an interrupted run leaves
/// something a human can recognise and a later run can recognise too.
pub const TEMP_PREFIX: &str = ".hcmd-rename-";

/// How many temporary names are tried before a row gives up.
///
/// The name already carries the process id and a nonce, so a clash means
/// somebody is creating files with this exact prefix while the batch runs.
/// Eight attempts is generous for that and finite for the case where the
/// directory refuses to answer.
const TEMP_ATTEMPTS: u64 = 8;

/// One source on its way to its target.
struct Staged {
    /// Which pair of the spec this is.
    index: usize,
    /// Where it started, which is where the recovery pass puts it back.
    from: VfsPath,
    /// Where it is going.
    to: VfsPath,
    /// Where it is right now.
    temp: VfsPath,
}

/// run as a job.
///
/// `spec.sources` and `spec.targets` are the pairs, positionally. Progress is
/// files only - a rename moves no bytes - so this is `ctx.start(n, 0)` and one
/// [`JobContext::add_file`] per completed rename.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    let count = spec.sources.len().min(spec.targets.len());
    ctx.start(u64::try_from(count).unwrap_or(u64::MAX), 0);

    // One slot per pair, so that exactly one failure is reported per pair that
    // did not reach its target and none is reported twice.
    let mut done: Vec<bool> = vec![false; count];
    let mut why: Vec<Option<String>> = vec![None; count];
    let nonce = nonce();

    // ------------------------------------------------------------- phase 1 --
    let mut staged: Vec<Staged> = Vec::new();
    for index in 0..count {
        let (Some(from), Some(to)) = (spec.sources.get(index), spec.targets.get(index)) else {
            continue;
        };
        if ctx.cancelled() {
            break;
        }
        if from == to {
            // A no-op reaches here only from a hand-built spec; the plan
            // filters them out. Counting it done is the honest answer: the
            // file is already where it was asked to be.
            set(&mut done, index, true);
            ctx.add_file();
            continue;
        }
        ctx.set_file(&from.to_string(), 0);
        let Some(dir) = from.parent() else {
            note(
                &mut why,
                index,
                "a rename needs a parent directory".to_string(),
            );
            continue;
        };
        let Some(temp) = free_temp(vfs, &dir, nonce, index) else {
            note(
                &mut why,
                index,
                "no free temporary name in this directory".to_string(),
            );
            continue;
        };
        match vfs.rename(from, &temp) {
            Ok(()) => staged.push(Staged {
                index,
                from: from.clone(),
                to: to.clone(),
                temp,
            }),
            Err(err) => note(&mut why, index, err.to_string()),
        }
    }

    // ------------------------------------------------------------- phase 2 --
    let mut left: Vec<Staged> = Vec::new();
    for item in staged {
        if ctx.cancelled() {
            note(&mut why, item.index, "cancelled".to_string());
            left.push(item);
            continue;
        }
        ctx.set_file(&item.to.to_string(), 0);
        if exists(vfs, &item.to) {
            note(
                &mut why,
                item.index,
                format!("{} already exists", item.to.tail().display()),
            );
            left.push(item);
            continue;
        }
        match vfs.rename(&item.temp, &item.to) {
            Ok(()) => {
                set(&mut done, item.index, true);
                ctx.add_file();
            }
            Err(err) => {
                note(&mut why, item.index, err.to_string());
                left.push(item);
            }
        }
    }

    // ------------------------------------------------------- recovery pass --
    for item in left {
        // The original name may have been taken by another row of this very
        // batch - which is the swap case, interrupted. The file stays where it
        // is and the result list says exactly where that is.
        if exists(vfs, &item.from) {
            append(&mut why, item.index, &format!("left at {}", item.temp));
            continue;
        }
        if let Err(err) = vfs.rename(&item.temp, &item.from) {
            append(
                &mut why,
                item.index,
                &format!("left at {}: {err}", item.temp),
            );
        }
    }

    // ----------------------------------------------------------- reporting --
    for index in 0..count {
        if done.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some(from) = spec.sources.get(index) else {
            continue;
        };
        let reason = why
            .get(index)
            .and_then(Clone::clone)
            .unwrap_or_else(|| "cancelled before this file was renamed".to_string());
        ctx.fail(from, reason);
    }
}

/// A temporary name in `dir` that nothing is using.
///
/// `None` when every attempt is taken, which leaves the file where it is
/// rather than renaming it onto something.
fn free_temp(vfs: &dyn Vfs, dir: &VfsPath, nonce: u64, index: usize) -> Option<VfsPath> {
    let pid = std::process::id();
    for attempt in 0..TEMP_ATTEMPTS {
        let name = format!("{TEMP_PREFIX}{pid}-{}-{index}", nonce.wrapping_add(attempt));
        let path = dir.join(&name);
        if !exists(vfs, &path) {
            return Some(path);
        }
    }
    None
}

/// Is there something at `path`?
///
/// The one question asked before every rename in this file. A backend that
/// cannot answer says so by failing the `stat`, and a failed `stat` is read as
/// "nothing there" - which is what a rename onto it will discover anyway, from
/// the kernel, atomically.
fn exists(vfs: &dyn Vfs, path: &VfsPath) -> bool {
    vfs.stat(path).is_ok()
}

/// A nonce for this batch's temporary names.
///
/// The clock, not a counter: two `hcmd` processes renaming in one directory
/// have different pids, and one process renaming twice in one directory has a
/// different nanosecond.
fn nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| {
            u64::from(d.subsec_nanos()).wrapping_add(d.as_secs().wrapping_mul(1_000_000_000))
        })
}

/// Record the first reason a pair failed. The first is the interesting one:
/// everything after it is a consequence.
fn note(why: &mut [Option<String>], index: usize, reason: String) {
    if let Some(slot) = why.get_mut(index)
        && slot.is_none()
    {
        *slot = Some(reason);
    }
}

/// Add to a pair's reason, for the recovery pass's "and here is where it is".
fn append(why: &mut [Option<String>], index: usize, extra: &str) {
    if let Some(slot) = why.get_mut(index) {
        match slot {
            Some(existing) => {
                existing.push_str("; ");
                existing.push_str(extra);
            }
            None => *slot = Some(extra.to_string()),
        }
    }
}

/// Set one flag without indexing.
fn set(flags: &mut [bool], index: usize, value: bool) {
    if let Some(slot) = flags.get_mut(index) {
        *slot = value;
    }
}

/// What one finished batch can be undone with.
///
/// Held on [`crate::rename::MultiRename`] for the session, one batch, the last.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Undo {
    /// `(what it is now, what it was)`, in the order they were renamed.
    pub pairs: Vec<(VfsPath, VfsPath)>,
}

impl Undo {
    /// The intended pairs of a plan, reversed.
    pub fn from_pairs(pairs: &[(VfsPath, VfsPath)]) -> Self {
        Self {
            pairs: pairs
                .iter()
                .map(|(from, to)| (to.clone(), from.clone()))
                .collect(),
        }
    }

    /// Drop the pairs whose rename failed, so an undo restores exactly what
    /// happened.
    ///
    /// `failed` is [`JobSummary::failures`], which name the **source** path of
    /// the rename that failed - which is the *second* half of an undo pair,
    /// because an undo pair is the original one reversed.
    pub fn prune(&mut self, failed: &[JobFailure]) {
        self.pairs
            .retain(|(_, was)| !failed.iter().any(|f| f.path == *was));
    }

    /// Nothing left to undo.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The job spec that reverses it.
    pub fn spec(&self) -> JobSpec {
        JobSpec::rename(self.pairs.clone())
    }
}

/// One line of the result list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultLine {
    /// Where the file was, in full.
    pub from: String,
    /// Where it was asked to go: the bare name when it stayed in its own
    /// directory, which is the overwhelmingly common case, and the whole path
    /// when it did not.
    pub to: String,
    /// `None` for a success.
    pub error: Option<String>,
}

/// The result list, built from the plan and the finished job's summary.
///
/// Pure; it reads nothing. Building it from the same pairs the job was given
/// is what stops what the dialog shows and what happened from drifting apart.
pub fn result_lines(pairs: &[(VfsPath, VfsPath)], summary: &JobSummary) -> Vec<ResultLine> {
    pairs
        .iter()
        .map(|(from, to)| {
            let same_dir = from.parent() == to.parent();
            ResultLine {
                from: from.to_string(),
                to: if same_dir {
                    to.file_name().unwrap_or_else(|| to.to_string())
                } else {
                    to.to_string()
                },
                error: summary
                    .failures
                    .iter()
                    .find(|f| f.path == *from)
                    .map(|f| f.error.clone()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::JobKind;
    use crate::vfs::LocalFs;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A directory that removes itself. The same shape `ops::delete`'s tests
    /// use, so a reader who knows one knows both.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-ren-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, body: &str) -> VfsPath {
            let p = self.0.join(name);
            fs::write(&p, body).expect("write");
            VfsPath::local(p)
        }

        fn at(&self, name: &str) -> VfsPath {
            VfsPath::local(self.0.join(name))
        }

        fn read(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.0.join(name)).ok()
        }

        /// Every name in the tree, sorted, so a test can assert on the whole
        /// directory rather than on the files it remembered to check.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn drive(spec: &JobSpec) -> JobSummary {
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
        run(&LocalFs::new(), spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);
        summary
    }

    #[test]
    fn a_swap_and_a_three_cycle_both_survive() {
        // and `a -> b` together with `b -> a`.
        let t = TempTree::new("swap");
        t.write("a", "A");
        t.write("b", "B");
        let spec = JobSpec::rename(vec![(t.at("a"), t.at("b")), (t.at("b"), t.at("a"))]);
        let summary = drive(&spec);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(t.read("a").as_deref(), Some("B"));
        assert_eq!(t.read("b").as_deref(), Some("A"));
        assert_eq!(t.names(), vec!["a", "b"], "no temporary name is left");

        // And a three-cycle, which no ordering of one-phase renames survives.
        let t = TempTree::new("cycle");
        t.write("x", "X");
        t.write("y", "Y");
        t.write("z", "Z");
        let spec = JobSpec::rename(vec![
            (t.at("x"), t.at("y")),
            (t.at("y"), t.at("z")),
            (t.at("z"), t.at("x")),
        ]);
        let summary = drive(&spec);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(t.read("y").as_deref(), Some("X"));
        assert_eq!(t.read("z").as_deref(), Some("Y"));
        assert_eq!(t.read("x").as_deref(), Some("Z"));
        assert_eq!(t.names(), vec!["x", "y", "z"]);
        assert_eq!(summary.files_done, 3);
    }

    #[test]
    fn no_phase_of_a_rename_ever_lands_on_an_existing_path() {
        // A target the dialog could not see - a file in the
        // directory that is not one of the rows.
        let t = TempTree::new("exists");
        t.write("a", "A");
        t.write("taken", "T");
        let spec = JobSpec::rename(vec![(t.at("a"), t.at("taken"))]);
        let summary = drive(&spec);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.files_done, 0);
        assert!(
            summary
                .failures
                .first()
                .is_some_and(|f| f.error.contains("already exists")),
            "{:?}",
            summary.failures
        );
        // Nothing was overwritten and nothing was left in a temporary name.
        assert_eq!(t.read("taken").as_deref(), Some("T"));
        assert_eq!(t.read("a").as_deref(), Some("A"));
        assert_eq!(t.names(), vec!["a", "taken"]);
    }

    #[test]
    fn one_refusal_does_not_stop_the_rest_of_the_batch() {
        // errors never abort the whole batch silently.
        let t = TempTree::new("partial");
        t.write("a", "A");
        t.write("b", "B");
        t.write("taken", "T");
        let spec = JobSpec::rename(vec![(t.at("a"), t.at("taken")), (t.at("b"), t.at("c"))]);
        let summary = drive(&spec);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.files_done, 1);
        assert_eq!(t.read("c").as_deref(), Some("B"));
        assert_eq!(t.read("a").as_deref(), Some("A"));
        assert_eq!(t.names(), vec!["a", "c", "taken"]);
    }

    #[test]
    fn a_missing_source_fails_only_its_own_row() {
        let t = TempTree::new("missing");
        t.write("b", "B");
        let spec = JobSpec::rename(vec![(t.at("gone"), t.at("x")), (t.at("b"), t.at("c"))]);
        let summary = drive(&spec);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.files_done, 1);
        assert_eq!(t.names(), vec!["c"]);
    }

    #[test]
    fn an_interrupted_rename_names_every_file_it_left_behind() {
        // Contract the second half. Cancelling between the phases is the
        // case the recovery pass exists for: the file is at a temporary name
        // and its own name may already be taken by the other half of a swap.
        let t = TempTree::new("cancel");
        t.write("a", "A");
        t.write("b", "B");
        let spec = JobSpec::rename(vec![(t.at("a"), t.at("b")), (t.at("b"), t.at("a"))]);
        let (mut ctx, rx, _dtx, flag) = JobContext::for_test(JobKind::Rename);
        // Cancelled before the job starts: phase 1 does nothing, phase 2 has
        // nothing to do, and every pair is reported as not renamed.
        flag.cancel();
        run(&LocalFs::new(), &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);
        assert!(summary.cancelled);
        assert_eq!(summary.failures.len(), 2);
        assert_eq!(t.read("a").as_deref(), Some("A"), "nothing moved");
        assert_eq!(t.read("b").as_deref(), Some("B"));
        assert_eq!(t.names(), vec!["a", "b"]);

        // And the result list says so for both, from the summary alone.
        let lines = result_lines(
            &spec
                .sources
                .iter()
                .cloned()
                .zip(spec.targets.iter().cloned())
                .collect::<Vec<_>>(),
            &summary,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.error.is_some()), "{lines:?}");
    }

    #[test]
    fn the_recovery_pass_puts_a_failed_row_back_under_its_own_name() {
        // A phase-2 failure whose original name is still free: the file goes
        // home and nothing is left at a temporary name. `wall/x` cannot exist,
        // because `wall` is a file.
        let t = TempTree::new("recover");
        t.write("a", "A");
        t.write("wall", "W");
        let spec = JobSpec::rename(vec![(t.at("a"), t.at("wall").join("x"))]);
        let summary = drive(&spec);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.files_done, 0);
        assert_eq!(
            t.read("a").as_deref(),
            Some("A"),
            "the recovery pass put it back under its own name"
        );
        assert_eq!(t.names(), vec!["a", "wall"], "no temporary name survives");
    }

    #[test]
    fn a_file_the_recovery_pass_cannot_put_back_is_named_in_full() {
        // The one case where a file stays at a temporary name: its original
        // name was taken by another row of the same batch while it was away,
        // which is the swap case interrupted halfway. `a` moves out, `b` moves
        // into `a`, and `a`'s own move fails - so `a` has nowhere to go back
        // to. Nothing is deleted, nothing is overwritten, and the result list
        // says exactly where the file is.
        let t = TempTree::new("stranded");
        t.write("a", "A");
        t.write("b", "B");
        t.write("wall", "W");
        let spec = JobSpec::rename(vec![
            (t.at("a"), t.at("wall").join("x")),
            (t.at("b"), t.at("a")),
        ]);
        let summary = drive(&spec);
        assert_eq!(summary.files_done, 1, "the second row went through");
        assert_eq!(summary.failures.len(), 1);
        let failure = summary.failures.first().expect("one failure");
        assert_eq!(failure.path, t.at("a"));
        assert!(
            failure.error.contains("left at ") && failure.error.contains(TEMP_PREFIX),
            "the result list has to name the file in full: {}",
            failure.error
        );
        assert_eq!(t.read("a").as_deref(), Some("B"), "b moved into a");

        // The stranded file is still on disk, under the name the failure named.
        let names = t.names();
        let stranded: Vec<&String> = names
            .iter()
            .filter(|n| n.starts_with(TEMP_PREFIX))
            .collect();
        assert_eq!(stranded.len(), 1, "{names:?}");
        assert!(
            stranded
                .first()
                .is_some_and(|n| failure.error.contains(n.as_str())),
            "{} does not name {stranded:?}",
            failure.error
        );
        assert_eq!(
            fs::read_to_string(t.path().join(stranded.first().map_or("", |n| n.as_str()))).ok(),
            Some("A".to_string()),
            "and it still has its contents"
        );
    }

    #[test]
    fn a_temporary_name_says_what_it_is() {
        // An interrupted run has to leave something a human recognises.
        let t = TempTree::new("tempname");
        let temp = free_temp(&LocalFs::new(), &VfsPath::local(t.path()), 7, 3)
            .expect("a free temporary name");
        let name = temp.file_name().unwrap_or_default();
        assert!(name.starts_with(TEMP_PREFIX), "{name}");
        assert!(name.ends_with("-3"), "{name}");
        assert!(name.contains(&std::process::id().to_string()), "{name}");

        // A taken name is stepped over rather than renamed onto.
        fs::write(t.path().join(&name), b"x").expect("write");
        let second = free_temp(&LocalFs::new(), &VfsPath::local(t.path()), 7, 3)
            .expect("a second free name");
        assert_ne!(second, temp);
    }

    #[test]
    fn undo_restores_the_renames_that_happened_and_only_those() {
        // One row of the batch fails; the undo must not try to
        // put that one back, because it never moved.
        let t = TempTree::new("undo");
        t.write("a", "A");
        t.write("b", "B");
        t.write("taken", "T");
        let pairs = vec![(t.at("a"), t.at("taken")), (t.at("b"), t.at("c"))];
        let mut undo = Undo::from_pairs(&pairs);
        assert_eq!(undo.pairs.len(), 2);

        let summary = drive(&JobSpec::rename(pairs.clone()));
        undo.prune(&summary.failures);
        assert_eq!(
            undo.pairs,
            vec![(t.at("c"), t.at("b"))],
            "the failed row is not undone"
        );
        assert!(!undo.is_empty());

        let back = drive(&undo.spec());
        assert!(back.is_clean(), "{:?}", back.failures);
        assert_eq!(t.read("b").as_deref(), Some("B"));
        assert_eq!(t.names(), vec!["a", "b", "taken"]);
    }

    #[test]
    fn undoing_a_swap_swaps_it_back() {
        // Acceptance criterion 10. Undo runs through the same two phases,
        // which is why it works for the same reason performing one does.
        let t = TempTree::new("undoswap");
        t.write("a", "A");
        t.write("b", "B");
        let pairs = vec![(t.at("a"), t.at("b")), (t.at("b"), t.at("a"))];
        let summary = drive(&JobSpec::rename(pairs.clone()));
        assert!(summary.is_clean());
        assert_eq!(t.read("a").as_deref(), Some("B"));

        let mut undo = Undo::from_pairs(&pairs);
        undo.prune(&summary.failures);
        let back = drive(&undo.spec());
        assert!(back.is_clean(), "{:?}", back.failures);
        assert_eq!(t.read("a").as_deref(), Some("A"));
        assert_eq!(t.read("b").as_deref(), Some("B"));
    }

    #[test]
    fn an_empty_undo_is_empty() {
        let mut undo = Undo::default();
        assert!(undo.is_empty());
        assert!(undo.spec().sources.is_empty());
        undo.prune(&[]);
        assert!(undo.is_empty());
    }

    #[test]
    fn the_result_list_says_what_happened_per_file() {
        // "shows what happened per file, including failures".
        let dir = VfsPath::local("/srv/media");
        let pairs = vec![
            (dir.join("a.txt"), dir.join("b.txt")),
            (dir.join("c.txt"), VfsPath::local("/srv/other/c.txt")),
        ];
        let summary = JobSummary {
            kind: JobKind::Rename,
            files_done: 1,
            dirs_done: 0,
            bytes_done: 0,
            skipped: 0,
            failures: vec![JobFailure {
                path: dir.join("c.txt"),
                error: "left at /srv/media/.hcmd-rename-1-2-1".to_string(),
            }],
            cancelled: false,
            elapsed: Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        };
        let lines = result_lines(&pairs, &summary);
        assert_eq!(
            lines,
            vec![
                ResultLine {
                    from: "/srv/media/a.txt".to_string(),
                    to: "b.txt".to_string(),
                    error: None,
                },
                ResultLine {
                    from: "/srv/media/c.txt".to_string(),
                    to: "/srv/other/c.txt".to_string(),
                    error: Some("left at /srv/media/.hcmd-rename-1-2-1".to_string()),
                },
            ]
        );
    }
}
