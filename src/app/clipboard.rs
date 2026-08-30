//! `Ctrl+C`, `Ctrl+X` and `Ctrl+V` on files.
//!
//! The file clipboard holds the entry **under the cursor**, deliberately and
//! not the marks: `F5` is the operation on a marked set, and this is the one
//! where the target is chosen after the source. It lasts the session, and a
//! cut is spent by its paste while a copy can be pasted again.
//!
//! Taking a file also forgets the text clipboard, because the same key has
//! just said "remember this instead" and the command line should paste the
//! more recent of the two.
//!
//! # Pasting into the source's own directory is a rename
//!
//! It is how a file gets a backup before being edited, so the new name is
//! settled here and handed to the copy engine as a full destination path
//! rather than raised as a conflict. A cut is the exception: moving something
//! to where it already is means nothing.

use crate::app::App;
use crate::ops::clipboard::Clipboard;
use crate::ops::{JobKind, JobSpec};
use crate::vfs::VfsPath;

impl App {
    /// `Ctrl+C` / `Ctrl+X`: remember the entry under the cursor.
    ///
    /// Deliberately the cursor's entry and not the marks: `F5` is the operation
    /// on a marked set, and this is the one where the target is chosen after
    /// the source.
    pub fn clipboard_take(&mut self, cut: bool) {
        let Some(path) = self.active_panel().active_tab().current_path() else {
            return;
        };
        if self
            .active_panel()
            .active_tab()
            .current()
            .is_some_and(|e| e.is_parent)
        {
            self.message = Some("`..` is not something to copy".to_string());
            return;
        }
        let held = Clipboard::one(path, cut);
        self.message = Some(held.describe());
        self.clipboard = Some(held);
        // The same key has just said "remember this instead", so the command
        // line should paste the more recent of the two. the file
        // clipboard is untouched and still lasts the session.
        //
        self.text_clipboard = None;
    }

    /// `Ctrl+V`: put the clipboard down in the active panel's directory.
    ///
    /// Pasting into the source's *own* directory renames rather than raising a
    /// conflict - it is how a file gets a backup before being edited - except
    /// for a cut, where moving something where it already is means nothing.
    ///
    pub fn clipboard_paste(&mut self) {
        let Some(held) = self.clipboard.clone() else {
            self.message = Some("nothing on the clipboard".to_string());
            return;
        };
        let dest_dir = self.active_panel().active_tab().path.clone();
        let same_dir = held
            .paths
            .iter()
            .all(|p| p.parent().is_some_and(|parent| parent == dest_dir));

        if same_dir && held.cut {
            self.message = Some("already here; nothing to move".to_string());
            return;
        }

        let kind = if held.cut {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        let mut spec = JobSpec::new(kind, held.paths.clone(), Some(dest_dir.clone()));
        if same_dir {
            // The engine would see a name collision; this case is a rename, so
            // the new name is settled here and handed over as the destination
            // rather than being asked about.
            let existing: Vec<String> = self
                .active_panel()
                .active_tab()
                .entries
                .iter()
                .map(|e| e.name.clone())
                .collect();
            let Some(source) = held.paths.first() else {
                return;
            };
            let name = source.file_name().unwrap_or_default();
            let is_dir = self
                .active_panel()
                .active_tab()
                .entries
                .iter()
                .find(|e| e.name == name)
                .is_some_and(crate::vfs::Entry::is_dir);
            let new_name = crate::ops::clipboard::copy_name(&name, is_dir, |candidate| {
                existing.iter().any(|e| e == candidate)
            });
            // `dest` as a full, not-yet-existing path is what makes the copy
            // engine treat it as a rename target rather than a directory to
            // copy into (`rename_target` in `ops::copy`).
            spec.dest = Some(dest_dir.join(&new_name));
        }
        // Land the cursor on what the paste produced, once the panel re-reads -
        // the same `pending_select` that `F7` and "go to parent" use. With more
        // than one source it is the first, which is the one the eye looks for.
        let landed = spec
            .dest
            .as_ref()
            .filter(|_| same_dir)
            .and_then(VfsPath::file_name)
            .or_else(|| held.paths.first().and_then(VfsPath::file_name));
        if let Some(name) = landed {
            let side = self.active_side;
            self.panel_mut(side).active_tab_mut().pending_select = Some(name);
        }

        // A cut is spent by its paste; a copy can be pasted again.
        if held.cut {
            self.clipboard = None;
        }
        self.request_job(spec);
    }
}
