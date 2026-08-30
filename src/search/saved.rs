//! `searches.toml` and the Find dialog's history.
//!
//! the Load/Save tab is "named saved searches, so a frequently repeated query
//! is one selection away. Stored in `searches.toml`". the design lists four
//! configuration files and this is not one of them, which the design reports as
//! an incomplete list rather than a closed one and resolves here:
//!
//! * [`SAVED_FILE`] lives in the **config** directory beside `hotlist.toml`,
//!   because a named saved search is a thing the user curates and would want
//!   version-controlled - which is the own criterion for what
//!   belongs there.
//! * [`HISTORY_FILE`] lives in the **state** directory, because the design
//!   names history there explicitly.
//!
//! # Nothing here can fail the dialog
//!
//! A missing file, an unreadable file or a file that is not TOML all degrade to
//! an empty list and a warning, exactly as `panel::state` degrades - the Find
//! dialog opens either way. Only writing returns an error, because a `Save
//! as…` that silently did nothing would be worse than one that says why.
//!
//! # What a saved search does *not* carry
//!
//! Three of [`crate::search::Query`]'s fields are deliberately not written:
//!
//! * `restrict` is the "Only search in selected directories/files",
//!   which is the panel's current marks. Marks do not survive a directory
//!   change, so a saved one would name files that are not
//!   marked any more.
//! * `respect_gitignore` and `follow_symlinks` are `search.respect_gitignore`
//!   and `ops.follow_symlinks`. They are configuration, and a saved search
//!   that pinned them would
//!   quietly override the config file the user edited afterwards.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::units::ByteSize;
use crate::config::{config_dir, state_dir};
use crate::error::{Error, Result};
use crate::search::query::{
    AttrFilter, Charsets, ContentQuery, DateRange, Depth, NameMode, Query, SizeRange, TextMode, Tri,
};
use crate::vfs::VfsPath;

/// the Load/Save tab, in the config directory.
pub const SAVED_FILE: &str = "searches.toml";

/// The combo boxes' history, in the state directory.
pub const HISTORY_FILE: &str = "search-history.toml";

/// How many entries each history list keeps.
pub const MAX_HISTORY: usize = 32;

/// The date format the Advanced tab types and this file writes.
///
/// One spelling, here, so the field's parser and the file's writer cannot
/// disagree about what `2026-08-27` means.
pub const DATE_FORMAT: &str = "%Y-%m-%d";

// ------------------------------------------------------------- searches -----

/// One named search, as `searches.toml` holds it.
///
/// The query is flattened, so one entry is one TOML table with no nesting:
///
/// ```toml
/// [[search]]
/// name   = "rust TODOs"
/// mask   = "*.rs"
/// roots  = ["/home/thorin/dev"]
/// text   = "TODO"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSearch {
    /// What the Load/Save tab lists it under.
    pub name: String,
    /// Everything [`Query`] holds, flattened into TOML scalars.
    #[serde(flatten)]
    pub query: SavedQuery,
}

impl SavedSearch {
    /// A named search from a query the dialog collected.
    pub fn new(name: impl Into<String>, query: &Query) -> Self {
        Self {
            name: name.into(),
            query: SavedQuery::from_query(query),
        }
    }
}

