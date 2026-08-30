//! The open viewers, and the focus the stack displaced.

use crate::app::ViewRequest;
use crate::config::ViewerMode;
use crate::input::Focus;
use crate::viewer::find::FindQuery;
use crate::viewer::{Viewer, ViewerId};

/// Viewers form a stack with monotone ids.
///
/// Closing one uncovers the one beneath it; closing the last returns focus to
/// what held it **before the first was opened**, which is not the same as
/// before the last was opened. `F1` inside a viewer opens the viewer's own
/// help page and the design makes that page another viewer over generated
/// text, which is why this is a stack rather than an `Option`.
///
/// The mode and the find query outlive any individual viewer, because they are
/// what the next one is opened with: the design remembers the mode "per
/// session", and the session is the application's rather than the file's.
///
/// The geometry is here because it is the size the top of the stack was last
/// laid out at. It has to be kept rather than measured on demand: a viewer
/// pushed between two frames has keys waiting behind it, and a viewer that has
/// never been laid out believes the screen is zero rows high, so a held `PgDn`
/// would page by one row.
#[derive(Debug)]
pub struct Stack {
    open: Vec<Viewer>,
    restore: Focus,
    view: (u16, u16),
    pending: Option<ViewRequest>,
    next: u64,
    /// the remembered mode. `None` until one has actually been
    /// chosen, so an untouched session still opens on `viewer.default_mode`
    /// and a binary file still overrides both.
    pub mode: Option<ViewerMode>,
    /// The last `F7` query, so `F3` on a hit can walk the matches inside the
    /// file without retyping what went into the dialog a moment ago.
    ///
    /// Session state deliberately: `searches.toml` is the place for a search
    /// worth keeping, and this one is not written to disk.
    pub last_find: Option<FindQuery>,
}

impl Default for Stack {
    /// Empty, with focus recorded as the left panel, which is where a session
    /// starts.
    fn default() -> Self {
        Self {
            open: Vec::new(),
            restore: Focus::Panel(crate::panel::Side::Left),
            view: (0, 0),
            pending: None,
            next: 0,
            mode: None,
            last_find: None,
        }
    }
}

impl Stack {
    /// Is anything open?
    pub fn is_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// How deep the stack is. `F1` over a viewer makes it two.
    pub fn depth(&self) -> usize {
        self.open.len()
    }

    /// The innermost viewer, which is the one on screen.
    pub fn top(&self) -> Option<&Viewer> {
        self.open.last()
    }

    /// The innermost viewer, mutably.
    pub fn top_mut(&mut self) -> Option<&mut Viewer> {
        self.open.last_mut()
    }

    /// The next id, which is never one that has been handed out before.
    ///
    /// Monotone so that a batch from a scan whose viewer has been closed is
    /// dropped rather than applied to its replacement.
    pub fn next_id(&mut self) -> ViewerId {
        self.next = self.next.saturating_add(1);
        ViewerId(self.next)
    }

    /// Push a viewer, recording where focus came from if this is the first.
    ///
    /// Only the first push records: closing the last viewer has to return
    /// focus to what held it before the stack was entered, not before the
    /// frame being popped.
    pub fn push(&mut self, viewer: Viewer, from: Focus) {
        if self.open.is_empty() {
            self.restore = from;
        }
        self.open.push(viewer);
    }

    /// Pop the innermost viewer.
    pub fn pop(&mut self) -> Option<Viewer> {
        self.open.pop()
    }

    /// Drop every viewer at once, as leaving the whole stack does.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// The focus the stack displaced, for when it empties.
    pub const fn restore(&self) -> Focus {
        self.restore
    }

    /// The body geometry the last drawn frame measured, in rows and columns.
    pub const fn view(&self) -> (u16, u16) {
        self.view
    }

    /// Record the geometry the next frame will draw at.
    pub const fn set_view(&mut self, rows: u16, cols: u16) {
        self.view = (rows, cols);
    }

    /// A viewer asked for and not yet opened.
    pub const fn pending(&self) -> Option<&ViewRequest> {
        self.pending.as_ref()
    }

    /// Queue a viewer for the event loop, replacing whatever was queued.
    pub fn request(&mut self, request: ViewRequest) {
        self.pending = Some(request);
    }

    /// Take the queued viewer, so the event loop can open it.
    pub fn take_pending(&mut self) -> Option<ViewRequest> {
        self.pending.take()
    }

    /// Offer one line-index batch to every viewer on the stack, and say
    /// whether any of them wanted it.
    ///
    /// Every viewer rather than only the top one: the help page sits over a
    /// file whose scan is still running, and that scan's batches must keep
    /// landing so the file underneath is still indexed when the help page
    /// closes. A batch whose id matches nothing is dropped, which is what
    /// makes a scan winding down after its viewer closed harmless.
    pub fn apply_index(&mut self, batch: &crate::viewer::index::IndexBatch) -> bool {
        self.open.iter_mut().any(|v| v.apply_index(batch))
    }

    /// Offer one match-counter batch to every viewer, for the same reason and
    /// dropped just as quietly.
    pub fn apply_find(&mut self, batch: &crate::viewer::find::FindBatch) -> bool {
        self.open.iter_mut().any(|v| v.apply_find(batch))
    }

    /// The background match counters the event loop owes the open viewers.
    pub fn take_find_jobs(&mut self) -> Vec<crate::viewer::find::FindJob> {
        self.open
            .iter_mut()
            .filter_map(Viewer::take_find_job)
            .collect()
    }
}
