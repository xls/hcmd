//! What one run of the program remembers about searching.

use crate::app::SearchRequest;
use crate::search::Query;
use crate::search::saved::{History, SavedSearch};

/// Everything one session remembers about searching: the last query, the
/// drop-down history, the saved searches, whether either needs writing back to
/// disk, and the search the event loop has not started yet.
///
/// The name is accurate rather than borrowed: its lifetime is exactly one
/// session. The previous answer and the combo-box history are session state
/// and not configuration, which is why the design keeps them out of
/// `config.toml`; the saved searches are the on-disk half, loaded once and
/// carried here so the dialog opens without reading the disk.
///
/// Neither dirty flag is a write. [`crate::input::dispatch`] may not touch the
/// filesystem, so the Load/Save tab sets a flag and
/// [`crate::app::App::service_search_state`] performs the write.
///
/// # A remote content search is queued only after the user has agreed
///
/// Reading the contents of every file a mask admits costs somebody else's
/// machine, so the design makes it opt-in. That is what the second slot is:
/// a search that has been fully described and is waiting on an answer, held
/// apart from one that is merely waiting on the event loop.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// The last query the dialog answered with, so reopening offers it again.
    pub last: Option<Query>,
    /// The three combo-box drop-downs of.
    pub history: History,
    /// `searches.toml`, as last loaded or stored.
    pub saved: Vec<SavedSearch>,
    /// Which tab of the dialog was open, so it reopens where it was left.
    pub tab: usize,

    /// `searches.toml` needs writing: the Load/Save tab added, replaced or
    /// deleted an entry.
    pub saved_dirty: bool,
    /// `search-history.toml` needs writing: a search remembered a mask, a
    /// pattern or a root (the design puts history in the state directory).
    pub history_dirty: bool,

    /// `Alt+F7` or `Ctrl+B` queued a search.
    ///
    /// One slot, because a second search into the same tab replaces the first
    /// and there is nothing a queue of them would mean.
    pub pending: Option<Box<SearchRequest>>,
    /// A content search across a network, waiting on the opt-in.
    pub pending_remote: Option<Box<SearchRequest>>,
}
