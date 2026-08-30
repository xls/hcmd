//! Copy and move.
//!
//! # The five rules this file exists to keep
//!
//! 1. **Cancellation leaves no half-written destination.** Every regular file
//!    is written to a temporary name beside its destination and renamed into
//!    place only once it is complete, verified and stamped. A cancel, an error
//!    or a full disk removes the temporary and leaves whatever was already
//!    there untouched. [`super::ConflictChoice::Append`] is the one exception,
//!    and says so.
//! 2. **Cancellation is honoured inside the chunk loop**, not merely between
//!    files: [`copy_stream`] checks on every chunk, so `Esc` during a 40 GiB
//!    file stops in milliseconds.
//! 3. **A failure never aborts the batch.** Each source is attempted; failures
//!    are collected on the summary.
//! 4. **A non-writable destination is refused before anything starts**, not
//!    halfway through. The rule is about
//!    [`Capabilities::writable`] and applies to every backend, not only to
//!    archives - and it is asked of the destination *directory* as well as of
//!    the backend, because a read-only mount and a mode-555 directory are the
//!    ordinary ways a local copy has nowhere to go. See [`probe_writable`].
//! 5. **A move deletes its source only after a copy that took everything.**
//!    See [`super::move_::may_delete_source`].
//!
//! # Sparse files
//!
//! "Sparse files should stay sparse where the platform allows."
//! *Detecting* a hole in the source needs `SEEK_HOLE`, which std does not
//! expose and which would mean a `libc` dependency this milestone is not
//! authorised to add. *Making* a hole in the destination needs no syscall std
//! lacks: seeking past a run of zero bytes instead of writing them leaves a
//! hole behind. [`copy_stream`] does exactly that, so a sparse source comes
//! out sparse (its holes read as zeros, and zeros are not written) and a dense
//! file of zeros comes out sparse too, which is harmless.
//!
//! # Preservation, honestly
//!
//! the design asks for "mode, mtime, and where permitted uid/gid and xattrs".
//! Without `libc` and without `xattr`, std reaches mode (`PermissionsExt`)
//! and both timestamps (`File::set_times`, stable since 1.75) and nothing
//! else. [`PRESERVED`] says what is actually carried across so the UI can
//! tell the truth rather than implying more; the design records the deferral.
//!
//! # Cycles
//!
//! Cycle protection is unconditional, matching [`super::walk`]: a bind mount
//! builds a cycle with no symlink in it, and the cost is one `(dev, ino)` pair
//! per directory against an otherwise unbounded copy. The destination's own
//! directory is seeded into the set, so a symlink pointing back into the
//! destination cannot be followed into a copy of the copy.
//!
//! [`Capabilities::writable`]: crate::vfs::Capabilities::writable

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::conflict::{Facts, Plan, Policy};
use super::mask;
use super::move_;
use super::walk::{TreeStats, WalkOptions, walk_stats_filtered};
use super::{ConflictChoice, ConflictRequest, Decision, JobContext, JobKind, JobOptions, JobSpec};
use crate::error::{Error, Result};
use crate::vfs::{Entry, EntryKind, Vfs, VfsPath};

pub use super::conflict::free_name;

pub mod vfs;

/// How much is read and written at a time.
///
/// 256 KiB: large enough that the per-call overhead vanishes against the copy
/// itself, small enough that a cancel is noticed within a few milliseconds even
/// on a slow network mount, which is what the "within a large
/// file's chunk loop" is asking for. [`super::chunk_size`] scales it by
/// [`crate::vfs::LatencyClass`].
pub const COPY_CHUNK: usize = 256 * 1024;

/// What "preserve attributes" actually preserves in v0.2.
///
/// the design says "where permitted", and this is what std permits without
/// a new dependency. uid/gid needs `chown(2)` and xattrs need
/// `listxattr`/`getxattr`/`setxattr`; neither is in std and neither `libc` nor
/// `xattr` is authorised for this milestone.
pub const PRESERVED: &[&str] = &["mode", "mtime", "atime"];

/// What is not preserved, and why - shown beside the "Preserve attributes"
/// checkbox so the box does not promise more than it does.
pub const NOT_PRESERVED: &str = "uid/gid and xattrs need syscalls std does not expose";

/// The suffix a partially written destination carries until it is complete.
const PARTIAL_SUFFIX: &str = ".hcmd-part";

/// What happened under one source.
///
/// A move consults this before deleting anything
/// ([`super::move_::may_delete_source`]), which is why "skipped" is tracked
/// separately from "failed": a skipped file is not at the destination, and a
/// recursive delete of the source would take it with the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceOutcome {
    /// Something under this source could not be copied.
    pub failed: bool,
    /// Something under this source was deliberately not copied - a conflict
    /// answered `Skip`, or a name the file mask excluded.
    pub skipped: bool,
    /// The batch was cancelled or the user answered `Cancel` to a conflict.
    /// The caller stops after this one.
    pub stopped: bool,
}

impl SourceOutcome {
    /// True when everything under this source reached the destination.
    pub const fn complete(&self) -> bool {
        !self.failed && !self.skipped && !self.stopped
    }
}

/// How many names are tried before a partial file gives up.
///
/// Only a name another process is holding at this instant is ever refused, and
/// the counter has already moved on by then, so one retry would do; sixteen is
/// free and cannot loop.
const PARTIAL_ATTEMPTS: u32 = 16;

