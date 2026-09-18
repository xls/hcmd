//! Building one rendered line, with its colour runs.
//!
//! Every renderer appends through here so that a span's byte range and the
//! text it describes cannot drift apart. Nothing else in this module tree
//! touches a `RenderLine`'s string directly.

use super::super::highlight::{Span, SynSlot};
use super::{Link, RenderLine};

/// A line being built, with its colour runs.
///
/// Shared by the three renderers so that a span's byte range and the text it
/// describes cannot drift apart: nothing appends to the string without going
/// through here.
#[derive(Debug, Default)]
pub(crate) struct LineBuf {
    text: String,
    spans: Vec<Span>,
    links: Vec<Link>,
}

impl LineBuf {
    /// How many bytes have been appended so far - the offset the next piece
    /// starts at, which is what a link needs to remember its own start.
    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }

    /// Record that everything appended since `start` is a link to `target`.
    ///
    /// An empty target, or a range with nothing in it, records nothing: a link
    /// with no label has no text to focus.
    pub(crate) fn link_from(&mut self, start: usize, target: &str) {
        let end = self.text.len();
        if target.is_empty() || start >= end {
            return;
        }
        // The HTML walker puts its own space before a word, so a range that
        // began at the anchor's open tag begins on that space; the link is the
        // label, not the whitespace around it.
        let Some(slice) = self.text.get(start..end) else {
            return;
        };
        let leading = slice.len().saturating_sub(slice.trim_start().len());
        let trailing = slice.len().saturating_sub(slice.trim_end().len());
        let (start, end) = (start.saturating_add(leading), end.saturating_sub(trailing));
        if start >= end {
            return;
        }
        self.links.push(Link {
            range: start..end,
            target: target.to_string(),
        });
    }

    /// Append `piece`, coloured with `slot`.
    pub(crate) fn push(&mut self, piece: &str, slot: Option<SynSlot>) {
        if piece.is_empty() {
            return;
        }
        let from = self.text.len();
        self.text.push_str(piece);
        if slot.is_some() {
            self.spans.push(Span {
                range: from..self.text.len(),
                slot,
            });
        }
    }

    /// Append `piece` with no colour of its own.
    pub(crate) fn plain(&mut self, piece: &str) {
        self.push(piece, None);
    }

    /// What has been appended so far.
    pub(crate) fn as_text(&self) -> &str {
        &self.text
    }

    /// Finish, as a line with no fold on it.
    pub(crate) fn done(self) -> RenderLine {
        RenderLine {
            text: self.text,
            spans: self.spans,
            fold: None,
            links: self.links,
        }
    }
}

/// Two spaces per level, which is what a tree reads as at any depth a terminal
/// can show.
pub(crate) fn indent(depth: usize) -> String {
    " ".repeat(depth.saturating_mul(2).min(120))
}
