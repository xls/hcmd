//! Mode 3's find: over the text on screen, not over the file's bytes.
//!
//! Modes 1 and 2 search the file, streaming, because the file is the thing
//! they are showing and it may be forty gigabytes of it. Mode 3 shows a
//! *document* - JSON laid out, HTML with its tags resolved, a binary read
//! through a template - and the text on screen is frequently nowhere in the
//! file in that form. Searching the bytes there would answer a question the
//! reader did not ask: `beta` is on screen, and in the file it may be
//! `beta`, split across a tag, or a field name the template supplied that
//! the file never contained at all.
//!
//! So mode 3 searches what it draws. `/` in mode 1 finds every occurrence in
//! the text; `/` in mode 3 finds every occurrence in the document.
//!
//! # Why this needs none of the streaming machinery
//!
//! The rendered document is already whole, in memory, as
//! [`super::render::Rendered`] - that is what the mode's size ceiling buys.
//! There is no window to slide, no chunk boundary for a match to straddle, and
//! no background counter: the hits are all found in one pass over lines that
//! are already `String`s, so `3/57` is exact and never wears a `+`.
//!
//! The one bound kept is [`MAX_RENDER_HITS`], for the same reason the byte
//! search caps its hit list: highlighting needs the hits on screen, and
//! stepping needs the next one, and neither needs a million of them.

use std::ops::Range;

use super::find::Found;
use super::*;

/// The most hits mode 3 keeps.
///
/// A document that is one repeated character and a search for that character
/// is bounded by the mode's own size ceiling, so this is a second bound rather
/// than the only one. Stepping past the last kept hit wraps, which is what
/// stepping does at the end of the list anyway.
pub const MAX_RENDER_HITS: usize = 100_000;

/// Where one match is in the rendered document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderHit {
    /// Which rendered line it is on.
    pub line: usize,
    /// Where in that line's text, as a byte range.
    pub range: Range<usize>,
}

impl Viewer {
    /// Every match in the rendered document, and the one the cursor lands on.
    ///
    /// Called when the query changes, and when mode 3 is entered with a query
    /// already typed. Lands on the first hit at or after the rendered cursor
    /// so that typing continues from where the reader is looking, and wraps to
    /// the first hit in the document when there is none below.
    pub(super) fn run_find_rendered(&mut self) -> Found {
        self.render_hits.clear();
        self.render_hit = None;
        let (Some(matcher), Some(document)) =
            (self.find.matcher().cloned(), self.rendered.as_ref())
        else {
            return Found::None;
        };
        for (line, source) in document.lines.iter().enumerate() {
            if self.render_hits.len() >= MAX_RENDER_HITS {
                break;
            }
            for range in matcher.matches_in(source.text.as_bytes()) {
                self.render_hits.push(RenderHit { line, range });
            }
        }
        self.render_hits_built = true;
        let from = self.render_cursor;
        let at = self
            .render_hits
            .iter()
            .position(|hit| hit.line >= from)
            .or_else(|| self.render_hits.first().map(|_| 0));
        self.land_on_render_hit(at)
    }

    /// `n` and `Shift+N` in mode 3: the next hit, wrapping at either end.
    ///
    /// Wrapping rather than stopping, because the whole document was searched:
    /// "no match below" and "no match" are the same statement here, unlike in
    /// the streaming search where the rest of the file is genuinely unread.
    pub(super) fn find_step_rendered(&mut self, forward: bool) -> Found {
        // A pattern can be live without ever having been run here: `seed_find`
        // installs the session's last search and compiles it without
        // searching, so the first `n` after opening a document would otherwise
        // step an empty list and report a word that is plainly on screen as
        // missing. Building it here costs one pass over lines already in
        // memory, and only the first time.
        if !self.render_hits_built {
            let found = self.run_find_rendered();
            if !matches!(found, Found::None) {
                return found;
            }
        }
        if self.render_hits.is_empty() {
            return Found::None;
        }
        let last = self.render_hits.len().saturating_sub(1);
        let at = match (self.render_hit, forward) {
            (Some(at), true) if at >= last => 0,
            (Some(at), true) => at.saturating_add(1),
            (Some(0), false) => last,
            (Some(at), false) => at.saturating_sub(1),
            // No current hit: `n` takes the first, `Shift+N` the last.
            (None, true) => 0,
            (None, false) => last,
        };
        self.land_on_render_hit(Some(at))
    }

    /// Put the cursor on hit `at` and scroll it into view.
    fn land_on_render_hit(&mut self, at: Option<usize>) -> Found {
        let Some(at) = at else {
            return Found::None;
        };
        let Some(hit) = self.render_hits.get(at) else {
            return Found::None;
        };
        let line = hit.line;
        self.render_hit = Some(at);
        self.render_cursor = line;
        self.reveal_render();
        // The offset a `Found` carries is a file position and mode 3 has none;
        // the line is the answer here, and it has already been applied. The
        // value is what the status line counts with.
        Found::Hit(line as u64)
    }

    /// Which hit the cursor is on, and how many there are: `3/57`.
    ///
    /// `None` when nothing has been searched for. The count is exact - the
    /// whole document was read to produce it.
    #[must_use]
    pub fn render_find_counter(&self) -> Option<(usize, usize)> {
        if self.render_hits.is_empty() {
            return None;
        }
        Some((
            self.render_hit.map_or(0, |at| at.saturating_add(1)),
            self.render_hits.len(),
        ))
    }

    /// Forget mode 3's hits.
    ///
    /// They are line numbers into one particular rendered document; a new
    /// document, or another mode, makes every one of them meaningless.
    pub(super) fn clear_render_hits(&mut self) {
        self.render_hits.clear();
        self.render_hit = None;
        self.render_hits_built = false;
    }

    /// The find bar, with mode 3's own counter in it.
    ///
    /// Delegates for every other mode. The counter is exact here, so unlike
    /// the streaming one it never carries a `+`.
    #[must_use]
    pub fn find_bar_text(&self) -> String {
        if !matches!(self.mode, ViewerMode::Render) {
            return self.find.bar_text();
        }
        let counter = self
            .render_find_counter()
            .map(|(at, total)| format!("{at}/{total}"))
            .or_else(|| self.find.query().input.is_empty().then(String::new));
        self.find.bar_text_with(counter.filter(|c| !c.is_empty()))
    }

    /// The matches on one rendered line, for the row builder.
    #[must_use]
    pub(super) fn render_matches_on(&self, line: usize) -> Vec<MatchRun> {
        let current = self.render_hit.and_then(|at| self.render_hits.get(at));
        self.render_hits
            .iter()
            .filter(|hit| hit.line == line)
            .map(|hit| MatchRun {
                range: hit.range.clone(),
                current: current.is_some_and(|now| now == hit),
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "find_render_tests.rs"]
mod tests;