/// [`Query`] as TOML scalars.
///
/// Every field has a default, so a hand-written entry needs only the keys it
/// cares about and a file written by an older version still loads. A key that
/// will not parse - an unknown depth, an unspellable size - falls back to that
/// default rather than failing the load, because the Load/Save tab must not be
/// closed by one bad line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQuery {
    /// The name mask. Empty means `*`.
    pub mask: String,
    /// The `RegEx` checkbox beside it.
    pub mask_regex: bool,
    /// The search roots, as plain paths. Read back as local paths; a root that
    /// no longer exists loads and is refused at `Start search`, not at load.
    pub roots: Vec<String>,
    /// "Search archives".
    pub search_archives: bool,
    /// The "Search in subdirectories" dropdown: `none`, `all`, or `1`…`9`.
    pub depth: String,
    /// The "Find text" checkbox. **This is the `Option`**:
    /// `false` is a name-only search whatever
    /// `text` holds.
    pub find_text: bool,
    /// The text pattern, kept even when `find_text` is false so that ticking
    /// the box next time offers what was typed last time.
    pub text: String,
    /// `plain`, `regex` or `hex`.
    pub text_mode: String,
    /// `Whole words only`.
    pub whole_words: bool,
    /// `Case sensitive`.
    pub case_sensitive: bool,
    /// `Find files NOT containing the text`.
    pub inverted: bool,
    /// The `UTF-8` charset box. Ticked when nothing says otherwise, which is
    /// what [`Charsets::DEFAULT`] is.
    #[serde(default = "yes")]
    pub utf8: bool,
    /// The `UTF-16` box.
    pub utf16: bool,
    /// The `Latin-1 / windows-1252` box.
    pub latin1: bool,
    /// The `CP437 (DOS)` box.
    pub cp437: bool,
    /// `Size at least`, spelled as `config.toml` spells a size.
    pub size_min: String,
    /// `Size at most`.
    pub size_max: String,
    /// `any`, `between` or `newer`.
    pub date: String,
    /// The `after` field, `YYYY-MM-DD`.
    pub after: String,
    /// The `before` field, `YYYY-MM-DD`.
    pub before: String,
    /// The `newer than N days` field.
    pub days: u32,
    /// `Directories`: `ignore`, `yes` or `no`.
    pub attr_directories: String,
    /// `Hidden`.
    pub attr_hidden: String,
    /// `Executable`.
    pub attr_executable: String,
    /// `Symlinks`.
    pub attr_symlinks: String,
    /// `Read-only`.
    pub attr_read_only: String,
}

/// `true`, for [`SavedQuery::utf8`]'s serde default.
const fn yes() -> bool {
    true
}

impl SavedQuery {
    /// Flatten a query.
    pub fn from_query(q: &Query) -> Self {
        let content = q.content.clone();
        let charsets = content.as_ref().map_or(Charsets::DEFAULT, |c| c.charsets);
        let (date, after, before, days) = write_date(q.date);
        Self {
            mask: q.name.clone(),
            mask_regex: matches!(q.name_mode, NameMode::Regex),
            roots: q.roots.iter().map(ToString::to_string).collect(),
            search_archives: q.search_archives,
            depth: write_depth(q.depth),
            find_text: content.is_some(),
            text: content
                .as_ref()
                .map(|c| c.pattern.clone())
                .unwrap_or_default(),
            text_mode: write_text_mode(content.as_ref().map_or(TextMode::Plain, |c| c.mode))
                .to_string(),
            whole_words: content.as_ref().is_some_and(|c| c.whole_words),
            case_sensitive: content.as_ref().is_some_and(|c| c.case_sensitive),
            inverted: content.as_ref().is_some_and(|c| c.inverted),
            utf8: charsets.utf8,
            utf16: charsets.utf16,
            latin1: charsets.latin1,
            cp437: charsets.cp437,
            size_min: q
                .size
                .min
                .map(|b| ByteSize(b).to_string())
                .unwrap_or_default(),
            size_max: q
                .size
                .max
                .map(|b| ByteSize(b).to_string())
                .unwrap_or_default(),
            date,
            after,
            before,
            days,
            attr_directories: write_tri(q.attrs.directories).to_string(),
            attr_hidden: write_tri(q.attrs.hidden).to_string(),
            attr_executable: write_tri(q.attrs.executable).to_string(),
            attr_symlinks: write_tri(q.attrs.symlinks).to_string(),
            attr_read_only: write_tri(q.attrs.read_only).to_string(),
        }
    }

