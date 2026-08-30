//! What a search is.
//!
//! One type describes a search and one function refuses one. The dialog fills
//! in a [`Query`], [`Query::compile`] turns it into a [`Compiled`], and the
//! walker and the content searcher read only the compiled form - so the
//! message the dialog shows for a pattern it cannot accept and the message the
//! engine would have shown are the same message, because there is only one.
//!
//! # "Find text" is a checkbox, in the type system
//!
//! the design calls this out as the important structural detail: content
//! search is "explicitly enabled, not inferred from the field being
//! non-empty". [`Query::content`] is therefore an `Option<ContentQuery>`:
//!
//! * `None` is the box unticked, and the text field's contents are irrelevant.
//! * `Some` with an empty pattern is a **validation error**, not a quiet
//!   downgrade to a name-only search.
//!
//! No function in this module takes a pattern and a `bool`; constructing the
//! `Some` is the only way to ask for a content search.
//!
//! # Nothing here reads a `Config`
//!
//! `search.respect_gitignore` and `ops.follow_symlinks` are carried **on the
//! query**, read once by the dialog that built it. A test can therefore drive a
//! whole search without building a `Config`, and the walker cannot develop a
//! second opinion about a setting the dialog already resolved.

use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};
use crate::ops::mask::{self, MaskMode};
use crate::vfs::{Entry, VfsPath};

use super::content::ContentMatcher;

/// The most search roots one query carries (the `>>` button).
///
/// Sixteen is far above the handful a person appends by hand and low enough
/// that the header, the walk and the saved-search file all stay comprehensible.
pub const MAX_ROOTS: usize = 16;

/// The longest pattern either field accepts.
///
/// A regular expression this long is generated rather than typed, and a
/// generated one belongs in a tool that can report where it went wrong.
pub const MAX_PATTERN_BYTES: usize = 4096;

/// How much of the text pattern the panel header quotes.
const HEADER_TEXT_CHARS: usize = 32;

// ------------------------------------------------------------- the parts ----

/// How the **name** mask is read (the `RegEx` checkbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameMode {
    /// Total Commander's wildcard language: `*`, `?`, lists, `|` exclusion.
    ///
    /// [`crate::ops::mask`] is the matcher, so `+`, `-`, "Only files of this
    /// type" and this field cannot disagree about what `*.rs` means.
    #[default]
    Glob,
    /// A regular expression, unanchored and case-insensitive unless the
    /// pattern says otherwise.
    Regex,
}

impl NameMode {
    /// Which language [`crate::ops::mask`] should read the mask in.
    pub const fn mask_mode(self) -> MaskMode {
        match self {
            Self::Glob => MaskMode::Wildcard,
            Self::Regex => MaskMode::Regex,
        }
    }
}

/// the "Search in subdirectories" dropdown.
///
/// A dropdown and not a number field, which is the own wording:
/// the three answers people actually want are "here", "everywhere" and "a
/// couple of levels", and a spin box makes the first two harder to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    /// `none`: the roots' own entries and nothing below them.
    None,
    /// `all (unlimited depth)`.
    #[default]
    Unlimited,
    /// `1`…`n` levels of subdirectory below the root.
    Levels(u16),
}

impl Depth {
    /// The dropdown's entries, in order.
    pub const CHOICES: &'static [Self] = &[
        Self::None,
        Self::Unlimited,
        Self::Levels(1),
        Self::Levels(2),
        Self::Levels(3),
        Self::Levels(4),
        Self::Levels(5),
        Self::Levels(6),
        Self::Levels(7),
        Self::Levels(8),
        Self::Levels(9),
    ];

    /// What `ignore::WalkBuilder::max_depth` is given.
    ///
    /// `ignore` counts the root itself as depth 0, so every answer here is one
    /// larger than the number of subdirectory levels the dropdown names. That
    /// off-by-one is the whole reason this is a method and not a field.
    ///
    pub const fn max_depth(self) -> Option<usize> {
        match self {
            Self::None => Some(1),
            Self::Unlimited => None,
            Self::Levels(n) => Some((n as usize).saturating_add(1)),
        }
    }

    /// The dropdown's own text.
    pub fn label(self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Unlimited => "all (unlimited depth)".to_string(),
            Self::Levels(n) => n.to_string(),
        }
    }
}

