//! Text or hex, and which encoding the text is being read in.
//!
//!
//! A viewer shows the same bytes in one of two modes, and the mode decides the
//! unit everything else is measured in: a text position is a line and a
//! grapheme cluster, a hex position is an offset. Switching modes therefore
//! has to carry the position across, not merely repaint, which is why the two
//! toggles are here rather than being a flag anyone can set.
//!
//! The initial mode is chosen rather than configured when the file decides it:
//! `viewer.default_mode` sets it, the session's last choice overrides that,
//! and a binary file overrides both, because showing the bytes of a binary as
//! text is not a view of anything.
//!
//! An encoding is a **reading** of the bytes and never a change to them. The
//! file on disk is untouched; what moves is which decoder the rows are built
//! through.

use super::*;

impl Viewer {
    /// Tell the viewer which glyph set the terminal has.
    ///
    /// The renderer knows `ui.ascii_borders` and the layout is the model's, so
    /// the flag has to cross. It is set once a frame beside the layout, for the
    /// same reason the row count is: a terminal's capabilities are the
    /// renderer's business and the rows are built before the renderer runs.
    pub const fn set_ascii(&mut self, ascii: bool) {
        self.ascii = ascii;
    }

    /// How many bytes of one line are worth materialising for the screen as it
    /// now stands.
    ///
    /// What the screen can show, plus the horizontal scroll, times the widest
    /// encoding of a character. **A function of the terminal - never of the
    /// file** - and the same number for laying a screen out and for stepping
    /// down it, so navigation and layout cannot disagree about where the row
    /// below begins.
    pub(crate) fn per_line(&self) -> u64 {
        let columns = usize::from(self.view_cols);
        let per_line = (self.hscroll.saturating_add(columns).saturating_add(8) as u64)
            .saturating_mul(4)
            .saturating_add(64)
            .min(text::MAX_LINE_BYTES);
        if self.wrap {
            per_line
                .saturating_mul(u64::from(self.view_rows.max(1)))
                .min(text::MAX_LINE_BYTES)
        } else {
            per_line
        }
    }

    /// `1` / `2` / `F4`.
    ///
    /// Switching keeps the position: the byte offset is the position in both
    /// modes, so hex mode opens on the bytes that were on screen and text mode
    /// comes back to the line containing them.
    ///
    /// It also re-runs the search, because the mode is part of what the pattern
    /// *means* - see [`Viewer::refind`].
    pub fn set_mode(&mut self, mode: ViewerMode) -> Result<()> {
        if self.mode == mode {
            return Ok(());
        }
        self.render_note = None;
        // Mode 3 reads the whole file, so it is the one mode that can refuse
        // to be entered. A refusal falls back to text and says why, rather
        // than leaving the key looking broken or showing an empty screen.
        if mode == ViewerMode::Render
            && let Err(refusal) = self.build_render(self.render_max)
        {
            self.render_note = Some(refusal.message(&self.title));
            if self.mode == ViewerMode::Text {
                return Ok(());
            }
            return self.set_mode(ViewerMode::Text);
        }
        // "there is one cursor and one selection, and both are
        // byte ranges in the file", which settles what a mode switch does to
        // them - a byte range does not care how the bytes are drawn.
        //
        let was = self.cursor;
        self.mode = mode;
        self.hscroll = 0;
        match mode {
            ViewerMode::Hex => {
                self.top = self.hex.snap(self.top);
                self.cursor = hex::clamp_cursor(self.top, self.source.len());
            }
            // Mode 3 leaves the byte window exactly where it was. Its own
            // window is a rendered-line index and there is nothing to carry
            // across; leaving the byte side untouched is what makes `1` and
            // `2` come back to the position that was there before.
            ViewerMode::Render => {}
            ViewerMode::Text => {
                // Land on the start of the line the bytes belong to, so text
                // mode never begins mid-sequence (see `decode`'s module docs).
                let at = self.top;
                let start = self.line_start_at_or_before(at);
                self.top = start.as_ref().map_or(at, |s| s.at);
                self.top_at_line_start = start.is_ok_and(|s| s.line_start);
                self.top_row = 0;
                self.cursor = hex::clamp_cursor(self.top, self.source.len());
                self.top_line = None;
            }
        }
        self.hl_states.clear();
        self.hl_marks.clear();
        // the "switching keeps the position" is about the window,
        // and the two modes do not put a window in the same place: hex snaps to
        // its byte grid, text backs up to a line start. The cursor keeps its
        // own byte offset when the new window still shows it, and otherwise
        // goes to the top of that window rather than dragging it back.
        self.cursor_row = 0;
        if self.cursor_enabled && self.window_shows(was)? {
            self.cursor = hex::clamp_cursor(was, self.source.len());
            self.cursor_row = match self.mode {
                ViewerMode::Hex => self.hex_cursor_row(),
                ViewerMode::Render => 0,
                ViewerMode::Text => self.window_row_of(self.cursor)?.unwrap_or(0),
            };
        }
        // `FindKind::Auto` reads `dead` as four bytes in hex mode and as text
        // in text mode, so the mode switch is a change to what the pattern
        // means (the "the same bar accepts either").
        self.refind()
    }

