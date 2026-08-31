//! Turning `F5`, `F6`, `F7`, `F8` and `Alt+F5` into jobs.
//!
//! Every one of these follows the same two-step shape, and the shape is the
//! reason they are one module: a keystroke opens a dialog and captures what it
//! is about, and the dialog's answer turns that into exactly one
//! [`crate::ops::JobSpec`]. Nothing here runs an operation, because
//! [`super::dispatch`] may not touch the filesystem; the spec is queued and
//! the event loop starts it.
//!
//! The operands are captured when the prompt is built rather than read off the
//! panel again when it is answered, which is what
//! [`crate::app::jobs::draft::JobDraft`] holds and why.

use crate::app::App;
use crate::dialog::{ConfirmDialog, InputDialog};
use crate::input::DialogId;
use crate::ops::mask::is_match_all;
use crate::ops::{JobId, JobKind, JobOptions, JobSpec};
use crate::ui::dialog::{CopyMoveDialog, JobAction, QueueDialog};
use crate::vfs::VfsPath;

/// The mask half of the pre-filled target (the `/srv/media/*.*`).
const DEFAULT_TARGET_MASK: &str = "*.*";

/// Turn the copy/move dialog's answer into a [`JobSpec`].
///
/// The target field holds a path **and** a mask - `/srv/media/*.*`. The path
/// half is the destination; the mask half is a *rename template*, and nothing
/// in `ops` implements one (`JobOptions::file_mask`
/// filters, it does not rename). So a mask that means "everything" is dropped
/// and anything else is **refused** rather than silently ignored - a `*.bak`
/// that quietly copied the names unchanged is the worst of the three answers.
///
/// A tail with no wildcard in it is left on the destination: the design's
/// "edit the filename and it is renamed" is decided by
/// [`crate::ops::copy::run`], on the worker, because telling a new name from an
/// existing directory takes a `stat` and `dispatch` may not do one.
pub(super) fn copy_move_accepted(app: &mut App, answer: &crate::dialog::CopyMoveAnswer) {
    // The list the dialog's title and statistics were computed from, not
    // whatever the panel holds now: a listing that arrived while the dialog was
    // open re-sorts `entries` under a cursor that is a raw index, and the
    // operation would then move a file the dialog never named (the design -
    // these are "the last chance to notice a mistake", which they only are if
    // they describe what happens).
    let sources = app.draft.take_sources();
    if sources.is_empty() {
        app.draft.op = None;
        app.message = Some("nothing to operate on".to_string());
        return;
    }
    let base = app.active_panel().active_tab().path.clone();

    let (dir, tail) = split_target(answer.target.trim());
    if tail.contains(['*', '?']) && !is_match_all(&tail) {
        app.message = Some(format!(
            // No milestone is named because the design assigns this one to
            // none: it belongs to v0.2 and did not land. Inventing a release
            // for it would be the kind of sentence that ages into a lie.
            "{tail}: a renaming target mask is not implemented yet - \
             use a plain directory, or *.* to keep the names"
        ));
        return;
    }
    // A relative target resolves against **the panel**, exactly as `Ctrl+G`
    // resolves one: the process's working directory is
    // wherever the program was launched from and has nothing to do with either
    // panel, so resolving there writes somewhere the user never named. An empty
    // directory half is the bare-filename case - `F6` `newname` - and means the
    // panel's own directory, which is what makes it a rename in place.
    // The seeded destination, whole, when the field still says what it said
    // when the dialog opened. This is what makes `F5` into an archive add a
    // member rather than create a local file called `bundle.zip#outside.txt`:
    // `…/bundle.zip#/` is a two-segment `VfsPath` and a line of text is not
    // (and [`crate::app::jobs::draft::JobDraft::target`]).
    let seeded = app.draft.target.take().filter(|path| {
        let typed = answer.target.trim();
        path.join(DEFAULT_TARGET_MASK).to_string() == typed || path.to_string() == typed
    });
    let mut dest = match seeded {
        Some(path) => path,
        None => match crate::panel::goto::expand(&dir, base.local_path()) {
            Ok(path) => VfsPath::local(path),
            Err(why) => {
                app.draft.op = None;
                app.message = Some(why);
                return;
            }
        },
    };
    if !tail.contains(['*', '?']) && !tail.is_empty() {
        dest = dest.join(&tail);
    }

    let kind = match app.draft.op.take() {
        Some(kind @ (JobKind::Copy | JobKind::Move)) => kind,
        _ => JobKind::Copy,
    };
    let mut options = JobOptions::from_config(&app.config.ops);
    options.preserve_attrs = answer.preserve_attrs;
    options.verify = answer.verify;
    options.file_mask = answer.file_mask.clone();
    options.conflict = answer.conflict;

    // "Only files of this type", remembered for `+ F8`.
    app.masks.offer(answer.file_mask.trim());

    let spec = JobSpec::new(kind, sources, Some(dest)).with_options(options);
    // `F2 Queue`: "append to the background queue instead of starting now".
    // It really does wait - `App::queue_job` puts it behind
    // the queue's admission control rather than merely hiding its dialog.
    if answer.queue {
        app.queue_job(spec);
    } else {
        app.request_job(spec);
    }
}