/// Where a destination lives while it is still being written.
///
/// The name carries this **process's id and a counter**, because the basename
/// alone is not unique: two jobs - a queue running two copies, two terminals,
/// or one panel and one background job - can be writing the same destination
/// name into the same directory at once, and a shared partial file means two
/// writers at independent offsets in one inode. The first to finish renames the
/// mixture into place and reports a clean copy. [`probe_writable`] already took
/// this precaution for its own probe file; this is the same rule applied to the
/// file that actually becomes the destination.
fn partial_path(dst: &Path, unique: u64) -> PathBuf {
    let parent = dst.parent().unwrap_or(Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let pid = std::process::id();
    parent.join(format!(".{name}.{pid}-{unique}{PARTIAL_SUFFIX}"))
}

/// Create the partial file for `dst` under a name nothing else has open.
///
/// `create_new` rather than `create`: a name that is somehow already there is
/// stepped over rather than truncated, so this can never take a file another
/// job is in the middle of writing.
fn create_partial(dst: &Path) -> Result<(PathBuf, fs::File)> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..PARTIAL_ATTEMPTS {
        let tmp = partial_path(dst, next_unique());
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => return Err(Error::io(&tmp, err)),
        }
    }
    Err(match last {
        Some(err) => Error::io(dst, err),
        None => Error::msg(format!(
            "{}: no free name for the partial file",
            dst.display()
        )),
    })
}

/// Put a fully written partial file on the disk, and report it if that fails.
///
/// **This is the commit, and `Write::flush` is not it.** `flush` on a
/// `std::fs::File` is documented to do nothing and return `Ok(())`: the bytes
/// are in the kernel's page cache, not on the medium. Worse, dropping the
/// handle discards the result of `close(2)`, which is exactly where a network
/// or quota-backed filesystem reports `ENOSPC`, `EDQUOT` and `EIO`. Without
/// this, a truncated copy was renamed over the destination and reported as
/// done, and for a **move** the source was then deleted, which loses the file.
///
/// So the file is synced while the handle is still owned here, and the error
/// belongs to the partial file, which the caller then removes.
fn commit_partial(writer: fs::File, tmp: &Path) -> Result<()> {
    writer.sync_all().map_err(|e| Error::io(tmp, e))?;
    // Only now, with the bytes durable, is the handle allowed to close.
    drop(writer);
    Ok(())
}

/// Sync the directory entry a rename just created.
///
/// Best effort, deliberately: some filesystems refuse to `fsync` a directory,
/// and a copy that has already landed must not be failed because of it. Same
/// call and same reasoning as `vfs::archive::rewrite`'s commit.
fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    }
}

/// A number no other partial file or write probe in this process shares.
fn next_unique() -> u64 {
    UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Copy bytes from one open file to another, reporting progress and stopping
/// on cancel.
///
/// Returns the number of bytes copied. `Err(Error::Cancelled)` means the caller
/// must clean up: this function writes and does not delete.
///
/// A run of zero bytes is turned into a **hole** by seeking past it rather than
/// writing it, which is how a file is made sparse without `fallocate` or any
/// other syscall std does not expose. `set_len` at the end materialises a
/// trailing hole, which a bare seek would otherwise leave off the file.
pub fn copy_stream(
    reader: &mut fs::File,
    writer: &mut fs::File,
    ctx: &mut JobContext,
) -> Result<u64> {
    copy_stream_from(reader, writer, ctx, COPY_CHUNK)
}

/// [`copy_stream`] over any reader.
///
/// The seam exists for one reason: a test needs a reader that cancels the job
/// partway through so the chunk loop's cancellation can be proved
/// deterministically, rather than by racing a thread against a large file and
/// hoping. The writer stays a [`fs::File`] because the hole logic seeks.
pub(crate) fn copy_stream_from<R: Read>(
    reader: &mut R,
    writer: &mut fs::File,
    ctx: &mut JobContext,
    chunk: usize,
) -> Result<u64> {
    // rule 3: the size comes from the source's `Capabilities`,
    // so a read over a network is one big request rather than four small ones
    // (the design I12). Never zero, whatever a caller computes.
    let mut buf = vec![0u8; chunk.max(1)];
    let mut total: u64 = 0;
    let mut hole: u64 = 0;

    loop {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Error::Bare(err)),
        };
        let chunk = buf.get(..read).unwrap_or(&[]);

        if chunk.iter().all(|b| *b == 0) {
            hole = hole.saturating_add(read as u64);
        } else {
            if hole > 0 {
                match i64::try_from(hole) {
                    Ok(skip) => {
                        writer.seek(SeekFrom::Current(skip))?;
                    }
                    // Absurd, but a wrong seek would corrupt the file: fall
                    // back to writing the zeros out.
                    Err(_) => write_zeros(writer, hole)?,
                }
                hole = 0;
            }
            writer.write_all(chunk)?;
        }

        total = total.saturating_add(read as u64);
        if !ctx.add_bytes(read as u64) {
            return Err(Error::Cancelled);
        }
    }

    // Covers the trailing-hole case and is a no-op otherwise.
    writer.set_len(total)?;
    Ok(total)
}

/// Write `count` zero bytes, for the seek fallback above.
fn write_zeros(writer: &mut fs::File, count: u64) -> Result<()> {
    let zeros = vec![0u8; COPY_CHUNK];
    let mut left = count;
    while left > 0 {
        let n = usize::try_from(left.min(COPY_CHUNK as u64)).unwrap_or(COPY_CHUNK);
        writer.write_all(zeros.get(..n).unwrap_or(&[]))?;
        left = left.saturating_sub(n as u64);
    }
    Ok(())
}