    /// `F4` toggles text and hex, and only those two.
    ///
    /// Not a three-way cycle, and the reason is what the key is for. `F4` is
    /// "show me the bytes" and then "put it back", pressed twice in a row
    /// dozens of times while reading one file; a third stop turns that round
    /// trip into two presses and lands on a mode most files do not even have a
    /// renderer for. Mode 3 is reached by `3`, which is how the other two
    /// modes are reached as well, and `F4` from it goes back to text - the
    /// mode it falls back to and the one it is a reading of.
    pub fn toggle_mode(&mut self) -> Result<()> {
        self.set_mode(match self.mode {
            ViewerMode::Text => ViewerMode::Hex,
            ViewerMode::Hex | ViewerMode::Render => ViewerMode::Text,
        })
    }

    /// `F2`.
    /// `F2`: re-read the file from disk.
    ///
    /// A viewer is often pointed at something still being written - a log, a
    /// build's output, a file an editor is saving in another window - and
    /// re-opening it to see the new bytes is a poor answer.
    ///
    /// The cursor keeps its byte offset when the file is still that long. One
    /// that has shrunk past it moves the cursor to the new end and says so,
    /// rather than leaving it pointing into nothing.
    pub fn reload(&mut self) -> Result<String> {
        let was = self.cursor;
        let opener = self.source.opener();
        let len = self.source.len();
        let mut source = Source::open(opener, None)?;
        let now = source.len();
        std::mem::swap(&mut self.source, &mut source);

        // The old index describes bytes that may have moved. Cancel the scan
        // behind it and start from nothing rather than trusting checkpoints
        // taken against a file that has since changed.
        self.cancel.store(true, Ordering::Relaxed);
        self.idx = LineIndex::new(self.chunk);

        let shrank = now.is_some_and(|n| was >= n);
        if shrank {
            let end = now.unwrap_or(0);
            self.top = end;
            self.cursor = hex::clamp_cursor(end, now);
            self.cursor_row = 0;
            return Ok(format!(
                "reloaded; the file shrank to {end} bytes, so the cursor moved to the end"
            ));
        }
        Ok(match (len, now) {
            (Some(before), Some(after)) if after > before => {
                format!("reloaded; {} bytes more", after.saturating_sub(before))
            }
            _ => "reloaded".to_string(),
        })
    }

    /// `g`, `d`, `e`: cycle the hex grouping, display format and byte order.
    ///
    ///
    /// Trying one and then another against an unfamiliar file is the whole
    /// activity, which is why these are keys and not only settings. The cursor
    /// keeps its byte offset - regrouping changes how the row is written, never
    /// which bytes it covers.
    pub fn cycle_hex_group(&mut self) -> String {
        self.hex_cfg.group = self.hex_cfg.group.next();
        // the rounding is a function of the grouping, so it is
        // re-applied here rather than once at the door: 12 bytes a row is
        // twelve at `group = 8` and eight at `group = 64`. The configured
        // width is what it is re-applied to, so the row grows back when the
        // key comes round again.
        self.hex = HexLayout::grouped(self.hex_width_cfg, self.hex_cfg);
        if self.mode == ViewerMode::Hex {
            // The rows have a new stride, and the grid is the
            // file's own: the top of the screen belongs on a row start of the
            // width now in force.
            self.top = self.hex.snap(self.top);
        }
        let bits = self.hex_cfg.group.bits();
        match self.rounded_hex_width() {
            // "and says so".
            Some((asked, used)) => {
                format!("hex: {bits}-bit columns - hex_width {asked} rounded down to {used}")
            }
            None => format!("hex: {bits}-bit columns"),
        }
    }