/// The `Alt+F5` dialog came back with every answer.
///
/// It resolves the target exactly as the copy dialog resolves its own - against
/// **the panel**, not against the process's working directory - and then queues
/// the request. Nothing is created here: `dispatch` may not touch the
/// filesystem, and creating the container is
/// precisely that.
pub(super) fn pack_accepted(app: &mut App, answer: &crate::dialog::PackAnswer) {
    let sources = app.draft.take_sources();
    if sources.is_empty() {
        app.message = Some("nothing to pack".to_string());
        return;
    }
    let base = app.active_panel().active_tab().path.clone();
    let (dir, name) = split_target(answer.target.trim());
    if name.is_empty() {
        app.message = Some("an archive needs a name".to_string());
        return;
    }
    let container = match crate::panel::goto::expand(&dir, base.local_path()) {
        Ok(path) => VfsPath::local(path).join(&name),
        Err(why) => {
            app.message = Some(why);
            return;
        }
    };
    app.request_pack(crate::app::PackRequest {
        container,
        format: answer.format,
        level: answer.level,
        sources,
        move_sources: answer.move_sources,
    });
}

/// `+ F7` in the copy/move dialog: create a directory **for the target** and
/// point the target field at it.
///
/// The copy dialog is still on the stack underneath - the prompt was pushed on
/// top of it - so the field is rewritten in place, which is the half that makes
/// the button worth pressing: without it the user creates `2026` and then still
/// has to type it into the field, and if they do not, `OK` copies into the old
/// directory.
pub(super) fn mkdir_for_target(app: &mut App, name: &str) {
    let base = app.active_panel().active_tab().path.clone();
    let Some(target) = app
        .top_dialog()
        .and_then(crate::dialog::Dialog::as_any)
        .and_then(|any| any.downcast_ref::<CopyMoveDialog>())
        .map(|dialog| dialog.target().to_string())
    else {
        app.message = Some("the copy dialog is no longer open".to_string());
        return;
    };
    let (dir, tail) = split_target(target.trim());
    let parent = match crate::panel::goto::expand(&dir, base.local_path()) {
        Ok(path) => path,
        Err(why) => {
            app.message = Some(why);
            return;
        }
    };
    let trimmed = name.trim().trim_matches('/');
    if trimmed.is_empty() {
        app.message = Some("nothing to create".to_string());
        return;
    }
    let created = parent.join(trimmed);
    app.request_job(JobSpec::new(
        JobKind::Mkdir,
        Vec::new(),
        Some(VfsPath::local(&created)),
    ));

    // The field now names what is being created, mask and all, so `OK` copies
    // into it. The job is queued rather than done, and a `mkdir` that fails
    // reports on the status line - the copy would then refuse the destination
    // up front, which is the same answer one step later.
    let mut next = created.to_string_lossy().into_owned();
    if !tail.is_empty() {
        next.push('/');
        next.push_str(&tail);
    }
    if let Some(dialog) = app
        .top_dialog_mut()
        .and_then(|d| d.as_any_mut())
        .and_then(|any| any.downcast_mut::<CopyMoveDialog>())
    {
        dialog.set_target(next);
    }
}

/// `F2`'s answer: rename in place.
///
/// A `Move` job whose destination is the panel's own directory plus the new
/// name, which [`crate::ops::copy::run`] turns into one `rename(2)` - the same
/// engine as `F6`, so a cross-device rename, a conflict and a failure all
/// behave the way they do everywhere else.
pub(super) fn rename_accepted(app: &mut App, name: &str) {
    let sources = app.draft.take_sources();
    let Some(source) = sources.into_iter().next() else {
        app.message = Some("nothing to rename".to_string());
        return;
    };
    let base = app.active_panel().active_tab().path.clone();
    let dest = base.join(name);
    let side = app.active_side;
    // Land the cursor on the renamed entry once the panel re-reads.
    app.panel_mut(side).active_tab_mut().pending_select = Some(name.to_string());
    let spec = JobSpec::new(JobKind::Move, vec![source], Some(dest))
        .with_options(JobOptions::from_config(&app.config.ops));
    app.request_job(spec);
}

/// `F2` / `Shift+F6`: the rename dialog.
///
/// The entry **under the cursor**, marks or no marks: this dialog shows one
/// filename and renames one file. Renaming a marked set is `Ctrl+M`, which
/// the design puts in v0.6.
pub(super) fn open_rename(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing to rename".to_string());
        return;
    };
    let name = entry.name.clone();
    let is_dir = entry.is_dir();
    // The other names in this directory, so the dialog can refuse a collision
    // itself - it reads no filesystem of its own.
    let siblings: Vec<String> = tab
        .entries
        .iter()
        .filter(|e| !e.is_parent && e.name != name)
        .map(|e| e.name.clone())
        .collect();
    let Some(path) = tab.current_path() else {
        app.message = Some("nothing to rename".to_string());
        return;
    };
    app.draft.sources = vec![path];
    app.push_dialog(Box::new(crate::ui::dialog::RenameDialog::new(
        name, is_dir, siblings,
    )));
}

