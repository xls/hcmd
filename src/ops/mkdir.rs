//! `F7` - create a directory (the `+ F7` button).
//!
//! The smallest job there is, and the reason it is a job at all rather than a
//! direct call: `F7` on a slow NFS mount blocks, and the rule that
//! "a slow mount must not freeze the UI" is not weaker for a `mkdir` than for a
//! listing. It also means the one progress-and-failure path serves every
//! operation instead of `F7` inventing its own error reporting.
//!
//! Intermediate components are created, so typing `photos/2026/summer` into the
//! prompt makes all three. That is what Total Commander does and what the
//! `+ F7` button inside the copy dialog needs, since the target it is creating
//! may be several levels below anything that exists.
//!
//! # Where the cursor ends up
//!
//! the design gives `F7` no explicit clause about the cursor, but a create
//! that leaves the cursor at the top of the panel is a create the user then has
//! to go and find. v0.1 already solved this shape of problem for "go to parent" -
//! [`crate::panel::Tab::pending_select`], resolved by
//! [`crate::panel::Tab::resolve_pending_select`] as listing batches arrive -
//! and this file reuses it rather than inventing a second mechanism.
//! [`cursor_name`] is the piece that belongs here: given the panel's directory
//! and the path being created, it names the row that will *appear in that
//! panel*. For `photos/2026/summer` typed at `/home/t` that is `photos`, not
//! `summer`, because `summer` is three levels down and never becomes a row.
//!
//! # Refusals
//!
//! Every failure mode is a message, never a panic: a read-only backend
//! (the `Capabilities`), a backend with no directories at all, an
//! empty name ([`validate_name`], used by the prompt before a job is ever
//! built), a name already taken by a *file*, a permission error, and a
//! read-only filesystem. A name already taken by a *directory* is deliberately
//! not a failure: `+ F7` in the copy dialog wants create-if-missing, and so
//! does creating `a/b/c` twice.

use super::{JobContext, JobKind, JobSpec};
use crate::error::Error;
use crate::vfs::{Vfs, VfsPath};

/// Why an empty name is refused, phrased for the prompt.
pub const EMPTY_NAME: &str = "a directory name cannot be empty";

/// Is this something the `F7` prompt may accept?
///
/// Checked in the dialog, before a [`JobSpec`] exists, because by the time a
/// name has been joined onto the panel's path an empty one is
/// indistinguishable from "the panel's own directory" and the runner can only
/// report the confusing version. [`crate::dialog::InputDialog`] already refuses
/// an empty field; this is the same rule stated where a caller that is not a
/// dialog can also reach it.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err(EMPTY_NAME);
    }
    // A NUL cannot survive the conversion to a C string and the syscall would
    // reject it with a message about the whole path rather than the name.
    if name.contains('\0') {
        return Err("a directory name cannot contain a null byte");
    }
    Ok(())
}

/// The name of the row that will appear in `base` once `dest` is created, for
/// [`crate::panel::Tab::pending_select`].
///
/// `None` when `dest` is `base` itself - nothing new appears, so there is
/// nothing to select - and the leaf's own name when `dest` is somewhere else
/// entirely, which is the best a panel showing an unrelated directory can do
/// with it.
pub fn cursor_name(base: &VfsPath, dest: &VfsPath) -> Option<String> {
    if dest == base {
        return None;
    }
    if !dest.starts_with(base) {
        return dest.file_name();
    }
    let mut current = dest.clone();
    loop {
        let parent = current.parent()?;
        if &parent == base {
            return current.file_name();
        }
        if !parent.starts_with(base) {
            // `dest` is under `base` but the chain up from it left `base`
            // without landing on it - a nested segment boundary. The leaf is
            // the only honest answer.
            return dest.file_name();
        }
        current = parent;
    }
}

