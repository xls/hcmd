//! The conflict policy.
//!
//! > **Conflict policy** on an existing destination: overwrite / skip / rename
//! > / append, each with an "all" variant, plus "overwrite if newer" and
//! > "overwrite if different size". Decisions apply for the remainder of the
//! > batch.
//!
//! One [`Policy`] lives for the length of one job. It is asked about every
//! destination that already exists, it asks the UI through
//! [`JobContext::ask`] when it has no standing answer, and an answer carrying
//! [`Decision::apply_to_all`] installs itself so nothing is asked twice.
//!
//!
//! # Why the "all" variants are not six more enum arms
//!
//! The *choice* and its *scope* are orthogonal, so [`ConflictChoice`] has six
//! arms and `Decision::Conflict::apply_to_all` carries the scope. An "all"
//! rename cannot use a typed name - one name cannot serve a batch - so it
//! generates free ones with [`free_name`].
//!
//! # Why two directories never raise a conflict
//!
//! Asking "overwrite?" about a directory that is only going to gain children
//! is a question with no useful answer, and Total Commander does not ask it
//! either. Only a file, or a type mismatch, reaches the UI.

use std::fs;
use std::path::{Path, PathBuf};

use super::{ConflictChoice, ConflictRequest, Decision, JobContext};
use crate::error::{Error, Result};
use crate::vfs::VfsPath;

/// What to do with one destination once the conflict, if any, is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Write here, replacing whatever is there. The path may differ from the
    /// one asked about, which is what [`ConflictChoice::Rename`] means.
    Write(PathBuf),
    /// Remove what is in the way, then write here.
    ///
    /// Only ever for a **non-directory** destination whose type does not match
    /// the source's - a directory being copied over a regular file, say, where
    /// `mkdir` would simply fail with `EEXIST`. A directory destination is
    /// never removed on the strength of a conflict answer that described a
    /// file; see [`Plan::Refuse`].
    Replace(PathBuf),
    /// Append to what is already here.
    ///
    /// The one plan that is not atomic from the destination's point of view:
    /// appending *is* mutating in place. The conflict dialog says so.
    Append(PathBuf),
    /// Leave the destination alone and count the source as skipped.
    Skip,
    /// This one destination cannot be written, and here is why.
    ///
    /// A per-item failure rather than a silent skip, because the user asked for
    /// something that cannot happen and the design collects failures rather
    /// than hiding them.
    Refuse(String),
    /// The user cancelled; unwind the whole batch.
    Stop,
}

impl Plan {
    /// True for the plans that write something.
    pub const fn writes(&self) -> bool {
        matches!(self, Self::Write(_) | Self::Replace(_) | Self::Append(_))
    }
}

/// The standing conflict policy for one batch.
///
/// "Decisions apply for the remainder of the batch." That is
/// this struct: one "…all" answer installs itself here and no further question
/// is asked.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    standing: Option<ConflictChoice>,
    asked: u64,
}

/// The three things a conflict needs to know about a source.
///
/// Split out so a source that is not a file on this machine - a member of an
/// archive, a row of a search listing - can answer the same
/// questions the same way. See [`Policy::resolve_from`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// Whether the source is a directory. Two directories merge in silence.
    pub is_dir: bool,
    /// The source's size, for [`ConflictChoice::OverwriteIfDifferentSize`].
    pub size: u64,
    /// The source's mtime, for [`ConflictChoice::OverwriteIfNewer`]. `None`
    /// makes that choice skip rather than guess, which is the answer that
    /// destroys nothing.
    pub mtime: Option<std::time::SystemTime>,
}

impl Facts {
    /// What `lstat` says about a local source.
    ///
    /// A `Result` rather than a `Default`, because every field here decides
    /// whether a destination that already holds bytes keeps them. A source
    /// that could not be stat'ed used to answer "zero bytes, no mtime", and
    /// [`ConflictChoice::OverwriteIfDifferentSize`] reads a size of zero as
    /// "different from every non-empty destination": the destination was
    /// removed on the strength of a size nobody ever read. The same swallow
    /// made [`ConflictChoice::OverwriteIfNewer`] answer `Skip` and drop the
    /// file in silence. Neither is a guess this is entitled to make, so the
    /// error travels and [`Policy::resolve`] refuses.
    pub fn of(src: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let meta = fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?;
        Ok(Self {
            is_dir: meta.is_dir(),
            size: meta.size(),
            mtime: meta.modified().ok(),
        })
    }
}

