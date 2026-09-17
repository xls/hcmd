//! Per-panel tab persistence.
//!
//! Tabs are persisted to `$XDG_STATE_HOME/holoscommander/tabs.toml`, falling
//! back to `~/.local/state/holoscommander/`, and restored on start. This is
//! state, not configuration, so it lives outside `~/.config` and a user's
//! configuration can be version-controlled without churn.
//!
//! **Nothing here can fail the launch.** A missing file, an unreadable file, a
//! file that is not TOML, a file describing forty tabs or a negative index: all
//! of them degrade to [`SavedState::default`], which restores nothing and leaves
//! the panels with the one tab they were constructed with.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::state_dir;
use crate::error::Result;
use crate::panel::{ColumnId, Panel, SortKey, SortState, Tab};
use crate::vfs::VfsPath;

/// The file tabs are saved to, inside the state directory.
pub const STATE_FILE: &str = "tabs.toml";

/// One saved tab. Only what the design says a tab *is*: a path and its sort
/// order, plus the cursor so reopening lands where you left off.
///
/// Marks are deliberately **not** saved: the design clears them on directory
/// change, and a listing read fresh at start-up is exactly that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    /// The directory, as a plain path. v0.1 is local-only, so a nested
    /// [`VfsPath`] cannot occur; one is written flattened and reads back as the
    /// local path, which is the safe degradation.
    pub path: String,
    /// `"unsorted"` or a column id.
    #[serde(default = "default_sort_key")]
    pub sort: String,
    /// Descending.
    #[serde(default)]
    pub reverse: bool,
    /// Where the cursor was.
    #[serde(default)]
    pub cursor: usize,
}

fn default_sort_key() -> String {
    ColumnId::Name.id().to_string()
}

/// One saved panel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPanel {
    /// Which tab was active. Clamped on restore.
    pub active: usize,
    /// The tabs, in bar order.
    pub tabs: Vec<SavedTab>,
}

/// Both panels' tabs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    /// The left panel.
    pub left: SavedPanel,
    /// The right panel.
    pub right: SavedPanel,
}

/// Where the state file lives.
pub fn state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(STATE_FILE))
}

/// Parse a saved state from TOML text. A parse failure is not an error to the
/// caller: it is an empty state and a warning.
pub fn parse(text: &str) -> (SavedState, Vec<String>) {
    match toml::from_str::<SavedState>(text) {
        Ok(state) => (state, Vec::new()),
        Err(err) => (
            SavedState::default(),
            vec![format!(
                "{STATE_FILE}: {err}; starting with one tab per panel"
            )],
        ),
    }
}

/// Read the saved state, degrading to [`SavedState::default`] on every failure.
///
/// Returns the state and any warnings, which the caller surfaces the same way
/// as configuration warnings.
pub fn load() -> (SavedState, Vec<String>) {
    let Ok(path) = state_path() else {
        return (SavedState::default(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        // A missing file is the normal first run, not a warning.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            (SavedState::default(), Vec::new())
        }
        Err(err) => (
            SavedState::default(),
            vec![format!("{}: {err}", path.display())],
        ),
    }
}

/// The local directory a tab reopens in next session.
///
/// A virtual tab (search results, a git branch view) and a connected tab are
/// persisted as **the real directory they came from**, never as their `list:`
/// or `Remote(3)` path: those name a listing or a connection that will not
/// exist next session, and the panel would open on an error instead of on a
/// directory. Reconnecting on startup would also need credentials before the
/// UI exists.
///
/// A tab nested inside a container - an archive member, a disk image, a
/// database row - is persisted as the directory that **holds the container
/// file**, not its container-internal path: an `emp.db#sqlite/employee`
/// restored as `/employee` names nothing the kernel can open, and the panel
/// would fall back to the home directory. Reopening beside the file is where
/// the design says leaving a container lands anyway.
fn restore_directory(tab: &Tab) -> String {
    if let Some(origin) = tab
        .remote_view()
        .map(|view| &view.origin)
        .or_else(|| tab.virtual_view().map(|view| &view.origin))
    {
        return origin.tail().to_string_lossy().into_owned();
    }
    // The outermost segment is the real local path that was entered. For a
    // plain local tab it is the directory itself; for a container it is the
    // container file, whose parent is the directory to return to.
    let outer = tab.path.outermost().1.as_path();
    let dir = if tab.path.depth() > 1 {
        outer.parent().unwrap_or(outer)
    } else {
        outer
    };
    dir.to_string_lossy().into_owned()
}

/// Snapshot one panel.
pub fn snapshot(panel: &Panel) -> SavedPanel {
    SavedPanel {
        active: panel.active_index(),
        tabs: panel
            .tabs()
            .iter()
            .map(|tab| SavedTab {
                path: restore_directory(tab),
                sort: match tab.sort.key {
                    SortKey::Unsorted => "unsorted".to_string(),
                    SortKey::Column(c) => c.id().to_string(),
                },
                reverse: tab.sort.reverse,
                cursor: tab.cursor,
            })
            .collect(),
    }
}

/// Write both panels' tabs to the state file, creating the directory if needed.
pub fn save(left: &Panel, right: &Panel) -> Result<()> {
    let path = state_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::error::Error::io(dir, e))?;
    }
    let state = SavedState {
        left: snapshot(left),
        right: snapshot(right),
    };
    let text = toml::to_string_pretty(&state)
        .map_err(|e| crate::error::Error::msg(format!("serializing {STATE_FILE}: {e}")))?;
    std::fs::write(&path, text).map_err(|e| crate::error::Error::io(&path, e))
}