/// `F4` - edit the entry under the cursor.
///
/// The keystroke only *plans*. Leaving the alternate screen, spawning an editor
/// on the real stdio, waiting for it and putting the terminal back is a
/// terminal-and-filesystem operation and `dispatch` may do none of those,
/// so the request goes on [`crate::ops::open::Handoff`]
/// and [`crate::ops::editor::service`] performs it one turn later - exactly the
/// way a directory read is queued and then serviced.
///
/// The cursor stays on the edited file through
/// [`crate::panel::Tab::pending_select`], which the panel already resolves as a
/// re-read's batches arrive; the re-read itself is `ExternalCommand::follow`.
///
/// Refusals are up front and say why: nothing under the cursor, a directory, a
/// backend that is not the local filesystem (v0.5).
pub fn open_editor(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing to edit".to_string());
        return;
    };
    if entry.is_dir() {
        app.message = Some(format!("{}: a directory cannot be edited", entry.name));
        return;
    }
    let name = entry.name.clone();
    let size = entry.size;
    let Some(path) = tab.current_path() else {
        app.message = Some("nothing to edit".to_string());
        return;
    };
    // A big file in a line editor is slow to open and easy to open by
    // accident. The warning is a confirmation, not a refusal - the file may
    // genuinely need editing - and it is skipped once it has been answered, so
    // `F4 Enter` opens the file the second time without asking again.
    let warn = app.config.editor.warn_above.bytes();
    if warn > 0 && size > warn && !app.editor_size_confirmed {
        let human = crate::panel::format::human_size(size);
        let over = crate::panel::format::human_size(warn);
        app.editor_size_pending = Some(path.clone());
        app.push_dialog(Box::new(
            crate::dialog::ConfirmDialog::new(
                DialogId::ConfirmEditLarge,
                format!("Edit {name}?"),
                vec![
                    format!("{name} is {human}B, over the {over}B warning size."),
                    "A large file can be slow to open in an editor.".to_string(),
                ],
            )
            .with_buttons("Edit", "Cancel"),
        ));
        return;
    }
    app.editor_size_confirmed = false;
    // The *path's own* backend, not `app.vfs`: a panel showing search results
    // over local files is editable, and an archive member is not.
    let caps = path.backend().capabilities();
    match crate::ops::editor::plan(&app.config.editor, &path, caps, None, app.active_side) {
        Ok(command) => {
            let side = app.active_side;
            app.panel_mut(side).active_tab_mut().pending_select = Some(name);
            // Nothing to create: `F4` edits a file that is already there, and
            // clearing the operands of whatever queued them last is what stops
            // `service` from creating a stale one.
            app.draft.sources.clear();
            app.handoff.external = Some(command);
        }
        Err(why) => app.message = Some(why.to_string()),
    }
}

/// `Shift+F4` - "prompts for a name, creates the file, then opens the editor".
///
///
/// The refusal is checked *before* the prompt rather than after it: being asked
/// for a name and only then told that this backend cannot be written to is the
/// shape of failure the design opens by designing out.
pub(super) fn open_edit_new(app: &mut App) {
    use crate::ops::editor::Refusal;

    let base = app.active_panel().active_tab().path.clone();
    if base.local_path().is_none() {
        app.message = Some(Refusal::NotLocal.to_string());
        return;
    }
    // The same answer `open_mkdir` below is gated on, and for the same reason:
    // these two keys ask the identical question of the identical directory.
    // They used to ask it of different sources - this one of the static
    // per-kind guess, that one of the panel's cached answer - so a `.zip`
    // root, which is writable and whose kind is not, offered one key and
    // refused the other.
    if !app.active_panel().active_tab().caps.writable {
        app.message = Some(Refusal::ReadOnly.to_string());
        return;
    }
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::EditNew,
        "Edit new file",
        "New file name:",
        "",
    )));
}

/// The `Shift+F4` prompt came back with a name.
///
/// The file itself is created by [`crate::ops::editor::service`], from
/// [`crate::app::jobs::draft::JobDraft::sources`] - the field a keystroke already uses to carry the
/// operands of the operation it is queueing.
pub(super) fn edit_new_accepted(app: &mut App, name: &str) {
    let name = name.trim();
    if let Err(why) = crate::ops::editor::validate_name(name) {
        app.message = Some(why.to_string());
        return;
    }
    let base = app.active_panel().active_tab().path.clone();
    let target = base.join(name);
    let caps = target.backend().capabilities();
    match crate::ops::editor::plan(&app.config.editor, &target, caps, None, app.active_side) {
        Ok(command) => {
            // `sub/notes.txt` creates one row in *this* listing, `sub`, and
            // that is what the cursor can land on - the same rule as `F7`.
            let first = name.trim_matches('/').split('/').next().unwrap_or(name);
            if !first.is_empty() {
                let side = app.active_side;
                app.panel_mut(side).active_tab_mut().pending_select = Some(first.to_string());
            }
            app.draft.sources = vec![target];
            app.handoff.external = Some(command);
        }
        Err(why) => app.message = Some(why.to_string()),
    }
}

/// Split `/srv/media/*.*` into its directory and its tail.
///
/// A target with no separator at all is a tail under the panel's own
/// directory, which is how `F6` `newname` renames in place: the empty
/// directory half is resolved against the panel by
/// [`crate::panel::goto::expand`].
fn split_target(target: &str) -> (String, String) {
    match target.rsplit_once('/') {
        // `/photo.jpg` - the tail is under the root, not under "".
        Some(("", tail)) => ("/".to_string(), tail.to_string()),
        Some((dir, tail)) => (dir.to_string(), tail.to_string()),
        None => (String::new(), target.to_string()),
    }
}

/// The `F8` / `Shift+F8` confirmation came back `Yes`.
///
/// It deletes **exactly what the prompt named**. The list was captured when the
/// confirmation was built and is not re-derived here: a re-read landing while
/// the prompt is up rebuilds and re-sorts the listing under a cursor that is a
/// raw index, and `Shift+F8` unlinking the wrong file is not recoverable.
pub(super) fn delete_confirmed(app: &mut App) {
    let sources = app.draft.take_sources();
    let split = app.draft.trash_split.take();
    let kind = match app.draft.op.take() {
        Some(kind @ JobKind::Delete { .. }) => kind,
        // Nothing but `F8` opens this confirmation, and the safe reading of a
        // lost flag is the recoverable one.
        _ => JobKind::Delete { trash: true },
    };
    if sources.is_empty() {
        app.message = Some("nothing to delete".to_string());
        return;
    }
    let options = JobOptions::from_config(&app.config.ops);

    // the mixed selection: one decision covers the batch, and the
    // batch is then carried out honestly - what the trash can take goes to the
    // trash, and what has nowhere to go is unlinked, which is what the prompt
    // said would happen. Two jobs rather than one because `JobKind::Delete`
    // carries a single `trash` flag; the queue runs them in order.
    if let Some(split) = split.filter(crate::ops::delete::TrashSplit::is_mixed) {
        app.request_job(
            JobSpec::new(JobKind::Delete { trash: true }, split.trashable, None)
                .with_options(options.clone()),
        );
        app.request_job(
            JobSpec::new(JobKind::Delete { trash: false }, split.untrashable, None)
                .with_options(options),
        );
        return;
    }

    let spec = JobSpec::new(kind, sources, None).with_options(options);
    app.request_job(spec);
}