    /// Read it back.
    ///
    /// `fallback` is the root a saved search with no usable root gets, which is
    /// the panel the dialog was opened on: [`Query::roots`] is documented never
    /// to be empty, and an entry someone hand-edited the roots out of must not
    /// be the one place that breaks it.
    pub fn to_query(&self, fallback: &VfsPath) -> Query {
        let mut roots: Vec<VfsPath> = self
            .roots
            .iter()
            .filter(|r| !r.trim().is_empty())
            .map(|r| VfsPath::local(r.trim()))
            .collect();
        if roots.is_empty() {
            roots.push(fallback.clone());
        }
        let mut query = Query::new(fallback.clone());
        query.name = self.mask.clone();
        query.name_mode = if self.mask_regex {
            NameMode::Regex
        } else {
            NameMode::Glob
        };
        query.roots = roots;
        query.search_archives = self.search_archives;
        query.depth = read_depth(&self.depth);
        query.content = self.find_text.then(|| ContentQuery {
            pattern: self.text.clone(),
            mode: read_text_mode(&self.text_mode),
            whole_words: self.whole_words,
            case_sensitive: self.case_sensitive,
            inverted: self.inverted,
            charsets: Charsets {
                utf8: self.utf8,
                utf16: self.utf16,
                latin1: self.latin1,
                cp437: self.cp437,
            },
        });
        query.size = SizeRange {
            min: read_size(&self.size_min),
            max: read_size(&self.size_max),
        };
        query.date = read_date(&self.date, &self.after, &self.before, self.days);
        query.attrs = AttrFilter {
            directories: read_tri(&self.attr_directories),
            hidden: read_tri(&self.attr_hidden),
            executable: read_tri(&self.attr_executable),
            symlinks: read_tri(&self.attr_symlinks),
            read_only: read_tri(&self.attr_read_only),
        };
        query
    }
}

/// The file as a whole: an array of tables called `search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SavedFile {
    search: Vec<SavedSearch>,
}

/// Where `searches.toml` lives.
pub fn saved_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(SAVED_FILE))
}

/// Parse `searches.toml`'s text. A parse failure is an empty list and a
/// warning, never an error to the caller.
pub fn parse_saved(text: &str) -> (Vec<SavedSearch>, Vec<String>) {
    match toml::from_str::<SavedFile>(text) {
        Ok(file) => (file.search, Vec::new()),
        Err(err) => (
            Vec::new(),
            vec![format!("{SAVED_FILE}: {err}; no saved searches loaded")],
        ),
    }
}

/// Render a saved-search list back to TOML.
pub fn render_saved(searches: &[SavedSearch]) -> Result<String> {
    let file = SavedFile {
        search: searches.to_vec(),
    };
    toml::to_string_pretty(&file).map_err(|e| Error::msg(format!("{SAVED_FILE}: {e}")))
}

/// Read the saved searches, degrading to an empty list on every failure.
pub fn load_saved() -> (Vec<SavedSearch>, Vec<String>) {
    let Ok(path) = saved_path() else {
        return (Vec::new(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_saved(&text),
        // A missing file is the normal first run, not a warning.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(err) => (Vec::new(), vec![format!("{}: {err}", path.display())]),
    }
}

/// Write the saved searches, creating the config directory if needed.
///
/// Called from the event loop, never from `Dialog::handle_key`
/// (the design).
pub fn store_saved(searches: &[SavedSearch]) -> Result<()> {
    let path = saved_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let text = render_saved(searches)?;
    std::fs::write(&path, text).map_err(|e| Error::io(&path, e))
}

// -------------------------------------------------------------- history -----

/// The Find dialog's combo-box history.
///
/// Three lists, one per combo box the design gives one: the name mask, the
/// text pattern and the start path. The Load/Save tab's names are not a fourth
/// list - they are `searches.toml` itself, which is a curated file rather than
/// a history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// "Search for" - name masks.
    pub names: Vec<String>,
    /// "Find text" - text patterns.
    pub texts: Vec<String>,
    /// "Search in" - start paths.
    pub roots: Vec<String>,
}

impl History {
    /// Push `value` onto `list`: most recent first, de-duplicated, capped at
    /// [`MAX_HISTORY`].
    ///
    /// An empty or whitespace-only value is not remembered: a combo box that
    /// offers a blank line is offering nothing.
    pub fn remember(list: &mut Vec<String>, value: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        list.retain(|existing| existing != value);
        list.insert(0, value.to_string());
        list.truncate(MAX_HISTORY);
    }
}

/// Where `search-history.toml` lives.
pub fn history_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(HISTORY_FILE))
}

