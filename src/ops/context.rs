//! The worker's side of a job: progress, questions, cancellation, failure.
//!
//! Runners never touch the channel directly; every rule about cancellation,
//! progress rate and error collection is enforced in [`JobContext`] so it
//! cannot be got wrong per operation. [`FailReason`] and [`is_fatal`] decide
//! which failures end the batch - a dead connection does, a refusal this
//! program made itself does not - and [`RateMeter`] is the honest rate: a
//! moving window that can show a stall.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::OpsConfig;
use crate::error::Error;
use crate::vfs::VfsPath;

use super::kind::{JobId, JobKind};
use super::spec::{ConflictRequest, Decision};
use super::status::{CancelFlag, JobEvent, JobUpdate};
use super::summary::{JobFailure, JobSummary};
use super::walk::TreeStats;
use super::{JOB_CHANNEL_DEPTH, PROGRESS_INTERVAL};

/// Everything a runner needs to report progress, ask questions and notice
/// cancellation.
///
/// Runners never touch the channel directly; every rule the design states
/// about cancellation, progress rate and error collection is enforced here so
/// it cannot be got wrong per operation.
pub struct JobContext {
    id: JobId,
    kind: JobKind,
    tx: mpsc::Sender<JobUpdate>,
    decisions: mpsc::Receiver<Decision>,
    cancel: CancelFlag,

    file: String,
    file_bytes_done: u64,
    file_bytes_total: u64,
    files_done: u64,
    files_total: u64,
    dirs_done: u64,
    bytes_done: u64,
    bytes_total: u64,
    skipped: u64,

    failures: Vec<JobFailure>,
    sized: Vec<(VfsPath, TreeStats)>,
    differing: Vec<String>,
    /// Where a [`JobKind::CompareFiles`] pair first differed.
    first_difference: Option<u64>,

    /// The one place bytes are counted, above the `Vfs` trait, so every backend
    /// reports the same way.
    rate: RateMeter,
    started_at: Instant,
    last_emit: Option<Instant>,
    /// The UI dropped the receiver. Indistinguishable from a cancel, and
    /// treated as one - exactly how a dropped `read_dir` receiver stops the
    /// listing walk.
    lost: bool,
    /// A failure that stops the batch rather than one file.
    ///
    /// Set by [`JobContext::fail`] when the reason is [`is_fatal`], read by
    /// the copy and delete loops. It is not a cancel: the file that failed has
    /// already been reported, the summary keeps it, and `Retry failures` will
    /// fail again until the user reconnects - which is honest, and better than
    /// a summary with two hundred identical rows.
    ///
    fatal: bool,
}

/// Why one file failed, and whether the rest of the batch can still be tried.
///
/// A seam rather than a `String`, so [`JobContext::fail`] can tell a lost
/// connection from a missing file without every one of its forty call sites
/// having to say which it is passing.
///
/// Every way of building one carries a classification. There is deliberately
/// no route that defaults to "not fatal": that default is what let a dead
/// connection reach the failure summary as two hundred ordinary rows.
pub struct FailReason {
    /// Already phrased for the failure summary.
    text: String,
    /// True when the whole batch has to stop.
    fatal: bool,
}

impl From<Error> for FailReason {
    fn from(err: Error) -> Self {
        Self {
            fatal: is_fatal(&err),
            text: err.to_string(),
        }
    }
}

impl From<&Error> for FailReason {
    fn from(err: &Error) -> Self {
        Self {
            fatal: is_fatal(err),
            text: err.to_string(),
        }
    }
}

impl FailReason {
    /// A refusal this program decided by itself, before anything was
    /// attempted: an empty name, a directory being copied into itself, a
    /// backend that has no directories to create.
    ///
    /// Never fatal, and that is a claim about the *cause* rather than a
    /// default: nothing about the connection or the filesystem changed, so the
    /// next file in the batch is still worth trying. Anything that came back
    /// from a backend must arrive as an [`Error`] instead, so [`is_fatal`] can
    /// see it.
    pub fn refused(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fatal: false,
        }
    }

    /// A failure that stops the batch, for a caller that already knows it is
    /// looking at one and has only a sentence left to report.
    pub fn stops_the_batch(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fatal: true,
        }
    }
}

