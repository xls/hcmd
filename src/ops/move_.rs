//! The move half of.
//!
//! > **Cross-device moves** degrade to copy-then-delete, with the delete
//! > happening only after a successful copy.
//!
//! A move is a rename when it can be one, and a copy followed by a delete when
//! it cannot. Everything about *how* the copy is done lives in [`super::copy`];
//! what lives here is the two decisions that are the move's own:
//!
//! 1. **Whether the cheap path is available**, decided by *trying the rename*
//!    and reading the error, not by comparing device ids up front. Comparing
//!    `st_dev` says nothing about bind mounts, overlayfs upper/lower layers,
//!    subvolumes or a destination that is a mount point in its own right; the
//!    kernel is the only authority on whether `rename(2)` can work, and asking
//!    it costs one failed syscall on the path that is about to do far more
//!    work anyway.
//! 2. **Whether the source may be deleted**, decided by [`may_delete_source`].
//!    The rule is conservative on purpose: anything under this source that
//!    failed, was skipped, or never ran keeps the whole source. Deleting the
//!    parts that happened to copy is the one outcome nobody can recover from.
//!
//! # The rename is not always the right answer
//!
//! Two things disqualify it even on one device, and both are checked by the
//! caller in [`super::copy::run`] before this module is reached:
//!
//! * a **file mask** - a rename moves the whole tree, mask or no mask, so a
//!   filtered move has to go through the copy path to filter anything;
//! * **verify** - there is nothing to re-read and compare when no bytes moved,
//!   but a user who asked for verification asked for bytes to be checked.

use std::fs;
use std::path::Path;

use super::copy::SourceOutcome;
use super::{JobContext, JobKind};
use crate::error::{Error, Result};
use crate::vfs::VfsPath;

/// `EXDEV`, "invalid cross-device link", on Linux.
///
/// [`std::io::ErrorKind::CrossesDevices`] covers this, and is what
/// [`is_cross_device`] tests first; matching the raw errno as well costs one
/// comparison and keeps the fallback working if the mapping ever changes
/// underfoot.
pub const EXDEV: i32 = 18;

/// How a rename is performed.
///
/// A function pointer rather than a call to [`fs::rename`] so a test can
/// inject `EXDEV` and exercise the copy-then-delete path on a machine with one
/// filesystem - which is every machine CI runs on. Production always passes
/// [`plain_rename`].
pub type RenameFn = fn(&Path, &Path) -> std::io::Result<()>;

/// The real rename.
pub fn plain_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

/// Is this the error that means "these two paths are not on one filesystem"?
pub fn is_cross_device(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(EXDEV)
}

/// What came of trying the cheap path.
#[derive(Debug)]
pub enum Rename {
    /// The move is done; there is nothing left to copy and nothing to delete.
    Done,
    /// `EXDEV`: degrade to copy-then-delete.
    CrossDevice,
    /// Some other refusal - a non-empty destination directory, a file in the
    /// way of a directory, a read-only mount.
    ///
    /// This also degrades to copy-then-delete, deliberately: the copy path
    /// reports the real error against the real operation instead of blaming a
    /// rename the user never asked for, and for the common case - a
    /// destination directory that exists and must be merged - copy-then-delete
    /// is not a fallback but the correct answer.
    Refused(std::io::Error),
}

impl Rename {
    /// True when the caller still has work to do.
    pub const fn needs_copy(&self) -> bool {
        !matches!(self, Self::Done)
    }
}

/// Try the cheap path.
pub fn try_rename(src: &Path, dst: &Path, rename: RenameFn) -> Rename {
    match rename(src, dst) {
        Ok(()) => Rename::Done,
        Err(err) if is_cross_device(&err) => Rename::CrossDevice,
        Err(err) => Rename::Refused(err),
    }
}

/// May a move delete this source now that its copy has finished?
///
/// the design says the delete happens "only after a successful copy". A
/// tree copy can succeed partly, so "successful" is spelled out here:
///
/// * **nothing failed** under this source - the design;
/// * **nothing was skipped** under it either. A conflict answered `Skip`, or a
///   file the mask excluded, means that byte range is *not* at the
///   destination, and `remove_any` on the source tree would take it with the
///   rest. That is the one way a move can silently destroy data, so a skip
///   keeps the whole source exactly as a failure does;
/// * **the job was not cancelled**, because a cancelled copy stopped somewhere
///   arbitrary.
///
/// The cost is a source tree left behind beside a destination that has most of
/// it. That is visible, and reversible by hand, which is the property that
/// matters.
pub const fn may_delete_source(outcome: &SourceOutcome, cancelled: bool) -> bool {
    !outcome.failed && !outcome.skipped && !outcome.stopped && !cancelled
}