/// `F5`, `Shift+F5` and `F6`: the copy / move dialog.
///
/// The target is pre-filled with **the other panel's path plus a file mask** -
/// `/srv/media/*.*` - with the mask half preselected, which is what makes
/// "copy these as `*.bak`" two keystrokes. `Shift+F5` fills in the *active*
/// panel's own directory instead, because copying within one directory is what
/// it is for.
///
/// The statistics come from [`crate::ops::walk::stats_of`] over the same names
/// the job will be given, so the line and the operation can never describe
/// different things - the design calls them "the last chance to notice a
/// mistake", which they only are if they are true.
pub(super) fn open_copy_move(app: &mut App, kind: JobKind, same_dir: bool) {
    // Rows and their real addresses, not names joined to the panel's path:
    // on a virtual listing every row lives somewhere else.
    let rows = app.active_panel().active_tab().operand_rows();
    let sources = app.active_panel().active_tab().operand_paths();
    if sources.is_empty() {
        app.message = Some("nothing to operate on".to_string());
        return;
    }
    let target_dir = if same_dir {
        app.active_panel().active_tab().path.clone()
    } else {
        app.panel(app.active_side.other()).active_tab().path.clone()
    };

    // refused **before the question**. Asking "Delete permanently
    // - 3 files" about a `.rar`, and only then reporting one failure per file
    // because the backend was never writable, is precisely the "failing
    // halfway through" the trait's `Capabilities` exist to prevent. The answer
    // is the one the panel cached when it listed this directory, so no
    // filesystem is touched here.
    //
    // **It is the source panel being asked about, so only a move asks.** A
    // move deletes its sources once the copy half has succeeded
    // and so needs a source it may delete from; a copy takes
    // nothing away from where it came from. Applying the test to `F5` as well
    // made every read-only backend unreadable through the one key that reads
    // out of it: `F5` out of a `.rar` (the read-only row) and out
    // of a disk image (read-only entire) were both refused
    // with a sentence about deleting, which is neither what was asked nor
    // true. The destination's writability is a separate question and is
    // answered where the job is queued.
    let takes_the_source_away = match kind {
        JobKind::Move => true,
        JobKind::Copy
        | JobKind::Delete { .. }
        | JobKind::Mkdir
        | JobKind::Size
        | JobKind::Rename
        | JobKind::Compare
        | JobKind::CompareFiles
        | JobKind::Checksum { .. }
        | JobKind::Split
        | JobKind::Merge
        | JobKind::Resize => false,
    };
    if takes_the_source_away && !app.active_panel().active_tab().caps.writable {
        app.message = Some("this backend is read-only; nothing can be moved out of it".to_string());
        return;
    }
    let stats =
        crate::ops::walk::stats_of_rows(app.active_panel().active_tab(), &app.jobs.sizes, &rows);
    let sizing = app
        .jobs
        .rows()
        .iter()
        .any(|j| j.kind == JobKind::Size && j.finished.is_none());
    let mut dialog = CopyMoveDialog::new(
        kind,
        sources.len(),
        target_dir.join(DEFAULT_TARGET_MASK).to_string(),
        stats,
        &app.config.panel,
    )
    .with_config(&app.config.ops)
    .with_history(app.masks.offered.clone());
    // the design computes the statistics before the dialog opens; the
    // design allows them to still be a lower bound, and the spinner is what
    // says a walk is what would resolve it rather than the figure being
    // final.
    dialog.set_sizing(sizing);
    // The statistics keep up with the walks that resolve them: without this the
    // spinner turned for as long as the dialog stayed open and the figure stayed
    // at the lower bound the dialog was born with, while the panel behind it
    // already showed the real one.
    dialog.watch(sources.clone());
    app.draft.op = Some(kind);
    // Act on this list, not on whatever the panel holds when `OK` is pressed.
    app.draft.sources = sources;
    // And keep the destination whole, not only as the text of it: see
    // [`crate::app::jobs::draft::JobDraft::target`].
    app.draft.target = Some(target_dir);
    app.push_dialog(Box::new(dialog));
}

