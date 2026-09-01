//! Reading a directory, and knowing whose answer is whose.
//!
//! Every directory read carries a **generation**, generations are monotone per
//! tab, and an answer whose generation is not the tab's current one is dropped
//! rather than drawn, so walking quickly through four directories cannot end
//! with the third one's rows under the fourth one's path.
//!
//! The generation answers what is *drawn*. What is *produced* is answered by
//! [`App::register_read`]: a tab has one live read at a time, and starting one
//! aborts the task still streaming the last one. Both halves are needed - a
//! batch already in flight when the tab moved on has to be dropped by
//! generation, and a walk over a million-entry directory that nobody is
//! waiting for any more has to stop rather than run to completion filling a
//! channel whose rows are all discarded.
//!
//! A read is queued and never performed here.
//! [`crate::input::dispatch`] touches no filesystem, so navigating pushes a
//! [`ReadRequest`] and the event loop turns it into a stream of
//! [`VfsEvent`]s.
//!
//! # The thread that draws never blocks on I/O
//!
//! The event loop is also the render thread: it services these queues and then
//! calls `term.terminal().draw()`. So "not in `dispatch`" is only half the
//! rule. Anything that can wait on a disk, an archive or a socket has to
//! happen on the blocking pool and come back through a channel, or a panel
//! pointed at a hung mount takes the whole application down with it - no key
//! read, no frame drawn, no way out.
//!
//! [`Vfs::capabilities_for`] is one of those: on an archive path it opens the
//! archive to learn the format, and on a remote one it can wait on the
//! transport. So the completed listing hands back a [`CapsRequest`] instead of
//! asking, [`probe_capabilities`] answers it off this thread, and the tab
//! carries its backend's conservative answer until the real one arrives -
//! which is the same fill-in-later behaviour the rows themselves have.
//!
//! A re-read keeps the cursor, the marks and the quick-search buffer and
//! replaces the rows only when the new ones arrive, so refreshing costs a
//! directory read and disturbs nothing the user was in the middle of.

use std::path::PathBuf;
use std::sync::Arc;

use crate::app::{App, ReadRequest, VfsEvent, leaving_name};
use crate::panel::Side;
use crate::remote::RemoteId;
use crate::vfs::{BackendKind, Capabilities, Vfs, VfsPath};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// The question "what can be done here", asked of a path off the thread that
/// draws.
///
/// Carries the same `(side, tab, generation)` a [`ReadRequest`] does, because
/// the answer is stale under exactly the same conditions: the tab has since
/// been pointed somewhere else.
#[derive(Debug, Clone)]
pub struct CapsRequest {
    /// Which panel asked.
    pub side: Side,
    /// Which of that panel's tabs.
    pub tab: usize,
    /// The read this answer belongs to.
    pub generation: u64,
    /// The path to ask about.
    pub path: VfsPath,
}

/// A finished [`CapsRequest`], on its way back to the event loop.
#[derive(Debug, Clone, Copy)]
pub struct CapsEvent {
    /// Which panel asked.
    pub side: Side,
    /// Which of that panel's tabs.
    pub tab: usize,
    /// The read this answer belongs to.
    pub generation: u64,
    /// What the backend that really services the path can do.
    pub caps: Capabilities,
}

/// A directory whose git flags should be computed off the event loop.
///
/// Fired when a local listing finishes, when `panel.git_status` is on. The
/// generation pins it to the listing that asked, so a fast `Tab` change cannot
/// paint one directory's flags onto another's rows.
#[derive(Debug, Clone)]
pub struct GitStatusRequest {
    /// Which panel.
    pub side: Side,
    /// Which tab.
    pub tab: usize,
    /// The listing this is for.
    pub generation: u64,
    /// The directory to read git's state of.
    pub dir: std::path::PathBuf,
    /// Whether the rows want per-file flags, or only the branch.
    ///
    /// A commit's listing already knows what it did to each file and says so
    /// on the rows themselves; the working tree's flags describe a different
    /// moment entirely and would overwrite them with nothing. It still wants
    /// the branch, which is a fact about the repository rather than about any
    /// listing of it.
    pub wants_flags: bool,
}

/// What the git probe should look at for `path`, and whether the rows want
/// per-file flags.
///
/// A real local directory wants both: the flags describe the very files on
/// screen. A listing that merely *hangs off* one - a commit and its changed
/// files, which live in the object store rather than the working tree - wants
/// only the branch, because it is still that repository's history and saying
/// so is the point. Anything with no local directory under it at all, a remote
/// or a search result, has no repository to speak of.
fn git_probe_target(path: &crate::vfs::VfsPath) -> Option<(std::path::PathBuf, bool)> {
    if let Some(dir) = path.local_path() {
        return Some((dir.to_path_buf(), true));
    }
    match path.segments().first() {
        Some((crate::vfs::BackendKind::Local, root)) => Some((root.clone(), false)),
        _ => None,
    }
}

/// The answer: each file's git state, by name.
#[derive(Debug, Clone)]
pub struct GitStatusEvent {
    /// Which panel.
    pub side: Side,
    /// Which tab.
    pub tab: usize,
    /// The listing this is for.
    pub generation: u64,
    /// The flags, by file name, or `None` when the listing did not want them.
    pub flags: Option<std::collections::HashMap<String, crate::git::FileState>>,
    /// The branch this directory's repository is on, for the status line.
    pub branch: Option<String>,
}

