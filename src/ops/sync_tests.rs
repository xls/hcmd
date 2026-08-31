//! Tests for the synchronise plan: the pure direction policy, and the tree
//! walk that builds a plan against real directories.

use super::*;
use crate::vfs::LocalFs;
use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ------------------------------------------------------------ the policy ----

/// The one rule that matters most: a two-way run only ever adds.
#[test]
fn both_mode_never_deletes() {
    let states = [
        PairState::LeftOnly,
        PairState::RightOnly,
        PairState::LeftNewer,
        PairState::RightNewer,
        PairState::Conflict,
        PairState::Same,
    ];
    for state in states {
        assert!(
            !default_action(state, SyncMode::Both).deletes(),
            "Both must not delete for {state:?}"
        );
    }
}

#[test]
fn the_defaults_read_off_the_table() {
    use PairState as P;
    use SyncAction as A;
    assert_eq!(default_action(P::LeftOnly, SyncMode::Both), A::CopyRight);
    assert_eq!(default_action(P::RightOnly, SyncMode::Both), A::CopyLeft);
    assert_eq!(default_action(P::LeftNewer, SyncMode::Both), A::CopyRight);
    assert_eq!(default_action(P::RightNewer, SyncMode::Both), A::CopyLeft);
    assert_eq!(default_action(P::Conflict, SyncMode::Both), A::Skip);
    // Mirror onto the right: what the right has and the left does not is gone.
    assert_eq!(
        default_action(P::RightOnly, SyncMode::ToRight),
        A::DeleteRight
    );
    assert_eq!(
        default_action(P::RightNewer, SyncMode::ToRight),
        A::CopyRight
    );
    assert_eq!(default_action(P::LeftOnly, SyncMode::ToRight), A::CopyRight);
    // Mirror onto the left is the mirror image.
    assert_eq!(default_action(P::LeftOnly, SyncMode::ToLeft), A::DeleteLeft);
    assert_eq!(default_action(P::LeftNewer, SyncMode::ToLeft), A::CopyLeft);
    // Same is never an action.
    assert_eq!(default_action(P::Same, SyncMode::ToRight), A::Skip);
    assert_eq!(default_action(P::Same, SyncMode::ToLeft), A::Skip);
}

#[test]
fn cycling_a_left_only_item_walks_its_choices() {
    let mut plan = plan_of(vec![item("a", PairState::LeftOnly, SyncAction::CopyRight)]);
    // CopyRight -> DeleteLeft -> Skip -> CopyRight.
    plan.cycle(0);
    assert_eq!(plan.items()[0].action, SyncAction::DeleteLeft);
    plan.cycle(0);
    assert_eq!(plan.items()[0].action, SyncAction::Skip);
    plan.cycle(0);
    assert_eq!(plan.items()[0].action, SyncAction::CopyRight);
}

#[test]
fn a_same_item_cannot_be_cycled_into_an_action() {
    let mut plan = plan_of(vec![item("a", PairState::Same, SyncAction::Skip)]);
    plan.cycle(0);
    assert_eq!(plan.items()[0].action, SyncAction::Skip);
}

#[test]
fn setting_the_mode_redefaults_every_item() {
    let mut plan = plan_of(vec![
        item("gone", PairState::RightOnly, SyncAction::CopyLeft),
        item("newer", PairState::LeftNewer, SyncAction::CopyRight),
    ]);
    plan.set_mode(SyncMode::ToRight);
    assert_eq!(plan.mode(), SyncMode::ToRight);
    assert_eq!(plan.items()[0].action, SyncAction::DeleteRight);
    assert_eq!(plan.items()[1].action, SyncAction::CopyRight);
}

#[test]
fn the_counts_tally_each_kind() {
    let plan = plan_of(vec![
        item("a", PairState::LeftOnly, SyncAction::CopyRight),
        item("b", PairState::RightOnly, SyncAction::CopyLeft),
        item("c", PairState::RightOnly, SyncAction::DeleteRight),
        item("d", PairState::Conflict, SyncAction::Skip),
    ]);
    let c = plan.counts();
    assert_eq!(c.copy_right, 1);
    assert_eq!(c.copy_left, 1);
    assert_eq!(c.delete, 1);
    assert_eq!(c.skip, 1);
}