/// One charset a content search is tried in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// UTF-8, and what an untouched dialog searches.
    Utf8,
    /// UTF-16, little-endian unless a byte-order mark says otherwise.
    Utf16,
    /// Latin-1, decoded as windows-1252, which is what every browser and
    /// `encoding_rs` mean by the label.
    Latin1,
    /// The DOS code page, which `encoding_rs` does not implement and
    /// `crate::viewer::decode` carries in-tree.
    Cp437,
}

impl Charset {
    /// The name shown in the dialog and carried on a hit.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16 => "UTF-16",
            Self::Latin1 => "windows-1252",
            Self::Cp437 => "cp437",
        }
    }

    /// The `encoding_rs` label `grep_searcher::Encoding` is built from, or
    /// `None` for the two charsets the searcher does not decode itself: UTF-8
    /// is the searcher's own native form, and CP437 is transcoded on the way
    /// in by [`super::content::Cp437Reader`].
    pub const fn encoding_label(self) -> Option<&'static str> {
        match self {
            Self::Utf8 | Self::Cp437 => None,
            Self::Utf16 => Some("utf-16"),
            Self::Latin1 => Some("windows-1252"),
        }
    }

    /// True when searching in this charset searches a **transcoded** stream
    /// rather than the file's own bytes.
    ///
    /// Which is what decides whether a hit's byte offset is a position in the
    /// file or only a position in the decoded text. UTF-8 is
    /// the identity and the only one that is not transcoded: `grep_searcher`
    /// decodes UTF-16 and windows-1252 through its own `encoding()`, and
    /// `crate::search::content::Cp437Reader` decodes the DOS code page on the
    /// way in. See [`crate::vfs::ContentHit::decoded`].
    pub const fn is_transcoded(self) -> bool {
        match self {
            Self::Utf8 => false,
            Self::Utf16 | Self::Latin1 | Self::Cp437 => true,
        }
    }
}

/// the four independent charset checkboxes.
///
/// Checkboxes and not a dropdown, in the design's own words, "because a tree can
/// hold files in several encodings".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charsets {
    /// UTF-8.
    pub utf8: bool,
    /// UTF-16.
    pub utf16: bool,
    /// Latin-1 / windows-1252.
    pub latin1: bool,
    /// CP437 (DOS).
    pub cp437: bool,
}

impl Charsets {
    /// UTF-8 alone, which is what an untouched dialog searches.
    pub const DEFAULT: Self = Self {
        utf8: true,
        utf16: false,
        latin1: false,
        cp437: false,
    };

    /// Is any charset selected at all?
    pub const fn any(self) -> bool {
        self.utf8 || self.utf16 || self.latin1 || self.cp437
    }

    /// The charsets to try, **in this fixed order**: UTF-8, UTF-16,
    /// Latin-1/windows-1252, CP437.
    ///
    /// Fixed, because a file that matches in two of them reports the first one
    /// that hit, and two runs of the same search must not name different
    /// charsets for the same file.
    pub fn selected(self) -> Vec<Charset> {
        let mut out = Vec::new();
        if self.utf8 {
            out.push(Charset::Utf8);
        }
        if self.utf16 {
            out.push(Charset::Utf16);
        }
        if self.latin1 {
            out.push(Charset::Latin1);
        }
        if self.cp437 {
            out.push(Charset::Cp437);
        }
        out
    }
}

impl Default for Charsets {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How the **text** pattern is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextMode {
    /// A literal.
    #[default]
    Plain,
    /// A regular expression.
    Regex,
    /// A byte sequence, `DE AD BE EF`, **the same syntax as the viewer's hex
    /// find**.
    ///
    /// That is a code fact rather than a promise: `crate::viewer::find` owns
    /// the one parser and [`super::content::hex_regex`] owns the one
    /// translation of its output.
    Hex,
}

/// Everything the "Find text" group collects.
///
/// **This type existing at all is the checkbox** - see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentQuery {
    /// What to look for. Empty is a validation error, never a name-only
    /// search.
    pub pattern: String,
    /// How to read it.
    pub mode: TextMode,
    /// `Whole words only`.
    pub whole_words: bool,
    /// `Case sensitive`.
    pub case_sensitive: bool,
    /// `Find files NOT containing the text`.
    pub inverted: bool,
    /// The charsets to try, each independently.
    pub charsets: Charsets,
}

impl Default for ContentQuery {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            mode: TextMode::default(),
            whole_words: false,
            case_sensitive: false,
            inverted: false,
            charsets: Charsets::DEFAULT,
        }
    }
}

