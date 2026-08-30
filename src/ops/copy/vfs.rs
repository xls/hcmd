//! Copying where at least one side is not the local filesystem.
//!
//!
//! The general path, through the [`crate::vfs::Vfs`] trait, and the rule that
//! makes it a boundary rather than a size cut: **nothing here may use
//! `std::fs` to reach a source or a destination.** A remote directory, an
//! archive member and a search result are all paths this can be asked to copy,
//! and none of them is a file the operating system can open. Everything goes
//! through the trait, which is what makes the "archives are
//! directories" true of `F5` and not only of the panel.
//!
//! Where the two paths overlap is deliberate rather than duplicated: the local
//! runner beside this one is a fast path that knows both ends are on one
//! filesystem and may therefore use `std::fs` directly, and losing that would
//! cost an ordinary copy the trait's indirection on every chunk.

use super::*;

/// The `Copy`/`Move` runner for a batch where at least one side is not the
/// local filesystem.
///
/// the design asks for `F5` **out of** an archive and `F5` **into** one, and
/// why there is no archive-specific code here to do it: "archives, search
/// results and (later) remote filesystems are uniform to the panel". Nothing
/// below names a format, an extension or a container. It asks the [`Vfs`] it
/// was handed for a listing, for a reader and for a writer, and
/// `crate::vfs::VfsRouter` decides which backend answers - so the same
/// function extracts a `.tar.gz`, adds to a `.zip`, and (later) copies off a
/// remote.
///
/// The all-local path above is kept as it is rather than folded into this one.
/// It is not a duplicate: it does things this cannot, because it is talking to
/// a kernel rather than to a trait - `rename(2)` for a move, holes for a sparse
/// file, `(dev, ino)` cycle detection, `preserve` from a real `fs::Metadata`.
/// Routing every local copy through `Vfs::open_read` to share the code would
/// throw all four away for every ordinary `F5`, which is the overwhelming
/// majority of them.
pub(super) fn run_through_vfs(
    vfs: &dyn Vfs,
    spec: &JobSpec,
    ctx: &mut JobContext,
    dest: &VfsPath,
    moving: bool,
) {
    // A batch lands *inside* the destination; a single source may instead be
    // naming the destination itself. Same rule as the local path, asked of the
    // backend rather than of `Path::is_dir`.
    let dest_is_dir = matches!(
        vfs.stat(dest).map(|e| e.kind),
        Ok(EntryKind::Dir) | Ok(EntryKind::Symlink { to_dir: true })
    );
    let names_the_target = spec.sources.len() == 1 && !dest_is_dir;

    // "Counts are `done / total`, files and bytes both". A job
    // that never says what its total is has no batch bar, no percentage and no
    // ETA - and, because `JobStatus` only becomes `Running` when a `Started`
    // event arrives, sits in the queue view marked `Pending` for its whole
    // run. `Alt+F6` on a 2 GB `.tar.gz` did all four.
    //
    // The walk is the trait's, so the same code counts an archive's members
    // and a local tree; for an archive it is the index, which the copy is
    // about to need in full anyway.
    let totals = preflight_via_vfs(vfs, &spec.sources, &spec.options, ctx);
    ctx.start(totals.files, totals.bytes);

    let mut policy = Policy::new(spec.options.conflict);

    for source in &spec.sources {
        // a dropped connection **stops** the batch rather than
        // failing every remaining file with the same message.
        //
        if ctx.cancelled() || ctx.fatal() {
            break;
        }
        // A source with no file name is the *root* of its backend, which is
        // what `Alt+F6` hands over: the design says it "unpacks the archive
        // under the cursor to the other panel's directory", and the archive's
        // root is the archive. Its contents go into the destination itself
        // rather than into a directory named after a name it does not have.
        let target = match source.file_name() {
            Some(_) if names_the_target => dest.clone(),
            Some(name) => dest.join(&name),
            None => dest.clone(),
        };
        if source == &target {
            ctx.fail(source, "the source and the destination are the same file");
            continue;
        }
        if target.starts_with(source) {
            // `a.zip#/x` into `a.zip#/x/y` would recurse until the container
            // filled the disk. The local path refuses the same shape.
            ctx.fail(source, "a directory cannot be copied into itself");
            continue;
        }

        // The root's own conflict, before anything is read. `copy_tree_via_vfs`
        // asks about every *child* it discovers; the root was named by the
        // user rather than discovered, and the design does not exempt it -
        // `F5` of one member onto a destination that already has that name is
        // exactly the case the dialog exists for.
        let target = match vfs.stat(source) {
            Ok(entry) => match resolve_via_vfs(vfs, &mut policy, &entry, &target, ctx) {
                VfsPlan::Write(target) => target,
                VfsPlan::Append(target) => {
                    ctx.set_file(&source.to_string(), entry.size);
                    match append_one_via_vfs(vfs, source, &target, ctx) {
                        Ok(()) => ctx.add_file(),
                        Err(err) => ctx.fail(source, err),
                    }
                    continue;
                }
                VfsPlan::Skip => {
                    ctx.add_skipped();
                    continue;
                }
                VfsPlan::Refuse(why) => {
                    ctx.fail(source, why);
                    continue;
                }
                VfsPlan::Stop => break,
            },
            // Unreadable: let the walk report it once, with the path, rather
            // than reporting it twice from two places.
            Err(_) => target,
        };

        let outcome = copy_tree_via_vfs(vfs, source, &target, &spec.options, &mut policy, ctx);

        // "with the delete happening only after a successful copy",
        // and through the trait for the same reason as the
        // rest of it: the source may be a member of an archive.
        if moving
            && move_::may_delete_source(&outcome, ctx.cancelled())
            && let Err(err) = vfs.remove(source)
        {
            ctx.fail(source, err);
        }
        if outcome.stopped {
            break;
        }
    }
}