#[test]
fn an_all_skip_plan_is_empty() {
    let plan = plan_of(vec![
        item("a", PairState::Same, SyncAction::Skip),
        item("b", PairState::Conflict, SyncAction::Skip),
    ]);
    assert!(plan.is_empty());
}

// -------------------------------------------------------------- the jobs ----

#[test]
fn copies_group_by_destination_directory() {
    // Two files into the same right-hand directory become one copy job.
    let plan = plan_of(vec![
        pathed("sub/a", PairState::LeftOnly, SyncAction::CopyRight),
        pathed("sub/b", PairState::LeftOnly, SyncAction::CopyRight),
    ]);
    let jobs = plan.into_jobs(true);
    assert_eq!(jobs.len(), 1, "one job for one destination: {jobs:?}");
    assert_eq!(jobs[0].kind, JobKind::Copy);
    assert_eq!(jobs[0].sources.len(), 2);
    assert_eq!(
        jobs[0].dest,
        Some(VfsPath::local("/right/sub")),
        "the destination is the shared parent directory"
    );
}

#[test]
fn deletes_group_by_side_and_honour_trash() {
    let plan = plan_of(vec![
        pathed("x", PairState::RightOnly, SyncAction::DeleteRight),
        pathed("y", PairState::LeftOnly, SyncAction::DeleteLeft),
    ]);
    let jobs = plan.into_jobs(false);
    let deletes: Vec<&JobSpec> = jobs
        .iter()
        .filter(|j| matches!(j.kind, JobKind::Delete { .. }))
        .collect();
    assert_eq!(deletes.len(), 2, "one delete job per side: {jobs:?}");
    for job in deletes {
        assert_eq!(job.kind, JobKind::Delete { trash: false });
        assert_eq!(job.sources.len(), 1);
        assert!(job.dest.is_none());
    }
}

#[test]
fn a_skip_produces_no_job() {
    let plan = plan_of(vec![pathed("a", PairState::Same, SyncAction::Skip)]);
    assert!(plan.into_jobs(true).is_empty());
}

// -------------------------------------------------------------- the walk ----

#[test]
fn a_file_on_one_side_only_is_a_copy_towards_the_other() {
    let left = TempTree::new("only-l");
    let right = TempTree::new("only-r");
    left.file("here.txt", 3);
    right.file("there.txt", 3);
    let plan = built(&left, &right, SyncMode::Both);
    let here = find(&plan, "here.txt");
    let there = find(&plan, "there.txt");
    assert_eq!(here.state, PairState::LeftOnly);
    assert_eq!(here.action, SyncAction::CopyRight);
    assert_eq!(there.state, PairState::RightOnly);
    assert_eq!(there.action, SyncAction::CopyLeft);
}

#[test]
fn a_differing_pair_copies_the_newer_side() {
    let left = TempTree::new("newer-l");
    let right = TempTree::new("newer-r");
    let l = left.file("f.txt", 10);
    let r = right.file("f.txt", 20);
    set_mtime(&l, 2_000);
    set_mtime(&r, 1_000);
    let plan = built(&left, &right, SyncMode::Both);
    let f = find(&plan, "f.txt");
    assert_eq!(f.state, PairState::LeftNewer);
    assert_eq!(f.action, SyncAction::CopyRight);
}

#[test]
fn identical_files_are_not_in_the_plan() {
    let left = TempTree::new("same-l");
    let right = TempTree::new("same-r");
    let l = left.file("f.txt", 8);
    let r = right.file("f.txt", 8);
    set_mtime(&l, 1_500);
    set_mtime(&r, 1_500);
    let plan = built(&left, &right, SyncMode::Both);
    assert!(plan.is_empty(), "nothing to do: {:?}", plan.items());
}

#[test]
fn a_directory_present_on_one_side_is_a_single_subtree_item() {
    let left = TempTree::new("subtree-l");
    let right = TempTree::new("subtree-r");
    left.file("only/deep/a.txt", 4);
    left.file("only/deep/b.txt", 4);
    let plan = built(&left, &right, SyncMode::Both);
    // The whole `only` directory is one item, not one per file inside it.
    assert_eq!(plan.items().len(), 1, "{:?}", plan.items());
    let only = find(&plan, "only");
    assert!(only.is_dir);
    assert_eq!(only.action, SyncAction::CopyRight);
}