impl From<String> for FailReason {
    /// Text that has already lost its [`Error`].
    ///
    /// The classification is **recovered from the wording** rather than
    /// assumed to be harmless, because assuming it was harmless is the bug
    /// this seam exists to prevent: a `fatal: false` hard-coded here made
    /// every `format!` call site permanently non-fatal whatever the cause, and
    /// a dropped connection came out as one ordinary failure per remaining
    /// file. Recognising the sentence is a net under the real fix, which is to
    /// hand [`JobContext::fail`] the [`Error`] itself.
    fn from(text: String) -> Self {
        let fatal = names_a_dead_connection(&text);
        Self { text, fatal }
    }
}

impl From<&str> for FailReason {
    /// [`FailReason::from<String>`]'s reasoning, for a literal.
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// Whether a sentence is one of the two a lost connection produces.
///
/// It matches on [`crate::error::CONNECTION_LOST_TEXT`] and
/// [`crate::error::CONNECTION_CLOSED_TEXT`], which are the same constants
/// [`Error`]'s `Display` is built from, so the two cannot drift apart. It is
/// `contains` rather than `ends_with` because a caller may have wrapped the
/// sentence in context of its own before it got here.
fn names_a_dead_connection(text: &str) -> bool {
    text.contains(crate::error::CONNECTION_LOST_TEXT)
        || text.contains(crate::error::CONNECTION_CLOSED_TEXT)
}

/// Whether a failure stops the batch instead of failing one file.
///
///
/// True for [`Error::ConnectionLost`] and for [`Error::ConnectionClosed`], and
/// nothing else. Every remaining file would fail identically, and two hundred
/// identical lines in the failure summary say less than one. The second is the
/// same event told from this end - the panel disconnected, or a tab closed,
/// while the job was still running - and it used to be an [`Error::Msg`],
/// which slipped past this test and produced exactly the summary it exists to
/// prevent. An [`Error::Msg`] whose text is one of those two sentences answers
/// true for that reason: the trap was still armed for the next site that
/// hand-rolled the wording, and it had been sprung twice.
///
/// **And for a lost connection that came back through `std::io`**, which is
/// the case the contract's one-line rule missed: a read or a write failing
/// mid-transfer travels out of `Read::read` or `Write::write` as an
/// `io::Error`, and `ops::copy` wraps that as [`Error::Bare`]. The SFTP
/// backend builds those errors as
/// `io::Error::new(ErrorKind::ConnectionAborted, Error::ConnectionLost(..))`,
/// so both halves of the same event answer the same way here rather than one
/// of them silently failing two hundred files one at a time.
pub fn is_fatal(err: &Error) -> bool {
    match err {
        // A connection that was closed under a running job is the same event
        // as one that dropped, told from the other end: every remaining file
        // would fail identically.
        Error::ConnectionLost(_) | Error::ConnectionClosed(_) => true,
        Error::Bare(io) => io_is_fatal(io),
        Error::Io { source, .. } => io_is_fatal(source),
        // The last resort. A message is a failure that has already lost its
        // variant, and hand-rolling one of the two sentences above into an
        // `Error::msg` is how a dead connection reached the summary as N
        // ordinary rows once already. Reading the wording back is not as good
        // as never losing the variant - which is why the sites that did it
        // have been changed - but it is what stops the next one being silent.
        Error::Msg(text) => names_a_dead_connection(text),
        Error::Config { .. }
        | Error::Binding { .. }
        | Error::Unsupported(_)
        | Error::NotFound(_)
        | Error::InvalidPath(_)
        | Error::Cancelled => false,
    }
}

/// [`is_fatal`] for an error that has been through `std::io`.
fn io_is_fatal(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::ConnectionAborted {
        return true;
    }
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .is_some_and(|inner| matches!(inner, Error::ConnectionLost(_)))
}

impl JobContext {
    /// Build one. The event loop does not call this; [`spawn`] does.
    pub(super) fn new(
        id: JobId,
        kind: JobKind,
        tx: mpsc::Sender<JobUpdate>,
        decisions: mpsc::Receiver<Decision>,
        cancel: CancelFlag,
        rate: RateMeter,
    ) -> Self {
        Self {
            id,
            kind,
            tx,
            decisions,
            cancel,
            rate,
            file: String::new(),
            file_bytes_done: 0,
            file_bytes_total: 0,
            files_done: 0,
            files_total: 0,
            dirs_done: 0,
            bytes_done: 0,
            bytes_total: 0,
            skipped: 0,
            failures: Vec::new(),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
            started_at: Instant::now(),
            last_emit: None,
            fatal: false,
            lost: false,
        }
    }

