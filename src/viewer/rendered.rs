//! Mode 3's half of the viewer: reading the document, and moving around it.
//!
//! Everything here is about a **rendered line index** rather than a byte
//! offset, which is the one place this viewer departs from its own rule that a
//! position is a byte. It has to: a rendered line does not correspond to a run
//! of bytes at all - an HTML paragraph is assembled from text that was
//! scattered across a dozen tags - so there is no offset to keep.
//!
//! That is why mode 3 has its own cursor, its own window and its own
//! navigation, and why the byte cursor is left exactly where it was. Going
//! back to `1` or `2` finds the position that was there before, rather than a
//! position invented from a line number that never meant an offset.

use std::collections::{BTreeMap, BTreeSet};

use super::refusal::RenderRefusal;
use super::render;
use super::render::{RenderKind, Rendered, render};
use super::select::Motion;
use super::source::WindowLen;
use super::*;

impl Viewer {
    /// The rendered document, when mode 3 has one.
    #[must_use]
    pub const fn rendered(&self) -> Option<&Rendered> {
        self.rendered.as_ref()
    }

    /// Which line the rendered view's cursor is on.
    #[must_use]
    pub const fn render_cursor(&self) -> usize {
        self.render_cursor
    }

    /// The first rendered line on screen.
    #[must_use]
    pub const fn render_top(&self) -> usize {
        self.render_top
    }

    /// What mode 3 is showing, as the status line names it.
    #[must_use]
    pub fn status_render_label(&self) -> Option<String> {
        self.rendered.as_ref().map(|r| r.label.clone())
    }

    /// Why mode 3 is not showing what was asked for, if it is not.
    #[must_use]
    pub fn render_note(&self) -> Option<&str> {
        self.render_note.as_deref()
    }