/// Count what a trait-side batch is about to copy, so the dialog has a
/// denominator.
///
/// Metadata only: it lists directories and reads sizes from the entries the
/// listing produced, and opens nothing. For an archive that means waiting for
/// as much of the index as the walk reaches, which is the same index every
/// subsequent read uses - `Index::wait_until_final` names `Alt+F6` as exactly
/// the caller that "reports a total before it starts".
///
/// A listing that fails is not reported here: the copy walks the same tree a
/// moment later and reports it once, with the path, rather than twice from two
/// places (the rule about never showing an unreadable thing as an
/// empty one is the copy's to keep).
fn preflight_via_vfs(
    vfs: &dyn Vfs,
    sources: &[VfsPath],
    options: &JobOptions,
    ctx: &JobContext,
) -> TreeStats {
    let mut totals = TreeStats::ZERO;
    let mut stack: Vec<VfsPath> = sources.iter().rev().cloned().collect();
    while let Some(at) = stack.pop() {
        if ctx.cancelled() {
            break;
        }
        let Ok(entry) = vfs.stat(&at) else {
            continue;
        };
        if !matches!(entry.kind, EntryKind::Dir) {
            if mask::matches(&options.file_mask, &entry.name) {
                totals.files = totals.files.saturating_add(1);
                totals.bytes = totals.bytes.saturating_add(entry.size);
            }
            continue;
        }
        totals.dirs = totals.dirs.saturating_add(1);
        let (children, _) = list_via_vfs(vfs, &at);
        for child in children {
            if matches!(child.kind, EntryKind::Dir) {
                stack.push(at.join(&child.name));
            } else if mask::matches(&options.file_mask, &child.name) {
                totals.files = totals.files.saturating_add(1);
                totals.bytes = totals.bytes.saturating_add(child.size);
            }
        }
    }
    totals
}

