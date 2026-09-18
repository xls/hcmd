//! What a job came to: the summary a finished worker hands back.
//!
//! Every per-file failure rides here rather than aborting the batch, and
//! [`JobSummary::outcome`] is the one classifier of clean, cancelled and
//! failed - the place the three earlier ad-hoc readings were folded into.

use std::time::Duration;

use crate::vfs::VfsPath;

use super::kind::JobKind;
use super::walk::TreeStats;

/// One per-file failure. errors never abort the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    /// What failed.
    pub path: VfsPath,
    /// Why, already phrased for a human.
    pub error: String,
}

/// What a finished job reports (the end-of-batch summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    /// Which job this was.
    pub kind: JobKind,
    /// Files created, deleted or measured.
    pub files_done: u64,
    /// Directories created, deleted or measured.
    pub dirs_done: u64,
    /// Bytes transferred or measured.
    pub bytes_done: u64,
    /// Destinations skipped by a conflict decision or a file mask.
    pub skipped: u64,
    /// Per-file failures, in the order they happened. Empty on a clean run.
    pub failures: Vec<JobFailure>,
    /// True when the job stopped because it was cancelled rather than because
    /// it ran out of work.
    pub cancelled: bool,
    /// Wall-clock time the job took.
    pub elapsed: Duration,
    /// [`JobKind::Size`] results, one per source that was walked to
    /// completion. A cancelled walk still reports the roots that finished, so
    /// `Esc` part-way through `Ctrl+L` keeps what it already learned.
    ///
    pub sized: Vec<(VfsPath, TreeStats)>,
    /// [`JobKind::Compare`]'s result: the **names** that differ, which is what
    /// a panel mark is. Empty for every other kind, exactly as
    /// `sized` is.
    ///
    /// A cancelled comparison reports what it had already decided, the way a
    /// cancelled `Ctrl+L` keeps what it already learned.
    pub differing: Vec<String>,
    /// [`JobKind::CompareFiles`]'s result: where the two files first stop
    /// agreeing, or `None` when they never do.
    ///
    /// Meaningful only once the job is clean - a pair that could not be read
    /// has no verdict, and `None` there would read as "identical". The caller
    /// checks [`JobSummary::is_clean`] first, which is what
    /// [`crate::app::App`] does before it says anything to the user.
    pub first_difference: Option<u64>,
}

/// How a finished job ended.
///
/// The one classification of a terminal [`JobSummary`], so the rule that
/// cancelling outranks failing lives in exactly one place
/// ([`JobSummary::outcome`]) rather than being re-derived - and re-disagreed -
/// at every consumer. The failure detail itself stays on the summary
/// ([`JobSummary::failures`]); this only says which of the three a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Ran to the end, nothing failed, nothing cancelled.
    Clean,
    /// The user stopped it. Any file it recorded is the one it was interrupted
    /// on, not a failure offered for retry.
    Cancelled,
    /// Ran to the end and something could not be done. Has at least one
    /// [`JobFailure`], and is the one outcome the queue keeps for retry.
    Failed,
}

impl JobSummary {
    /// How a finished job ended, classified once.
    ///
    /// This is the single place the rule "cancelling outranks failing" is
    /// decided. A job stopped part-way through a file records that file, so a
    /// cancelled summary routinely carries a [`JobFailure`]; every consumer
    /// asks here rather than re-deriving the precedence from `cancelled` and
    /// `failures` on its own. When those derivations disagreed, a cancelled job
    /// read as failed was the "1 failed - Retry?" box on a Cancel, and the
    /// cancelled row the queue would not clear.
    pub fn outcome(&self) -> Outcome {
        if self.cancelled {
            Outcome::Cancelled
        } else if self.failures.is_empty() {
            Outcome::Clean
        } else {
            Outcome::Failed
        }
    }

    /// True when the job ran to the end with nothing failed and nothing
    /// cancelled.
    pub fn is_clean(&self) -> bool {
        matches!(self.outcome(), Outcome::Clean)
    }

    /// A one-line status-line report.
    pub fn message(&self) -> String {
        let verb = match self.kind {
            JobKind::Copy => "copied",
            JobKind::Move => "moved",
            JobKind::Delete { trash: true } => "trashed",
            JobKind::Delete { trash: false } => "deleted",
            JobKind::Mkdir => "created",
            JobKind::Size => "measured",
            JobKind::Rename => "renamed",
            JobKind::Compare | JobKind::CompareFiles => "compared",
            JobKind::Checksum { verify: false } => "checksummed",
            JobKind::Checksum { verify: true } => "verified",
            JobKind::Split => "split",
            JobKind::Merge => "merged",
            JobKind::Resize => "resized",
            JobKind::Download => "downloaded",
            JobKind::LocalSend => "sent",
        };
        let mut out = format!(
            "{verb} {} file{}, {} dir{}",
            self.files_done,
            if self.files_done == 1 { "" } else { "s" },
            self.dirs_done,
            if self.dirs_done == 1 { "" } else { "s" },
        );
        if self.skipped > 0 {
            out.push_str(&format!("; {} skipped", self.skipped));
        }
        if !self.failures.is_empty() {
            out.push_str(&format!("; {} failed", self.failures.len()));
        }
        if self.cancelled {
            out.push_str("; cancelled");
        }
        out
    }
}