/// Ask [`Vfs::capabilities_for`] on the blocking pool and deliver the answer
/// to the event loop, the way [`crate::app::stream_read`] delivers a listing.
///
/// The call it makes may open an archive to learn its format, or wait on a
/// remote transport, and neither has any business on the thread that draws.
/// A send that fails means the event loop has gone, and a probe whose thread
/// panicked has no answer to send; both end the task quietly.
pub async fn probe_capabilities(
    vfs: Arc<dyn Vfs>,
    request: CapsRequest,
    tx: mpsc::Sender<CapsEvent>,
) {
    let CapsRequest {
        side,
        tab,
        generation,
        path,
    } = request;
    let Ok(caps) = tokio::task::spawn_blocking(move || vfs.capabilities_for(&path)).await else {
        return;
    };
    let _ = tx
        .send(CapsEvent {
            side,
            tab,
            generation,
            caps,
        })
        .await;
}

impl App {
    /// Point a panel at a path and ask for the listing.
    ///
    /// Marks and the quick-search buffer are cleared, because selection is
    /// cleared on directory change.
    pub fn navigate(&mut self, side: Side, path: VfsPath) {
        self.navigate_selecting(side, path, None);
    }

    /// Navigate, and put the cursor on `select` once the listing produces it.
    ///
    /// Used by "go to parent", which passes the name of the directory being
    /// left so the cursor lands on it instead of at the top - see
    /// [`crate::panel::Tab::pending_select`]. A name that never arrives is
    /// abandoned when the read completes and the cursor stays at 0.
    pub fn navigate_selecting(&mut self, side: Side, path: VfsPath, select: Option<String>) {
        let generation = self.next_generation();
        let panel = self.panel_mut(side);
        panel.quick.clear();
        let tab_index = panel.active_index();
        let tab = panel.active_tab_mut();
        // `display_title` has no name for the root of an archive segment -
        // the tail is `/` - and the honest label there is the archive's own
        // file name (the panel path reads `.../foo.zip#/`).
        tab.title = leaving_name(&path).unwrap_or_else(|| path.to_string());
        tab.path = path.clone();
        // The previous directory's answer describes the previous directory.
        // Refreshed below, once the borrow of the tab has ended, from the one
        // place a capability answer lives.
        tab.clear_entries();
        tab.marks.clear();
        // A quick-search filter belongs to the directory it was typed in; a new
        // directory drops it. A re-read of the *same* directory keeps it, which
        // is why this lives here and not in `clear_entries`.
        tab.clear_filter();
        // The same rule, for the same reason: whether this is a repository is a
        // fact about the directory, and it is answered by a probe that lands
        // after the listing does. Clearing it on every finished read instead
        // meant the column blinked out and back on each rescan - and the watch
        // rescans often.
        tab.git_branch = None;
        // A different directory has nothing to reconcile against, so any rescan
        // that was mid-flight is abandoned rather than merged into the new one.
        tab.merging = None;
        tab.cursor = 0;
        tab.scroll = 0;
        tab.loading = true;
        tab.generation = generation;
        tab.pending_select = select;
        // "Entering a directory from within a virtual listing
        // also leaves it, since the panel then has a real path." Every route
        // into a new directory passes through here, so leaving is a property
        // of navigating rather than of the routes somebody remembered - and
        // the listing is forgotten, which cancels the walk still filling it.
        //
        // the invariant, kept by the one function every route into
        // a new directory passes through: a tab is connected exactly while its
        // path is on that connection (the design I8). Navigating
        // off a connection - `Ctrl+G`, a hotlist, the other panel's directory -
        // therefore leaves it, and leaving closes it, because nothing else
        // will - unless a job is still using it, which is the exception below.
        let leaving_remote = match (tab.remote_view.as_deref(), RemoteId::from_path(&path)) {
            (Some(view), Some(id)) if view.id == id => None,
            (Some(_), _) => tab.remote_view.take(),
            (None, _) => None,
        };
        let leaving = tab.virtual_view.take();
        if let Some(view) = leaving {
            self.router.forget_listing(view.listing);
        }
        if let Some(view) = leaving_remote {
            // and the same rule `App::close_tab` keeps: a
            // connection with a job on it is **not** closed under the job. The
            // tab still leaves - a tab is connected exactly while its path is
            // on that connection (I8) - but the transport stays until the
            // operation that is using it has finished, and the status line
            // says so. Closing it here made every remaining file of the batch
            // fail with the same "that connection has been closed", which is
            // the failure summary `ops::is_fatal` exists to prevent.
            if self.job_on(view.id).is_some() {
                self.message = Some(format!(
                    "{} stays connected until the running operation finishes",
                    view.authority
                ));
            } else {
                self.router.remotes().close(view.id);
            }
        }
        self.jobs.sizes.invalidate(&path);
        self.refresh_caps(side, tab_index);
        self.forget_container_attempt(side, tab_index);
        self.cancel_read(side, tab_index);
        self.pending_reads.push(ReadRequest {
            side,
            tab: tab_index,
            generation,
            path,
        });
        // the panel → shell half. Here rather than at each call
        // site, so every route that *reads a new directory* - `Enter`,
        // `Backspace`, `Ctrl+G`, the other panel's `Ctrl+Right` - keeps the
        // shell in step, instead of the ones somebody remembered. The routes
        // that change which directory is active **without** reading one - a
        // tab switch, the other panel, `Ctrl+U` - go through
        // [`App::sync_active_cwd`] instead.
        self.sync_shell_cwd(side);
    }

