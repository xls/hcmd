//! What the UI knows about a job: the event stream and the status folded
//! from it.
//!
//! The worker talks in [`JobEvent`]s; [`JobStatus::apply`] folds them into
//! the one record the progress dialog, the queue and the quit prompt read.
//! [`JobHandle`] and [`CancelFlag`] are the UI's end of the same channel,
//! which is why they sit with the events rather than with the worker's
//! context.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::vfs::VfsPath;

use super::kind::{JobId, JobKind};
use super::spec::{ConflictRequest, Decision, JobSpec};
use super::summary::{JobFailure, JobSummary};

/// What a running job tells the UI (the progress dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    /// The worker has started. `files_total` and `bytes_total` are zero when
    /// the totals are not known up front - a [`JobKind::Size`] walk cannot
    /// know them, and a copy does because the design computes the selection
    /// statistics before the dialog opens.
    Started {
        /// Which job this is.
        kind: JobKind,
        /// How many files the batch holds, or 0 when unknown.
        files_total: u64,
        /// How many bytes the batch holds, or 0 when unknown.
        bytes_total: u64,
    },
    /// Everything the design put in the progress dialog.
    Progress {
        /// The file being worked on right now, as **a path with enough parent
        /// to be unambiguous**: a batch routinely holds twenty
        /// files called `index.html`, and the renderer crops this from the
        /// *left* so the filename and its nearest parents survive.
        file: String,
        /// Bytes written of the current file - the top bar.
        file_bytes_done: u64,
        /// The current file's size, or 0 when it is not known.
        file_bytes_total: u64,
        /// Files finished.
        files_done: u64,
        /// Files in the batch, or 0 when unknown.
        files_total: u64,
        /// Bytes finished - the bottom bar.
        bytes_done: u64,
        /// Bytes in the batch, or 0 when unknown.
        bytes_total: u64,
        /// The **displayed** rate: a moving average over `ops.rate_window`,
        /// so a stalled transfer shows as stalled.
        ///
        /// `None` below `ops.rate_min_samples`, where the dialog shows `-`
        /// rather than a number it cannot stand behind.
        throughput: Option<u64>,
        /// Estimated time remaining, from the **cumulative** average, which
        /// does not jump the way the windowed rate does.
        /// `None` when no honest estimate exists.
        eta: Option<Duration>,
        /// How long the job has been running, for the elapsed line.
        elapsed: Duration,
    },
    /// The worker is blocked on a conflict and will not proceed until the UI
    /// answers on [`JobHandle::decisions`].
    NeedsDecision {
        /// What is in the way.
        request: Box<ConflictRequest>,
    },
    /// One item failed. The batch continues.
    Failed {
        /// What failed.
        path: VfsPath,
        /// Why.
        error: String,
    },
    /// The worker is done, cleanly or otherwise. Always the last event.
    Finished {
        /// The end-of-batch summary.
        summary: Box<JobSummary>,
    },
}

/// A [`JobEvent`] with the id of the job that produced it.
///
/// The id lives here rather than in every variant for the same reason
/// [`crate::app::VfsEvent`] repeats `side`/`tab`/`generation`: something has to
/// route the event, and a wrapper says it once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobUpdate {
    /// Which job.
    pub id: JobId,
    /// What happened.
    pub event: JobEvent,
}

/// A job the UI has asked for and the event loop has not spawned yet.
///
/// The exact analogue of [`crate::app::ReadRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRequest {
    /// The id already allocated for it, so the UI can register a status row
    /// before the worker exists.
    pub id: JobId,
    /// What to run.
    pub spec: JobSpec,
    /// `F2 Queue` was pressed: "append to the background queue instead of
    /// starting now". [`queue::JobQueue::enqueue`] rather than
    /// [`queue::JobQueue::submit`], so it waits even when a slot is free.
    pub queue: bool,
}

/// A shared "stop now" flag.
///
/// Cloned into the worker. `Esc` on the progress dialog sets it, and every
/// runner checks it between files; [`copy::copy_stream`] checks it on every
/// chunk.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// A flag that is not set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set it. Idempotent, and safe from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Has anyone cancelled?
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The UI's end of a running job.
///
/// Dropping it does **not** cancel - the events channel is what the UI holds -
/// so a handle can be kept in the background queue without stopping the work.
#[derive(Debug, Clone)]
pub struct JobHandle {
    /// Which job.
    pub id: JobId,
    /// What it is doing.
    pub kind: JobKind,
    /// Set this to stop it (the `Esc`).
    pub cancel: CancelFlag,
    /// Where an answer to [`JobEvent::NeedsDecision`] goes.
    pub decisions: mpsc::Sender<Decision>,
}