impl Policy {
    /// A policy that starts with `preset` already standing.
    ///
    /// `Some` comes from the copy/move dialog, or from
    /// `ops.confirm_overwrite = false` in `config.toml`; `None` asks the UI on
    /// the first conflict.
    pub const fn new(preset: Option<ConflictChoice>) -> Self {
        Self {
            standing: preset,
            asked: 0,
        }
    }

    /// The choice that will be applied without asking, if any.
    pub const fn standing(&self) -> Option<ConflictChoice> {
        self.standing
    }

    /// Adopt `choice` as standing, for a caller resolving a conflict whose
    /// destination is not a path on this machine and so cannot go through
    /// [`Policy::resolve`]. "Apply to all" has to mean the same thing there.
    pub const fn adopt(&mut self, choice: ConflictChoice) {
        self.standing = Some(choice);
    }

    /// Count one question that reached the UI, for the same callers.
    pub const fn note_asked(&mut self) {
        self.asked = self.asked.saturating_add(1);
    }

    /// How many questions have actually reached the UI.
    ///
    /// Exists so a test can assert that an "all" answer really did stop the
    /// asking rather than merely producing the same outcome twice.
    pub const fn asked(&self) -> u64 {
        self.asked
    }

    /// Decide what to do about `dst`, asking the UI when there is no standing
    /// answer.
    ///
    /// `dst_vfs` is the same destination as a [`VfsPath`], because that is
    /// what the dialog displays and what a failure is recorded against.
    pub fn resolve(
        &mut self,
        src: &Path,
        dst: &Path,
        dst_vfs: &VfsPath,
        ctx: &mut JobContext,
    ) -> Plan {
        let facts = match Facts::of(src) {
            Ok(facts) => facts,
            // A source the kernel will not describe decides nothing. With
            // nothing in the way there is no decision to make and the writer
            // reports the same error itself a moment later; with something in
            // the way, refusing is the only honest answer - every choice below
            // this point would be comparing a size and an mtime that were
            // never read against a destination that exists.
            Err(err) => {
                return if fs::symlink_metadata(dst).is_ok() {
                    Plan::Refuse(format!(
                        "{err}: the destination {} was left alone rather than \
                         overwritten on the strength of a source that cannot be read",
                        dst.display()
                    ))
                } else {
                    Plan::Write(dst.to_path_buf())
                };
            }
        };
        self.resolve_from(src, facts, dst, dst_vfs, ctx)
    }

    /// [`Policy::resolve`] for a source the kernel cannot be asked about.
    ///
    /// A member of an archive has a size, an mtime and a kind, and they come
    /// from the container's own index rather than from `lstat`.
    /// Everything downstream of those three facts - "overwrite if newer",
    /// "overwrite if a different size", whether two directories merge - is the
    /// same question with the same answer, so it is the same code: only the
    /// way the facts are obtained differs.
    pub fn resolve_from(
        &mut self,
        src: &Path,
        facts: Facts,
        dst: &Path,
        dst_vfs: &VfsPath,
        ctx: &mut JobContext,
    ) -> Plan {
        // Nothing in the way: the common case, and it costs one `lstat`.
        let Ok(existing) = fs::symlink_metadata(dst) else {
            return Plan::Write(dst.to_path_buf());
        };
        let source = facts;
        let both_dirs = existing.is_dir() && source.is_dir;

        // Two directories merge without asking; see the module docs.
        if both_dirs {
            return Plan::Write(dst.to_path_buf());
        }

        let (choice, rename_to) = match self.standing {
            Some(standing) => (standing, None),
            None => {
                self.asked = self.asked.saturating_add(1);
                let request = ConflictRequest {
                    source: VfsPath::local(src),
                    dest: dst_vfs.clone(),
                    source_size: source.size,
                    dest_size: std::os::unix::fs::MetadataExt::size(&existing),
                    source_mtime: source.mtime,
                    dest_mtime: existing.modified().ok(),
                    both_dirs,
                    dest_is_dir: existing.is_dir(),
                };
                match ctx.ask(request) {
                    Some(Decision::Conflict {
                        choice,
                        rename_to,
                        apply_to_all,
                    }) => {
                        if apply_to_all {
                            self.standing = Some(choice);
                        }
                        (choice, rename_to)
                    }
                    // A `Cancel`, or a UI that went away, unwinds the batch.
                    Some(Decision::Cancel) | None => return Plan::Stop,
                }
            }
        };

        self.apply(choice, rename_to.as_deref(), src, dst, &existing, source)
    }