    /// The real directories the panels are showing, for the filesystem watch
    /// to point at.
    ///
    /// Only local paths: a change under `/home` arrives from inotify, but there
    /// is nothing to watch on an S3 bucket or an SFTP host, and an archive path
    /// is a file the panel reads, not a directory the kernel reports on. The
    /// two panels may be in the same directory, so the caller deduplicates.
    pub fn watch_targets(&self) -> Vec<PathBuf> {
        [Side::Left, Side::Right]
            .into_iter()
            .filter_map(|side| {
                self.panel(side)
                    .active_tab()
                    .path
                    .local_path()
                    .map(std::path::Path::to_path_buf)
            })
            .collect()
    }

    /// Re-read whichever panel is showing one of `changed`, after the
    /// filesystem watch reported activity there.
    ///
    /// A watch is set on a directory and its events name the entries inside it,
    /// so a panel showing directory `d` is stale when a changed path is `d`
    /// itself or a direct child of it. Matched per side rather than rereading
    /// both, so a change under the left panel does not re-walk the right.
    pub fn reread_changed(&mut self, changed: &[PathBuf]) {
        for side in [Side::Left, Side::Right] {
            let Some(dir) = self
                .panel(side)
                .active_tab()
                .path
                .local_path()
                .map(std::path::Path::to_path_buf)
            else {
                continue;
            };
            let touched = changed
                .iter()
                .any(|path| path == &dir || path.parent() == Some(dir.as_path()));
            if touched {
                self.reread(side);
            }
        }
    }

    /// `F2` / `Ctrl+R`: re-read a panel's active tab **in place**.
    ///
    /// Deliberately not [`App::navigate`] to the same path. the design makes
    /// selection "preserved across re-reads where the path still exists,
    /// cleared on directory change", and a re-read is not a directory change -
    /// so the marks, the cursor and the quick-search buffer all survive it.
    /// Marks whose entry is genuinely gone are dropped by
    /// [`Tab::prune_marks`] when the read completes, which is the mechanism
    /// that clause describes.
    pub fn reread(&mut self, side: Side) {
        let panel = self.panel_mut(side);
        let tab_index = panel.active_index();
        let tab = panel.active_tab_mut();
        let path = tab.path.clone();
        // "`Ctrl+R` on a normal panel refreshes it, the same as
        // `F2`", and the design keeps the cursor across a re-read. Neither
        // holds on its own here. The replacement batch arrives in the
        // backend's `readdir` order and `Tab::sort_entries` re-anchors the
        // cursor by reading the name at its *index* first - which, once the
        // vector underneath has been swapped, is whatever row happened to land
        // there. So the name is captured here, before the read, and restored
        // by the `pending_select` machinery that "go to parent" already uses:
        // it survives a listing that arrives in several batches, and a name
        // that is genuinely gone is abandoned when the read completes.
        //
        // Only when nothing is pending. A re-read is what the routes that
        // *create* a row wait for - `F7`, `F2`, `F4`, a paste - and each of
        // them has already said which row the cursor belongs on. Their answer
        // is about the listing that is coming; the cursor's current name is
        // about the one being replaced, and the two disagree precisely when
        // the caller knew better.
        if tab.pending_select.is_none() {
            tab.pending_select = tab.cursor_name();
        }
        // A rescan updates the listing in place instead of rebuilding it, so
        // nothing on screen blanks: the rows stay, their computed columns stay,
        // and the read reconciles into them when it completes. Only a plain
        // local directory qualifies - a virtual listing is flat and can hold
        // two rows of one name from different directories, and a remote one has
        // no stable in-place identity to reconcile against, so both keep the
        // old clear-and-replace.
        let mergeable =
            tab.virtual_view.is_none() && tab.remote_view.is_none() && path.local_path().is_some();
        if mergeable {
            tab.merging = Some(Vec::new());
            // A merge keeps the rows and their sizes on screen, so neither the
            // "reading…" state nor a size-cache flush belongs on this path.
            self.request_read_inner(side, tab_index, path, true);
        } else {
            // The old rows stay on screen until the replacement arrives;
            // clearing here would flash bare background after every copy.
            tab.replace_on_next_batch = true;
            self.request_read(side, tab_index, path);
        }
    }

    /// Ask for a listing without disturbing the tab (used by the event loop on
    /// a resize or an explicit refresh).
    ///
    /// Re-reading a path **invalidates its cached size and every size beneath
    /// it** ("invalidated when the panel re-reads it"). Doing
    /// it here rather than at each call site is what makes the guarantee hold
    /// for every route into a re-read - `F2`, `Ctrl+R`, `show_hidden`, the
    /// event loop's own refresh after a job - rather than for the ones someone
    /// remembered.
    pub fn request_read(&mut self, side: Side, tab_index: usize, path: VfsPath) -> u64 {
        self.request_read_inner(side, tab_index, path, false)
    }

