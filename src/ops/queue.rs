//! The background job queue.
//!
//! > **Background queue**: a second `F5` while a copy is running enqueues
//! > rather than refusing. A queue view lists pending, active and failed jobs.
//!
//! and,:
//!
//! > `F2 Queue` (append to the background queue instead of starting now) …
//! > `F2 Queue` and the queue view are not an afterthought - copying a large
//! > tree while continuing to work is the normal case, not the exception.
//!
//! # What is here and what is not
//!
//! This module is the queue's **model**. The queue *view* is UI and belongs to
//! agent 2d: [`rows`] and [`counts`] give it everything it needs to draw
//! without knowing anything about workers, and [`JobState`] is the one place
//! "pending / active / failed" is defined so the view and the key bar cannot
//! disagree about what a job is doing.
//!
//! [`JobQueue`] is the admission controller: it decides whether a newly
//! requested job starts now or waits, and hands the waiting ones out as slots
//! free up. It holds [`JobRequest`]s - specs that have not been spawned - and
//! reads [`JobStatus`] rows for what is already running. It never spawns
//! anything itself, exactly as `dispatch` never touches the filesystem.
//!
//! # Why only the destructive kinds queue
//!
//! Serialising every job would make `Space` on a directory wait behind a
//! four-hour copy, which is absurd: a [`JobKind::Size`] walk reads metadata,
//! writes nothing, and is what the panel status line is waiting on. `Mkdir` is
//! a single syscall. Copy, move and delete are the ones that contend for the
//! disk and for the same paths, and they are what the sentence is
//! about. [`serialises`] is that rule in one function.

use std::collections::{HashSet, VecDeque};

use super::{JobId, JobKind, JobRequest, JobStatus};
use crate::config::OpsConfig;

/// How many contending jobs run at once by default.
///
/// One. the design wants a second `F5` to *enqueue*, and two large copies
/// sharing a spindle finish later than the same two run in sequence. It is a
/// constant rather than a config key because nothing in the `[ops]`
/// table offers to set it.
pub const DEFAULT_CONCURRENCY: usize = 1;

/// Does this kind of job wait its turn?
///
/// See the module docs: the destructive kinds contend, the two cheap read-only
/// ones do not.
pub const fn serialises(kind: JobKind) -> bool {
    matches!(
        kind,
        // A resize is the one that is CPU-bound rather than disk-bound, and it
        // waits its turn for the same reason the other three do: two batches
        // decoding at once finish later than the same two in sequence.
        JobKind::Copy | JobKind::Move | JobKind::Delete { .. } | JobKind::Resize
    )
}

/// What the queue view shows in its state column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Accepted but not started: it is waiting for a slot, or the user pressed
    /// `F2 Queue` instead of `OK`.
    Pending,
    /// Running.
    Running,
    /// Running, but parked on a conflict nobody has answered.
    ///
    /// such a job "does not sit silently blocked" - it marks
    /// itself as waiting here and the key bar says a job needs attention.
    Waiting,
    /// Finished with nothing to report.
    Done,
    /// Finished with per-file failures (the summary).
    Failed,
    /// Stopped by `Esc`.
    Cancelled,
}