/// One source tree, copied through the trait.
///
/// An explicit stack rather than recursion, exactly as [`copy_item`] does it
/// and for the same reason: the depth of the tree is the archive's to choose,
/// not ours.
fn copy_tree_via_vfs(
    vfs: &dyn Vfs,
    src_root: &VfsPath,
    dst_root: &VfsPath,
    options: &JobOptions,
    policy: &mut Policy,
    ctx: &mut JobContext,
) -> SourceOutcome {
    let mut outcome = SourceOutcome::default();
    let mut stack: Vec<(VfsPath, VfsPath)> = vec![(src_root.clone(), dst_root.clone())];

    'walk: while let Some((src, dst)) = stack.pop() {
        // A cancel and a lost connection both stop the walk; only the first
        // reports itself as a cancel.
        if ctx.cancelled() || ctx.fatal() {
            outcome.stopped = true;
            break 'walk;
        }
        let entry = match vfs.stat(&src) {
            Ok(entry) => entry,
            Err(err) => {
                ctx.fail(&src, err);
                outcome.failed = true;
                continue;
            }
        };
        // the design wants enough of the path to be unambiguous, and the
        // renderer crops from the left.
        ctx.set_file(&src.to_string(), entry.size);

        if !matches!(entry.kind, EntryKind::Dir) {
            if !mask::matches(&options.file_mask, &entry.name) {
                ctx.add_skipped();
                outcome.skipped = true;
                continue;
            }
            match copy_one_via_vfs(vfs, &src, &dst, &entry, options, ctx) {
                Ok(()) => ctx.add_file(),
                Err(Error::Cancelled) => {
                    outcome.stopped = true;
                    break 'walk;
                }
                Err(err) => {
                    ctx.fail(&src, err);
                    outcome.failed = true;
                }
            }
            continue;
        }

        if let Err(err) = ensure_dir_via_vfs(vfs, &dst) {
            ctx.fail(&src, err);
            outcome.failed = true;
            continue;
        }
        ctx.add_dir();

        let (mut children, listing_error) = list_via_vfs(vfs, &src);
        // Pushed in reverse so they are *popped* in listing order. The stack
        // is what keeps the walk's depth the archive's business rather than
        // the call stack's, but a stack reverses whatever is pushed onto it,
        // and reading a compressed tar backwards is the one order that cannot
        // be served from an open decoder: every member would start the
        // decompression again (and `tar::cursors`). It also
        // makes the file named in the progress dialog move down the listing
        // the user is looking at instead of up it.
        children.reverse();
        if let Some(err) = listing_error {
            // A listing that failed part-way still copies what it produced -
            // the rule about never showing an unreadable thing as
            // an empty one, applied to an operation rather than to a panel -
            // but it is a failure, so a move will not delete the source.
            ctx.fail(&src, err);
            outcome.failed = true;
        }
        for child in children {
            if ctx.cancelled() || ctx.fatal() {
                outcome.stopped = true;
                break 'walk;
            }
            let child_is_dir = matches!(child.kind, EntryKind::Dir);
            // "Only files of this type" filters files, never directories: a
            // mask of `*.rs` still has to descend to find any.
            if !child_is_dir && !mask::matches(&options.file_mask, &child.name) {
                ctx.add_skipped();
                outcome.skipped = true;
                continue;
            }
            let child_src = src.join(&child.name);
            let child_dst = dst.join(&child.name);
            match resolve_via_vfs(vfs, policy, &child, &child_dst, ctx) {
                VfsPlan::Write(target) => stack.push((child_src, target)),
                VfsPlan::Append(target) => {
                    // the dialog names the file being written,
                    // and an append writes a different one from the last thing
                    // announced.
                    ctx.set_file(&child_src.to_string(), child.size);
                    match append_one_via_vfs(vfs, &child_src, &target, ctx) {
                        Ok(()) => ctx.add_file(),
                        Err(Error::Cancelled) => {
                            outcome.stopped = true;
                            break 'walk;
                        }
                        Err(err) => {
                            ctx.fail(&child_src, err);
                            outcome.failed = true;
                        }
                    }
                }
                VfsPlan::Skip => {
                    ctx.add_skipped();
                    outcome.skipped = true;
                }
                VfsPlan::Refuse(why) => {
                    ctx.fail(&child_src, why);
                    outcome.failed = true;
                }
                VfsPlan::Stop => {
                    outcome.stopped = true;
                    break 'walk;
                }
            }
        }
    }

    if ctx.cancelled() {
        outcome.stopped = true;
    }
    outcome
}

/// What to do about one destination that may already exist.
///
/// The trait-side counterpart of [`Plan`], which addresses a `Path`. Two
/// choices `Plan` offers are not offered here: `Append`, because appending is
/// a mutation in place and a member of a container has no such operation, and
/// `Replace`, because [`Vfs::remove`] is what a write over an existing entry
/// already does.
enum VfsPlan {
    /// Write here. The path may differ from the one asked about, which is what
    /// [`ConflictChoice::Rename`] means.
    Write(VfsPath),
    /// Append to what is already here. Only ever reached for a *local*
    /// destination: appending is a mutation in place, and a member of a
    /// container has no such operation.
    Append(VfsPath),
    /// Leave it alone and count the source as skipped.
    Skip,
    /// This destination cannot be written, and here is why.
    Refuse(String),
    /// The user cancelled; unwind the batch.
    Stop,
}

