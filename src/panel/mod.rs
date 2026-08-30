//! Panel state.
//!
//! Panel state is `Vec<Tab>` plus an active index **from v0.1**, because it is
//! not retrofittable: the cursor, the marks and the sort order all belong to
//! the [`Tab`], not to the [`Panel`]. The quick-search
//! buffer belongs to the panel, because it is a property of the typing session
//! rather than of the folder.

pub mod columns;
pub mod format;
pub mod goto;
pub mod marks;
pub mod mask;
pub mod state;
pub mod text;

use std::collections::HashSet;
use std::fmt;

use crate::ops::SizeCache;
use crate::vfs::list::ListingId;
use crate::vfs::{Capabilities, Entry, VfsPath};

pub use columns::{Allocated, Allocation, allocate};
pub use format::{Cell, Counts};
pub use text::{Align, Crop};

/// Nine tabs per panel, the maximum the design sets - "which is what makes
/// single-key switching sufficient", since `Alt+1`-`Alt+9` is the whole of the
/// tab-switching keyspace. It is an invariant, not a default: `panel.max_tabs`
/// tunes the limit downwards and is clamped to this on load.
pub const MAX_TABS: usize = 9;

/// The narrowest a tab-bar label is worth rendering: an index digit, a space,
/// and one cell of the name or the ellipsis that replaced it. Below this the
/// bar scrolls instead of shrinking further ([`Panel::tab_bar_labels`]).
pub const MIN_TAB_LABEL: usize = 3;

/// Below this many rows, a listing that is still filling re-sorts on **every**
/// arriving batch ([`Tab::sort_streaming`]).
///
/// Two thousand rows sort in single-digit milliseconds, so a real directory -
/// which is what this whole path was written for - behaves exactly as it did
/// before the doubling schedule existed, and only the unbounded
/// virtual listings ever reach the other branch.
pub const SORT_EVERY_BATCH_BELOW: usize = 2048;

/// Which side of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// The left panel.
    Left,
    /// The right panel.
    Right,
}

impl Side {
    /// The other one.
    pub const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// A stable id, for state files and messages.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// A column that a panel can render.
///
/// The order of the variants is not the render order - that comes from
/// `config.panel.columns.order`, which is also what `Ctrl+<n>` addresses.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnId {
    /// Filename. Always present, always the flexible column.
    Name,
    /// Extension, split out of the name, case preserved.
    Ext,
    /// Byte count, right-aligned; `<DIR>` for directories.
    Size,
    /// Modification time.
    Date,
    /// Attribute flags: `drwxr-xr-x` under `attr_style = "unix"`, `-a--` under
    /// `"dos"`.
    Attr,
    /// Owning user, resolved to a name.
    Owner,
    /// Owning group, resolved to a name.
    Group,
    /// Numeric mode, `0644`.
    PermsOctal,
}

impl ColumnId {
    /// Every column, for validation and for the `F1` reference.
    pub const ALL: &'static [Self] = &[
        Self::Name,
        Self::Ext,
        Self::Size,
        Self::Date,
        Self::Attr,
        Self::Owner,
        Self::Group,
        Self::PermsOctal,
    ];

    /// The stable id used in `config.toml`.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Ext => "ext",
            Self::Size => "size",
            Self::Date => "date",
            Self::Attr => "attr",
            Self::Owner => "owner",
            Self::Group => "group",
            Self::PermsOctal => "perms_octal",
        }
    }

    /// Parse a `config.toml` column id.
    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.id() == s)
    }

    /// The column header text, before the sort arrow is prefixed.
    ///
    pub const fn header(&self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Ext => "Ext",
            Self::Size => "Size",
            Self::Date => "Date",
            Self::Attr => "Attr",
            Self::Owner => "Owner",
            Self::Group => "Group",
            Self::PermsOctal => "Perms",
        }
    }

    /// True for the one column that absorbs the leftover width and is never
    /// dropped.
    pub const fn is_flexible(&self) -> bool {
        matches!(self, Self::Name)
    }
}

impl fmt::Display for ColumnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Serialized as its `config.toml` id, so it works both as a value in
/// `columns.order` and as a *key* in the `width` / `min_chars` tables.
impl serde::Serialize for ColumnId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.id())
    }
}

impl<'de> serde::Deserialize<'de> for ColumnId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = ColumnId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a column id such as \"name\" or \"size\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ColumnId, E> {
                ColumnId::from_id(v).ok_or_else(|| {
                    let known: Vec<&str> = ColumnId::ALL.iter().map(|c| c.id()).collect();
                    E::custom(format!(
                        "unknown column {v:?}; known columns are {}",
                        known.join(", ")
                    ))
                })
            }
        }
        d.deserialize_str(V)
    }
}

/// What a panel is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortKey {
    /// Directory order as the backend produced it (`Ctrl+F7`).
    Unsorted,
    /// Sorted by a column's field.
    Column(ColumnId),
}

impl Default for SortKey {
    fn default() -> Self {
        Self::Column(ColumnId::Name)
    }
}

/// The sort state of one tab (it belongs to the tab, not the
/// panel, so switching tabs restores that tab's ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SortState {
    /// The field.
    pub key: SortKey,
    /// Pressing the same sort key again reverses.
    pub reverse: bool,
    /// The field deciding the order of entries the primary key ties, and its
    /// own direction.
    ///
    /// Reverses independently of the primary: sorting by extension descending
    /// walks the extensions z→a while the files within each stay a→z. Name is
    /// still the final tiebreak underneath both, so ties are never arbitrary.
    pub secondary: Option<(ColumnId, bool)>,
}

impl SortState {
    /// The default: by name, ascending.
    pub const BY_NAME: Self = Self {
        key: SortKey::Column(ColumnId::Name),
        reverse: false,
        secondary: None,
    };

    /// Apply a sort key. Selecting the key that is already active reverses it
    /// instead.
    pub fn apply(&mut self, key: SortKey) {
        if self.key == key {
            self.reverse = !self.reverse;
        } else {
            self.key = key;
            self.reverse = false;
        }
        // Unsorted means the backend's own order, which a tiebreak would
        // disturb.
        if key == SortKey::Unsorted {
            self.secondary = None;
        }
        self.drop_redundant_secondary();
    }

    /// `Ctrl+Shift+<n>`: set the secondary key, or reverse it if it is already
    /// the one set.
    pub fn apply_secondary(&mut self, column: ColumnId) {
        self.secondary = match self.secondary {
            Some((current, reverse)) if current == column => Some((column, !reverse)),
            _ => Some((column, false)),
        };
        self.drop_redundant_secondary();
    }

    /// Clear the secondary key.
    pub const fn clear_secondary(&mut self) {
        self.secondary = None;
    }

    /// "By size, then by size" means nothing, so a secondary that has become
    /// the primary is dropped rather than kept as a no-op.
    fn drop_redundant_secondary(&mut self) {
        if let (SortKey::Column(primary), Some((secondary, _))) = (self.key, self.secondary)
            && primary == secondary
        {
            self.secondary = None;
        }
    }

    /// The short tag for the panel status line: `[size ▼]`, `[unsorted]`.
    /// `ascii` follows `ui.ascii_borders`.
    pub fn indicator(&self, ascii: bool) -> String {
        let primary = match self.key {
            SortKey::Unsorted => return "[unsorted]".to_string(),
            SortKey::Column(c) => format!("{} {}", c.id(), self.arrow(ascii)),
        };
        // with a secondary set the tag carries the whole
        // ordering, because one header row cannot prefix two arrows to two
        // different columns without reading as two primaries.
        match self.secondary {
            None => format!("[{primary}]"),
            Some((column, reverse)) => {
                let arrow = Self::arrow_for(reverse, ascii);
                format!("[{primary} · {} {arrow}]", column.id())
            }
        }
    }

    /// The direction arrow alone: `▲` / `▼`, or `^` / `v` under
    /// `ui.ascii_borders`.
    ///
    /// Empty for an unsorted listing, which has no direction to point in.
    pub const fn arrow(&self, ascii: bool) -> &'static str {
        match self.key {
            SortKey::Unsorted => "",
            SortKey::Column(_) => Self::arrow_for(self.reverse, ascii),
        }
    }

    /// The arrow for a direction, without reference to a key - the secondary
    /// needs one too.
    pub const fn arrow_for(reverse: bool, ascii: bool) -> &'static str {
        match (reverse, ascii) {
            (false, false) => "\u{25B2}",
            (true, false) => "\u{25BC}",
            (false, true) => "^",
            (true, true) => "v",
        }
    }

    /// The header text for one column, with the arrow **prefixed** to the
    /// sorted column's name as Total Commander does it - `▲Ext`, `▲Name`.
    ///
    ///
    /// Every other column gets its plain header, so the arrow is unambiguous.
    /// A sorted column that the width allocation hid contributes no header at
    /// all, which is why [`SortState::indicator`] is shown in the status line as
    /// well: the footer tag is the half that survives the column disappearing.
    pub fn header_text(&self, column: ColumnId, ascii: bool) -> String {
        if self.key == SortKey::Column(column) {
            format!("{}{}", self.arrow(ascii), column.header())
        } else {
            column.header().to_string()
        }
    }

    /// Which column `Ctrl+<n>` addresses, counting from 1 in the **configured**
    /// order.
    ///
    /// Positional, following the configured layout, so putting `size` third
    /// makes `Ctrl+3` sort by size with nothing rebound. The mapping follows the
    /// configured order and **not** what is currently rendered: a hidden column
    /// keeps its number, and a panel can be sorted by a column too narrow to
    /// show. `None` for a key beyond the configured column count, which the
    /// caller turns into a message rather than a silent nothing.
    pub fn column_for_key(order: &[ColumnId], n: usize) -> Option<ColumnId> {
        order.get(n.checked_sub(1)?).copied()
    }
}

impl Default for SortState {
    fn default() -> Self {
        Self::BY_NAME
    }
}

/// Which virtual listing a panel is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKind {
    /// `Alt+F7`: the results of a search.
    Search,
    /// `Ctrl+B`, which is the same mechanism with an empty pattern.
    /// There is no second engine and no second code path;
    /// only the word in the header and the status line differs.
    Branch,
}

impl VirtualKind {
    /// The stable id, used in the header and the status line.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Branch => "branch",
        }
    }
}