/// The Advanced tab's size range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeRange {
    /// `Size at least`, in bytes.
    pub min: Option<u64>,
    /// `Size at most`, in bytes.
    pub max: Option<u64>,
}

impl SizeRange {
    /// True when neither bound is set, so the test can be skipped entirely.
    pub const fn is_any(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    /// Is `bytes` inside the range? Both bounds are inclusive.
    pub const fn accepts(&self, bytes: u64) -> bool {
        if let Some(min) = self.min
            && bytes < min
        {
            return false;
        }
        if let Some(max) = self.max
            && bytes > max
        {
            return false;
        }
        true
    }
}

/// The Advanced tab's modification-date range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateRange {
    /// No date filter.
    #[default]
    Any,
    /// Absolute, either end optional.
    Between {
        /// Modified at or after this instant.
        after: Option<SystemTime>,
        /// Modified at or before this instant.
        before: Option<SystemTime>,
    },
    /// "newer than N days", resolved against `now` once, when the search
    /// starts, so every file in one search is measured against one instant.
    NewerThanDays(u32),
}

impl DateRange {
    /// True when there is no date filter.
    pub const fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    /// Does a file modified at `mtime` pass?
    ///
    /// `now` is the search's own start instant. A file with **no** mtime is
    /// accepted by [`DateRange::Any`] and refused by everything else: a filter
    /// that cannot be evaluated must not silently pass, which is the same rule
    /// the design applies to a listing it cannot read.
    pub fn accepts(&self, mtime: Option<SystemTime>, now: SystemTime) -> bool {
        match self {
            Self::Any => true,
            Self::Between { after, before } => {
                let Some(mtime) = mtime else {
                    return false;
                };
                if let Some(after) = after
                    && mtime < *after
                {
                    return false;
                }
                if let Some(before) = before
                    && mtime > *before
                {
                    return false;
                }
                true
            }
            Self::NewerThanDays(days) => {
                let Some(mtime) = mtime else {
                    return false;
                };
                let window = Duration::from_secs(u64::from(*days).saturating_mul(24 * 60 * 60));
                // A file with a modification time in the future is newer than
                // any window, which is the only answer that is not surprising.
                match now.checked_sub(window) {
                    Some(cutoff) => mtime >= cutoff,
                    // `now - window` underflowed the epoch, so every file is
                    // inside the window.
                    None => true,
                }
            }
        }
    }
}

/// One tri-state attribute filter: ignore it, require it, or forbid it.
///
/// Tri-state and not a checkbox, because "not hidden" is as much a filter as
/// "hidden" and a two-state control can only express one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tri {
    /// The attribute is not consulted.
    #[default]
    Ignore,
    /// The attribute must be true.
    Yes,
    /// The attribute must be false.
    No,
}

impl Tri {
    /// `Space` cycles Ignore -> Yes -> No -> Ignore.
    pub const fn next(self) -> Self {
        match self {
            Self::Ignore => Self::Yes,
            Self::Yes => Self::No,
            Self::No => Self::Ignore,
        }
    }

    /// Does `value` pass this filter?
    pub const fn accepts(self, value: bool) -> bool {
        match self {
            Self::Ignore => true,
            Self::Yes => value,
            Self::No => !value,
        }
    }

    /// The one character drawn in the control's box.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ignore => " ",
            Self::Yes => "x",
            Self::No => "-",
        }
    }
}

/// The Advanced tab's attributes.
///
/// Every one defaults to [`Tri::Ignore`], so an Advanced tab nobody has
/// touched cannot change a result - the only defensible default for a filter
/// on a tab you cannot see from the one you are typing in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttrFilter {
    /// Is it a directory?
    pub directories: Tri,
    /// Hidden by the platform's convention - a leading dot on Unix.
    pub hidden: Tri,
    /// Executable, from the mode bits only.
    pub executable: Tri,
    /// A symbolic link, whatever it points at.
    pub symlinks: Tri,
    /// No write bit set for anyone.
    pub read_only: Tri,
}

impl AttrFilter {
    /// True when every attribute is ignored.
    pub const fn is_any(&self) -> bool {
        matches!(self.directories, Tri::Ignore)
            && matches!(self.hidden, Tri::Ignore)
            && matches!(self.executable, Tri::Ignore)
            && matches!(self.symlinks, Tri::Ignore)
            && matches!(self.read_only, Tri::Ignore)
    }