    /// A context wired to a channel the caller owns, for tests and for driving
    /// a runner synchronously.
    ///
    /// The returned receiver is the UI's end: drop it and the context reports
    /// [`JobContext::cancelled`], which is the same cancellation path the real
    /// event loop uses.
    pub fn for_test(
        kind: JobKind,
    ) -> (
        Self,
        mpsc::Receiver<JobUpdate>,
        mpsc::Sender<Decision>,
        CancelFlag,
    ) {
        let (tx, rx) = mpsc::channel(JOB_CHANNEL_DEPTH);
        let (dtx, drx) = mpsc::channel(4);
        let cancel = CancelFlag::new();
        let rate = RateMeter::from_config(&OpsConfig::default());
        let ctx = Self::new(JobId(0), kind, tx, drx, cancel.clone(), rate);
        (ctx, rx, dtx, cancel)
    }

    /// Which job this is.
    pub const fn id(&self) -> JobId {
        self.id
    }

    /// What it is doing.
    pub const fn kind(&self) -> JobKind {
        self.kind
    }

    /// Should the runner stop *right now*?
    ///
    /// True when `Esc` set the flag or when the UI dropped the receiver. Every
    /// loop in every runner is guarded by this, between files and inside the
    /// chunk loop.
    pub fn cancelled(&self) -> bool {
        self.lost || self.cancel.is_cancelled()
    }

    /// Announce the batch totals and emit [`JobEvent::Started`].
    ///
    /// Pass zeros when the totals are not known; the progress dialog renders a
    /// count rather than a bar in that case, which is honest rather than a bar
    /// that lurches.
    pub fn start(&mut self, files_total: u64, bytes_total: u64) {
        self.files_total = files_total;
        self.bytes_total = bytes_total;
        let kind = self.kind;
        self.send(JobEvent::Started {
            kind,
            files_total,
            bytes_total,
        });
    }

    /// Set the file being worked on, and its size.
    ///
    /// `display` is a **path with enough parent to be unambiguous**, not a bare
    /// basename: the design requires it, because a batch routinely holds
    /// twenty files called `index.html`. `total` is that file's size, or 0 when
    /// it is not known; it drives the per-file bar and the decision about
    /// whether to draw one at all.
    ///
    /// Always forces a [`JobEvent::Progress`], so the dialog never shows a name
    /// the worker has moved on from.
    pub fn set_file(&mut self, display: &str, total: u64) {
        if self.file != display {
            self.file.clear();
            self.file.push_str(display);
        }
        self.file_bytes_done = 0;
        self.file_bytes_total = total;
        self.emit(true);
    }

    /// Account for bytes **the writer accepted**,
    /// rate-limiting the progress event.
    ///
    /// Returns false when the runner should stop, so a chunk loop reads
    /// `if !ctx.add_bytes(n) { … }` and cannot forget the check.
    pub fn add_bytes(&mut self, bytes: u64) -> bool {
        self.bytes_done = self.bytes_done.saturating_add(bytes);
        self.file_bytes_done = self.file_bytes_done.saturating_add(bytes);
        self.rate.record(bytes);
        self.emit(false);
        !self.cancelled()
    }

    /// Account for one finished file.
    pub fn add_file(&mut self) {
        self.add_files(1);
    }

    /// Account for several finished files at once, which is what a walk
    /// reporting a batch of entries needs.
    pub fn add_files(&mut self, count: u64) {
        self.files_done = self.files_done.saturating_add(count);
        self.emit(false);
    }

    /// Account for one finished directory.
    pub fn add_dir(&mut self) {
        self.dirs_done = self.dirs_done.saturating_add(1);
        self.emit(false);
    }