/// Parse the history file. Anything unreadable is an empty history *and a
/// warning*, the way [`parse_saved`] treats `searches.toml`.
///
/// The warning is the whole point. Losing the history is a convenience gone
/// and must never be visible as an error, but the empty history that results
/// is also what would be written back on the next flush - so a file that
/// merely failed to parse would be replaced by an empty one, and the second
/// swallow is what destroys it. [`store_history_to`] refuses that write; this
/// says out loud why the lists came up empty.
pub fn parse_history(text: &str) -> (History, Vec<String>) {
    match toml::from_str::<History>(text) {
        Ok(history) => (history, Vec::new()),
        Err(err) => (
            History::default(),
            vec![format!(
                "{HISTORY_FILE}: {err}; no history loaded, and the file is left alone"
            )],
        ),
    }
}

/// Read the history, degrading to an empty one and a warning on every failure.
pub fn load_history() -> (History, Vec<String>) {
    let Ok(path) = history_path() else {
        return (History::default(), Vec::new());
    };
    load_history_from(&path)
}

/// [`load_history`], against a named file so a test can point it somewhere.
fn load_history_from(path: &Path) -> (History, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_history(&text),
        // A missing file is the normal first run, not a warning.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (History::default(), Vec::new()),
        Err(err) => (
            History::default(),
            vec![format!("{}: {err}", path.display())],
        ),
    }
}

/// Write the history, creating the state directory if needed.
pub fn store_history(history: &History) -> Result<()> {
    store_history_to(&history_path()?, history)
}

/// [`store_history`], against a named file so a test can point it somewhere.
///
/// A file that is there and will not parse is never overwritten. It loaded as
/// an empty history, so what would be written over it is that empty history,
/// and a file the user could have repaired in an editor becomes a file with
/// nothing in it. Refusing is a returned error and the caller's status line;
/// the file keeps whatever it had.
fn store_history_to(path: &Path, history: &History) -> Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && toml::from_str::<History>(&existing).is_err()
    {
        return Err(Error::msg(format!(
            "{}: not overwritten, because it did not parse; \
             delete it to start a fresh history",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let text =
        toml::to_string_pretty(history).map_err(|e| Error::msg(format!("{HISTORY_FILE}: {e}")))?;
    std::fs::write(path, text).map_err(|e| Error::io(path, e))
}

// ----------------------------------------------------------- the scalars ----

/// `none`, `all`, or the number of levels.
fn write_depth(depth: Depth) -> String {
    match depth {
        Depth::None => "none".to_string(),
        Depth::Unlimited => "all".to_string(),
        Depth::Levels(n) => n.to_string(),
    }
}

/// Read one back. Anything unrecognised is [`Depth::Unlimited`], which is the
/// dropdown's own default and the only reading that cannot silently narrow a
/// search.
fn read_depth(text: &str) -> Depth {
    match text.trim().to_ascii_lowercase().as_str() {
        "none" | "0" => Depth::None,
        "" | "all" | "unlimited" => Depth::Unlimited,
        other => match other.parse::<u16>() {
            Ok(n) => Depth::Levels(n),
            Err(_) => Depth::Unlimited,
        },
    }
}

/// `plain`, `regex` or `hex`.
const fn write_text_mode(mode: TextMode) -> &'static str {
    match mode {
        TextMode::Plain => "plain",
        TextMode::Regex => "regex",
        TextMode::Hex => "hex",
    }
}

/// Read one back; anything unrecognised is a literal, which is the mode that
/// cannot mean something the user did not type.
fn read_text_mode(text: &str) -> TextMode {
    match text.trim().to_ascii_lowercase().as_str() {
        "regex" => TextMode::Regex,
        "hex" => TextMode::Hex,
        _ => TextMode::Plain,
    }
}

/// `ignore`, `yes` or `no`.
const fn write_tri(tri: Tri) -> &'static str {
    match tri {
        Tri::Ignore => "ignore",
        Tri::Yes => "yes",
        Tri::No => "no",
    }
}

/// Read one back; anything unrecognised is [`Tri::Ignore`], the value that
/// cannot change a result.
fn read_tri(text: &str) -> Tri {
    match text.trim().to_ascii_lowercase().as_str() {
        "yes" | "require" | "true" => Tri::Yes,
        "no" | "forbid" | "false" => Tri::No,
        _ => Tri::Ignore,
    }
}

/// A size as `config.toml` spells one, or the empty string for "no bound".
fn read_size(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    ByteSize::parse(text).ok().map(ByteSize::bytes)
}

/// The four keys one [`DateRange`] becomes.
fn write_date(date: DateRange) -> (String, String, String, u32) {
    match date {
        DateRange::Any => ("any".to_string(), String::new(), String::new(), 0),
        DateRange::Between { after, before } => (
            "between".to_string(),
            after.map(format_date).unwrap_or_default(),
            before.map(format_date).unwrap_or_default(),
            0,
        ),
        DateRange::NewerThanDays(days) => ("newer".to_string(), String::new(), String::new(), days),
    }
}

/// Read them back. An unparseable date is "no bound at that end" and an
/// unrecognised mode is [`DateRange::Any`]: a date filter nobody can read must
/// not silently hide files.
fn read_date(mode: &str, after: &str, before: &str, days: u32) -> DateRange {
    match mode.trim().to_ascii_lowercase().as_str() {
        "between" => DateRange::Between {
            after: parse_date(after, DayEdge::Start),
            before: parse_date(before, DayEdge::End),
        },
        "newer" if days > 0 => DateRange::NewerThanDays(days),
        _ => DateRange::Any,
    }
}

/// Which instant of a day a typed date means.
///
/// A date is a day, not a moment, and the two ends of a range want opposite
/// edges of it: `after 2026-08-27` includes everything written that day and
/// `before 2026-08-27` does too, so typing one date in both fields selects
/// exactly that day. Anything else surprises the person who typed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayEdge {
    /// Local midnight at the start of the day.
    Start,
    /// The last instant of the day, local.
    End,
}

