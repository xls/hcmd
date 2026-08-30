//! The file operation the user has started describing.

use crate::ops::JobKind;
use crate::ops::delete::TrashSplit;
use crate::vfs::VfsPath;

/// The file operation the user has started describing and not yet confirmed.
///
/// A draft exists exactly while a dialog is asking about it: accepting turns it
/// into exactly one [`crate::ops::JobSpec`] and clears the draft, and
/// dismissing discards it whole. Exactly one such dialog can be open at a time,
/// which is what makes one slot enough.
///
/// # The operands are captured, not re-derived
///
/// The sources are taken when the prompt is built and acted on when the answer
/// comes back, rather than being read off the panel again. A listing that
/// arrives while the prompt is up, which a background job finishing and
/// re-reading both panels does routinely, rebuilds and re-sorts the entries
/// under a cursor that is a raw index, so "the entry under the cursor" at
/// answer time can be a different file from the one the prompt named. For
/// `Shift+F8` that difference is unrecoverable.
///
/// The target is kept as a structured path for a related reason: the dialog's
/// target is a line of text, and a line of text cannot carry a path's segment
/// stack. `…/bundle.zip#/` read back as text is a local file whose name ends in
/// `#`, and `F5` into an archive would write one instead of adding a member.
///
#[derive(Debug, Default)]
pub struct JobDraft {
    /// Which operation the open `F5`/`F6`/`F8` dialog is about.
    ///
    /// The dialog answers carry no operation kind and a dialog deliberately
    /// gets no `&mut App`, so the kind waits here between opening the dialog
    /// and its answer coming back.
    pub op: Option<JobKind>,

    /// The operands the open confirmation was written from.
    pub sources: Vec<VfsPath>,

    /// The destination the copy/move dialog was **seeded** with, kept whole,
    /// and used when the text still says what it said when the dialog opened.
    /// A target the user actually typed is a local path, as it always was.
    pub target: Option<VfsPath>,

    /// A trash-availability probe the event loop owes the delete confirmation.
    ///
    ///
    /// The one question a delete draft asks the filesystem before it can
    /// finish phrasing itself. `dispatch` may not ask, and the design
    /// requires the absence of a trash to be decided before the operation
    /// starts and never discovered during it, so the keystroke queues the
    /// question here and the event loop answers it.
    pub trash_probe: Option<Vec<VfsPath>>,

    /// How the probe divided the pending delete: what the trash will take, and
    /// what has nowhere to go (the mixed selection, which "names
    /// how many of each, and applies one decision to the batch").
    pub trash_split: Option<TrashSplit>,
}

impl JobDraft {
    /// Take the sources, leaving the draft with none.
    pub fn take_sources(&mut self) -> Vec<VfsPath> {
        std::mem::take(&mut self.sources)
    }

    /// Discard the draft whole, which is what dismissing a dialog does.
    pub fn discard(&mut self) {
        *self = Self::default();
    }
}