    /// Account for one item skipped by a conflict decision or a file mask.
    pub fn add_skipped(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    /// Record a completed [`JobKind::Size`] walk (the cache).
    pub fn add_sized(&mut self, path: VfsPath, stats: TreeStats) {
        self.sized.push((path, stats));
    }

    /// Record where a [`JobKind::CompareFiles`] pair first stopped agreeing.
    ///
    /// Set once, by the single-pair job; a batch comparison never calls it,
    /// because "the offset" is not a question a set of pairs has an answer to.
    pub fn set_first_difference(&mut self, at: u64) {
        self.first_difference = Some(at);
    }

    /// Record one facing pair that a [`JobKind::Compare`] found to differ.
    ///
    ///
    /// The twin of [`JobContext::add_sized`], and here for the same reason: a
    /// runner's findings ride home on the summary, and the summary is built
    /// from the context.
    pub fn add_differing(&mut self, name: String) {
        self.differing.push(name);
    }

    /// Record a per-item failure and report it.
    ///
    /// The batch goes on unless the reason classifies as fatal, in which case
    /// [`JobContext::fatal`] latches and the runner's loop is expected to
    /// stop. Prefer handing this the [`Error`] itself: a reason that has been
    /// flattened to a sentence can only be classified by recognising its
    /// wording.
    pub fn fail(&mut self, path: &VfsPath, error: impl Into<FailReason>) {
        let FailReason { text, fatal } = error.into();
        self.fatal = self.fatal || fatal;
        self.failures.push(JobFailure {
            path: path.clone(),
            error: text.clone(),
        });
        self.send(JobEvent::Failed {
            path: path.clone(),
            error: text,
        });
    }

    /// Whether a failure has stopped the batch.
    ///
    /// Distinct from [`JobContext::cancelled`]: a cancel cleans up its partial
    /// destination and reports "cancelled", while this leaves the failure
    /// summary saying what went wrong and offering `Retry failures`.
    ///
    pub const fn fatal(&self) -> bool {
        self.fatal
    }

    /// Ask the UI what to do about a conflict and block until it answers.
    ///
    ///
    /// `None` means the UI went away or cancelled, and the runner must stop.
    /// The worker is on the blocking pool, so parking here costs a blocking
    /// thread and never a runtime worker.
    pub fn ask(&mut self, request: ConflictRequest) -> Option<Decision> {
        self.send(JobEvent::NeedsDecision {
            request: Box::new(request),
        });
        if self.cancelled() {
            return None;
        }
        match self.decisions.blocking_recv() {
            Some(Decision::Cancel) | None => {
                self.lost = true;
                None
            }
            Some(other) => Some(other),
        }
    }

    /// Emit the final [`JobEvent::Finished`] and hand back the summary.
    ///
    /// Consumes the context so a runner cannot report progress after it has
    /// finished.
    pub fn finish(mut self) -> JobSummary {
        let summary = JobSummary {
            kind: self.kind,
            files_done: self.files_done,
            dirs_done: self.dirs_done,
            bytes_done: self.bytes_done,
            skipped: self.skipped,
            failures: std::mem::take(&mut self.failures),
            cancelled: self.cancelled(),
            elapsed: self.started_at.elapsed(),
            sized: std::mem::take(&mut self.sized),
            differing: std::mem::take(&mut self.differing),
            first_difference: self.first_difference,
        };
        self.send(JobEvent::Finished {
            summary: Box::new(summary.clone()),
        });
        summary
    }

    /// Progress so far, for a runner that needs to read its own counters.
    pub const fn counters(&self) -> (u64, u64) {
        (self.files_done, self.bytes_done)
    }

    /// Send a progress event, subject to [`PROGRESS_INTERVAL`] unless `force`.
    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && let Some(last) = self.last_emit
            && now.duration_since(last) < PROGRESS_INTERVAL
        {
            return;
        }
        self.last_emit = Some(now);
        let event = JobEvent::Progress {
            file: self.file.clone(),
            file_bytes_done: self.file_bytes_done,
            file_bytes_total: self.file_bytes_total,
            files_done: self.files_done,
            files_total: self.files_total,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            // Windowed for display, cumulative for the estimate.
            //
            throughput: self.rate.rate(),
            eta: self.rate.eta(self.bytes_done, self.bytes_total),
            elapsed: self.started_at.elapsed(),
        };
        self.send(event);
    }

    /// A failed send means the UI dropped the receiver, which cancels the job
    /// exactly as dropping a `read_dir` receiver stops the listing.
    fn send(&mut self, event: JobEvent) {
        if self
            .tx
            .blocking_send(JobUpdate { id: self.id, event })
            .is_err()
        {
            self.lost = true;
        }
    }
}

/// Bytes per second over an elapsed span, `None` when there is nothing to
/// divide by.
fn rate_of(bytes: u64, elapsed: Duration) -> Option<u64> {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || bytes == 0 {
        return None;
    }
    // Precision, not accounting: this figure is rounded to one decimal place
    // and a suffix before anyone sees it, and the guard above is what keeps the
    // division honest.
    #[allow(
        clippy::cast_precision_loss,
        reason = "rounded to one decimal place and a suffix before anyone sees it"
    )]
    let rate = (bytes as f64 / secs).round();
    if !rate.is_finite() || rate < 0.0 {
        return None;
    }
    // Saturating on the way back to an integer: a rate that overflows a u64 is
    // not a number anyone is going to read.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a rate that overflows a u64 is not a number anyone is going to read"
    )]
    Some(rate.min(u64::MAX as f64) as u64)
}