impl JobState {
    /// Every state, in the order a view should group them.
    pub const ALL: &'static [Self] = &[
        Self::Waiting,
        Self::Running,
        Self::Pending,
        Self::Failed,
        Self::Cancelled,
        Self::Done,
    ];

    /// A stable string id.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The word the queue view prints.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Pending => "queued",
            Self::Running => "running",
            Self::Waiting => "needs answer",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// True once the job can no longer change.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// True while the job is going to make no further progress until a human
    /// answers something.
    pub const fn needs_attention(&self) -> bool {
        matches!(self, Self::Waiting)
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Which state one job is in.
///
/// The order of the tests is the meaning: a job parked on a conflict is
/// `Waiting` even though it is also running, because that is what the view has
/// to say about it.
pub fn state_of(status: &JobStatus) -> JobState {
    if let Some(summary) = status.finished.as_ref() {
        if summary.cancelled {
            return JobState::Cancelled;
        }
        return if summary.failures.is_empty() {
            JobState::Done
        } else {
            JobState::Failed
        };
    }
    if status.pending_decision.is_some() {
        return JobState::Waiting;
    }
    if status.started {
        JobState::Running
    } else {
        JobState::Pending
    }
}

/// One line of the queue view.
///
/// Owned rather than borrowed so the view can build, sort and filter these
/// without holding a borrow on [`crate::app::App`] while it draws.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRow {
    /// Which job.
    pub id: JobId,
    /// What it is doing.
    pub kind: JobKind,
    /// Where it is up to.
    pub state: JobState,
    /// True when its progress dialog is not on screen.
    pub background: bool,
    /// The verb, e.g. `Copying`.
    pub title: &'static str,
    /// A one-line "what and how far", already phrased.
    pub detail: String,
    /// The batch bar's fraction, `None` when there is no total to divide by.
    pub fraction: Option<f64>,
    /// How many per-file failures it has collected so far.
    pub failures: usize,
}

/// Build one row.
pub fn row(status: &JobStatus) -> QueueRow {
    let state = state_of(status);
    QueueRow {
        id: status.id,
        kind: status.kind,
        state,
        background: status.background,
        title: status.kind.title(),
        detail: detail(status, state),
        fraction: status.fraction(),
        failures: status.failures.len(),
    }
}

/// Build every row, in the order the jobs were requested.
pub fn rows(jobs: &[JobStatus]) -> Vec<QueueRow> {
    jobs.iter().map(row).collect()
}

/// The right-hand text for one row.
fn detail(status: &JobStatus, state: JobState) -> String {
    match state {
        JobState::Pending => "waiting for a slot".to_string(),
        JobState::Waiting => status.pending_decision.as_ref().map_or_else(
            || "waiting for an answer".to_string(),
            |request| format!("{} already exists", request.dest),
        ),
        JobState::Running => {
            if status.files_total > 0 {
                format!("{} / {} files", status.files_done, status.files_total)
            } else {
                format!("{} files", status.files_done)
            }
        }
        // A finished job's own sentence, which already names the counts, the
        // skips and the failures (the end-of-batch summary).
        JobState::Done | JobState::Failed | JobState::Cancelled => status
            .finished
            .as_ref()
            .map_or_else(String::new, |summary| summary.message()),
    }
}

/// How many jobs are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    /// Accepted, not started.
    pub pending: usize,
    /// Running and making progress.
    pub running: usize,
    /// Running but parked on a conflict.
    pub waiting: usize,
    /// Finished cleanly.
    pub done: usize,
    /// Finished with failures.
    pub failed: usize,
    /// Cancelled.
    pub cancelled: usize,
}

impl QueueCounts {
    /// Jobs that have not finished, parked ones included.
    pub const fn active(&self) -> usize {
        self.pending
            .saturating_add(self.running)
            .saturating_add(self.waiting)
    }

    /// True when there is nothing at all to show.
    pub const fn is_empty(&self) -> bool {
        self.active() == 0 && self.done == 0 && self.failed == 0 && self.cancelled == 0
    }

    /// A key-bar-sized summary, or `None` when there is nothing worth saying.
    ///
    /// the indicator "changes to say a job needs attention",
    /// which is why the parked count comes first when there is one.
    pub fn indicator(&self) -> Option<String> {
        if self.waiting > 0 {
            return Some(format!("{} job(s) need an answer", self.waiting));
        }
        if self.active() > 0 {
            return Some(format!("{} job(s)", self.active()));
        }
        if self.failed > 0 {
            return Some(format!("{} job(s) failed", self.failed));
        }
        None
    }
}

/// Count the jobs by state.
pub fn counts(jobs: &[JobStatus]) -> QueueCounts {
    let mut out = QueueCounts::default();
    for status in jobs {
        match state_of(status) {
            JobState::Pending => out.pending = out.pending.saturating_add(1),
            JobState::Running => out.running = out.running.saturating_add(1),
            JobState::Waiting => out.waiting = out.waiting.saturating_add(1),
            JobState::Done => out.done = out.done.saturating_add(1),
            JobState::Failed => out.failed = out.failed.saturating_add(1),
            JobState::Cancelled => out.cancelled = out.cancelled.saturating_add(1),
        }
    }
    out
}

