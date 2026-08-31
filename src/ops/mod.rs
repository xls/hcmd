//! The job engine.
//!
//! the design opens with the whole design in one sentence: "All of copy, move,
//! delete and pack run on a worker with progress reported to the UI over a
//! channel." This module is that worker and that channel.
//!
//! # Modelled on the directory read
//!
//! v0.1 already had one asynchronous, cancellable, streaming operation - the
//! directory read - and the event loop already drains it. A job works the same
//! way, deliberately, so there is one pattern in the codebase rather than two:
//!
//! | Directory read | Job |
//! |---|---|
//! | [`App::request_read`] queues a [`ReadRequest`] | [`App::request_job`] queues a [`JobRequest`] |
//! | [`App::take_pending_reads`] drains it | [`App::take_pending_jobs`] drains it |
//! | the event loop spawns the read | the event loop calls [`spawn`] |
//! | [`VfsEvent`] comes back over an `mpsc` channel | [`JobUpdate`] comes back over an `mpsc` channel |
//! | [`App::apply_vfs_event`] folds it into state | [`App::apply_job_event`] folds it into state |
//! | a stale `generation` is dropped | a [`JobId`] with no live status is dropped |
//! | dropping the receiver cancels | dropping the receiver cancels, and so does [`CancelFlag`] |
//!
//! [`App::request_read`]: crate::app::App::request_read
//! [`ReadRequest`]: crate::app::ReadRequest
//! [`VfsEvent`]: crate::app::VfsEvent
//! [`App::take_pending_reads`]: crate::app::App::take_pending_reads
//! [`App::apply_vfs_event`]: crate::app::App::apply_vfs_event
//! [`App::request_job`]: crate::app::App::request_job
//! [`App::take_pending_jobs`]: crate::app::App::take_pending_jobs
//! [`App::apply_job_event`]: crate::app::App::apply_job_event
//!
//! # Cancellation
//!
//! the design requires cancellation "between files and within a large file's
//! chunk loop", leaving "no half-written destination". Both halves are
//! enforced here rather than left to each runner:
//!
//! * [`JobContext::cancelled`] is true as soon as either the [`CancelFlag`] is
//!   set (`Esc` on the progress dialog) **or** the UI dropped the receiver.
//!   Every runner checks it between files, and [`copy::copy_stream`] checks it
//!   on every chunk.
//! * A copy writes to a temporary name beside the destination and renames it
//!   into place only on success, so a cancelled or failed copy removes the
//!   partial file and never touches what was already there.
//!
//! # Errors never abort the batch
//!
//! "collect per-file failures and show a summary at the end".
//! [`JobContext::fail`] records a [`JobFailure`] *and* emits
//! [`JobEvent::Failed`] so the UI can show it as it happens; the runner keeps
//! going. The failures ride home on [`JobSummary::failures`].

pub mod checksum;
pub mod clipboard;
pub mod compare;
pub mod conflict;
pub mod copy;
pub mod delete;
pub mod editor;
pub mod gate;
pub mod mask;
pub mod mkdir;
pub mod move_;
pub mod open;
pub mod pack;
pub mod queue;
pub mod resize;
pub mod split;
pub mod walk;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;

use crate::config::OpsConfig;
use crate::error::Error;
use crate::vfs::{Vfs, VfsPath};

pub use mask::{MaskMode, matches as mask_matches};
pub use walk::{
    SelectionStats, SizeCache, TreeStats, WalkOptions, WalkOutcome, selection_stats, walk_stats,
};

/// How often a running job emits [`JobEvent::Progress`].
///
/// A copy of ten thousand small files would otherwise send one message per
/// file and spend more time in the channel than in `write`. The current file's
/// name always forces an emit, so the dialog never shows a stale name.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// The channel depth for [`JobUpdate`]s.
pub const JOB_CHANNEL_DEPTH: usize = 128;