/// What a tab needs to remember while it is showing a virtual listing.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualView {
    /// Search results or a branch view.
    pub kind: VirtualKind,
    /// the header: `[search: *.rs "TODO" in ~/dev]`, already
    /// formatted, so the panel renderer does not have to hold a `Query`.
    pub header: String,
    /// The real directory this tab came from and returns to (
    /// "returns the panel to its underlying real directory").
    pub origin: VfsPath,
    /// The name the cursor was on when the listing was created, so leaving
    /// lands back on it.
    pub origin_cursor: Option<String>,
    /// The registered listing. [`Tab::path`] is `listing.to_path()`, and
    /// [`Tab::is_virtual`] is exactly the question of whether the two agree.
    pub listing: ListingId,
    /// The find query a hit's viewer opens with, so the "the hit
    /// already highlighted" is the **same pattern** that found it.
    ///
    /// `None` for a name-only search, which has no text to highlight, and for
    /// a regex content search, which the viewer's find bar cannot compile yet.
    /// It lives on the view rather than on the `App` because it is a property
    /// of the listing the tab is showing and dies with it; there is nowhere
    /// else it would not have to be keyed by listing id and swept.
    pub find: Option<crate::viewer::find::FindQuery>,
}

/// What a tab remembers while it is connected.
///
/// The twin of [`VirtualView`], and here for the same reason: the connection
/// belongs to the **tab**, so one tab can be on `nas.local` while another on
/// the same panel is local. The `Arc<RemoteFs>` itself lives in the registry
/// and is reached through the router; what the tab holds is the id and enough
/// text to draw a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteView {
    /// The connection this tab is on.
    pub id: crate::remote::RemoteId,
    /// `sftp://thorin@nas.local:2222`, already formatted and secret-free.
    pub authority: String,
    /// The local directory this tab was showing before it connected, which
    /// disconnecting returns it to.
    pub origin: VfsPath,
    /// The name the cursor was on there, so disconnecting lands back on it.
    pub origin_cursor: Option<String>,
    /// True once the connection has dropped: the last listing
    /// stays on screen, greyed, and the path is not lost.
    pub disconnected: bool,
}

/// One tab: a working folder and nothing more.
#[derive(Debug, Clone)]
pub struct Tab {
    /// Where this tab is looking.
    pub path: VfsPath,
    /// The rows, in render order. Index 0 is the `..` row when there is one.
    pub entries: Vec<Entry>,
    /// Index into `entries`. Always read through [`Tab::current`] rather than
    /// by indexing, because a re-read can shorten `entries` under it.
    pub cursor: usize,
    /// Index of the first rendered row.
    pub scroll: usize,
    /// Marked entries, by [`Entry::mark_key`].
    ///
    /// v0.1 keyed this on the name, on the reasoning that the set is cleared
    /// on directory change anyway. A **virtual** listing breaks that: it is
    /// flat, so it can hold two rows called `mod.rs` from different
    /// directories, and marking one would mark both - and `F8` would then
    /// delete a file the user did not mark. The key is
    /// therefore the row's real address where it has one and its name where it
    /// does not, which is what the "a set of paths per panel"
    /// asked for in the first place. On an ordinary directory listing nothing
    /// changes.
    pub marks: HashSet<String>,
    /// This tab's sort order.
    pub sort: SortState,
    /// The tab-bar label.
    pub title: String,
    /// True while a listing is still streaming in.
    pub loading: bool,
    /// What the backend servicing [`Tab::path`] can do.
    ///
    /// > `Capabilities` is what the UI consults before offering an operation -
    /// > a read-only archive backend causes `F5` *into* it to be refused up
    /// > front with a clear message rather than failing halfway through a copy.
    ///
    /// **A memo, not an answer.** The answer lives in one place - the
    /// router's capability cache, read through
    /// [`crate::vfs::VfsRouter::known_capabilities`] - and this field is a
    /// copy of it for [`Tab::path`] that `dispatch` can read without reaching
    /// for the router. [`crate::app::App::refresh_caps`] is the only thing
    /// that writes it, and every route that changes what this tab is pointed
    /// at calls that.
    ///
    /// It was five sources of truth before, and which one a key happened to be
    /// gated on decided whether the key worked: a tab whose read failed kept a
    /// pessimistic value for the rest of the session, and a search panel
    /// refused `F6` for exactly as long as the search was still finding things
    /// to press it on.
    ///
    /// A tab that has just been constructed holds the conservative path-free
    /// answer for its backend kind, which is the same thing the cache returns
    /// for a path nothing has resolved yet - under-promising rather than
    /// over-promising, so a refusal can be retried where a false promise would
    /// have failed halfway through a copy.
    pub caps: Capabilities,
    /// Monotonic id of the listing currently being awaited. A [`crate::app::VfsEvent`]
    /// carrying a different generation is stale and is dropped. Not part of the
    /// the design tab model; it is bookkeeping for the async read path.
    pub generation: u64,
    /// Replace `entries` wholesale when the next batch arrives, rather than
    /// appending to them.
    ///
    /// A re-read shows the *same* directory, so clearing the rows the moment it
    /// is asked for leaves the panel empty for every frame until the first batch
    /// lands - a visible flash of bare background after every copy, delete or
    /// `F2`. Keeping the old rows on screen and swapping them out when the
    /// replacement is in hand removes it, and the listing is still rebuilt from
    /// scratch rather than appended to.
    ///
    /// Not used by [`crate::app::App::navigate`], which genuinely has nothing
    /// to show: you have left, and the old directory's rows would be a lie.
    pub replace_on_next_batch: bool,

    /// Entry name to put the cursor on once the listing contains it.
    ///
    /// Going up a directory leaves the cursor on the directory you came *out
    /// of* rather than at the top of the parent, which is what makes walking
    /// back up a tree with `Backspace` keep your place at every level. Reads
    /// are streaming, so the name cannot be resolved to an index
    /// at navigation time - the entries are not there yet. It is resolved by
    /// [`Tab::resolve_pending_select`] as batches arrive and abandoned when the
    /// listing completes without ever producing the name (it was deleted, or
    /// `show_hidden` is off, or a filter excludes it), leaving the cursor where
    /// it would otherwise have been.
    ///
    /// Not in the design; requested behaviour, and what Total Commander does.
    pub pending_select: Option<String>,

    /// The virtual listing this tab is showing, or `None` for a real directory.
    ///
    ///
    /// Boxed because a virtual tab is the rare one: eighteen tabs a session
    /// pay one pointer each rather than five fields each.
    pub virtual_view: Option<Box<VirtualView>>,

    /// The remote connection this tab is on, or `None` for a local one.
    ///
    ///
    /// Boxed for the reason [`Tab::virtual_view`] is: a connected tab is the
    /// rare one. An invariant holds it in step with the path -
    /// `remote_view.is_some()` exactly when `path.backend()` is
    /// `BackendKind::Remote(_)`, and the ids agree
    /// (the design I8).
    pub remote_view: Option<Box<RemoteView>>,

    /// How many rows were in `entries` the last time [`Tab::sort_entries`]
    /// ran, for [`Tab::sort_streaming`]'s doubling schedule.
    ///
    /// Bookkeeping for the streaming read path, not part of the tab
    /// model: it is never saved and never restored.
    pub sorted_rows: usize,
}

impl Tab {
    /// A tab at a path, with nothing read yet.
    pub fn new(path: VfsPath) -> Self {
        Self {
            title: path.display_title(),
            // Exactly what the cache answers for a path nothing has resolved,
            // so a tab built here and a tab refreshed from the router agree
            // until something is actually known. See [`Tab::caps`].
            caps: path.backend().capabilities(),
            path,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            marks: HashSet::new(),
            sort: SortState::default(),
            loading: false,
            generation: 0,
            pending_select: None,
            replace_on_next_batch: false,
            virtual_view: None,
            remote_view: None,
            sorted_rows: 0,
        }
    }

    /// The entry under the cursor, or `None` for an empty listing.
    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// True when the entry under the cursor is marked.
    pub fn current_is_marked(&self) -> bool {
        self.current().is_some_and(|e| self.is_marked(e))
    }

    /// Is this panel showing a virtual listing?
    ///
    /// The question the design resolves `Ctrl+R` and `Esc` by. It is
    /// answered from the field rather than from the path, and an invariant
    /// holds the two in step: `virtual_view.is_some()` exactly when
    /// `path.backend()` is [`crate::vfs::BackendKind::List`].
    pub fn is_virtual(&self) -> bool {
        self.virtual_view.is_some()
    }

    /// What it is showing, when it is showing one.
    pub fn virtual_view(&self) -> Option<&VirtualView> {
        self.virtual_view.as_deref()
    }

    /// The connection this tab is on, when it is on one.
    pub fn remote_view(&self) -> Option<&RemoteView> {
        self.remote_view.as_deref()
    }

    /// The same, mutably, for the one flag that changes while connected:
    /// [`RemoteView::disconnected`].
    pub fn remote_view_mut(&mut self) -> Option<&mut RemoteView> {
        self.remote_view.as_deref_mut()
    }

    /// True while connected.
    ///
    /// An invariant holds it in step with the path: `remote_view.is_some()`
    /// exactly when `path.backend()` is `BackendKind::Remote(_)`, and the ids
    /// agree (the design I8).
    pub fn is_remote(&self) -> bool {
        self.remote_view.is_some()
    }

    /// True when this tab is connected and the connection has dropped
    /// (the disconnected state).
    pub fn is_disconnected(&self) -> bool {
        self.remote_view().is_some_and(|view| view.disconnected)
    }

    /// The full [`VfsPath`] of the entry under the cursor, honouring
    /// [`crate::vfs::Entry::location`] so a virtual listing addresses the real
    /// file.
    pub fn current_path(&self) -> Option<VfsPath> {
        let entry = self.current()?;
        if entry.is_parent {
            return self.path.parent();
        }
        Some(
            entry
                .location
                .clone()
                .unwrap_or_else(|| self.path.join(&entry.name)),
        )
    }

    /// Empty the rows, and with them everything derived from their order.
    ///
    /// One method rather than a bare `entries.clear()` at each site, so that
    /// [`Tab::sorted_rows`] cannot be left describing a listing that is no
    /// longer there - which would make [`Tab::sort_streaming`] skip the first
    /// sorts of the next one.
    pub fn clear_entries(&mut self) {
        self.entries.clear();
        self.sorted_rows = 0;
    }

