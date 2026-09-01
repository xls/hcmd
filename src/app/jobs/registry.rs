//! Every job this session has handed out an id for.

use std::collections::{HashMap, HashSet};

use crate::config::OpsConfig;
use crate::ops::queue::JobQueue;
use crate::ops::{JobHandle, JobId, JobRequest, JobSpec, JobStatus, SizeCache};
use crate::panel::Side;
use crate::vfs::VfsPath;

/// Every [`JobId`] this session has handed out is in exactly one state, and a
/// [`JobStatus`] is never shown without the [`JobSpec`] that explains what it
/// is a status of.
///
/// Ids are monotone and never reused, so an event about a job that has been
/// forgotten is dropped rather than resurrecting it. Forgetting a job drops
/// its status row, its spec, its handle and its place in the queue together,
/// which is what keeps the "exactly one state" true.
///
/// The admission controller lives here rather than in the event loop so that
/// cancelling a job that has not started yet, and asking whether one is still
/// waiting, are answerable from the same place as everything else about a job.
///
/// The size cache is here because it is what a finished
/// [`crate::ops::JobKind::Size`] leaves behind, and the only other thing that
/// touches it is a panel re-read invalidating an entry.
#[derive(Debug)]
pub struct Jobs {
    /// Running and recently finished jobs, in the order they were started,
    /// which is also the order the background queue view lists them.
    rows: Vec<JobStatus>,
    /// Jobs asked for and not yet offered to the queue.
    pending: Vec<JobRequest>,
    /// The admission controller (the background queue).
    queue: JobQueue,
    /// The spec each live job was built from, so the "option to
    /// retry the failures" can rebuild one over the paths that failed.
    specs: HashMap<JobId, JobSpec>,
    /// The workers' cancel flags and decision channels, by id.
    handles: HashMap<JobId, JobHandle>,
    /// Monotonic source for [`JobId`].
    next: u64,
    /// Jobs whose progress dialog the user closed, so it is not immediately
    /// put back while the worker winds down. Cancelling is not instantaneous.
    dismissed: HashSet<JobId>,
    /// Jobs whose `Finished` has already been acted on, so a finish is reported
    /// once rather than on every frame.
    settled: HashSet<JobId>,
    /// "When this job finishes cleanly, re-read this panel and put the cursor
    /// on the entry this path produced" - how `F7` lands on the directory it
    /// just made (the `+ F7`).
    follow: HashMap<JobId, (Side, VfsPath)>,

    /// Directory sizes computed this session.
    ///
    /// Written by a finished size job and invalidated when the panel re-reads
    /// the tree, which is the second half of the sentence.
    pub sizes: SizeCache,
}

impl Jobs {
    /// An empty registry whose queue is configured from `[ops]`.
    pub fn new(ops: &OpsConfig) -> Self {
        Self {
            rows: Vec::new(),
            pending: Vec::new(),
            queue: JobQueue::from_config(ops),
            specs: HashMap::new(),
            handles: HashMap::new(),
            next: 0,
            dismissed: HashSet::new(),
            settled: HashSet::new(),
            follow: HashMap::new(),
            sizes: SizeCache::new(),
        }
    }

    /// Every status row, in the order the jobs were started.
    pub fn rows(&self) -> &[JobStatus] {
        &self.rows
    }

    /// One job's status row.
    pub fn status(&self, id: JobId) -> Option<&JobStatus> {
        self.rows.iter().find(|j| j.id == id)
    }

    /// One job's status row, mutably.
    pub fn status_mut(&mut self, id: JobId) -> Option<&mut JobStatus> {
        self.rows.iter_mut().find(|j| j.id == id)
    }

    /// The spec a job was built from, kept as long as its status row is.
    pub fn spec(&self, id: JobId) -> Option<&JobSpec> {
        self.specs.get(&id)
    }

    /// True while any size walk is running, so the loop knows to keep waking to
    /// advance the size-column animation even when the walk is quiet.
    pub fn any_walking(&self) -> bool {
        self.rows
            .iter()
            .any(|status| status.finished.is_none() && status.kind == crate::ops::JobKind::Size)
    }

    /// True while any job is unfinished - running, queued or waiting on an
    /// answer. Drives the top-right activity indicator and keeps its animation
    /// advancing.
    pub fn any_active(&self) -> bool {
        self.rows.iter().any(|status| status.finished.is_none())
    }

