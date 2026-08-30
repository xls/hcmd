//! The multi-rename, and the one undo it leaves behind.
//!
//! At most one multi-rename is in flight, and the undo held for the session
//! describes exactly the plan that ran: the pairs folded back in are the ones
//! the job was given, not a re-derivation from the plan, which is what stops
//! the result list and what actually happened from drifting apart.
//!
//! The undo is armed before the job starts and pruned as it finishes, so a
//! batch interrupted half way through stays undoable for the half that
//! happened. That is the "atomic per file, never per batch", said
//! from the undo's side.
//!
//! An undo is not itself undoable. There is one batch for the session, and
//! re-arming from the reversal would make `Undo` toggle back and forth with no
//! way to tell which way round it is.
//!
//! Nothing here runs a rename: [`crate::input::dispatch`] queues, and the
//! event loop turns the queue into a [`crate::ops::JobKind::Rename`].

use crate::app::{App, RenameRequest};
use crate::ops::{JobId, JobSummary};

impl App {
    /// Queue what `Start!` or `Undo` asked for.
    ///
    /// Queued, never performed: turning a list of pairs into a job touches the
    /// queue and the filesystem, and `dispatch` may do neither.
    ///
    pub fn request_rename(&mut self, request: RenameRequest) {
        self.rename.pending = Some(Box::new(request));
    }

    /// Drain the queued rename. The event loop calls this once a frame.
    pub fn take_pending_rename(&mut self) -> Option<Box<RenameRequest>> {
        self.rename.pending.take()
    }

    /// Turn a queued rename into a [`JobKind::Rename`] job and arm the undo.
    /// **Event loop only.**
    ///
    /// The undo is armed *before* the job runs and pruned when it finishes
    /// ([`App::finish_rename`]), so a batch interrupted half way through is
    /// still undoable for the half that happened - which is the design's
    /// "the operation is atomic per file, never per batch", said from the
    /// undo's side.
    pub fn start_rename(&mut self, request: RenameRequest) {
        let RenameRequest { pairs, undoing } = request;
        if pairs.is_empty() {
            self.message = Some("nothing to rename".to_string());
            return;
        }
        self.rename.arm_undo(&pairs, undoing);
        let spec = crate::ops::JobSpec::rename(pairs.clone());
        let id = self.request_job(spec);
        self.rename.remember(id, pairs);
    }

    /// Fold a finished rename job into the undo store and the result list.
    ///
    ///
    /// The pairs are the ones the job was given rather than a re-derivation
    /// from the plan, which is what stops the result list and what happened
    /// from drifting apart.
    pub fn finish_rename(&mut self, id: JobId, summary: &JobSummary) {
        let Some(pairs) = self.rename.take(id) else {
            return;
        };
        self.rename.result = crate::rename::exec::result_lines(&pairs, summary);
        self.rename.prune_undo(&summary.failures);
    }
}
