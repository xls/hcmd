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
use crate::vfs::VfsPath;

impl App {
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
