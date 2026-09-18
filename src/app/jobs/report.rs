//! What to show when a job finishes - decided apart from showing it.
//!
//! The rules are few but they crossed three things - the job's kind, how it
//! ended and whether it was on screen - and a match that mixed them with
//! the side effects (pushing a dialog, setting the status line) was where a
//! cancelled copy once got a "1 failed - Retry?" box. So the decision is a
//! pure function here, with every [`JobKind`] named, and
//! [`crate::app::App::report_finished`] only carries out the answer.

use crate::ops::{JobKind, JobSummary, Outcome, View};
use crate::vfs::VfsPath;

/// What a finished job puts in front of the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Nothing: the result waits in the queue view, or there is nothing to
    /// say.
    Nothing,
    /// One line on the status bar.
    Message(String),
    /// The contents comparison: marks in both panels, and a count.
    Compare,
    /// Two named files compared: a sentence in a box.
    CompareFiles,
    /// A foreground job that failed: the summary box with its retry, and
    /// the "delete permanently?" question for what could not be trashed.
    Failures {
        /// Whether the summary box is worth opening. A summary that would
        /// say nothing but "there was no trash" is not: the offer below says
        /// the same thing and asks the one question that is left.
        show_summary: bool,
        /// What `F8` could not trash for want of a trash, to be offered for
        /// permanent deletion. Empty for anything but a trash delete.
        offer_permanent_delete: Vec<VfsPath>,
    },
}

/// The report for a job of `summary.kind` that ended as `summary` says,
/// seen from `view`.
#[must_use]
pub fn report_for(view: View, summary: &JobSummary) -> Report {
    match summary.kind {
        // The `\u{2265}` in the status line resolving into a number is the
        // feedback; a message per `Space` would be noise. A walk that could
        // not read everything is the exception: its figure stays a lower
        // bound, and the "never silently report a computed-looking total
        // that is actually partial" means the reason has to reach the user
        // somewhere other than the queue view. The status line, not a
        // dialog: a walk steals no focus.
        JobKind::Size => match summary.failures.first() {
            Some(first) => Report::Message(match summary.failures.len() {
                1 => format!("{}: {}", first.path, first.error),
                n => format!(
                    "{}: {} (and {} more)",
                    first.path,
                    first.error,
                    n.saturating_sub(1)
                ),
            }),
            None => Report::Nothing,
        },
        // No dialog was shown, so the status line is the only report there
        // is - and it is where a failure has to appear.
        JobKind::Mkdir => Report::Message(summary.message()),
        // A rename has no progress dialog and therefore no summary box
        // either, so the status line is the whole of its report - and a
        // batch that failed has to say where the detail is, which is the
        // result list the dialog's button and `Action::RenameResult` both
        // open.
        JobKind::Rename => {
            let mut line = summary.message();
            if !summary.failures.is_empty() {
                line.push_str("; see the result list");
            }
            Report::Message(line)
        }
        // The contents comparison marks and says how many, in the status
        // line. It opens no summary box, because a comparison that found
        // nothing has nothing to show and one that found something has
        // already shown it - in both panels.
        JobKind::Compare => Report::Compare,
        // Comparing two named files answers in a sentence rather than in
        // marks, so this one does open a box: there is nothing on either
        // panel for it to have shown already.
        JobKind::CompareFiles => Report::CompareFiles,
        JobKind::Copy
        | JobKind::Move
        | JobKind::Delete { .. }
        | JobKind::Checksum { .. }
        | JobKind::Split
        | JobKind::Merge
        | JobKind::Resize
        | JobKind::Download
        | JobKind::LocalSend => generic(view, summary),
    }
}

/// The report for every kind that has a progress dialog and a summary.
fn generic(view: View, summary: &JobSummary) -> Report {
    match view {
        // A job that finishes in the background "does not steal focus"; its
        // result waits in the queue view.
        View::Background => Report::Nothing,
        View::Foreground { .. } => match summary.outcome() {
            // A clean finish, and a cancelled one, report in the status line
            // and nothing more. Cancelling is not failure: a copy stopped
            // part-way through a file records that file, but the user
            // pressed Cancel and must not be answered with a "1 failed -
            // Retry?" box for having done so. `outcome` is the one place
            // that line is drawn.
            Outcome::Clean | Outcome::Cancelled => Report::Message(summary.message()),
            // "show a summary at the end with the option to retry the
            // failures".
            Outcome::Failed => {
                let offer = crate::ops::delete::permanent_delete_offer(summary);
                Report::Failures {
                    show_summary: offer.len() < summary.failures.len(),
                    offer_permanent_delete: offer,
                }
            }
        },
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