#[test]
fn shared_directories_are_descended_into() {
    let left = TempTree::new("descend-l");
    let right = TempTree::new("descend-r");
    left.dir("shared");
    right.dir("shared");
    left.file("shared/only-left.txt", 5);
    let plan = built(&left, &right, SyncMode::Both);
    // The shared directory itself is not an item; the file inside it is,
    // addressed by its relative path.
    assert_eq!(plan.items().len(), 1, "{:?}", plan.items());
    let inner = find(&plan, "shared/only-left.txt");
    assert_eq!(inner.state, PairState::LeftOnly);
}

#[test]
fn equal_times_with_different_sizes_is_a_conflict() {
    let left = TempTree::new("conflict-l");
    let right = TempTree::new("conflict-r");
    let l = left.file("f.txt", 10);
    let r = right.file("f.txt", 99);
    set_mtime(&l, 3_000);
    set_mtime(&r, 3_000);
    let plan = built(&left, &right, SyncMode::Both);
    let f = find(&plan, "f.txt");
    assert_eq!(f.state, PairState::Conflict);
    assert_eq!(f.action, SyncAction::Skip, "Both leaves a conflict alone");
}

#[test]
fn a_mirror_plan_deletes_what_the_target_has_spare() {
    let left = TempTree::new("mirror-l");
    let right = TempTree::new("mirror-r");
    left.file("keep.txt", 3);
    right.file("keep.txt", 3);
    right.file("spare.txt", 3);
    let plan = built(&left, &right, SyncMode::ToRight);
    let spare = find(&plan, "spare.txt");
    assert_eq!(spare.state, PairState::RightOnly);
    assert_eq!(spare.action, SyncAction::DeleteRight);
}

// ---------------------------------------------------------------- helpers ----

fn plan_of(items: Vec<SyncItem>) -> SyncPlan {
    // The test module is a child of `sync`, so it reaches the private fields
    // directly rather than through a test-only constructor.
    SyncPlan {
        items,
        mode: SyncMode::Both,
    }
}

fn item(rel: &str, state: PairState, action: SyncAction) -> SyncItem {
    SyncItem {
        rel: rel.to_string(),
        is_dir: false,
        state,
        action,
        left: VfsPath::local(format!("/left/{rel}")),
        right: VfsPath::local(format!("/right/{rel}")),
        left_size: Some(1),
        right_size: Some(1),
    }
}

/// Like [`item`], but with real nested paths so job grouping can be checked.
fn pathed(rel: &str, state: PairState, action: SyncAction) -> SyncItem {
    item(rel, state, action)
}

fn built(left: &TempTree, right: &TempTree, mode: SyncMode) -> SyncPlan {
    // `read_dir` feeds its channel from the blocking pool, so a plan is built
    // inside `spawn_blocking` on a runtime, exactly as the event loop does it.
    let l = VfsPath::local(left.path());
    let r = VfsPath::local(right.path());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async move {
        tokio::task::spawn_blocking(move || {
            plan(
                &LocalFs::new(),
                &l,
                &r,
                PlanOptions {
                    mode,
                    slack: Duration::from_secs(1),
                    contents: false,
                },
            )
        })
        .await
        .expect("join")
    })
    .expect("plan built")
}

fn find<'a>(plan: &'a SyncPlan, rel: &str) -> &'a SyncItem {
    plan.items()
        .iter()
        .find(|i| i.rel == rel)
        .unwrap_or_else(|| panic!("no item {rel} in {:?}", plan.items()))
}

/// Set a file's mtime to `secs` seconds after the epoch, deterministically.
fn set_mtime(path: &Path, secs: u64) {
    let when = UNIX_EPOCH + Duration::from_secs(secs);
    let f = File::options()
        .write(true)
        .open(path)
        .expect("open for times");
    f.set_times(FileTimes::new().set_modified(when))
        .expect("set mtime");
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory tree, removed on drop. Built by hand rather than with
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
            "hcmd-sync-{tag}-{pid}-{nanos}-{n}",
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
        fs::write(&p, vec![b'x'; bytes]).expect("write");
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