    /// Try to satisfy [`Tab::pending_select`] against the entries read so far.
    ///
    /// Returns true once the name has been found and the cursor moved, after
    /// which the request is spent. Called on every arriving batch, so the
    /// cursor lands as soon as the name appears rather than waiting for a slow
    /// listing to finish.
    pub fn resolve_pending_select(&mut self) -> bool {
        let Some(name) = self.pending_select.as_deref() else {
            return false;
        };
        let Some(index) = self.entries.iter().position(|e| e.name == name) else {
            return false;
        };
        self.cursor = index;
        self.pending_select = None;
        true
    }

    /// Clamp the cursor into `entries`, after a re-read shortened it.
    ///
    /// A tab that is still [`Tab::loading`] is left alone. Its cursor is a
    /// *hint* until the read finishes - the position restored from the state
    /// file or the one held across a re-read - and the listing
    /// underneath it is empty or half-read, so clamping now would throw the
    /// hint away before it could ever take effect. The read's `Done` clamps it
    /// for real against the finished listing.
    pub fn clamp_cursor(&mut self) {
        if self.loading {
            return;
        }
        let last = self.entries.len().saturating_sub(1);
        if self.cursor > last {
            self.cursor = last;
        }
        if self.entries.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
        }
    }

    /// Bring the cursor into view for a viewport `rows` tall.
    pub fn scroll_into_view(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll.saturating_add(rows) {
            self.scroll = self.cursor.saturating_sub(rows.saturating_sub(1));
        }
        let max_scroll = self.entries.len().saturating_sub(rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    /// The rows a viewport `rows` tall shows, as an index range into
    /// [`Tab::entries`]. Always in bounds, including for `rows == 0` and an
    /// empty listing.
    pub fn visible_range(&self, rows: usize) -> std::ops::Range<usize> {
        let start = self.scroll.min(self.entries.len());
        let end = start.saturating_add(rows).min(self.entries.len());
        start..end
    }

    // ------------------------------------------------------------ cursor ----

    /// Move the cursor by a signed number of rows, clamping at both ends and
    /// keeping the scroll offset consistent for a `rows`-tall viewport.
    ///
    /// `rows` changes on every resize and is zero before the first frame, so
    /// every caller has to tolerate that; this does.
    pub fn move_by(&mut self, delta: isize, rows: usize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        let last = self.entries.len().saturating_sub(1);
        self.cursor = if delta >= 0 {
            self.cursor.saturating_add(delta.unsigned_abs()).min(last)
        } else {
            self.cursor.saturating_sub(delta.unsigned_abs())
        };
        self.scroll_into_view(rows);
    }

    /// Put the cursor on a row, clamping.
    pub fn move_to(&mut self, index: usize, rows: usize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = index.min(self.entries.len().saturating_sub(1));
        self.scroll_into_view(rows);
    }

    /// `Home`.
    pub fn move_first(&mut self, rows: usize) {
        self.move_to(0, rows);
    }

    /// `End`.
    pub fn move_last(&mut self, rows: usize) {
        self.move_to(self.entries.len().saturating_sub(1), rows);
    }

    /// `PgUp`. A zero-row viewport still moves by one, so the key is never dead.
    pub fn page_up(&mut self, rows: usize) {
        let step = isize::try_from(rows.max(1)).unwrap_or(isize::MAX);
        self.move_by(step.saturating_neg(), rows);
    }

    /// `PgDn`.
    pub fn page_down(&mut self, rows: usize) {
        let step = isize::try_from(rows.max(1)).unwrap_or(isize::MAX);
        self.move_by(step, rows);
    }

    // ------------------------------------------------------------- marks ----

    /// The index of an entry by name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }

    /// The name under the cursor, for re-finding it after a re-sort or a
    /// re-read.
    pub fn cursor_name(&self) -> Option<String> {
        self.current().map(|e| e.name.clone())
    }

    /// Put the cursor back on a named entry. False when it is gone.
    pub fn focus_name(&mut self, name: &str) -> bool {
        match self.index_of(name) {
            Some(index) => {
                self.cursor = index;
                true
            }
            None => false,
        }
    }

    /// The address of row `index`, honouring [`Entry::location`] so a virtual
    /// listing addresses the real file. `None` for `..` and
    /// for an index that is not a row.
    pub fn path_of(&self, index: usize) -> Option<VfsPath> {
        let entry = self.entries.get(index)?;
        if entry.is_parent {
            return None;
        }
        Some(entry_home(&self.path, entry))
    }

    /// The directory row `index` really lives in.
    ///
    /// On an ordinary listing that is the tab's own path; on a virtual one it
    /// is the parent of the row's real home, and the two differ for every row.
    /// That is what a collision, a `[P]` placeholder and a temporary rename
    /// name all have to be judged against.
    pub fn dir_of(&self, index: usize) -> Option<VfsPath> {
        let entry = self.entries.get(index)?;
        if entry.is_parent {
            return None;
        }
        match entry.location.as_ref() {
            Some(home) => home.parent(),
            None => Some(self.path.clone()),
        }
    }

    /// The rows an operation acts on: the marked rows, or the
    /// row under the cursor when nothing is marked.
    ///
    /// **Indices**, not names, because a flat virtual listing can hold two
    /// rows called `mod.rs` and a name is therefore not an identity there.
    /// `..` is never one of them.
    pub fn operand_rows(&self) -> Vec<usize> {
        if !self.marks.is_empty() {
            return self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| self.is_marked(e))
                .map(|(i, _)| i)
                .collect();
        }
        self.entries
            .get(self.cursor)
            .filter(|e| !e.is_parent)
            .map(|_| vec![self.cursor])
            .unwrap_or_default()
    }

    /// Those rows' real addresses, honouring [`Entry::location`].
    pub fn operand_paths(&self) -> Vec<VfsPath> {
        self.operand_rows()
            .into_iter()
            .filter_map(|i| self.path_of(i))
            .collect()
    }

    /// the different rule: the marked rows, **or every row** when
    /// nothing is marked.
    ///
    /// Deliberately not [`Tab::operand_rows`]. the first sentence
    /// says the multi-rename tool "operates on the marked entries, or the
    /// whole directory if nothing is marked"; the design
    /// settles `F5` and `F8` on the cursor instead. Both are right for their
    /// own key, so they are two functions and nobody unifies them.
    pub fn rename_rows(&self) -> Vec<usize> {
        if !self.marks.is_empty() {
            return self.operand_rows();
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_parent)
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether an entry is marked. The `..` row never is.
    ///
    /// Keyed on [`Entry::mark_key`], which is the row's real address on a
    /// virtual listing and its name everywhere else. **Every** mark operation
    /// goes through that key; two rows called `mod.rs` from different
    /// directories mark independently.
    pub fn is_marked(&self, entry: &Entry) -> bool {
        !entry.is_parent && self.marks.contains(entry.mark_key().as_ref())
    }

    /// Toggle the mark under the cursor. Returns whether anything changed -
    /// `..` is never markable, and an empty listing has nothing to mark.
    ///
    /// `Insert` toggles and then moves down; `Space` toggles and stays. Both
    /// conditions on focus and on the quick-search buffer belong to the input
    /// layer; this is the primitive underneath them.
    pub fn toggle_mark(&mut self) -> bool {
        let Some(entry) = self.entries.get(self.cursor) else {
            return false;
        };
        if entry.is_parent {
            return false;
        }
        let key = entry.mark_key().into_owned();
        if !self.marks.remove(&key) {
            self.marks.insert(key);
        }
        true
    }

    /// `Ctrl+A`: mark everything but `..`.
    pub fn mark_all(&mut self) {
        self.marks = self
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.mark_key().into_owned())
            .collect();
    }

    /// `*`: invert the marks.
    pub fn invert_marks(&mut self) {
        let keys: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.mark_key().into_owned())
            .collect();
        for key in keys {
            if !self.marks.remove(&key) {
                self.marks.insert(key);
            }
        }
    }

    /// Drop every mark. Called on a directory change.
    pub fn clear_marks(&mut self) {
        self.marks.clear();
    }

    /// Drop marks whose entry is no longer in the listing.
    ///
    /// marks are "preserved across a re-read where the path still
    /// exists". Keeping them by name and pruning what vanished is exactly that,
    /// and it means a re-read of the same directory does not silently
    /// resurrect a mark if the file comes back.
    pub fn prune_marks(&mut self) {
        if self.marks.is_empty() {
            return;
        }
        let present: HashSet<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.mark_key().into_owned())
            .collect();
        self.marks.retain(|key| present.contains(key.as_str()));
    }

    /// The counts the panel status line reports, with no directory
    /// sizes available. Every marked directory is therefore unsized and the
    /// line shows a lower bound.
    pub fn counts(&self) -> Counts {
        self.counts_with(&SizeCache::new())
    }

    /// The counts, consulting the session's directory sizes.
    ///
    /// > Directories contribute to the **counts** always, and to the **size**
    /// > only once they have been sized.
    ///
    /// That is the whole difference between this and [`Tab::counts`]: a marked
    /// directory found in `sizes` adds its bytes to `marked_bytes`, and one
    /// that is not bumps `unsized_dirs`, which is what puts the `≥` on the
    /// line. `..` never counts, in either form.
    pub fn counts_with(&self, sizes: &SizeCache) -> Counts {
        let mut counts = Counts::default();
        for entry in &self.entries {
            if entry.is_parent {
                continue;
            }
            let marked = self.marks.contains(entry.mark_key().as_ref());
            if entry.is_dir() {
                counts.total_dirs = counts.total_dirs.saturating_add(1);
                if marked {
                    counts.marked_dirs = counts.marked_dirs.saturating_add(1);
                    // The size cache is keyed on the real home, which on a
                    // virtual listing is not `self.path.join(name)`.
                    //
                    match sizes.get(&entry_home(&self.path, entry)) {
                        Some(stats) => {
                            counts.marked_bytes = counts.marked_bytes.saturating_add(stats.bytes);
                        }
                        None => counts.unsized_dirs = counts.unsized_dirs.saturating_add(1),
                    }
                }
            } else {
                counts.total_files = counts.total_files.saturating_add(1);
                counts.total_bytes = counts.total_bytes.saturating_add(entry.size);
                if marked {
                    counts.marked_files = counts.marked_files.saturating_add(1);
                    counts.marked_bytes = counts.marked_bytes.saturating_add(entry.size);
                }
            }
        }
        counts
    }

    // ------------------------------------------------------------- sorts ----

    /// Re-sort this tab's entries in place.
    ///
    /// * The sort is **stable**, so equal keys keep their relative order.
    /// * `..` is always first, whatever the key and whatever the direction.
    /// * `directories_first` applies on top of every order and is configured
    ///   separately, not toggled by the sort keys.
    /// * The cursor is re-found **by name** afterwards, so it stays on the same
    ///   entry rather than jumping to whatever landed on its old row.
    pub fn sort_entries(&mut self, directories_first: bool) {
        let focused = self.cursor_name();
        let key = self.sort.key;
        let reverse = self.sort.reverse;
        let secondary = self.sort.secondary;

        self.entries.sort_by(|a, b| {
            if a.is_parent != b.is_parent {
                return b.is_parent.cmp(&a.is_parent);
            }
            if directories_first && a.is_dir() != b.is_dir() {
                return b.is_dir().cmp(&a.is_dir());
            }
            match key {
                // `Ctrl+F7` leaves the backend's own order alone, so there is
                // no tiebreak to apply either - `sort_by` is stable, and
                // `Equal` everywhere is what preserves it.
                SortKey::Unsorted => std::cmp::Ordering::Equal,
                SortKey::Column(column) => {
                    let ord = compare_by(column, a, b);
                    let ord = if reverse { ord.reverse() } else { ord };
                    // The secondary key, reversing independently of the primary.
                    //
                    let ord = ord.then_with(|| match secondary {
                        Some((column, reverse)) => {
                            let ord = compare_by(column, a, b);
                            if reverse { ord.reverse() } else { ord }
                        }
                        None => std::cmp::Ordering::Equal,
                    });
                    // The tiebreak is applied *after* the reversal and is never
                    // itself reversed: `Ctrl+2` twice walks the extensions z→a,
                    // but the files sharing an extension stay a→z. Only the key
                    // you actually chose changes direction.
                    ord.then_with(|| name_cmp(&a.name, &b.name))
                }
            }
        });

        if let Some(name) = focused {
            self.focus_name(&name);
        }
        self.sorted_rows = self.entries.len();
        self.clamp_cursor();
    }

    /// Re-sort a listing that is **still filling**, on a doubling schedule.
    ///
    /// Reads stream in batches and the panel has to look sorted
    /// while they arrive, so the obvious thing is to sort on every batch. That
    /// is quadratic in the number of rows, and the design puts an unbounded
    /// row count on this path: a 275k-row search sorts 2,149 times over a
    /// vector growing to 275k, which measured at 26 s of event-loop work for a
    /// listing one final sort orders in 0.2 s.
    ///
    /// So a small listing - every real directory anyone points a file manager
    /// at - still sorts on every batch and looks exactly as it did, and a large
    /// one sorts when it has **doubled** since the last time. That is
    /// `O(log n)` sorts instead of `O(n / batch)`, the rows on screen are the
    /// sorted ones, and the unsorted tail is off the bottom of the panel. The
    /// order is final either way: [`crate::app::App::apply_vfs_event`] sorts
    /// once more when the read completes.
    pub fn sort_streaming(&mut self, directories_first: bool) {
        let rows = self.entries.len();
        if rows <= SORT_EVERY_BATCH_BELOW || rows >= self.sorted_rows.saturating_mul(2) {
            self.sort_entries(directories_first);
        }
    }

    /// Apply a sort key and re-sort. The same key again reverses.
    ///
    pub fn apply_sort(&mut self, key: SortKey, directories_first: bool) {
        self.sort.apply(key);
        self.sort_entries(directories_first);
    }
}