/// Identifies one job for its whole life, including in the background queue.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What a job does.
///
/// `Pack` and `Unpack` join this enum in v0.5; adding a variant
/// is a source-compatible change for everything in this crate that matches
/// exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// `F5`: copy the sources into `dest`.
    Copy,
    /// `F6`: rename or move the sources into `dest`.
    Move,
    /// `F8` / `Shift+F8`: delete the sources.
    Delete {
        /// True for `F8`, which moves to the XDG trash; false for `Shift+F8`,
        /// which unlinks.
        trash: bool,
    },
    /// `F7`: create `dest`.
    Mkdir,
    /// `Ctrl+L` and `Space`: walk the sources and report their size.
    /// Reads nothing but metadata and writes
    /// nothing at all.
    Size,
    /// `Ctrl+M`: rename each source to the target beside it.
    /// Moves no bytes, so its progress is files only.
    Rename,
    /// `Shift+F2` with `ops.compare_contents` on: read both sides of each
    /// facing pair and say which differ. Writes nothing.
    Compare,
    /// Comparing two named files byte for byte, for a verdict rather than for
    /// a mark. Shares [`crate::ops::compare`]'s reader with [`Self::Compare`];
    /// it is a separate kind because its answer is a sentence about one pair,
    /// not a set of names to mark.
    CompareFiles,
    /// Cutting one file into numbered parts.
    Split,
    /// Putting a numbered set back together.
    Merge,
    /// Hashing files and writing a sidecar, or reading one and checking what
    /// it names. [`crate::ops::checksum`] does both; the flag says which.
    Checksum {
        /// True to check an existing sidecar, false to write a new one.
        verify: bool,
    },
    /// `Shift+R`: decode each source, resample it and write it into `dest`
    /// under the new format and the new name.
    Resize,
}

impl JobKind {
    /// A stable string id, for state files and messages.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Delete { trash: true } => "trash",
            Self::Delete { trash: false } => "delete",
            Self::Mkdir => "mkdir",
            Self::Size => "size",
            Self::Rename => "rename",
            Self::Compare => "compare",
            Self::CompareFiles => "compare_files",
            Self::Split => "split",
            Self::Merge => "merge",
            Self::Checksum { verify: false } => "checksum",
            Self::Checksum { verify: true } => "verify",
            Self::Resize => "resize",
        }
    }

    /// A verb for the progress dialog's title.
    pub const fn title(&self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Delete { trash: true } => "Moving to trash",
            Self::Delete { trash: false } => "Deleting",
            Self::Mkdir => "Creating directory",
            Self::Size => "Calculating size",
            Self::Rename => "Renaming",
            Self::Compare => "Comparing",
            Self::CompareFiles => "Comparing files",
            Self::Split => "Splitting",
            Self::Merge => "Merging",
            Self::Checksum { verify: false } => "Checksumming",
            Self::Checksum { verify: true } => "Verifying",
            Self::Resize => "Resizing",
        }
    }

    /// True for a job that changes the filesystem. A [`JobKind::Size`] does
    /// not, which is what lets it run without a confirmation of any kind, and
    /// neither does a [`JobKind::Compare`]: the contents comparison
    /// reads both sides and writes nothing at all.
    pub const fn is_destructive(&self) -> bool {
        !matches!(
            self,
            Self::Size | Self::Compare | Self::CompareFiles | Self::Checksum { verify: true }
        )
    }

    /// True for a job whose sources are paired positionally with
    /// [`JobSpec::targets`] rather than copied into one [`JobSpec::dest`].
    ///
    /// Two kinds: [`JobKind::Rename`]'s source and its new name,
    /// and [`JobKind::Compare`]'s two facing files.
    /// The invariant that every other kind leaves `targets`
    /// empty is checked in this module's tests.
    pub const fn is_paired(&self) -> bool {
        matches!(self, Self::Rename | Self::Compare | Self::CompareFiles)
    }

    /// True for a job whose failures can be re-run as a fresh job over the
    /// sources that failed (the "option to retry the failures").
    ///
    /// A paired job cannot: [`JobSpec::targets`] is positional, so a retry
    /// over a *subset* of the sources would have to carry exactly the targets
    /// of the sources it kept, and dropping them instead queues a job with no
    /// targets that renames nothing and reports a clean run. the design
    /// gives a multi-rename `Undo` and a result list rather than a retry, so
    /// there is nothing missing here to add later - the retry is the wrong
    /// verb for it.
    pub const fn is_retryable(&self) -> bool {
        !self.is_paired()
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// What to do about a destination that already exists.
///
/// Every one of these has an "all" variant, which is [`Decision::apply_to_all`]
/// rather than a separate set of enum arms - the choice and its scope are
/// orthogonal, and folding them together doubles the enum for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictChoice {
    /// Replace the destination.
    Overwrite,
    /// Leave the destination alone and move on.
    Skip,
    /// Write beside it under a free name.
    Rename,
    /// Append the source to the end of the destination.
    Append,
    /// Overwrite only when the source is newer than the destination.
    OverwriteIfNewer,
    /// Overwrite only when the two differ in size.
    OverwriteIfDifferentSize,
}