/// Set one destination's mode, best effort, naming what stopped it if
/// anything did.
///
/// The one place a mode is stamped, so the three preservation paths - a local
/// file, a member coming out of a container, a directory on the way out -
/// cannot disagree about what a refusal means.
fn stamp_mode(dest: &Path, bits: u32) -> Option<String> {
    fs::set_permissions(dest, fs::Permissions::from_mode(bits))
        .err()
        .map(|err| format!("copied, but its mode {bits:04o} was not preserved: {err}"))
}

/// The twin of [`stamp_mode`] for the timestamps, on an already-open
/// destination because that is what `futimens` wants.
fn stamp_times(dest_file: &fs::File, times: fs::FileTimes) -> Option<String> {
    dest_file
        .set_times(times)
        .err()
        .map(|err| format!("copied, but its timestamps were not preserved: {err}"))
}

/// Carry across what std can carry (the design; see [`PRESERVED`]).
///
/// Best effort on all of it, and the whole function rather than only the
/// times: mode and mtime are *attributes* of a destination whose every byte
/// is already written, flushed and about to be verified, so a filesystem that
/// refuses one is not a reason to throw the bytes away - which is what a `?`
/// here did, by returning an error that deletes the finished temporary file.
/// A mount that will not take a `chmod` discarded the whole copy while the
/// same file extracted out of an archive landed at 0644 and reported success.
///
/// Best effort is not silent, which is the other half of the policy: what
/// could not be carried comes back here, one line per attribute, for the
/// caller to record against the destination.
#[must_use]
pub fn preserve(source: &fs::Metadata, dest_file: &fs::File, dest: &Path) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(stamp_mode(dest, source.mode() & 0o7777));

    if let Ok(modified) = source.modified() {
        let accessed = source.accessed().unwrap_or(modified);
        let times = fs::FileTimes::new()
            .set_modified(modified)
            .set_accessed(accessed);
        warnings.extend(stamp_times(dest_file, times));
    }
    warnings
}

/// the Verify checkbox: re-read both files and compare.
///
/// A streamed byte comparison rather than a digest, so nothing is held in
/// memory and no hash dependency is implied.
///
fn verify(src: &Path, dst: &Path, ctx: &mut JobContext) -> Result<()> {
    let mut a = fs::File::open(src).map_err(|e| Error::io(src, e))?;
    let mut b = fs::File::open(dst).map_err(|e| Error::io(dst, e))?;
    let mut ba = vec![0u8; COPY_CHUNK];
    let mut bb = vec![0u8; COPY_CHUNK];
    loop {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let na = read_full(&mut a, &mut ba)?;
        let nb = read_full(&mut b, &mut bb)?;
        if na != nb || ba.get(..na) != bb.get(..nb) {
            return Err(Error::msg(format!(
                "verify failed: {} differs from {}",
                src.display(),
                dst.display()
            )));
        }
        if na == 0 {
            return Ok(());
        }
    }
}

/// Fill `buf` as far as EOF allows, so two readers stay in step.
fn read_full(file: &mut fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(buf.get_mut(filled..).unwrap_or(&mut [])) {
            Ok(0) => break,
            Ok(n) => filled = filled.saturating_add(n),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(Error::Bare(err)),
        }
    }
    Ok(filled)
}

/// Copy one regular file, atomically from the destination's point of view.
fn copy_regular(src: &Path, dst: &Path, options: &JobOptions, ctx: &mut JobContext) -> Result<u64> {
    let meta = fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?;
    let mut reader = fs::File::open(src).map_err(|e| Error::io(src, e))?;
    copy_regular_from(&mut reader, &meta, src, dst, options, ctx)
}