/// [`Policy::resolve`] for a destination the kernel cannot be asked about.
///
/// The same standing choice, the same "apply to all", the same dialog: only
/// the two `stat`s are the backend's rather than `lstat`'s.
fn resolve_via_vfs(
    vfs: &dyn Vfs,
    policy: &mut Policy,
    source: &Entry,
    dst: &VfsPath,
    ctx: &mut JobContext,
) -> VfsPlan {
    // A destination on this machine is `Policy`'s own question, and asking it
    // there rather than here is what keeps `free_name`, `Append` and the
    // "replace a mismatched type" rule identical whether the bytes came out of
    // a directory or out of a `.tar.gz`. Only the *source* facts differ, and
    // that is exactly what `resolve_from` takes.
    if let Some(local) = dst.local_path() {
        let facts = Facts {
            is_dir: matches!(source.kind, EntryKind::Dir),
            size: source.size,
            mtime: source.mtime,
        };
        return match policy.resolve_from(local, facts, local, dst, ctx) {
            Plan::Write(target) => VfsPlan::Write(VfsPath::local(target)),
            Plan::Replace(target) => match remove_any(&target) {
                Ok(()) => VfsPlan::Write(VfsPath::local(target)),
                Err(err) => VfsPlan::Refuse(err.to_string()),
            },
            Plan::Append(target) => VfsPlan::Append(VfsPath::local(target)),
            Plan::Skip => VfsPlan::Skip,
            Plan::Refuse(why) => VfsPlan::Refuse(why),
            Plan::Stop => VfsPlan::Stop,
        };
    }

    let Ok(existing) = vfs.stat(dst) else {
        return VfsPlan::Write(dst.clone());
    };
    let source_is_dir = matches!(source.kind, EntryKind::Dir);
    let dest_is_dir = matches!(existing.kind, EntryKind::Dir);
    // Two directories merge without asking, as they do everywhere else.
    if source_is_dir && dest_is_dir {
        return VfsPlan::Write(dst.clone());
    }

    let (choice, rename_to) = match policy.standing() {
        Some(standing) => (standing, None),
        None => {
            policy.note_asked();
            let request = ConflictRequest {
                source: dst.clone(),
                dest: dst.clone(),
                source_size: source.size,
                dest_size: existing.size,
                source_mtime: source.mtime,
                dest_mtime: existing.mtime,
                both_dirs: false,
                dest_is_dir,
            };
            match ctx.ask(request) {
                Some(Decision::Conflict {
                    choice,
                    rename_to,
                    apply_to_all,
                }) => {
                    if apply_to_all {
                        policy.adopt(choice);
                    }
                    (choice, rename_to)
                }
                Some(Decision::Cancel) | None => return VfsPlan::Stop,
            }
        }
    };

    let overwrite = |dst: &VfsPath| {
        if dest_is_dir != source_is_dir {
            // A file over a directory, or the reverse. The local path refuses
            // this rather than removing a tree on the strength of an answer
            // that described a file; so does this one.
            VfsPlan::Refuse(format!(
                "{dst} is {} and the source is not",
                if dest_is_dir { "a directory" } else { "a file" }
            ))
        } else {
            VfsPlan::Write(dst.clone())
        }
    };

    match choice {
        ConflictChoice::Overwrite => overwrite(dst),
        ConflictChoice::Skip => VfsPlan::Skip,
        ConflictChoice::Append => VfsPlan::Refuse(format!(
            "{dst}: this backend replaces what is there rather than appending to it"
        )),
        ConflictChoice::Rename => {
            let base = match dst.parent() {
                Some(parent) => parent,
                None => return VfsPlan::Refuse(format!("{dst} has nowhere to be renamed into")),
            };
            match rename_to.as_deref().filter(|n| !n.is_empty()) {
                Some(name) => VfsPlan::Write(base.join(name)),
                // No name to use, and no directory to scan for a free one that
                // is not another round-trip through the backend. Skipping
                // destroys nothing, which is the rule everywhere else here.
                None => VfsPlan::Skip,
            }
        }
        ConflictChoice::OverwriteIfNewer => match (source.mtime, existing.mtime) {
            (Some(s), Some(d)) if s > d => overwrite(dst),
            _ => VfsPlan::Skip,
        },
        ConflictChoice::OverwriteIfDifferentSize => {
            if source.size == existing.size {
                VfsPlan::Skip
            } else {
                overwrite(dst)
            }
        }
    }
}

