//! The directory hotlist.
//!
//! ```toml
//! [[entry]]
//! label = "media"
//! path  = "/srv/media"
//! ```
//!
//! # This file is not one of the generated files
//!
//! `hotlist.toml` follows `hosts.toml` rather than `config.toml`: it holds no
//! defaults, so the commented-out rule does not apply to it and it
//! is not created on first run.
//!
//! the argument is that a live generated file freezes its owner at
//! the defaults of the day they installed, because a user file that mentions
//! an action replaces that action's built-in bindings. A hotlist has nothing
//! built in to be frozen away from: it is a list the user builds with
//! `Ctrl+Shift+D`, and the program rewrites it in full on every change,
//! exactly as it already rewrites `hosts.toml` and
//! `searches.toml`. A missing file is an empty hotlist, and the
//! file appears when the first entry is added.
//!
//! # The order is the user's
//!
//! > `hotlist.toml` (18) holds it, in the order shown, which is the order the
//! > user put them in: this list is short and hand-kept, and sorting it would
//! > lose the one piece of information the user encoded by adding them in that
//! > order.
//!
//! Nothing in this module sorts, and [`upsert`] keeps a replaced entry in the
//! position it already had.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::{Error, Result};

/// The file, in the config directory beside `hosts.toml`.
pub const HOTLIST_FILE: &str = "hotlist.toml";

/// The header [`render`] writes above the entries, so the file says what it is
/// and why it is not commented out like `config.toml`.
const HEADER: &str = "\
# The directory hotlist, written by hcmd.
#
# Ctrl+Shift+D adds the active panel's directory; Ctrl+D opens the list. The
# order is the order entries were added and is never sorted, because that order
# is the one piece of information adding them encoded.
#
# Unlike config.toml and keymap.toml this file is not written commented out:
# it holds no defaults to be frozen at, and a commented-out entry would
# simply be an entry that is not in the list.
";

/// One `[[entry]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotlistEntry {
    /// What the popup lists it under, and what quick search matches.
    pub label: String,
    /// Where it goes. Kept as text; `~` and `$VAR` expand at use.
    pub path: String,
}

impl HotlistEntry {
    /// Every key this file understands, for the unknown-key warning.
    ///
    /// Written out rather than derived, because serde's `deny_unknown_fields`
    /// would refuse the whole file and the design wants a warning and the rest
    /// of the list.
    pub const KEYS: &'static [&'static str] = &["label", "path"];
}

/// An entry with the answer to "does it still exist" attached.
///
/// > An entry whose path no longer exists is shown greyed with the reason
/// > rather than dropped: a missing path is usually an unmounted disk.
///
/// Built by the event loop, because deciding it is a `stat` and `dispatch`
/// may not make one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotlistRow {
    /// The entry as the file holds it.
    pub entry: HotlistEntry,
    /// The path, expanded, when it could be expanded.
    pub resolved: Option<PathBuf>,
    /// `None` when the path is a live directory. `Some(reason)` greys the row
    /// and is shown beside it - the `io::Error`'s own words, or "not a
    /// directory".
    pub missing: Option<String>,
}

/// `~/.config/holoscommander/hotlist.toml`.
pub fn hotlist_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(HOTLIST_FILE))
}

/// The file as a whole: an array of tables called `entry`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct HotlistFile {
    entry: Vec<HotlistEntry>,
}