/// `Alt+F6`: unpack the container under the cursor, after asking where.
///
/// It used to unpack straight into the other panel with no question asked,
/// which is the one operation in the program that wrote a whole archive's
/// worth of files somewhere without showing the destination first. The
/// destination is the other panel's directory, as `F5`'s is, and it is now a
/// prefilled answer rather than an assumption.
///
/// The source is the container's *root*, so this is an ordinary `F5` and
/// extraction is the copy engine reading through the [`Vfs`] - progress,
/// conflicts and the failure summary all come for free.
///
/// Which backend the root is opened through comes from the name, exactly as
/// `Enter` decides it: an `.iso` is a disk image and not a zip, and unpacking
/// one used to fail with a message about archives because this hardcoded the
/// archive backend.
///
/// [`Vfs`]: crate::vfs::Vfs
pub(super) fn open_unpack(app: &mut App) {
    let side = app.active_side;
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current() else {
        app.message = Some("there is nothing under the cursor to unpack".to_string());
        return;
    };
    if entry.is_parent || entry.is_dir() {
        app.message = Some(format!(
            "{} is a directory, not an archive; Alt+F5 packs, Alt+F6 unpacks",
            entry.name
        ));
        return;
    }
    let name = entry.name.clone();
    let Some(container) = tab.current_path() else {
        app.message = Some(format!("{name} has no path to unpack"));
        return;
    };
    // The name is a hint, as everywhere else: what it cannot claim is opened
    // as an archive, and the content decides, one frame later.
    let kind = crate::app::container_backend(&name);
    let root = container.with_segment(kind, "/");
    let target_dir = app.panel(side.other()).active_tab().path.clone();

    let dialog = CopyMoveDialog::new(
        JobKind::Copy,
        1,
        target_dir.join(DEFAULT_TARGET_MASK).to_string(),
        crate::ops::walk::SelectionStats::default(),
        &app.config.panel,
    )
    .with_config(&app.config.ops)
    .with_history(app.masks.offered.clone())
    .with_verb(format!("Unpack {name}"));
    app.draft.op = Some(JobKind::Copy);
    app.draft.sources = vec![root];
    app.draft.target = Some(target_dir);
    app.push_dialog(Box::new(dialog));
}

/// `Alt+F5`.
///
/// The target opens on the **other** panel's directory, as `F5`'s does, and on
/// a name taken from what is being packed: the one item's own name when there
/// is one item, and the directory's name when there are several - which is what
/// somebody packing a whole folder meant, and is what Total Commander offers.
pub(super) fn open_pack(app: &mut App) {
    let sources = app.active_panel().active_tab().operand_paths();
    if sources.is_empty() {
        app.message = Some("nothing to pack".to_string());
        return;
    }
    let base = app.active_panel().active_tab().path.clone();
    let stem = match sources.as_slice() {
        [only] => crate::input::search::stem_of(&only.display_title()),
        _ => base
            .file_name()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "archive".to_string()),
    };
    let default = crate::vfs::archive::format::FormatId::Zip;
    let target_dir = app.panel(app.active_side.other()).active_tab().path.clone();
    let target = target_dir
        .join(format!("{stem}{}", default.extension()))
        .to_string();

    let count = sources.len();
    app.draft.sources = sources;
    app.push_dialog(Box::new(crate::ui::dialog::pack::PackDialog::new(
        count, target,
    )));
}

/// Put the full path of everything selected on the system clipboard.
///
/// The marked entries, or the row under the cursor when nothing is marked -
/// the same operands every other file action takes, so what is copied is what
/// `F5` would have copied.
///
/// One path per line, because a multi-line clipboard is what every other
/// program expects to receive from a list, and a single path has no separator
/// to be wrong about.
///
/// Distinct from the two things that already exist and are easy to confuse it
/// with: `ctrl+c` fills the *file* clipboard for a later paste, and `ctrl+p`
/// writes the current directory to the *command line*. This is text, on the
/// system clipboard, for pasting into something else entirely.
pub fn copy_paths(app: &mut App) {
    let paths = app.active_panel().active_tab().operand_paths();
    if paths.is_empty() {
        app.message = Some("nothing to copy the path of".to_string());
        return;
    }
    let text = paths
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    app.queue_clipboard(text, paths.len());
}

/// `Shift+R`: ask for the image resize dialog over the marked selection.
///
/// **This is the one call that opens it.** It captures the operands the way
/// every other operation here does - the marks, or the entry under the cursor -
/// and records the other panel's directory as the destination, which is the
/// convention `F5` and `F6` already follow.
///
/// What it does not do is open the dialog. The dialog names the image's pixel
/// size, which is in the file's own header, and `dispatch` performs no I/O; so
/// this queues the request and the event loop pushes the dialog once the
/// header has been read. See [`crate::app::resize`].
///
/// `pub` rather than `pub(super)` so a key binding can reach it from anywhere
/// in `input` without this module having to know which key it was.
pub fn open_resize(app: &mut App) {
    let sources = app.active_panel().active_tab().operand_paths();
    let Some(first) = sources.first().cloned() else {
        app.message = Some("nothing to resize".to_string());
        return;
    };
    // The name and the size come from the listing, not from a fresh `stat`:
    // the panel has already formatted them and asking again could answer
    // differently from what is on screen.
    let entry = app
        .active_panel()
        .active_tab()
        .operand_rows()
        .first()
        .and_then(|row| app.active_panel().active_tab().entries.get(*row))
        .cloned();
    let target_dir = app.panel(app.active_side.other()).active_tab().path.clone();
    let request = crate::app::resize::ResizeRequest {
        name: entry
            .as_ref()
            .map_or_else(|| first.display_title(), |e| e.name.clone()),
        size: entry.as_ref().map_or(0, |e| e.size),
        path: first,
        count: sources.len(),
        destination: target_dir.to_string(),
    };
    app.draft.op = Some(JobKind::Resize);
    // Act on this list, not on whatever the panel holds when `OK` is pressed:
    // a listing that arrived while the dialog was open re-sorts the rows under
    // a cursor that is a raw index.
    app.draft.sources = sources;
    app.draft.target = Some(target_dir);
    app.request_resize(request);
}

/// The resize dialog came back with every answer.
///
/// One [`JobSpec`], queued. The destination is the directory the dialog was
/// opened against, and a name already in the way is the job's business: it
/// goes through the conflict dialog every other operation uses, which is what
/// makes writing back into the source directory an answer the user gives
/// rather than a default nobody chose.
pub(super) fn resize_accepted(app: &mut App, settings: &crate::ops::resize::ResizeSettings) {
    let sources = app.draft.take_sources();
    app.draft.op = None;
    if sources.is_empty() {
        app.message = Some("nothing to resize".to_string());
        return;
    }
    let Some(dest) = app.draft.target.take() else {
        app.message = Some("there is nowhere to write the images".to_string());
        return;
    };
    let mut options = JobOptions::from_config(&app.config.ops);
    options.resize = Some(settings.clone());
    app.request_job(JobSpec::new(JobKind::Resize, sources, Some(dest)).with_options(options));
}