    /// The configured `viewer.hex_width` and the one in force, when the
    /// rounding had anything to do (`None` when it did not).
    ///
    /// The status line and the `g` key both report it, which is the whole of
    /// "and says so" - a row narrower than the configuration asked for is not
    /// something to discover by counting columns.
    pub const fn rounded_hex_width(&self) -> Option<(u16, u16)> {
        let used = self.hex.width();
        let asked = self.hex_width_cfg;
        // Only a rounding *down* has anything to say: `HexLayout::new` also
        // pulls a zero or an absurd width into range, and that is a clamp of
        // the configuration rather than the grouping arithmetic.
        if used < asked {
            Some((asked, used))
        } else {
            None
        }
    }

    /// `d`: hex → unsigned → signed → hex.
    pub fn cycle_hex_format(&mut self) -> String {
        if self.hex_cfg.format.is_decimal() {
            self.hex_sign = self.hex_cfg.format;
        }
        self.hex_cfg.format = self.hex_cfg.format.toggle_base(self.hex_sign);
        format!("hex: {}", self.hex_cfg.format.label())
    }

    /// `s`: unsigned ↔ signed.
    ///
    /// Refused in hex rather than silently switching base, because the user
    /// asked about sign and a change of base answers a different question.
    pub fn toggle_hex_sign(&mut self) -> String {
        match self.hex_cfg.format.toggle_sign() {
            Some(next) => {
                self.hex_cfg.format = next;
                self.hex_sign = next;
                format!("hex: {}", next.label())
            }
            None => "hex digits have no sign - press d for decimal first".to_string(),
        }
    }

    /// `e`: little ↔ big.
    ///
    /// Only meaningful above 8 bits, and it says so rather than silently doing
    /// nothing - a decimal column with an unstated byte order is a number you
    /// cannot trust.
    pub fn flip_hex_endian(&mut self) -> String {
        self.hex_cfg.endian = self.hex_cfg.endian.flip();
        if self.hex_cfg.group.bytes() == 1 {
            format!(
                "hex: {} - byte order applies above 8-bit columns (g)",
                self.hex_cfg.endian.label()
            )
        } else {
            format!("hex: {}", self.hex_cfg.endian.label())
        }
    }

    /// The grouping in force, for the renderer and the status line.
    pub const fn hex_config(&self) -> crate::config::HexConfig {
        self.hex_cfg
    }

    /// `i`: open or close the reading panel.
    ///
    /// Hex only where it is drawn, but the flag is kept whatever the mode is:
    /// looking at something as text and coming back should not cost the key
    /// press again.
    pub const fn toggle_inspect(&mut self) {
        self.inspect = !self.inspect;
    }

    /// Whether the reading panel is open.
    pub const fn inspecting(&self) -> bool {
        self.inspect
    }