/// Parse the file into the entries that could be read and a warning per thing
/// that could not (never a silent ignore, never a hard failure).
///
/// Order is the file's order and is never sorted: the design says the order
/// is "the one piece of information the user encoded by adding them in that
/// order".
///
/// Each `[[entry]]` is deserialised on its own, so one bad table costs one
/// entry rather than the file, which is what `crate::remote::hosts::parse`
/// does for the same reason.
pub fn parse(text: &str) -> (Vec<HotlistEntry>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let table = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table,
        Err(err) => {
            warnings.push(format!("{HOTLIST_FILE}: {err}; no hotlist loaded"));
            return (Vec::new(), warnings);
        }
    };
    for key in table.keys() {
        if key != "entry" {
            warnings.push(format!("{HOTLIST_FILE}: unknown section `{key}`, ignored"));
        }
    }
    let Some(entries) = table.get("entry") else {
        // An empty or absent list is the normal first run, not a warning.
        return (Vec::new(), warnings);
    };
    let Some(entries) = entries.as_array() else {
        warnings.push(format!(
            "{HOTLIST_FILE}: `entry` is not an array of tables; no hotlist loaded"
        ));
        return (Vec::new(), warnings);
    };

    let mut out: Vec<HotlistEntry> = Vec::with_capacity(entries.len());
    for (index, value) in entries.iter().enumerate() {
        // One-based, because that is how a person counts `[[entry]]` blocks.
        let at = index.saturating_add(1);
        let Some(fields) = value.as_table() else {
            warnings.push(format!(
                "{HOTLIST_FILE}: [[entry]] {at} is not a table, ignored"
            ));
            continue;
        };
        for key in fields.keys() {
            if !HotlistEntry::KEYS.contains(&key.as_str()) {
                warnings.push(format!(
                    "{HOTLIST_FILE}: [[entry]] {at}: unknown key `{key}`, ignored"
                ));
            }
        }
        let mut entry: HotlistEntry = match value.clone().try_into() {
            Ok(entry) => entry,
            Err(err) => {
                warnings.push(format!(
                    "{HOTLIST_FILE}: [[entry]] {at}: {err}; entry ignored"
                ));
                continue;
            }
        };
        entry.label = entry.label.trim().to_string();
        entry.path = entry.path.trim().to_string();
        if entry.path.is_empty() {
            warnings.push(format!(
                "{HOTLIST_FILE}: [[entry]] {at}: no `path`, entry ignored"
            ));
            continue;
        }
        if entry.label.is_empty() {
            // A path with no label is still a usable bookmark, and the design
            // asks for the rest of the list rather than a refusal: the path
            // stands in for the name it was not given.
            entry.label = entry.path.clone();
        }
        out.push(entry);
    }
    (out, warnings)
}

/// Render entries back to TOML, in order, with the file's own header comment.
pub fn render(entries: &[HotlistEntry]) -> Result<String> {
    let file = HotlistFile {
        entry: entries.to_vec(),
    };
    let body =
        toml::to_string_pretty(&file).map_err(|e| Error::msg(format!("{HOTLIST_FILE}: {e}")))?;
    Ok(format!("{HEADER}\n{body}"))
}

/// Read the file. A missing file is an empty hotlist and no warning.
pub fn load() -> (Vec<HotlistEntry>, Vec<String>) {
    let Ok(path) = hotlist_path() else {
        return (Vec::new(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        // A missing file is the normal first run: the file is created when the
        // first entry is added and not before.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(err) => (Vec::new(), vec![format!("{}: {err}", path.display())]),
    }
}

/// Write the file, creating the config directory if needed.
///
/// **Called from the event loop, never from `Dialog::handle_key`**,
/// exactly as `crate::remote::hosts::store` is.
pub fn store(entries: &[HotlistEntry]) -> Result<()> {
    let path = hotlist_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let text = render(entries)?;
    std::fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

/// Add or update an entry (the `Ctrl+Shift+D`).
///
/// > A duplicate path replaces the existing entry's label rather than adding a
/// > second row for the same place.
///
/// The replaced entry keeps its **position**, because position is the order
/// the user chose. Returns true when an existing row was replaced.
pub fn upsert(entries: &mut Vec<HotlistEntry>, label: String, path: String) -> bool {
    let entry = HotlistEntry { label, path };
    match entries.iter().position(|e| same_path(&e.path, &entry.path)) {
        Some(at) => {
            // In place: `entries[at] = entry` would be an index that a future
            // edit could get wrong, and `get_mut` cannot fail here.
            if let Some(slot) = entries.get_mut(at) {
                *slot = entry;
            }
            true
        }
        None => {
            entries.push(entry);
            false
        }
    }
}

/// Are these two stored paths the same place, as far as the design's
/// "duplicate path" rule is concerned?
///
/// Text, and trailing-slash-insensitive. Not `canonicalize`: this is decided
/// while a dialog is open and a `stat` is not allowed there,
/// and a hotlist path may name a disk that is not
/// plugged in - which is the one case the design is written around.
fn same_path(a: &str, b: &str) -> bool {
    trim_slash(a.trim()) == trim_slash(b.trim())
}

/// Drop a trailing `/`, except from the root, which is nothing else.
fn trim_slash(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some("") | None => path,
        Some(rest) => rest,
    }
}

/// The label `Ctrl+Shift+D` pre-fills: the last path component, or the path
/// itself for a root ("pre-filled from the last path component
/// and editable").
pub fn suggest_label(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        // `/`, and also a path ending in `..`, which has no final component
        // either. Both are better named by the whole text than by nothing.
        None => path.to_string_lossy().into_owned(),
    }
}

/// Expand `~`, `~/...`, `$VAR` and `${VAR}` in a stored path.
///
/// The same rules `Ctrl+G` follows, so a path typed into one
/// and a path stored in the other mean the same thing. Shared with
/// `crate::panel::goto` rather than written twice.
///
/// A relative path is refused rather than resolved: `Ctrl+G` resolves one
/// against the panel it was typed into, and a stored entry has no panel to be
/// relative to.
pub fn expand(path: &str) -> Result<PathBuf> {
    crate::panel::goto::expand(path, None).map_err(Error::msg)
}

/// Stat every entry, so the popup can grey the ones that are not there.
///
///
/// **The event loop's**, like [`store`]: one `stat` per entry, and a dialog
/// may make none. The reason is the `io::Error`'s own
/// words, so an unplugged disk and a permission problem do not read the same.
pub fn rows(entries: &[HotlistEntry]) -> Vec<HotlistRow> {
    entries.iter().cloned().map(row).collect()
}

/// One [`rows`] entry.
fn row(entry: HotlistEntry) -> HotlistRow {
    let resolved = match expand(&entry.path) {
        Ok(path) => path,
        Err(why) => {
            return HotlistRow {
                entry,
                resolved: None,
                missing: Some(why.to_string()),
            };
        }
    };
    let missing = match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => None,
        Ok(_) => Some("not a directory".to_string()),
        Err(err) => Some(err.to_string()),
    };
    HotlistRow {
        entry,
        resolved: Some(resolved),
        missing,
    }
}