/// `F8` / `Shift+F8`: confirm, then delete.
///
/// > `F8` moves to the XDG trash. `Shift+F8` unlinks, with a confirmation
/// > naming the count. Directories are recursed with a single confirmation,
/// > not one per entry.
///
/// The confirmation's wording is [`crate::ops::delete::confirm_lines`], so the
/// number in the prompt and the number of sources the runner is handed are one
/// expression.
pub(super) fn open_delete_confirm(app: &mut App, trash: bool) {
    let sources = app.active_panel().active_tab().operand_paths();
    if sources.is_empty() {
        app.message = Some("nothing to delete".to_string());
        return;
    }

    // `F8` has to know **before it asks** whether the trash can take these
    // ("decided *before* the operation starts, never discovered
    // during it"). That is a filesystem question and `dispatch` may not ask one,
    // so it is queued for the event loop exactly as
    // a directory read is; `App::service_trash_probe` pushes the confirmation
    // one frame later, with the wording and the affirmative label the answer
    // dictates. `Shift+F8` needs no probe: it unlinks whatever the trash would
    // have done.
    if trash {
        app.draft.trash_probe = Some(sources);
        return;
    }

    let title = "Delete permanently";
    let lines = crate::ops::delete::confirm_lines(&sources, false);
    // **The affirmative is the default button, for both keys.** the design
    // states it without a trash/unlink qualifier and the design records it as
    // settled: "The delete confirmation defaults to the **action**, not
    // `Cancel`." A `Shift+F8` prompt that opened on `Cancel` meant `Enter` did
    // nothing at all and said nothing about it, so the deliberate answer the
    // guard was meant to buy was really a second `Shift+F8`.
    //
    // `Esc` is no either way, and `y` / `n` answer directly from either.
    let confirm = ConfirmDialog::new(DialogId::ConfirmDelete, title, lines)
        .with_buttons("Delete", "Cancel")
        .defaulting_to_yes();
    app.draft.op = Some(JobKind::Delete { trash: false });
    // Act on exactly what the prompt named (see `delete_confirmed`).
    app.draft.sources = sources;
    app.draft.trash_split = None;
    app.push_dialog(Box::new(confirm));
}

/// `F7`, refusing before it asks.
///
/// The capabilities are `Tab::caps`, which is a memo of the one place a
/// capability answer lives - see [`crate::app::App::refresh_caps`] - so the
/// refusal costs nothing and `dispatch` still touches no filesystem.
/// Being asked for a name and only then told that a
/// `.rar` cannot be written to is the shape of failure the design exists to
/// design out.
pub(super) fn open_mkdir(app: &mut App) {
    let caps = app.active_panel().active_tab().caps;
    if !caps.writable {
        app.message =
            Some("this backend is read-only; no directory can be created in it".to_string());
        return;
    }
    if !caps.has_directories {
        app.message = Some("this backend has no directories to create".to_string());
        return;
    }
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::Mkdir,
        "Create directory",
        "New directory name:",
        "",
    )));
}

/// Write a checksum file for the selection.
///
/// The name is asked for rather than assumed, because its **extension chooses
/// the digest**: `.sha256` writes SHA-256 in the format `sha256sum -c` reads,
/// `.sfv` writes CRC32. One question, two answers, and no dialog full of radio
/// buttons for a choice that a file name already expresses.
///
/// The default sits beside the files rather than in the other panel: a sidecar
/// names its files relative to itself, so it and they belong together, and a
/// checksum written into a directory you were not looking at is one nobody
/// will find again.
pub(super) fn open_checksum(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let sources = tab.operand_paths();
    if sources.is_empty() {
        app.message = Some("nothing to checksum".to_string());
        return;
    }
    if !tab.caps.writable {
        app.message =
            Some("this backend is read-only; a checksum file cannot be written here".to_string());
        return;
    }
    // One file gets its own name; several get the directory's, which is what
    // somebody checksumming a folder meant.
    let stem = match sources.as_slice() {
        [only] => crate::input::search::stem_of(&only.display_title()),
        _ => tab
            .path
            .file_name()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "checksums".to_string()),
    };
    app.draft.sources = sources;
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::Checksum,
        "Checksum",
        "Write to (.sha256 or .sfv):",
        format!("{stem}.sha256"),
    )));
}

/// Check the files a checksum file under the cursor names.
///
/// No dialog: the file says which digest it carries and which files it is
/// about, so there is nothing left to ask.
pub(super) fn verify_checksum(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent && !e.is_dir()) else {
        app.message = Some("no checksum file under the cursor".to_string());
        return;
    };
    if crate::ops::checksum::Digest::of_name(&entry.name).is_none() {
        app.message = Some(format!(
            "{}: not a checksum file (.sha256 or .sfv)",
            entry.name
        ));
        return;
    }
    let Some(path) = tab.current_path() else {
        app.message = Some("that row has no path to read".to_string());
        return;
    };
    app.request_job(JobSpec::new(
        JobKind::Checksum { verify: true },
        vec![path],
        None,
    ));
}