/// The two transfer-rate numbers the design requires, measured **once, in
/// the copy loop above the `Vfs` trait**, so every backend reports identically
/// and none implements its own accounting.
///
/// > Two different numbers for two different questions. The *displayed rate* is
/// > a moving average over a short window … so it responds when a transfer
/// > stalls. … The *ETA* is computed from the cumulative average instead,
/// > because an estimate that jumps around every time the window twitches is
/// > worse than useless.
///
/// And the third rule, which is why both methods return `Option`: "Neither is
/// fabricated: below `ops.rate_min_samples` the rate shows `-` and the ETA is
/// omitted rather than guessed."
#[derive(Debug, Clone)]
pub struct RateMeter {
    window: Duration,
    min_samples: usize,
    /// `(when, cumulative bytes at that moment)`, oldest first.
    samples: std::collections::VecDeque<(Instant, u64)>,
    total: u64,
    started: Instant,
}

impl RateMeter {
    /// A meter over `window`, refusing to report below `min_samples`.
    pub fn new(window: Duration, min_samples: usize) -> Self {
        let started = Instant::now();
        Self {
            window,
            min_samples,
            samples: std::collections::VecDeque::new(),
            total: 0,
            started,
        }
    }

    /// The meter `[ops]` describes.
    pub fn from_config(cfg: &OpsConfig) -> Self {
        Self::new(cfg.rate_window.0, cfg.rate_min_samples)
    }

    /// Record bytes the **writer accepted** (rule 1: never bytes
    /// the reader produced, which on a buffering remote destination measures
    /// how fast a buffer is filling rather than progress).
    pub fn record(&mut self, bytes: u64) {
        self.total = self.total.saturating_add(bytes);
        let now = Instant::now();
        self.samples.push_back((now, self.total));
        // Keep one sample from before the window so the difference across it is
        // measurable; everything older goes.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|(t, _)| now.duration_since(*t) > self.window)
        {
            self.samples.pop_front();
        }
    }

    /// Total bytes seen.
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// How long the meter has been running.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The **displayed** rate: bytes per second over the window, measured
    /// against **now**.
    ///
    /// `None` below `min_samples`, which the dialog renders as `-`. A stall
    /// drives this towards zero, which is the entire point of using a window -
    /// and it only can if the window's upper end is the present moment rather
    /// than the last sample. Measuring between the first and last *samples*
    /// froze the last healthy figure on screen for as long as nothing moved,
    /// which rule 2 calls a lie; a batch of one large file
    /// followed by ten thousand empty ones reaches it without any hang at all,
    /// because a zero-byte file never records a sample.
    pub fn rate(&self) -> Option<u64> {
        if self.samples.len() < self.min_samples.max(2) {
            return None;
        }
        let now = Instant::now();
        // The baseline is the newest sample that is already outside the window,
        // so the span measured is the window itself rather than whatever
        // happens to be left in the deque. With every sample inside it, the
        // oldest one is the baseline.
        let window_start = now.checked_sub(self.window);
        let mut base = *self.samples.front()?;
        for sample in &self.samples {
            match window_start {
                Some(start) if sample.0 <= start => base = *sample,
                _ => break,
            }
        }
        let elapsed = now.duration_since(base.0);
        if elapsed.is_zero() {
            return None;
        }
        // An idle window is a real measurement of zero, not an absent one: the
        // dialog must be able to show a stall, and `-` reads as "no figure yet".
        Some(rate_of(self.total.saturating_sub(base.1), elapsed).unwrap_or(0))
    }

    /// The **cumulative** average, which the ETA uses because it must not jump.
    pub fn average(&self) -> Option<u64> {
        if self.samples.len() < self.min_samples.max(2) {
            return None;
        }
        rate_of(self.total, self.elapsed())
    }

    /// Time remaining, or `None` when no honest estimate exists.
    ///
    /// Without a byte total, or before enough samples, there is nothing to
    /// estimate and the dialog shows nothing rather than a guess.
    pub fn eta(&self, done: u64, total: u64) -> Option<Duration> {
        if total == 0 || done >= total {
            return None;
        }
        let average = self.average()?;
        if average == 0 {
            return None;
        }
        Some(Duration::from_secs(total.saturating_sub(done) / average))
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