/// Restore one panel's tabs from a snapshot.
///
/// Returns false - and leaves the panel exactly as it was - when the snapshot
/// holds nothing usable, which is the "degrade to one tab" path. `max_tabs`
/// caps the restore, so a hand-edited file cannot exceed the nine-tab limit of
///
pub fn restore(panel: &mut Panel, saved: &SavedPanel, max_tabs: usize) -> bool {
    let limit = max_tabs.clamp(1, crate::panel::MAX_TABS);
    let tabs: Vec<Tab> = saved
        .tabs
        .iter()
        .filter(|t| !t.path.is_empty())
        .take(limit)
        .map(|saved| {
            let path = VfsPath::local(&saved.path);
            let mut tab = Tab::new(path);
            tab.sort = SortState {
                key: if saved.sort == "unsorted" {
                    SortKey::Unsorted
                } else {
                    ColumnId::from_id(&saved.sort).map_or(SortKey::default(), SortKey::Column)
                },
                reverse: saved.reverse,
                // Not persisted: a secondary sort is a working detail of the
                // moment, like the mask, not a property of the
                // folder worth restoring days later.
                secondary: None,
            };
            // The listing has not been read yet, so the cursor is a hint that
            // `Tab::clamp_cursor` will trim once entries arrive.
            tab.cursor = saved.cursor;
            tab
        })
        .collect();

    if tabs.is_empty() {
        return false;
    }
    let active = saved.active.min(tabs.len().saturating_sub(1));
    panel.replace_tabs(tabs, active);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::Side;

    fn panel_with(paths: &[&str]) -> Panel {
        let mut first = paths.iter();
        let head = first.next().copied().unwrap_or("/");
        let mut p = Panel::new(Side::Left, VfsPath::local(head));
        for path in first {
            assert!(p.open_tab(VfsPath::local(*path), 9));
        }
        p
    }

    #[test]
    fn a_round_trip_preserves_paths_sort_and_the_active_index() {
        let mut p = panel_with(&["/a", "/b", "/c"]);
        p.active_tab_mut().sort = SortState {
            key: SortKey::Column(ColumnId::Size),
            reverse: true,
            secondary: None,
        };
        assert!(p.select_tab(1));
        let saved = snapshot(&p);

        let mut restored = Panel::new(Side::Right, VfsPath::local("/"));
        assert!(restore(&mut restored, &saved, 9));
        assert_eq!(restored.tab_count(), 3);
        assert_eq!(restored.active_index(), 1);
        let paths: Vec<String> = restored.tabs().iter().map(|t| t.path.to_string()).collect();
        assert_eq!(paths, ["/a", "/b", "/c"]);
        assert_eq!(
            restored.tabs().get(2).map(|t| t.sort),
            Some(SortState {
                key: SortKey::Column(ColumnId::Size),
                reverse: true,
                secondary: None,
            })
        );
        assert_eq!(
            restored.side,
            Side::Right,
            "restoring contents never moves a panel's identity"
        );
    }

    #[test]
    fn a_corrupt_state_file_degrades_to_one_tab() {
        let (state, warnings) = parse("this is not toml {{{");
        assert!(!warnings.is_empty());
        let mut p = Panel::new(Side::Left, VfsPath::local("/home/t"));
        assert!(!restore(&mut p, &state.left, 9));
        assert_eq!(p.tab_count(), 1);
        assert_eq!(p.active_tab().path, VfsPath::local("/home/t"));
    }

    #[test]
    fn an_empty_state_file_degrades_to_one_tab() {
        let (state, warnings) = parse("");
        assert!(warnings.is_empty());
        let mut p = Panel::new(Side::Left, VfsPath::local("/home/t"));
        assert!(!restore(&mut p, &state.left, 9));
        assert_eq!(p.tab_count(), 1);
    }

    #[test]
    fn a_hand_edited_file_cannot_exceed_the_nine_tab_limit() {
        let saved = SavedPanel {
            active: 40,
            tabs: (0..40)
                .map(|i| SavedTab {
                    path: format!("/d{i}"),
                    sort: "name".to_string(),
                    reverse: false,
                    cursor: 0,
                })
                .collect(),
        };
        let mut p = Panel::new(Side::Left, VfsPath::local("/"));
        assert!(restore(&mut p, &saved, 99));
        assert_eq!(p.tab_count(), 9);
        assert_eq!(p.active_index(), 8, "the active index is clamped");
    }

    #[test]
    fn an_unknown_sort_column_falls_back_to_the_default() {
        let saved = SavedPanel {
            active: 0,
            tabs: vec![SavedTab {
                path: "/a".to_string(),
                sort: "wibble".to_string(),
                reverse: false,
                cursor: 0,
            }],
        };
        let mut p = Panel::new(Side::Left, VfsPath::local("/"));
        assert!(restore(&mut p, &saved, 9));
        assert_eq!(p.active_tab().sort.key, SortKey::default());
    }

    #[test]
    fn a_tab_inside_a_container_is_saved_beside_the_container_file() {
        use crate::vfs::BackendKind;
        // Sitting inside a database at `/home/t/emp.db`, on the table `employee`.
        // The saved path is the directory holding the file, not the
        // container-internal `/employee`, which names nothing on restart.
        let inside =
            VfsPath::local("/home/t/emp.db").with_segment(BackendKind::Sqlite, "/employee");
        let p = Panel::new(Side::Left, inside);
        let saved = snapshot(&p);
        assert_eq!(
            saved.tabs.first().map(|t| t.path.as_str()),
            Some("/home/t"),
            "a container tab reopens in the folder the file is in"
        );

        // And it restores to a real local directory, not the bogus inner path.
        let mut restored = Panel::new(Side::Right, VfsPath::local("/"));
        assert!(restore(&mut restored, &saved, 9));
        assert_eq!(
            restored.active_tab().path,
            VfsPath::local("/home/t"),
            "restored to the file's directory"
        );
    }

    #[test]
    fn a_deeply_nested_container_still_saves_the_outermost_directory() {
        use crate::vfs::BackendKind;
        // An archive inside an archive: the real local file is the outer one,
        // and its directory is where the tab returns.
        let nested = VfsPath::local("/data/pkgs/outer.zip")
            .with_segment(BackendKind::Archive, "/inner.zip")
            .with_segment(BackendKind::Archive, "/sub/file");
        let p = Panel::new(Side::Left, nested);
        let saved = snapshot(&p);
        assert_eq!(
            saved.tabs.first().map(|t| t.path.as_str()),
            Some("/data/pkgs")
        );
    }

    #[test]
    fn the_saved_form_is_toml_that_reads_back() {
        let p = panel_with(&["/a", "/b"]);
        let state = SavedState {
            left: snapshot(&p),
            right: SavedPanel::default(),
        };
        let text = toml::to_string_pretty(&state).expect("serializes");
        let (back, warnings) = parse(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back.left.tabs.len(), 2);
        assert!(back.right.tabs.is_empty());
    }
}