    /// Does this row pass every attribute filter?
    pub fn accepts(&self, entry: &Entry) -> bool {
        self.directories.accepts(entry.is_dir())
            && self.hidden.accepts(entry.is_hidden)
            && self.executable.accepts(entry.is_executable())
            && self.symlinks.accepts(entry.is_symlink())
            // Read-only is the absence of every write bit, which is the same
            // question `Capabilities::writable` asks of a backend and the only
            // one the mode bits can answer without a `access(2)`.
            && self.read_only.accepts(entry.mode & 0o222 == 0)
    }
}

// ------------------------------------------------------------ the query ----

/// One search, exactly as the dialog collected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The name mask, as typed. Empty means `*`.
    pub name: String,
    /// Which language `name` is written in.
    pub name_mode: NameMode,
    /// Where to start. Never empty; at most [`MAX_ROOTS`].
    pub roots: Vec<VfsPath>,
    /// the "Only search in selected directories/files". Empty when
    /// the box is off. A path here is either a root of the walk or a file that
    /// is admitted on its own.
    pub restrict: Vec<VfsPath>,
    /// the "Search archives".
    pub search_archives: bool,
    /// the "Search in subdirectories".
    pub depth: Depth,
    /// `None` is "Find text" unticked.
    pub content: Option<ContentQuery>,
    /// The Advanced tab's size range.
    pub size: SizeRange,
    /// The Advanced tab's modification-date range.
    pub date: DateRange,
    /// The Advanced tab's attributes.
    pub attrs: AttrFilter,
    /// `search.respect_gitignore`, carried on the query rather
    /// than read from the config inside the walker.
    pub respect_gitignore: bool,
    /// `ops.follow_symlinks`. No new configuration key: a search that followed
    /// links would hang on the first loop, and `ignore` does not detect one.
    pub follow_symlinks: bool,
}

impl Query {
    /// A name-only search of one root, with everything else at its default.
    pub fn new(root: VfsPath) -> Self {
        Self {
            name: String::new(),
            name_mode: NameMode::default(),
            roots: vec![root],
            restrict: Vec::new(),
            search_archives: false,
            depth: Depth::default(),
            content: None,
            size: SizeRange::default(),
            date: DateRange::default(),
            attrs: AttrFilter::default(),
            respect_gitignore: false,
            follow_symlinks: false,
        }
    }

    /// `Ctrl+B`: the same thing with an empty pattern.
    ///
    /// There is no second engine and no second code path; branch view is a
    /// search whose mask matches everything. `search_archives` stays off
    /// because `Ctrl+B` is one keystroke with no dialog in front of it, and
    /// opening every archive in a tree is not what one keystroke should be
    /// able to ask for.
    pub fn branch(root: VfsPath) -> Self {
        Self::new(root)
    }

    /// True for the query `Ctrl+B` makes: no name mask, no content, no
    /// filters. What makes the header say `branch` rather than `search`.
    pub fn is_branch(&self) -> bool {
        self.name.trim().is_empty()
            && self.content.is_none()
            && self.restrict.is_empty()
            && !self.search_archives
            && matches!(self.depth, Depth::Unlimited)
            && self.size.is_any()
            && self.date.is_any()
            && self.attrs.is_any()
    }

    /// the panel header: `[search: *.rs "TODO" in ~/dev]`.
    ///
    /// The grammar is fixed here so that nothing else has to invent one:
    ///
    /// ```text
    /// [search: <mask>[ "<text>"] in <root>[ +<n>]]
    /// [branch: <root>]
    /// ```
    ///
    /// Every character of it is ASCII, so `ui.ascii_borders` changes nothing.
    pub fn header(&self) -> String {
        let root = self
            .roots
            .first()
            .map(fold_home)
            .unwrap_or_else(|| "/".to_string());
        if self.is_branch() {
            return format!("[branch: {root}]");
        }
        let mask = if self.name.trim().is_empty() {
            "*"
        } else {
            self.name.as_str()
        };
        let text = match self.content.as_ref() {
            Some(content) => format!(" \"{}\"", crop(&content.pattern, HEADER_TEXT_CHARS)),
            None => String::new(),
        };
        let more = match self.roots.len() {
            0 | 1 => String::new(),
            n => format!(" +{}", n.saturating_sub(1)),
        };
        format!("[search: {mask}{text} in {root}{more}]")
    }

