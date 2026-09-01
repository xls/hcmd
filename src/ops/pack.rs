//! `Alt+F5`: pack a selection into one new archive.
//!
//! > `Alt+F5` packs the selection: a dialog for target name, format,
//! > compression level, and "move to archive" (pack then delete sources).
//!
//! # Why this is not the copy engine
//!
//! It used to be. `App::perform_pack` wrote an **empty** container and then
//! queued an ordinary `F5` into `<archive>#/`, which is a pleasing shape - the
//! same progress, the same conflicts, the same `Esc` - and is wrong for the
//! formats that cannot write a member without rewriting the container.
//!
//! `F5` writes members one at a time through [`crate::vfs::Vfs::open_write`],
//! and each `flush` is one `ArchiveFormat::apply`. For a `.zip` or a plain
//! `.tar` that is a member-level write and costs what it looks like it costs.
//! For a compressed tar - or a `.7z` - it is a full decompress-rewrite-
//! recompress of everything written so far, **per file**, so packing *N* files
//! cost *N²*: measured on this machine, 8 files 0.49 s, 16 files 1.85 s,
//! 32 files 7.19 s, 64 files 28.6 s, against about a second for `tar czf` on
//! the same bytes. A few hundred files is half an hour.
//!
//! It was also wrong about the design, which requires the rewrite gates to
//! be "checked before anything is touched": the gate measured the empty
//! container the pack had just written, said `Proceed`, and the per-member
//! backstop inside `ArchiveFs` then refused every member after the archive
//! being built crossed `rewrite_max_size` - so a large pack wrote half of
//! itself and failed the rest, advising the user to "extract it, change it and
//! repack it deliberately" about the pack they had just asked for. With "move
//! to archive" the sources of the first half were already gone.
//!
//! So a pack is **one** [`ArchiveFormat::create`] call with the whole
//! selection. That is linear, it is what every archiver does, it needs no
//! gates because it rewrites nothing, and it leaves no container behind at all
//! when it fails: `create` writes beside the target and renames only on
//! success.
//!
//! # What it keeps from the copy engine
//!
//! Everything the user can see. It is still a [`JobKind::Copy`] or
//! [`JobKind::Move`] with the ordinary progress dialog, the ordinary summary
//! and the ordinary `Esc` - the difference is entirely
//! in how the bytes reach the container. "Move to archive" is the `Move`,
//! which deletes the sources only after a pack that wholly succeeded.

use std::path::{Path, PathBuf};

use super::walk::TreeStats;
use super::{JobContext, JobKind, JobOptions, JobSpec, PackInto};
use crate::error::{Error, Result};
use crate::vfs::archive::format::{ArchiveFormat, CompressionLevel, MemberEdit, WriteProgress};
use crate::vfs::{EntryKind, Vfs, VfsPath};

/// Everything one pack needs that is not in the [`JobSpec`].
struct Target<'a> {
    container: &'a Path,
    format: &'static dyn ArchiveFormat,
    level: CompressionLevel,
}

/// The `Alt+F5` runner.
///
/// `into` is what the pack dialog collected; `spec.dest` is the container to
/// write, as a **local** path rather than as an archive segment, because the
/// archive does not exist yet and nothing is being written *into* one.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext, into: PackInto) {
    debug_assert!(matches!(spec.kind, JobKind::Copy | JobKind::Move));

    let Some(dest) = spec.dest.as_ref() else {
        ctx.start(0, 0);
        for source in &spec.sources {
            ctx.fail(source, "no archive was named to pack into");
        }
        return;
    };
    let Some(container) = dest.local_path() else {
        ctx.start(0, 0);
        ctx.fail(dest, "an archive is packed onto a real filesystem");
        return;
    };
    let format = into.format.backend();
    if !format.can_create() {
        // refused up front, before a source is read. `can_create`, not
        // `write_model`: this job makes a new container, and a format that
        // can rewrite a member of an existing archive is not thereby one
        // that a selection can be packed into.
        ctx.start(0, 0);
        ctx.fail(
            dest,
            format!("a new {} archive cannot be created", into.format),
        );
        return;
    }
    if container.exists() {
        // Overwriting an archive that already exists would silently discard
        // everything in it. The name is the user's to change.
        ctx.start(0, 0);
        ctx.fail(dest, "it already exists");
        return;
    }

    let target = Target {
        container,
        format,
        level: CompressionLevel::new(into.level),
    };
    let staging = Staging::new(container);
    let mut edits = Vec::new();
    let mut totals = TreeStats::ZERO;
    let mut failed = false;

    for source in &spec.sources {
        if ctx.cancelled() {
            break;
        }
        if let Err(err) = collect(
            vfs,
            source,
            &spec.options,
            &staging,
            &mut edits,
            &mut totals,
            ctx,
        ) {
            ctx.fail(source, err);
            failed = true;
        }
    }
    ctx.start(totals.files, totals.bytes);

    if ctx.cancelled() {
        return;
    }
    if edits.is_empty() {
        if !failed {
            ctx.fail(dest, "there is nothing to pack");
        }
        return;
    }

    ctx.set_file(&dest.to_string(), totals.bytes);
    let written = {
        let mut progress = Bytes { ctx };
        target
            .format
            .create(target.container, &edits, target.level, &mut progress)
    };
    if let Err(err) = written {
        ctx.fail(dest, err);
        return;
    }
    // Counted here rather than as each member is staged: until `create`
    // returns there is no archive, so nothing has been packed yet.
    for edit in &edits {
        if matches!(edit, MemberEdit::Put { .. }) {
            ctx.add_file();
        } else {
            ctx.add_dir();
        }
    }

    // "pack then delete sources", and only after a pack that
    // wholly succeeded - the same rule a move follows.
    if spec.kind == JobKind::Move && !failed && !ctx.cancelled() {
        for source in &spec.sources {
            if let Err(err) = vfs.remove(source) {
                ctx.fail(source, err);
            }
        }
    }
}

