//! Every file operation this session has started.
//!
//! Every [`JobId`] handed out is in exactly one state, and a [`JobStatus`] is
//! never shown without the [`JobSpec`] that explains what it is a status of.
//! Ids only go up and are never reused, so an event about a job that has been
//! forgotten is dropped rather than resurrecting it.
//!
//! Nothing here runs an operation. [`crate::input::dispatch`] queues a spec,
//! the event loop starts it, and what comes back are
//! [`crate::ops::JobUpdate`]s. A job whose progress dialog is on screen and
//! one that was sent to the background are the same job in the same state
//! seen from two places, which is what makes backgrounding reversible.
//!
//! # The rewrite gate
//!
//! A job that rewrites an archive in place is held until the user has agreed,
//! once, per job. A job that has been agreed to is never asked about again,
//! and a held job already has its id and its status row, so refusing has to
//! forget it rather than merely drop it or a copy that never ran would sit in
//! the queue view for ever.

pub mod draft;
pub mod registry;

use crate::app::{App, PackRequest};
use crate::input::{DialogId, Focus};
use crate::ops::{
    Decision, JobEvent, JobHandle, JobId, JobKind, JobRequest, JobSpec, JobStatus, JobSummary,
    JobUpdate,
};
use crate::panel::Side;
use crate::ui::dialog::{ConflictDialog, ProgressDialog, SummaryDialog};
use crate::vfs::{BackendKind, VfsPath};

impl App {
    /// Queue `Alt+F5`'s pack.
    pub fn request_pack(&mut self, request: PackRequest) {
        self.pending_pack = Some(request);
    }

    /// Drain the queued pack. The event loop calls this once a frame.
    pub fn take_pending_pack(&mut self) -> Option<PackRequest> {
        self.pending_pack.take()
    }