/// Parse `YYYY-MM-DD` as a local instant, or `None` when it is not one.
///
/// Local rather than UTC because a file's date is read in the timezone the
/// person is sitting in, and `mtime` is compared against it directly. A date
/// in a gap the local zone skips (a DST spring forward) resolves to the first
/// instant that does exist that day, because refusing a legal date on a
/// timezone technicality is not an answer a search dialog can give.
pub fn parse_date(text: &str, edge: DayEdge) -> Option<std::time::SystemTime> {
    use chrono::{Local, NaiveDate, TimeZone};

    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let day = NaiveDate::parse_from_str(text, DATE_FORMAT).ok()?;
    let naive = match edge {
        DayEdge::Start => day.and_hms_opt(0, 0, 0)?,
        DayEdge::End => day.and_hms_opt(23, 59, 59)?,
    };
    let local = Local
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| Local.from_local_datetime(&naive).latest())?;
    Some(local.to_utc().into())
}

/// Spell an instant as `YYYY-MM-DD`, in the same local zone [`parse_date`]
/// read it in, so a saved range reopens on the dates that were typed.
pub fn format_date(at: std::time::SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format(DATE_FORMAT).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Query {
        let mut q = Query::new(VfsPath::local("/home/thorin/dev"));
        q.name = "*.rs".to_string();
        q.name_mode = NameMode::Regex;
        q.roots.push(VfsPath::local("/srv/media"));
        q.search_archives = true;
        q.depth = Depth::Levels(3);
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            mode: TextMode::Hex,
            whole_words: true,
            case_sensitive: true,
            inverted: true,
            charsets: Charsets {
                utf8: false,
                utf16: true,
                latin1: true,
                cp437: false,
            },
        });
        q.size = SizeRange {
            min: Some(8 * 1024 * 1024),
            max: Some(1024 * 1024 * 1024),
        };
        q.date = DateRange::NewerThanDays(7);
        q.attrs = AttrFilter {
            directories: Tri::No,
            hidden: Tri::Yes,
            executable: Tri::Ignore,
            symlinks: Tri::No,
            read_only: Tri::Yes,
        };
        q
    }

    #[test]
    fn a_saved_search_survives_a_round_trip_through_toml() {
        // the Load/Save tab: what is saved is what is loaded, or
        // the tab is a way to run a search nobody asked for.
        let query = sample();
        let entry = SavedSearch::new("rust TODOs", &query);
        let text = render_saved(std::slice::from_ref(&entry)).expect("render");
        let (back, warnings) = parse_saved(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, vec![entry]);

        let fallback = VfsPath::local("/tmp");
        let reloaded = back.first().expect("one entry").query.to_query(&fallback);
        assert_eq!(reloaded.name, query.name);
        assert_eq!(reloaded.name_mode, query.name_mode);
        assert_eq!(reloaded.roots, query.roots);
        assert_eq!(reloaded.search_archives, query.search_archives);
        assert_eq!(reloaded.depth, query.depth);
        assert_eq!(reloaded.content, query.content);
        assert_eq!(reloaded.size, query.size);
        assert_eq!(reloaded.date, query.date);
        assert_eq!(reloaded.attrs, query.attrs);
    }

    #[test]
    fn an_absolute_date_range_round_trips_on_the_dates_that_were_typed() {
        // The two ends take opposite edges of the day, so one date in both
        // fields is that day and nothing else.
        let after = parse_date("2026-01-31", DayEdge::Start).expect("a date");
        let before = parse_date("2026-01-31", DayEdge::End).expect("a date");
        assert!(after < before);
        assert_eq!(format_date(after), "2026-01-31");
        assert_eq!(format_date(before), "2026-01-31");

        let mut query = Query::new(VfsPath::local("/tmp"));
        query.date = DateRange::Between {
            after: Some(after),
            before: Some(before),
        };
        let saved = SavedQuery::from_query(&query);
        assert_eq!(saved.date, "between");
        assert_eq!(saved.after, "2026-01-31");
        assert_eq!(saved.before, "2026-01-31");
        assert_eq!(saved.to_query(&VfsPath::local("/tmp")).date, query.date);

        assert_eq!(parse_date("31/01/2026", DayEdge::Start), None);
        assert_eq!(parse_date("   ", DayEdge::Start), None);
    }

    #[test]
    fn an_unparseable_file_is_an_empty_list_and_a_warning() {
        // The same degradation `panel::state` documents: the dialog opens.
        let (searches, warnings) = parse_saved("this is not = = toml");
        assert!(searches.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(SAVED_FILE), "{warnings:?}");

        let (searches, warnings) = parse_saved("");
        assert!(searches.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_hand_written_entry_needs_only_the_keys_it_cares_about() {
        // Every key defaults, so the file is writable by hand - which is the
        // whole reason it is in the config directory.
        let (searches, warnings) = parse_saved(
            r#"
[[search]]
name = "sources"
mask = "*.rs"
roots = ["/home/thorin/dev"]
"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = searches.first().expect("one entry");
        assert_eq!(entry.name, "sources");
        let query = entry.query.to_query(&VfsPath::local("/tmp"));
        assert_eq!(query.name, "*.rs");
        assert_eq!(query.roots, vec![VfsPath::local("/home/thorin/dev")]);
        assert_eq!(query.depth, Depth::Unlimited, "an absent depth is `all`");
        assert_eq!(query.content, None, "an absent find_text is names only");
        assert!(query.attrs.is_any(), "absent attributes filter nothing");
        assert!(query.size.is_any());
    }

    #[test]
    fn an_entry_with_no_usable_root_falls_back_to_the_panel() {
        // `Query::roots` is documented never to be empty and a hand-edited
        // file must not be the one place that breaks it.
        let (searches, _) = parse_saved("[[search]]\nname = \"x\"\nroots = [\" \"]\n");
        let query = searches
            .first()
            .expect("one entry")
            .query
            .to_query(&VfsPath::local("/srv"));
        assert_eq!(query.roots, vec![VfsPath::local("/srv")]);
    }

    #[test]
    fn an_unreadable_scalar_falls_back_rather_than_failing_the_load() {
        let (searches, warnings) = parse_saved(
            r#"
[[search]]
name = "odd"
depth = "sideways"
text_mode = "runes"
attr_hidden = "perhaps"
size_min = "eight megabytes"
date = "sometime"
"#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let query = searches
            .first()
            .expect("one entry")
            .query
            .to_query(&VfsPath::local("/tmp"));
        assert_eq!(query.depth, Depth::Unlimited);
        assert_eq!(query.attrs.hidden, Tri::Ignore);
        assert_eq!(query.size.min, None);
        assert_eq!(query.date, DateRange::Any);
    }

    #[test]
    fn find_text_is_the_option_and_not_the_field() {
        // the design, in the file as well as in the dialog: a
        // pattern with the box unticked is a name-only search, and the pattern
        // is still there for next time.
        let (searches, _) =
            parse_saved("[[search]]\nname = \"x\"\ntext = \"TODO\"\nfind_text = false\n");
        let entry = searches.first().expect("one entry");
        assert_eq!(entry.query.text, "TODO");
        assert_eq!(entry.query.to_query(&VfsPath::local("/tmp")).content, None);
    }

    #[test]
    fn history_is_most_recent_first_deduplicated_and_capped() {
        let mut list = Vec::new();
        History::remember(&mut list, "*.rs");
        History::remember(&mut list, "*.toml");
        History::remember(&mut list, "*.rs");
        assert_eq!(list, vec!["*.rs".to_string(), "*.toml".to_string()]);

        History::remember(&mut list, "   ");
        History::remember(&mut list, "");
        assert_eq!(list.len(), 2, "a blank line is not a history entry");

        for i in 0..MAX_HISTORY * 2 {
            History::remember(&mut list, &format!("mask{i}"));
        }
        assert_eq!(list.len(), MAX_HISTORY);
        assert_eq!(list.first().map(String::as_str), Some("mask63"));
    }

    #[test]
    fn a_history_file_round_trips_and_an_unreadable_one_is_empty() {
        let history = History {
            names: vec!["*.rs".to_string()],
            texts: vec!["TODO".to_string()],
            roots: vec!["/home/thorin/dev".to_string()],
        };
        let text = toml::to_string_pretty(&history).expect("render");
        assert_eq!(parse_history(&text), (history, Vec::new()));

        // Empty is a legal history and says nothing; unreadable is an empty
        // history *and* a warning, so the emptiness is never silent.
        assert_eq!(parse_history(""), (History::default(), Vec::new()));
        let (parsed, warnings) = parse_history("not = = toml");
        assert_eq!(parsed, History::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(HISTORY_FILE), "{warnings:?}");
    }

    #[test]
    fn a_history_file_that_did_not_parse_is_never_written_over() {
        // The two swallows that chained into a deleted file: the load returned
        // an empty history without saying so, and the next flush wrote that
        // empty history back. The load warns now, and this write is refused.
        let dir = temp_dir("history-not-clobbered");
        let path = dir.join(HISTORY_FILE);
        let corrupt = "names = [\"*.rs\"\ntexts = oops\n";
        std::fs::write(&path, corrupt).expect("write the corrupt file");

        let (loaded, warnings) = load_history_from(&path);
        assert_eq!(loaded, History::default(), "unreadable loads as empty");
        assert_eq!(warnings.len(), 1, "and says so: {warnings:?}");

        let err = store_history_to(&path, &loaded).expect_err("the write is refused");
        assert!(err.to_string().contains("not overwritten"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            corrupt,
            "the user's file is untouched"
        );

        // A file that does parse is written normally, and so is a missing one.
        // The refusal is about *this* file's contents and nothing else: repair
        // it in an editor, as the message says, and the next flush lands.
        let history = History {
            names: vec!["*.rs".to_string()],
            ..History::default()
        };
        std::fs::write(&path, "names = []\n").expect("repair the file by hand");
        store_history_to(&path, &history).expect("a parseable file is replaced");
        assert_eq!(load_history_from(&path), (history.clone(), Vec::new()));

        let fresh = dir.join("subdir").join(HISTORY_FILE);
        store_history_to(&fresh, &history).expect("a missing file is created");
        assert_eq!(load_history_from(&fresh), (history, Vec::new()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory of this test module's own, made and removed by the test
    /// that asked for it.
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hcmd-saved-{tag}-{pid}-{n}",
            pid = std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_two_files_are_in_the_directories_their_contents_belong_in() {
        // curated beside `hotlist.toml`,
        // history beside the rest of the history.
        let (Ok(saved), Ok(history)) = (saved_path(), history_path()) else {
            return;
        };
        assert!(saved.ends_with(SAVED_FILE), "{}", saved.display());
        assert!(history.ends_with(HISTORY_FILE), "{}", history.display());
        assert_ne!(saved.parent(), history.parent());
    }
}
