//! `synchronize`: build the plan off the loop, then open the dialog.
//!
//! The keystroke cannot build the plan, because building it walks both trees -
//! two directory reads per level, possibly over a network - and `dispatch`
//! performs no I/O. So the keystroke queues a [`SyncRequest`], the event loop
//! runs [`crate::ops::sync::plan`] on the blocking pool, and the answer comes
//! back as a [`SyncOutcome`] the loop turns into a dialog. The same shape the
//! file-information dialog and the resize dialog already use.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::App;
use crate::ops::sync::{PlanOptions, SyncMode, SyncPlan, plan};
use crate::vfs::VfsPath;

/// How deep the answer channel is. One request is outstanding at a time, so it
/// is never full; bounded because every channel in the loop is.
pub const SYNC_CHANNEL_DEPTH: usize = 2;

/// A synchronise the keystroke asked for, waiting for the walk.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    /// The left tree's root.
    pub left: VfsPath,
    /// The right tree's root.
    pub right: VfsPath,
    /// How to compare, from the same `[ops]` config `compare_dirs` reads.
    pub options: PlanOptions,
}

/// The walk's result, on its way back to the loop.
#[derive(Debug)]
pub struct SyncOutcome {
    /// The left root, so the dialog can name what it is showing.
    pub left: VfsPath,
    /// The right root.
    pub right: VfsPath,
    /// The plan, or why the walk could not finish.
    pub result: crate::Result<SyncPlan>,
}

impl App {
    /// `synchronize`: queue a walk of the two panels' trees.
    ///
    /// The default mode is [`SyncMode::Both`], the one that never deletes: a
    /// synchronise opens on the safe direction and the user chooses to mirror,
    /// never the other way round.
    pub fn request_synchronize(&mut self) {
        let left = self.left.active_tab().path.clone();
        let right = self.right.active_tab().path.clone();
        let options = PlanOptions {
            mode: SyncMode::Both,
            slack: self.config.ops.compare_mtime_slack.duration(),
            contents: self.config.ops.compare_contents,
        };
        self.pending_sync = Some(SyncRequest {
            left,
            right,
            options,
        });
        self.message = Some("Synchronize: comparing the two trees...".to_string());
    }

    /// Perform the queued walk, off the event loop.
    pub fn service_synchronize(&mut self, tx: &mpsc::Sender<SyncOutcome>) {
        let Some(request) = self.pending_sync.take() else {
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = plan(vfs.as_ref(), &request.left, &request.right, request.options);
            let _ = tx.blocking_send(SyncOutcome {
                left: request.left,
                right: request.right,
                result,
            });
        });
    }
}