    /// [`App::request_read`], told whether to keep the rows and their cached
    /// sizes on screen (`keep`) or to clear as an ordinary read does.
    ///
    /// A rescan (a same-directory re-read that reconciles in place) keeps them:
    /// the listing does not blank, so there is no "reading…" to show and the
    /// walked-folder sizes stay put rather than being flushed and left blank
    /// until the user asks for them again. Every other read clears, which is
    /// what the size-invalidation guarantee ("a re-read invalidates the cached
    /// size of that tree") is about.
    fn request_read_inner(
        &mut self,
        side: Side,
        tab_index: usize,
        path: VfsPath,
        keep: bool,
    ) -> u64 {
        if !keep {
            self.jobs.sizes.invalidate(&path);
        }
        let generation = self.next_generation();
        if let Some(tab) = self.panel_mut(side).tab_mut(tab_index) {
            if !keep {
                tab.loading = true;
            }
            tab.generation = generation;
        }
        self.forget_container_attempt(side, tab_index);
        self.cancel_read(side, tab_index);
        self.pending_reads.push(ReadRequest {
            side,
            tab: tab_index,
            generation,
            path,
        });
        generation
    }

    /// Bring a tab's cached capability answer back into step with the one
    /// place that answer lives.
    ///
    /// **The only thing that writes [`crate::panel::Tab::caps`].** The field
    /// used to be written from four places that could disagree - tab
    /// construction, a listing finishing, connecting, disconnecting and
    /// starting a search - and which of them had run last decided whether a
    /// key worked. It is now a memo of
    /// [`crate::vfs::VfsRouter::known_capabilities`] and nothing else, so
    /// there is one place to be wrong.
    ///
    /// It cannot block, which is why every route that changes what a tab is
    /// pointed at can call it: the router answers from what reading the
    /// directory already learned, and falls back to the conservative
    /// path-free answer for a path nothing has resolved yet.
    pub fn refresh_caps(&mut self, side: Side, tab_index: usize) {
        let Some(path) = self.panel(side).tab(tab_index).map(|tab| tab.path.clone()) else {
            return;
        };
        let caps = self.router.known_capabilities(&path);
        if let Some(tab) = self.panel_mut(side).tab_mut(tab_index) {
            tab.caps = caps;
        }
    }

    /// Remember the task streaming a tab's listing, so the next read of that
    /// tab can stop it.
    ///
    /// Called by the event loop the moment it spawns
    /// [`crate::app::stream_read`], which is the only place a read is started.
    /// A handle already held for that tab is aborted here as well as in
    /// [`App::cancel_read`], so a route that starts a read without going
    /// through the two functions above still cannot leave two walks on one
    /// tab.
    pub fn register_read(&mut self, side: Side, tab_index: usize, task: AbortHandle) {
        if let Some(previous) = self.read_tasks.insert((side, tab_index), task) {
            previous.abort();
        }
    }

    /// Stop the read still filling this tab, because a new one supersedes it.
    ///
    /// Aborting drops the `stream_read` future and with it the receiver
    /// [`crate::vfs::Vfs::read_dir`] handed it; the producing task's next
    /// `send` then fails and the walk stops, which is the cancellation
    /// [`crate::vfs`] documents and nothing was using. Without it a walk over a
    /// large directory ran to completion after the panel had left it, sending
    /// batches that [`App::apply_vfs_event`] discarded one generation check at
    /// a time - and five directories in a row meant five live walks.
    ///
    /// A handle whose task has already finished is kept until the next read of
    /// that tab replaces it; aborting a finished task does nothing, so the map
    /// stays the size of the number of tabs rather than of the session.
    fn cancel_read(&mut self, side: Side, tab_index: usize) {
        if let Some(task) = self.read_tasks.remove(&(side, tab_index)) {
            task.abort();
        }
    }

    /// Drop the container attempt a tab was carrying, because a new read has
    /// superseded it.
    ///
    /// A tab has one live read at a time, so an attempt left over from an
    /// earlier one can never be answered and would only sit in the map.
    /// Called from both routes into a read, which is what keeps the map the
    /// size of the number of tabs rather than of the session.
    fn forget_container_attempt(&mut self, side: Side, tab_index: usize) {
        self.container_attempts
            .retain(|_, a| a.side != side || a.tab != tab_index);
    }

    pub(super) fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    /// Drain the queued directory reads. The event loop calls this once a
    /// frame and spawns each one.
    pub fn take_pending_reads(&mut self) -> Vec<ReadRequest> {
        std::mem::take(&mut self.pending_reads)
    }

