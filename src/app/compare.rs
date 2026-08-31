//! `Shift+F2`: mark what differs between the two listings.
//!
//! Comparing marks by **name** on both sides and counts each distinct name
//! once, so two identical directories report the work they did rather than
//! twice it. It is a marking operation and nothing else: no window opens, no
//! report is produced, the cursor is not touched, and everything that already
//! works on marks works on its result.
//!
//! The marks are replaced rather than added to, because the design marks the
//! differences "and nothing else". The one exception is the verdict of a
//! contents job, which is added, because that job only ever looked at the
//! pairs the cheap steps could not separate and everything it did not name is
//! still exactly as different as it already was.
//!
//! Nothing here reads a file. A comparison that needs contents becomes a
//! [`crate::ops::JobKind::Compare`] for the event loop.

use crate::app::App;
use crate::ops::{JobSpec, JobSummary};
use crate::ui::dialog::fileinfo::FileInfoDialog;
use crate::vfs::VfsPath;

impl App {
    /// `Alt+V`: browse the git history of the active panel's directory.
    ///
    /// Opens `dir#git/` as a virtual listing: commits are folders, a commit's
    /// changed files are inside it, and each is readable at that commit - so
    /// `F3`, `Alt+D` and `F5` all work on them because they are ordinary
    /// paths. Refused with a sentence when the directory is not in a
    /// repository, rather than opening onto an empty listing.
    pub fn open_git_history(&mut self) {
        let side = self.active_side;
        let dir = self.panel(side).active_tab().path.clone();
        // Already in the history: `Alt+V` is a toggle, so it leaves. Back to
        // the local directory the `#git` segment hangs off, which is the
        // outermost segment.
        if dir.backend() == crate::vfs::BackendKind::Git {
            if let Some((crate::vfs::BackendKind::Local, root)) = dir.segments().first() {
                let home = crate::vfs::VfsPath::local(root.clone());
                self.navigate(side, home);
            }
            return;
        }
        let Some(local) = dir.local_path() else {
            self.message = Some("git history is only for a local directory".to_string());
            return;
        };
        match crate::git::history(local) {
            Ok(commits) if !commits.is_empty() => {
                let git = dir.with_segment(crate::vfs::BackendKind::Git, "/");
                self.navigate(side, git);
            }
            Ok(_) => {
                self.message = Some(format!("{}: no git history here", local.display()));
            }
            Err(err) => self.message = Some(err.to_string()),
        }
    }

    /// the `Shift+F2`: mark, in both panels, every entry that is not
    /// the same on the other side, and nothing else.
    ///
    /// Pure: both listings are already in memory, so this marks in place and
    /// leaves its report in [`App::message`]. No window opens, no report is
    /// produced and the cursor is not touched - "it is a marking operation, so
    /// everything that already works on marks works on its result".
    ///
    /// The lists are the two panels' active tabs **as they stand**: hidden
    /// files excluded when `panel.show_hidden` is off, mask-filtered rows
    /// excluded when a filter is on. The marks are **replaced** rather than
    /// added to, because the design marks the differences "and nothing
    /// else".
    ///
    /// Queues a [`JobKind::Compare`] when `ops.compare_contents` left pairs
    /// undecided; nothing is read here.
    pub fn compare_lists(&mut self) {
        let slack = self.config.ops.compare_mtime_slack.duration();
        let contents = self.config.ops.compare_contents;
        // Both panels are separate fields, so the two listings are borrowed at
        // once and neither is cloned: a comparison of two directories of a
        // million rows copies nothing.
        let (outcome, compared) = {
            let left = &self.left.active_tab().entries;
            let right = &self.right.active_tab().entries;
            (
                crate::ops::compare::compare_lists(left, right, slack, contents),
                crate::panel::marks::compared_count(left, right),
            )
        };
        crate::panel::marks::replace_marks(self.left.active_tab_mut(), &outcome.left);
        crate::panel::marks::replace_marks(self.right.active_tab_mut(), &outcome.right);
        self.message = Some(crate::ops::compare::describe(&outcome, compared));

        if !outcome.undecided.is_empty() {
            let pairs = self.compare_pairs(&outcome.undecided);
            if !pairs.is_empty() {
                self.request_job(JobSpec::compare(pairs));
            }
        }
    }

    /// Compare the file under the cursor in each panel, byte for byte.
    ///
    /// The other half of `Shift+F2`'s question. That one asks which of two
    /// *listings* differ and answers in marks; this one asks whether two named
    /// files are the same and answers in a sentence, which is what a reader
    /// wants after pointing at one file on each side.
    ///
    /// Reads nothing here. Two files are two arbitrarily large reads, possibly
    /// over a network, so the comparison is a
    /// [`crate::ops::JobKind::CompareFiles`] and the keystroke only queues it:
    /// `dispatch` performs no I/O.
    ///
    /// A directory on either side is refused rather than walked. "Are these
    /// two trees the same" is a different question with a different answer,
    /// and `Shift+F2` is the one that asks it.
    pub fn compare_files(&mut self) {
        let Some((left, left_name)) = self.cursor_file(crate::app::Side::Left) else {
            self.message =
                Some("compare files: the left panel has no file under the cursor".into());
            return;
        };
        let Some((right, right_name)) = self.cursor_file(crate::app::Side::Right) else {
            self.message =
                Some("compare files: the right panel has no file under the cursor".into());
            return;
        };
        self.compare_names = Some((left_name, right_name));
        self.request_job(JobSpec::compare_files(left, right));
    }