    /// True while a size walk covering `path` is still running.
    ///
    /// `Space` or `Ctrl+L` on a directory queues a [`JobKind::Size`] over it;
    /// until that job finishes the figure is unknown, and the panel shows an
    /// animation in place of `<DIR>` for exactly these directories. Derived
    /// from the live jobs and their specs, so nothing has to be kept in step by
    /// hand and a cancelled walk stops animating the moment its row is gone.
    pub fn is_walking(&self, path: &crate::vfs::VfsPath) -> bool {
        self.rows.iter().any(|status| {
            status.finished.is_none()
                && status.kind == crate::ops::JobKind::Size
                && self
                    .specs
                    .get(&status.id)
                    .is_some_and(|spec| spec.sources.iter().any(|source| source == path))
        })
    }

    /// The first job that has not finished, which is the one a progress dialog
    /// shows.
    pub fn active(&self) -> Option<&JobStatus> {
        self.rows.iter().find(|j| j.finished.is_none())
    }

    /// Take an id nobody has had before, and file the status row and spec that
    /// go with it.
    ///
    /// The row exists before the worker does, so the progress dialog and the
    /// queue view have something to render from the moment the keystroke was
    /// pressed.
    pub fn admit(&mut self, spec: JobSpec, status: JobStatus, queue: bool) -> JobId {
        let id = status.id;
        self.rows.push(status);
        self.specs.insert(id, spec.clone());
        self.pending.push(JobRequest { id, spec, queue });
        id
    }

    /// The next id, which is never one that has been handed out before.
    pub fn next_id(&mut self) -> JobId {
        self.next = self.next.saturating_add(1);
        JobId(self.next)
    }

    /// Put a request back at the front of the queue, as an answered gate does.
    pub fn requeue(&mut self, request: JobRequest) {
        self.pending.push(request);
    }

    /// Take everything queued and not yet offered to admission control.
    pub fn take_pending(&mut self) -> Vec<JobRequest> {
        std::mem::take(&mut self.pending)
    }

    /// Replace what is queued, for the gate that holds some of it back.
    pub fn set_pending(&mut self, pending: Vec<JobRequest>) {
        self.pending = pending;
    }

    /// Offer one request to admission control, and say whether it may start.
    pub fn submit(&mut self, request: JobRequest) -> Option<JobRequest> {
        if request.queue {
            self.queue.enqueue(request);
            return None;
        }
        self.queue.submit(request, &self.rows)
    }

    /// Whatever the queue can release now that something has finished.
    pub fn release(&mut self) -> Vec<JobRequest> {
        self.queue.release(&self.rows)
    }

    /// How many jobs are waiting for a slot.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Remember a running worker's cancel flag and decision channel.
    pub fn register(&mut self, handle: JobHandle) {
        self.handles.insert(handle.id, handle);
    }

    /// The running worker's handle, if the job has started.
    pub fn handle(&self, id: JobId) -> Option<&JobHandle> {
        self.handles.get(&id)
    }

    /// Drop a running worker's handle, once it has finished.
    pub fn drop_handle(&mut self, id: JobId) {
        self.handles.remove(&id);
    }

    /// Take a job out of the queue before it has started, and say whether it
    /// was there.
    pub fn cancel_queued(&mut self, id: JobId) -> bool {
        if !self.queue.contains(id) {
            return false;
        }
        self.queue.cancel(id);
        self.pending.retain(|r| r.id != id);
        true
    }

    /// Forget a job entirely: its row, its spec, its handle and its place in
    /// the queue.
    ///
    /// All four together, because a job that is in one of them and not the
    /// others is a job in no state at all.
    pub fn forget(&mut self, id: JobId) {
        self.rows.retain(|j| j.id != id);
        self.handles.remove(&id);
        self.specs.remove(&id);
        self.dismissed.remove(&id);
        if self.queue.contains(id) {
            self.queue.cancel(id);
        }
    }

    /// Note that the user closed this job's progress dialog.
    pub fn dismiss(&mut self, id: JobId) {
        self.dismissed.insert(id);
    }

    /// Has the user closed this job's progress dialog?
    pub fn is_dismissed(&self, id: JobId) -> bool {
        self.dismissed.contains(&id)
    }

    /// Has this job's `Finished` already been acted on?
    pub fn is_settled(&self, id: JobId) -> bool {
        self.settled.contains(&id)
    }

    /// Record that this job's `Finished` has been acted on, and that its
    /// dialog is no longer dismissed because there is no longer one to dismiss.
    pub fn settle(&mut self, id: JobId) {
        self.settled.insert(id);
        self.dismissed.remove(&id);
    }

    /// Ask for a panel to be re-read and landed on `dest` when this job ends
    /// cleanly.
    pub fn follow(&mut self, id: JobId, side: Side, dest: VfsPath) {
        self.follow.insert(id, (side, dest));
    }

    /// Take back what this job was to be followed to, if anything.
    pub fn take_follow(&mut self, id: JobId) -> Option<(Side, VfsPath)> {
        self.follow.remove(&id)
    }
}