    /// Apply a result from a running read, and hand back the reading it still
    /// needs. Stale generations are dropped, which is how a superseded listing
    /// cancels itself.
    ///
    /// The returned [`CapsRequest`] is the one thing a completed listing
    /// cannot answer where it stands: [`Vfs::capabilities_for`] blocks, and
    /// this runs on the thread that draws. The event loop spawns
    /// [`probe_capabilities`] with it and feeds the answer back through
    /// [`App::apply_caps_event`].
    pub fn apply_read_event(&mut self, event: VfsEvent) -> Option<CapsRequest> {
        let (side, tab_index, generation) = match &event {
            VfsEvent::Entries {
                side,
                tab,
                generation,
                ..
            }
            | VfsEvent::Done {
                side,
                tab,
                generation,
            }
            | VfsEvent::Failed {
                side,
                tab,
                generation,
                ..
            } => (*side, *tab, *generation),
        };

        let show_hidden = self.config.panel.show_hidden;
        let directories_first = self.config.panel.directories_first;

        let tab = self.panel_mut(side).tab_mut(tab_index)?;
        if tab.generation != generation {
            return None;
        }

        // Set by the `Done` arm alone, and returned rather than answered: the
        // answer blocks and this is the thread that draws.
        let mut caps_request = None;

        match event {
            VfsEvent::Entries { batch, .. } => {
                // A row of the listing's own arrived, so this really was a
                // directory: the attempt is answered and there is nothing to
                // go back to. The `..` row alone does not answer it - every
                // backend sends that first, whether or not it could read
                // anything.
                if batch.iter().any(|e| !e.is_parent) {
                    self.container_attempts.remove(&generation);
                }
                let tab = self.panel_mut(side).tab_mut(tab_index)?;
                // A rescan collects its rows off to the side and leaves the
                // visible listing - rows, cursor, git flags, sizes - untouched
                // until `Done` reconciles them in [`Tab::merge_listing`]. The
                // `..` row is kept, and the same hidden-file rule applies, so
                // the buffer is the whole listing the merge reconciles against;
                // a row it does not contain is one that left the directory.
                if let Some(buffer) = tab.merging.as_mut() {
                    buffer.extend(batch.into_iter().filter(|e| show_hidden || !e.is_hidden));
                    return None;
                }
                // The first batch of a re-read replaces what is on screen; the
                // rest append to it.
                if std::mem::take(&mut tab.replace_on_next_batch) {
                    tab.clear_entries();
                    // With the rows gone the cursor's index means nothing, and
                    // `Tab::sort_entries` re-anchors by reading the name at
                    // that index. Left alone it would anchor onto an unrelated
                    // row of the incoming, still unsorted batch and the cursor
                    // would land somewhere different on every refresh.
                    // `Tab::pending_select`, set by `App::reread`, is what puts
                    // it back on the row it was on.
                    tab.cursor = 0;
                }
                // The `..` row arrives from the backend, which is the only
                // thing that knows where its own namespace ends - `LocalFs`
                // sends it first and omits it at the root, and a flat `ListFs`
                // listing has none at all. It is never synthesised here; doing
                // both is how a directory ends up with two of them.
                tab.entries
                    .extend(batch.into_iter().filter(|e| show_hidden || !e.is_hidden));
                // Not `sort_entries`: sorting the whole vector on every 128-row
                // batch is quadratic, and the virtual listings put
                // no bound on the row count. `sort_streaming` keeps a directory
                // sorted batch by batch and puts a large listing on a doubling
                // schedule; the `Done` arm below sorts once more, so the final
                // order is the same either way.
                tab.sort_streaming(directories_first);
                // Sort first: the cursor is an index into the sorted rows, so
                // resolving before the sort would point at the wrong entry.
                tab.resolve_pending_select();
            }
            VfsEvent::Done { .. } => {
                self.container_attempts.remove(&generation);
                // "`Capabilities` is what the UI consults before
                // offering an operation ... rather than failing halfway
                // through a copy." The answer costs a filesystem touch - it
                // opens the archive to learn which of the formats
                // it is - so it is taken here, on the event loop, at the one
                // moment the panel has just learned what it is looking at.
                // `dispatch` then reads it without touching anything,
                // which is what lets `F7` and
                // `Shift+F8` refuse *before* the prompt instead of after it.
                //
                // Read through, not asked: reading the directory is what
                // opened the backend that knows, and `Vfs::read_dir` recorded
                // what it said on the way past. So the answer that used to
                // cost an archive open on the thread that draws is now a hash
                // lookup, and the tab is in step the instant the listing is.
                self.refresh_caps(side, tab_index);
                // Handed back as well, for the path the router could not
                // resolve while it was streaming - a listing that came from a
                // cache, a backend that has since been reopened. The probe
                // costs a blocking-pool task and answers into the same one
                // place, so a cold path converges rather than staying
                // conservative until the next read.
                caps_request = self.panel(side).tab(tab_index).map(|tab| CapsRequest {
                    side,
                    tab: tab_index,
                    generation,
                    path: tab.path.clone(),
                });
                // A local listing in a repository also gets its git flags,
                // computed off the loop and merged when they arrive. Only for
                // a real local directory: a virtual listing or a remote has no
                // working tree to read git's state from.
                self.pending_git_status = self
                    .panel(side)
                    .tab(tab_index)
                    .filter(|_| self.config.panel.git_status)
                    .and_then(|tab| git_probe_target(&tab.path))
                    .map(|(dir, wants_flags)| GitStatusRequest {
                        side,
                        tab: tab_index,
                        generation,
                        dir,
                        wants_flags,
                    });
                let tab = self.panel_mut(side).tab_mut(tab_index)?;
                tab.loading = false;
                // A rescan reconciles its buffered rows into the visible ones
                // now, in place; an ordinary read has already built `entries`
                // and only needs the final sort. Either way the order is the
                // same as the incremental one the batches kept.
                if tab.merging.is_some() {
                    tab.merge_listing(directories_first);
                } else {
                    tab.sort_entries(directories_first);
                }
                tab.resolve_pending_select();
                // The listing is complete, so a name that has not turned up is
                // not going to. Drop the request rather than letting it fire
                // against a later, unrelated read of the same tab.
                tab.pending_select = None;
                // a mark survives a re-read where its path still
                // exists, and only then. The listing is complete here, so this
                // is the one point at which "still exists" can be decided.
                tab.prune_marks();
                // The merge and `resolve_pending_select` both place the cursor
                // by index and by name, and neither asks the filter: a rescan
                // that deleted the cursor's file can leave the index on a row
                // the filter hides - drawn nowhere, yet still what `Enter` and
                // `F8` act on. The clamp snaps it onto a shown row, by the
                // same rule a key-driven move uses.
                tab.clamp_cursor();
            }
            VfsEvent::Failed { message, .. } => {
                // A re-read that failed outright - a rescan or an ordinary one -
                // drops the rows it was refreshing, buffered half and all: they
                // describe a directory we can no longer read, and leaving them
                // up would have them sit there looking current. The rescan's
                // in-place update only holds while the read is succeeding.
                let was_rescan = tab.merging.take().is_some();
                if was_rescan || std::mem::take(&mut tab.replace_on_next_batch) {
                    tab.clear_entries();
                }
                tab.loading = false;
                let listed_nothing = tab.entries.iter().all(|e| e.is_parent);
                // A failure refreshes the answer too. Nothing did before, so a
                // tab whose read failed kept whatever it had been given last -
                // and because the only thing that upgraded it was a listing
                // finishing, a directory that failed once stayed pessimistic
                // for the rest of the session however many times it was read
                // again afterwards.
                self.refresh_caps(side, tab_index);

                // the design detects an archive by its content, and the
                // content is only read here. A `.zip` that is really HTML
                // fails without a single member, and the panel goes back where
                // it was rather than sitting in an empty listing of something
                // that is not a directory (the rule that an
                // unreadable place is never rendered as an empty one).
                //
                // *Without a single member* is the distinction that matters.
                // The `..` row does not count - every backend sends it whether
                // or not it could read anything. But an archive that listed
                // real entries and then failed keeps its panel: those rows are
                // true, and `..` is the way out of them.
                //
                // The one exception is `Ctrl+PgDn`'s retry: an archive attempt
                // that key made and that listed nothing is entered again as a
                // disk image before the panel is put back, so a `backup.dat`
                // holding a partition table opens on the same key that opens
                // one holding a zip. The retry is owed once, it is owed only
                // by an `Archive` attempt, and the attempt it makes is owed
                // none.
                match self.container_attempts.remove(&generation) {
                    Some(attempt)
                        if listed_nothing
                            && attempt.retry
                            && attempt.tried == BackendKind::Archive =>
                    {
                        self.retry_as_image(attempt);
                    }
                    Some(attempt) if listed_nothing => {
                        let name = attempt.name.clone();
                        self.navigate_selecting(attempt.side, attempt.from, Some(attempt.name));
                        self.message = Some(format!("{name}: {message}"));
                    }
                    _ => self.message = Some(message),
                }
            }
        }
        // the quick view follows the cursor, and a listing that
        // arrives, is sorted, or fails has moved what the cursor is on without
        // any key being pressed. Idempotent for a cursor that is still on the
        // same row, so a listing arriving in twenty batches costs twenty
        // comparisons and no reads.
        self.note_quick_view_cursor();
        caps_request
    }
    /// Take the queued git-status request, for the event loop.
    pub fn take_pending_git_status(&mut self) -> Option<GitStatusRequest> {
        self.pending_git_status.take()
    }

