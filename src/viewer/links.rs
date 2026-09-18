//! The links in a rendered document, and the one the cursor is on.
//!
//! Mode 3 has no byte cursor - its unit is a drawn line - so a link is reached
//! by walking link to link with `Tab` and `Shift+Tab`, which puts the cursor
//! on the link's line and marks the link itself. Nothing is cached: the links
//! are read off the rendered lines each time, which is cheap for a document
//! bounded by `viewer.render.max_size` and cannot go stale when the document
//! is rendered again.

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

    /// The focused link, when there is one and the cursor is still on its
    /// line. Moving the cursor away is how `Enter` goes back to meaning fold.
    #[must_use]
    pub fn focused_link(&self) -> Option<&Link> {
        let (line, index) = self.render_link?;
        if line != self.render_cursor {
            return None;
        }
        self.links_on(line).get(index)
    }

    /// The focused link's range on `line`, for the row painter to mark.
    pub(super) fn focused_link_on(&self, line: usize) -> Option<std::ops::Range<usize>> {
        let (at, index) = self.render_link?;
        if at != line {
            return None;
        }
        self.links_on(line)
            .get(index)
            .map(|link| link.range.clone())
    }

    /// Step the focus to the next link (or the previous, backwards), wrapping
    /// at either end, and bring the cursor to its line. Links inside a
    /// collapsed fold are skipped: a link that is not drawn cannot be focused.
    ///
    /// Returns the target now focused, or `None` when the document has no
    /// visible links at all.
    pub fn step_link(&mut self, forward: bool) -> Option<String> {
        let folded = self.folded();
        let all: Vec<(usize, usize)> = self
            .rendered
            .as_ref()?
            .lines
            .iter()
            .enumerate()
            .filter(|(line, _)| folded.shows(*line))
            .flat_map(|(line, rendered)| (0..rendered.links.len()).map(move |i| (line, i)))
            .collect();
        if all.is_empty() {
            return None;
        }
        let current = self
            .render_link
            .and_then(|focus| all.iter().position(|&candidate| candidate == focus));
        let next = match (current, forward) {
            (Some(at), true) => at.saturating_add(1) % all.len(),
            (Some(at), false) => (at.saturating_add(all.len()).saturating_sub(1)) % all.len(),
            (None, true) => 0,
            (None, false) => all.len().saturating_sub(1),
        };
        let &(line, index) = all.get(next)?;
        self.render_link = Some((line, index));
        self.render_cursor = line;
        self.reveal_render();
        self.links_on(line)
            .get(index)
            .map(|link| link.target.clone())
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
