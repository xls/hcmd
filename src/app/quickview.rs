//! One panel showing a file instead of a listing.
//!
//! A panel is showing either its listing or one file, never both. The file it
//! shows is the row the cursor **settled** on: every cursor move arms a
//! deadline and the event loop opens the file only once
//! `panel.quick_view_delay` has passed with the cursor still on it, so holding
//! `Down` through a directory opens nothing.
//!
//! The viewer inside it is a real [`crate::viewer::Viewer`], laid out for the
//! panel body rather than for the screen, which is why the renderer pushes the
//! geometry in every frame whether or not a quick view exists: the size has to
//! be known before the keys waiting behind it are applied.

use std::sync::Arc;

use crate::app::App;
use crate::panel::Side;
use crate::ui::quickview::{DirSummary, Pending, QuickView};
use crate::vfs::{Entry, VfsPath};
use crate::viewer::Viewer;

impl App {
    /// the `Ctrl+Q`: open the quick view in the other panel, or
    /// close it and give the panel its listing back.
    ///
    /// The panel that shows it is fixed when it opens - `active_side.other()`,
    /// "the viewer, in the **other panel**" - and stays where it is when `Tab`
    /// moves the focus into it, because it is still that panel.
    ///
    pub fn quick_view_toggle(&mut self) {
        if self.quick.take().is_some() {
            // Dropping it cancels the background line index through the
            // existing `ScanJob` flag; the panel draws its
            // listing again on the next frame with nothing else to undo.
            return;
        }
        self.quick = Some(Box::new(QuickView::new(self.active_side.other())));
        self.note_quick_view_cursor();
    }

    /// Which panel is showing a quick view, if any.
    ///
    /// **Not `const`, unlike the signature**: the
    /// state is behind a `Box` and a `Box` cannot be dereferenced in a
    /// `const fn` on the pinned toolchain. Every caller is unaffected.
    pub fn quick_view_side(&self) -> Option<Side> {
        self.quick.as_deref().map(|q| q.side)
    }

    /// The open quick viewer.
    pub fn quick_viewer(&self) -> Option<&Viewer> {
        self.quick.as_deref().and_then(|q| q.viewer.as_ref())
    }

