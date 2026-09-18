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
pub mod context;
pub mod copy;
pub mod delete;
pub mod download;
pub mod editor;
pub mod gate;
pub mod kind;
pub mod localsend;
pub mod mask;
pub mod mkdir;
pub mod move_;
pub mod open;
pub mod pack;
pub mod queue;
pub mod resize;
pub mod spec;
pub mod split;
pub mod status;
pub mod summary;
pub mod walk;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::OpsConfig;
use crate::vfs::Vfs;

// The engine's types, each in the file of its responsibility and reachable
// here as they always were.
pub use context::{FailReason, JobContext, RateMeter, is_fatal};
pub use kind::{JobAction, JobId, JobKind};
pub use mask::{MaskMode, matches as mask_matches};
pub use spec::{
    ConflictChoice, ConflictRequest, Decision, DownloadRequest, JobOptions, JobSpec, PackInto,
};
pub use status::{
    CancelFlag, JobEvent, JobHandle, JobRequest, JobStatus, JobUpdate, running_job_lines,
};
pub use summary::{JobFailure, JobSummary, Outcome};
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
        JobKind::Download => download::run(vfs, spec, ctx),
        JobKind::LocalSend => localsend::run(vfs, spec, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
