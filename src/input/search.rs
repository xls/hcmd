//! `Alt+F7` and `Ctrl+M`: the two dialogs that describe work over many files.
//!
//!
//! Both are the same shape and neither is a job: the Find Files dialog
//! produces a query the event loop walks with, and the Multi-Rename Tool
//! produces a plan that becomes a rename. Both remember what they were last
//! answered with, so reopening offers it back, and both write that memory
//! through a dirty flag rather than to disk, because
//! [`super::dispatch`] may not touch the filesystem.

use std::collections::{HashMap, HashSet};

use crate::app::App;
use crate::search::saved::History;
use crate::ui::dialog::{FindDialog, MultiRenameDialog, RenameResultDialog};
use crate::vfs::VfsPath;

/// `Alt+F7`: the Find Files dialog over the active panel.
///
/// The start path is the panel's **real** directory: on a virtual listing that
/// is the origin the search came from, because `list:/7` is not a tree and a
/// second `Alt+F7` there means "search again", not "search these results"
/// (which is `Alt+Shift+F7`).
pub(super) fn open_find(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let start = tab
        .virtual_view()
        .map_or_else(|| tab.path.clone(), |view| view.origin.clone());
    let marked = marked_paths(tab);
    push_find(app, start, marked, Vec::new());
}

/// `Alt+Shift+F7`: search **within** what the panel
/// is showing.
///
/// the design names the key nowhere; the contract gives it one job, which is to
/// stop a second `Alt+F7` on a virtual listing having to choose silently
/// between narrowing and starting again. Narrowing is expressed with the
/// machinery that already exists: the rows the user marked (or the row under
/// the cursor) become the search roots, because a root is walked as a tree
/// when it is a directory and admitted on its own when it is a file. No new
/// engine concept, no per-entry scan of a list of ten thousand paths, and it
/// reads the same on a real directory as on a set of results.
pub(super) fn open_find_in_panel(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let roots = tab.operand_paths();
    if roots.is_empty() {
        app.message = Some("nothing to search within; mark some rows first".to_string());
        return;
    }
    if roots.len() > crate::search::query::MAX_ROOTS {
        app.message = Some(format!(
            "a search takes at most {} roots; {} are marked",
            crate::search::query::MAX_ROOTS,
            roots.len()
        ));
        return;
    }
    let start = roots.first().cloned().unwrap_or_else(|| tab.path.clone());
    let marked = marked_paths(tab);
    push_find(app, start, marked, roots);
}

/// The marks, as real addresses, for "Only search in selected directories/files"
/// (the design - the box is disabled when nothing is marked).
fn marked_paths(tab: &crate::panel::Tab) -> Vec<VfsPath> {
    if tab.marks.is_empty() {
        return Vec::new();
    }
    tab.operand_paths()
}

/// Push the Find dialog, remembering nothing the dialog itself does not need.
fn push_find(app: &mut App, start: VfsPath, marked: Vec<VfsPath>, roots: Vec<VfsPath>) {
    let mut dialog = FindDialog::new(start, marked, &app.search);
    if !roots.is_empty() {
        dialog = dialog.with_roots(roots);
    }
    app.push_dialog(Box::new(dialog));
}

/// `Start search`.
///
/// Three things, in this order: remember what was typed so the combo boxes
/// offer it next time, keep the saved list the Load/Save tab left, and queue
/// the search. The two writes to disk are the event loop's - `dispatch` may not
/// touch the filesystem - so they are queued as part
/// of the search session and performed by
/// [`crate::app::App::service_search_state`].
pub(super) fn find_accepted(app: &mut App, answer: &crate::dialog::FindAnswer) {
    let query = answer.query.clone();
    let state = &mut app.search;
    History::remember(&mut state.history.names, &query.name);
    if let Some(content) = query.content.as_ref() {
        History::remember(&mut state.history.texts, &content.pattern);
    }
    for root in &query.roots {
        History::remember(&mut state.history.roots, &root.to_string());
    }
    if let Some(saved) = answer.saved.clone() {
        state.saved = saved;
        state.saved_dirty = true;
    }
    state.last = Some(query.clone());
    state.tab = answer.tab;
    state.history_dirty = true;
    app.request_search(query, crate::panel::VirtualKind::Search);
}