/// The `Mkdir` runner. The directory to create is [`JobSpec::dest`].
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    debug_assert_eq!(spec.kind, JobKind::Mkdir);
    ctx.start(0, 0);

    let Some(dest) = spec.dest.as_ref() else {
        ctx.fail(&VfsPath::local_root(), "no directory name was given");
        return;
    };

    // refused up front, with a clear message, rather than halfway
    // through. `has_directories` is the object-store case,
    // where "directories" are prefixes and creating an empty one means nothing.
    //
    // Of the **destination**, not of the `Vfs` handle: the handle is the
    // router, and a router's own `capabilities` can only answer for the local
    // filesystem. `F7` inside a `.rar` asked the wrong object and
    // got `writable: true`.
    let caps = vfs.capabilities_for(dest);
    if !caps.writable {
        ctx.fail(
            dest,
            "this backend is read-only; no directory can be created in it",
        );
        return;
    }
    if !caps.has_directories {
        ctx.fail(dest, "this backend has no directories to create");
        return;
    }

    if dest.file_name().is_none() {
        // The root of a backend. There is no name here to create.
        ctx.fail(dest, EMPTY_NAME);
        return;
    }
    ctx.set_file(&dest.to_string(), 0);

    // A destination that is already there is only acceptable when it is
    // already a directory. `+ F7` in the copy dialog wants create-if-missing,
    // but `F7` over the name of an existing *file* has to say so - the chain
    // below would otherwise be empty and the job would report a success that
    // created nothing.
    match vfs.stat(dest) {
        Ok(entry) if entry.is_dir() => return,
        Ok(_) => {
            ctx.fail(dest, "a file of that name is already here");
            return;
        }
        Err(_) => {}
    }

    for path in missing_ancestors(vfs, dest) {
        if ctx.cancelled() {
            return;
        }
        ctx.set_file(&path.to_string(), 0);
        match vfs.create_dir(&path) {
            Ok(()) => ctx.add_dir(),
            // Something else created it between the check and the call. That
            // is a race, not a failure: the directory the user asked for
            // exists.
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            }
            Err(err) => {
                ctx.fail(&path, err);
                return;
            }
        }
    }
}