    /// Queue the job `Alt+F5` asked for.
    ///
    /// **Blocking**, and called from the event loop rather than from
    /// `dispatch`, because it asks the filesystem whether the target is
    /// already there.
    ///
    /// The container is **not** written here. It used to be: an empty archive
    /// was created and an ordinary `F5` into `<archive>#/` was queued to fill
    /// it, which made packing *N* files *N* rewrites of the container for
    /// every format that cannot write a member in place, and put the size
    /// gate in the middle of the job instead of in front of it. The whole
    /// selection now goes into one `ArchiveFormat::create` - see
    /// [`crate::ops::pack`] - so there is nothing to create in advance and
    /// nothing left behind when a pack fails.
    ///
    /// What is still decided here is what the design wants decided before
    /// anything is read: a format that cannot be written, and a target that
    /// already exists, are refused with a message rather than queued.
    /// "Move to archive" is `JobKind::Move`, which deletes the sources only
    /// after a pack that succeeded.
    pub fn perform_pack(&mut self, request: PackRequest) {
        let PackRequest {
            container,
            format,
            level,
            sources,
            move_sources,
        } = request;
        let Some(local) = container.local_path() else {
            self.message = Some(format!(
                "{container}: an archive is packed onto a real filesystem"
            ));
            return;
        };
        if local.exists() {
            // Overwriting an archive that already exists would silently
            // discard everything in it. The name is the user's to change.
            self.message = Some(format!("{container} already exists"));
            return;
        }
        if !format.backend().write_model().writable() {
            self.message = Some(format!(
                "{container}: a {format} archive cannot be written to"
            ));
            return;
        }

        let kind = if move_sources {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        let name = container
            .file_name()
            .unwrap_or_else(|| format.label().to_string());
        let mut options = crate::ops::JobOptions::from_config(&self.config.ops);
        options.pack = Some(crate::ops::PackInto { format, level });
        self.message = Some(format!("packing {} item(s) into {name}", sources.len()));
        self.request_job(
            JobSpec::new(kind, sources, Some(container.clone())).with_options(options),
        );
    }

    /// the two gates, applied to everything queued and not yet
    /// started - "both checked before anything is touched".
    ///
    /// Serviced by the event loop rather than by `dispatch`, for the reason
    /// every other filesystem touch is: the refusal needs the archive's size
    /// and the free space beside it, and `dispatch` may not ask.
    /// [`crate::vfs::ArchiveFs`] keeps the refusal
    /// as a backstop for a write that arrives without having come through
    /// here; it cannot keep the **warning**, because only a human can answer
    /// one.
    ///
    /// **Blocking**, like [`App::perform_pack`] and for the same reasons.
    pub fn service_rewrite_gates(&mut self) {
        use crate::vfs::archive::RewriteGate;

        let pending = self.jobs.take_pending();
        let mut keep = Vec::with_capacity(pending.len());
        for request in pending {
            // Already answered: through, and out of the set, so the next job
            // to be given this id cannot inherit the answer.
            if self.rewrite_gate.admits(request.id) {
                keep.push(request);
                continue;
            }
            match self.rewrite_gate_for(&request.spec) {
                None | Some(RewriteGate::Proceed) => keep.push(request),
                Some(RewriteGate::Refuse(why)) => {
                    // Refused, not asked: step 1 is "not a
                    // warning - a refusal". The status row goes with it, since
                    // nothing will ever run under it.
                    //
                    // In a box rather than on the status line, because the refusal
                    // to carry "the reason stated and the suggestion to extract,
                    // modify and repack deliberately" - two clauses and a number,
                    // which is more than one cropped status line can hold.
                    self.forget_job(request.id);
                    self.show_message("Rewrite refused", gate_lines(&why));
                }
                Some(RewriteGate::Warn(why)) => {
                    // A held job still has the status row `enqueue_job` gave
                    // it, and `sync_job_dialogs` would open a progress dialog
                    // over the question - a `Copying` box, with a bar, for a
                    // copy that has not been allowed to start. So it is put in
                    // the background for as long as it is held, which is
                    // exactly what "there is nothing on screen for this job"
                    // means here.
                    let foreground = self.job(request.id).is_some_and(|s| !s.background);
                    self.background_job(request.id);
                    self.rewrite_gate.hold(crate::ops::gate::Held {
                        request,
                        why,
                        foreground,
                    });
                }
            }
        }
        self.jobs.set_pending(keep);
        self.ask_rewrite_gate();
    }

    /// Which of the gates a queued job falls into, or `None` when
    /// the design has nothing to say about it - which is every job
    /// whose destination is not inside a format that rewrites in
    /// full.
    fn rewrite_gate_for(&self, spec: &JobSpec) -> Option<crate::vfs::archive::RewriteGate> {
        if !matches!(spec.kind, JobKind::Copy | JobKind::Move) {
            return None;
        }
        let dest = spec.dest.as_ref()?;
        if dest.backend() != BackendKind::Archive {
            return None;
        }
        // Deliberately the session that is **already** open: a destination
        // inside an archive can only have been named by a panel that opened
        // one, so this asks a question rather than creating a directory.
        let session = self.open_archive_session()?;
        let archive = session.open(dest).ok()?;
        if !archive.write_model().needs_rewrite_gates() {
            return None;
        }
        let size = std::fs::metadata(archive.container())
            .map(|meta| meta.len())
            .ok()?;
        Some(session.limits().gate(
            size,
            session.temp_root(),
            &archive.display_path().to_string(),
        ))
    }

    /// Put the warning on screen, if one is waiting and none is
    /// already being asked.
    ///
    /// The buttons are `Rewrite` / `Cancel` and the dialog opens on the
    /// second: the design asks for "a cancel that is the default button",
    /// which is [`crate::dialog::ConfirmDialog`]'s own default and is not
    /// overridden here.
    fn ask_rewrite_gate(&mut self) {
        if self
            .dialogs
            .iter()
            .any(|frame| frame.dialog.id() == DialogId::ConfirmRewrite)
        {
            return;
        }
        let Some(held) = self.rewrite_gate.asking() else {
            return;
        };
        let lines = gate_lines(&held.why);
        self.push_dialog(Box::new(
            crate::dialog::ConfirmDialog::new(
                DialogId::ConfirmRewrite,
                "Rewrite the archive?",
                lines,
            )
            .with_buttons("Rewrite", "Cancel"),
        ));
    }

    /// the warning was answered.
    ///
    /// `true` puts the job back where it was, marked as answered so the gate
    /// does not ask again; it then goes through admission control exactly as
    /// an ungated one does. `false` forgets it, because the status row was
    /// created when the keystroke queued it and nothing will ever run under
    /// it.
    pub fn resume_rewrite_gate(&mut self, proceed: bool) {
        let Some(crate::ops::gate::Held {
            request,
            foreground,
            ..
        }) = self.rewrite_gate.answered()
        else {
            return;
        };
        if proceed {
            self.rewrite_gate.allow(request.id);
            if foreground {
                self.foreground_job(request.id);
            }
            self.jobs.requeue(request);
        } else {
            self.forget_job(request.id);
            self.message = Some("the rewrite was cancelled; the archive is unchanged".to_string());
        }
        self.ask_rewrite_gate();
    }

    /// Queue a job. Returns the id it will run under.
    ///
    /// The exact analogue of [`App::request_read`]: `dispatch` never touches
    /// the filesystem, so an operation becomes a [`JobRequest`] and the event
    /// loop spawns it. A [`JobStatus`] row is created immediately, so the
    /// progress dialog and the queue view have something to render before the
    /// worker exists.
    pub fn request_job(&mut self, spec: JobSpec) -> JobId {
        self.enqueue_job(spec, false)
    }

    /// `F2 Queue`: "append to the background queue instead of starting now".
    ///
    ///
    /// The difference from [`App::request_job`] is real work rather than a
    /// display flag: the request waits in [`crate::ops::queue::JobQueue`] until
    /// a slot frees, which is what makes the button's own label true.
    pub fn queue_job(&mut self, spec: JobSpec) -> JobId {
        let id = self.enqueue_job(spec, true);
        self.background_job(id);
        id
    }

    fn enqueue_job(&mut self, spec: JobSpec, queue: bool) -> JobId {
        let id = self.jobs.next_id();
        let mut status = JobStatus::queued(id, spec.kind);
        // A kind with no progress dialog starts in the background rather than
        // as an invisible foreground job: it is still listed in the queue
        // view, still cancellable, and `foreground_job_status` correctly says
        // there is nothing on screen. See [`App::shows_progress`].
        status.background = !Self::shows_progress(spec.kind);
        self.jobs.admit(spec, status, queue)
    }

    /// Drain the jobs that may start now. The event loop calls this once a
    /// frame and hands each to [`crate::ops::spawn`].
    ///
    /// Everything newly requested is offered to the queue first, and whatever
    /// the queue lets go of comes back with it: the "a second `F5`
    /// while a copy is running enqueues rather than refusing" is this call, not
    /// a display flag. `Size` walks and `mkdir` never wait - see
    /// [`crate::ops::queue::serialises`].
    pub fn take_pending_jobs(&mut self) -> Vec<JobRequest> {
        let mut out = Vec::new();
        for request in self.jobs.take_pending() {
            if let Some(go) = self.jobs.submit(request) {
                out.push(go);
            }
        }
        out.extend(self.jobs.release());
        out
    }

    /// How many jobs are waiting for a slot (the queue view).
    pub fn queued_jobs(&self) -> usize {
        self.jobs.queued()
    }

    /// Record a spawned worker's handle, so `Esc` can cancel it and a conflict
    /// dialog can answer it. The event loop calls this with what `spawn`
    /// returned.
    pub fn register_job(&mut self, handle: JobHandle) {
        self.jobs.register(handle);
    }

    /// One job's live state.
    pub fn job(&self, id: JobId) -> Option<&JobStatus> {
        self.jobs.status(id)
    }

    /// The job the progress dialog should be showing: the first that has not
    /// finished. `None` when nothing is running.
    pub fn active_job(&self) -> Option<&JobStatus> {
        self.jobs.active()
    }

    /// True while any job is still running (the "a second `F5`
    /// while a copy is running enqueues rather than refusing").
    pub fn has_running_job(&self) -> bool {
        self.active_job().is_some()
    }

    /// `Esc` on the progress dialog: stop a job.
    ///
    /// The worker notices between files and inside its chunk loop, removes any
    /// partial destination, and reports `cancelled` on its summary. Safe to
    /// call on a job that has already finished.
    pub fn cancel_job(&mut self, id: JobId) {
        // A job that has not started has no worker to notice a flag, so it is
        // dropped from the queue and reported as cancelled here - otherwise the
        // row would sit at "queued" for the rest of the session.
        if self.jobs.cancel_queued(id) {
            if let Some(status) = self.jobs.status_mut(id) {
                status.pending_decision = None;
                status.finished = Some(Box::new(JobSummary {
                    kind: status.kind,
                    files_done: 0,
                    dirs_done: 0,
                    bytes_done: 0,
                    skipped: 0,
                    failures: Vec::new(),
                    cancelled: true,
                    elapsed: std::time::Duration::ZERO,
                    sized: Vec::new(),
                    differing: Vec::new(),
                    first_difference: None,
                }));
            }
            return;
        }
        if let Some(handle) = self.jobs.handle(id) {
            handle.cancel();
        }
    }

    /// The spec a job was built from, for the retry.
    pub fn job_spec(&self, id: JobId) -> Option<&JobSpec> {
        self.jobs.spec(id)
    }

    /// Answer a [`JobEvent::NeedsDecision`].
    ///
    /// Non-blocking, so it is safe from `dispatch`. Returns false when the
    /// worker has already gone.
    pub fn answer_job(&mut self, id: JobId, decision: Decision) -> bool {
        if let Some(status) = self.jobs.status_mut(id) {
            status.pending_decision = None;
        }
        self.jobs
            .handle(id)
            .is_some_and(|handle| handle.answer(decision))
    }

    /// Fold one worker event into state.
    ///
    /// The analogue of [`App::apply_vfs_event`], including the staleness rule:
    /// an update for a job with no status row is dropped, which is what
    /// happens after [`App::forget_job`].
    ///
    /// A finished [`JobKind::Size`] populates the size cache,
    /// which is what makes the panel status line's `\u{2265}` resolve into a
    /// number.
    pub fn apply_job_event(&mut self, update: JobUpdate) {
        let JobUpdate { id, event } = update;
        let Some(status) = self.jobs.status_mut(id) else {
            return;
        };
        status.apply(&event);

        if let JobEvent::Finished { summary } = &event {
            // Only completed walks are here: `walk::run` records a root only
            // when its walk finished, so a partial figure can never be cached
            // (the last bullet).
            for (path, stats) in &summary.sized {
                self.jobs.sizes.insert(path.clone(), *stats);
            }
            self.jobs.drop_handle(id);
        }
    }

    /// `F2` / the `Background` button: send a job's dialog away and leave the
    /// job running.
    ///
    /// Nothing about the worker changes. Foreground and background are two
    /// **views of one job**, not two kinds of job, which is why this is a flag
    /// on the status row and not a different code path.
    pub fn background_job(&mut self, id: JobId) {
        if let Some(status) = self.jobs.status_mut(id) {
            status.background = true;
        }
    }

    /// `Enter` on a running job in the queue view: bring it back to the
    /// foreground, "exactly as it was - same bars, same counts, same rate,
    /// still running".
    ///
    /// It is exactly as it was because the status row never stopped being
    /// updated while it was in the background.
    pub fn foreground_job(&mut self, id: JobId) {
        if let Some(status) = self.jobs.status_mut(id) {
            status.background = false;
        }
    }

    /// The job whose progress dialog belongs on screen: the first running one
    /// that has not been backgrounded.
    pub fn foreground_job_status(&self) -> Option<&JobStatus> {
        self.jobs
            .rows()
            .iter()
            .find(|j| j.finished.is_none() && !j.background)
    }

    /// Is any backgrounded job still going? The key bar shows an indicator
    /// while one is.
    pub fn has_background_job(&self) -> bool {
        self.jobs
            .rows()
            .iter()
            .any(|j| j.finished.is_none() && j.background)
    }

    /// Is a backgrounded job blocked on a conflict?
    ///
    /// such a job "does not sit silently blocked" - the key-bar
    /// indicator changes to say a job needs attention.
    pub fn job_needs_attention(&self) -> bool {
        self.jobs.rows().iter().any(JobStatus::needs_attention)
    }

    /// Jobs that finished while backgrounded and have not been looked at.
    ///
    /// a job finishing in the background "does not steal
    /// focus". Its result waits in the queue view, so this is what the queue
    /// view reads rather than something that opens a dialog.
    pub fn finished_background_jobs(&self) -> impl Iterator<Item = &JobStatus> {
        self.jobs
            .rows()
            .iter()
            .filter(|j| j.background && j.finished.is_some())
    }

    /// Drop a finished job's status row.
    ///
    /// The queue view keeps finished rows until the user dismisses them
    /// (the design lists "pending, active and failed"), so nothing removes
    /// them automatically.
    pub fn forget_job(&mut self, id: JobId) {
        // Row, spec, handle and queue slot together: a job forgotten before
        // it ever started must not be handed out by a later `release`, since
        // nothing would render it and nothing would be able to cancel it.
        self.jobs.forget(id);
    }

    /// `Ctrl+L`, and `Space` on a directory: size some paths.
    ///
    ///
    /// Paths that are already in the cache are dropped, so `Space` on a
    /// directory that has been sized costs nothing and `Ctrl+L` after a few
    /// `Space`s only walks what is left. Returns `None` when there was nothing
    /// to do.
    pub fn request_size(&mut self, paths: Vec<VfsPath>) -> Option<JobId> {
        let wanted: Vec<VfsPath> = paths
            .into_iter()
            .filter(|p| !self.jobs.sizes.contains(p))
            .collect();
        if wanted.is_empty() {
            return None;
        }
        Some(self.request_job(JobSpec::size(wanted)))
    }

    /// Whether starting a job of this kind opens a progress dialog
    /// ("Starting an operation opens a progress dialog").
    ///
    /// Two kinds are excluded, and both exclusions are the design's:
    ///
    /// * [`JobKind::Size`], because the design requires the panel to stay
    ///   usable while a tree is walked - "a slow tree must not freeze the UI".
    ///   A modal box over `Ctrl+L` is exactly the freeze that forbids, and it
    ///   would also make `Space` on a directory unusable as a marking key.
    /// * [`JobKind::Mkdir`], because it is one `mkdir(2)` per level. A dialog
    ///   for it could only ever flash, and its result is a status line.
    ///
    /// Excluded kinds start already backgrounded, so they are still listed in
    /// the queue view and are still cancellable - they simply do not take the
    /// screen.
    const fn shows_progress(kind: JobKind) -> bool {
        matches!(
            kind,
            JobKind::Copy
                | JobKind::Move
                | JobKind::Delete { .. }
                | JobKind::Compare
                | JobKind::Resize
        )
    }

    /// Note that `job`, once it finishes cleanly, should leave `side`'s cursor
    /// on whatever entry `dest` produced in the panel's own directory.
    ///
    /// This is `F7`: after `photos/2026/summer` is created under `/home/t`,
    /// the cursor belongs on `photos`, which is the only one of the three that
    /// becomes a row (see [`crate::ops::mkdir::cursor_name`]).
    pub fn follow_job(&mut self, job: JobId, side: Side, dest: VfsPath) {
        self.jobs.follow(job, side, dest);
    }

    /// The job whose progress dialog belongs on screen right now.
    ///
    /// Differs from [`App::foreground_job_status`] in one way: a job whose
    /// dialog the user closed with `Esc` is skipped. Cancelling is not
    /// instantaneous - the worker notices between chunks - and without this
    /// the dialog would be put straight back on the frame after the `Esc`
    /// that dismissed it.
    fn progress_job(&self) -> Option<JobId> {
        self.jobs
            .rows()
            .iter()
            .find(|j| {
                j.finished.is_none()
                    && !j.background
                    && Self::shows_progress(j.kind)
                    && !self.jobs.is_dismissed(j.id)
            })
            .map(|j| j.id)
    }

    /// Keep the job dialogs in step with the job table - once a frame.
    ///
    /// This is the whole of the job-dialog wiring, in one place the event loop
    /// calls and a headless test can call too:
    ///
    /// 1. a job that has finished pops its dialog and reports (a summary box
    ///    for a foreground failure, the status line otherwise), and the panels
    ///    re-read so the result is visible;
    /// 2. a job that is waiting on a conflict raises the conflict dialog;
    /// 3. the progress dialog opens for a job that wants one and closes when
    ///    that job is backgrounded, finished or dismissed;
    /// 4. every dialog on the stack is handed the live job table, which is
    ///    what makes a backgrounded job come back "exactly as it was".
    pub fn sync_job_dialogs(&mut self) {
        self.settle_finished_jobs();
        self.sync_progress_dialog();
        self.sync_conflict_dialog();
        let jobs = self.jobs.rows().to_vec();
        for frame in &mut self.dialogs {
            frame.dialog.job_update(&jobs);
        }
    }

    /// Act on each job that has finished since the last frame, exactly once.
    fn settle_finished_jobs(&mut self) {
        let finished: Vec<(JobId, bool, JobSummary)> = self
            .jobs
            .rows()
            .iter()
            .filter(|j| !self.jobs.is_settled(j.id))
            .filter_map(|j| {
                j.finished
                    .as_deref()
                    .map(|summary| (j.id, j.background, summary.clone()))
            })
            .collect();
        for (id, background, summary) in finished {
            self.jobs.settle(id);
            self.close_progress_dialog(id);
            self.report_finished(id, background, summary);
        }
    }

    /// The panels, the cursor and the status line after one job.
    fn report_finished(&mut self, id: JobId, background: bool, summary: JobSummary) {
        // A job that changes nothing on disk leaves the panels alone; every
        // other kind has to re-read them or the operation looks like it did
        // not happen. the compare is the second read-only kind, and
        // for it a re-read would be worse than pointless: it would rebuild the
        // listings underneath the marks the comparison just placed.
        if summary.kind.is_destructive() {
            self.reread_both();
        }
        if let Some((side, dest)) = self.jobs.take_follow(id)
            && summary.is_clean()
        {
            let base = self.panel(side).active_tab().path.clone();
            if let Some(name) = crate::ops::mkdir::cursor_name(&base, &dest) {
                self.panel_mut(side).active_tab_mut().pending_select = Some(name);
            }
        }
        // the undo and result list, folded in before anything is
        // reported: both are built from the pairs the job was given, and both
        // have to exist whether the batch ran in the foreground or the
        // background.
        if summary.kind == JobKind::Rename {
            self.finish_rename(id, &summary);
        }
        match summary.kind {
            // The `\u{2265}` in the status line resolving into a number is the
            // feedback; a message per `Space` would be noise.
            // A walk that could not read everything is the exception: its
            // figure stays a lower bound, and the "never silently
            // report a computed-looking total that is actually partial" means
            // the reason has to reach the user somewhere other than the queue
            // view. The status line, not a dialog: a walk steals no focus.
            JobKind::Size => {
                if let Some(first) = summary.failures.first() {
                    self.message = Some(match summary.failures.len() {
                        1 => format!("{}: {}", first.path, first.error),
                        n => format!("{}: {} (and {} more)", first.path, first.error, n - 1),
                    });
                }
            }
            // No dialog was shown, so the status line is the only report there
            // is - and it is where a failure has to appear.
            JobKind::Mkdir => self.message = Some(summary.message()),
            // A rename has no progress dialog and therefore no
            // summary box either, so the status line is the whole of its
            // report - and a batch that failed has to say where the detail is,
            // which is the result list the dialog's button and
            // `Action::RenameResult` both open.
            JobKind::Rename => {
                let mut line = summary.message();
                if !summary.failures.is_empty() {
                    line.push_str("; see the result list");
                }
                self.message = Some(line);
            }
            // the contents comparison marks and says how many,
            // in the status line. It opens no summary box, because a
            // comparison that found nothing has nothing to show and one that
            // found something has already shown it - in both panels.
            JobKind::Compare => self.finish_compare(&summary),
            // Comparing two named files answers in a sentence rather than in
            // marks, so this one does open a box: there is nothing on either
            // panel for it to have shown already.
            JobKind::CompareFiles => self.finish_compare_files(&summary),
            // a job that finishes in the background "does not
            // steal focus"; its result waits in the queue view.
            _ if background => {}
            _ if summary.failures.is_empty() => self.message = Some(summary.message()),
            // "show a summary at the end with the option to
            // retry the failures".
            _ => {
                let offer = crate::ops::delete::permanent_delete_offer(&summary);
                // A summary of nothing but "there was no trash" would say the
                // same thing as the offer and bury the question underneath it.
                if offer.len() < summary.failures.len() {
                    self.push_dialog(Box::new(SummaryDialog::new(id, summary)));
                }
                if !offer.is_empty() {
                    self.offer_permanent_delete(offer);
                }
            }
        }
    }

    /// what `F8` could not trash because there was nowhere to
    /// trash it to is **never silent** - it comes back as the one question that
    /// can still be answered.
    ///
    /// The up-front probe ([`App::service_trash_probe`]) is what normally keeps
    /// this from happening at all; this is the recovery half, for the trash
    /// that disappeared between the prompt and the operation.
    fn offer_permanent_delete(&mut self, sources: Vec<VfsPath>) {
        let mut lines = vec![
            "There is no trash on this filesystem.".to_string(),
            String::new(),
        ];
        lines.extend(crate::ops::delete::confirm_lines(&sources, false));
        let confirm =
            crate::dialog::ConfirmDialog::new(DialogId::ConfirmDelete, "Delete permanently", lines)
                .with_buttons("Delete", "Cancel")
                .defaulting_to_yes();
        self.draft.op = Some(JobKind::Delete { trash: false });
        self.draft.sources = sources;
        self.push_dialog(Box::new(confirm));
    }

    /// Answer the trash-availability question the delete keystroke queued, and
    /// push the confirmation the design describes.
    ///
    /// Called by the event loop, never by `dispatch`.
    /// The whole point of doing it here is the spec's own clause: whether `F8`
    /// can keep its promise is "decided *before* the operation starts, never
    /// discovered during it", so the prompt the user answers is the one that
    /// already knows.
    pub fn service_trash_probe(&mut self) {
        let Some(sources) = self.draft.trash_probe.take() else {
            return;
        };
        if sources.is_empty() {
            return;
        }
        let split = crate::ops::delete::split_by_trash(&sources);
        let lines = crate::ops::delete::availability_lines(&split);
        // The affirmative says what will actually happen to the batch: `Trash`
        // only when every item can go there, `Delete` otherwise - the spec's
        // "changes its own affirmative to `Delete`".
        let affirmative = if split.untrashable.is_empty() {
            "Trash"
        } else {
            "Delete"
        };
        let title = if split.untrashable.is_empty() {
            "Delete to trash"
        } else {
            "Delete permanently"
        };
        let confirm = crate::dialog::ConfirmDialog::new(DialogId::ConfirmDelete, title, lines)
            .with_buttons(affirmative, "Cancel")
            // "The confirmation's affirmative is the default button, not
            // `Cancel`."
            .defaulting_to_yes();
        self.draft.op = Some(JobKind::Delete {
            trash: !split.trashable.is_empty(),
        });
        self.draft.sources = sources;
        self.draft.trash_split = Some(split);
        self.push_dialog(Box::new(confirm));
    }

    /// Re-read both panels' active tabs, which also invalidates their cached
    /// directory sizes.
    pub(super) fn reread_both(&mut self) {
        for side in [Side::Left, Side::Right] {
            self.reread(side);
        }
    }

    /// Open, close or replace the progress dialog so it shows
    /// [`App::progress_job`].
    fn sync_progress_dialog(&mut self) {
        let want = self.progress_job();
        let have = self
            .dialogs
            .iter()
            .find(|f| f.dialog.id() == DialogId::Progress)
            .and_then(|f| f.dialog.job());
        if have == want {
            return;
        }
        if let Some(id) = have {
            self.close_progress_dialog(id);
        }
        if let Some(id) = want
            && let Some(status) = self.job(id)
        {
            let min = self.config.ops.file_bar_min_size.bytes();
            self.push_dialog(Box::new(ProgressDialog::new(status.clone(), min)));
        }
    }

    /// Raise the conflict dialog for a job parked on a decision.
    ///
    ///
    /// Only for a job whose progress dialog is on screen: a backgrounded one
    /// "marks itself as waiting in the queue view" instead, and is answered by
    /// bringing it forward.
    fn sync_conflict_dialog(&mut self) {
        if self
            .dialogs
            .iter()
            .any(|f| f.dialog.id() == DialogId::Conflict)
        {
            return;
        }
        let Some(id) = self.progress_job() else {
            return;
        };
        let Some(request) = self.job(id).and_then(|j| j.pending_decision.clone()) else {
            return;
        };
        let suggested = request
            .dest
            .local_path()
            .map(crate::ops::copy::free_name)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let dialog = ConflictDialog::new(id, request, suggested, &self.config.panel);
        self.push_dialog(Box::new(dialog));
    }

    /// Take a job's progress dialog off the stack wherever it sits.
    ///
    /// A conflict dialog opens *on top* of it, so this cannot be a `pop`. Any
    /// conflict dialog above it goes too: the question it was asking belongs
    /// to a job that is no longer on screen.
    fn close_progress_dialog(&mut self, id: JobId) {
        let Some(at) = self
            .dialogs
            .iter()
            .position(|f| f.dialog.id() == DialogId::Progress && f.dialog.job() == Some(id))
        else {
            return;
        };
        self.dialogs.truncate(at);
        match self.dialogs.last() {
            Some(next) => self.set_focus(Focus::Dialog(next.dialog.id())),
            None => self.set_focus(Focus::Panel(self.active_side)),
        }
    }

    /// The user closed a job's progress dialog: do not put it back.
    ///
    /// `Esc` cancels, and the worker takes a moment to notice. Without this
    /// the next frame would reopen the dialog the `Esc` had just dismissed.
    pub fn dismiss_job_dialog(&mut self, id: JobId) {
        self.jobs.dismiss(id);
    }
}

/// One the design gate message, split into dialog lines.
///
/// [`crate::dialog::ConfirmDialog`] takes a line per `String` and **crops**
/// rather than wraps - the same contract every other dialog in this program is
/// written to - so a message that arrives as one sentence would be shown as
/// one cropped line. the design wants the warning to name "the size and
/// that the whole archive will be rewritten", and a crop removes exactly the
/// second half of that.
///
/// Split at `; ` and at `. `, which is where [`crate::vfs::ArchiveSession`]'s
/// gate messages join their clauses. Neither sequence occurs inside a figure,
/// a path or a `the design`, so this cuts sentences and nothing else.
fn gate_lines(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = message;
    loop {
        let at = ["; ", ". "]
            .iter()
            .filter_map(|sep| rest.find(sep).map(|at| at.saturating_add(sep.len())))
            .min();
        let Some(at) = at else { break };
        let (head, tail) = rest.split_at(at);
        out.push(head.trim().to_string());
        rest = tail;
    }
    if !rest.trim().is_empty() {
        out.push(rest.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};

    #[test]
    fn a_job_is_queued_for_the_event_loop_rather_than_run_by_dispatch() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let id = app.request_job(JobSpec::size(vec![VfsPath::local("/etc")]));
        // A status row exists before the worker does, so the progress dialog
        // has something to render immediately.
        assert!(app.job(id).is_some());
        assert!(app.has_running_job());
        let queued = app.take_pending_jobs();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, id);
        assert!(
            app.take_pending_jobs().is_empty(),
            "draining is destructive"
        );
    }