    /// Show the file under each panel's cursor as a unified diff.
    ///
    /// The other half of `Ctrl+F2`'s question. That one answers "the same, or
    /// different at byte 4,231"; this one answers "different how", which is
    /// the question a reader asks next often enough that answering only the
    /// first was leaving the job half done.
    ///
    /// The left panel is the `---` side and the right is `+++`, whichever
    /// panel is active: a diff whose direction depended on which side the
    /// cursor happened to be in would be a diff nobody could read twice.
    ///
    /// Reads nothing here. Both files are read by the event loop, because
    /// `dispatch` performs no I/O.
    pub fn diff_files(&mut self) {
        let Some((left, _)) = self.cursor_file(crate::app::Side::Left) else {
            self.message = Some("diff: the left panel has no file under the cursor".into());
            return;
        };
        let Some((right, _)) = self.cursor_file(crate::app::Side::Right) else {
            self.message = Some("diff: the right panel has no file under the cursor".into());
            return;
        };
        self.request_view(crate::app::ViewRequest::Diff {
            old: left,
            new: right,
        });
    }

    /// The path and name of the file under one panel's cursor.
    ///
    /// `None` for an empty listing, for `..`, and for a directory - each of
    /// which is a reason this comparison has no operand rather than an error
    /// worth its own message, so the caller says which side was empty.
    fn cursor_file(&self, side: crate::app::Side) -> Option<(VfsPath, String)> {
        let tab = self.panel(side).active_tab();
        let entry = tab.entries.get(tab.cursor)?;
        if entry.is_dir() || entry.name == ".." {
            return None;
        }
        Some((tab.path_of(tab.cursor)?, entry.name.clone()))
    }

    /// Say whether the two files turned out to be the same.
    ///
    /// A job that could not read one of them has no verdict at all: "could not
    /// be read" and "is not the same" are different answers, and only one of
    /// them was asked for. That case reports the failure and claims nothing.
    pub fn finish_compare_files(&mut self, summary: &JobSummary) {
        let names = self.compare_names.take();
        if summary.cancelled {
            self.message = Some("the comparison was cancelled".to_string());
            return;
        }
        if let Some(failure) = summary.failures.first() {
            self.message = Some(format!("compare files: {}", failure.error));
            return;
        }
        let note = match summary.first_difference {
            None => "The two files are identical.".to_string(),
            Some(at) => format!("The two files differ, from byte {at}."),
        };
        let facts = names.map_or_else(Vec::new, |(left, right)| {
            vec![("Left".to_string(), left), ("Right".to_string(), right)]
        });
        self.push_dialog(Box::new(FileInfoDialog::statement(
            "Compare files",
            facts,
            note,
        )));
    }

    /// The facing addresses of the names `ops.compare_contents` left undecided.
    ///
    /// Addressed through [`Tab::path_of`], so a virtual listing's rows are
    /// compared where they really live rather than under the virtual path they
    /// are shown at. A name that has stopped being a row on
    /// either side between the comparison and this call is dropped rather than
    /// guessed at.
    fn compare_pairs(&self, names: &[String]) -> Vec<(VfsPath, VfsPath)> {
        let left = self.left.active_tab();
        let right = self.right.active_tab();
        names
            .iter()
            .filter_map(|name| {
                let a = left.index_of(name).and_then(|i| left.path_of(i))?;
                let b = right.index_of(name).and_then(|i| right.path_of(i))?;
                Some((a, b))
            })
            .collect()
    }

    /// Apply a finished compare job's verdict to both panels.
    ///
    /// **Added** to what steps 1 to 3 already marked rather than replacing it:
    /// the job only ever looked at pairs those steps could not separate, so
    /// everything it did not name is still exactly as different as it was.
    ///
    /// A cancelled job applies whatever it decided before it stopped and says
    /// so, the way a cancelled `Ctrl+L` keeps what it already learned.
    ///
    pub fn finish_compare(&mut self, summary: &JobSummary) {
        crate::panel::marks::add_marks(self.left.active_tab_mut(), &summary.differing);
        crate::panel::marks::add_marks(self.right.active_tab_mut(), &summary.differing);
        let mut line = match (summary.differing.len(), summary.cancelled) {
            (0, true) => "the comparison was cancelled".to_string(),
            (0, false) => "the contents are identical".to_string(),
            (n, true) => format!("{n} differ by contents; cancelled"),
            (n, false) => format!("{n} differ by contents"),
        };
        if !summary.failures.is_empty() {
            line.push_str(&format!("; {} could not be read", summary.failures.len()));
        }
        self.message = Some(line);
    }
}