/// Delete a moved source, reporting a failure against the source rather than
/// the destination.
///
/// A failure here is a real failure: the bytes are at the destination and the
/// original is still there too, which the user has to know about.
pub fn delete_source(src: &Path, source_vfs: &VfsPath, ctx: &mut JobContext) {
    if let Err(err) = super::copy::remove_any(src) {
        ctx.fail(source_vfs, err);
    }
}

/// Refuse a move whose destination backend cannot be written.
///
/// Kept beside the move because a move needs *both* halves writable - the
/// destination to receive the bytes and the source to give them up - and a
/// half-done move is worse than a refused one.
pub fn check_writable(kind: JobKind, writable: bool, dest: &VfsPath) -> Result<()> {
    if writable {
        return Ok(());
    }
    let verb = match kind {
        JobKind::Move => "move into",
        _ => "copy into",
    };
    Err(Error::msg(format!(
        "cannot {verb} {dest}: this backend is read-only, so nothing was written"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::copy::tests::{TempTree, drive, drive_answering, listing, move_spec};
    use crate::ops::{ConflictChoice, Decision, JobContext, JobSpec};
    use crate::vfs::LocalFs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    /// A rename that always reports `EXDEV`, which is how the cross-device
    /// path is reached on a machine with one filesystem.
    fn always_cross_device(_from: &Path, _to: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(EXDEV))
    }

    /// Drive a move with the rename injected.
    fn drive_move(spec: &JobSpec, rename: RenameFn) -> crate::ops::JobSummary {
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
        crate::ops::copy::run_with(&LocalFs::new(), spec, &mut ctx, rename);
        let summary = ctx.finish();
        drop(rx);
        summary
    }

    #[test]
    fn exdev_is_recognised_however_it_is_reported() {
        assert!(is_cross_device(&std::io::Error::from_raw_os_error(EXDEV)));
        assert!(is_cross_device(&std::io::Error::from(
            std::io::ErrorKind::CrossesDevices
        )));
        assert!(!is_cross_device(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn a_move_within_one_filesystem_is_a_rename() {
        let t = TempTree::new("mv-rename");
        let src = t.file("a.txt", b"payload");
        let dest = t.dir("dest");

        let spec = move_spec(vec![src.clone()], &dest);
        let summary = drive(spec);

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert!(!src.exists(), "the source is gone");
        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"payload");
    }

    #[test]
    fn try_rename_reports_which_path_it_took() {
        let t = TempTree::new("try-rename");
        let src = t.file("a.txt", b"x");
        let dst = t.path().join("b.txt");
        assert!(!try_rename(&src, &dst, plain_rename).needs_copy());
        assert!(dst.exists());

        let src2 = t.file("c.txt", b"x");
        let outcome = try_rename(&src2, &t.path().join("d.txt"), always_cross_device);
        assert!(matches!(outcome, Rename::CrossDevice));
        assert!(outcome.needs_copy());
        assert!(src2.exists(), "nothing moved");

        // Anything else also degrades, and says which it was.
        let missing = t.path().join("nope.txt");
        let outcome = try_rename(&missing, &t.path().join("e.txt"), plain_rename);
        assert!(matches!(outcome, Rename::Refused(_)));
    }

    #[test]
    fn a_cross_device_move_degrades_to_copy_then_delete() {
        // The rename is made to fail with `EXDEV`, which is the
        // only way to reach this path without two filesystems.
        let t = TempTree::new("mv-exdev");
        t.file("tree/a.txt", b"alpha");
        t.file("tree/sub/b.txt", b"beta");
        let src = t.path().join("tree");
        let dest = t.dir("dest");

        let summary = drive_move(&move_spec(vec![src.clone()], &dest), always_cross_device);

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(fs::read(dest.join("tree/a.txt")).expect("a"), b"alpha");
        assert_eq!(fs::read(dest.join("tree/sub/b.txt")).expect("b"), b"beta");
        assert!(!src.exists(), "the source went only after the copy landed");
        assert_eq!(summary.files_done, 2);
    }

    #[test]
    fn a_cross_device_move_keeps_the_source_when_part_of_it_failed() {
        // any failure anywhere under a source
        // keeps that source. Deleting the parts that copied is the one outcome
        // nobody can recover from.
        let t = TempTree::new("mv-partial");
        t.file("tree/ok.txt", b"ok");
        let locked = t.dir("tree/locked");
        t.file("tree/locked/deep.txt", b"deep");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let src = t.path().join("tree");
        let dest = t.dir("dest");

        let summary = drive_move(&move_spec(vec![src.clone()], &dest), always_cross_device);
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));

        if summary.failures.is_empty() {
            // Running as root: the permission bits mean nothing and there is
            // no failure to react to.
            return;
        }
        assert!(src.exists(), "the whole source stayed put");
        assert!(t.path().join("tree/ok.txt").exists());
    }

    #[test]
    fn a_move_keeps_its_source_when_a_conflict_was_skipped() {
        // The data-loss case this rule exists for: a skipped file is *not* at
        // the destination, so a recursive delete of the source would take the
        // only copy of it.
        let t = TempTree::new("mv-skip");
        t.file("tree/a.txt", b"NEW-A");
        t.file("tree/b.txt", b"NEW-B");
        let dest = t.dir("dest");
        t.file("dest/tree/a.txt", b"old-a");
        let src = t.path().join("tree");

        let mut spec = move_spec(vec![src.clone()], &dest);
        spec.options.conflict = Some(ConflictChoice::Skip);
        let summary = drive(spec);

        assert_eq!(summary.skipped, 1);
        assert!(
            t.path().join("tree/a.txt").exists(),
            "the skipped file still has exactly one copy, and it is this one"
        );
        assert_eq!(fs::read(dest.join("tree/a.txt")).expect("dest a"), b"old-a");
        assert_eq!(fs::read(dest.join("tree/b.txt")).expect("dest b"), b"NEW-B");
    }

    #[test]
    fn a_masked_move_never_takes_the_rename_shortcut() {
        // A rename moves the whole tree, mask or no mask, so a filtered move
        // has to go through the copy path or the mask would mean nothing.
        let t = TempTree::new("mv-mask");
        t.file("tree/keep.rs", b"rs");
        t.file("tree/drop.md", b"md");
        let dest = t.dir("dest");

        let mut spec = move_spec(vec![t.path().join("tree")], &dest);
        spec.options.file_mask = "*.rs".to_string();
        let summary = drive(spec);

        assert_eq!(listing(&dest.join("tree")), vec!["keep.rs"]);
        assert!(
            t.path().join("tree/drop.md").exists(),
            "the excluded file was neither copied nor deleted"
        );
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn a_cancelled_move_never_deletes_its_source() {
        let t = TempTree::new("mv-cancel");
        let src = t.file("a.txt", b"payload");
        let dest = t.dir("dest");

        let spec = move_spec(vec![src.clone()], &dest);
        let (mut ctx, rx, _dtx, flag) = JobContext::for_test(spec.kind);
        flag.cancel();
        crate::ops::copy::run_with(&LocalFs::new(), &spec, &mut ctx, always_cross_device);
        let summary = ctx.finish();
        drop(rx);

        assert!(summary.cancelled);
        assert!(src.exists(), "the source is untouched");
        assert!(listing(&dest).is_empty());
    }

    #[test]
    fn cancelling_a_conflict_during_a_move_deletes_nothing() {
        let t = TempTree::new("mv-cancel-conflict");
        let src = t.file("a.txt", b"NEW");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");

        let summary = drive_answering(move_spec(vec![src.clone()], &dest), vec![Decision::Cancel]);

        assert!(summary.cancelled);
        assert!(src.exists());
        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"old");
    }

    #[test]
    fn the_delete_rule_is_conservative_in_every_direction() {
        let clean = SourceOutcome::default();
        assert!(clean.complete());
        assert!(may_delete_source(&clean, false));
        assert!(!may_delete_source(&clean, true), "cancelled");

        for outcome in [
            SourceOutcome {
                failed: true,
                ..SourceOutcome::default()
            },
            SourceOutcome {
                skipped: true,
                ..SourceOutcome::default()
            },
            SourceOutcome {
                stopped: true,
                ..SourceOutcome::default()
            },
        ] {
            assert!(!outcome.complete());
            assert!(!may_delete_source(&outcome, false), "{outcome:?}");
        }
    }

    #[test]
    fn a_read_only_destination_refuses_a_move_by_name() {
        let dest = VfsPath::local("/srv/archive.zip");
        let err = check_writable(JobKind::Move, false, &dest).expect_err("refused");
        assert!(err.to_string().contains("move into"), "{err}");
        assert!(err.to_string().contains("read-only"), "{err}");

        let err = check_writable(JobKind::Copy, false, &dest).expect_err("refused");
        assert!(err.to_string().contains("copy into"), "{err}");

        assert!(check_writable(JobKind::Move, true, &dest).is_ok());
    }

    #[test]
    fn a_move_onto_itself_is_refused_rather_than_deleting_the_file() {
        let t = TempTree::new("mv-self");
        let src = t.file("a.txt", b"payload");
        let dest: PathBuf = t.path().to_path_buf();

        let summary = drive(move_spec(vec![src.clone()], &dest));

        assert_eq!(summary.failures.len(), 1);
        assert!(summary.failures[0].error.contains("same file"));
        assert_eq!(fs::read(&src).expect("still there"), b"payload");
    }
}