/// Split the file under the cursor into numbered parts.
///
/// The size is asked for because there is no sensible default: a part that
/// fits a medium is the whole point, and which medium is not something this
/// program can know. The parts go into the other panel's directory, as `F5`
/// and `Alt+F5` do.
pub(super) fn open_split(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent && !e.is_dir()) else {
        app.message = Some("no file under the cursor to split".to_string());
        return;
    };
    let Some(path) = tab.current_path() else {
        app.message = Some("that row has no path to split".to_string());
        return;
    };
    let size = entry.size;
    let name = entry.name.clone();
    let target = app.panel(app.active_side.other()).active_tab().path.clone();
    app.draft.sources = vec![path];
    app.draft.target = Some(target);
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::Split,
        "Split",
        format!(
            "Part size for {name} ({}B):",
            crate::panel::format::human_size(size)
        ),
        "100M",
    )));
}

/// Merge the numbered set the cursor is on.
///
/// Only from the **first** part: merging from the middle would produce a file
/// missing its head, which looks like a file and is not one. No dialog, since
/// the set names itself and the output name is the parts' own stem.
pub(super) fn merge_parts(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent && !e.is_dir()) else {
        app.message = Some("no file under the cursor to merge".to_string());
        return;
    };
    if !crate::ops::split::is_first_part(&entry.name) {
        app.message = Some(format!(
            "{}: merge starts at the first part, the one ending .001",
            entry.name
        ));
        return;
    }
    let Some(path) = tab.current_path() else {
        app.message = Some("that row has no path to merge".to_string());
        return;
    };
    app.request_job(JobSpec::new(JobKind::Merge, vec![path], None));
}

/// Create a link to the file under the cursor.
///
/// `symbolic` chooses which kind. Both ask for the link's **name**, not its
/// target: the target is what the cursor is on, which is the thing the user
/// pointed at, and asking for both would make the common case two fields long.
///
/// Refused before the dialog opens where the backend has no links, which is
/// the rule `writable` already follows: a question answered with a form and
/// then refused is worse than one never asked.
pub(super) fn open_link(app: &mut App, symbolic: bool) {
    let tab = app.active_panel().active_tab();
    if !tab.caps.links {
        app.message = Some(format!(
            "this backend has no {} links",
            if symbolic { "symbolic" } else { "hard" }
        ));
        return;
    }
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing under the cursor to link to".to_string());
        return;
    };
    if !symbolic && entry.is_dir() {
        // Every Unix refuses this, and saying so here is better than handing
        // back `EPERM` from three layers down.
        app.message = Some("a hard link cannot point at a directory".to_string());
        return;
    }
    let name = entry.name.clone();
    let Some(path) = tab.current_path() else {
        app.message = Some("that row has no path to link to".to_string());
        return;
    };
    app.draft.sources = vec![path];
    let (id, title) = if symbolic {
        (DialogId::Symlink, "Create symbolic link")
    } else {
        (DialogId::Hardlink, "Create hard link")
    };
    app.push_dialog(Box::new(InputDialog::new(
        id,
        title,
        format!("Name of the link to {name}:"),
        format!("{name}.link"),
    )));
}

/// Change the permissions of the selection.
///
/// Octal, because that is how anybody who wants to change a mode already
/// thinks about it, and because a grid of nine checkboxes is a bigger dialog
/// that answers the same question less directly.
pub(super) fn open_permissions(app: &mut App) {
    let tab = app.active_panel().active_tab();
    if !tab.caps.settable_mode {
        app.message = Some(
            "this backend has no permissions to change; an archive member's mode is in its header"
                .to_string(),
        );
        return;
    }
    let sources = tab.operand_paths();
    if sources.is_empty() {
        app.message = Some("nothing selected to change".to_string());
        return;
    }
    // The current mode of the first operand, so the field opens on what is
    // there rather than on a guess. A selection of many shows the first one's,
    // which is what a reader is looking at.
    let current = tab.current().map_or(0o644, |e| e.mode & 0o7777);
    let count = sources.len();
    app.draft.sources = sources;
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::Permissions,
        "Permissions",
        if count == 1 {
            "New mode, octal:".to_string()
        } else {
            format!("New mode for {count} items, octal:")
        },
        format!("{current:o}"),
    )));
}

/// `synchronize` accepted: turn the plan into copy and delete jobs.
///
/// The deletions go to the trash, not past it: a synchronise that removes the
/// wrong file is the expensive mistake this feature exists to make visible
/// first, and the trash is the one place that mistake is still recoverable.
/// The jobs are queued rather than run at once so a large synchronise goes
/// through admission control like any other batch.
pub(super) fn synchronize_accepted(app: &mut App, plan: &crate::ops::sync::SyncPlan) {
    let jobs = plan.into_jobs(true);
    if jobs.is_empty() {
        app.message = Some("Synchronize: nothing to do".to_string());
        return;
    }
    let count = jobs.len();
    for spec in jobs {
        app.queue_job(spec);
    }
    app.message = Some(format!("Synchronize: queued {count} job(s)"));
}

/// `Alt+F9` / `Ctrl+X Q`: the background queue view.
pub(super) fn open_job_queue(app: &mut App) {
    let jobs = app.jobs.rows().to_vec();
    app.push_dialog(Box::new(QueueDialog::new(jobs)));
}

/// Act on one of the job dialogs' answers.
pub(super) fn run_job_action(app: &mut App, action: JobAction) {
    match action {
        JobAction::Background(id) => app.background_job(id),
        JobAction::Cancel(id) => {
            app.cancel_job(id);
            // The worker notices between chunks, so the row stays "running"
            // for a moment. Without this the dialog the `Esc` just closed
            // would be put straight back on the next frame.
            app.dismiss_job_dialog(id);
        }
        JobAction::Foreground(id) => app.foreground_job(id),
        JobAction::Forget(id) => app.forget_job(id),
        // "show a summary at the end with the option to retry
        // the failures". The failures carry their own paths and `App` keeps the
        // spec each job was built from, so the retry is that job again over
        // exactly the paths that did not make it - same destination, same
        // options, so a retry cannot quietly become a different operation.
        JobAction::Retry(id) => app.message = Some(retry_failures(app, id)),
    }
}