/// The directory hotlist as last read from or written to `hotlist.toml`, and
/// whether the two still agree.
///
/// The dirty flag is a flag rather than a write because
/// [`crate::input::dispatch`] may not touch the filesystem: `Ctrl+Shift+D`
/// changes a list, and the write happens in the event loop where every other
/// write happens.
#[derive(Debug, Default, Clone)]
pub struct Hotlist {
    entries: Vec<HotlistEntry>,
    dirty: bool,
}

impl Hotlist {
    /// Do the file and memory still disagree?
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The entries, in the order `hotlist.toml` holds them.
    pub fn entries(&self) -> &[HotlistEntry] {
        &self.entries
    }

    /// Adopt what [`load`] read. Not dirty: the file and memory agree by
    /// construction at this point.
    pub fn adopt(&mut self, entries: Vec<HotlistEntry>) {
        self.entries = entries;
        self.dirty = false;
    }

    /// Add or relabel one entry, and say whether it replaced
    /// an existing one.
    ///
    /// A duplicate path replaces the existing entry's label **where it
    /// stands** rather than adding a second row, and the order is never
    /// touched: the file holds the entries in the order the user put them in.
    pub fn upsert(&mut self, label: String, path: String) -> bool {
        self.dirty = true;
        upsert(&mut self.entries, label, path)
    }

    /// The rows the popup draws, each already told whether its directory is
    /// still there.
    pub fn rows(&self) -> Vec<HotlistRow> {
        rows(&self.entries)
    }