    #[test]
    fn a_finished_size_job_populates_the_cache_and_resolves_the_bound() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let path = VfsPath::local("/root/sub");
        let id = app.request_job(JobSpec::size(vec![path.clone()]));
        assert!(app.jobs.sizes.get(&path).is_none());

        let stats = crate::ops::TreeStats {
            bytes: 4096,
            files: 2,
            dirs: 1,
        };
        app.apply_job_event(JobUpdate {
            id,
            event: JobEvent::Finished {
                summary: Box::new(crate::ops::JobSummary {
                    kind: JobKind::Size,
                    files_done: 2,
                    dirs_done: 1,
                    bytes_done: 4096,
                    skipped: 0,
                    failures: Vec::new(),
                    cancelled: false,
                    elapsed: std::time::Duration::ZERO,
                    sized: vec![(path.clone(), stats)],
                    differing: Vec::new(),
                    first_difference: None,
                }),
            },
        });
        assert_eq!(app.jobs.sizes.get(&path), Some(stats));
        assert!(!app.has_running_job());
    }

    /// A walked directory shows its tree size in the `size` column and still
    /// sorts as though it had none, next to the directories that were never
    /// walked. That is the decision, not a gap: sorting must never queue a
    /// walk, and a key only the already-visited rows carry would order the
    /// panel by where the user happened to have been.
    #[test]
    fn a_walked_directory_sorts_by_name_like_every_other_directory() {
        use crate::panel::{ColumnId, SortKey};
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let root = VfsPath::local("/root");
        let tab = app.left.active_tab_mut();
        tab.path = root.clone();
        tab.entries = ["zulu", "alpha", "mike"]
            .iter()
            .map(|n| crate::vfs::Entry::dir(*n))
            .collect();
        // `alpha` has been walked to 926 MB and `mike` to 176 bytes; `zulu`
        // never was. Their inode sizes disagree with all of that.
        for e in &mut app.left.active_tab_mut().entries {
            e.size = if e.name == "zulu" { 12_288 } else { 4_096 };
        }
        app.jobs.sizes.insert(
            root.join("alpha"),
            crate::ops::TreeStats {
                bytes: 926_000_000,
                files: 9,
                dirs: 2,
            },
        );
        app.jobs.sizes.insert(
            root.join("mike"),
            crate::ops::TreeStats {
                bytes: 176,
                files: 1,
                dirs: 0,
            },
        );

        app.sort_active(SortKey::Column(ColumnId::Size));

        let names: Vec<&str> = app
            .left
            .active_tab()
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["alpha", "mike", "zulu"],
            "walked or not, directories tie on size and the name tiebreak orders them"
        );
        assert!(
            app.take_pending_jobs().is_empty(),
            "sorting queues no walk: a panel of ten thousand folders must cost what it did"
        );
        assert!(!app.has_running_job());
    }

    #[test]
    fn a_re_read_invalidates_the_cached_size_of_that_tree() {
        // "invalidated when the panel re-reads it".
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.jobs
            .sizes
            .insert(VfsPath::local("/a"), crate::ops::TreeStats::ZERO);
        app.jobs
            .sizes
            .insert(VfsPath::local("/a/b"), crate::ops::TreeStats::ZERO);
        app.jobs
            .sizes
            .insert(VfsPath::local("/other"), crate::ops::TreeStats::ZERO);

        app.request_read(Side::Left, 0, VfsPath::local("/a"));
        assert!(app.jobs.sizes.get(&VfsPath::local("/a")).is_none());
        assert!(
            app.jobs.sizes.get(&VfsPath::local("/a/b")).is_none(),
            "the subtree goes too: a stale figure is never shown"
        );
        assert!(app.jobs.sizes.get(&VfsPath::local("/other")).is_some());
    }

    #[test]
    fn an_event_for_a_forgotten_job_is_dropped_rather_than_resurrecting_it() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let id = app.request_job(JobSpec::size(vec![VfsPath::local("/etc")]));
        app.forget_job(id);
        app.apply_job_event(JobUpdate {
            id,
            event: JobEvent::Started {
                kind: JobKind::Size,
                files_total: 0,
                bytes_total: 0,
            },
        });
        assert!(app.job(id).is_none());
        assert!(app.jobs.rows().is_empty());
    }

    #[test]
    fn backgrounding_a_job_changes_the_view_and_not_the_job() {
        // "Foreground and background are two views of one job."
        //
        // A `Copy`, not a `Size`: the design requires a walk to leave the
        // panel usable, so a `Size` job starts backgrounded and has no
        // foreground view to send away - asserted separately below.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let id = app.request_job(JobSpec::new(
            JobKind::Copy,
            vec![VfsPath::local("/etc/hostname")],
            Some(VfsPath::local("/tmp")),
        ));
        assert!(app.foreground_job_status().is_some());
        assert!(!app.has_background_job());

        app.background_job(id);
        assert!(app.foreground_job_status().is_none(), "the dialog is gone");
        assert!(app.has_background_job(), "the job is not");
        assert!(app.has_running_job());

        app.foreground_job(id);
        assert!(
            app.foreground_job_status().is_some(),
            "and it comes back exactly as it was"
        );
    }

    #[test]
    fn a_size_walk_never_takes_the_screen() {
        // "a slow tree must not freeze the UI". A modal
        // progress dialog over `Ctrl+L` is exactly that freeze, so a `Size`
        // job starts in the background - still listed, still cancellable.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let id = app.request_job(JobSpec::size(vec![VfsPath::local("/etc")]));
        assert!(
            app.foreground_job_status().is_none(),
            "no dialog for a walk"
        );
        assert!(app.has_background_job());
        assert!(app.has_running_job());
        app.sync_job_dialogs();
        assert!(!app.dialog_is_open(), "and syncing does not open one");
        // `Mkdir` is the other one: one syscall per level has no bars.
        let dir = app.request_job(JobSpec::new(
            JobKind::Mkdir,
            Vec::new(),
            Some(VfsPath::local("/tmp/x")),
        ));
        assert!(app.foreground_job_status().is_none());
        assert_ne!(id, dir);
    }
}
