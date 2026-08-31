//! Mode 3: the file as what it *is*, not as its source.
//!
//! Text mode colours a document's source and hex mode shows its bytes. This
//! shows the document: a JSON file as a tree that folds, an HTML page as the
//! text it renders to, a Markdown file with its headings and lists made rather
//! than spelled.
//!
//! # Why this one has a ceiling
//!
//! Everything else in the viewer streams. The window is bounded by the
//! terminal and a 40 GB file opens as fast as a 4 KB one, which is the
//! promise the whole module is built around.
//!
//! A rendered view cannot keep it. There is no way to show the tree of a JSON
//! document without having read the document: the closing brace that says how
//! many keys an object has is at the far end of the file. So mode 3 reads the
//! whole file, and therefore refuses to open one bigger than
//! `viewer.render.max_size` - naming the limit and the setting, because the
//! alternative is rendering the first megabyte and calling it the document,
//! which is a wrong answer given confidently.
//!
//! `viewer.highlight.max_size` is the same shape of decision one step milder:
//! above it the colours go away and the file still opens, because colour is a
//! garnish. A tree of the first part of a file is not a garnish, it is a lie,
//! so this refuses instead of degrading.
//!
//! # No new dependencies
//!
//! All three renderers are written here. `serde_json` and `pulldown-cmark`
//! would each have been the obvious choice, and each was measured against the
//! release binary's 24 MB ceiling, which had 380 kB of headroom left. Neither
//! fits. What is needed here is also less than either offers: nothing
//! deserializes into Rust types and nothing round-trips, so a scanner that
//! walks the text once and emits lines is the whole job.

pub mod diff;
pub mod entities;
pub mod facts;
pub mod html;
pub mod json;
pub mod json_scan;
pub mod line;
pub mod markdown;
pub mod markdown_inline;

use std::collections::BTreeMap;

pub use facts::summary_document;
pub(crate) use line::{LineBuf, indent};

use super::highlight::Span;

/// Which renderer a file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderKind {
    /// A tree, foldable.
    Json,
    /// The page's text, tags gone.
    Html,
    /// Headings, lists and code blocks made rather than spelled.
    Markdown,
    /// Two files, or one file and what git has for it, as a unified diff.
    /// Produced by [`diff::render`] and never by [`render`], which has one
    /// text and a diff needs two.
    Diff,
    /// Not a renderer at all: a binary whose format a template recognised,
    /// shown as what that template says it is. See [`summary_document`].
    Summary,
}

impl RenderKind {
    /// The name shown in the status line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Json => "json",
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Summary => "summary",
        }
    }

    /// Which renderer a file name asks for, or `None` for one nothing renders.
    ///
    /// By extension alone, and deliberately: sniffing the contents would have
    /// to read the file to decide whether it is allowed to read the file, and
    /// the size ceiling above is the thing that decides that. A `.json` that
    /// turns out not to be JSON is caught by the parse, which is the honest
    /// place to catch it.
    #[must_use]
    pub fn of_name(name: &str) -> Option<Self> {
        let ext = name.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "json" | "jsonc" | "geojson" | "ipynb" | "webmanifest" => Self::Json,
            "html" | "htm" | "xhtml" => Self::Html,
            "md" | "markdown" | "mdown" | "mkd" => Self::Markdown,
            _ => return None,
        })
    }
}

/// A region a line opens, which folding collapses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// The last line index inside the region, so the region is
    /// `line + 1 ..= through`. A region with nothing in it is not recorded.
    pub through: usize,
    /// What the line reads as when it is collapsed: `{...} 12 keys`.
    pub summary: String,
}

/// One line of a rendered document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLine {
    /// The text, indentation included. Never contains a newline or a tab.
    pub text: String,
    /// Colour runs, as byte ranges into `text`, in the same shape the
    /// highlighter produces so the renderer needs no second code path.
    pub spans: Vec<Span>,
    /// Set when this line opens a foldable region.
    pub fold: Option<Fold>,
}

impl RenderLine {
    /// A line with no colour on it.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            spans: Vec::new(),
            fold: None,
        }
    }
}

/// A rendered document: the lines, and which renderer made them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// Which renderer produced this.
    pub kind: RenderKind,
    /// What the status line calls it: the renderer's name, or the template's
    /// where a template is what is being shown. One field rather than two
    /// cases at the far end, so the status line has one thing to print.
    pub label: String,
    /// Every line, in order, with folds unapplied.
    pub lines: Vec<RenderLine>,
}