impl ConflictChoice {
    /// A stable string id.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Skip => "skip",
            Self::Rename => "rename",
            Self::Append => "append",
            Self::OverwriteIfNewer => "overwrite_if_newer",
            Self::OverwriteIfDifferentSize => "overwrite_if_different_size",
        }
    }

    /// The label a conflict dialog's button carries.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Overwrite => "Overwrite",
            Self::Skip => "Skip",
            Self::Rename => "Rename",
            Self::Append => "Append",
            Self::OverwriteIfNewer => "If newer",
            Self::OverwriteIfDifferentSize => "If different size",
        }
    }

    /// Every choice, in the order a dialog should offer them.
    pub const ALL: &'static [Self] = &[
        Self::Overwrite,
        Self::Skip,
        Self::Rename,
        Self::Append,
        Self::OverwriteIfNewer,
        Self::OverwriteIfDifferentSize,
    ];
}

/// Everything the UI needs to describe one conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRequest {
    /// The file being copied or moved.
    pub source: VfsPath,
    /// The existing file in the way.
    pub dest: VfsPath,
    /// Source size in bytes.
    pub source_size: u64,
    /// Destination size in bytes.
    pub dest_size: u64,
    /// Source mtime, `None` when the backend does not report one.
    pub source_mtime: Option<SystemTime>,
    /// Destination mtime.
    pub dest_mtime: Option<SystemTime>,
    /// True when both sides are directories, where only
    /// [`ConflictChoice::Skip`] and a merge (`Overwrite`) make sense.
    pub both_dirs: bool,
    /// True when the **destination** is a directory, whatever the source is.
    ///
    /// Separate from `both_dirs` because the asymmetric case is the dangerous
    /// one: a file arriving where a directory already stands must not be
    /// described to the user as a file-versus-file collision, and must not be
    /// answered by recursively removing that directory
    /// ([`conflict::Plan::Refuse`]).
    pub dest_is_dir: bool,
}

/// The UI's answer to a [`JobEvent::NeedsDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Resolve this conflict.
    Conflict {
        /// What to do.
        choice: ConflictChoice,
        /// A name for [`ConflictChoice::Rename`]. `None` asks the worker to
        /// generate a free one - which is also what an "all" rename means,
        /// since one typed name cannot serve a whole batch.
        rename_to: Option<String>,
        /// "Decisions apply for the remainder of the batch."
        /// True installs `choice` as the standing policy.
        apply_to_all: bool,
    },
    /// Abandon the job. Equivalent to setting the [`CancelFlag`], and provided
    /// so a dialog that is already answering has one channel to answer on.
    Cancel,
}

/// What `Alt+F5`'s dialog decided, for a job that is a **pack**.
///
///
/// It rides on [`JobOptions`] rather than on [`JobKind`] because a pack is a
/// copy - or, with "move to archive", a move - as far as everything the user
/// sees is concerned: the same dialog, the same progress, the same summary.
/// What it changes is where the bytes go, which is [`crate::ops::pack`]'s
/// business and nobody else's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackInto {
    /// Which of the formats to write.
    pub format: crate::vfs::archive::format::FormatId,
    /// `0` stores, `9` is maximum; every format maps it onto its own scale.
    pub level: u8,
}

/// Everything the copy/move dialog collects and the worker obeys.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOptions {
    /// "Symlinks are copied as links by default;
    /// `ops.follow_symlinks` changes it."
    pub follow_symlinks: bool,
    /// Mode, mtime, and where permitted uid/gid and xattrs. See
    /// [`copy::preserve`] for what "where permitted" actually amounts to
    /// without a `libc` dependency.
    pub preserve_attrs: bool,
    /// Re-read and compare after writing (the Verify checkbox).
    pub verify: bool,
    /// "Only files of this type": a wildcard mask filtering what actually gets
    /// copied out of the selection. Empty or `*` means everything.
    pub file_mask: String,
    /// A conflict policy already chosen, so the batch runs without asking.
    /// `None` asks the UI on the first conflict.
    pub conflict: Option<ConflictChoice>,
    /// Set only by `Alt+F5`: this job writes a **new archive** rather than
    /// copying into an existing destination.
    pub pack: Option<PackInto>,
    /// How many bytes go in each part of a [`JobKind::Split`]. Zero everywhere
    /// else, and refused there.
    pub part_size: u64,
    /// Set only by `Shift+R`: what the resize dialog collected.
    ///
    /// It rides here rather than on [`JobSpec`] for the reason [`PackInto`]
    /// does: every kind of job that has ever needed its own settings has taken
    /// them from the dialog through these options, and one shape is easier to
    /// keep true than two.
    pub resize: Option<resize::ResizeSettings>,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            preserve_attrs: true,
            verify: false,
            file_mask: String::new(),
            conflict: None,
            pack: None,
            part_size: 0,
            resize: None,
        }
    }
}