/// Compare two entries by one column's field - the **primary key only**.
///
/// Every arm is a *total* order, which is what makes the surrounding
/// `sort_by` stable in the way the spec asks for: names compare
/// case-insensitively with an exact-comparison tiebreak, so two entries are
/// `Equal` only when their names are byte-identical.
///
/// `Size` is the one column whose key is not the field it appears to be. A
/// directory's [`Entry::size`] is its **inode** size - 4096, 12288, whatever
/// the filesystem spent on the directory itself - and it says nothing about
/// what the directory holds. Sorting on it is what produced the order this
/// comparator was written to fix: 926 MB, then 14 MB, then 176 bytes, in a
/// column that rendered `<DIR>` for all three. So every directory ties with
/// every other directory here, and the name tiebreak in
/// [`Tab::sort_entries`] orders them.
///
/// That holds for a directory the user has walked with `Space` too: it keeps
/// showing its tree size in the column while sorting as though it had none.
/// That is a decision, not an oversight. Sorting must never trigger a walk -
/// a panel of ten thousand folders has to cost what it costs today - and a
/// key that only the already-visited rows carry would order the panel by
/// where the user happened to have been.
///
/// The name tiebreak deliberately lives outside this function, in
/// [`Tab::sort_entries`], because the primary key is what reverses and the
/// tiebreak is not: sorting by extension descending should walk the extensions
/// `z`→`a` while the files *within* each extension stay `a`→`z`. Folding the
/// tiebreak in here and reversing the result would invert both, which reads as
/// the filenames scrambling for no reason the user asked for.
pub fn compare_by(column: ColumnId, a: &Entry, b: &Entry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match column {
        ColumnId::Name => name_cmp(&a.name, &b.name),
        ColumnId::Ext => name_cmp(a.extension(), b.extension()),
        // The pair (is_dir, a file's size) is a total order, so the stable
        // sort above stays well defined. Directories group ahead of files,
        // which means reversing the key moves the whole group rather than
        // shuffling names inside it.
        ColumnId::Size => match (a.is_dir(), b.is_dir()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.size.cmp(&b.size),
        },
        ColumnId::Date => match (a.mtime, b.mtime) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        },
        ColumnId::Attr | ColumnId::PermsOctal => a.mode.cmp(&b.mode),
        ColumnId::Owner => a.uid.cmp(&b.uid),
        ColumnId::Group => a.gid.cmp(&b.gid),
    }
}

/// Case-insensitive comparison, breaking a tie in favour of **lower** case.
///
/// The tiebreak is deliberately not a plain byte comparison. Two names that
/// differ only in case are adjacent either way, but a byte comparison puts
/// `Thorin` before `thorin` (`'T'` is 0x54, `'t'` is 0x74), whereas glibc's
/// collation under a normal `en_US.UTF-8` locale - what `ls` shows the user
/// every day - puts `thorin` first. Matching the platform matters here because
/// this ordering is also what quick search walks: typing `tho`
/// should land on `thorin`, not on a neighbour the user did not expect.
pub fn name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

/// The quick-search buffer.
///
/// It belongs to the panel rather than the tab: it is the state of the typing
/// session, and it is cleared by cursor movement, by `Esc`, and by leaving the
/// directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuickSearch {
    /// What has been typed so far.
    pub buffer: String,
    /// `Ctrl+S` started a search that has nothing in it yet.
    ///
    /// The buffer normally starts on the first printable key, so this matters
    /// in exactly one configuration: `panel.digit_keys = "tabs"`, where a bare
    /// digit switches tabs *while the buffer is empty* and a file called
    /// `2026-budget.xlsx` would otherwise be unreachable by its first
    /// character. Arming says "the next digit is a search, not a tab".
    pub armed: bool,
}

impl QuickSearch {
    /// True when nothing has been typed.
    ///
    /// This is about the *buffer*, not about arming: the design gives `Space`
    /// to marking and `Backspace` to the parent directory whenever the buffer is
    /// empty, and an armed but empty search does not change either.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// True when the panel status line should show a search.
    ///
    /// An armed empty search shows too, so `Ctrl+S` gives visible feedback
    /// rather than silently changing what the next digit does.
    pub fn is_active(&self) -> bool {
        self.armed || !self.buffer.is_empty()
    }

    /// `Ctrl+S`: start a search with an empty buffer.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    /// Append a character.
    pub fn push(&mut self, c: char) {
        self.buffer.push(c);
    }

    /// Remove the last character. Returns false when there was nothing to
    /// remove, which is how `Backspace` decides to go to the parent directory
    /// instead.
    pub fn pop(&mut self) -> bool {
        self.buffer.pop().is_some()
    }

    /// Drop the search, arming included.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.armed = false;
    }
}

/// One of the two panels.
///
/// `tabs` and `active` are private on purpose. Keeping them behind accessors is
/// what makes [`Panel::active_tab`] total: the invariant "there is always at
/// least one tab, and `active` indexes it" is enforced in one place instead of
/// being re-checked at every call site. See the design.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Which side this is. Fixed for the life of the panel; `Ctrl+U` swaps
    /// contents, not identities.
    pub side: Side,
    /// At least one, at most `config.panel.max_tabs`.
    tabs: Vec<Tab>,
    /// Index into `tabs`, always in range.
    active: usize,
    /// The quick-search buffer.
    pub quick: QuickSearch,
    /// The active file mask, shown on the path line. `*` means no filter.
    ///
    pub filter_mask: String,
    /// How many entry rows the last layout gave this panel. Written by the
    /// renderer, read by `PgUp`/`PgDn` and by scrolling. Zero before the first
    /// frame, which every consumer must tolerate.
    pub view_rows: usize,
}

impl Panel {
    /// A panel with one tab at `path`.
    pub fn new(side: Side, path: VfsPath) -> Self {
        Self {
            side,
            tabs: vec![Tab::new(path)],
            active: 0,
            quick: QuickSearch::default(),
            filter_mask: "*".to_string(),
            view_rows: 0,
        }
    }

    /// Every tab, in bar order.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// How many tabs there are. Never zero.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// The index of the active tab. Always a valid index into [`Panel::tabs`].
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab.
    pub fn active_tab(&self) -> &Tab {
        match self.tabs.get(self.active) {
            Some(tab) => tab,
            // Unreachable while the invariant holds; falling back beats
            // panicking, and `Tab::new` is cheap.
            None => self.tabs.first().unwrap_or(&*FALLBACK_TAB),
        }
    }

