//! The viewer's status line.
//!
//! One line saying where in the file the view is, in the unit the current mode
//! reads the file in: a line number and a percentage in text, an offset in
//! hex. It is built from what the viewer already knows and reads nothing, so a
//! status line can be asked for at any moment without touching the file.

use super::*;

impl Viewer {
    /// Everything the status line needs.
    pub fn status(&self) -> Status {
        let len = self.source.len();
        // From the bottom of the window, not the top: "how far through the
        // file am I" is a question about what is on screen, and measuring the
        // top means the last page reads 98% and 100 is never reached at all.
        let seen = self.window_end.max(self.top);
        let percent = len.filter(|l| *l > 0).map(|l| {
            let p = seen
                .min(l)
                .saturating_mul(100)
                .checked_div(l)
                .unwrap_or(0)
                .min(100);
            u8::try_from(p).unwrap_or(100)
        });
        let index_percent = if self.idx.is_complete() {
            None
        } else {
            len.filter(|l| *l > 0).map(|l| {
                let p = self
                    .idx
                    .scanned()
                    .saturating_mul(100)
                    .checked_div(l)
                    .unwrap_or(0)
                    .min(100);
                u8::try_from(p).unwrap_or(100)
            })
        };
        Status {
            title: self.title.clone(),
            mode: self.mode,
            offset: self.cursor,
            len,
            percent,
            line: self.top_line,
            lines: self.idx.known_lines(),
            indexed: self.idx.is_complete(),
            index_percent,
            approximate: self.approximate || (!self.idx.is_complete() && self.top_line.is_none()),
            encoding: self.encoding.label(),
            encoding_how: self.encoding_how,
            decode_errors: self.decode_errors,
            wrap: self.wrap,
            binary: self.binary,
            highlighted: self.highlighting,
            render: self.rendered.as_ref().map(|r| r.label.clone()),
            git: self.git_state.map(crate::git::State::label),
            field: self.field_reading.clone(),
            // An empty selection is not one: it covers no byte, paints nothing
            // and `Ctrl+C` refuses it, so the status line does not announce it
            // either ([`Viewer::clear_selection`]).
            selection: self
                .sel
                .as_ref()
                .filter(|sel| !sel.is_empty())
                .map(SelectionStatus::of),
            // The reading the status line shows is the reading `Ctrl+Shift+C`
            // copies, character for character, because both come from here
            // (the design invariant 15).
            interpretation: self.sel.and_then(|sel| {
                // A **linear** selection only. A block's bytes are not one run:
                // between its two corners lie the bytes either side of the
                // column band, which `Ctrl+C` does not copy and a reading of
                // the span would silently include (the "the selected
                // bytes in file order"). `Ctrl+Shift+C` refuses a block for the
                // same reason, so the two still agree
                // (the design invariant 15).
                if !matches!(sel.kind, SelectKind::Linear) {
                    return None;
                }
                let n = usize::try_from(sel.len()).ok()?;
                let bytes = self.sel_preview.as_ref()?.get(..n)?;
                copy::interpretations(bytes, self.hex_cfg, self.ascii)
            }),
            side: self.side,
            hex_width_rounded: self.rounded_hex_width(),
            error: self.idx.error().map(str::to_string),
        }
    }
}