    /// Merge a directory's git flags into the listing that asked for them.
    ///
    /// The generation guards it: flags computed for a listing the panel has
    /// since replaced are dropped, so one directory's state never paints
    /// another's rows.
    pub fn apply_git_status_event(&mut self, event: GitStatusEvent) {
        let Some(tab) = self.panel_mut(event.side).tab_mut(event.tab) else {
            return;
        };
        if tab.generation != event.generation {
            return;
        }
        tab.git_branch = event.branch;
        let Some(flags) = event.flags else {
            return;
        };
        for entry in &mut tab.entries {
            entry.git_state = flags.get(&entry.name).copied();
        }
    }

    /// [`App::apply_read_event`] for a caller with nowhere to send the probe.
    ///
    /// A test drives a panel through its listing with no event loop under it,
    /// so there is no blocking pool to hand the capability question to and no
    /// channel to hear the answer on. The tab keeps its backend's own
    /// conservative answer, which is what it had before the listing arrived.
    pub fn apply_vfs_event(&mut self, event: VfsEvent) {
        let _ = self.apply_read_event(event);
    }

    /// Fold a finished [`CapsRequest`] into the tab that asked.
    ///
    /// The generation check is the [`VfsEvent`] one, for the same reason: a
    /// tab that has been pointed somewhere else since is described by a
    /// different answer, and this one is dropped rather than drawn.
    pub fn apply_caps_event(&mut self, event: CapsEvent) {
        let Some(tab) = self.panel(event.side).tab(event.tab) else {
            return;
        };
        if tab.generation != event.generation {
            return;
        }
        // Into the one place the answer lives, and then read back out of it,
        // rather than straight onto the tab: everything else that gates a key
        // asks the router, and an answer written only onto the tab would be a
        // second copy that the two could disagree about - which is the whole
        // of what was wrong here.
        let path = tab.path.clone();
        self.router.capability_cache().remember(&path, event.caps);
        self.refresh_caps(event.side, event.tab);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn a_commit_listing_asks_for_the_branch_but_not_the_working_trees_flags() {
        use crate::vfs::{BackendKind, VfsPath};
        // A real directory wants both: the flags are about the files on screen.
        let here = VfsPath::local("/repo/src");
        assert_eq!(
            git_probe_target(&here),
            Some((std::path::PathBuf::from("/repo/src"), true))
        );
        // History hangs off the repository. It is still that repository, so it
        // is named - but the working tree's flags describe another moment and
        // would wipe what the commit itself said about each file.
        let history = VfsPath::local("/repo").with_segment(BackendKind::Git, "/abc123");
        assert_eq!(
            git_probe_target(&history),
            Some((std::path::PathBuf::from("/repo"), false))
        );
        // Nothing local underneath: no repository to speak of.
        let listing = VfsPath::new(BackendKind::List, "/results");
        assert_eq!(git_probe_target(&listing), None);
    }
    use crate::app::{VfsEvent, stream_read};
    use crate::config::{Config, Keymap, Theme};
    use crate::error::Result as VfsResult;
    use crate::vfs::{Capabilities, Entry, unsupported};

    /// How long a test waits for something that a working cancellation makes
    /// happen at once. Long enough that a loaded machine is not the reason a
    /// run goes red.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// How many rows the synthetic directory has. Far more than any test can
    /// consume, so a walk that reaches the end is a walk nothing stopped.
    const TREE_ROWS: usize = 1_000_000;

    /// A directory with a million rows in it, and a walk that reports how far
    /// it got.
    #[derive(Debug, Default)]
    struct BigTree {
        /// How many rows the walk handed over.
        sent: Arc<AtomicUsize>,
        /// Set when the walk stopped because nobody was listening any more,
        /// which is the only thing that can stop it.
        stopped: Arc<AtomicBool>,
    }

    impl Vfs for BigTree {
        fn kind(&self) -> BackendKind {
            BackendKind::Local
        }

        fn read_dir(&self, _path: &VfsPath) -> mpsc::Receiver<VfsResult<Entry>> {
            let (tx, rx) = mpsc::channel(16);
            let sent = Arc::clone(&self.sent);
            let stopped = Arc::clone(&self.stopped);
            tokio::spawn(async move {
                for row in 0..TREE_ROWS {
                    if tx.send(Ok(Entry::file(format!("row{row}")))).await.is_err() {
                        // The receiver has gone, which is the cancellation
                        // `crate::vfs` documents and the whole point of this
                        // fixture.
                        stopped.store(true, Ordering::SeqCst);
                        return;
                    }
                    sent.fetch_add(1, Ordering::SeqCst);
                }
            });
            rx
        }

        fn stat(&self, path: &VfsPath) -> VfsResult<Entry> {
            Ok(Entry::file(path.file_name().unwrap_or_default()))
        }

        fn open_read(&self, _path: &VfsPath) -> VfsResult<Box<dyn std::io::Read + Send>> {
            unsupported("read")
        }

        fn open_write(&self, _path: &VfsPath) -> VfsResult<Box<dyn std::io::Write + Send>> {
            unsupported("write")
        }

        fn create_dir(&self, _path: &VfsPath) -> VfsResult<()> {
            unsupported("mkdir")
        }

        fn remove(&self, _path: &VfsPath) -> VfsResult<()> {
            unsupported("remove")
        }

        fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> VfsResult<()> {
            unsupported("rename")
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::LOCAL
        }
    }

    /// Start the reads a frame queued exactly as the event loop does: spawn
    /// each one and hand its handle to the tab it is filling.
    fn service_reads(app: &mut App, tx: &mpsc::Sender<VfsEvent>) {
        for request in app.take_pending_reads() {
            let (side, tab) = (request.side, request.tab);
            let task = tokio::spawn(stream_read(Arc::clone(&app.vfs), request, tx.clone()));
            app.register_read(side, tab, task.abort_handle());
        }
    }

    /// Navigating away stops the walk that was filling the tab.
    ///
    /// The generation check alone left it running: every batch it went on
    /// producing was discarded on arrival, at the cost of the whole walk. Here
    /// nothing drains the `VfsEvent` channel after the first batch, which is
    /// the real shape of a superseded read - the rows have nowhere to go - so
    /// a walk that is not stopped stays parked on a full channel forever and
    /// `stopped` is never set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn navigating_away_stops_the_walk_that_was_filling_the_tab() {
        let tree = Arc::new(BigTree::default());
        let sent = Arc::clone(&tree.sent);
        let stopped = Arc::clone(&tree.stopped);

        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.vfs = tree as Arc<dyn Vfs>;

        app.navigate(Side::Left, VfsPath::local("/big"));
        let (tx, mut rx) = mpsc::channel::<VfsEvent>(8);
        service_reads(&mut app, &tx);

        // The walk is genuinely under way before the panel leaves it.
        let first = tokio::time::timeout(PATIENCE, rx.recv())
            .await
            .expect("the first batch of a million-row directory")
            .expect("the reader is still going");
        assert!(matches!(first, VfsEvent::Entries { .. }));

        app.navigate(Side::Left, VfsPath::local("/elsewhere"));

        let stopped_early = tokio::time::timeout(PATIENCE, async {
            while !stopped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            stopped_early.is_ok(),
            "the walk was still running after the tab left it"
        );
        assert!(
            sent.load(Ordering::SeqCst) < TREE_ROWS,
            "the walk ran to completion instead of stopping"
        );
    }

    /// The same cancellation on the other route into a read: `F2` / `Ctrl+R`
    /// re-reading a tab that is still filling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_re_read_stops_the_walk_it_replaces() {
        let tree = Arc::new(BigTree::default());
        let stopped = Arc::clone(&tree.stopped);

        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.vfs = tree as Arc<dyn Vfs>;

        app.navigate(Side::Left, VfsPath::local("/big"));
        let (tx, mut rx) = mpsc::channel::<VfsEvent>(8);
        service_reads(&mut app, &tx);
        let first = tokio::time::timeout(PATIENCE, rx.recv())
            .await
            .expect("the first batch of a million-row directory");
        assert!(first.is_some());

        app.request_read(Side::Left, 0, VfsPath::local("/big"));

        let stopped_early = tokio::time::timeout(PATIENCE, async {
            while !stopped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            stopped_early.is_ok(),
            "the superseded walk was still running after the re-read"
        );
    }

    /// **A completed listing must not ask the filesystem what it can do.**
    ///
    /// `Vfs::capabilities_for` opens an archive to learn its format and waits
    /// on a remote transport, and the thread that folds a listing in is the
    /// thread that draws. So the question comes back as a request rather than
    /// an answer, and the tab keeps what its backend promises on its own until
    /// the probe reports.
    #[test]
    fn a_finished_listing_asks_for_its_capabilities_rather_than_taking_them() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.navigate(Side::Left, VfsPath::local("/etc"));
        let generation = app.left.active_tab().generation;
        let before = app.left.active_tab().caps;

        let request = app
            .apply_read_event(VfsEvent::Done {
                side: Side::Left,
                tab: 0,
                generation,
            })
            .expect("the reading is handed back");
        assert_eq!(request.generation, generation);
        assert_eq!(request.path, VfsPath::local("/etc"));
        assert_eq!(
            app.left.active_tab().caps,
            before,
            "nothing was asked, so nothing changed"
        );

        app.apply_caps_event(CapsEvent {
            side: Side::Left,
            tab: 0,
            generation,
            caps: Capabilities::ARCHIVE_UNKNOWN,
        });
        assert_eq!(
            app.left.active_tab().caps,
            Capabilities::ARCHIVE_UNKNOWN,
            "and the answer lands when it arrives"
        );
    }