/// The `Save as…` prompt's answer, which belongs to the Find dialog underneath
/// it - exactly as `+ F7`'s belongs to the copy dialog underneath it.
pub(super) fn save_search_named(app: &mut App, name: &str) {
    let Some(dialog) = app
        .top_dialog_mut()
        .and_then(|d| d.as_any_mut())
        .and_then(|any| any.downcast_mut::<FindDialog>())
    else {
        app.message = Some("the find dialog is no longer open".to_string());
        return;
    };
    dialog.save_as(name);
    if let Some(why) = dialog.error() {
        app.message = Some(why.to_string());
    }
}

/// `Ctrl+M`: the multi-rename tool over the marked entries, or
/// the whole directory when nothing is marked.
///
/// `Tab::rename_rows` is that rule and is deliberately not `operand_rows`,
/// which falls back to the cursor for `F5` and `F8`.
///
pub(super) fn open_multi_rename(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let rows: Vec<(crate::vfs::Entry, VfsPath)> = tab
        .rename_rows()
        .into_iter()
        .filter_map(|i| Some((tab.entries.get(i)?.clone(), tab.path_of(i)?)))
        .collect();
    if rows.is_empty() {
        app.message = Some("nothing to rename".to_string());
        return;
    }
    // What names are already taken, per directory. On an ordinary listing that
    // is the whole directory and the answer is exact; on a virtual one it is
    // only the rows the search found, which is why `RenameStatus::Exists` also
    // reasons from the rows themselves.
    let mut siblings: HashMap<VfsPath, HashSet<String>> = HashMap::new();
    for (index, entry) in tab.entries.iter().enumerate() {
        if entry.is_parent {
            continue;
        }
        if let Some(dir) = tab.dir_of(index) {
            siblings.entry(dir).or_default().insert(entry.name.clone());
        }
    }
    let settings = app.rename.settings.clone();
    let can_undo = app.rename.undo.is_some();
    let has_result = !app.rename.result.is_empty();
    let cfg = app.config.panel.clone();
    app.push_dialog(Box::new(
        MultiRenameDialog::new(rows, siblings, settings, can_undo)
            .with_result(has_result)
            .with_config(&cfg),
    ));
}

/// `Start!`, `Undo` or `Result list`.
pub(super) fn multi_rename_accepted(app: &mut App, answer: &crate::dialog::MultiRenameAnswer) {
    // Whichever button was pressed, the four control groups are remembered, so
    // reopening `Ctrl+M` offers what was last used.
    app.rename.settings = answer.settings.clone();
    if answer.show_result {
        open_rename_result(app);
        return;
    }
    if answer.undo {
        let Some(undo) = app.rename.undo.clone() else {
            app.message = Some("there is nothing to undo".to_string());
            return;
        };
        app.request_rename(crate::app::RenameRequest {
            pairs: undo.pairs,
            undoing: true,
        });
        return;
    }
    app.request_rename(crate::app::RenameRequest {
        pairs: answer.pairs.clone(),
        undoing: false,
    });
}

/// `Alt+Shift+M` and the dialog's `Result list` button.
pub(super) fn open_rename_result(app: &mut App) {
    if app.rename.result.is_empty() {
        app.message = Some("no multi-rename has run yet this session".to_string());
        return;
    }
    let lines = app.rename.result.clone();
    app.push_dialog(Box::new(RenameResultDialog::new(lines)));
}

/// A name without its extension, for the pre-filled archive name.
///
/// `photos.tar.gz` packs into `photos.zip` rather than `photos.tar.gz.zip`, and
/// a dotfile keeps its leading dot because that is not a separator (the `ext`
/// column rule, which the rename dialog uses too).
pub(super) fn stem_of(name: &str) -> String {
    if let Some((suffix, _)) = crate::vfs::archive::format::FormatId::suffix_of(name) {
        return name
            .get(..name.len().saturating_sub(suffix.len()))
            .unwrap_or(name)
            .to_string();
    }
    match name.rfind('.') {
        Some(0) | None => name.to_string(),
        Some(at) => name.get(..at).unwrap_or(name).to_string(),
    }
}
