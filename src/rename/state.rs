//! What one session remembers about renaming.

use std::collections::HashMap;

use crate::app::RenameRequest;
use crate::ops::JobId;
use crate::rename::exec::{ResultLine, Undo};
use crate::rename::plan::Settings;
use crate::vfs::VfsPath;

/// Everything one run of the program remembers about the Multi-Rename Tool.
///
/// At most one multi-rename is in flight at a time; the undo held for the
/// session is the exact inverse of the last plan that actually ran, and the
/// result list describes that same plan and no other.
///
/// The pairs are kept per [`JobId`] rather than in a single slot because
/// the design lets a rename be backgrounded and a second one started on top
/// of it, and each job's result list has to be built from what that job was
/// asked to do.
#[derive(Debug, Default)]
pub struct MultiRename {
    /// `Start!` or `Undo` queued a multi-rename.
    ///
    /// One slot: the dialog closes when it answers, so a second rename cannot
    /// be asked for before this one has been turned into a job.
    pub pending: Option<Box<RenameRequest>>,

    /// the undo, "held for the session". One batch, the last one
    /// that ran, cleared once it has been undone.
    pub undo: Option<Undo>,

    /// The last multi-rename's result list, for the `Result list`
    /// button and the action that reopens it.
    pub result: Vec<ResultLine>,

    /// The dialog's remembered settings, so reopening `Ctrl+M` offers what was
    /// last used. Session state, not configuration.
    pub settings: Settings,

    /// The pairs each in-flight rename job was given, so its result list can
    /// be built from exactly what it was asked to do.
    jobs: HashMap<JobId, Vec<(VfsPath, VfsPath)>>,
}

impl MultiRename {
    /// Remember what a job was asked to rename, so its result list can be
    /// built from that and not from a re-derivation of the plan.
    pub fn remember(&mut self, id: JobId, pairs: Vec<(VfsPath, VfsPath)>) {
        self.jobs.insert(id, pairs);
    }

    /// Take back what a job was asked to rename, or `None` for a job this is
    /// not the record of.
    pub fn take(&mut self, id: JobId) -> Option<Vec<(VfsPath, VfsPath)>> {
        self.jobs.remove(&id)
    }

    /// Arm the undo for a plan that is about to run, or disarm it for one that
    /// is itself an undo.
    ///
    /// An undo is not itself undoable: the design holds one batch for the
    /// session, and re-arming from the reversal would make `Undo` toggle back
    /// and forth with no way to tell which way round it is.
    pub fn arm_undo(&mut self, pairs: &[(VfsPath, VfsPath)], undoing: bool) {
        self.undo = if undoing {
            None
        } else {
            Some(Undo::from_pairs(pairs))
        };
    }

    /// Drop from the undo the pairs whose rename failed, so it restores
    /// exactly what happened and never asks the filesystem to move a file that
    /// is still where it was.
    pub fn prune_undo(&mut self, failures: &[crate::ops::JobFailure]) {
        if let Some(undo) = self.undo.as_mut() {
            undo.prune(failures);
            if undo.is_empty() {
                self.undo = None;
            }
        }
    }
}