impl JobOptions {
    /// The defaults from `[ops]` in `config.toml`.
    pub fn from_config(cfg: &OpsConfig) -> Self {
        Self {
            follow_symlinks: cfg.follow_symlinks,
            preserve_attrs: cfg.preserve_attrs,
            verify: false,
            file_mask: String::new(),
            conflict: if cfg.confirm_overwrite {
                None
            } else {
                Some(ConflictChoice::Overwrite)
            },
            pack: None,
            part_size: 0,
            resize: None,
        }
    }

    /// The [`WalkOptions`] these options imply, so a pre-flight walk and the
    /// copy that follows it agree about symlinks.
    pub const fn walk(&self) -> WalkOptions {
        WalkOptions {
            follow_symlinks: self.follow_symlinks,
        }
    }
}

/// One job, fully described. The dialog produces it; the worker consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// What to do.
    pub kind: JobKind,
    /// What to do it to. Never empty except for [`JobKind::Mkdir`].
    pub sources: Vec<VfsPath>,
    /// Where to do it. The target *directory* for `Copy`/`Move`, the directory
    /// to create for `Mkdir`, and `None` for `Delete` and `Size`.
    pub dest: Option<VfsPath>,
    /// The dialog's answers.
    pub options: JobOptions,
    /// The new addresses, one per source, for [`JobKind::Rename`].
    /// **Empty for every other kind**, and an invariant
    /// checks it.
    pub targets: Vec<VfsPath>,
}

impl JobSpec {
    /// A job with default options.
    pub fn new(kind: JobKind, sources: Vec<VfsPath>, dest: Option<VfsPath>) -> Self {
        Self {
            kind,
            sources,
            dest,
            options: JobOptions::default(),
            targets: Vec::new(),
        }
    }

    /// A [`JobKind::Rename`] over pairs.
    ///
    /// The pairs are split into two positionally-matched vectors rather than
    /// carried as a `Vec<(VfsPath, VfsPath)>`, because `sources` is what every
    /// other part of the job machinery - the progress dialog, the failure
    /// summary, the retry - already reads.
    pub fn rename(pairs: Vec<(VfsPath, VfsPath)>) -> Self {
        let (sources, targets): (Vec<VfsPath>, Vec<VfsPath>) = pairs.into_iter().unzip();
        Self {
            kind: JobKind::Rename,
            sources,
            dest: None,
            options: JobOptions::default(),
            targets,
        }
    }

    /// A [`JobKind::Size`] walk over some paths.
    pub fn size(sources: Vec<VfsPath>) -> Self {
        Self::new(JobKind::Size, sources, None)
    }

    /// A [`JobKind::Compare`] over facing pairs.
    ///
    /// Split into two positionally-matched vectors exactly as
    /// [`JobSpec::rename`] is, and for the same reason: `sources` is what the
    /// progress dialog and the failure summary already read.
    pub fn compare(pairs: Vec<(VfsPath, VfsPath)>) -> Self {
        let (sources, targets): (Vec<VfsPath>, Vec<VfsPath>) = pairs.into_iter().unzip();
        Self {
            kind: JobKind::Compare,
            sources,
            dest: None,
            options: JobOptions::default(),
            targets,
        }
    }

    /// A [`JobKind::CompareFiles`] over exactly one facing pair.
    ///
    /// The same two positionally-matched vectors [`JobSpec::compare`] builds,
    /// with one entry each, so the runner they share needs no special case for
    /// the single pair.
    pub fn compare_files(a: VfsPath, b: VfsPath) -> Self {
        Self {
            kind: JobKind::CompareFiles,
            sources: vec![a],
            dest: None,
            options: JobOptions::default(),
            targets: vec![b],
        }
    }

    /// Set the options, by value, so a spec can be built in one expression.
    #[must_use]
    pub fn with_options(mut self, options: JobOptions) -> Self {
        self.options = options;
        self
    }
}

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