    /// Turn a resolved choice into a plan. Split out from [`Policy::resolve`]
    /// so every branch is reachable from a test without a channel.
    fn apply(
        &self,
        choice: ConflictChoice,
        rename_to: Option<&str>,
        src: &Path,
        dst: &Path,
        existing: &fs::Metadata,
        source: Facts,
    ) -> Plan {
        use std::os::unix::fs::MetadataExt as _;

        let source_is_dir = source.is_dir;
        match choice {
            ConflictChoice::Overwrite => write_over(dst, existing, source_is_dir),
            ConflictChoice::Skip => Plan::Skip,
            ConflictChoice::Append => {
                // append is files only. A
                // directory conflict never asks, so this can only be reached
                // by a preset policy pointed at a mismatched pair - where
                // appending a file to a directory is not an operation.
                if existing.is_dir() || source.is_dir {
                    Plan::Skip
                } else {
                    Plan::Append(dst.to_path_buf())
                }
            }
            ConflictChoice::Rename => {
                let target = match rename_to {
                    Some(name) if !name.is_empty() => {
                        dst.parent().unwrap_or(Path::new(".")).join(name)
                    }
                    // No name to use: generate one. This is the whole of
                    // what an "all" rename does after the first conflict,
                    // because a single typed name cannot serve a batch.
                    // The conflict that was
                    // actually answered still uses the name that was typed
                    // for it - the user was looking at that file - and only
                    // the ones that follow are generated.
                    _ => free_name(dst),
                };
                Plan::Write(target)
            }
            ConflictChoice::OverwriteIfNewer => {
                let newer = match (source.mtime, existing.modified().ok()) {
                    (Some(s), Some(d)) => s > d,
                    // No comparable timestamps: the conservative answer is the
                    // one that destroys nothing.
                    _ => false,
                };
                if newer {
                    write_over(dst, existing, source_is_dir)
                } else {
                    Plan::Skip
                }
            }
            ConflictChoice::OverwriteIfDifferentSize => {
                // The **source's** size, compared before anything is written.
                // For a plain copy the source
                // size and the final size are the same; for an append they are
                // not, and the source is the one the user is comparing.
                let _ = src;
                if source.size == existing.size() {
                    Plan::Skip
                } else {
                    write_over(dst, existing, source_is_dir)
                }
            }
        }
    }
}

/// The plan for a choice that means "this one goes over what is there".
///
/// The three overwriting choices share it so they cannot disagree about what
/// `Overwrite` means - which they did: [`super::copy::copy_symlink`] used to
/// remove any destination itself, including a whole directory tree, while
/// `ensure_dir` and the regular-file `rename` simply failed on the same
/// collision.
///
/// * Same kind of thing on both sides - two files, two symlinks, two
///   directories: write, and let the writer replace it atomically.
/// * A directory going over a **non-directory**: the thing in the way is one
///   entry, removing it is what the answer asked for, and `mkdir` cannot
///   replace it on its own. [`Plan::Replace`].
/// * A file or a link going over a **directory**: refused. The answer described
///   a file - the dialog showed a size and an mtime - and it is not consent to
///   `remove_dir_all` a tree that may hold thousands of files. the design
///   collects it as a per-item failure naming the destination, which is the
///   "never silent" half of the same section.
fn write_over(dst: &Path, existing: &fs::Metadata, source_is_dir: bool) -> Plan {
    if existing.is_dir() == source_is_dir {
        return Plan::Write(dst.to_path_buf());
    }
    if existing.is_dir() {
        return Plan::Refuse(format!(
            "{}: the destination is a directory; delete it first if that is what you meant",
            dst.display()
        ));
    }
    Plan::Replace(dst.to_path_buf())
}