/// Re-run one job over the paths it failed on.
///
/// Returns the line for the status bar, so every outcome - including "there is
/// nothing to retry" - says something.
pub(super) fn retry_failures(app: &mut App, id: JobId) -> String {
    let Some(status) = app.job(id) else {
        return "that job is no longer listed".to_string();
    };
    let failures: Vec<VfsPath> = status
        .finished
        .as_ref()
        .map(|summary| summary.failures.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default();
    if failures.is_empty() {
        return "there is nothing to retry".to_string();
    }
    let Some(spec) = app.job_spec(id).cloned() else {
        return "that job's destination is no longer known; start it again".to_string();
    };
    // A paired job is not retried through this path and the summary dialog no
    // longer offers the button for one (`JobKind::is_retryable`). The check
    // is here as well because the button is not the only way in: the queue
    // view opens a summary for any finished job with failures, and a retry
    // built from `spec` with its positional `targets` dropped would rename
    // nothing and report a clean run.
    if !spec.kind.is_retryable() {
        return "a multi-rename is not retried; use Undo, or run it again".to_string();
    }
    // A failure is recorded against the path that could not be dealt with,
    // which deep in a tree is a *child* of one of the job's sources - and a
    // child cannot be a source of its own here, because one job has one
    // destination and `/src/tree/sub/a.txt` retried directly would land at
    // `/dest/a.txt` rather than back where it belongs. So each failure is
    // mapped to the top-level source that contains it, deduplicated: the retry
    // re-walks only the trees that actually lost something, and everything
    // lands where the first attempt meant to put it. A failure that names no
    // source at all - a refusal naming the destination, say - is dropped rather
    // than turned into one.
    let mut sources: Vec<VfsPath> = Vec::new();
    for failure in failures {
        let Some(root) = spec
            .sources
            .iter()
            .find(|source| failure.starts_with(source))
        else {
            continue;
        };
        if !sources.iter().any(|s| s == root) {
            sources.push(root.clone());
        }
    }
    if sources.is_empty() {
        return "none of the failures name a source that can be retried".to_string();
    }
    let count = sources.len();
    let retry = JobSpec {
        kind: spec.kind,
        sources,
        dest: spec.dest.clone(),
        options: spec.options.clone(),
        // Empty, and safe to leave empty: the guard above refuses every kind
        // for which `targets` means anything, so this is the unpaired case
        // where the spec carries none in the first place.
        targets: Vec::new(),
    };
    app.request_job(retry);
    format!(
        "retrying {count} failed item{}",
        if count == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PendingView;
    use crate::config::{Config, Keymap, Theme};
    use crate::panel::{Side, VirtualKind};
    use crate::vfs::{Entry, ListFs};

    /// A panel showing a search that is **still running**, with one hit in it.
    ///
    /// The row is put into the tab directly rather than streamed in, because
    /// what is under test is the gate and not the read path: a first batch
    /// puts exactly this into exactly this place. The sink is returned
    /// unfinished and is what keeps the listing in its filling state - drop it
    /// and the listing is complete, which is the case that already worked.
    fn app_showing_a_running_search(root: &VfsPath) -> (App, crate::vfs::list::ListSink) {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let (listing, sink) = ListFs::streaming("search: *.rs", std::slice::from_ref(root));
        let pending = PendingView {
            kind: VirtualKind::Search,
            header: "search: *.rs".to_string(),
            find: None,
            origin: root.clone(),
            origin_cursor: None,
            previous: None,
        };
        assert!(
            app.show_listing(Side::Left, 0, listing, pending),
            "the listing registered"
        );

        let hit = root.join("found.rs");
        let mut entry = Entry::file("found.rs");
        entry.location = Some(hit);
        let tab = app.panel_mut(Side::Left).active_tab_mut();
        tab.entries = vec![entry];
        tab.cursor = 0;
        // What the panel looks like while the walk is still going.
        tab.loading = true;
        (app, sink)
    }

    /// **`F6` is offered on a search result panel while the listing is still
    /// filling.**
    ///
    /// Acting on the rows as they arrive is the whole point of a streaming
    /// search, and this key was refused for as long as the search ran: the tab
    /// was seeded with the read-only per-kind answer and only the walk's
    /// completion replaced it, so moving a hit worked if you waited and did
    /// not if you did not. The answer now comes from the one cache, which
    /// knows what a listing over local roots can do the moment the listing is
    /// registered.
    #[test]
    fn f6_is_offered_on_a_search_result_panel_that_is_still_filling() {
        let root = VfsPath::local("/tmp");
        let (mut app, sink) = app_showing_a_running_search(&root);

        open_copy_move(&mut app, JobKind::Move, false);

        assert_eq!(
            app.message, None,
            "moving out of a running search was refused"
        );
        assert!(
            app.dialog_is_open(),
            "the move dialog was not offered while the search was still filling"
        );
        // Held to here so the listing cannot have finished behind the test.
        drop(sink);
    }

    /// The other half of the same gate: a listing whose rows live somewhere
    /// read-only is still refused, so the fix above is not "stop asking".
    #[test]
    fn f6_is_still_refused_on_a_search_over_a_read_only_backend() {
        let root = VfsPath::local("/tmp").with_segment(crate::vfs::BackendKind::Image, "/");
        let (mut app, sink) = app_showing_a_running_search(&root);

        open_copy_move(&mut app, JobKind::Move, false);

        assert!(
            app.message.is_some_and(|m| m.contains("read-only")),
            "a search over a disk image is not a source anything can be moved out of"
        );
        drop(sink);
    }
}