/// [`copy_regular`] over an already-open reader.
///
/// The whole point of the split: a test can hand this a reader that cancels
/// after one chunk and then assert the two things the design actually
/// promises - the destination does not exist, and neither does the partial.
pub(crate) fn copy_regular_from<R: Read>(
    reader: &mut R,
    meta: &fs::Metadata,
    src: &Path,
    dst: &Path,
    options: &JobOptions,
    ctx: &mut JobContext,
) -> Result<u64> {
    let (tmp, mut writer) = create_partial(dst)?;
    // What the attribute stamp could not carry, held until the rename has
    // made the destination real: a warning about a file that then failed and
    // was removed would be a warning about nothing.
    let mut attr_warnings: Vec<String> = Vec::new();
    let outcome = (|| -> Result<u64> {
        let bytes = copy_stream_from(reader, &mut writer, ctx, COPY_CHUNK)?;
        writer.flush()?;
        if options.preserve_attrs {
            attr_warnings = preserve(meta, &writer, &tmp);
        }
        // The durability barrier, before the rename that makes this the
        // destination and before the verify that claims to have read it back.
        // A verify that runs against the page cache the write just filled can
        // only prove the copy loop, never the medium, which is the one reason
        // to ask for it.
        commit_partial(writer, &tmp)?;
        if options.verify {
            verify(src, &tmp, ctx)?;
        }
        Ok(bytes)
    })();

    match outcome {
        Ok(bytes) => {
            // The rename is the moment the destination becomes real. Until it
            // lands, a cancel or a crash leaves only the dotted partial file.
            fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                Error::io(dst, e)
            })?;
            sync_parent(dst);
            // The file arrived, so this is not a failed item - but an
            // attribute that was asked for and did not survive is something
            // the user is told rather than something they find later.
            for warning in attr_warnings {
                ctx.fail(&VfsPath::local(dst), warning);
            }
            Ok(bytes)
        }
        Err(err) => {
            // "must leave no half-written destination".
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Append one file to the end of another.
///
/// The one operation that cannot be made atomic: appending *is* mutating the
/// destination in place, so a cancel here leaves a destination that is longer
/// than it was. That is inherent to the choice, and the conflict dialog is
/// where it should be said.
///
/// It is also the one write that does not go through a temporary file and a
/// rename, so it is the one that has to check what it is opening. Two things
/// are refused before a byte is read:
///
/// * **A destination that is a symlink.** `O_APPEND` follows it, so the bytes
///   would land outside the directory the user chose - the design puts the
///   destination *in* that directory - and a link pointing back at the source
///   would make the reader and the writer the same file, which never reaches
///   EOF: the loop would run until the filesystem was full.
/// * **A destination that is the source.** Same file, same non-termination,
///   reached without a symlink through a hard link. [`same_path`] compares
///   `(dev, ino)`, which is what recognises it.
fn append_regular(src: &Path, dst: &Path, ctx: &mut JobContext) -> Result<u64> {
    let dst_meta = fs::symlink_metadata(dst).map_err(|e| Error::io(dst, e))?;
    if dst_meta.file_type().is_symlink() {
        return Err(Error::msg(format!(
            "{}: the destination is a symlink; appending would write through it",
            dst.display()
        )));
    }
    if same_path(src, dst) {
        return Err(Error::msg(format!(
            "{}: the source and the destination are the same file",
            dst.display()
        )));
    }
    let mut reader = fs::File::open(src).map_err(|e| Error::io(src, e))?;
    let mut writer = fs::OpenOptions::new()
        .append(true)
        .open(dst)
        .map_err(|e| Error::io(dst, e))?;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Error::Bare(err)),
        };
        writer.write_all(buf.get(..read).unwrap_or(&[]))?;
        total = total.saturating_add(read as u64);
        if !ctx.add_bytes(read as u64) {
            return Err(Error::Cancelled);
        }
    }
    writer.flush()?;
    Ok(total)
}

/// Recreate a symlink at the destination ("Symlinks are copied
/// as links by default").
/// Rule 1 of this file applies to a link exactly as it does to a file. `symlink`
/// refuses an existing destination, so a link that has to replace something is
/// made under the partial name and **renamed** over the destination:
///
/// * nothing is removed for a link that then fails to be created - the old
///   ordering unlinked the destination first, so an `EPERM` on a filesystem
///   without symlinks (FAT, exFAT) left the user with neither;
/// * `rename` refuses to replace a directory, so a file-shaped `Overwrite`
///   answer can never take a directory tree with it. That guard is doubled by
///   [`super::conflict::Plan::Refuse`], which never lets this be asked in the
///   first place.
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = fs::read_link(src).map_err(|e| Error::io(src, e))?;
    copy_symlink_target(&target, dst)
}

/// The half of [`copy_symlink`] that has the target already.
///
/// Split out for [`copy_link_via_vfs`], where the target came from an archive
/// member's contents rather than from `readlink(2)`, and where it has been
/// checked before it gets here. The ordering rules in [`copy_symlink`]'s own
/// documentation apply to both callers, which is why there is one of this.
fn copy_symlink_target(target: &Path, dst: &Path) -> Result<()> {
    if fs::symlink_metadata(dst).is_err() {
        return std::os::unix::fs::symlink(target, dst).map_err(|e| Error::io(dst, e));
    }
    let tmp = create_partial_symlink(target, dst)?;
    fs::rename(&tmp, dst).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(dst, e)
    })
}

/// Make the replacement link under a name nothing else holds.
fn create_partial_symlink(target: &Path, dst: &Path) -> Result<PathBuf> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..PARTIAL_ATTEMPTS {
        let tmp = partial_path(dst, next_unique());
        match std::os::unix::fs::symlink(target, &tmp) {
            Ok(()) => return Ok(tmp),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => return Err(Error::io(&tmp, err)),
        }
    }
    Err(match last {
        Some(err) => Error::io(dst, err),
        None => Error::msg(format!(
            "{}: no free name for the partial link",
            dst.display()
        )),
    })
}

/// Delete a path whatever it is: a file, a symlink, or a whole tree.
///
/// `remove_dir_all` does not follow symlinks - it removes the link - which is
/// what makes this safe to point at a tree containing them.
pub fn remove_any(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
    if meta.is_dir() {
        fs::remove_dir_all(path).map_err(|e| Error::io(path, e))
    } else {
        fs::remove_file(path).map_err(|e| Error::io(path, e))
    }
}

/// One item to copy: where it is now and where it should end up.
struct Item {
    src: PathBuf,
    dst: PathBuf,
}

/// The `Copy` and `Move` runner.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    run_with(vfs, spec, ctx, move_::plain_rename);
}