/// Every child of `path`, and the reason the listing is not the whole truth if
/// it is not.
///
/// [`Vfs::read_dir`] streams, and every backend sends the `..` row first
/// because it is navigation rather than content; it is dropped here, because a
/// copy of `..` is a copy of the parent.
///
/// A row whose name is not a plain file name is dropped too, and **says so**:
/// every caller joins the name onto a destination
/// ([`copy_tree_via_vfs`]'s `dst.join(&child.name)`), `VfsPath::join` joins a
/// name containing a separator as written, and `Path::join` with an absolute
/// argument discards the base entirely - so a listing that names
/// `../../.bashrc` would write outside the destination the user chose. The
/// backends refuse such a name at the wire (`crate::vfs::is_plain_name` is
/// called by `RemoteFs::read_dir` and by both FTP listing parsers); this is
/// the check on the side that does the joining, so no future backend can
/// reintroduce Zip Slip by forgetting it.
pub fn list_via_vfs(vfs: &dyn Vfs, path: &VfsPath) -> (Vec<Entry>, Option<Error>) {
    let mut rx = vfs.read_dir(path);
    let mut rows = Vec::new();
    let mut failure = None;
    let mut refused = 0usize;
    while let Some(item) = rx.blocking_recv() {
        match item {
            Ok(entry) if entry.is_parent => {}
            Ok(entry) if !crate::vfs::is_plain_name(&entry.name) => {
                refused = refused.saturating_add(1);
            }
            Ok(entry) => rows.push(entry),
            // The last word wins: a backend reports the reason after the rows
            // it did produce.
            Err(err) => failure = Some(err),
        }
    }
    if refused > 0 && failure.is_none() {
        // Not silent: a listing that was edited on the way in is a failure of
        // that directory, so a move will not delete the source. The offending
        // name is not quoted back - it was chosen by whoever answered the
        // listing, and this text goes to the failure summary.
        failure = Some(Error::msg(format!(
            "{refused} entries were refused: a listing name that is a path and not a file name"
        )));
    }
    (rows, failure)
}

/// `mkdir -p` through the trait.
///
/// [`Vfs::create_dir`] is one level by contract, so a destination two levels
/// below anything that exists is built one level at a time - the same walk
/// `ops::mkdir` does, for the same reason: a failure names the level that
/// actually failed.
fn ensure_dir_via_vfs(vfs: &dyn Vfs, dir: &VfsPath) -> Result<()> {
    if matches!(vfs.stat(dir).map(|e| e.kind), Ok(EntryKind::Dir)) {
        return Ok(());
    }
    let mut missing = Vec::new();
    let mut at = dir.clone();
    loop {
        if matches!(vfs.stat(&at).map(|e| e.kind), Ok(EntryKind::Dir)) {
            break;
        }
        let Some(parent) = at.parent() else {
            break;
        };
        missing.push(at.clone());
        at = parent;
    }
    for level in missing.iter().rev() {
        if matches!(vfs.stat(level).map(|e| e.kind), Ok(EntryKind::Dir)) {
            continue;
        }
        vfs.create_dir(level)?;
    }
    Ok(())
}

/// One file, copied through the trait.
///
/// The **destination** decides how it is written, and the difference is not
/// cosmetic - it is the "must leave no half-written destination",
/// answered in whichever way the destination can answer it:
///
/// * a local destination gets [`copy_regular_from`]'s dotted partial file and
///   its rename, so a cancel leaves the partial and never the destination;
/// * a destination inside an archive gets [`Vfs::open_write`], whose writer
///   buffers into a session temp file and commits on `flush` - so a writer
///   dropped without one changes nothing at all.
///
/// Either way the **source** is [`Vfs::open_read`], which for an archive
/// member streams through a pipe under the byte caps.
fn copy_one_via_vfs(
    vfs: &dyn Vfs,
    src: &VfsPath,
    dst: &VfsPath,
    entry: &Entry,
    options: &JobOptions,
    ctx: &mut JobContext,
) -> Result<()> {
    // "Symlinks are copied as links by default". A member read
    // through the trait carries its target as its *contents*, which is what
    // makes both halves of the design answerable here - and answering them
    // is not optional: writing the target's text into a regular file loses the
    // link, and writing the link without checking it is Zip Slip's second
    // spelling. See [`copy_link_via_vfs`].
    if matches!(entry.kind, EntryKind::Symlink { .. }) && src.local_path().is_none() {
        return copy_link_via_vfs(vfs, src, dst);
    }

    let mut reader = vfs.open_read(src)?;

    if let Some(local) = dst.local_path() {
        let (tmp, mut writer) = create_partial(local)?;
        let outcome = (|| -> Result<u64> {
            // Remote to local: the read is the half that crosses the
            // network, so the source's chunk is what sizes it (I12).
            let chunk = crate::ops::chunk_size(&vfs.capabilities_for(src));
            let bytes = copy_stream_from(&mut reader, &mut writer, ctx, chunk)?;
            writer.flush()?;
            if options.preserve_attrs {
                preserve_entry(entry, src.local_path().is_some(), &writer, &tmp);
            }
            // Durable before the rename, for the reason `commit_partial`
            // gives: a file pulled off a remote and left in the page cache is
            // still reported as copied if the close fails.
            super::commit_partial(writer, &tmp)?;
            Ok(bytes)
        })();
        return match outcome {
            Ok(_) => fs::rename(&tmp, local)
                .map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    Error::io(local, e)
                })
                .inspect(|()| super::sync_parent(local)),
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                Err(err)
            }
        };
    }

    let mut writer = vfs.open_write(dst)?;
    // rule 3: the chunk comes from `Capabilities`, not from a
    // constant. A `LatencyClass::Network` backend copies in larger chunks than
    // a local one, which is the difference between a round trip per 256 KiB
    // and one per megabyte.
    let chunk = crate::ops::chunk_size(&vfs.capabilities_for(dst));
    let mut buf = vec![0u8; chunk];
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
        if !ctx.add_bytes(read as u64) {
            return Err(Error::Cancelled);
        }
    }
    // The commit, for a backend whose `flush` is one. Its failure is reported
    // here, where a caller is checking, rather than by a `drop` nobody can.
    writer.flush()?;
    Ok(())
}