impl JobHandle {
    /// `Esc`: stop the job. The worker notices between files and inside the
    /// chunk loop, removes any partial destination, and reports
    /// [`JobSummary::cancelled`].
    ///
    /// A worker parked on [`JobContext::ask`] is also released, which is why
    /// this sends `Cancel` as well as setting the flag.
    pub fn cancel(&self) {
        self.cancel.cancel();
        let _ = self.decisions.try_send(Decision::Cancel);
    }

    /// Answer a [`JobEvent::NeedsDecision`].
    ///
    /// Non-blocking on purpose: [`crate::input::dispatch`] must never block,
    /// and the worker is parked waiting, so the one-slot channel is free.
    /// Returns false when the worker is already gone.
    pub fn answer(&self, decision: Decision) -> bool {
        self.decisions.try_send(decision).is_ok()
    }
}

/// Where a job is in its life. One value, so that "finished and also
/// waiting on a question" or "started but never queued" cannot be written
/// down at all - three shipped bugs came from deriving this from booleans
/// that could disagree.
///
/// Only [`JobStatus::apply`] and the few named transitions on [`JobStatus`]
/// move a job between these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Admitted, no worker yet: waiting for a slot, or held at a gate.
    Queued,
    /// The worker is going.
    Running,
    /// The worker is parked on a conflict and will not proceed until the UI
    /// answers it.
    Blocked {
        /// What is in the way.
        on: Box<ConflictRequest>,
    },
    /// The worker is done, cleanly or otherwise.
    Finished {
        /// The end-of-batch summary.
        summary: Box<JobSummary>,
        /// Whether the finish has been acted on - reported, its dialog
        /// closed - so that happens once and not on every frame.
        presented: bool,
    },
}

/// How a job is shown, orthogonal to where it is in its life: foreground
/// and background are two views of one job, and this is which one it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Its progress dialog belongs on screen.
    Foreground {
        /// The user closed the dialog with `Esc` and the worker has not
        /// yet noticed the cancel, so the dialog is not put straight back.
        dismissed: bool,
    },
    /// `F2` sent it away; it is listed in the queue view and nowhere else.
    Background,
}

/// The live state of one job, as the progress dialog and the queue view
/// render it.
///
/// [`crate::app::App::apply_job_event`] keeps this in step with the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    /// Which job.
    pub id: JobId,
    /// What it is doing.
    pub kind: JobKind,
    /// The file currently being worked on, with enough parent to be
    /// unambiguous.
    pub file: String,
    /// Bytes written of the current file - the top bar.
    pub file_bytes_done: u64,
    /// The current file's size, 0 when unknown.
    pub file_bytes_total: u64,
    /// Files finished.
    pub files_done: u64,
    /// Files in the batch, 0 when unknown.
    pub files_total: u64,
    /// Bytes finished.
    pub bytes_done: u64,
    /// Bytes in the batch, 0 when unknown.
    pub bytes_total: u64,
    /// The displayed rate: a windowed average, `None` while there is not
    /// enough of one to show.
    pub throughput: Option<u64>,
    /// Estimated time remaining, when one can honestly be given.
    pub eta: Option<Duration>,
    /// How long it has been running.
    pub elapsed: Duration,
    /// Where it is in its life. Read through the accessors below; written
    /// only by [`JobStatus::apply`] and the named transitions.
    state: JobState,
    /// Which view it has: on screen, or in the queue only.
    view: View,
    /// Failures seen so far.
    pub failures: Vec<JobFailure>,
}

impl JobStatus {
    /// A fresh status row for a queued job, before the worker exists.
    pub fn queued(id: JobId, kind: JobKind) -> Self {
        Self {
            id,
            kind,
            file: String::new(),
            file_bytes_done: 0,
            file_bytes_total: 0,
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            throughput: None,
            eta: None,
            elapsed: Duration::ZERO,
            state: JobState::Queued,
            view: View::Foreground { dismissed: false },
            failures: Vec::new(),
        }
    }