/// The admission controller.
///
/// Holds the [`JobRequest`]s that have been accepted but not spawned, and
/// decides when each may start. The event loop's contract is three calls:
///
/// ```text
/// let go = queue.submit(request, app.jobs.rows());   // start now, or None
/// …
/// for request in queue.release(app.jobs.rows()) { … }  // once a frame
/// ```
///
/// A released request is remembered as launched until its [`JobStatus`]
/// reports finished, so the slot is not handed out twice in the frames before
/// [`super::JobEvent::Started`] arrives.
#[derive(Debug, Default)]
pub struct JobQueue {
    limit: usize,
    waiting: VecDeque<JobRequest>,
    launched: HashSet<JobId>,
}

impl JobQueue {
    /// A queue running `limit` contending jobs at once. A limit of zero is
    /// read as one; a queue that admits nothing is a deadlock, not a setting.
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            waiting: VecDeque::new(),
            launched: HashSet::new(),
        }
    }

    /// The queue `[ops]` asks for (`examples/config.toml`).
    ///
    /// `ops.background_queue = false` does **not** mean "refuse the second
    /// `F5`" - the design forbids refusing - it means nothing ever waits:
    /// every job starts when it is requested. `true`, the default, serialises
    /// the contending kinds. the design defines the key nowhere, and
    /// `examples/config.toml` ships it with no reader; this is the only
    /// reading compatible with, and it is written down here rather than
    /// guessed at again in the event loop.
    pub fn from_config(cfg: &OpsConfig) -> Self {
        Self::new(if cfg.background_queue {
            DEFAULT_CONCURRENCY
        } else {
            usize::MAX
        })
    }

    /// How many contending jobs may run at once.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Offer a request to the queue.
    ///
    /// `Some` means "spawn this now"; `None` means it is queued and will come
    /// back from [`JobQueue::release`]. a second `F5` while a
    /// copy is running "enqueues rather than refusing", so this never fails.
    pub fn submit(&mut self, request: JobRequest, jobs: &[JobStatus]) -> Option<JobRequest> {
        // A `Size` walk or a `mkdir` never waits: see the module docs.
        if !serialises(request.spec.kind) {
            return Some(request);
        }
        if self.waiting.is_empty() && self.occupied(jobs) < self.limit {
            self.launched.insert(request.id);
            return Some(request);
        }
        self.waiting.push_back(request);
        None
    }

    /// `F2 Queue`: queue it whatever the queue is doing.
    ///
    /// Distinct from [`JobQueue::submit`] because the button means "not now"
    /// even when a slot is free - the user asked to keep working.
    pub fn enqueue(&mut self, request: JobRequest) {
        self.waiting.push_back(request);
    }

    /// Hand out whatever now fits. Call once a frame.
    pub fn release(&mut self, jobs: &[JobStatus]) -> Vec<JobRequest> {
        self.forget_finished(jobs);
        let mut out = Vec::new();
        while self.occupied(jobs).saturating_add(out.len()) < self.limit {
            let Some(request) = self.waiting.pop_front() else {
                break;
            };
            self.launched.insert(request.id);
            out.push(request);
        }
        out
    }

    /// The queued-but-not-started requests, oldest first.
    pub fn pending(&self) -> impl ExactSizeIterator<Item = &JobRequest> {
        self.waiting.iter()
    }

    /// How many are waiting for a slot.
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    /// True when nothing is waiting for a slot.
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Is this job still waiting to start?
    pub fn contains(&self, id: JobId) -> bool {
        self.waiting.iter().any(|r| r.id == id)
    }

    /// `Esc` on a queued job: drop it before it ever runs.
    ///
    /// Returns true when there was one to drop. A job that has already started
    /// is cancelled through its [`super::JobHandle`] instead, which is what
    /// `App::cancel_job` does.
    pub fn cancel(&mut self, id: JobId) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|r| r.id != id);
        self.launched.remove(&id);
        self.waiting.len() != before
    }

    /// Contending jobs that hold a slot: launched and not yet finished.
    fn occupied(&self, jobs: &[JobStatus]) -> usize {
        jobs.iter()
            .filter(|j| serialises(j.kind) && j.finished.is_none() && self.launched.contains(&j.id))
            .count()
    }

    /// Drop launch records for jobs that have reported `Finished`, so the set
    /// cannot grow for the life of the process.
    fn forget_finished(&mut self, jobs: &[JobStatus]) {
        if self.launched.is_empty() {
            return;
        }
        let done: HashSet<JobId> = jobs
            .iter()
            .filter(|j| j.finished.is_some())
            .map(|j| j.id)
            .collect();
        self.launched.retain(|id| !done.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpsConfig;
    use crate::ops::{ConflictRequest, JobEvent, JobFailure, JobSpec, JobSummary, TreeStats};
    use crate::vfs::VfsPath;
    use std::time::Duration;

    fn request(id: u64, kind: JobKind) -> JobRequest {
        JobRequest {
            id: JobId(id),
            spec: JobSpec::new(
                kind,
                vec![VfsPath::local("/tmp/a")],
                Some(VfsPath::local("/tmp/b")),
            ),
            queue: false,
        }
    }

    fn started(id: u64, kind: JobKind) -> JobStatus {
        let mut status = JobStatus::queued(JobId(id), kind);
        status.apply(&JobEvent::Started {
            kind,
            files_total: 3,
            bytes_total: 300,
        });
        status
    }

    fn summary(kind: JobKind, failures: Vec<JobFailure>, cancelled: bool) -> JobSummary {
        JobSummary {
            kind,
            files_done: 3,
            dirs_done: 0,
            bytes_done: 300,
            skipped: 0,
            failures,
            cancelled,
            elapsed: Duration::from_secs(1),
            sized: Vec::<(VfsPath, TreeStats)>::new(),
            differing: Vec::new(),
        }
    }

    fn finish(status: &mut JobStatus, failures: Vec<JobFailure>, cancelled: bool) {
        status.apply(&JobEvent::Finished {
            summary: Box::new(summary(status.kind, failures, cancelled)),
        });
    }

    #[test]
    fn every_state_a_job_can_be_in_is_named() {
        let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
        assert_eq!(state_of(&status), JobState::Pending);

        status.apply(&JobEvent::Started {
            kind: JobKind::Copy,
            files_total: 1,
            bytes_total: 1,
        });
        assert_eq!(state_of(&status), JobState::Running);

        status.apply(&JobEvent::NeedsDecision {
            request: Box::new(ConflictRequest {
                source: VfsPath::local("/tmp/a"),
                dest: VfsPath::local("/tmp/b"),
                source_size: 1,
                dest_size: 2,
                source_mtime: None,
                dest_mtime: None,
                both_dirs: false,
                dest_is_dir: false,
            }),
        });
        assert_eq!(
            state_of(&status),
            JobState::Waiting,
            "parked on a conflict, which is not the same as running"
        );
        assert!(state_of(&status).needs_attention());

        finish(&mut status, Vec::new(), false);
        assert_eq!(state_of(&status), JobState::Done);
        assert!(state_of(&status).is_terminal());

        let mut failed = started(2, JobKind::Copy);
        finish(
            &mut failed,
            vec![JobFailure {
                path: VfsPath::local("/tmp/x"),
                error: "no".to_string(),
            }],
            false,
        );
        assert_eq!(state_of(&failed), JobState::Failed);

        let mut cancelled = started(3, JobKind::Move);
        finish(&mut cancelled, Vec::new(), true);
        assert_eq!(state_of(&cancelled), JobState::Cancelled);
    }

    #[test]
    fn a_second_copy_is_enqueued_rather_than_refused() {
        // "a second `F5` while a copy is running enqueues
        // rather than refusing".
        let mut queue = JobQueue::new(DEFAULT_CONCURRENCY);
        let mut jobs: Vec<JobStatus> = Vec::new();

        let first = queue.submit(request(1, JobKind::Copy), &jobs);
        assert!(first.is_some(), "the first one starts at once");
        jobs.push(started(1, JobKind::Copy));

        let second = queue.submit(request(2, JobKind::Copy), &jobs);
        assert!(second.is_none(), "the second one waits");
        assert_eq!(queue.len(), 1);
        assert!(queue.contains(JobId(2)));
        jobs.push(JobStatus::queued(JobId(2), JobKind::Copy));

        // Nothing is released while the first is still going.
        assert!(queue.release(&jobs).is_empty());

        finish(&mut jobs[0], Vec::new(), false);
        let released = queue.release(&jobs);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].id, JobId(2));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_released_job_holds_its_slot_before_it_reports_started() {
        // The frame between "spawned" and "Started" must not look idle, or the
        // queue hands the same slot out twice.
        let mut queue = JobQueue::new(1);
        let mut jobs = vec![
            JobStatus::queued(JobId(1), JobKind::Copy),
            JobStatus::queued(JobId(2), JobKind::Copy),
        ];
        assert!(queue.submit(request(1, JobKind::Copy), &jobs).is_some());
        assert!(queue.submit(request(2, JobKind::Copy), &jobs).is_none());

        assert!(
            queue.release(&jobs).is_empty(),
            "job 1 has not reported Started yet, and still holds the slot"
        );

        jobs[0] = started(1, JobKind::Copy);
        assert!(queue.release(&jobs).is_empty());
        finish(&mut jobs[0], Vec::new(), false);
        assert_eq!(queue.release(&jobs).len(), 1);
    }

    #[test]
    fn a_size_walk_never_waits_behind_a_copy() {
        let mut queue = JobQueue::new(1);
        let jobs = vec![started(1, JobKind::Copy)];
        assert!(queue.submit(request(1, JobKind::Copy), &[]).is_some());

        let size = JobRequest {
            id: JobId(9),
            spec: JobSpec::size(vec![VfsPath::local("/tmp/a")]),
            queue: false,
        };
        assert!(
            queue.submit(size, &jobs).is_some(),
            "`Space` on a directory must not wait behind a four-hour copy"
        );
        assert!(queue.is_empty());
        assert!(!serialises(JobKind::Size));
        assert!(!serialises(JobKind::Mkdir));
        assert!(serialises(JobKind::Delete { trash: true }));
    }

    #[test]
    fn f2_queue_waits_even_when_a_slot_is_free() {
        // the `F2 Queue`: "append to the background queue instead
        // of starting now". The point is that the user asked to keep working.
        let mut queue = JobQueue::new(1);
        queue.enqueue(request(1, JobKind::Copy));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pending().len(), 1);

        let jobs = vec![JobStatus::queued(JobId(1), JobKind::Copy)];
        assert_eq!(queue.release(&jobs).len(), 1, "and then it runs");
    }

    #[test]
    fn a_queued_job_keeps_its_place_in_line() {
        let mut queue = JobQueue::new(1);
        let jobs = vec![started(1, JobKind::Copy)];
        assert!(queue.submit(request(1, JobKind::Copy), &[]).is_some());
        for id in 2..=4 {
            assert!(queue.submit(request(id, JobKind::Move), &jobs).is_none());
        }
        let ids: Vec<JobId> = queue.pending().map(|r| r.id).collect();
        assert_eq!(ids, vec![JobId(2), JobId(3), JobId(4)]);

        assert!(queue.cancel(JobId(3)), "dropped before it ever ran");
        assert!(!queue.cancel(JobId(3)), "and only once");
        let ids: Vec<JobId> = queue.pending().map(|r| r.id).collect();
        assert_eq!(ids, vec![JobId(2), JobId(4)]);
    }

    #[test]
    fn a_free_slot_is_not_jumped_by_a_later_request() {
        // Something queued behind a running job must not be overtaken the
        // moment that job ends, just because a new request arrived first.
        let mut queue = JobQueue::new(1);
        let jobs = vec![started(1, JobKind::Copy)];
        assert!(queue.submit(request(1, JobKind::Copy), &[]).is_some());
        assert!(queue.submit(request(2, JobKind::Copy), &jobs).is_none());
        assert!(
            queue.submit(request(3, JobKind::Copy), &[]).is_none(),
            "there is a queue, so join it"
        );
        let ids: Vec<JobId> = queue.pending().map(|r| r.id).collect();
        assert_eq!(ids, vec![JobId(2), JobId(3)]);
    }

    #[test]
    fn turning_the_background_queue_off_starts_everything_rather_than_refusing() {
        // the design forbids refusing a second `F5`, so the only thing the
        // key can turn off is the waiting.
        let cfg = OpsConfig {
            background_queue: false,
            ..OpsConfig::default()
        };
        let mut queue = JobQueue::from_config(&cfg);
        let jobs = vec![started(1, JobKind::Copy)];
        assert!(queue.submit(request(1, JobKind::Copy), &[]).is_some());
        assert!(
            queue.submit(request(2, JobKind::Copy), &jobs).is_some(),
            "nothing waits"
        );
        assert!(queue.is_empty());

        let on = JobQueue::from_config(&OpsConfig::default());
        assert_eq!(on.limit(), DEFAULT_CONCURRENCY);
    }

    #[test]
    fn a_limit_of_zero_is_read_as_one() {
        let mut queue = JobQueue::new(0);
        assert_eq!(queue.limit(), 1);
        assert!(queue.submit(request(1, JobKind::Copy), &[]).is_some());
    }

    #[test]
    fn the_counts_are_what_the_queue_view_groups_by() {
        let mut running = started(1, JobKind::Copy);
        let mut failed = started(2, JobKind::Move);
        finish(
            &mut failed,
            vec![JobFailure {
                path: VfsPath::local("/tmp/x"),
                error: "no".to_string(),
            }],
            false,
        );
        let pending = JobStatus::queued(JobId(3), JobKind::Copy);
        running.background = true;

        let jobs = vec![running, failed, pending];
        let counts = counts(&jobs);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.active(), 2);
        assert!(!counts.is_empty());
        assert_eq!(counts.indicator().as_deref(), Some("2 job(s)"));

        assert!(QueueCounts::default().is_empty());
        assert_eq!(QueueCounts::default().indicator(), None);
    }

    #[test]
    fn a_job_needing_an_answer_takes_over_the_indicator() {
        // a backgrounded job blocked on a conflict "does not sit
        // silently blocked" - the indicator changes to say so.
        let mut waiting = started(1, JobKind::Copy);
        waiting.apply(&JobEvent::NeedsDecision {
            request: Box::new(ConflictRequest {
                source: VfsPath::local("/tmp/a"),
                dest: VfsPath::local("/tmp/dest/a"),
                source_size: 1,
                dest_size: 2,
                source_mtime: None,
                dest_mtime: None,
                both_dirs: false,
                dest_is_dir: false,
            }),
        });
        let jobs = vec![waiting, started(2, JobKind::Copy)];
        let counts = counts(&jobs);
        assert_eq!(counts.waiting, 1);
        assert_eq!(
            counts.indicator().as_deref(),
            Some("1 job(s) need an answer")
        );

        let row = row(&jobs[0]);
        assert_eq!(row.state, JobState::Waiting);
        assert!(row.detail.contains("/tmp/dest/a"), "{}", row.detail);
    }

    #[test]
    fn a_row_says_what_the_job_is_and_how_far_it_got() {
        let mut jobs = vec![
            JobStatus::queued(JobId(1), JobKind::Copy),
            started(2, JobKind::Move),
        ];
        jobs[1].files_done = 1;
        finish(&mut jobs[0], Vec::new(), false);

        let rows = rows(&jobs);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].state, JobState::Done);
        assert_eq!(rows[0].title, "Copying");
        assert!(rows[0].detail.contains("copied"), "{}", rows[0].detail);
        assert_eq!(rows[1].title, "Moving");
        assert_eq!(rows[1].detail, "1 / 3 files");
        assert_eq!(rows[1].fraction, Some(0.0));

        assert_eq!(JobState::Pending.to_string(), "queued");
        assert_eq!(JobState::Failed.id(), "failed");
        assert_eq!(JobState::ALL.len(), 6);
    }
}