    /// Compile every pattern and check every field.
    ///
    /// **The one place a search is refused**, so the dialog's message and the
    /// engine's cannot differ. Nothing here touches the filesystem: a root
    /// that no longer exists compiles and is reported by the walk, which is
    /// where an unreadable directory is reported anyway.
    pub fn compile(&self) -> Result<Compiled> {
        if self.roots.is_empty() {
            return Err(Error::msg("a search needs somewhere to start"));
        }
        if self.roots.len() > MAX_ROOTS {
            return Err(Error::msg(format!(
                "a search takes at most {MAX_ROOTS} roots; this one has {}",
                self.roots.len()
            )));
        }
        if self.name.len() > MAX_PATTERN_BYTES {
            return Err(Error::msg(format!(
                "the name mask is longer than {MAX_PATTERN_BYTES} bytes"
            )));
        }
        // An empty mask, and a `*` or `*.*` in the glob language, match
        // everything: skipping the matcher entirely is what makes a branch view
        // of a million files cost nothing per row.
        let name = match self.name_mode {
            NameMode::Glob if mask::is_match_all(&self.name) => None,
            mode => Some(mask::compile(&self.name, mode.mask_mode())?),
        };

        if let Some(min) = self.size.min
            && let Some(max) = self.size.max
            && min > max
        {
            return Err(Error::msg(
                "the smallest size is larger than the largest one",
            ));
        }
        match self.date {
            DateRange::Any => {}
            DateRange::Between { after, before } => {
                if let Some(after) = after
                    && let Some(before) = before
                    && after > before
                {
                    return Err(Error::msg("the date range starts after it ends"));
                }
            }
            DateRange::NewerThanDays(days) => {
                if days == 0 {
                    return Err(Error::msg("\"newer than\" needs at least one day"));
                }
            }
        }

        let content = match self.content.as_ref() {
            Some(content) => Some(ContentMatcher::compile(content)?),
            None => None,
        };

        Ok(Compiled {
            query: self.clone(),
            name,
            content,
        })
    }
}

/// `$HOME` folded to `~`, for the header.
///
/// Display only: nothing resolves a path back out of this.
fn fold_home(path: &VfsPath) -> String {
    let text = path.to_string();
    let Ok(home) = crate::config::paths::home_dir() else {
        return text;
    };
    let home = home.to_string_lossy();
    if home.is_empty() || home == "/" {
        return text;
    }
    match text.strip_prefix(home.as_ref()) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        Some(_) | None => text,
    }
}