    /// Fold one event in. The one place the worker's word moves a job
    /// between states.
    pub fn apply(&mut self, event: &JobEvent) {
        match event {
            JobEvent::Started {
                kind,
                files_total,
                bytes_total,
            } => {
                self.kind = *kind;
                self.files_total = *files_total;
                self.bytes_total = *bytes_total;
                self.start();
            }
            JobEvent::Progress {
                file,
                file_bytes_done,
                file_bytes_total,
                files_done,
                files_total,
                bytes_done,
                bytes_total,
                throughput,
                eta,
                elapsed,
            } => {
                self.file.clone_from(file);
                self.file_bytes_done = *file_bytes_done;
                self.file_bytes_total = *file_bytes_total;
                self.files_done = *files_done;
                self.files_total = *files_total;
                self.bytes_done = *bytes_done;
                self.bytes_total = *bytes_total;
                self.throughput = *throughput;
                self.eta = *eta;
                self.elapsed = *elapsed;
                self.start();
            }
            JobEvent::NeedsDecision { request } => {
                // A finished job has nothing left to be blocked on; the
                // question, if it arrives late, is moot.
                if !self.is_finished() {
                    self.state = JobState::Blocked {
                        on: request.clone(),
                    };
                }
            }
            JobEvent::Failed { path, error } => self.failures.push(JobFailure {
                path: path.clone(),
                error: error.clone(),
            }),
            JobEvent::Finished { summary } => {
                self.files_done = summary.files_done;
                self.bytes_done = summary.bytes_done;
                self.elapsed = summary.elapsed;
                self.state = JobState::Finished {
                    summary: summary.clone(),
                    presented: false,
                };
            }
        }
    }

    /// The worker has spoken: a queued job is now running. A job already
    /// past that - blocked, or finished - stays where it is.
    fn start(&mut self) {
        if self.state == JobState::Queued {
            self.state = JobState::Running;
        }
    }

    /// The question was answered: a blocked job is running again.
    pub fn unblock(&mut self) {
        if self.is_blocked() {
            self.state = JobState::Running;
        }
    }

    /// The finish has been reported and its dialog closed; it is not
    /// reported again. A no-op on a job that has not finished.
    pub fn mark_presented(&mut self) {
        if let JobState::Finished { presented, .. } = &mut self.state {
            *presented = true;
        }
    }

    /// `F2`: send the dialog away and leave the job running.
    pub fn send_to_background(&mut self) {
        self.view = View::Background;
    }

    /// `Enter` on the queue view: bring it back "exactly as it was". The
    /// user asked to see it, so an earlier `Esc` no longer keeps it away.
    pub fn bring_to_foreground(&mut self) {
        self.view = View::Foreground { dismissed: false };
    }

    /// `Esc` on the progress dialog: do not put it back while the worker
    /// winds down. Nothing to dismiss for a backgrounded job.
    pub fn dismiss(&mut self) {
        if let View::Foreground { dismissed } = &mut self.view {
            *dismissed = true;
        }
    }

    /// Where the job is in its life.
    pub fn state(&self) -> &JobState {
        &self.state
    }

    /// Which view it has.
    pub fn view(&self) -> View {
        self.view
    }

    /// True once the worker has spoken at all: running, blocked or done.
    pub fn has_started(&self) -> bool {
        self.state != JobState::Queued
    }

    /// True while the job is neither finished nor waiting for an answer.
    pub fn is_running(&self) -> bool {
        self.state == JobState::Running
    }

    /// True while the worker is parked on a conflict. a job blocked on a
    /// conflict "does not sit silently blocked": this is what the queue view
    /// and the key-bar indicator ask.
    pub fn is_blocked(&self) -> bool {
        matches!(self.state, JobState::Blocked { .. })
    }

    /// [`JobStatus::is_blocked`], under the name the key bar uses.
    pub fn needs_attention(&self) -> bool {
        self.is_blocked()
    }