impl JobSummary {
    /// True when nothing failed and nothing was cancelled.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
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
    /// True once [`JobEvent::Started`] has arrived.
    pub started: bool,
    /// `F2` sent it to the background queue.
    ///
    /// Nothing about the worker changes: foreground and background are two
    /// views of one job, and this is which view it currently has. Setting it
    /// back to false is what "bringing it forward" means.
    pub background: bool,
    /// The conflict the worker is parked on, if any. The UI answers with
    /// [`crate::app::App::answer_job`].
    pub pending_decision: Option<Box<ConflictRequest>>,
    /// Failures seen so far.
    pub failures: Vec<JobFailure>,
    /// `Some` once the worker is done. A status with this set is finished and
    /// may be dropped from the queue view whenever the UI likes.
    pub finished: Option<Box<JobSummary>>,
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
            started: false,
            background: false,
            pending_decision: None,
            failures: Vec::new(),
            finished: None,
        }
    }

    /// Fold one event in.
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
                self.started = true;
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
                self.started = true;
            }
            JobEvent::NeedsDecision { request } => {
                self.pending_decision = Some(request.clone());
            }
            JobEvent::Failed { path, error } => self.failures.push(JobFailure {
                path: path.clone(),
                error: error.clone(),
            }),
            JobEvent::Finished { summary } => {
                self.pending_decision = None;
                self.files_done = summary.files_done;
                self.bytes_done = summary.bytes_done;
                self.elapsed = summary.elapsed;
                self.finished = Some(summary.clone());
            }
        }
    }

    /// True while the job is neither finished nor waiting for an answer.
    pub fn is_running(&self) -> bool {
        self.finished.is_none() && self.pending_decision.is_none()
    }

    /// a job blocked on a conflict "does not sit silently
    /// blocked". This is what the queue view and the key-bar indicator ask.
    pub fn needs_attention(&self) -> bool {
        self.pending_decision.is_some()
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
        .filter(|j| j.finished.is_none() && j.kind.is_destructive())
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
    } else if job.started {
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
    fn new(
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

/// How much a copy reads and writes at a time, for a given backend
/// (rule 3: "Chunk size and read-ahead come from `Capabilities`,
/// not from a constant").
///
/// 256 KiB is right for a local copy and wrong for a network backend, where
/// the design asks for pipelined reads rather than a round trip per block -
/// "the difference between 2 MB/s and saturating the link". The pipelining
/// itself arrives with the SFTP backend in v0.65; the chunk size is the part
/// that belongs here, in the one copy loop, so that backend adds none of it.
pub const fn chunk_size(caps: &crate::vfs::Capabilities) -> usize {
    match caps.latency {
        crate::vfs::LatencyClass::Local => copy::COPY_CHUNK,
        crate::vfs::LatencyClass::Network => copy::COPY_CHUNK * 4,
    }
}

/// Start a job on the blocking pool and hand back the UI's end of it.
///
/// The exact analogue of `main::spawn_read`: the caller owns `tx`, the worker
/// owns everything else, and dropping the receiver cancels.
///
/// Must be called from inside a tokio runtime.
pub fn spawn(
    vfs: Arc<dyn Vfs>,
    id: JobId,
    spec: JobSpec,
    tx: mpsc::Sender<JobUpdate>,
    ops: &OpsConfig,
) -> JobHandle {
    let cancel = CancelFlag::new();
    // Depth 1 is enough: the worker parks on `ask` and consumes each answer
    // before it can ask again. The extra slot is for the `Cancel` that
    // `JobHandle::cancel` pushes to release a parked worker.
    let (decision_tx, decision_rx) = mpsc::channel(2);
    let handle = JobHandle {
        id,
        kind: spec.kind,
        cancel: cancel.clone(),
        decisions: decision_tx,
    };

    // File I/O is blocking, so a job belongs on the blocking pool exactly as
    // `LocalFs::read_dir` does. A runtime worker is never held.
    let rate = RateMeter::from_config(ops);
    tokio::task::spawn_blocking(move || {
        let mut ctx = JobContext::new(id, spec.kind, tx, decision_rx, cancel, rate);
        run(vfs.as_ref(), &spec, &mut ctx);
        let _ = ctx.finish();
    });

    handle
}

/// Run a job to completion on the current thread.
///
/// Split out from [`spawn`] so a test can drive a runner without a runtime,
/// and so the dispatch on [`JobKind`] lives in one place.
pub fn run(vfs: &dyn Vfs, spec: &JobSpec, ctx: &mut JobContext) {
    match spec.kind {
        JobKind::Size => walk::run(vfs, spec, ctx),
        JobKind::Mkdir => mkdir::run(vfs, spec, ctx),
        JobKind::Copy | JobKind::Move => copy::run(vfs, spec, ctx),
        JobKind::Delete { trash } => delete::run(vfs, spec, ctx, trash),
        JobKind::Rename => crate::rename::exec::run(vfs, spec, ctx),
        JobKind::Compare | JobKind::CompareFiles => compare::run(vfs, spec, ctx),
        JobKind::Checksum { verify } => checksum::run(vfs, spec, ctx, verify),
        JobKind::Split => split::run_split(vfs, spec, ctx),
        JobKind::Merge => split::run_merge(vfs, spec, ctx),
        JobKind::Resize => resize::run(vfs, spec, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_byte_batch_divides_by_nothing_anywhere() {
        // The obvious division by zero: a batch whose byte total is 0 - one
        // empty file, or a `mkdir`. Every figure the progress dialog asks for
        // has to come back as "no figure" rather than as a NaN, an infinity or
        // a panic.
        assert_eq!(fraction(0, 0), None);
        assert_eq!(fraction(5, 0), None, "done without a total is still no bar");
        assert_eq!(rate_of(0, Duration::from_secs(1)), None);
        assert_eq!(rate_of(100, Duration::ZERO), None);

        let meter = RateMeter::new(Duration::from_secs(3), 4);
        assert_eq!(meter.eta(0, 0), None);

        let mut status = JobStatus::queued(JobId(1), JobKind::Copy);
        status.started = true;
        status.files_total = 1;
        status.files_done = 1;
        assert_eq!(status.fraction(), None, "no batch bar without a total");
        assert_eq!(status.file_fraction(), None);
        assert!(!status.show_file_bar(0), "and no file bar either");

        // A zero-byte transfer is still a transfer, so the quit prompt names it
        // rather than dividing its way into saying nothing.
        assert_eq!(
            running_job_lines(&[status]),
            vec!["Copying: 1 of 1 files".to_string()]
        );
    }

    #[test]
    fn the_quit_prompt_names_every_transfer_and_no_size_walk() {
        // the design names what is still running; a `Size` walk is not a
        // transfer and quitting during one loses nothing.
        let mut copying = JobStatus::queued(JobId(1), JobKind::Copy);
        copying.bytes_done = 2048;
        copying.bytes_total = 8192;
        let mut walking = JobStatus::queued(JobId(2), JobKind::Size);
        walking.files_total = 900;
        let lines = running_job_lines(&[copying, walking]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Copying:"), "{lines:?}");
        assert!(
            lines[0].contains("2.0 K"),
            "the extent, in bytes: {lines:?}"
        );
    }

    #[test]
    fn a_job_with_no_byte_total_is_still_described() {
        let mut deleting = JobStatus::queued(JobId(1), JobKind::Delete { trash: true });
        deleting.files_total = 12;
        deleting.files_done = 3;
        assert_eq!(
            running_job_lines(&[deleting]),
            vec!["Moving to trash: 3 of 12 files".to_string()]
        );

        // And one that has not reported anything yet says so rather than
        // claiming `0 of 0`.
        let queued = JobStatus::queued(JobId(2), JobKind::Move);
        assert_eq!(
            running_job_lines(&[queued]),
            vec!["Moving: queued".to_string()]
        );
    }

    #[test]
    fn the_list_is_capped_so_the_prompt_fits_a_sixty_column_terminal() {
        // nine named jobs would not fit at 60x15, so the tail
        // becomes a count rather than being dropped silently.
        let jobs: Vec<JobStatus> = (0..9)
            .map(|n| JobStatus::queued(JobId(n), JobKind::Copy))
            .collect();
        let lines = running_job_lines(&jobs);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[3], "and 6 more.");
    }

    #[test]
    fn nothing_running_is_no_prompt_at_all() {
        assert!(running_job_lines(&[]).is_empty());
        let mut done = JobStatus::queued(JobId(1), JobKind::Copy);
        done.finished = Some(Box::new(JobSummary {
            kind: JobKind::Copy,
            files_done: 1,
            dirs_done: 0,
            bytes_done: 1,
            skipped: 0,
            failures: Vec::new(),
            cancelled: false,
            elapsed: Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        }));
        assert!(running_job_lines(&[done]).is_empty());
    }

    #[test]
    fn a_rate_is_never_fabricated() {
        // rule 3: below `rate_min_samples` the rate shows `-`
        // and the ETA is omitted rather than guessed.
        let mut meter = RateMeter::new(Duration::from_secs(3), 4);
        assert_eq!(meter.rate(), None, "nothing measured yet");
        assert_eq!(meter.eta(0, 1_000_000), None);
        for _ in 0..3 {
            meter.record(1024);
        }
        assert_eq!(meter.rate(), None, "still under the sample floor");
        meter.record(1024);
        assert_eq!(meter.total(), 4096);
        // Four samples in microseconds is a very large rate, but it is a real
        // measurement rather than a guess, so it is reported.
        assert!(meter.rate().is_some());
        assert_eq!(meter.eta(1_000, 1_000), None, "already done");
        assert_eq!(meter.eta(0, 0), None, "no total to divide by");
    }

    /// rule 2: "a stalled transfer showing a healthy rate is a lie": "The
    /// displayed rate is a short moving average (it must be able to show a
    /// stall)."
    ///
    /// The window's upper end is **now**, not the last sample - otherwise the
    /// numerator, the denominator and the window are all frozen the moment the
    /// bytes stop, and the last healthy figure stays on screen. It needs no
    /// hang to reach: a batch of one large file followed by ten thousand empty
    /// ones emits progress continuously while never recording a byte.
    #[test]
    fn a_stalled_transfer_shows_a_stall_rather_than_its_last_healthy_rate() {
        let window = Duration::from_millis(60);
        let mut meter = RateMeter::new(window, 4);
        for _ in 0..8 {
            meter.record(1024 * 1024);
        }
        let moving = meter.rate().expect("a rate while bytes are moving");
        assert!(moving > 0);

        // Nothing more is recorded, and more than a window goes by.
        std::thread::sleep(window * 4);
        let stalled = meter.rate().expect("still a measurement, and it is zero");
        assert_eq!(
            stalled, 0,
            "an idle window is a rate of zero, not the {moving} B/s it was"
        );
        // The ETA is the cumulative average and deliberately does not lurch.
        assert!(meter.average().is_some());
    }

    #[test]
    fn rate_never_divides_by_zero() {
        assert_eq!(rate_of(100, Duration::ZERO), None);
        assert_eq!(rate_of(0, Duration::from_secs(1)), None);
        assert_eq!(rate_of(2048, Duration::from_secs(2)), Some(1024));
    }

    #[test]
    fn the_chunk_size_comes_from_capabilities_not_a_constant() {
        use crate::vfs::{Capabilities, LatencyClass};
        assert_eq!(chunk_size(&Capabilities::LOCAL), copy::COPY_CHUNK);
        let remote = Capabilities {
            latency: LatencyClass::Network,
            ..Capabilities::LOCAL
        };
        assert!(
            chunk_size(&remote) > chunk_size(&Capabilities::LOCAL),
            "256 KiB is right locally and wrong for SFTP"
        );
    }

    #[test]
    fn a_dropped_receiver_cancels_the_job() {
        let (mut ctx, rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
        assert!(!ctx.cancelled());
        drop(rx);
        // The next send notices.
        ctx.set_file("/tmp/a", 0);
        assert!(ctx.cancelled(), "a dropped receiver stops the worker");
    }

    #[test]
    fn the_cancel_flag_stops_the_chunk_loop() {
        let (mut ctx, _rx, _dtx, flag) = JobContext::for_test(JobKind::Copy);
        assert!(ctx.add_bytes(1024));
        flag.cancel();
        assert!(!ctx.add_bytes(1024), "add_bytes reports the stop");
        assert!(ctx.cancelled());
    }

    #[test]
    fn failures_are_collected_and_do_not_stop_the_batch() {
        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Delete { trash: false });
        ctx.fail(&VfsPath::local("/a"), "no");
        ctx.fail(&VfsPath::local("/b"), "nope");
        assert!(!ctx.cancelled());
        let summary = ctx.finish();
        assert_eq!(summary.failures.len(), 2);
        assert!(!summary.is_clean());
        assert!(summary.message().contains("2 failed"));
    }

    /// The trap this seam exists to disarm.
    ///
    /// A caller that has already flattened its [`Error`] to a sentence used to
    /// get `fatal: false` by construction, whatever the sentence said, so a
    /// dead connection reached the summary as an ordinary failure and the
    /// batch carried on against it. Recognising the wording is the net; the
    /// variant is still the right answer where one is in hand.
    #[test]
    fn a_lost_connection_is_fatal_even_after_it_has_been_flattened_to_text() {
        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
        ctx.fail(
            &VfsPath::local("/a"),
            Error::connection_lost("sftp://user@host:22").to_string(),
        );
        assert!(ctx.fatal(), "the sentence still names a dead connection");

        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
        ctx.fail(
            &VfsPath::local("/a"),
            Error::connection_closed("sftp://user@host:22").to_string(),
        );
        assert!(ctx.fatal(), "and so does a closed one");
    }

    /// The regression itself: a backend that spelled the sentence out instead
    /// of returning the variant.
    ///
    /// `vfs::archive::session` did exactly this - an `Error::msg` whose text
    /// was character for character [`Error::ConnectionClosed`]'s `Display` -
    /// so a batch extracting out of an archive on a connection that had gone
    /// away produced one row per remaining member. The site now returns the
    /// variant; this is the net that catches the next one.
    #[test]
    fn a_hand_rolled_connection_sentence_is_still_fatal() {
        let hand_rolled = Error::msg(format!(
            "/srv/x.tar#/a: {}",
            crate::error::CONNECTION_CLOSED_TEXT
        ));
        assert!(is_fatal(&hand_rolled), "{hand_rolled}");
        let ordinary = Error::msg("/srv/x.tar#/a: that member is not in the index");
        assert!(!is_fatal(&ordinary), "{ordinary}");
    }

    /// The other half: a refusal this program decided by itself is not fatal,
    /// and says so rather than defaulting to it.
    #[test]
    fn a_refusal_the_program_decided_itself_does_not_stop_the_batch() {
        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
        ctx.fail(
            &VfsPath::local("/a"),
            FailReason::refused("a directory cannot be copied into itself"),
        );
        assert!(!ctx.fatal());
        assert_eq!(ctx.finish().failures.len(), 1);
    }

    /// A runner's loop, in miniature: twenty files, the connection gone at the
    /// second, one failure row rather than twenty identical ones.
    #[test]
    fn a_batch_stops_at_the_first_lost_connection() {
        let (mut ctx, _rx, _dtx, _flag) = JobContext::for_test(JobKind::Copy);
        let mut attempted = 0;
        for n in 0..20 {
            attempted += 1;
            let path = VfsPath::local(format!("/src/file{n}"));
            if n == 0 {
                ctx.fail(&path, FailReason::refused("something ordinary"));
            } else {
                ctx.fail(&path, Error::connection_lost("sftp://user@host:22"));
            }
            if ctx.fatal() {
                break;
            }
        }
        assert_eq!(attempted, 2, "the loop stopped at the drop");
        assert_eq!(ctx.finish().failures.len(), 2);
    }

    /// The subject of a failure can be compared against the path that was
    /// asked for, which is what tells "this file is missing" from "something
    /// else went missing while we were working".
    #[test]
    fn a_failure_names_a_subject_that_can_be_compared() {
        let missing = Error::not_found("/srv/gone");
        assert!(missing.is_about(&"/srv/gone"));
        assert!(!missing.is_about(&"/srv/here"));

        let bad = Error::invalid_path("/srv/x", "a remote path has to be valid UTF-8");
        assert_eq!(bad.subject(), crate::error::Subject::Path("/srv/x"));

        let gone = Error::connection_lost("sftp://user@host:22");
        assert_eq!(
            gone.subject(),
            crate::error::Subject::Connection("sftp://user@host:22"),
            "the port is not mistaken for the start of an explanation"
        );
        assert_eq!(
            Error::msg("no idea").subject(),
            crate::error::Subject::Nothing
        );
    }

    #[test]
    fn job_status_folds_the_event_stream() {
        let mut status = JobStatus::queued(JobId(7), JobKind::Copy);
        assert!(!status.started);
        status.apply(&JobEvent::Started {
            kind: JobKind::Copy,
            files_total: 4,
            bytes_total: 400,
        });
        assert!(status.started);
        status.apply(&JobEvent::Progress {
            file: "src/a.txt".to_string(),
            file_bytes_done: 10,
            file_bytes_total: 40,
            files_done: 1,
            files_total: 4,
            bytes_done: 100,
            bytes_total: 400,
            throughput: Some(50),
            eta: Some(Duration::from_secs(6)),
            elapsed: Duration::from_secs(2),
        });
        assert_eq!(status.file, "src/a.txt", "with enough parent to be unique");
        assert_eq!(status.fraction(), Some(0.25), "the batch bar");
        assert_eq!(status.file_fraction(), Some(0.25), "the file bar");
        assert!(status.show_batch_bar(), "four files is a batch");
        assert!(!status.show_file_bar(1024 * 1024), "40 bytes only flashes");
        assert!(status.is_running());
        status.apply(&JobEvent::Finished {
            summary: Box::new(JobSummary {
                kind: JobKind::Copy,
                files_done: 4,
                dirs_done: 0,
                bytes_done: 400,
                skipped: 0,
                failures: Vec::new(),
                cancelled: false,
                elapsed: Duration::from_secs(8),
                sized: Vec::new(),
                differing: Vec::new(),
                first_difference: None,
            }),
        });
        assert!(!status.is_running());
        assert_eq!(status.fraction(), Some(1.0));
    }

    #[test]
    fn a_size_walk_and_a_compare_are_the_only_kinds_that_change_nothing() {
        assert!(!JobKind::Size.is_destructive());
        assert!(
            !JobKind::Compare.is_destructive(),
            "the contents comparison reads both sides and writes nothing"
        );
        for kind in [
            JobKind::Copy,
            JobKind::Move,
            JobKind::Mkdir,
            JobKind::Delete { trash: true },
            JobKind::Delete { trash: false },
        ] {
            assert!(kind.is_destructive(), "{kind} should be destructive");
        }
    }

    #[test]
    fn options_follow_the_ops_config() {
        let mut cfg = OpsConfig {
            confirm_overwrite: false,
            ..OpsConfig::default()
        };
        let opts = JobOptions::from_config(&cfg);
        assert_eq!(opts.conflict, Some(ConflictChoice::Overwrite));
        cfg.confirm_overwrite = true;
        assert_eq!(JobOptions::from_config(&cfg).conflict, None);
    }
}