/// The chain of directories that have to be created, outermost first.
///
/// Built by walking up until something exists, so nothing that is already
/// there is touched and a failure names the level that actually failed rather
/// than the leaf.
fn missing_ancestors(vfs: &dyn Vfs, dest: &VfsPath) -> Vec<VfsPath> {
    let mut chain = Vec::new();
    let mut current = Some(dest.clone());
    while let Some(path) = current {
        if vfs.stat(&path).is_ok() {
            break;
        }
        current = path.parent();
        chain.push(path);
    }
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{JobSpec, JobSummary};
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
                "hcmd-mkdir-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn drive(spec: JobSpec) -> JobSummary {
        drive_on(&LocalFs::new(), spec)
    }

    fn drive_on(vfs: &dyn Vfs, spec: JobSpec) -> JobSummary {
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(spec.kind);
        crate::ops::run(vfs, &spec, &mut ctx);
        let summary = ctx.finish();
        drop(rx);
        summary
    }

    fn mkdir(dest: impl Into<PathBuf>) -> JobSummary {
        drive(JobSpec::new(
            JobKind::Mkdir,
            Vec::new(),
            Some(VfsPath::local(dest.into())),
        ))
    }

    #[test]
    fn a_nested_name_creates_every_level() {
        let t = TempTree::new("nested");
        let target = t.path().join("photos/2026/summer");
        let summary = mkdir(&target);
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert!(target.is_dir());
        assert_eq!(summary.dirs_done, 3);
    }

    #[test]
    fn intervening_components_are_created_outermost_first() {
        // Not merely "all three exist at the end": each level is created by its
        // own call, so a failure can name the level that failed. Checked by
        // making the middle level impossible and asserting the outer one was
        // still made and the inner one was not.
        use std::os::unix::fs::PermissionsExt as _;

        let t = TempTree::new("order");
        let outer = t.path().join("outer");
        fs::create_dir(&outer).expect("mkdir");
        fs::set_permissions(&outer, fs::Permissions::from_mode(0o500)).expect("chmod");

        let summary = mkdir(outer.join("middle/inner"));
        let _ = fs::set_permissions(&outer, fs::Permissions::from_mode(0o700));

        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0]
                .path
                .to_string()
                .ends_with("outer/middle"),
            "the level that actually failed: {:?}",
            summary.failures[0]
        );
        assert_eq!(summary.dirs_done, 0);
        assert!(!outer.join("middle").exists());
    }

    #[test]
    fn an_existing_directory_is_not_an_error() {
        let t = TempTree::new("exists");
        let summary = mkdir(t.path());
        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(summary.dirs_done, 0, "nothing needed creating");
    }

    #[test]
    fn a_name_already_taken_by_a_file_is_refused() {
        let t = TempTree::new("taken");
        let blocker = t.path().join("blocker");
        fs::write(&blocker, b"not a directory").expect("write");

        let summary = mkdir(&blocker);
        assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
        assert!(
            summary.failures[0].error.contains("already here"),
            "{:?}",
            summary.failures[0]
        );
        assert_eq!(summary.dirs_done, 0);
        assert_eq!(
            fs::read(&blocker).expect("read"),
            b"not a directory",
            "the file is untouched"
        );
    }

    #[test]
    fn a_name_that_collides_with_a_file_reports_which_level_failed() {
        let t = TempTree::new("collide");
        let blocker = t.path().join("blocker");
        fs::write(&blocker, b"not a directory").expect("write");

        let summary = mkdir(blocker.join("child"));
        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0]
                .path
                .to_string()
                .ends_with("blocker/child"),
            "the failure names the level that failed: {:?}",
            summary.failures[0]
        );
        assert_eq!(summary.dirs_done, 0, "nothing was created");
    }

    #[test]
    fn a_permission_error_is_reported_rather_than_panicking() {
        use std::os::unix::fs::PermissionsExt as _;

        let t = TempTree::new("perm");
        let locked = t.path().join("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");

        let summary = mkdir(locked.join("nope"));
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));

        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0]
                .error
                .to_lowercase()
                .contains("permission"),
            "{:?}",
            summary.failures[0]
        );
    }

    #[test]
    fn a_read_only_backend_is_refused_up_front() {
        // `Capabilities` refuses before anything is attempted.
        let list = crate::vfs::ListFs::new("results", Vec::new());
        let summary = drive_on(
            &list,
            JobSpec::new(
                JobKind::Mkdir,
                Vec::new(),
                Some(VfsPath::local("/tmp/hcmd-never-created")),
            ),
        );
        assert_eq!(summary.failures.len(), 1);
        assert!(
            summary.failures[0].error.contains("read-only"),
            "{:?}",
            summary.failures
        );
        assert!(!Path::new("/tmp/hcmd-never-created").exists());
    }

    #[test]
    fn a_job_with_no_destination_says_so() {
        let summary = drive(JobSpec::new(JobKind::Mkdir, Vec::new(), None));
        assert_eq!(summary.failures.len(), 1);
        assert!(summary.failures[0].error.contains("no directory name"));
    }

    #[test]
    fn the_root_of_a_backend_is_not_a_name() {
        let summary = mkdir("/");
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].error, EMPTY_NAME);
    }

    #[test]
    fn an_empty_name_is_refused_before_a_job_is_built() {
        assert_eq!(validate_name(""), Err(EMPTY_NAME));
        assert_eq!(validate_name("   "), Err(EMPTY_NAME));
        assert_eq!(validate_name("\t\n"), Err(EMPTY_NAME));
        assert!(validate_name("photos").is_ok());
        assert!(validate_name("photos/2026").is_ok());
        assert!(validate_name("a\0b").is_err());
    }

    #[test]
    fn the_cursor_lands_on_the_row_that_appears_in_this_panel() {
        // the design plus `Tab::pending_select`: the panel is showing `base`,
        // so the row that appears is the *outermost* new component.
        let base = VfsPath::local("/home/t");
        assert_eq!(
            cursor_name(&base, &base.join("photos")).as_deref(),
            Some("photos")
        );
        assert_eq!(
            cursor_name(&base, &base.join("photos/2026/summer")).as_deref(),
            Some("photos"),
            "`summer` never becomes a row in this panel"
        );
        // The panel's own directory: nothing new appears, and the caller has
        // nothing to select.
        assert_eq!(cursor_name(&base, &base), None);
        // Somewhere else entirely - an absolute path typed into the prompt.
        assert_eq!(
            cursor_name(&base, &VfsPath::local("/srv/media/new")).as_deref(),
            Some("new")
        );
        // Component-wise, so a sibling with a shared prefix is not "inside".
        assert_eq!(
            cursor_name(
                &VfsPath::local("/home/t"),
                &VfsPath::local("/home/toolbox/x")
            )
            .as_deref(),
            Some("x")
        );
        assert_eq!(
            cursor_name(&VfsPath::local("/"), &VfsPath::local("/a/b")).as_deref(),
            Some("a")
        );
        assert_eq!(
            cursor_name(&VfsPath::local_root(), &VfsPath::local_root()),
            None
        );
    }

    #[test]
    fn a_nested_backend_path_is_refused_with_its_milestone() {
        let t = TempTree::new("nestedbackend");
        let nested = VfsPath::local(t.path()).with_segment(BackendKind::List, "/inner/new");
        let summary = drive(JobSpec::new(JobKind::Mkdir, Vec::new(), Some(nested)));
        assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
        assert!(
            summary.failures[0]
                .error
                .contains("not on the local filesystem"),
            "{:?}",
            summary.failures[0]
        );
        assert_eq!(summary.dirs_done, 0);
    }
}