    /// The document's folds against the collapsed set, ready to walk.
    #[must_use]
    pub fn folded(&self) -> render::Folded<'_> {
        render::Folded::new(
            &self.render_regions,
            &self.render_folds,
            self.rendered.as_ref().map_or(0, Rendered::len),
        )
    }

    /// Read the whole file, up to `limit` bytes.
    ///
    /// One window at a time, because that is the only size a [`Source`] hands
    /// out and the cap exists so no other part of the viewer can accidentally
    /// read a file whole. This is the one place that is allowed to, and the
    /// ceiling it is called with is what makes that safe.
    pub(crate) fn read_all(&mut self, limit: u64) -> Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let mut at = 0_u64;
        while (out.len() as u64) < limit {
            let window = self.source.read_window(at, WindowLen::MAX)?;
            let bytes = window.bytes();
            if bytes.is_empty() {
                break;
            }
            out.extend_from_slice(bytes);
            at = at.saturating_add(bytes.len() as u64);
        }
        out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(out)
    }

    /// Build the rendered document, or say why there is not one.
    ///
    /// Called when mode 3 is entered and on `F2`, and nowhere else: this is
    /// the read the size ceiling exists to bound, and doing it once per mode
    /// switch rather than once per frame is what keeps mode 3 from being the
    /// slow mode.
    pub fn build_render(
        &mut self,
        limit: u64,
        head: usize,
    ) -> std::result::Result<(), RenderRefusal> {
        self.rendered = None;
        self.render_regions.clear();
        self.render_folds.clear();
        self.render_top = 0;
        self.render_cursor = 0;

        // A diff first, and instead of everything below: this viewer is
        // showing two files, and what its own file is made of is not the
        // question. The other side was read once by the event loop; nothing
        // here reads anything but the file the viewer already holds.
        if let Some(other) = self.diff_old.clone().filter(|_| self.diff_shown) {
            let fits = self.source.len().is_none_or(|len| len <= limit);
            if !fits {
                return Err(RenderRefusal::TooBig {
                    len: self.source.len().unwrap_or(0),
                    limit,
                });
            }
            let bytes = self
                .read_all(limit)
                .map_err(|_| RenderRefusal::NoRenderer)?;
            let (text, _) = self.encoding.decode(&bytes);
            // Both sides named the same way. The viewer's title is the whole
            // path, and `--- a.txt` against `+++ /tmp/long/path/b.txt` reads
            // as two different kinds of thing rather than as two files.
            let mine = self
                .path
                .as_ref()
                .and_then(crate::vfs::VfsPath::file_name)
                .unwrap_or_else(|| self.title.clone());
            let document = super::render::diff::render(&other.text, &text, &other.label, &mine)
                .ok_or(RenderRefusal::NoRenderer)?;
            self.install_render(document);
            return Ok(());
        }

        let kind = RenderKind::of_name(&self.title);
        let fits = self.source.len().is_none_or(|len| len <= limit);
        // One read for both attempts below. Where a renderer applies and the
        // file fits, that is the whole file and the head is a prefix of it;
        // where it does not, only the head is needed and only the head is
        // read.
        let want = if kind.is_some() && fits {
            limit
        } else {
            head as u64
        };
        let bytes = self.read_all(want).map_err(|_| RenderRefusal::NoRenderer)?;

        // Before anything that reads the bytes as text: a compiled Android
        // manifest is not text and would decode to nothing legible. It is
        // recognised by its own header, so a `.xml` that is really one is
        // caught and a `.xml` that is written by hand is not.
        if let Some(document) = super::axml::decode(&bytes).map(|text| Rendered {
            kind: RenderKind::Axml,
            label: RenderKind::Axml.label().to_string(),
            lines: text.lines().map(super::render::RenderLine::plain).collect(),
        }) {
            self.install_render(document);
            return Ok(());
        }

        // First: what the file is made of, where a renderer can read it.
        let refusal = match kind {
            None => RenderRefusal::NoRenderer,
            Some(_) if !fits => RenderRefusal::TooBig {
                len: self.source.len().unwrap_or(0),
                limit,
            },
            Some(kind) => {
                let (text, _) = self.encoding.decode(&bytes);
                if let Some(document) = render(kind, &text) {
                    self.install_render(document);
                    return Ok(());
                }
                RenderRefusal::NotThatFormat(kind)
            }
        };

        // Then: what the file *is*, where a template recognises it. The
        // fallback and never the first choice - a `.json` file is a tree, not
        // "JSON document, 4 keys". It needs the head and not the whole file,
        // so it is worth trying even for one too big to render.
        if let Some(found) =
            crate::viewer::summary::auto_match(crate::viewer::fileinfo::builtin(), &bytes)
            && let Some(document) = render::summary_document(found, &bytes)
        {
            self.install_render(document);
            return Ok(());
        }
        Err(refusal)
    }

    /// Take a built document and make it the one on screen.
    fn install_render(&mut self, document: Rendered) {
        // A diff opens with its unchanged runs already collapsed, which is
        // what `diff -u` does by dropping them and what a reader means by
        // "show me the diff". Every other document opens expanded: a JSON
        // tree collapsed to `{...}` would be one row.
        let collapse = document.kind == RenderKind::Diff;
        self.render_regions = document.foldable();
        self.rendered = Some(document);
        if collapse {
            self.fold_all(true);
        }
    }

    /// How many rendered rows the window shows.
    fn render_rows(&self) -> usize {
        usize::from(self.view_rows.max(1))
    }

    /// The lines the window shows, as document line indexes.
    #[must_use]
    pub fn render_window(&self) -> Vec<usize> {
        self.folded().window(self.render_top, self.render_rows())
    }

    /// Move the rendered cursor, and bring the window with it.
    pub(crate) fn move_render(&mut self, motion: Motion) {
        let folded = self.folded();
        let last = self.rendered.as_ref().map_or(0, Rendered::len);
        let page = self.render_rows().saturating_sub(1).max(1);
        let mut at = self.render_cursor;
        match motion {
            Motion::Up | Motion::Left => at = folded.prev(at).unwrap_or(at),
            Motion::Down | Motion::Right => at = folded.next(at).unwrap_or(at),
            Motion::PageUp => {
                for _ in 0..page {
                    at = folded.prev(at).unwrap_or(at);
                }
            }
            Motion::PageDown => {
                for _ in 0..page {
                    at = folded.next(at).unwrap_or(at);
                }
            }
            Motion::FileStart | Motion::RowStart => at = folded.at_or_after(0).unwrap_or(0),
            Motion::FileEnd | Motion::RowEnd => {
                // Walking back from the end rather than forward from the top:
                // a folded document's last drawn line is not its last line,
                // and walking forward through a million of them to find out
                // would make `Ctrl+End` the slow key.
                let mut back = last.saturating_sub(1);
                while back > 0 && !folded.shows(back) {
                    back = folded.prev(back).unwrap_or(0);
                }
                at = back;
            }
        }
        self.render_cursor = at;
        self.reveal_render();
    }

    /// Bring the window to the rendered cursor by the shortest scroll.
    pub(crate) fn reveal_render(&mut self) {
        let rows = self.render_rows();
        let folded = self.folded();
        if self.render_cursor < self.render_top {
            self.render_top = self.render_cursor;
            return;
        }
        // How far down the window the cursor is, counting only drawn lines.
        let mut at = folded.at_or_after(self.render_top);
        for _ in 0..rows {
            match at {
                Some(line) if line == self.render_cursor => return,
                Some(line) => at = folded.next(line),
                None => break,
            }
        }
        // Below the window: put the cursor on the last row by walking back
        // `rows - 1` drawn lines from it.
        let mut top = self.render_cursor;
        for _ in 0..rows.saturating_sub(1) {
            top = folded.prev(top).unwrap_or(top);
        }
        self.render_top = top;
    }

    /// Scroll the rendered window without moving the cursor.
    pub(crate) fn scroll_render(&mut self, delta: isize) {
        let folded = self.folded();
        let mut at = self.render_top;
        for _ in 0..delta.unsigned_abs() {
            at = if delta < 0 {
                folded.prev(at).unwrap_or(at)
            } else {
                folded.next(at).unwrap_or(at)
            };
        }
        self.render_top = at;
    }

    /// `Enter` in mode 3: collapse or expand the region on the cursor line.
    ///
    /// Returns what to say, which is either the count that has just been
    /// hidden or the reason there was nothing to fold - a key that appears to
    /// do nothing is the thing this avoids.
    pub fn toggle_fold(&mut self) -> String {
        if self.mode != ViewerMode::Render {
            return "folding is a mode 3 thing; press 3".to_string();
        }
        let at = self.render_cursor;
        if !self.render_regions.contains_key(&at) {
            return "nothing to fold on this line".to_string();
        }
        if self.render_folds.remove(&at) {
            return "expanded".to_string();
        }
        self.render_folds.insert(at);
        // The cursor stays on the line that was folded, which is still drawn.
        self.reveal_render();
        "collapsed".to_string()
    }

    /// Collapse or expand everything foldable.
    pub fn fold_all(&mut self, collapse: bool) -> String {
        if collapse {
            self.render_folds = self.render_regions.keys().copied().collect();
        } else {
            self.render_folds.clear();
        }
        // The cursor may now be inside something collapsed; the nearest drawn
        // line at or before it is where it belongs.
        let folded = self.folded();
        if !folded.shows(self.render_cursor) {
            self.render_cursor = folded.prev(self.render_cursor).unwrap_or(0);
        }
        self.reveal_render();
        if collapse {
            format!("collapsed {} regions", self.render_regions.len())
        } else {
            "expanded everything".to_string()
        }
    }

    /// The fold state, for tests.
    #[must_use]
    pub const fn render_folds(&self) -> &BTreeSet<usize> {
        &self.render_folds
    }

    /// Every foldable line, for tests.
    #[must_use]
    pub const fn render_regions(&self) -> &BTreeMap<usize, usize> {
        &self.render_regions
    }
}

#[cfg(test)]
#[path = "rendered_tests.rs"]
mod tests;