/// Walk one source and add it, and everything under it, to `edits`.
///
/// Member paths are relative to the source's **parent**, so packing
/// `stuff/alpha.txt` records `alpha.txt` and packing the directory `stuff`
/// records `stuff/alpha.txt` - which is what the copy engine did when it
/// joined the source's file name onto the destination, and what every
/// archiver does.
fn collect(
    vfs: &dyn Vfs,
    source: &VfsPath,
    options: &JobOptions,
    staging: &Staging,
    edits: &mut Vec<MemberEdit>,
    totals: &mut TreeStats,
    ctx: &JobContext,
) -> Result<()> {
    let entry = vfs.stat(source)?;
    let Some(name) = source.file_name() else {
        return Err(Error::InvalidPath(format!(
            "{source}: the root of a backend has no name to pack it under"
        )));
    };
    let mut stack = vec![(source.clone(), name, entry)];
    while let Some((at, member_path, entry)) = stack.pop() {
        if ctx.cancelled() {
            return Ok(());
        }
        match entry.kind {
            EntryKind::Dir => {
                edits.push(MemberEdit::PutDir {
                    member_path: member_path.clone(),
                    mode: entry.mode,
                });
                totals.dirs = totals.dirs.saturating_add(1);
                let (children, listing) = super::copy::vfs::list_via_vfs(vfs, &at);
                if let Some(err) = listing {
                    return Err(err);
                }
                for child in children {
                    let child_is_dir = matches!(child.kind, EntryKind::Dir);
                    // "Only files of this type" filters files, never
                    // directories: a mask of `*.rs` still has to descend to
                    // find any.
                    if !child_is_dir && !super::mask::matches(&options.file_mask, &child.name) {
                        continue;
                    }
                    let path = format!("{member_path}/{}", child.name);
                    stack.push((at.join(&child.name), path, child));
                }
            }
            _ => {
                if !super::mask::matches(&options.file_mask, &entry.name) {
                    continue;
                }
                let (source_file, mode) = match at.local_path() {
                    Some(local) => (local.to_path_buf(), entry.mode),
                    // A source that is not on this machine - a member of
                    // another archive - has no path `create` can open, so it
                    // is staged into a temp file first. The same answer
                    // the design gives for a nested archive, for the same
                    // reason: the library takes a path or nothing.
                    None => (staging.stage(vfs, &at, &member_path)?, entry.mode),
                };
                totals.files = totals.files.saturating_add(1);
                totals.bytes = totals.bytes.saturating_add(entry.size);
                edits.push(MemberEdit::Put {
                    member_path,
                    source: source_file,
                    mode,
                    mtime: entry.mtime,
                });
            }
        }
    }
    Ok(())
}

/// Temp copies of sources that are not files on this machine.
///
/// Created lazily - the overwhelmingly common pack is of local files and never
/// makes the directory at all - and removed when the job ends, whether it
/// succeeded or not.
struct Staging {
    dir: PathBuf,
    made: std::sync::OnceLock<()>,
}

impl Staging {
    fn new(container: &Path) -> Self {
        // Beside the archive being written, so the staged bytes are on the
        // filesystem that is about to hold them and the copy into the
        // container is not a second cross-device move.
        let parent = container.parent().unwrap_or(Path::new("."));
        let stem = container
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_string());
        Self {
            dir: parent.join(format!(".{stem}.hcmd-pack-{}", std::process::id())),
            made: std::sync::OnceLock::new(),
        }
    }

    /// Copy `src` into the staging directory and return where it landed.
    fn stage(&self, vfs: &dyn Vfs, src: &VfsPath, member_path: &str) -> Result<PathBuf> {
        if self.made.get().is_none() {
            std::fs::create_dir_all(&self.dir)
                .map_err(|e| crate::error::Error::io(&self.dir, e))?;
            let _ = self.made.set(());
        }
        // One flat name per member, so two members called `a.txt` in different
        // directories do not collide.
        let flat: String = member_path
            .chars()
            .map(|c| if c == '/' { '_' } else { c })
            .collect();
        let at = self
            .dir
            .join(format!("{:x}-{flat}", self.dir.as_os_str().len()));
        let mut reader = vfs.open_read(src)?;
        let mut file = std::fs::File::create(&at).map_err(|e| Error::io(&at, e))?;
        std::io::copy(&mut reader, &mut file).map_err(Error::Bare)?;
        Ok(at)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.made.get().is_some() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// The bytes a format writes, counted in the job.
///
/// The one place a rate is measured that is *not* the copy loop, for the
/// reason `WriteProgress`'s own documentation gives: the bytes never pass
/// through one, because the format is writing them itself.
struct Bytes<'a> {
    ctx: &'a mut JobContext,
}

impl WriteProgress for Bytes<'_> {
    fn bytes(&mut self, n: u64) -> bool {
        self.ctx.add_bytes(n)
    }
}