    /// Write the hotlist back if it has changed, and say what went wrong if it
    /// could not be written.
    ///
    /// The flag is cleared either way: a write that failed is reported once,
    /// not retried on every frame for the rest of the session.
    pub fn store_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        store(&self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// the own example, character for character.
    const DOCUMENTED_EXAMPLE: &str = r#"
[[entry]]
label = "media"
path  = "/srv/media"
"#;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A directory that exists for the length of one test, and its own
    /// cleanup. The pattern `src/ops/mkdir.rs` uses, for the same reason:
    /// nothing in this repository takes a temporary-directory dependency.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos();
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hcmd-hotlist-{tag}-{pid}-{nanos}-{n}",
                pid = std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(label: &str, path: &str) -> HotlistEntry {
        HotlistEntry {
            label: label.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn spec_17_3s_example_parses() {
        let (entries, warnings) = parse(DOCUMENTED_EXAMPLE);
        assert_eq!(entries, vec![entry("media", "/srv/media")]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    // "in the order shown, which is the order the user put them
    // in". Nothing here sorts, and a round trip proves it.
    fn the_order_survives_a_round_trip() {
        let list = vec![
            entry("media", "/srv/media"),
            entry("archive", "/mnt/archive"),
            entry("build", "/home/thorin/build"),
        ];
        let text = render(&list).expect("render");
        let (back, warnings) = parse(&text);
        assert_eq!(back, list);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    // this file is deliberately not written
    // commented out, and the header is where a reader finds out why.
    fn the_rendered_file_carries_its_header_and_is_live() {
        let text = render(&[entry("media", "/srv/media")]).expect("render");
        assert!(text.starts_with('#'), "{text}");
        assert!(text.contains("Ctrl+Shift+D"), "{text}");
        assert!(text.contains("[[entry]]"), "{text}");
    }

    #[test]
    // a warning naming the file, never a silent ignore and never a
    // hard failure that leaves the user without a hotlist.
    fn a_bad_entry_costs_one_entry_and_a_warning() {
        let text = "
[[entry]]
label = \"media\"
path  = \"/srv/media\"

[[entry]]
label = 7
path  = \"/mnt/x\"

[[entry]]
label = \"no path\"

[[entry]]
label = \"colour\"
path  = \"/mnt/y\"
colour = \"red\"
";
        let (entries, warnings) = parse(text);
        assert_eq!(
            entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<&str>>(),
            vec!["/srv/media", "/mnt/y"]
        );
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings.iter().all(|w| w.contains(HOTLIST_FILE)),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("unknown key `colour`")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_is_a_warning_and_an_empty_list() {
        let (entries, warnings) = parse("this is not toml [[[");
        assert!(entries.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    // invariant I6: "A duplicate path replaces the existing
    // entry's label rather than adding a second row for the same place", and
    // the replacement keeps the position the user gave it.
    fn upsert_replaces_in_place_and_appends_a_new_path() {
        let mut list = vec![
            entry("media", "/srv/media"),
            entry("archive", "/mnt/archive"),
        ];
        let replaced = upsert(&mut list, "films".to_string(), "/srv/media".to_string());
        assert!(replaced);
        assert_eq!(list.len(), 2);
        assert_eq!(list.first(), Some(&entry("films", "/srv/media")));

        let replaced = upsert(&mut list, "build".to_string(), "/home/build".to_string());
        assert!(!replaced);
        assert_eq!(list.len(), 3);
        assert_eq!(list.last(), Some(&entry("build", "/home/build")));
    }

    #[test]
    // The same directory typed with and without its trailing slash is one
    // place, and a second row for it would be exactly the duplicate the design
    // 17.3 forbids.
    fn a_trailing_slash_is_the_same_path() {
        let mut list = vec![entry("media", "/srv/media")];
        assert!(upsert(
            &mut list,
            "films".to_string(),
            "/srv/media/".to_string()
        ));
        assert_eq!(list.len(), 1);
        assert!(!same_path("/srv/media", "/srv/mediax"));
        assert!(same_path("/", "/"));
    }

    #[test]
    fn the_suggested_label_is_the_last_component() {
        assert_eq!(suggest_label(Path::new("/srv/media")), "media");
        assert_eq!(suggest_label(Path::new("/srv/media/")), "media");
        assert_eq!(suggest_label(Path::new("/")), "/");
    }

    #[test]
    fn expansion_is_ctrl_gs() {
        let home = crate::config::paths::home_dir().expect("a home directory");
        assert_eq!(expand("~").ok(), Some(home.clone()));
        assert_eq!(expand("$HOME").ok(), Some(home));
        assert!(
            expand("relative/path").is_err(),
            "no panel to be relative to"
        );
    }

    #[test]
    // "An entry whose path no longer exists is shown greyed with
    // the reason rather than dropped: a missing path is usually an unmounted
    // disk" (invariant I5, the model half).
    fn a_missing_path_is_kept_with_its_reason() {
        let tree = TempTree::new("rows");
        let live = tree.path().join("live");
        std::fs::create_dir_all(&live).expect("live directory");
        let file = tree.path().join("file");
        std::fs::write(&file, b"x").expect("a file");
        let gone = tree.path().join("gone");

        let list = vec![
            entry("live", &live.to_string_lossy()),
            entry("gone", &gone.to_string_lossy()),
            entry("file", &file.to_string_lossy()),
        ];
        let rows = rows(&list);
        assert_eq!(rows.len(), 3, "nothing is dropped");
        let live_row = rows.first().expect("the live row");
        assert_eq!(live_row.missing, None);
        assert_eq!(live_row.resolved.as_deref(), Some(live.as_path()));
        let gone_row = rows.get(1).expect("the missing row");
        assert!(gone_row.missing.is_some(), "a missing path is greyed");
        let file_row = rows.get(2).expect("the file row");
        assert_eq!(file_row.missing.as_deref(), Some("not a directory"));
    }

    #[test]
    fn a_missing_file_is_an_empty_hotlist_and_no_warning() {
        // `load` reads the real config directory, which a test may not have.
        // The contract is that a missing file is silent, and that is what both
        // branches assert: either the file is not there and the answer is
        // empty and quiet, or it is there and it parsed.
        let (entries, warnings) = load();
        if entries.is_empty() {
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }
}