/// `report.txt` → `report (2).txt`, then `report (3).txt`, and so on.
///
/// The counter goes before the extension, which is what makes the result still
/// open in the right application. A leading dot is not an extension, so
/// `.bashrc` becomes `.bashrc (2)` rather than `. (2)bashrc`.
pub fn free_name(dst: &Path) -> PathBuf {
    let parent = dst.parent().unwrap_or(Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(0) | None => (name.as_str(), ""),
        Some(i) => (
            name.get(..i).unwrap_or(&name),
            name.get(i.saturating_add(1)..).unwrap_or(""),
        ),
    };
    for n in 2..=9999u32 {
        let candidate = if ext.is_empty() {
            parent.join(format!("{stem} ({n})"))
        } else {
            parent.join(format!("{stem} ({n}).{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    // Ten thousand collisions is not a case worth a second naming scheme; the
    // copy fails with "file exists" rather than looping forever.
    dst.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::copy::tests::{TempTree, copy_spec, drive, drive_answering, listing};
    use crate::ops::{ConflictChoice, Decision};

    /// Two sources, both already present at the destination, so a batch has to
    /// make the same decision twice.
    fn two_conflicts(t: &TempTree) -> (Vec<std::path::PathBuf>, std::path::PathBuf) {
        let a = t.file("a.txt", b"NEW-A");
        let b = t.file("b.txt", b"NEW-B");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old-a").expect("seed a");
        fs::write(dest.join("b.txt"), b"old-b").expect("seed b");
        (vec![a, b], dest)
    }

    fn conflict(choice: ConflictChoice, apply_to_all: bool) -> Decision {
        Decision::Conflict {
            choice,
            rename_to: None,
            apply_to_all,
        }
    }

    #[test]
    fn overwrite_replaces_the_destination() {
        let t = TempTree::new("overwrite");
        t.file("a.txt", b"new");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");

        let mut spec = copy_spec(vec![t.path().join("a.txt")], &dest);
        spec.options.conflict = Some(ConflictChoice::Overwrite);
        assert!(drive(spec).is_clean());
        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"new");
    }

    #[test]
    fn skip_leaves_the_destination_alone() {
        let t = TempTree::new("skip");
        t.file("a.txt", b"new");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");

        let mut spec = copy_spec(vec![t.path().join("a.txt")], &dest);
        spec.options.conflict = Some(ConflictChoice::Skip);
        let summary = drive(spec);

        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"old");
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.files_done, 0);
    }

    #[test]
    fn rename_writes_beside_the_existing_file() {
        let t = TempTree::new("rename");
        t.file("a.txt", b"new");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");

        let mut spec = copy_spec(vec![t.path().join("a.txt")], &dest);
        spec.options.conflict = Some(ConflictChoice::Rename);
        let summary = drive(spec);

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(fs::read(dest.join("a.txt")).expect("old"), b"old");
        assert_eq!(fs::read(dest.join("a (2).txt")).expect("new"), b"new");
    }

    #[test]
    fn append_concatenates() {
        let t = TempTree::new("append");
        t.file("a.txt", b"world");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"hello ").expect("seed");

        let mut spec = copy_spec(vec![t.path().join("a.txt")], &dest);
        spec.options.conflict = Some(ConflictChoice::Append);
        let summary = drive(spec);

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"hello world");
    }

    #[test]
    fn overwrite_if_newer_compares_both_ways() {
        for (older_source, expected) in [(true, b"old".as_slice()), (false, b"new".as_slice())] {
            let t = TempTree::new("ifnewer");
            let dest = t.dir("dest");
            let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
            let new = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000);

            let src = t.file("a.txt", b"new");
            fs::write(dest.join("a.txt"), b"old").expect("seed");
            stamp(&src, if older_source { old } else { new });
            stamp(&dest.join("a.txt"), if older_source { new } else { old });

            let mut spec = copy_spec(vec![src], &dest);
            spec.options.conflict = Some(ConflictChoice::OverwriteIfNewer);
            // Preservation would carry the source's stamp across and make the
            // second round of this loop ambiguous.
            spec.options.preserve_attrs = false;
            drive(spec);
            assert_eq!(
                fs::read(dest.join("a.txt")).expect("read"),
                expected,
                "older_source = {older_source}"
            );
        }
    }

    /// Give a file a known mtime, so "newer" is a fact rather than a race.
    fn stamp(path: &std::path::Path, when: std::time::SystemTime) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for stamping");
        file.set_times(fs::FileTimes::new().set_modified(when).set_accessed(when))
            .expect("set_times");
    }

    #[test]
    fn overwrite_if_newer_refuses_when_the_source_cannot_be_stated() {
        // The conservative answer is the one that destroys nothing - but it is
        // also the one that says so. A source that cannot be stat'd used to
        // answer `Default`, which made this a silent `Skip`: the file was not
        // copied and nothing told anyone.
        let t = TempTree::new("ifnewer-missing");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");
        let missing = t.path().join("a.txt");

        let mut spec = copy_spec(vec![missing], &dest);
        spec.options.conflict = Some(ConflictChoice::OverwriteIfNewer);
        let summary = drive(spec);

        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"old");
        assert_eq!(summary.skipped, 0, "a refusal is not a skip");
        assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
    }

    #[test]
    fn overwrite_if_different_size_refuses_when_the_source_cannot_be_stated() {
        // The bug this pins: `Facts::of` answered `Default` for a source it
        // could not stat, so its size was 0, `0 != 3` was true, and the
        // destination was overwritten on the strength of a size that was
        // never read. The destination has to survive, and the batch has to
        // say why it did not get its file.
        let t = TempTree::new("ifsize-missing");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");
        let missing = t.path().join("a.txt");

        let mut spec = copy_spec(vec![missing], &dest);
        spec.options.conflict = Some(ConflictChoice::OverwriteIfDifferentSize);
        let summary = drive(spec);

        assert_eq!(
            fs::read(dest.join("a.txt")).expect("read"),
            b"old",
            "the destination was overwritten from a size nobody read"
        );
        assert_eq!(summary.files_done, 0);
        // Not merely "a failure": the copy loop would report the unreadable
        // source on its own a moment later, so what is pinned here is that the
        // *policy* refused before the destination was ever a candidate.
        assert_eq!(summary.failures.len(), 1, "{:?}", summary.failures);
        assert!(
            summary
                .failures
                .first()
                .is_some_and(|f| f.error.contains("was left alone rather than overwritten")),
            "{:?}",
            summary.failures
        );
    }

    #[test]
    fn a_source_that_cannot_be_stated_refuses_rather_than_guessing() {
        // Straight at `Policy::resolve`, so the refusal is shown to come from
        // the conflict policy rather than from the copy loop noticing later.
        let t = TempTree::new("facts-refuse");
        let dst = t.file("dest/a.txt", b"y");
        let missing = t.path().join("gone.txt");
        assert!(Facts::of(&missing).is_err());

        let mut policy = Policy::new(Some(ConflictChoice::OverwriteIfDifferentSize));
        let (mut ctx, rx, _dtx, _flag) =
            crate::ops::JobContext::for_test(crate::ops::JobKind::Copy);
        let plan = policy.resolve(&missing, &dst, &VfsPath::local(&dst), &mut ctx);
        drop(rx);

        assert!(matches!(plan, Plan::Refuse(_)), "{plan:?}");
        assert!(!plan.writes());
    }

    #[test]
    fn a_source_that_cannot_be_stated_still_writes_where_nothing_is_in_the_way() {
        // The refusal is about a destination that would be destroyed. With
        // nothing there, there is no conflict to resolve and the copy reports
        // the unreadable source itself, which is one failure rather than two
        // spellings of it.
        let t = TempTree::new("facts-no-conflict");
        let missing = t.path().join("gone.txt");
        let dst = t.path().join("dest/a.txt");

        let mut policy = Policy::new(Some(ConflictChoice::Overwrite));
        let (mut ctx, rx, _dtx, _flag) =
            crate::ops::JobContext::for_test(crate::ops::JobKind::Copy);
        let plan = policy.resolve(&missing, &dst, &VfsPath::local(&dst), &mut ctx);
        drop(rx);

        assert_eq!(plan, Plan::Write(dst));
    }

    #[test]
    fn overwrite_if_different_size_compares_the_source_size() {
        let t = TempTree::new("ifsize");
        let dest = t.dir("dest");

        // Same size: nothing happens.
        let same = t.file("same.txt", b"1234");
        fs::write(dest.join("same.txt"), b"abcd").expect("seed same");
        // Different size: it goes across.
        let diff = t.file("diff.txt", b"123456");
        fs::write(dest.join("diff.txt"), b"abcd").expect("seed diff");

        let mut spec = copy_spec(vec![same, diff], &dest);
        spec.options.conflict = Some(ConflictChoice::OverwriteIfDifferentSize);
        let summary = drive(spec);

        assert_eq!(fs::read(dest.join("same.txt")).expect("same"), b"abcd");
        assert_eq!(fs::read(dest.join("diff.txt")).expect("diff"), b"123456");
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.files_done, 1);
    }

    #[test]
    fn an_all_decision_is_not_asked_a_second_time() {
        // "Decisions apply for the remainder of the batch."
        // The second answer queued here is deliberately the opposite one: if
        // the engine asked twice it would take it, and `b.txt` would change.
        let t = TempTree::new("all-skip");
        let (sources, dest) = two_conflicts(&t);

        let summary = drive_answering(
            copy_spec(sources, &dest),
            vec![
                conflict(ConflictChoice::Skip, true),
                conflict(ConflictChoice::Overwrite, false),
            ],
        );

        assert_eq!(fs::read(dest.join("a.txt")).expect("a"), b"old-a");
        assert_eq!(
            fs::read(dest.join("b.txt")).expect("b"),
            b"old-b",
            "the second conflict was never asked about"
        );
        assert_eq!(summary.skipped, 2);
        assert_eq!(summary.files_done, 0);
    }

    #[test]
    fn a_single_decision_only_covers_a_single_conflict() {
        // The mirror of the test above, so "all" is shown to be what made the
        // difference rather than the order of the answers.
        let t = TempTree::new("one-at-a-time");
        let (sources, dest) = two_conflicts(&t);

        drive_answering(
            copy_spec(sources, &dest),
            vec![
                conflict(ConflictChoice::Skip, false),
                conflict(ConflictChoice::Overwrite, false),
            ],
        );

        let a = fs::read(dest.join("a.txt")).expect("a");
        let b = fs::read(dest.join("b.txt")).expect("b");
        // The order the two sources are answered in is the order they were
        // given, so exactly one of them was overwritten.
        assert!(
            (a == b"old-a" && b == b"NEW-B") || (a == b"NEW-A" && b == b"old-b"),
            "one skipped and one overwritten: {a:?} / {b:?}"
        );
    }

    #[test]
    fn an_all_rename_generates_a_name_for_every_file_after_the_first() {
        // one typed name cannot serve a batch. The
        // conflict that was answered keeps the name that was typed for it; the
        // rest are generated, because there is nothing else they could be.
        let t = TempTree::new("all-rename");
        let (sources, dest) = two_conflicts(&t);

        let summary = drive_answering(
            copy_spec(sources, &dest),
            vec![Decision::Conflict {
                choice: ConflictChoice::Rename,
                rename_to: Some("typed.txt".to_string()),
                apply_to_all: true,
            }],
        );

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(
            listing(&dest),
            vec![
                "a.txt".to_string(),
                "b (2).txt".to_string(),
                "b.txt".to_string(),
                "typed.txt".to_string(),
            ]
        );
        assert_eq!(
            fs::read(dest.join("typed.txt")).expect("typed"),
            b"NEW-A",
            "the file the user was looking at took the name they typed"
        );
        assert_eq!(
            fs::read(dest.join("b (2).txt")).expect("b2"),
            b"NEW-B",
            "and the rest were generated without asking again"
        );
        assert_eq!(fs::read(dest.join("a.txt")).expect("a"), b"old-a");
        assert_eq!(fs::read(dest.join("b.txt")).expect("b"), b"old-b");
    }

    #[test]
    fn a_typed_rename_is_used_for_the_one_file_it_was_typed_for() {
        let t = TempTree::new("typed-rename");
        t.file("a.txt", b"new");
        let dest = t.dir("dest");
        fs::write(dest.join("a.txt"), b"old").expect("seed");

        let summary = drive_answering(
            copy_spec(vec![t.path().join("a.txt")], &dest),
            vec![Decision::Conflict {
                choice: ConflictChoice::Rename,
                rename_to: Some("chosen.txt".to_string()),
                apply_to_all: false,
            }],
        );

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(fs::read(dest.join("chosen.txt")).expect("read"), b"new");
        assert_eq!(fs::read(dest.join("a.txt")).expect("old"), b"old");
    }

    #[test]
    fn cancelling_a_conflict_stops_the_batch() {
        let t = TempTree::new("cancel-conflict");
        let (sources, dest) = two_conflicts(&t);

        let summary = drive_answering(copy_spec(sources, &dest), vec![Decision::Cancel]);

        assert!(summary.cancelled);
        assert_eq!(fs::read(dest.join("a.txt")).expect("a"), b"old-a");
        assert_eq!(fs::read(dest.join("b.txt")).expect("b"), b"old-b");
    }

    #[test]
    fn two_directories_merge_without_asking() {
        // the design. No answers are queued: if the engine
        // asked, `ask` would block and this test would never finish.
        let t = TempTree::new("merge");
        t.file("src/a.txt", b"a");
        let dest = t.dir("dest");
        t.file("dest/src/b.txt", b"b");

        let summary = drive(copy_spec(vec![t.path().join("src")], &dest));

        assert!(summary.is_clean(), "{:?}", summary.failures);
        assert_eq!(listing(&dest.join("src")), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn a_standing_policy_is_never_asked_about_at_all() {
        let mut policy = Policy::new(Some(ConflictChoice::Skip));
        assert_eq!(policy.standing(), Some(ConflictChoice::Skip));

        let t = TempTree::new("standing");
        let src = t.file("a.txt", b"x");
        let dst = t.file("dest/a.txt", b"y");
        let (mut ctx, rx, _dtx, _flag) =
            crate::ops::JobContext::for_test(crate::ops::JobKind::Copy);
        let plan = policy.resolve(&src, &dst, &VfsPath::local(&dst), &mut ctx);
        drop(rx);

        assert_eq!(plan, Plan::Skip);
        assert!(!plan.writes());
        assert_eq!(policy.asked(), 0, "no question reached the UI");
    }

    #[test]
    fn an_absent_destination_is_never_a_conflict() {
        let t = TempTree::new("no-conflict");
        let src = t.file("a.txt", b"x");
        let dst = t.path().join("dest/a.txt");
        let mut policy = Policy::new(None);
        let (mut ctx, rx, _dtx, _flag) =
            crate::ops::JobContext::for_test(crate::ops::JobKind::Copy);
        let plan = policy.resolve(&src, &dst, &VfsPath::local(&dst), &mut ctx);
        drop(rx);

        assert_eq!(plan, Plan::Write(dst));
        assert_eq!(policy.asked(), 0);
    }

    #[test]
    fn free_name_never_loops_forever() {
        let t = TempTree::new("freename-loop");
        let a = t.file("x.txt", b"1");
        assert_eq!(free_name(&a).file_name().expect("n"), "x (2).txt");
        // A leading dot is not an extension.
        let dotfile = t.file(".bashrc", b"1");
        assert_eq!(free_name(&dotfile).file_name().expect("n"), ".bashrc (2)");
    }
}