    /// Mutable access to the active tab.
    ///
    /// Repairs the invariant first, so it is always safe to call.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new(VfsPath::local_root()));
        }
        if self.active >= self.tabs.len() {
            self.active = 0;
        }
        let idx = self.active;
        match self.tabs.get_mut(idx) {
            Some(tab) => tab,
            // Dead: the two statements above guarantee `tabs` is non-empty and
            // `idx == 0` or `idx < tabs.len()`. This is not a runtime condition
            // being unwrapped, it is a branch the compiler cannot see is
            // impossible; safe Rust offers no way to take `&mut Vec[i]` without
            // either an `Option` or an indexing panic, and the borrow checker
            // rejects the retry loop that would avoid both.
            #[expect(
                clippy::unreachable,
                reason = "the two lines above make `tabs` non-empty and clamp                           `active` into it; safe Rust offers no way to take                           `&mut Vec[i]` without either an Option or an indexing                           panic, and the borrow checker rejects the retry loop                           that would avoid both"
            )]
            None => unreachable!("Panel::tabs was just made non-empty and active clamped"),
        }
    }

    /// Borrow a tab by index.
    pub fn tab(&self, index: usize) -> Option<&Tab> {
        self.tabs.get(index)
    }

    /// Borrow a tab by index, mutably.
    pub fn tab_mut(&mut self, index: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(index)
    }

    /// Switch to a tab. A no-op for an index that does not exist, so
    /// `Alt+5` with three tabs open does nothing rather than misbehaving;
    /// returns whether the switch happened so the caller can post a message.
    pub fn select_tab(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active = index;
            self.quick.clear();
            true
        } else {
            false
        }
    }

    /// `Ctrl+Tab` / `Ctrl+Shift+Tab`: the next or previous tab, **wrapping**.
    ///
    /// Cycling is what the keys are for, so it wraps rather than stopping at
    /// the ends: with at most nine tabs the far end is never
    /// more than a few presses away, and a key that silently stops working at
    /// the edge reads as broken.
    ///
    /// A no-op with one tab, which is not a failure worth a message -
    /// `Ctrl+Tab` in a single-tab panel plainly has nowhere to go.
    pub fn cycle_tab(&mut self, forward: bool) {
        let count = self.tabs.len();
        if count < 2 {
            return;
        }
        let step = if forward { 1 } else { count.saturating_sub(1) };
        self.active = self.active.saturating_add(step) % count;
        self.quick.clear();
    }

    /// Open a new tab at `path` and make it active (`Ctrl+T`).
    ///
    /// Refused, returning false, at `max_tabs` - the caller posts the message
    /// the design asks for rather than ignoring the key silently.
    ///
    /// `max_tabs` is clamped to [`MAX_TABS`] here as well as on config load,
    /// because the nine-tab ceiling is what makes `Alt+1`-`Alt+9` sufficient:
    /// a tenth tab could be opened but never selected, and
    /// `state::restore` would drop it on the next start.
    pub fn open_tab(&mut self, path: VfsPath, max_tabs: usize) -> bool {
        if self.tabs.len() >= max_tabs.clamp(1, MAX_TABS) {
            return false;
        }
        self.tabs.push(Tab::new(path));
        self.active = self.tabs.len().saturating_sub(1);
        self.quick.clear();
        true
    }

    /// Close the active tab (`Ctrl+W`). A no-op when it is the only one.
    ///
    pub fn close_tab(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        self.quick.clear();
        true
    }

    /// True when the panel is showing a filter other than `*`.
    pub fn is_filtered(&self) -> bool {
        self.filter_mask != "*"
    }

    /// `Ctrl+U`: exchange contents with the other panel. Identities - which
    /// side each panel is - do not move.
    pub fn swap_contents(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.tabs, &mut other.tabs);
        std::mem::swap(&mut self.active, &mut other.active);
        std::mem::swap(&mut self.quick, &mut other.quick);
        std::mem::swap(&mut self.filter_mask, &mut other.filter_mask);
    }

    /// Whether the tab bar should be drawn, given `config.panel.show_tab_bar`.
    ///
    pub fn tab_bar_visible(&self, setting: crate::config::TabBar) -> bool {
        match setting {
            crate::config::TabBar::Never => false,
            crate::config::TabBar::Always => true,
            crate::config::TabBar::Auto => self.tabs.len() > 1,
        }
    }

    /// Replace every tab and the active index at once, used by the state
    /// restore of.
    ///
    /// An empty `tabs` is refused, because the never-empty invariant is what
    /// makes [`Panel::active_tab`] total; the panel is left untouched and the
    /// call returns false.
    pub fn replace_tabs(&mut self, tabs: Vec<Tab>, active: usize) -> bool {
        if tabs.is_empty() {
            return false;
        }
        self.active = active.min(tabs.len().saturating_sub(1));
        self.tabs = tabs;
        self.quick.clear();
        true
    }

    /// The tab-bar labels for a bar `width` cells wide.
    ///
    /// Each label is the tab's directory basename with its index, cropped with
    /// an ellipsis. The width is shared evenly, so nine tabs in a narrow panel
    /// each get a readable stub rather than the first three getting everything.
    /// Returns an empty vector for a zero-width bar.
    ///
    /// When even [`MIN_TAB_LABEL`] cells each will not fit - nine tabs in a
    /// 48-cell panel, which is the documented maximum at a very ordinary size -
    /// the bar **scrolls** rather than dropping the tabs that ran off the end.
    /// the design asks for "each tab's directory name … the active one
    /// highlighted", and a bar that has silently dropped the active tab
    /// highlights nothing at all. The window always contains the active tab,
    /// and the labels keep their real numbers, so `7 src` in the leftmost slot
    /// is itself the sign that the bar is scrolled.
    pub fn tab_bar_labels(&self, width: usize, ascii: bool) -> Vec<TabLabel> {
        let count = self.tabs.len();
        if width == 0 || count == 0 {
            return Vec::new();
        }
        // One separator between labels, paid for before the split. Dividing
        // without subtracting them first overspends by `count - 1` cells, which
        // is exactly enough to push the last tab off the bar.
        let separators = count.saturating_sub(1);
        let budget = width.saturating_sub(separators);
        let even = budget.checked_div(count).unwrap_or(0);

        let (first, visible, each) = if even >= MIN_TAB_LABEL {
            (0, count, even)
        } else {
            // How many labels of MIN_TAB_LABEL cells, each but the first
            // preceded by a separator, fit in `width`.
            let slot = MIN_TAB_LABEL.saturating_add(1);
            let visible = width
                .saturating_add(1)
                .checked_div(slot)
                .unwrap_or(0)
                .clamp(1, count);
            // Scroll the window so it holds the active tab.
            let first = self
                .active
                .saturating_add(1)
                .saturating_sub(visible)
                .min(count.saturating_sub(visible));
            (first, visible, MIN_TAB_LABEL.min(width))
        };

        self.tabs
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(index, tab)| {
                let number = index.saturating_add(1);
                let title = tab.path.display_title();
                let full = format!("{number} {title}");
                TabLabel {
                    index,
                    title,
                    text: text::truncate(&full, each, text::Crop::End, ascii),
                    active: index == self.active,
                }
            })
            .collect()
    }

    /// Re-clamp both the cursor and the scroll offset of every tab after the
    /// entry list or the viewport changed.
    ///
    /// The renderer writes [`Panel::view_rows`] on every frame - including
    /// frames where the terminal was resized - and this is what keeps a cursor
    /// from pointing past the end of a shortened listing.
    pub fn reclamp(&mut self) {
        let rows = self.view_rows;
        for tab in &mut self.tabs {
            tab.clamp_cursor();
            tab.scroll_into_view(rows);
        }
    }

    /// Apply a sort key to the active tab and keep the cursor visible.
    ///
    pub fn sort_active_tab(&mut self, key: SortKey, directories_first: bool) {
        let rows = self.view_rows;
        let tab = self.active_tab_mut();
        tab.apply_sort(key, directories_first);
        tab.scroll_into_view(rows);
    }
}

/// One entry in the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabLabel {
    /// Zero-based index into [`Panel::tabs`]. `Alt+<index + 1>` selects it.
    pub index: usize,
    /// The untruncated directory basename, for a tooltip or a status line.
    pub title: String,
    /// What to draw: `"1 stl"`, cropped to the label's share of the bar.
    pub text: String,
    /// Whether this is the active tab, which the theme highlights.
    pub active: bool,
}

/// Where a row really lives.
///
/// [`Entry::location`] when it has one - which is every row of a virtual
/// listing - and `base.join(name)` otherwise. The one place the fallback is
/// spelled, so nothing computes a search result's address as though the panel
/// path were its directory.
fn entry_home(base: &VfsPath, entry: &Entry) -> VfsPath {
    entry
        .location
        .clone()
        .unwrap_or_else(|| base.join(&entry.name))
}

/// Returned by [`Panel::active_tab`] only if the never-empty invariant is
/// somehow violated. Its path is the local root, so a caller that renders it
/// shows an empty `/` rather than crashing.
static FALLBACK_TAB: std::sync::LazyLock<Tab> =
    std::sync::LazyLock::new(|| Tab::new(VfsPath::local_root()));

#[cfg(test)]
mod tests {
    /// the design leaves the case tiebreak unstated. It matters because quick
    /// search walks this ordering, so it is pinned to what the
    /// user's own `ls` does under `en_US.UTF-8`: lowercase first.
    #[test]
    fn a_name_tie_on_case_alone_puts_lowercase_first() {
        use std::cmp::Ordering;
        assert_eq!(name_cmp("thorin", "Thorin"), Ordering::Less);
        assert_eq!(name_cmp("Thorin", "thorin"), Ordering::Greater);

        let mut v = vec!["Thorin", "thunder", "thorin"];
        v.sort_by(|a, b| name_cmp(a, b));
        assert_eq!(v, vec!["thorin", "Thorin", "thunder"]);
    }

    #[test]
    fn the_case_tiebreak_does_not_disturb_ordinary_ordering() {
        let mut v = vec!["zeta", "Alpha", "beta", "alpha"];
        v.sort_by(|a, b| name_cmp(a, b));
        assert_eq!(v, vec!["alpha", "Alpha", "beta", "zeta"]);
    }

    #[test]
    fn name_cmp_is_a_total_order() {
        // A comparator that is not antisymmetric makes `sort_by` unstable in
        // the sense that matters here: the cursor would not stay put.
        let names = ["a", "A", "ab", "Ab", "aB", "AB", "b", "B", "", "å", "Å"];
        for x in names {
            assert_eq!(name_cmp(x, x), std::cmp::Ordering::Equal);
            for y in names {
                assert_eq!(name_cmp(x, y), name_cmp(y, x).reverse(), "{x:?} vs {y:?}");
            }
        }
    }

    use super::*;

    #[test]
    fn column_ids_round_trip() {
        for c in ColumnId::ALL {
            assert_eq!(ColumnId::from_id(c.id()), Some(*c));
        }
    }

    #[test]
    fn same_key_reverses() {
        let mut s = SortState::BY_NAME;
        s.apply(SortKey::Column(ColumnId::Name));
        assert!(s.reverse);
        s.apply(SortKey::Column(ColumnId::Size));
        assert!(!s.reverse);
        assert_eq!(s.key, SortKey::Column(ColumnId::Size));
    }