    /// True once the worker is done. A finished status may be dropped from
    /// the queue view whenever the UI likes.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, JobState::Finished { .. })
    }

    /// True once the finish has been acted on.
    pub fn is_presented(&self) -> bool {
        matches!(
            self.state,
            JobState::Finished {
                presented: true,
                ..
            }
        )
    }

    /// `F2` sent it to the background.
    pub fn is_background(&self) -> bool {
        self.view == View::Background
    }

    /// The user closed its progress dialog and it is still in the
    /// foreground view.
    pub fn is_dismissed(&self) -> bool {
        self.view == View::Foreground { dismissed: true }
    }

    /// The end-of-batch summary, once there is one.
    pub fn summary(&self) -> Option<&JobSummary> {
        match &self.state {
            JobState::Finished { summary, .. } => Some(summary),
            JobState::Queued | JobState::Running | JobState::Blocked { .. } => None,
        }
    }

    /// The conflict the worker is parked on, if any. The UI answers with
    /// [`crate::app::App::answer_job`].
    pub fn pending_decision(&self) -> Option<&ConflictRequest> {
        match &self.state {
            JobState::Blocked { on } => Some(on),
            JobState::Queued | JobState::Running | JobState::Finished { .. } => None,
        }
    }

    /// The **batch** bar: completion as a fraction of the byte total, `None`
    /// when there is no total to divide by.
    ///
    /// The numbers beside the bar come off the fields directly; only the bar
    /// needs a fraction.
    pub fn fraction(&self) -> Option<f64> {
        fraction(self.bytes_done, self.bytes_total)
    }

    /// The **current file** bar (the top bar).
    ///
    /// `None` when the file's size is unknown; the caller also omits the bar
    /// below `ops.file_bar_min_size`, since a bar that only flashes is noise.
    pub fn file_fraction(&self) -> Option<f64> {
        fraction(self.file_bytes_done, self.file_bytes_total)
    }

    /// Should the per-file bar be drawn at all?
    pub fn show_file_bar(&self, min_size: u64) -> bool {
        self.file_bytes_total >= min_size && self.file_bytes_total > 0
    }

    /// Should the batch bar be drawn? the design omits it for a batch of
    /// one file, where it would only duplicate the file bar.
    pub fn show_batch_bar(&self) -> bool {
        self.files_total != 1
    }
}

/// How the quit prompt names what is still running.
///
/// > Quitting with a transfer in progress always prompts regardless of that
/// > setting, naming what is still running.
///
/// One line per unfinished job - the verb and how far it has got - so the
/// prompt answers "what would I be stopping?" rather than merely asserting
/// that something is. A [`JobKind::Size`] is deliberately **not** here: it
/// reads metadata and writes nothing, so quitting during one loses no work and
/// the word is *transfer*. [`JobKind::is_destructive`] is the same distinction
/// the delete confirmation already turns on.
///
/// The list is capped: at 60x15 a prompt naming nine queued jobs
/// would not fit, and the count in the last line carries what the cap drops.
pub fn running_job_lines(jobs: &[JobStatus]) -> Vec<String> {
    /// How many jobs are named before the rest become a count.
    const NAMED: usize = 3;

    let running: Vec<&JobStatus> = jobs
        .iter()
        .filter(|j| !j.is_finished() && j.kind.is_destructive())
        .collect();
    if running.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = running
        .iter()
        .take(NAMED)
        .map(|j| format!("{}: {}", j.kind.title(), job_extent(j)))
        .collect();
    let hidden = running.len().saturating_sub(NAMED);
    if hidden > 0 {
        lines.push(format!("and {hidden} more."));
    }
    lines
}

/// How far one job has got, in the units it knows.
///
/// Bytes when there are any to report and files otherwise, because a delete
/// moves no bytes and a copy that has not started yet knows neither.
fn job_extent(job: &JobStatus) -> String {
    if job.bytes_total > 0 {
        format!(
            "{} of {}",
            crate::panel::format::human_size(job.bytes_done),
            crate::panel::format::human_size(job.bytes_total)
        )
    } else if job.files_total > 0 {
        format!("{} of {} files", job.files_done, job.files_total)
    } else if job.has_started() {
        "in progress".to_string()
    } else {
        "queued".to_string()
    }
}

/// `done / total` as a fraction, `None` when there is no total.
fn fraction(done: u64, total: u64) -> Option<f64> {
    if total == 0 {
        return None;
    }
    // Both sides through f64 deliberately: this is a bar width, not
    // accounting, and integer division would quantize it to nothing.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a bar width, not accounting; integer division would quantize it away"
    )]
    Some((done as f64 / total as f64).clamp(0.0, 1.0))
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