    /// The open quick viewer, mutably.
    pub fn quick_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.quick.as_deref_mut().and_then(|q| q.viewer.as_mut())
    }

    /// Arm the debounce for whatever the followed panel's cursor is now on.
    ///
    /// **Replaces the pending file**: there is no queue and no
    /// counter, so held `Down` through two hundred files opens one viewer and
    /// it is the one the cursor came to rest on. Called from every path that
    /// moves a cursor or replaces a listing, which is why it lives here and
    /// not in one key handler.
    ///
    /// Reads the clock and nothing else - no filesystem, no terminal - which
    /// is what makes it safe to call from `dispatch`, exactly as
    /// [`App::command_was_run`] arms the deadline there.
    pub fn note_quick_view_cursor(&mut self) {
        let Some(side) = self.quick_view_side() else {
            return;
        };
        let followed = self.panel(side.other()).active_tab();
        let armed = followed.path_of(followed.cursor).map(|path| Pending {
            path,
            at: std::time::Instant::now(),
            is_dir: followed.current().is_some_and(Entry::is_dir),
        });
        let Some(quick) = self.quick.as_deref_mut() else {
            return;
        };
        match armed {
            // Re-arming the file that is already pending leaves its clock
            // alone. Otherwise a listing that arrives in twenty batches, or a
            // cursor that lands back where it was, would push the deadline
            // forward every time and a quick view over a slow directory would
            // never open at all.
            Some(pending)
                if quick
                    .pending
                    .as_ref()
                    .is_some_and(|held| held.path == pending.path) => {}
            // Nor does it re-open what is already showing: the design reads
            // "one window per file and no more", and a re-read of the panel is
            // not a new file.
            Some(pending) if quick.subject.as_ref() == Some(&pending.path) => {
                quick.pending = None;
            }
            Some(pending) => quick.pending = Some(pending),
            // `..`, or an empty listing: there is nothing under the cursor to
            // show, and leaving the previous file up would say the cursor is
            // somewhere it is not.
            None => {
                quick.pending = None;
                quick.clear();
            }
        }
    }

    /// When [`App::service_quick_view`] next has something to do.
    ///
    /// The event loop's `poll` timeout wakes for it, exactly as it wakes for
    /// [`App::console_switch_deadline`]. **Not `const`, unlike
    /// the signature**: `Instant::checked_add` is
    /// not a `const fn` on the pinned toolchain.
    pub fn quick_view_deadline(&self) -> Option<std::time::Instant> {
        let delay = self.config.viewer.quick_view_delay.duration();
        self.quick
            .as_deref()?
            .pending
            .as_ref()
            .and_then(|pending| pending.at.checked_add(delay))
    }

    /// Open the pending file once it has rested for
    /// `viewer.quick_view_delay`. Reads, so the event loop's.
    ///
    /// Also the frame's upkeep for a quick view that is already open: the
    /// viewer is laid out at the geometry the last frame measured, and a
    /// directory whose size walk has finished picks its figures up out of the
    /// shared [`SizeCache`].
    pub fn service_quick_view(&mut self, now: std::time::Instant) {
        if self.quick.is_none() {
            return;
        }
        self.refresh_quick_summary();
        self.layout_quick_view();

        let delay = self.config.viewer.quick_view_delay.duration();
        let Some(pending) = self.quick.as_deref().and_then(|q| q.pending.clone()) else {
            return;
        };
        if now.saturating_duration_since(pending.at) < delay {
            return;
        }
        // Taken, not left: a file that has been opened is not still pending,
        // and a file that failed to open must not be retried on every frame.
        if let Some(quick) = self.quick.as_deref_mut() {
            quick.pending = None;
        }
        // The cursor came back to what is already showing. Nothing is read.
        if self
            .quick
            .as_deref()
            .is_some_and(|q| q.subject.as_ref() == Some(&pending.path))
        {
            return;
        }
        if pending.is_dir {
            self.open_quick_dir(pending.path);
        } else {
            self.open_quick_file(pending.path);
        }
    }

    /// Tell the quick viewer how big its panel is, before keys are applied.
    ///
    /// Stored rather than applied, because a layout reads the file and this is
    /// called from the renderer's measuring pass;
    /// [`App::service_quick_view`] is what lays it out.
    pub const fn set_quick_view_geometry(&mut self, rows: u16, cols: u16) {
        self.quick_view_geometry = (rows, cols);
    }

    /// A directory under the cursor: its entry count and total size, through
    /// the **same** size walk `Ctrl+L` and `Space` use.
    ///
    /// So a directory already sized by `Space` shows instantly and costs
    /// nothing, and a directory sized by quick view is then free for `Ctrl+L`.
    /// There is no second walk and no second cache in the tree.
    fn open_quick_dir(&mut self, path: VfsPath) {
        let stats = self.jobs.sizes.get(&path);
        if let Some(quick) = self.quick.as_deref_mut() {
            quick.clear();
            quick.subject = Some(path.clone());
            quick.summary = Some(match stats {
                Some(stats) => DirSummary::Done(stats),
                None => DirSummary::Walking,
            });
        }
        if stats.is_none() {
            self.request_size(vec![path]);
        }
    }

    /// A file under the cursor: the ordinary [`Viewer`], opened over one
    /// window.
    ///
    /// The background line index rides out in the viewer's `ScanJob` for the
    /// event loop to spawn, exactly as an `F3` viewer's does. A file that
    /// cannot be read leaves its reason in the body rather than in a dialog: a
    /// modal box per unreadable file while walking a directory is what the
    /// debounce exists to prevent.
    fn open_quick_file(&mut self, path: VfsPath) {
        let id = self.next_viewer_id();
        let opened =
            Viewer::open_path(id, Arc::clone(&self.vfs), path.clone(), &self.config.viewer);
        let ascii = self.config.ui.ascii_borders;
        let (rows, cols) = self.quick_view_geometry;
        let Some(quick) = self.quick.as_deref_mut() else {
            return;
        };
        quick.clear();
        quick.subject = Some(path);
        match opened {
            Ok(mut viewer) => {
                viewer.set_ascii(ascii);
                if rows > 0
                    && cols > 0
                    && let Err(err) = viewer.layout(rows, cols)
                {
                    quick.error = Some(err.to_string());
                }
                quick.viewer = Some(viewer);
            }
            Err(err) => quick.error = Some(err.to_string()),
        }
    }

    /// A directory whose walk has finished picks its figures up (the design's
    /// cache is the only place they live).
    fn refresh_quick_summary(&mut self) {
        let resolved = match self.quick.as_deref() {
            Some(quick) if matches!(quick.summary, Some(DirSummary::Walking)) => quick
                .subject
                .as_ref()
                .and_then(|path| self.jobs.sizes.get(path))
                .map(DirSummary::Done),
            _ => None,
        };
        if let Some(summary) = resolved
            && let Some(quick) = self.quick.as_deref_mut()
        {
            quick.summary = Some(summary);
        }
    }

    /// Lay the quick viewer out at the geometry the last frame measured.
    ///
    /// The twin of [`App::service_viewer`], including why it is the model's
    /// job: `crate::ui::draw` has `&App` and reading the visible window is not
    /// a renderer's business.
    fn layout_quick_view(&mut self) {
        let ascii = self.config.ui.ascii_borders;
        let (rows, cols) = self.quick_view_geometry;
        if rows == 0 || cols == 0 {
            return;
        }
        let mut failed = None;
        if let Some(viewer) = self.quick_viewer_mut() {
            viewer.set_ascii(ascii);
            if let Err(err) = viewer.layout(rows, cols) {
                failed = Some(err.to_string());
            }
        }
        if let Some(err) = failed {
            self.message = Some(err);
        }
    }
}