/// `text`, cropped to `chars` characters with an ellipsis.
fn crop(text: &str, chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

// --------------------------------------------------------- the compiled ----

/// A validated query with its matchers built.
///
/// Constructed only by [`Query::compile`]. `Send + Sync`, because `ignore`'s
/// parallel walker shares one across its threads.
#[derive(Debug)]
pub struct Compiled {
    query: Query,
    /// `None` when the mask matches everything, which is the common case and
    /// the one worth not paying for.
    name: Option<mask::Compiled>,
    content: Option<ContentMatcher>,
}

impl Compiled {
    /// The query this was built from.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// Does this file name pass the name mask?
    ///
    /// A directory that fails this is still descended into by the walk: a mask
    /// names what you are looking for, not where.
    pub fn name_matches(&self, name: &str) -> bool {
        match self.name.as_ref() {
            None => true,
            Some(compiled) => compiled.matches(name),
        }
    }

    /// Does this row pass size, date and attributes?
    ///
    /// `now` is the search's own start instant, so every file in one search is
    /// measured against one clock reading.
    ///
    /// A directory is refused by any size bound at all: a directory's `size` is
    /// not a size (the design renders `<DIR>` in its place), and letting one
    /// through a `>= 1MiB` filter would be a wrong answer rather than a
    /// generous one.
    pub fn attrs_match(&self, entry: &Entry, now: SystemTime) -> bool {
        if !self.query.size.is_any() {
            if entry.is_dir() {
                return false;
            }
            if !self.query.size.accepts(entry.size) {
                return false;
            }
        }
        self.query.date.accepts(entry.mtime, now) && self.query.attrs.accepts(entry)
    }

    /// The content matcher, when there is one.
    pub fn content(&self) -> Option<&ContentMatcher> {
        self.content.as_ref()
    }

    /// The find query the viewer should open with, so the "the hit
    /// already highlighted" is the **same pattern** that found it.
    ///
    /// `None` for [`TextMode::Regex`], which the viewer's find cannot yet
    /// compile: its matcher is a byte-class list with a chunk-overlap rule that
    /// needs a bounded match length, and a regex has none. The viewer still
    /// opens at the hit and says so.
    pub fn viewer_find(&self) -> Option<crate::viewer::find::FindQuery> {
        let content = self.query.content.as_ref()?;
        let kind = match content.mode {
            TextMode::Plain => crate::viewer::find::FindKind::Text,
            TextMode::Hex => crate::viewer::find::FindKind::Hex,
            TextMode::Regex => return None,
        };
        Some(crate::viewer::find::FindQuery {
            input: content.pattern.clone(),
            kind,
            case: if content.case_sensitive {
                crate::config::QuickSearchCase::Sensitive
            } else {
                crate::config::QuickSearchCase::Insensitive
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::EntryKind;

    fn root() -> VfsPath {
        VfsPath::local("/tmp/tree")
    }

    fn file(name: &str) -> Entry {
        Entry::file(name)
    }

    #[test]
    fn the_depth_dropdown_counts_levels_not_walker_depth() {
        // `ignore` counts the root as depth 0, so every answer is one larger
        // than the number of subdirectory levels the dropdown names.
        assert_eq!(Depth::None.max_depth(), Some(1));
        assert_eq!(Depth::Levels(1).max_depth(), Some(2));
        assert_eq!(Depth::Levels(9).max_depth(), Some(10));
        assert_eq!(Depth::Unlimited.max_depth(), None);
        // The dropdown offers `none`, `all` and 1..=9, in that order.
        assert_eq!(Depth::CHOICES.first(), Some(&Depth::None));
        assert_eq!(Depth::CHOICES.get(1), Some(&Depth::Unlimited));
        assert_eq!(Depth::CHOICES.len(), 11);
        assert_eq!(Depth::Levels(3).label(), "3");
    }

    #[test]
    fn the_charsets_are_tried_in_one_fixed_order() {
        let all = Charsets {
            utf8: true,
            utf16: true,
            latin1: true,
            cp437: true,
        };
        assert_eq!(
            all.selected(),
            vec![
                Charset::Utf8,
                Charset::Utf16,
                Charset::Latin1,
                Charset::Cp437
            ]
        );
        assert_eq!(Charsets::DEFAULT.selected(), vec![Charset::Utf8]);
        assert!(
            !Charsets {
                utf8: false,
                ..Charsets::DEFAULT
            }
            .any()
        );
        // The labels a hit is stamped with are the dialog's own.
        assert_eq!(Charset::Latin1.label(), "windows-1252");
        assert_eq!(
            Charset::Utf8.encoding_label(),
            None,
            "the searcher's native"
        );
        assert_eq!(Charset::Cp437.encoding_label(), None, "transcoded in-tree");
    }

    #[test]
    fn content_search_is_the_checkbox_and_not_the_field() {
        // Unticked with a pattern typed into the field searches names only.
        let mut q = Query::new(root());
        q.name = "*.rs".to_string();
        let compiled = q.compile().expect("a name-only search");
        assert!(compiled.content().is_none());

        // Ticked with an empty pattern is refused, not downgraded.
        q.content = Some(ContentQuery::default());
        let err = q
            .compile()
            .expect_err("an empty pattern is refused")
            .to_string();
        assert!(err.contains("pattern"), "{err}");

        // Ticked with a pattern is a content search.
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            ..ContentQuery::default()
        });
        assert!(q.compile().expect("a content search").content().is_some());
    }

    #[test]
    fn a_search_with_no_charset_is_refused() {
        let mut q = Query::new(root());
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            charsets: Charsets {
                utf8: false,
                utf16: false,
                latin1: false,
                cp437: false,
            },
            ..ContentQuery::default()
        });
        let err = q.compile().expect_err("no charset").to_string();
        assert!(err.contains("charset"), "{err}");
    }

    #[test]
    fn an_empty_mask_matches_everything_and_costs_nothing() {
        let compiled = Query::new(root()).compile().expect("empty mask");
        assert!(compiled.name_matches("anything at all"));
        assert!(compiled.name_matches(""));

        let mut q = Query::new(root());
        q.name = "*.rs".to_string();
        let compiled = q.compile().expect("glob");
        assert!(compiled.name_matches("main.rs"));
        assert!(!compiled.name_matches("main.rst"));

        // A regex mask is unanchored, which is what makes `\.rs$` work.
        let mut q = Query::new(root());
        q.name = r"\.rs$".to_string();
        q.name_mode = NameMode::Regex;
        let compiled = q.compile().expect("regex");
        assert!(compiled.name_matches("main.rs"));
        assert!(!compiled.name_matches("main.rst"));
    }

    #[test]
    fn a_bad_pattern_is_refused_by_compile_and_by_nothing_else() {
        let mut q = Query::new(root());
        q.name = "*.rs".to_string();
        q.name_mode = NameMode::Regex;
        let err = q.compile().expect_err("a glob is not a regex").to_string();
        assert!(err.contains("regular expression"), "{err}");

        let mut q = Query::new(root());
        q.roots.clear();
        assert!(q.compile().is_err(), "a search needs a root");

        let mut q = Query::new(root());
        q.roots = (0..MAX_ROOTS + 1)
            .map(|i| VfsPath::local(format!("/{i}")))
            .collect();
        assert!(q.compile().is_err(), "and not too many of them");
    }

    #[test]
    fn an_impossible_range_is_refused_rather_than_matching_nothing() {
        let mut q = Query::new(root());
        q.size = SizeRange {
            min: Some(100),
            max: Some(10),
        };
        assert!(q.compile().is_err(), "min above max");

        let mut q = Query::new(root());
        q.date = DateRange::NewerThanDays(0);
        assert!(q.compile().is_err(), "zero days is a slip, not a filter");

        let mut q = Query::new(root());
        let now = SystemTime::now();
        q.date = DateRange::Between {
            after: Some(now),
            before: Some(now - Duration::from_secs(60)),
        };
        assert!(q.compile().is_err(), "after later than before");
    }

    #[test]
    fn size_and_date_and_attributes_are_judged_together() {
        let now = SystemTime::now();
        let mut q = Query::new(root());
        q.size = SizeRange {
            min: Some(10),
            max: None,
        };
        let compiled = q.compile().expect("size filter");

        let mut small = file("a");
        small.size = 9;
        let mut big = file("b");
        big.size = 11;
        assert!(!compiled.attrs_match(&small, now));
        assert!(compiled.attrs_match(&big, now));

        // A directory has no size, so a size bound refuses it outright rather
        // than reading `Entry::size` as one.
        let mut dir = Entry::dir("src");
        dir.size = 4096;
        assert!(!compiled.attrs_match(&dir, now));
    }

    #[test]
    fn a_date_filter_that_cannot_be_evaluated_refuses() {
        let now = SystemTime::now();
        assert!(DateRange::Any.accepts(None, now), "no filter, no question");
        assert!(!DateRange::NewerThanDays(7).accepts(None, now));
        assert!(DateRange::NewerThanDays(7).accepts(Some(now), now));
        assert!(
            !DateRange::NewerThanDays(1)
                .accepts(Some(now - Duration::from_secs(60 * 60 * 48)), now)
        );
        assert!(
            DateRange::NewerThanDays(3).accepts(Some(now - Duration::from_secs(60 * 60 * 48)), now)
        );
    }

    #[test]
    fn every_attribute_filter_is_three_valued() {
        let now = SystemTime::now();
        let mut q = Query::new(root());
        q.attrs.hidden = Tri::No;
        let compiled = q.compile().expect("attrs");
        assert!(compiled.attrs_match(&file("visible"), now));
        assert!(!compiled.attrs_match(&file(".hidden"), now));

        let mut q = Query::new(root());
        q.attrs.directories = Tri::Yes;
        let compiled = q.compile().expect("attrs");
        assert!(compiled.attrs_match(&Entry::dir("src"), now));
        assert!(!compiled.attrs_match(&file("main.rs"), now));

        // The cycle the `Space` key walks, and the label under it.
        assert_eq!(Tri::Ignore.next(), Tri::Yes);
        assert_eq!(Tri::Yes.next(), Tri::No);
        assert_eq!(Tri::No.next(), Tri::Ignore);
        assert_eq!(Tri::Ignore.label(), " ");
    }

    #[test]
    fn a_symlink_and_a_read_only_file_are_told_apart_by_their_bits() {
        let now = SystemTime::now();
        let mut q = Query::new(root());
        q.attrs.read_only = Tri::Yes;
        let compiled = q.compile().expect("attrs");

        let mut writable = file("a");
        writable.mode = 0o644;
        let mut locked = file("b");
        locked.mode = 0o444;
        assert!(!compiled.attrs_match(&writable, now));
        assert!(compiled.attrs_match(&locked, now));

        let mut q = Query::new(root());
        q.attrs.symlinks = Tri::Yes;
        let compiled = q.compile().expect("attrs");
        let mut link = file("l");
        link.kind = EntryKind::Symlink { to_dir: false };
        assert!(compiled.attrs_match(&link, now));
        assert!(!compiled.attrs_match(&file("plain"), now));
    }

    #[test]
    fn the_header_says_which_listing_you_are_in() {
        let mut q = Query::new(VfsPath::local("/srv/dev"));
        q.name = "*.rs".to_string();
        assert_eq!(q.header(), "[search: *.rs in /srv/dev]");

        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            ..ContentQuery::default()
        });
        assert_eq!(q.header(), "[search: *.rs \"TODO\" in /srv/dev]");

        q.roots.push(VfsPath::local("/srv/other"));
        q.roots.push(VfsPath::local("/srv/third"));
        assert_eq!(q.header(), "[search: *.rs \"TODO\" in /srv/dev +2]");

        // An empty mask is the `*` it means.
        let mut q = Query::new(VfsPath::local("/srv/dev"));
        q.depth = Depth::None;
        assert_eq!(q.header(), "[search: * in /srv/dev]");

        // Branch view says so, and says nothing else.
        let branch = Query::branch(VfsPath::local("/srv/dev"));
        assert!(branch.is_branch());
        assert_eq!(branch.header(), "[branch: /srv/dev]");

        // Every character of a header is ASCII, whatever was typed.
        assert!(q.header().is_ascii());
    }

    #[test]
    fn a_long_pattern_is_cropped_in_the_header_and_nowhere_else() {
        let mut q = Query::new(VfsPath::local("/srv/dev"));
        let pattern = "x".repeat(80);
        q.content = Some(ContentQuery {
            pattern: pattern.clone(),
            ..ContentQuery::default()
        });
        let header = q.header();
        assert!(header.contains("..."), "{header}");
        assert!(header.len() < pattern.len(), "{header}");
        // The query itself is untouched: the crop is the header's business.
        assert_eq!(
            q.content.as_ref().map(|c| c.pattern.len()),
            Some(pattern.len())
        );
    }

    #[test]
    fn a_branch_view_is_a_search_with_no_pattern() {
        // `Ctrl+B` and an untouched `Alt+F7` over the same root are the same
        // query, which is what makes them the same engine.
        assert_eq!(Query::branch(root()), Query::new(root()));

        // Anything at all in the dialog stops it being a branch view.
        let mut q = Query::branch(root());
        q.name = "*.rs".to_string();
        assert!(!q.is_branch());

        let mut q = Query::branch(root());
        q.depth = Depth::Levels(1);
        assert!(!q.is_branch());

        let mut q = Query::branch(root());
        q.attrs.hidden = Tri::Yes;
        assert!(!q.is_branch());
    }

    #[test]
    fn the_viewer_opens_with_the_pattern_that_found_the_hit() {
        use crate::viewer::find::FindKind;

        let mut q = Query::new(root());
        q.content = Some(ContentQuery {
            pattern: "TODO".to_string(),
            case_sensitive: true,
            ..ContentQuery::default()
        });
        let find = q.compile().expect("plain").viewer_find().expect("a query");
        assert_eq!(find.input, "TODO");
        assert_eq!(find.kind, FindKind::Text);
        assert_eq!(find.case, crate::config::QuickSearchCase::Sensitive);

        // A hex search hands the viewer the same hex pattern.
        q.content = Some(ContentQuery {
            pattern: "DE AD BE EF".to_string(),
            mode: TextMode::Hex,
            ..ContentQuery::default()
        });
        let find = q.compile().expect("hex").viewer_find().expect("a query");
        assert_eq!(find.kind, FindKind::Hex);

        // A regex search hands it nothing rather than something that means
        // something else.
        q.content = Some(ContentQuery {
            pattern: "TO+DO".to_string(),
            mode: TextMode::Regex,
            ..ContentQuery::default()
        });
        assert!(q.compile().expect("regex").viewer_find().is_none());

        // A name-only search has nothing to hand it either.
        assert!(
            Query::new(root())
                .compile()
                .expect("names")
                .viewer_find()
                .is_none()
        );
    }
}