    #[test]
    fn indicator_survives_a_hidden_column() {
        let s = SortState {
            key: SortKey::Column(ColumnId::Size),
            reverse: true,
            secondary: None,
        };
        assert_eq!(s.indicator(false), "[size ▼]");
        assert_eq!(s.indicator(true), "[size v]");
    }

    fn tab_with(names: &[&str]) -> Tab {
        let mut tab = Tab::new(VfsPath::local("/x"));
        tab.entries = names
            .iter()
            .map(|n| {
                if n.ends_with('/') {
                    Entry::dir(n.trim_end_matches('/'))
                } else {
                    Entry::file(*n)
                }
            })
            .collect();
        tab
    }

    // ------------------------------ the design rows with a real home ----

    /// A tab of rows that live somewhere other than the panel's own path -
    /// which is every row of a search result or a branch view.
    fn virtual_tab(homes: &[&str]) -> Tab {
        let mut tab = Tab::new(VfsPath::new(crate::vfs::BackendKind::List, "/1"));
        tab.entries = homes
            .iter()
            .map(|home| {
                let path = VfsPath::local(*home);
                let name = path.file_name().unwrap_or_else(|| (*home).to_string());
                let mut entry = Entry::file(name);
                entry.location = Some(path);
                entry
            })
            .collect();
        tab
    }

    #[test]
    fn two_rows_with_one_name_mark_independently() {
        // the design says every operation works on results unchanged, and
        // the design keys marks on the selection. A flat listing can hold two
        // rows called `mod.rs`, so the key has to be the row's real home or
        // `F8` deletes a file the user did not mark.
        let mut tab = virtual_tab(&["/root/one/mod.rs", "/root/two/mod.rs"]);
        tab.cursor = 0;
        assert!(tab.toggle_mark());
        assert_eq!(tab.marks.len(), 1);

        let first = tab.entries.first().cloned().expect("row 0");
        let second = tab.entries.get(1).cloned().expect("row 1");
        assert!(tab.is_marked(&first));
        assert!(!tab.is_marked(&second), "the same name is not the same row");
        assert_eq!(tab.operand_rows(), vec![0]);
        assert_eq!(
            tab.operand_paths(),
            vec![VfsPath::local("/root/one/mod.rs")]
        );

        tab.cursor = 1;
        assert!(tab.toggle_mark());
        assert_eq!(tab.marks.len(), 2);
        assert_eq!(tab.operand_rows(), vec![0, 1]);

        tab.invert_marks();
        assert!(tab.marks.is_empty());
        tab.mark_all();
        assert_eq!(tab.marks.len(), 2);
        // A row that has gone loses its mark and the other one keeps it.
        tab.entries.remove(1);
        tab.prune_marks();
        assert_eq!(tab.marks.len(), 1);
        assert!(tab.is_marked(&first));
    }

    #[test]
    fn an_ordinary_listing_still_marks_by_name() {
        // The same code path, and nothing about a real directory changes:
        // `Entry::location` is `None` there, so `mark_key` is the name.
        let mut tab = tab_with(&["a.rs", "b.rs"]);
        tab.path = VfsPath::local("/root");
        tab.cursor = 1;
        assert!(tab.toggle_mark());
        assert!(tab.marks.contains("b.rs"));
        assert_eq!(tab.operand_paths(), vec![VfsPath::local("/root/b.rs")]);
        assert_eq!(tab.dir_of(1), Some(VfsPath::local("/root")));
    }

    #[test]
    fn a_row_addresses_the_file_and_the_directory_it_really_lives_in() {
        // operations reach through the listing to the real
        // file. The panel's own path is `list:/1` and is never the answer.
        let tab = virtual_tab(&["/root/one/mod.rs", "/elsewhere/mod.rs"]);
        assert_eq!(tab.path_of(0), Some(VfsPath::local("/root/one/mod.rs")));
        assert_eq!(tab.dir_of(0), Some(VfsPath::local("/root/one")));
        assert_eq!(tab.dir_of(1), Some(VfsPath::local("/elsewhere")));
        assert_eq!(tab.path_of(99), None);
        assert_eq!(tab.dir_of(99), None);
    }

    #[test]
    fn the_operand_rules_of_f5_and_ctrl_m_differ_and_stay_two_functions() {
        // the design settles an unmarked `F5` on the cursor;
        // the first sentence gives an unmarked `Ctrl+M` the whole
        // listing. Both are right for their own key.
        let mut tab = tab_with(&["..", "a.rs", "b.rs"]);
        if let Some(first) = tab.entries.first_mut() {
            *first = Entry::parent_entry();
        }
        tab.cursor = 1;
        assert_eq!(tab.operand_rows(), vec![1]);
        assert_eq!(tab.rename_rows(), vec![1, 2], "never the `..` row");

        tab.cursor = 0;
        assert!(tab.operand_rows().is_empty(), "`..` is never an operand");
        assert!(!tab.toggle_mark(), "and is never markable");

        tab.cursor = 2;
        assert!(tab.toggle_mark());
        assert_eq!(tab.operand_rows(), vec![2]);
        assert_eq!(tab.rename_rows(), vec![2], "a mark decides both");
    }

    #[test]
    fn a_virtual_view_says_which_listing_the_tab_is_showing() {
        let mut tab = virtual_tab(&["/root/a.rs"]);
        assert!(!tab.is_virtual());
        assert!(tab.virtual_view().is_none());
        tab.virtual_view = Some(Box::new(VirtualView {
            kind: VirtualKind::Search,
            header: "[search: *.rs in /root]".to_string(),
            origin: VfsPath::local("/root"),
            origin_cursor: Some("a.rs".to_string()),
            listing: crate::vfs::list::ListingId(1),
            find: None,
        }));
        assert!(tab.is_virtual());
        let view = tab.virtual_view().expect("a view");
        assert_eq!(view.kind.id(), "search");
        assert_eq!(view.listing.to_path(), tab.path);
        assert_eq!(VirtualKind::Branch.id(), "branch");
    }

    // ------------------------------------------------------------ sorts ----

    #[test]
    fn sorting_is_stable_so_equal_keys_keep_their_relative_order() {
        let mut tab = tab_with(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        // Every entry has size 0, so a size sort must not reorder anything.
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Size),
            reverse: false,
            secondary: None,
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.rs", "b.rs", "c.rs", "d.rs"]);
    }