    /// **A read that failed refreshes the tab's answer too.**
    ///
    /// Nothing did before: only a listing *finishing* wrote the field, so a
    /// tab whose read failed kept whatever it had been handed last, for the
    /// rest of the session and however many times it was read again. Here the
    /// cache holds the real answer and the tab holds a pessimistic one, which
    /// is exactly the shape a failed read used to leave behind.
    #[test]
    fn a_failed_read_brings_the_tabs_answer_back_into_step() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.navigate(Side::Left, VfsPath::local("/etc"));
        let generation = app.left.active_tab().generation;

        let path = app.left.active_tab().path.clone();
        app.router
            .capability_cache()
            .remember(&path, Capabilities::LOCAL);
        app.left.active_tab_mut().caps = Capabilities::ARCHIVE_UNKNOWN;

        app.apply_vfs_event(VfsEvent::Failed {
            side: Side::Left,
            tab: 0,
            generation,
            message: "no".to_string(),
        });

        assert_eq!(
            app.left.active_tab().caps,
            Capabilities::LOCAL,
            "a failed read left a stale answer on the tab"
        );
    }

    /// An answer for a read the tab has moved on from is dropped, which is the
    /// generation check a listing batch already gets.
    #[test]
    fn a_capability_answer_from_a_superseded_read_is_dropped() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.navigate(Side::Left, VfsPath::local("/etc"));
        let stale = app.left.active_tab().generation;
        app.navigate(Side::Left, VfsPath::local("/var"));
        let now = app.left.active_tab().caps;

        app.apply_caps_event(CapsEvent {
            side: Side::Left,
            tab: 0,
            generation: stale,
            caps: Capabilities::ARCHIVE_UNKNOWN,
        });
        assert_eq!(
            app.left.active_tab().caps,
            now,
            "the tab is somewhere else and this answer describes where it was"
        );
    }
}