/// [`run`] with the rename injectable, so a test can force the cross-device
/// path on a machine that has one filesystem.
pub(crate) fn run_with(
    vfs: &dyn Vfs,
    spec: &JobSpec,
    ctx: &mut JobContext,
    rename: move_::RenameFn,
) {
    let moving = spec.kind == JobKind::Move;

    // the `Alt+F5`. A pack writes a container that does not exist
    // yet, in one call, rather than adding to one member by member - see
    // `ops::pack` for what that cost when it was the same code path.
    if let Some(into) = spec.options.pack {
        super::pack::run(vfs, spec, ctx, into);
        return;
    }

    let Some(dest) = spec.dest.as_ref() else {
        for source in &spec.sources {
            ctx.fail(source, "no destination was given");
        }
        return;
    };

    // "`Capabilities` is what the UI consults before offering an
    // operation - a read-only backend causes `F5` *into* it to be refused up
    // front with a clear message rather than failing halfway through a copy."
    // the design says the same about archives; the rule is about
    // writability, not about archives, so it is checked here for every
    // backend. Nothing has been read or written at this point.
    //
    // **Of the destination**, not of the `Vfs` handle: the handle a job is
    // given is the router, whose own `capabilities` can only speak for the
    // local filesystem. `capabilities_for` asks the backend that will really
    // service `dest`, which is what makes `F5` into a `.rar` a refusal here
    // rather than a failure discovered one `open_write` later.
    let caps = vfs.capabilities_for(dest);
    if let Err(err) = move_::check_writable(spec.kind, caps.writable, dest) {
        ctx.fail(dest, err);
        return;
    }

    // the `F5` **into** and **out of** an archive, and `Alt+F6`.
    // A side the kernel cannot be handed is serviced through the trait
    // instead - see `run_through_vfs`, which names no format and knows of no
    // archive. Everything below this point is the all-local engine, which is
    // where the overwhelming majority of copies stay.
    if dest.local_path().is_none() || spec.sources.iter().any(|s| s.local_path().is_none()) {
        vfs::run_through_vfs(vfs, spec, ctx, dest, moving);
        return;
    }

    let Some(dest_dir) = dest.local_path().map(Path::to_path_buf) else {
        ctx.fail(dest, "the destination is not a path on this machine");
        return;
    };

    // "refused up front with a clear message rather than failing
    // halfway through a copy". `Capabilities::writable` above answers that for
    // the *backend*; this answers it for the directory the bytes would land in,
    // which is the case a read-only mount, a mode-555 directory or a typo'd
    // path produces. One refusal naming the destination, before a single byte
    // has been read - not one failure per file discovered on the way through.
    //
    // Which directory that is: a batch always lands *inside* `dest_dir`, and a
    // single source may instead be renaming *to* `dest_dir`, in which case its
    // parent is what has to be writable (the same rule `rename_target` below
    // applies, minus the `stat` it has already cost us).
    let names_the_target = spec.sources.len() == 1 && !dest_dir.is_dir();
    let write_into = if names_the_target {
        dest_dir.parent().unwrap_or(dest_dir.as_path())
    } else {
        dest_dir.as_path()
    };
    if let Err(err) = probe_writable(write_into) {
        ctx.fail(dest, err);
        return;
    }

    // the design computes the selection statistics before the dialog opens,
    // so the totals are usually already known - but a job can also be built
    // without a dialog, and a progress bar with no total is a worse answer
    // than a metadata walk that costs milliseconds.
    let per_source = preflight(
        &spec.sources,
        &spec.options.walk(),
        &spec.options.file_mask,
        ctx,
    );
    let mut totals = TreeStats::ZERO;
    for stats in &per_source {
        totals.add(*stats);
    }
    ctx.start(totals.files, totals.bytes);

    // one dialog does both `F6` operations - "edit the *path*
    // and the file moves, edit the *filename* and it is renamed". That makes
    // `dest` a directory when it is one and the destination's own full path
    // when it is not, which is `mv`'s rule. It is decided **here** and not in
    // `dispatch`, because telling the two apart takes a `stat` and
    // the design forbids `dispatch` from touching the filesystem.
    //
    // Only for a single source: a batch has nothing to rename *to*, and the
    // parent has to exist already, so a typo'd path is refused rather than
    // silently turned into a file of that name.
    let rename_target = spec.sources.len() == 1
        && !dest_dir.is_dir()
        && dest_dir.parent().is_some_and(Path::is_dir);

    // A rename moves the whole tree at once, so it is only the right answer
    // when the whole tree is what was asked for: a file mask filters nothing
    // if no bytes move, and Verify has nothing to re-read.
    let may_rename = moving && mask::is_match_all(&spec.options.file_mask) && !spec.options.verify;

    let mut policy = Policy::new(spec.options.conflict);

    for (index, source) in spec.sources.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }
        let Some(src) = source.local_path().map(Path::to_path_buf) else {
            // Unreachable: a non-local source took `run_through_vfs` above.
            ctx.fail(source, "the source is not a path on this machine");
            continue;
        };
        let Some(name) = src.file_name().map(std::ffi::OsStr::to_os_string) else {
            ctx.fail(source, "a path with no file name cannot be copied");
            continue;
        };
        let dst = if rename_target {
            dest_dir.clone()
        } else {
            dest_dir.join(&name)
        };

        if same_path(&src, &dst) {
            ctx.fail(source, "the source and the destination are the same file");
            continue;
        }
        if is_ancestor(&src, &dest_dir) {
            // `cp -r a a/b` would otherwise recurse until the disk is full.
            ctx.fail(source, "a directory cannot be copied into itself");
            continue;
        }
        if !caps.has_directories && fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir()) {
            // An object store has prefixes, not directories.
            ctx.fail(
                source,
                format!("{dest} has no directories to copy one into"),
            );
            continue;
        }

        let dst_vfs = if rename_target {
            dest.clone()
        } else {
            dest.join(&name)
        };
        let target = match policy.resolve(&src, &dst, &dst_vfs, ctx) {
            Plan::Write(target) => target,
            Plan::Replace(target) => match remove_any(&target) {
                Ok(()) => target,
                Err(err) => {
                    ctx.fail(source, err);
                    continue;
                }
            },
            Plan::Append(target) => {
                // the dialog names the file being written. An
                // append writes a different file from the last one `copy_item`
                // announced, so it says so before a byte moves.
                let size = fs::symlink_metadata(&src).map(|m| m.len()).unwrap_or(0);
                ctx.set_file(&src.to_string_lossy(), size);
                match append_regular(&src, &target, ctx) {
                    Ok(_) => {
                        ctx.add_file();
                        if moving {
                            move_::delete_source(&src, source, ctx);
                        }
                    }
                    Err(err) => ctx.fail(source, err),
                }
                continue;
            }
            Plan::Skip => {
                ctx.add_skipped();
                continue;
            }
            Plan::Refuse(why) => {
                ctx.fail(source, why);
                continue;
            }
            Plan::Stop => break,
        };

        // a move is a rename when it can be, and degrades to
        // copy-then-delete when the rename crosses a device. Which it is comes
        // from *trying* it: only the kernel knows whether these two paths can
        // be linked, and a `st_dev` comparison is wrong about bind mounts,
        // overlay layers and subvolumes.
        if may_rename {
            let size = fs::symlink_metadata(&src).map(|m| m.len()).unwrap_or(0);
            ctx.set_file(&src.to_string_lossy(), size);
            if !move_::try_rename(&src, &target, rename).needs_copy() {
                let stats = per_source.get(index).copied().unwrap_or(TreeStats::ZERO);
                ctx.add_files(stats.files);
                for _ in 0..stats.dirs {
                    ctx.add_dir();
                }
                if fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
                    ctx.add_dir();
                }
                let _ = ctx.add_bytes(stats.bytes);
                continue;
            }
        }

        let outcome = copy_item(
            &Item {
                src: src.clone(),
                dst: target,
            },
            source,
            &spec.options,
            &mut policy,
            ctx,
        );

        // "with the delete happening only after a successful copy".
        // A cancelled, failed or partly skipped copy leaves
        // the source alone - see `move_::may_delete_source`.
        if moving && move_::may_delete_source(&outcome, ctx.cancelled()) {
            move_::delete_source(&src, source, ctx);
        }
        if outcome.stopped {
            break;
        }
    }
}

