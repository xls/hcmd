//! The links in a rendered document, and the one the cursor is on.
//!
//! Nothing is remembered: the focused link is whichever link the rendered
//! cursor stands in, so moving the cursor off it - an arrow, a page, a fold -
//! is what unfocuses it, and `Tab` is a search from where the cursor is for
//! the next link, which it then puts the cursor on. The links are read off
//! the rendered lines each time, which is cheap for a document bounded by
//! `viewer.render.max_size` and cannot go stale when the document is
//! rendered again.

use std::ops::Range;

use super::Viewer;
use super::render::Link;

impl Viewer {
    /// The links on one rendered line, or none for a line that is not there.
    fn links_on(&self, line: usize) -> &[Link] {
        self.rendered
            .as_ref()
            .and_then(|rendered| rendered.lines.get(line))
            .map_or(&[], |rendered| rendered.links.as_slice())
    }

    /// The link the cursor stands in: on its line, at a column the link's
    /// label covers. A collapsed line shows its summary, not its links, so
    /// nothing on it is focused.
    #[must_use]
    pub fn focused_link(&self) -> Option<&Link> {
        if self.render_folds.contains(&self.render_cursor) {
            return None;
        }
        let col = self.render_col;
        self.links_on(self.render_cursor)
            .iter()
            .find(|link| link.range.contains(&col))
    }

    /// The focused link's range on `line`, for the row painter to mark.
    pub(super) fn focused_link_on(&self, line: usize) -> Option<Range<usize>> {
        if line != self.render_cursor {
            return None;
        }
        self.focused_link().map(|link| link.range.clone())
    }

    /// Step to the next link after the cursor (or the previous one before
    /// it, backwards), wrapping at either end, and put the cursor on its
    /// first character. Links inside a collapsed fold are skipped: a link
    /// that is not drawn cannot be reached.
    ///
    /// Returns the target now under the cursor, or `None` when the document
    /// has no visible links at all.
    pub fn step_link(&mut self, forward: bool) -> Option<String> {
        let folded = self.folded();
        // Every visible link as (line, column), in document order.
        let all: Vec<(usize, usize)> = self
            .rendered
            .as_ref()?
            .lines
            .iter()
            .enumerate()
            .filter(|(line, _)| folded.shows(*line))
            .flat_map(|(line, rendered)| {
                rendered
                    .links
                    .iter()
                    .map(move |link| (line, link.range.start))
            })
            .collect();
        if all.is_empty() {
            return None;
        }
        let here = (self.render_cursor, self.render_col);
        let next = if forward {
            all.iter().position(|&at| at > here).unwrap_or(0)
        } else {
            all.iter()
                .rposition(|&at| at < here)
                .unwrap_or(all.len().saturating_sub(1))
        };
        let &(line, col) = all.get(next)?;
        self.render_cursor = line;
        self.render_col = col;
        self.render_goal = col;
        self.reveal_render();
        self.focused_link().map(|link| link.target.clone())
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
