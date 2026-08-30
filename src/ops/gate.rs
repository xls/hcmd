//! Jobs held back by the warning, and the ones let through.

use std::collections::HashSet;

use crate::ops::{JobId, JobRequest};

/// One job waiting on the warning.
///
/// It has already been given a [`JobId`] and a status row by the time it gets
/// here, so cancelling has to forget the job rather than merely drop this, or a
/// copy that never ran would sit in the queue view for ever. Holding it also
/// has to hide the progress dialog that row would otherwise open on top of the
/// question.
#[derive(Debug)]
pub struct Held {
    /// The job, exactly as it was queued.
    pub request: JobRequest,
    /// The message the design warned with.
    pub why: String,
    /// Whether its progress dialog was in the foreground before it was held,
    /// so `F2 Queue` is not silently undone by answering the question.
    pub foreground: bool,
}

/// A job that rewrites an archive in place is held until the user has agreed
/// once, per job, and a job that has been agreed to is never asked about again.
///
/// The two halves are one thing seen at two moments: a job is in the held
/// queue until it is answered and in the allowed set for exactly the one frame
/// between the answer and the spawn. Keeping them together is what makes
/// "never asked twice" checkable in one place, and what stops a later job that
/// is handed the same id inheriting an answer that was not about it.
#[derive(Debug, Default)]
pub struct RewriteGate {
    held: Vec<Held>,
    allowed: HashSet<JobId>,
}

impl RewriteGate {
    /// Hold a job until the warning it needs has been answered.
    pub fn hold(&mut self, held: Held) {
        self.held.push(held);
    }

    /// The warning on screen, which is the oldest one held.
    pub fn asking(&self) -> Option<&Held> {
        self.held.first()
    }

    /// Take the answered job off the front of the queue.
    pub fn answered(&mut self) -> Option<Held> {
        if self.held.is_empty() {
            return None;
        }
        Some(self.held.remove(0))
    }

    /// Record that this job may proceed. Consumed by the next [`Self::admits`]
    /// for that id and by no other.
    pub fn allow(&mut self, id: JobId) {
        self.allowed.insert(id);
    }

    /// Has this job already been agreed to?
    ///
    /// Takes the permission with it, so the next job to be given this id
    /// cannot inherit the answer.
    pub fn admits(&mut self, id: JobId) -> bool {
        self.allowed.remove(&id)
    }
}