/// Can this process create a file in `dir`?
///
/// `access(2)` and `faccessat(2)` are the direct answers and both need `libc`,
/// which this milestone is not authorised to add; the mode bits on their own
/// are **not** an answer, because they say nothing about ACLs, supplementary
/// groups, a read-only mount, or being root. So the check is the operation
/// itself: create a file under a name nothing else uses and remove it again.
///
/// That probe is the only thing this function writes, it is removed
/// immediately, and it happens before anything at all has been read - which is
/// what makes the refusal above an *up-front* one rather than a discovery.
fn probe_writable(dir: &Path) -> Result<()> {
    let meta = fs::metadata(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(Error::msg(format!(
            "{}: the destination is not a directory",
            dir.display()
        )));
    }
    let probe = dir.join(format!(
        ".hcmd-write-probe-{}-{}",
        std::process::id(),
        next_unique()
    ));
    match fs::File::create(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(err) => Err(Error::msg(format!(
            "{}: this destination cannot be written ({err})",
            dir.display()
        ))),
    }
}

/// Keeps two partial files or write probes in the same process off each
/// other's name. The pid keeps them off another process's.
static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Copy one source - a file, a symlink or a whole tree - into `item.dst`.
fn copy_item(
    item: &Item,
    source_vfs: &VfsPath,
    options: &JobOptions,
    policy: &mut Policy,
    ctx: &mut JobContext,
) -> SourceOutcome {
    let mut outcome = SourceOutcome::default();
    // An explicit stack rather than recursion: a deep tree must not be able to
    // overflow the worker's stack, and the depth of a directory tree is under
    // the user's control, not ours.
    let mut stack: Vec<(PathBuf, PathBuf, VfsPath)> =
        vec![(item.src.clone(), item.dst.clone(), source_vfs.clone())];

    // Every directory this copy creates, stamped **after** the tree is written
    // rather than as each one is made. See `stamp_directories`.
    let mut made_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();

    // Unconditional cycle protection, as in `walk` - a bind mount needs no
    // symlink to build a loop. The destination's own directory is seeded, so a
    // link pointing back into it cannot be followed into a copy of the copy.
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    if let Some(parent) = item.dst.parent()
        && let Ok(meta) = fs::metadata(parent)
    {
        seen.insert((meta.dev(), meta.ino()));
    }

    'walk: while let Some((src, dst, src_vfs)) = stack.pop() {
        if ctx.cancelled() {
            outcome.stopped = true;
            break 'walk;
        }
        let meta = match fs::symlink_metadata(&src) {
            Ok(meta) => meta,
            Err(err) => {
                ctx.fail(&src_vfs, Error::io(&src, err));
                outcome.failed = true;
                continue;
            }
        };

        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The **path**, not the basename: the design requires enough parent
        // to be unambiguous, and the renderer crops it from the left.
        ctx.set_file(&src.to_string_lossy(), meta.len());

        if meta.file_type().is_symlink() && !options.follow_symlinks {
            if let Err(err) = copy_symlink(&src, &dst) {
                ctx.fail(&src_vfs, err);
                outcome.failed = true;
            } else {
                ctx.add_file();
            }
            continue;
        }

        // With `follow_symlinks`, what matters is what the link points at.
        let resolved = if options.follow_symlinks {
            fs::metadata(&src).ok()
        } else {
            Some(meta.clone())
        };
        let is_dir = resolved.as_ref().is_some_and(fs::Metadata::is_dir);

        if is_dir {
            if let Some(r) = resolved.as_ref()
                && !seen.insert((r.dev(), r.ino()))
            {
                // Already entered: a symlink loop, a bind mount, or a link
                // back into the destination. Counted rather than fatal, and
                // counted as skipped so a move keeps the source.
                ctx.add_skipped();
                outcome.skipped = true;
                continue;
            }
            if let Err(err) = ensure_dir(&dst) {
                ctx.fail(&src_vfs, err);
                outcome.failed = true;
                continue;
            }
            // The mode and the timestamps are **not** stamped here: a mode-555
            // source directory would then refuse its own children, and every
            // file written into a directory bumps that directory's mtime
            // afterwards anyway. Both are applied once the tree is written, in
            // `stamp_directories`, which is the order `cp -a` uses.
            made_dirs.push((src.clone(), dst.clone()));
            ctx.add_dir();

            let entries = match fs::read_dir(&src) {
                Ok(entries) => entries,
                Err(err) => {
                    ctx.fail(&src_vfs, Error::io(&src, err));
                    outcome.failed = true;
                    continue;
                }
            };
            for item in entries {
                // An entry `readdir` could not produce - an NFS `ESTALE`
                // part-way through a directory, a FUSE fault - is a file that
                // is *not* going to the destination, and swallowing it is how a
                // cross-device move comes to delete a source it never copied
                // (and `move_::may_delete_source`).
                let entry = match item {
                    Ok(entry) => entry,
                    Err(err) => {
                        ctx.fail(&src_vfs, Error::io(&src, err));
                        outcome.failed = true;
                        continue;
                    }
                };
                let child_name = entry.file_name();
                let child_src = src.join(&child_name);
                let child_dst = dst.join(&child_name);
                let child_vfs = src_vfs.join(&child_name);

                // "Only files of this type" filters files, never directories:
                // a mask of `*.rs` still has to descend into `src/` to find
                // any.
                let child_is_dir = match entry.file_type() {
                    Ok(file_type) => file_type.is_dir(),
                    // Guessing "file" here would apply the mask to a directory
                    // and silently drop a whole subtree from the copy - and
                    // then let a move delete it.
                    Err(err) => {
                        ctx.fail(&child_vfs, Error::io(&child_src, err));
                        outcome.failed = true;
                        continue;
                    }
                };
                if !child_is_dir
                    && !mask::matches(&options.file_mask, &child_name.to_string_lossy())
                {
                    ctx.add_skipped();
                    outcome.skipped = true;
                    continue;
                }

                match policy.resolve(&child_src, &child_dst, &child_vfs, ctx) {
                    Plan::Write(target) => stack.push((child_src, target, child_vfs)),
                    Plan::Replace(target) => match remove_any(&target) {
                        Ok(()) => stack.push((child_src, target, child_vfs)),
                        Err(err) => {
                            ctx.fail(&child_vfs, err);
                            outcome.failed = true;
                        }
                    },
                    Plan::Append(target) => {
                        let size = fs::symlink_metadata(&child_src)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        ctx.set_file(&child_src.to_string_lossy(), size);
                        match append_regular(&child_src, &target, ctx) {
                            Ok(_) => ctx.add_file(),
                            Err(err) => {
                                ctx.fail(&child_vfs, err);
                                outcome.failed = true;
                            }
                        }
                    }
                    Plan::Skip => {
                        ctx.add_skipped();
                        outcome.skipped = true;
                    }
                    Plan::Refuse(why) => {
                        ctx.fail(&child_vfs, why);
                        outcome.failed = true;
                    }
                    Plan::Stop => {
                        outcome.stopped = true;
                        break 'walk;
                    }
                }
            }
            continue;
        }

        // A plain file, or a followed symlink to one.
        if !mask::matches(&options.file_mask, &name) {
            ctx.add_skipped();
            outcome.skipped = true;
            continue;
        }
        match copy_regular(&src, &dst, options, ctx) {
            Ok(_) => ctx.add_file(),
            Err(Error::Cancelled) => {
                outcome.stopped = true;
                break 'walk;
            }
            Err(err) => {
                ctx.fail(&src_vfs, err);
                outcome.failed = true;
            }
        }
    }

    if options.preserve_attrs {
        stamp_directories(&made_dirs, ctx);
    }
    if ctx.cancelled() {
        outcome.stopped = true;
    }
    outcome
}

