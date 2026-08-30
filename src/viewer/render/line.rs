//! Building one rendered line, with its colour runs.
//!
//! Every renderer appends through here so that a span's byte range and the
//! text it describes cannot drift apart. Nothing else in this module tree
//! touches a `RenderLine`'s string directly.

use super::super::highlight::{Span, SynSlot};
use super::RenderLine;

/// A line being built, with its colour runs.
///
/// Shared by the three renderers so that a span's byte range and the text it
/// describes cannot drift apart: nothing appends to the string without going
/// through here.
#[derive(Debug, Default)]
pub(crate) struct LineBuf {
    text: String,
    spans: Vec<Span>,
}

impl LineBuf {
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
        }
    }
}

/// Two spaces per level, which is what a tree reads as at any depth a terminal
/// can show.
pub(crate) fn indent(depth: usize) -> String {
    " ".repeat(depth.saturating_mul(2).min(120))
}