impl Rendered {
    /// How many lines there are before any folding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True for a document with no lines at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Every foldable line, as `line -> through`.
    ///
    /// Built once when the document is rendered rather than walked per frame:
    /// the answer is a function of the document and never of the fold state.
    #[must_use]
    pub fn foldable(&self) -> BTreeMap<usize, usize> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(at, line)| line.fold.as_ref().map(|f| (at, f.through)))
            .collect()
    }
}

/// The lines a set of collapsed folds hides, and how to walk what is left.
///
/// The fold state lives on the viewer and the document is immutable, so this
/// is the piece that puts the two together. It is a borrow rather than a
/// rebuilt list because a 4 MB JSON file is hundreds of thousands of lines and
/// materialising the visible ones every frame would be the one allocation in
/// the viewer that scales with the file.
#[derive(Debug, Clone, Copy)]
pub struct Folded<'a> {
    /// Every foldable line and where its region ends.
    regions: &'a BTreeMap<usize, usize>,
    /// Which of them are collapsed.
    collapsed: &'a std::collections::BTreeSet<usize>,
    /// How many lines the document has.
    len: usize,
}

impl<'a> Folded<'a> {
    /// Put the document's regions together with the collapsed set.
    #[must_use]
    pub const fn new(
        regions: &'a BTreeMap<usize, usize>,
        collapsed: &'a std::collections::BTreeSet<usize>,
        len: usize,
    ) -> Self {
        Self {
            regions,
            collapsed,
            len,
        }
    }

    /// The collapsed region that hides `line`, if one does.
    ///
    /// The outermost one: an object inside a collapsed object is hidden by the
    /// outer fold whether or not it is collapsed itself, and jumping to the
    /// inner one would land on a line that is not on screen.
    fn hider(&self, line: usize) -> Option<usize> {
        let mut found = None;
        for at in self.collapsed {
            if *at >= line {
                break;
            }
            if self.regions.get(at).is_some_and(|through| line <= *through) {
                found = Some(*at);
                break;
            }
        }
        found
    }

    /// Is `line` drawn?
    #[must_use]
    pub fn shows(&self, line: usize) -> bool {
        line < self.len && self.hider(line).is_none()
    }

    /// The first drawn line at or after `line`, or `None` past the end.
    #[must_use]
    pub fn at_or_after(&self, line: usize) -> Option<usize> {
        let mut at = line;
        while at < self.len {
            match self.hider(at) {
                // Skip the whole region in one step rather than a line at a
                // time: a collapsed object of ten thousand lines must not cost
                // ten thousand comparisons to step over.
                Some(start) => at = self.regions.get(&start).map_or(at, |t| t.saturating_add(1)),
                None => return Some(at),
            }
        }
        None
    }

    /// The drawn line after `line`, or `None` at the end.
    #[must_use]
    pub fn next(&self, line: usize) -> Option<usize> {
        match self.regions.get(&line) {
            // A collapsed region's own line is drawn; everything inside is not.
            Some(through) if self.collapsed.contains(&line) => {
                self.at_or_after(through.saturating_add(1))
            }
            _ => self.at_or_after(line.saturating_add(1)),
        }
    }

    /// The drawn line before `line`, or `None` at the top.
    #[must_use]
    pub fn prev(&self, line: usize) -> Option<usize> {
        let mut at = line.checked_sub(1)?;
        // Walking out of nested collapsed regions, outermost last.
        while let Some(start) = self.hider(at) {
            at = start;
        }
        Some(at)
    }

    /// Up to `count` drawn lines starting at `from`.
    #[must_use]
    pub fn window(&self, from: usize, count: usize) -> Vec<usize> {
        let mut out = Vec::with_capacity(count);
        let mut at = self.at_or_after(from);
        while out.len() < count {
            let Some(line) = at else {
                break;
            };
            out.push(line);
            at = self.next(line);
        }
        out
    }
}

/// Render `text` as `kind`, or `None` where it does not parse as that.
///
/// A `None` is not a failure of the file: a `.json` that is really a log is a
/// perfectly good file that mode 3 has nothing to say about, and the viewer
/// falls back to text mode and says so rather than showing an empty screen.
#[must_use]
pub fn render(kind: RenderKind, text: &str) -> Option<Rendered> {
    let lines = match kind {
        RenderKind::Json => json::render(text)?,
        RenderKind::Html => html::render(text),
        RenderKind::Markdown => markdown::render(text),
        // Not a renderer over text: it is built from a template and the
        // file's head, by `summary_document`, which is the only thing that
        // can produce this kind.
        // Neither is produced from one text: a summary comes from a template
        // and the file's head, and a diff needs a second side.
        RenderKind::Summary | RenderKind::Diff => return None,
    };
    Some(Rendered {
        kind,
        label: kind.label().to_string(),
        lines,
    })
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