    #[test]
    fn the_cursor_stays_on_the_same_entry_across_a_re_sort() {
        let mut tab = tab_with(&["alpha", "beta", "gamma", "delta"]);
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Name),
            reverse: false,
            secondary: None,
        };
        tab.sort_entries(false);
        assert!(tab.focus_name("gamma"));
        let before = tab.cursor;
        tab.apply_sort(SortKey::Column(ColumnId::Name), false); // reverses
        assert!(tab.sort.reverse);
        assert_eq!(
            tab.current().map(|e| e.name.as_str()),
            Some("gamma"),
            "the cursor follows the entry, not the row"
        );
        assert_ne!(tab.cursor, before, "and the entry really did move");
    }

    #[test]
    fn the_parent_row_sorts_first_in_both_directions() {
        let mut tab = tab_with(&["z", "a"]);
        tab.entries.push(Entry::parent_entry());
        for reverse in [false, true] {
            tab.sort = SortState {
                key: SortKey::Column(ColumnId::Name),
                reverse,
                secondary: None,
            };
            tab.sort_entries(true);
            assert_eq!(tab.entries.first().map(|e| e.is_parent), Some(true));
        }
    }

    #[test]
    fn directories_first_applies_on_top_of_every_order() {
        let mut tab = tab_with(&["z.txt", "adir/", "a.txt", "zdir/"]);
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Name),
            reverse: true,
            secondary: None,
        };
        tab.sort_entries(true);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["zdir", "adir", "z.txt", "a.txt"]);

        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["zdir", "z.txt", "adir", "a.txt"]);
    }

    #[test]
    fn unsorted_leaves_backend_order_alone_but_still_honours_the_parent_row() {
        let mut tab = tab_with(&["z", "m", "a"]);
        tab.entries.insert(2, Entry::parent_entry());
        tab.sort = SortState {
            key: SortKey::Unsorted,
            reverse: false,
            secondary: None,
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "z", "m", "a"]);
    }

    #[test]
    fn reversing_a_sort_reverses_only_the_key_you_chose() {
        // Sorting by extension descending walks the extensions z→a, but the
        // files *within* one extension keep their a→z order. Reversing both
        // reads as the filenames scrambling for no reason.
        let mut tab = tab_with(&["b.rs", "a.rs", "b.txt", "a.txt"]);

        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Ext),
            reverse: false,
            secondary: None,
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.rs", "b.rs", "a.txt", "b.txt"]);

        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Ext),
            reverse: true,
            secondary: None,
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["a.txt", "b.txt", "a.rs", "b.rs"],
            "extensions reversed, names still ascending within each"
        );
    }

    #[test]
    fn a_secondary_sort_reverses_independently_of_the_primary() {
        // "by extension, then by size, biggest first".
        let mut tab = tab_with(&["small.rs", "big.rs", "small.txt", "big.txt"]);
        for e in &mut tab.entries {
            e.size = if e.name.starts_with("big") { 100 } else { 1 };
        }
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Ext),
            reverse: false,
            secondary: Some((ColumnId::Size, true)),
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["big.rs", "small.rs", "big.txt", "small.txt"]);

        // Reversing the primary walks the extensions the other way while the
        // secondary keeps its own direction.
        tab.sort.reverse = true;
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["big.txt", "small.txt", "big.rs", "small.rs"]);
    }

    #[test]
    fn the_same_secondary_key_again_reverses_it() {
        let mut sort = SortState::BY_NAME;
        sort.apply_secondary(ColumnId::Size);
        assert_eq!(sort.secondary, Some((ColumnId::Size, false)));
        sort.apply_secondary(ColumnId::Size);
        assert_eq!(sort.secondary, Some((ColumnId::Size, true)));
        sort.apply_secondary(ColumnId::Date);
        assert_eq!(
            sort.secondary,
            Some((ColumnId::Date, false)),
            "a new key starts ascending"
        );
    }

    #[test]
    fn a_secondary_that_becomes_the_primary_is_dropped() {
        // "By size, then by size" means nothing.
        let mut sort = SortState::BY_NAME;
        sort.apply_secondary(ColumnId::Size);
        sort.apply(SortKey::Column(ColumnId::Size));
        assert_eq!(sort.secondary, None);
        // ...and setting it to the current primary does not take either.
        let mut sort = SortState::BY_NAME;
        sort.apply_secondary(ColumnId::Name);
        assert_eq!(sort.secondary, None);
    }

    #[test]
    fn unsorted_clears_the_secondary_and_the_tag_shows_the_pair() {
        let mut sort = SortState::BY_NAME;
        sort.apply(SortKey::Column(ColumnId::Ext));
        sort.apply_secondary(ColumnId::Size);
        assert_eq!(sort.indicator(true), "[ext ^ · size ^]");
        sort.apply_secondary(ColumnId::Size);
        assert_eq!(sort.indicator(true), "[ext ^ · size v]");

        sort.apply(SortKey::Unsorted);
        assert_eq!(sort.secondary, None, "unsorted is the backend's own order");
        assert_eq!(sort.indicator(true), "[unsorted]");
    }

    #[test]
    fn a_reversed_size_sort_still_names_ties_ascending() {
        // Same rule for every column: equal sizes stay a→z either way.
        let mut tab = tab_with(&["b", "a", "c"]);
        for e in &mut tab.entries {
            e.size = 10;
        }
        for reverse in [false, true] {
            tab.sort = SortState {
                key: SortKey::Column(ColumnId::Size),
                reverse,
                secondary: None,
            };
            tab.sort_entries(false);
            let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["a", "b", "c"], "reverse={reverse}");
        }
    }

    #[test]
    fn files_sort_by_size_in_both_directions() {
        let mut tab = tab_with(&["medium", "big", "small"]);
        for e in &mut tab.entries {
            e.size = match e.name.as_str() {
                "big" => 1_000_000,
                "medium" => 1_000,
                _ => 1,
            };
        }
        for (reverse, want) in [
            (false, ["small", "medium", "big"]),
            (true, ["big", "medium", "small"]),
        ] {
            tab.sort = SortState {
                key: SortKey::Column(ColumnId::Size),
                reverse,
                secondary: None,
            };
            tab.sort_entries(false);
            let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, want, "reverse={reverse}");
        }
    }

    /// The bug: sorting by size put directories in an order nobody could read,
    /// because the comparator used `Entry::size`, which for a directory is the
    /// inode size the column never shows. Every directory ties on this key
    /// now, whatever its inode says, and the name tiebreak orders them.
    #[test]
    fn a_directory_sorts_by_name_not_by_its_inode_size() {
        let mut tab = tab_with(&["carol/", "alice/", "bob/"]);
        for e in &mut tab.entries {
            e.size = match e.name.as_str() {
                "alice" => 12_288,
                "bob" => 176,
                _ => 4_096,
            };
        }
        for reverse in [false, true] {
            tab.sort = SortState {
                key: SortKey::Column(ColumnId::Size),
                reverse,
                secondary: None,
            };
            tab.sort_entries(false);
            let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(
                names,
                ["alice", "bob", "carol"],
                "inode sizes must not move a directory, reverse={reverse}"
            );
        }
    }

    #[test]
    fn a_size_sort_groups_directories_and_orders_the_files_around_them() {
        let mut tab = tab_with(&["zdir/", "adir/", "big.txt", "small.txt"]);
        for e in &mut tab.entries {
            e.size = match e.name.as_str() {
                "big.txt" => 900,
                "small.txt" => 9,
                // Inode sizes, deliberately interleaved with the file sizes so
                // a comparator that reads them would show it.
                _ => 100,
            };
        }
        // Directories ahead of the files ascending, behind them reversed: the
        // whole group moves, and the names inside it never do.
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Size),
            reverse: false,
            secondary: None,
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir", "zdir", "small.txt", "big.txt"]);

        tab.sort.reverse = true;
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["big.txt", "small.txt", "adir", "zdir"]);

        // With `directories_first` the grouping is decided before the key is
        // consulted at all, so the directories stay on top either way.
        for reverse in [false, true] {
            tab.sort.reverse = reverse;
            tab.sort_entries(true);
            let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(&names[..2], ["adir", "zdir"], "reverse={reverse}");
        }
    }

    #[test]
    fn a_secondary_key_still_beats_the_name_tiebreak_among_directories() {
        // Every directory ties on size, so without a secondary these come out
        // a to z. With one, the secondary decides and the names follow it.
        let mut tab = tab_with(&["adir/", "bdir/", "cdir/"]);
        let base = std::time::SystemTime::UNIX_EPOCH;
        for e in &mut tab.entries {
            let secs = match e.name.as_str() {
                "adir" => 300,
                "bdir" => 100,
                _ => 200,
            };
            e.mtime = Some(base + std::time::Duration::from_secs(secs));
        }
        tab.sort = SortState {
            key: SortKey::Column(ColumnId::Size),
            reverse: false,
            secondary: Some((ColumnId::Date, false)),
        };
        tab.sort_entries(false);
        let names: Vec<&str> = tab.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["bdir", "cdir", "adir"], "oldest first");
    }

    #[test]
    fn ctrl_n_addresses_the_configured_order_not_the_rendered_one() {
        let order = [
            ColumnId::Name,
            ColumnId::Ext,
            ColumnId::Size,
            ColumnId::Date,
            ColumnId::Attr,
        ];
        assert_eq!(
            SortState::column_for_key(&order, 3),
            Some(ColumnId::Size),
            "Ctrl+3 is the third configured column"
        );
        // Put size third and Ctrl+3 sorts by size with nothing rebound.
        let reordered = [ColumnId::Name, ColumnId::Date, ColumnId::Size];
        assert_eq!(
            SortState::column_for_key(&reordered, 3),
            Some(ColumnId::Size)
        );
        assert_eq!(
            SortState::column_for_key(&order, 9),
            None,
            "beyond the configured count is a no-op with a message"
        );
        assert_eq!(SortState::column_for_key(&order, 0), None);
    }

    #[test]
    fn the_header_arrow_and_the_status_tag_agree() {
        let sort = SortState {
            key: SortKey::Column(ColumnId::Ext),
            reverse: true,
            secondary: None,
        };
        assert_eq!(sort.header_text(ColumnId::Ext, false), "\u{25BC}Ext");
        assert_eq!(sort.header_text(ColumnId::Name, false), "Name");
        assert_eq!(sort.header_text(ColumnId::Ext, true), "vExt");
        assert_eq!(sort.indicator(false), "[ext \u{25BC}]");
        let unsorted = SortState {
            key: SortKey::Unsorted,
            reverse: false,
            secondary: None,
        };
        assert_eq!(unsorted.arrow(false), "");
        assert_eq!(unsorted.indicator(false), "[unsorted]");
    }

    #[test]
    fn sort_state_belongs_to_the_tab_so_switching_tabs_restores_it() {
        let mut p = Panel::new(Side::Left, VfsPath::local("/a"));
        assert!(p.open_tab(VfsPath::local("/b"), 9));
        p.sort_active_tab(SortKey::Column(ColumnId::Size), true);
        assert_eq!(p.active_tab().sort.key, SortKey::Column(ColumnId::Size));
        assert!(p.select_tab(0));
        assert_eq!(
            p.active_tab().sort.key,
            SortKey::Column(ColumnId::Name),
            "the first tab kept its own order"
        );
        assert!(p.select_tab(1));
        assert_eq!(p.active_tab().sort.key, SortKey::Column(ColumnId::Size));
    }

    // ----------------------------------------------------------- cursor ----

    #[test]
    fn the_cursor_is_clamped_on_every_entry_list_change() {
        let mut tab = tab_with(&["a", "b", "c", "d", "e"]);
        tab.move_last(3);
        assert_eq!(tab.cursor, 4);
        tab.entries.truncate(2);
        tab.clamp_cursor();
        assert_eq!(tab.cursor, 1);
        tab.entries.clear();
        tab.clamp_cursor();
        assert_eq!(tab.cursor, 0);
        assert_eq!(tab.scroll, 0);
        assert_eq!(tab.current(), None);
        assert_eq!(tab.visible_range(10), 0..0);
    }

    #[test]
    fn cursor_movement_survives_a_zero_row_viewport_and_an_empty_listing() {
        let mut tab = Tab::new(VfsPath::local("/x"));
        for rows in [0usize, 1, 40] {
            tab.move_by(1, rows);
            tab.move_by(-1, rows);
            tab.page_down(rows);
            tab.page_up(rows);
            tab.move_first(rows);
            tab.move_last(rows);
            assert_eq!(tab.cursor, 0);
            assert_eq!(tab.scroll, 0);
        }
        tab.entries = (0..5).map(|i| Entry::file(format!("f{i}"))).collect();
        tab.move_last(0);
        assert_eq!(tab.cursor, 4);
        assert_eq!(tab.scroll, 0, "a zero-row viewport cannot scroll");
        assert_eq!(tab.visible_range(0), 0..0);
    }

    #[test]
    fn scrolling_keeps_the_cursor_inside_the_viewport_at_every_size() {
        let mut tab = tab_with(&["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        for rows in 1..=12usize {
            for target in 0..12usize {
                tab.move_to(target, rows);
                let range = tab.visible_range(rows);
                assert!(
                    range.contains(&tab.cursor) || tab.entries.is_empty(),
                    "rows {rows}, target {target}: cursor {} not in {range:?}",
                    tab.cursor
                );
                assert!(tab.scroll.saturating_add(rows) <= tab.entries.len() || tab.scroll == 0);
            }
        }
    }

    #[test]
    fn a_resize_that_shrinks_the_viewport_repairs_the_scroll_offset() {
        let mut p = Panel::new(Side::Left, VfsPath::local("/x"));
        p.active_tab_mut().entries = (0..50).map(|i| Entry::file(format!("f{i}"))).collect();
        p.view_rows = 20;
        p.active_tab_mut().move_last(20);
        assert_eq!(p.active_tab().scroll, 30);
        p.view_rows = 5;
        p.reclamp();
        assert_eq!(p.active_tab().scroll, 45);
        assert!(
            p.active_tab()
                .visible_range(5)
                .contains(&p.active_tab().cursor)
        );
    }

    // ------------------------------------------------------------ marks ----

    #[test]
    fn marking_skips_the_parent_row_and_survives_a_re_read() {
        let mut tab = tab_with(&["a", "b", "c"]);
        tab.entries.insert(0, Entry::parent_entry());
        assert!(!tab.toggle_mark(), "the cursor starts on [..]");
        assert!(tab.marks.is_empty());

        tab.move_to(1, 10);
        assert!(tab.toggle_mark());
        tab.move_to(2, 10);
        assert!(tab.toggle_mark());
        assert_eq!(tab.marks.len(), 2);

        // A re-read where "b" vanished keeps "a" and forgets "b".
        tab.entries = vec![Entry::parent_entry(), Entry::file("a"), Entry::file("c")];
        tab.prune_marks();
        assert!(tab.marks.contains("a"));
        assert!(!tab.marks.contains("b"));
    }

    #[test]
    fn mark_all_and_invert_never_touch_the_parent_row() {
        let mut tab = tab_with(&["a", "b"]);
        tab.entries.insert(0, Entry::parent_entry());
        tab.mark_all();
        assert_eq!(tab.marks.len(), 2);
        assert!(!tab.marks.contains(".."));
        tab.invert_marks();
        assert!(tab.marks.is_empty());
        tab.invert_marks();
        assert_eq!(tab.marks.len(), 2);
        tab.clear_marks();
        assert!(tab.marks.is_empty());
    }

    #[test]
    fn the_counts_exclude_the_parent_row() {
        let mut tab = tab_with(&["a", "b", "d/"]);
        tab.entries.insert(0, Entry::parent_entry());
        if let Some(e) = tab.entries.get_mut(1) {
            e.size = 100;
        }
        tab.marks.insert("a".to_string());
        let c = tab.counts();
        assert_eq!(c.total_files, 2);
        assert_eq!(c.total_dirs, 1);
        assert_eq!(c.marked_files, 1);
        assert_eq!(c.marked_bytes, 100);
        assert_eq!(c.total_bytes, 100);
    }

    // ------------------------------------------------------------- tabs ----

    #[test]
    fn the_tab_bar_labels_are_numbered_and_cropped() {
        let mut p = Panel::new(Side::Left, VfsPath::local("/home/thorin/Arcade/Leap/stl"));
        assert!(p.open_tab(VfsPath::local("/home/thorin/a-very-long-directory-name"), 9));
        let labels = p.tab_bar_labels(40, false);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels.first().map(|l| l.text.as_str()), Some("1 stl"));
        assert_eq!(labels.first().map(|l| l.active), Some(false));
        assert_eq!(labels.get(1).map(|l| l.active), Some(true));
        let second = labels.get(1).map(|l| l.text.clone()).unwrap_or_default();
        assert!(second.starts_with("2 a-very"), "{second}");
        assert!(second.ends_with('\u{2026}'), "{second}");
        assert!(text::width(&second) <= 20, "{second}");

        assert!(p.tab_bar_labels(0, false).is_empty());
    }

    #[test]
    fn nine_tabs_is_the_maximum_and_the_tenth_is_refused() {
        let mut p = Panel::new(Side::Left, VfsPath::local("/"));
        for i in 1..9 {
            assert!(p.open_tab(VfsPath::local(format!("/d{i}")), 9), "tab {i}");
        }
        assert_eq!(p.tab_count(), 9);
        assert!(
            !p.open_tab(VfsPath::local("/d9"), 9),
            "the tenth is refused so the caller can post a message"
        );
        assert_eq!(p.tab_count(), 9);
    }

    #[test]
    fn a_max_tabs_above_nine_is_clamped_rather_than_honoured() {
        // the design makes nine an invariant, not a default: Alt+1..Alt+9 is
        // the whole tab keyspace, and `state::restore` truncates to nine, so a
        // tenth tab could neither be selected nor survive a restart.
        let mut p = Panel::new(Side::Left, VfsPath::local("/"));
        for i in 1..20 {
            p.open_tab(VfsPath::local(format!("/d{i}")), 20);
        }
        assert_eq!(p.tab_count(), MAX_TABS);
    }

    #[test]
    fn the_tab_bar_never_drops_the_active_tab() {
        // "each tab's directory name, truncated with …, the active
        // one highlighted". At the documented maximum of nine tabs a 48-cell
        // panel cannot show them all, so the bar scrolls - a bar that dropped
        // the active tab would highlight nothing at all.
        let mut p = Panel::new(Side::Left, VfsPath::local("/"));
        for i in 1..MAX_TABS {
            assert!(p.open_tab(VfsPath::local(format!("/dir{i}")), MAX_TABS));
        }
        assert_eq!(p.tab_count(), MAX_TABS);

        for width in 1..=120usize {
            for active in 0..MAX_TABS {
                assert!(p.select_tab(active));
                let labels = p.tab_bar_labels(width, false);
                assert!(!labels.is_empty(), "width {width}");
                assert!(
                    labels.iter().any(|l| l.active),
                    "width {width}, active tab {active} is not in the bar"
                );
                // Labels plus one separator between each pair fit the bar.
                let used: usize = labels.iter().map(|l| text::width(&l.text)).sum::<usize>()
                    + labels.len().saturating_sub(1);
                assert!(used <= width, "width {width}: bar needs {used}");
                // The numbering is the tab's real index, so a scrolled bar
                // says so.
                for label in &labels {
                    assert!(
                        label.text.starts_with(&format!("{}", label.index + 1)),
                        "{label:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn replace_tabs_refuses_to_break_the_never_empty_invariant() {
        let mut p = Panel::new(Side::Left, VfsPath::local("/home"));
        assert!(!p.replace_tabs(Vec::new(), 0));
        assert_eq!(p.tab_count(), 1);
        assert!(p.replace_tabs(vec![Tab::new(VfsPath::local("/a"))], 40));
        assert_eq!(p.active_index(), 0, "the active index is clamped");
    }

    #[test]
    fn tabs_are_bounded_and_never_empty() {
        let mut p = Panel::new(Side::Left, VfsPath::local_root());
        assert!(!p.close_tab(), "the last tab cannot be closed");
        assert!(p.open_tab(VfsPath::local("/tmp"), 2));
        assert!(
            !p.open_tab(VfsPath::local("/var"), 2),
            "max_tabs is honoured"
        );
        assert_eq!(p.active_index(), 1);
        assert!(p.close_tab());
        assert_eq!(p.tab_count(), 1);
        assert!(!p.select_tab(7), "a tab that does not exist is a no-op");
    }

    #[test]
    fn no_sequence_of_public_calls_can_empty_a_panel_or_strand_its_active_index() {
        // `Panel::active_tab`/`active_tab_mut` are *total*, and the one
        // `unreachable!` left in `src/` (the design judgement call 10)
        // rests on that. This drives every mutator that touches `tabs` or
        // `active` and asserts the invariant after each, so "provably
        // unreachable" is tested rather than argued.
        let mut p = Panel::new(Side::Left, VfsPath::local_root());
        let mut q = Panel::new(Side::Right, VfsPath::local("/tmp"));

        let check = |p: &Panel| {
            assert!(p.tab_count() >= 1, "a panel always has a tab");
            assert!(
                p.active_index() < p.tab_count(),
                "active {} is out of range of {} tabs",
                p.active_index(),
                p.tab_count()
            );
        };

        check(&p);
        // Closing the only tab is refused.
        assert!(!p.close_tab());
        check(&p);
        // Opening past the limit is refused.
        for n in 0..12 {
            p.open_tab(VfsPath::local(format!("/t{n}")), 9);
            check(&p);
        }
        assert_eq!(p.tab_count(), 9, "nine is the cap");
        // Selecting out of range is refused and leaves `active` alone.
        assert!(!p.select_tab(99));
        check(&p);
        assert!(p.select_tab(8));
        check(&p);
        // Closing from the last index walks `active` back.
        while p.close_tab() {
            check(&p);
        }
        assert_eq!(p.tab_count(), 1);
        // An empty replacement is refused.
        assert!(!p.replace_tabs(Vec::new(), 0));
        check(&p);
        // An over-large active index is clamped.
        assert!(p.replace_tabs(vec![Tab::new(VfsPath::local("/a"))], 77));
        check(&p);
        // Swapping carries `tabs` and `active` together.
        q.open_tab(VfsPath::local("/b"), 9);
        q.open_tab(VfsPath::local("/c"), 9);
        p.swap_contents(&mut q);
        check(&p);
        check(&q);
        // And the total accessors work in every one of those states.
        let _ = p.active_tab();
        let _ = p.active_tab_mut();
        let _ = q.active_tab();
        let _ = q.active_tab_mut();
    }

    #[test]
    fn a_streaming_sort_is_bounded_but_the_final_order_is_not_negotiable() {
        // A full sort per arriving batch is quadratic in the row count, and
        // the design puts no bound on that count. `Tab::sort_streaming` sorts
        // every batch of a small listing - every real directory - and puts a
        // large one on a doubling schedule, so the cost is `O(log n)` sorts
        // rather than `O(n / batch)`.
        let mut tab = Tab::new(VfsPath::local("/x"));

        // Small: every batch re-sorts, exactly as it always did, so a
        // directory looks sorted while it fills.
        for i in 0..8 {
            tab.entries.push(Entry::file(format!("f{:03}", 100 - i)));
            tab.sort_streaming(false);
            assert_eq!(tab.sorted_rows, tab.entries.len(), "row {i}");
        }
        assert!(
            tab.entries.windows(2).all(|w| w[0].name <= w[1].name),
            "and it is in order at every step"
        );

        // Large: a batch that does not double the listing is appended without
        // a re-sort, and the panel is never more than one doubling behind.
        tab.clear_entries();
        for i in 0..SORT_EVERY_BATCH_BELOW * 3 {
            tab.entries.push(Entry::file(format!("f{i:06}")));
            tab.sort_streaming(false);
        }
        assert!(
            tab.sorted_rows < tab.entries.len(),
            "the tail is not re-sorted per batch, got {} of {}",
            tab.sorted_rows,
            tab.entries.len()
        );
        assert!(
            tab.sorted_rows.saturating_mul(2) > tab.entries.len(),
            "and never more than one doubling behind, got {}",
            tab.sorted_rows
        );

        // `clear_entries` is what stops a stale count from making the *next*
        // listing skip its first sorts.
        tab.clear_entries();
        assert_eq!(tab.sorted_rows, 0);
        tab.entries.push(Entry::file("z"));
        tab.entries.push(Entry::file("a"));
        tab.sort_streaming(false);
        assert_eq!(
            tab.entries.first().map(|e| e.name.as_str()),
            Some("a"),
            "a fresh listing sorts from its first batch"
        );
    }
}