/// [`preserve`] from an [`Entry`] rather than from an `fs::Metadata`.
///
/// Best-effort, exactly as [`preserve`] is - and reporting exactly as
/// [`preserve`] does: a mode of `0` means the backend has no concept of one
/// and there is nothing to carry, a filesystem that will not take a timestamp
/// is not a reason to fail a copy whose bytes are already written, and what
/// could not be carried comes back for the caller to record. Extracting a
/// member used to land it at 0644 and say nothing at all.
fn preserve_entry(
    entry: &Entry,
    from_local: bool,
    dest_file: &fs::File,
    dest: &Path,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    if entry.mode != 0 {
        // **setuid, setgid and the sticky bit do not survive a copy from a
        // backend that is not this machine's filesystem.** The mode came out
        // of a container, and a container is written by whoever made it: an
        // archive whose member declares `04755` would otherwise unpack as a
        // setuid binary owned by the person unpacking it, which in any
        // directory another local user can reach is a shell running as them.
        // `Alt+F6` on a downloaded tarball is the whole
        // attack, and `preserve_attrs` is on by default.
        //
        // A local source keeps `cp -p` semantics: those bits are already on a
        // file this user can already run.
        let bits = if from_local {
            entry.mode & 0o7777
        } else {
            crate::vfs::untrusted_mode(entry.mode)
        };
        warnings.extend(stamp_mode(dest, bits));
    }
    if let Some(mtime) = entry.mtime {
        let times = fs::FileTimes::new().set_modified(mtime);
        warnings.extend(stamp_times(dest_file, times));
    }
    warnings
}