/// Copy a symlink that is not on this machine as a link.
///
/// The target comes from [`Vfs::read_link`], which is the backend's answer and
/// not this function's guess. That matters: a `.zip` and a `.7z` store a
/// link's target as the member's *contents*, a tar stores it in the header's
/// link-name field and gives the member no contents at all, and a remote
/// filesystem has a `readlink` of its own. Reading the contents - which is
/// what this used to do - is right for one of those three, and every symlink
/// in every `.tar`, `.tar.gz`, `.tar.bz2`, `.tar.xz` and `.tar.zst` failed
/// with "an empty link target". Ordinary source tarballs contain symlinks;
/// this was not a hostile-archive case.
///
/// **Whether the target is allowed is the backend's question too.** the rule
/// that an extracted link may not point out of its destination is the
/// archive's threat model, and `ArchiveFs::read_link` enforces it before the
/// target is handed over - so there is no rule here for a later backend to
/// forget, and `ops` no longer reaches into `vfs::archive` to borrow one.
///
/// `options.follow_symlinks` is deliberately not consulted. Following a link
/// means resolving it, and a link inside an archive resolves inside the
/// archive - to a member that may not be there at all. Copying it as a link is
/// the conservative answer, and it is the one that cannot be tricked.
fn copy_link_via_vfs(vfs: &dyn Vfs, src: &VfsPath, dst: &VfsPath) -> Result<()> {
    let target = vfs.read_link(src)?;

    let Some(local) = dst.local_path() else {
        return Err(Error::msg(format!(
            "{dst}: this backend cannot be given a symbolic link"
        )));
    };
    copy_symlink_target(Path::new(&target), local)
}

/// Append a source read through the trait to the end of a local file.
///
/// The one write here that is not atomic from the destination's point of view,
/// exactly as [`append_regular`] is and for the same reason: appending *is*
/// mutating in place, and the conflict dialog is where that is said. The
/// destination is refused if it is a symlink, because `O_APPEND` follows one
/// and the bytes would land outside the directory the user chose.
fn append_one_via_vfs(
    vfs: &dyn Vfs,
    src: &VfsPath,
    dst: &VfsPath,
    ctx: &mut JobContext,
) -> Result<()> {
    let Some(local) = dst.local_path() else {
        return Err(Error::msg(format!(
            "{dst}: this backend replaces what is there rather than appending to it"
        )));
    };
    let meta = fs::symlink_metadata(local).map_err(|e| Error::io(local, e))?;
    if meta.file_type().is_symlink() {
        return Err(Error::msg(format!(
            "{dst} is a symbolic link; appending would write outside the destination"
        )));
    }
    let mut reader = vfs.open_read(src)?;
    let mut writer = fs::OpenOptions::new()
        .append(true)
        .open(local)
        .map_err(|e| Error::io(local, e))?;
    // The **source's** capabilities here, not the destination's: the
    // destination is local by the guard above, and the read is the half that
    // crosses the network (I12).
    let chunk = crate::ops::chunk_size(&vfs.capabilities_for(src));
    let mut buf = vec![0u8; chunk];
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
        if !ctx.add_bytes(read as u64) {
            return Err(Error::Cancelled);
        }
    }
    writer.flush()?;
    Ok(())
}