    /// `w`: wrap long lines, or stop.
    ///
    /// Horizontal scroll is reset: a wrapped view has no off-screen right-hand
    /// side to be scrolled to, and carrying the old offset into it would put
    /// the cursor somewhere the user did not leave it.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.hscroll = 0;
    }

    /// `Ctrl+Left` / `Ctrl+Right`: move the **view** sideways
    /// and leave the cursor and the selection where they are.
    ///
    /// Not [`Viewer::scroll_horizontal`], which routes through the cursor when
    /// there is one and is therefore the opposite of what this means.
    ///
    /// A no-op in hex, whose rows are a fixed width that always fits, and with
    /// wrapping on, where there is nothing to the side to scroll to. Both are
    /// silent rather than refused: a key that does nothing because there is
    /// nothing to do is not an error worth a message.
    pub fn scroll_view_horizontal(&mut self, delta: isize) {
        if matches!(self.mode, ViewerMode::Hex) || self.wrap {
            return;
        }
        self.hscroll = self.hscroll.saturating_add_signed(delta);
    }

    /// What `Left` / `Right` do: scroll sideways in text mode, move the cursor
    /// a byte at a time in hex (the "the current offset under the
    /// cursor is in the status line").
    ///
    /// Hex has no sideways to scroll - the row is as wide as it is - so the
    /// same key is what makes the cursor a *byte* rather than a whole row,
    /// which is what the status line's offset needs to be useful.
    pub fn scroll_horizontal(&mut self, delta: isize) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        if self.cursor_enabled {
            // With a cursor, one press is one character in text mode and one
            // unit of the focused side in hex. `delta` is a
            // scroll distance and a cursor does not scroll, so the sign is all
            // of it that is read.
            let motion = if delta < 0 {
                Motion::Left
            } else {
                Motion::Right
            };
            return self.move_cursor(motion, Extend::None);
        }
        if matches!(self.mode, ViewerMode::Hex) {
            let by = i64::try_from(delta).unwrap_or(if delta < 0 { i64::MIN } else { i64::MAX });
            self.cursor = hex::step_cursor(self.cursor, by, self.source.len());
            self.follow_hex_cursor();
            return Ok(());
        }
        if self.wrap {
            return Ok(());
        }
        self.hscroll = self.hscroll.saturating_add_signed(delta);
        Ok(())
    }

    /// Bring the top to where the hex cursor is, if the cursor left the screen.
    pub(crate) fn follow_hex_cursor(&mut self) {
        let stride = self.hex.stride();
        let span = stride.saturating_mul(u64::from(self.view_rows.max(1)));
        if self.cursor < self.top {
            self.top = self.hex.snap(self.cursor);
        } else if self.cursor >= self.top.saturating_add(span) {
            let row = self.hex.snap(self.cursor);
            self.top = self.clamp_hex_top(row.saturating_sub(span.saturating_sub(stride)));
        }
    }

    /// `F8`.
    ///
    /// > changing encoding re-decodes the visible window only, not the file.
    ///
    /// Which is all this does: it moves a field. The next [`Viewer::layout`]
    /// decodes the same window again with the new encoding, and the file is
    /// never touched.
    pub fn cycle_encoding(&mut self) -> Result<()> {
        let at = self
            .shortlist
            .iter()
            .position(|e| *e == self.encoding)
            .map_or(0, |i| i.saturating_add(1));
        let next = self
            .shortlist
            .get(at % self.shortlist.len().max(1))
            .copied()
            .unwrap_or(TextEncoding::UTF8);
        self.set_encoding(next)
    }

    /// Choose an encoding explicitly.
    pub fn set_encoding(&mut self, encoding: TextEncoding) -> Result<()> {
        if self.encoding == encoding {
            return Ok(());
        }
        let was_unit = self.encoding.line_term().unit();
        self.encoding = encoding;
        self.encoding_how = Detected::Chosen;
        self.hl_states.clear();
        // Every kept parse state was produced by decoding with the encoding
        // that has just been replaced, so none of them is about this text any
        // more (the "re-decodes the visible window").
        self.hl_marks.clear();
        // A line terminator is a fact about an encoding, so what was proven
        // about this file under the last one is not proven under this one.
        self.no_break = None;
        // Changing between a byte-grid and a unit-grid encoding moves where
        // line starts are, so the current top may no longer be one.
        if encoding.line_term().unit() != was_unit {
            let at = self.top;
            let start = self.line_start_at_or_before(at);
            self.top = start.as_ref().map_or(at, |s| s.at);
            self.top_at_line_start = start.is_ok_and(|s| s.line_start);
            self.top_row = 0;
            self.top_line = None;
        }
        // The pattern was encoded into the old encoding's bytes; recompile it
        // into the new one, or `café` in a cp437 file stops matching.
        self.refind()
    }

    /// Re-read the query in whatever the pattern now *means*, and throw away
    /// what the previous reading counted.
    ///
    /// Two keys change the meaning of a pattern without changing a character of
    /// it: `F4` / `1` / `2` (in hex mode `dead` is the bytes `DE AD`) and `F8`
    /// (in cp437 `café` is different bytes). Both have to do everything a
    /// keystroke into the bar does - bump the generation, drop the hits, stop
    /// the counter that is running for the old reading and start one for the
    /// new - or the bar shows the old pattern's tally beside a screen with none
    /// of its matches on it, and never corrects itself.
    fn refind(&mut self) -> Result<()> {
        if self.find.input().is_empty() {
            return Ok(());
        }
        self.cancel_find_scan();
        let hex_mode = matches!(self.mode, ViewerMode::Hex);
        let enc = self.encoding;
        self.find.recompile(enc, hex_mode);
        self.find_resume = (None, None);
        // Only while the bar is open does re-reading the pattern move the view:
        // the "typing searches immediately" is about typing, and a
        // mode switch with the bar closed is a statement about the *screen*,
        // which the design says keeps its position.
        if self.find.is_open() {
            let from = self.find_origin;
            let found = self.find.search(
                &mut self.source,
                enc,
                hex_mode,
                from,
                find::FIND_READ_BUDGET,
            )?;
            self.settle(found, true)?;
        }
        self.queue_find_scan();
        Ok(())
    }

    /// The `F8` ring, in order.
    pub fn encoding_shortlist(&self) -> &[TextEncoding] {
        &self.shortlist
    }
}