/// Carry mode and timestamps onto the directories a copy created, deepest
/// first, once nothing more is going to be written into them.
///
/// Two the design requirements meet here, and both need the *second* pass:
///
/// * **Mode.** Stamping a mode-555 source directory onto the destination as it
///   is created makes the destination refuse its own children, so every file
///   under a read-only directory failed with `EACCES` and the tree came out
///   empty. `cp -a` stamps directory modes on the way out for the same reason.
/// * **mtime.** Writing a child bumps its parent's mtime, so a stamp applied on
///   the way in is overwritten by the next file written. A tree copied with
///   "Preserve attributes" ticked otherwise came back with every directory
///   dated today, which is exactly what the checkbox says it will not do.
///
/// Best-effort, like [`preserve`], and reported like [`preserve`]: a
/// filesystem that cannot store a timestamp is not a reason to fail a copy
/// that has already written every byte, and it is not a reason to say nothing
/// either. A directory that came out at 0755 when the source was 0700 is a
/// permission change the user did not ask for.
fn stamp_directories(made: &[(PathBuf, PathBuf)], ctx: &mut JobContext) {
    // Deepest first, so a parent is stamped after the child whose creation
    // touched it.
    let mut order: Vec<&(PathBuf, PathBuf)> = made.iter().collect();
    order.sort_by_key(|(_, dst)| std::cmp::Reverse(dst.components().count()));
    for (src, dst) in order {
        let Ok(meta) = fs::symlink_metadata(src) else {
            continue;
        };
        if let Ok(modified) = meta.modified() {
            let accessed = meta.accessed().unwrap_or(modified);
            let times = fs::FileTimes::new()
                .set_modified(modified)
                .set_accessed(accessed);
            // A directory opens read-only, and `futimens` on that descriptor is
            // what stamps it - no `libc`, no `filetime`.
            match fs::File::open(dst) {
                Ok(handle) => {
                    if let Some(warning) = stamp_times(&handle, times) {
                        ctx.fail(&VfsPath::local(dst), warning);
                    }
                }
                Err(err) => ctx.fail(
                    &VfsPath::local(dst),
                    format!("copied, but its timestamps were not preserved: {err}"),
                ),
            }
        }
        // The mode goes last: it may take write permission away, and the times
        // above have already been applied.
        if let Some(warning) = stamp_mode(dst, meta.mode() & 0o7777) {
            ctx.fail(&VfsPath::local(dst), warning);
        }
    }
}

/// `create_dir`, tolerating a directory that is already there.
fn ensure_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
                Ok(())
            } else {
                Err(Error::io(path, err))
            }
        }
        Err(err) => Err(Error::io(path, err)),
    }
}

/// Are these two paths the same file? Compared by `(dev, ino)` where possible,
/// so `/tmp/a` and a bind-mounted `/mnt/a` are recognised as one file.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

/// Is `ancestor` at or above `path`? Guards "copy a directory into itself".
fn is_ancestor(ancestor: &Path, path: &Path) -> bool {
    let a = fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    let p = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    p.starts_with(&a)
}

/// Walk every source for its totals, so the progress dialog has a
/// denominator.
///
/// Cancellable like everything else: a user who hits `Esc` during the
/// pre-flight has cancelled the job, and the empty totals that come back mean
/// the run loop stops immediately afterwards.
/// The mask is part of it: the design wants `done / total` to be one rule,
/// and the copy applies "Only files of this type" per entry, so a pre-flight
/// that counted everything announced a denominator the job could never reach.
fn preflight(
    sources: &[VfsPath],
    options: &WalkOptions,
    file_mask: &str,
    ctx: &mut JobContext,
) -> Vec<TreeStats> {
    sources
        .iter()
        .map(|source| {
            if ctx.cancelled() {
                return TreeStats::ZERO;
            }
            let Some(local) = source.local_path() else {
                return TreeStats::ZERO;
            };
            let mut stats =
                walk_stats_filtered(local, options, &mut |_| !ctx.cancelled(), &mut |name| {
                    mask::matches(file_mask, name)
                })
                .stats;
            // The source itself counts: a walk reports what is *inside* a root.
            if fs::symlink_metadata(local)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                stats.add_dir();
            }
            stats
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
