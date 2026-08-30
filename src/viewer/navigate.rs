//! Moving through the file, and finding the line a byte is on.
//!
//!
//! Every move lands on a real position in the file, and the file is not
//! necessarily indexed yet: a viewer opens on the first screenful and the line
//! index is filled in behind it. So `G` and "go to line" are answered from the
//! index when it has reached that far and by reading forward when it has not,
//! and the answer is the same either way.
//!
//! The index is what makes a line number meaningful at all. Until a byte has
//! been indexed, this program knows where it is and not which line it is on,
//! which is why a status line can say a percentage before it can say a line
//! number.

use super::*;

impl Viewer {
    /// Fold in one background batch.
    ///
    /// A batch for another viewer is ignored, which is what makes reopening a
    /// file while the old scan is winding down harmless.
    pub fn apply_index(&mut self, batch: &IndexBatch) -> bool {
        if batch.id != self.id {
            return false;
        }
        self.idx.apply(batch);
        if batch.done
            && let Some(len) = self.source.len().or(Some(batch.scanned))
        {
            self.source.set_len(len);
        }
        // A file whose size nobody could report gets one from the scan.
        if batch.done && self.source.len().is_none() {
            self.source.set_len(batch.scanned);
        }
        true
    }

    /// Move the top by `delta` lines (text) or rows (hex).
    ///
    /// Bounded work per call: each step is one line break away from where it
    /// started, and the read budget stops a file with no line breaks turning
    /// one keystroke into a scan.
    pub fn scroll(&mut self, delta: isize) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        // With a cursor this moves the window and not the cursor, so the
        // cursor's row index no longer describes anything. See
        // `cursor_row_stale`.
        if self.cursor_enabled {
            self.cursor_row_stale = true;
        }
        if self.mode == ViewerMode::Render {
            // Mode 3's window is a rendered-line index, and scrolling it moves
            // the window without touching the cursor exactly as it does here.
            self.scroll_render(delta);
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => {
                let stride = self.hex.stride();
                let by = (delta.unsigned_abs() as u64).saturating_mul(stride);
                self.top = if delta < 0 {
                    self.top.saturating_sub(by)
                } else {
                    self.clamp_hex_top(self.top.saturating_add(by))
                };
                // With a cursor, scrolling moves the **window**; the cursor is
                // where the user left it and `follow_cursor` is what keeps the
                // two together. Without one,
                // this assignment *is* what pure scrolling means and it stays.
                if !self.cursor_enabled {
                    self.cursor = hex::clamp_cursor(self.top, self.source.len());
                }
            }
            ViewerMode::Text | ViewerMode::Render => {
                let per_line = self.per_line();
                // One budget for the whole call rather than one per line:
                // `PgDn` is a screenful of these, and a page may not cost a
                // screenful of keystrokes (the design I9).
                let mut budget = NAV_READ_BYTES;
                if self.wrap {
                    self.scroll_wrapped(delta, per_line, &mut budget)?;
                } else if delta > 0 {
                    for _ in 0..delta {
                        let Some(step) = self.step_down(self.top, per_line, &mut budget)? else {
                            break;
                        };
                        self.top = step.at;
                        self.top_at_line_start = step.line_start;
                        if step.line_start {
                            self.top_line = self.top_line.map(|l| l.saturating_add(1));
                        }
                    }
                } else {
                    for _ in 0..delta.unsigned_abs() {
                        if self.top <= self.bom_len {
                            self.top = self.bom_len;
                            self.top_line = Some(0);
                            self.top_at_line_start = true;
                            break;
                        }
                        if self.top_at_line_start {
                            let step = self.prev_line_start(self.top)?;
                            self.top = step.at;
                            self.top_at_line_start = step.line_start;
                            self.top_line = self.top_line.map(|l| l.saturating_sub(1));
                        } else {
                            // Inside a line too long to have been walked to its
                            // start. Step back a row's worth of it - the grid
                            // the rows below were laid out on - rather than to
                            // the line's start, which may be megabytes above
                            // and would make `Up` undo far more than `Down`
                            // did.
                            let back = self.top.saturating_sub(per_line).max(self.bom_len);
                            self.top = self.resync_offset(back)?;
                            self.top_at_line_start = self.top <= self.bom_len;
                        }
                    }
                }
                if !self.cursor_enabled {
                    self.cursor = hex::clamp_cursor(self.top, self.source.len());
                }
            }
        }
        Ok(())
    }

    /// `PgUp` / `PgDn`. One screenful less a row of overlap, as every pager
    /// does, so the eye keeps its place.
    pub fn page(&mut self, down: bool) -> Result<()> {
        let rows = isize::try_from(self.view_rows.max(1))
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1);
        self.scroll(if down { rows } else { -rows })
    }

    /// `Home`.
    pub fn goto_start(&mut self) -> Result<()> {
        self.top = if self.mode == ViewerMode::Hex {
            0
        } else {
            self.bom_len
        };
        self.cursor = hex::clamp_cursor(self.top, self.source.len());
        self.cursor_row = 0;
        self.top_line = Some(0);
        self.top_at_line_start = true;
        self.top_row = 0;
        self.hscroll = 0;
        self.approximate = false;
        self.hl_states.clear();
        Ok(())
    }

    /// `End`.
    ///
    /// > While the index is still building, `End` and percentage seeks are
    /// > marked approximate in the status line rather than blocked.
    ///
    /// So this never refuses. With a known size it goes to the last screenful
    /// of it; with the index still running the landing is exact in hex (which
    /// is arithmetic) and marked approximate in text (where the line number is
    /// the part that is not yet known).
    pub fn goto_end(&mut self) -> Result<()> {
        let Some(len) = self.source.len() else {
            // No size and no completed index: the honest answer is the furthest
            // point that has been proven to exist.
            self.goto_offset(self.idx.scanned())?;
            self.approximate = true;
            return Ok(());
        };
        if self.mode == ViewerMode::Render {
            self.move_render(crate::viewer::select::Motion::FileEnd);
            self.approximate = false;
            return Ok(());
        }
        let with_cursor = self.cursor_enabled;
        match self.mode {
            ViewerMode::Hex => {
                let rows = u64::from(self.view_rows.max(1));
                let last = self.hex.rows(len).saturating_sub(1);
                self.top = self
                    .hex
                    .row_offset(last.saturating_sub(rows.saturating_sub(1)));
                if !with_cursor {
                    self.cursor = self.hex.row_offset(last);
                }
                self.approximate = false;
            }
            ViewerMode::Text | ViewerMode::Render => {
                self.goto_offset(len)?;
                // Back up a screenful so the last line is at the bottom rather
                // than at the top with nothing under it.
                let rows = isize::try_from(self.view_rows.max(1))
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .max(0);
                self.scroll(-rows)?;
                self.approximate = !self.idx.is_complete();
            }
        }
        self.hl_states.clear();
        // `Ctrl+End` is the **last byte**, by seek: with a
        // cursor the window landing on the last page is only half of it.
        if with_cursor {
            self.cursor_to_last_row(len)?;
        }
        Ok(())
    }

    /// Put the cursor on the last row of the window, at its last character.
    ///
    /// `Ctrl+End`'s other half. In hex the answer is the last byte, which is
    /// arithmetic; in text it is the last character of the last row, because
    /// the last *byte* of a text file is usually its line terminator and the
    /// cursor has to sit on something the screen draws.
    fn cursor_to_last_row(&mut self, len: u64) -> Result<()> {
        if self.mode == ViewerMode::Render {
            self.move_render(crate::viewer::select::Motion::FileEnd);
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => {
                self.cursor = hex::clamp_cursor(len.saturating_sub(1), Some(len));
                self.follow_hex_cursor();
                self.cursor_row = self.hex_cursor_row();
            }
            ViewerMode::Text | ViewerMode::Render => {
                let last = usize::from(self.view_rows.max(1)).saturating_sub(1);
                // `usize::MAX` is "past every column there is", which
                // `text::column_range` answers with the whole row - so the
                // cursor lands on the row's last character.
                self.place_on_window_row(last, usize::MAX)?;
            }
        }
        Ok(())
    }

    /// `Home`: the start of the line, or of the hex row.
    ///
    /// Not the start of the *file* - that is `Ctrl+Home`. The two were the same
    /// key before the viewer had a cursor to put anywhere but the top left.
    pub fn line_start(&mut self) -> Result<()> {
        if self.cursor_enabled {
            // With a cursor, `Home` is the first character of the cursor's
            // **screen row**: the line with wrap off, the wrapped row with wrap
            // on.
            return self.move_cursor(Motion::RowStart, Extend::None);
        }
        match self.mode {
            ViewerMode::Hex => {
                self.cursor = self.hex.snap(self.cursor);
                self.follow_hex_cursor();
            }
            // With wrapping on there is no horizontal axis to be at the start
            // of: every line already begins at column zero. Mode 3 has none at
            // all, which is the same answer for a different reason.
            ViewerMode::Text | ViewerMode::Render => self.hscroll = 0,
        }
        Ok(())
    }

    /// `End`: the end of the line, or of the hex row.
    ///
    /// In text this scrolls horizontally far enough to put the end of the
    /// longest line on screen in view, which is what "end of the line" can mean
    /// while the cursor is still the top of the window. In hex it is the last
    /// byte of the cursor's row, clamped to the end of a short final row.
    pub fn line_end(&mut self) -> Result<()> {
        if self.cursor_enabled {
            return self.move_cursor(Motion::RowEnd, Extend::None);
        }
        if self.mode == ViewerMode::Render {
            // A rendered line has no end to go to: the row is the unit.
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => {
                let last = self
                    .hex
                    .snap(self.cursor)
                    .saturating_add(self.hex.stride().saturating_sub(1));
                self.cursor = match self.source.len() {
                    Some(len) => last.min(len.saturating_sub(1)),
                    None => last,
                };
                self.follow_hex_cursor();
            }
            ViewerMode::Text | ViewerMode::Render => {
                if self.wrap {
                    return Ok(());
                }
                let widest = self
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Text { text, .. } => Some(UnicodeWidthStr::width(text.as_str())),
                        Row::Hex { .. } => None,
                    })
                    .max()
                    .unwrap_or(0);
                // Leave the last column occupied rather than scrolled past: the
                // end of the line is what was asked for, not the blank after it.
                self.hscroll = widest.saturating_sub(usize::from(self.view_cols).max(1));
            }
        }
        Ok(())
    }

    /// `Ctrl+G`.
    ///
    /// Snaps to a row start in hex and to a line start in text, so the offset
    /// asked for is always visible even when it is not the first byte on
    /// screen.
    pub fn goto_offset(&mut self, offset: u64) -> Result<()> {
        let clamped = match self.source.len() {
            Some(len) => offset.min(len),
            None => offset,
        };
        // The window may be asked to show the very end of the file, but the
        // cursor is a **byte** and there is no byte at `len`
        // (the design invariant 1).
        self.cursor = hex::clamp_cursor(clamped, self.source.len());
        if self.mode == ViewerMode::Render {
            // A byte offset does not name a rendered line, so `Ctrl+G` keeps
            // the byte cursor - which is what `1` and `2` will come back to -
            // and leaves the rendered window where it is rather than jumping
            // it somewhere unrelated to the number that was typed.
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => {
                self.top = self.clamp_hex_top(self.hex.snap(clamped));
                self.approximate = false;
            }
            ViewerMode::Text | ViewerMode::Render => {
                let start = self.line_start_at_or_before(clamped)?;
                // A line longer than a row is laid out on a grid from its
                // start, so an offset deep inside one lands on the row that
                // *shows* it rather than on the top of a line whose first row
                // may be megabytes above. "`Ctrl+G` jumps to an
                // offset" - and an offset it does not put on the screen has not
                // been jumped to.
                let per_line = self.per_line().max(1);
                let rows_down = clamped
                    .saturating_sub(start.at)
                    .checked_div(per_line)
                    .unwrap_or(0);
                self.top = start.at.saturating_add(rows_down.saturating_mul(per_line));
                self.top_at_line_start = start.line_start && rows_down == 0;
                self.top_row = 0;
                self.top_line = self.line_number_of(self.top);
                // Approximate if the number is unknown **or** if the line start
                // it was counted from was a bounded guess rather than a proof
                // (the honesty rule).
                self.approximate = self.top_line.is_none() || !start.line_start;
            }
        }
        self.hscroll = 0;
        self.hl_states.clear();
        // The window has just moved to the cursor, so which row it is on is a
        // walk of the file rather than a search of `rows`.
        //
        if self.cursor_enabled {
            self.cursor_row = match self.mode {
                ViewerMode::Hex => self.hex_cursor_row(),
                ViewerMode::Text | ViewerMode::Render => self.window_row_of(clamped)?.unwrap_or(0),
            };
        }
        Ok(())
    }

    /// Go to a percentage of the file (the "percentage seeks").
    pub fn goto_percent(&mut self, percent: u8) -> Result<()> {
        let len = self.source.len().unwrap_or(self.idx.scanned());
        let at = len
            .saturating_mul(u64::from(percent.min(100)))
            .checked_div(100)
            .unwrap_or(0);
        self.goto_offset(at)?;
        if self.source.len().is_none() {
            self.approximate = true;
        }
        Ok(())
    }

    /// Go to a 0-based line number (`Ctrl+G` in text mode).
    ///
    /// Answered from the nearest checkpoint plus a bounded forward scan. While
    /// the index is still building, a line beyond what has been scanned lands
    /// on the furthest known line and is marked approximate rather than
    /// refused.
    pub fn goto_line(&mut self, line: u64) -> Result<()> {
        let cp = self.idx.checkpoint_for_line(line);
        let term = self.line_term();
        let mut at = cp.offset;
        let mut n = cp.line;
        // The furthest line start actually proven, which is where a line that
        // is not there lands - the doc comment's "the furthest known line".
        // Walking to the end of the read and stopping there would land inside
        // a line, or past the last byte of a file with no breaks in it at all.
        let mut start = (cp.offset, cp.line);
        // **Bounded by the checkpoint spacing, not by the file.** The
        // checkpoint below `line` is at most one spacing away from it, so
        // reading that far is the whole cost - which is what a sparse index
        // buys, and why the spacing is what decimation trades against memory.
        //
        let limit = cp
            .offset
            .saturating_add(self.idx.spacing())
            .saturating_add(WindowLen::MAX.get() as u64);
        'scan: while n < line && at < limit {
            let w = self.source.read_window(at, WindowLen::MAX)?;
            if w.is_empty() {
                break;
            }
            let mut i = 0_usize;
            while let Some(brk) = term.find(w.bytes(), i) {
                n = n.saturating_add(1);
                i = brk.saturating_add(term.unit());
                start = (w.at().saturating_add(i as u64), n);
                if n == line {
                    at = start.0;
                    break 'scan;
                }
            }
            if w.hit_eof() {
                at = w.end();
                break;
            }
            // No break in the whole window means the line runs past it; step
            // over the window rather than sitting on it.
            at = if i > 0 {
                w.at().saturating_add(i as u64)
            } else {
                w.end()
            };
        }
        let (at, n) = if n == line { (at, n) } else { start };
        self.top = at;
        self.cursor = hex::clamp_cursor(at, self.source.len());
        // Both landings are line starts; only one of them is the line asked
        // for, and the status line says which.
        self.top_at_line_start = true;
        self.top_row = 0;
        self.top_line = Some(n);
        self.approximate = n != line;
        self.hscroll = 0;
        self.cursor_row = 0;
        self.hl_states.clear();
        Ok(())
    }

    pub(crate) fn clamp_hex_top(&self, at: u64) -> u64 {
        let Some(len) = self.source.len() else {
            return self.hex.snap(at);
        };
        let rows = u64::from(self.view_rows.max(1));
        let last_top = self.hex.row_offset(self.hex.rows(len).saturating_sub(rows));
        self.hex.snap(at.min(last_top))
    }

    /// Look for the next line terminator at or after `from`, spending at most
    /// `budget` **bytes of reading** on it (
    /// the design I9).
    ///
    /// The window starts at [`SCAN_STEP`] and grows. A line break is nearly
    /// always a few hundred bytes away, and reading a quarter of a megabyte to
    /// find something four hundred bytes ahead - once per screen row, once per
    /// navigation step - is how a viewer comes to read far more than it shows.
    ///
    /// What is *not* found is worth keeping as much as what is: a run with no
    /// terminator in it is remembered ([`NoBreak`]), so a screen sitting inside
    /// a line longer than the budget does not re-prove that fact on every
    /// frame.
    fn scan_forward(&mut self, from: u64, budget: &mut u64) -> Result<ScanEnd> {
        let term = self.line_term();
        let unit = term.unit() as u64;
        // A run already proven to hold no terminator is skipped for nothing
        // rather than read again.
        let mut at = match self.no_break {
            Some(run) if run.covers(from) => {
                if run.eof {
                    return Ok(ScanEnd::Eof(run.to));
                }
                run.to
            }
            _ => from,
        };
        let mut want = SCAN_STEP;
        while *budget > 0 {
            let cap = usize::try_from(*budget).unwrap_or(usize::MAX);
            let len = want.min(cap);
            // A UTF-16 break lives on the code-unit grid, so a window ending
            // half way through one would step over it.
            let len = len.saturating_sub(len % (unit as usize).max(1));
            if len == 0 {
                break;
            }
            let w = self.source.read_window(at, WindowLen::new(len))?;
            *budget = budget.saturating_sub(w.len() as u64);
            if w.is_empty() {
                self.remember_no_break(from, at, true);
                return Ok(ScanEnd::Eof(at));
            }
            if let Some(idx) = term.find(w.bytes(), 0) {
                let brk = w.at().saturating_add(idx as u64);
                return Ok(ScanEnd::LineStart(brk.saturating_add(unit)));
            }
            if w.hit_eof() {
                self.remember_no_break(from, w.end(), true);
                return Ok(ScanEnd::Eof(w.end()));
            }
            at = w.end();
            want = want.saturating_mul(SCAN_GROWTH).min(source::MAX_WINDOW);
        }
        self.remember_no_break(from, at, false);
        Ok(ScanEnd::Budget(at))
    }

    /// Record that `[from, to)` holds no line terminator. See [`NoBreak`].
    pub(crate) fn remember_no_break(&mut self, from: u64, to: u64, eof: bool) {
        if to <= from {
            return;
        }
        let run = match self.no_break {
            // Overlapping or touching what is already proven: one run, not two.
            Some(old) if from <= old.to && to >= old.from => NoBreak {
                from: old.from.min(from),
                to: old.to.max(to),
                eof: if old.to >= to { old.eof } else { eof },
            },
            _ => NoBreak { from, to, eof },
        };
        self.no_break = Some(run);
    }

    /// Where the row below the one starting at `from` begins, or `None` when
    /// that row is the last.
    ///
    /// **The same rule [`Viewer::read_line`] lays a screen out with**, which is
    /// the point of it existing: `Down` used to ask "where is the next line
    /// start" while the screen below the top row was drawn by asking "where
    /// does this row end", and on a file whose last line has no newline the two
    /// disagreed - the screen showed the file continuing and `Down`, `PgDn` and
    /// `End` all did nothing.
    fn step_down(&mut self, from: u64, per_line: u64, budget: &mut u64) -> Result<Option<Step>> {
        let term = self.line_term();
        let want = usize::try_from(per_line).unwrap_or(WindowLen::MAX.get());
        let w = self.source.read_window(from, WindowLen::new(want))?;
        *budget = budget.saturating_sub(w.len() as u64);
        if w.is_empty() {
            return Ok(None);
        }
        if let Some(idx) = term.find(w.bytes(), 0) {
            let take = idx.saturating_add(term.unit()).min(w.len());
            return Ok(Some(Step {
                at: w.at().saturating_add(take as u64),
                line_start: true,
            }));
        }
        if w.hit_eof() {
            // The last line has no terminator and it ends inside this window,
            // so there is no row below it.
            self.remember_no_break(from, w.end(), true);
            return Ok(None);
        }
        let (step, _) = self.continue_after(&w, budget)?;
        Ok(Some(step))
    }

    /// Where the row below a window that holds no line terminator begins, and
    /// how many of that window's bytes decode whole.
    ///
    /// The window is a row's worth of a line that runs past it. Two answers are
    /// possible and both are honest:
    ///
    /// * the next line starts near enough to be found inside `budget`, and the
    ///   row below is the next line;
    /// * it does not, and the row below is **this same line continuing** from
    ///   the last character boundary in the window.
    ///
    /// The second is what the budget buys. Chasing a line start costs a read of
    /// unknown length, and doing it once per screen row is how one frame comes
    /// to read eighty megabytes of a file with no newlines in it. Continuing
    /// the line reads nothing extra, shows the bytes that are actually there,
    /// and leaves line-start discovery to the background index.
    ///
    /// The boundary is a *character* boundary, not the raw end of the window:
    /// the window's last bytes may be half a character, and starting the row
    /// below at the raw end would drop that character and open the row with a
    /// replacement glyph made out of good bytes.
    pub(crate) fn continue_after(
        &mut self,
        w: &source::Window,
        budget: &mut u64,
    ) -> Result<(Step, usize)> {
        let tail = encoding::incomplete_tail(self.encoding, w.bytes());
        let used = w.len().saturating_sub(tail);
        let (used, boundary) = if used == 0 {
            (w.len(), w.end())
        } else {
            (used, w.at().saturating_add(used as u64))
        };
        let carry_on = Step {
            at: boundary,
            line_start: false,
        };
        // Already proven to be the middle of a line longer than the budget:
        // proving it again is the read this whole mechanism exists to stop.
        if self.no_break.is_some_and(|run| run.covers(w.end())) {
            return Ok((carry_on, used));
        }
        let step = match self.scan_forward(w.end(), budget)? {
            ScanEnd::LineStart(at) => Step {
                at,
                line_start: true,
            },
            ScanEnd::Eof(_) | ScanEnd::Budget(_) => carry_on,
        };
        Ok((step, used))
    }

    /// Nudge an offset that is **not** a line start onto a character boundary.
    ///
    ///
    /// Only reached when a line-start search ran out of read budget - a line
    /// longer than [`NAV_READ_BUDGET`] windows, which is the residue case
    /// [`decode`]'s module docs describe. Decoding from an arbitrary byte would
    /// otherwise begin mid-sequence and render the first character as a
    /// replacement glyph for no reason.
    ///
    /// Sets [`Status::approximate`] when the encoding has no local resync rule
    /// and the answer really is a guess - the honesty rule, applied
    /// to decoding rather than to seeking.
    pub(crate) fn resync_offset(&mut self, at: u64) -> Result<u64> {
        let lead = Resync::LEAD_IN.min(at.saturating_sub(self.bom_len));
        let from = at.saturating_sub(lead);
        let want = usize::try_from(lead.saturating_add(8)).unwrap_or(0);
        if want == 0 {
            return Ok(at);
        }
        let w = self.source.read_window(from, WindowLen::new(want))?;
        let (start, guessed) = decode::resync(self.encoding, w.at(), w.bytes(), at);
        if guessed {
            self.approximate = true;
        }
        Ok(start.max(self.bom_len))
    }

    /// How many screen rows the line at `at` occupies, with wrapping on.
    ///
    /// Laid out exactly as [`Viewer::layout_text`] lays it out - same read, same
    /// tab expansion, same wrap - because a disagreement here is a row that
    /// scrolling counts and the renderer does not, or the reverse.
    fn rows_in_line(&mut self, at: u64, per_line: u64, budget: &mut u64) -> Result<usize> {
        let term = self.line_term();
        let mut scan = *budget;
        let slice = self.read_line(at, per_line, &mut scan)?;
        *budget = budget.saturating_sub(slice.bytes.len() as u64);
        if slice.bytes.is_empty() {
            return Ok(1);
        }
        let body = term.trim_break(&slice.bytes);
        let decoded = encoding::decode_window(self.encoding, body, !slice.cut).text;
        let (expanded, _, _) = expand_row(&decoded, self.tab_width, self.ascii, &[], &[]);
        Ok(text::row_count(&expanded, usize::from(self.view_cols)).max(1))
    }

    /// [`Viewer::scroll`] in text mode with wrapping **on**, where a row is not
    /// a line.
    ///
    /// A wrapped line is several rows, and `Down` moves one row: the window can
    /// therefore begin part-way down a line, which is what `top_row` records.
    /// Stepping whole lines instead - which is what this used to do - moves the
    /// window by however many rows the line happened to have, so `Down` jumps,
    /// `Up` undoes more than `Down` did, and `End` lands wherever a screenful
    /// of *lines* back from the end happens to be, which on a file of long
    /// lines is nowhere near the end.
    fn scroll_wrapped(&mut self, delta: isize, per_line: u64, budget: &mut u64) -> Result<()> {
        if delta > 0 {
            for _ in 0..delta {
                let rows = self.rows_in_line(self.top, per_line, budget)?;
                if self.top_row.saturating_add(1) < rows {
                    self.top_row = self.top_row.saturating_add(1);
                    continue;
                }
                let Some(step) = self.step_down(self.top, per_line, budget)? else {
                    break;
                };
                self.top = step.at;
                self.top_row = 0;
                self.top_at_line_start = step.line_start;
                if step.line_start {
                    self.top_line = self.top_line.map(|l| l.saturating_add(1));
                }
            }
            return Ok(());
        }
        for _ in 0..delta.unsigned_abs() {
            if self.top_row > 0 {
                self.top_row = self.top_row.saturating_sub(1);
                continue;
            }
            if self.top <= self.bom_len {
                self.top = self.bom_len;
                self.top_line = Some(0);
                self.top_at_line_start = true;
                self.top_row = 0;
                break;
            }
            let step = if self.top_at_line_start {
                self.prev_line_start(self.top)?
            } else {
                // Inside a line too long to have been walked to its start: the
                // same byte step the unwrapped path takes, for the same reason.
                let back = self.top.saturating_sub(per_line).max(self.bom_len);
                Step {
                    at: self.resync_offset(back)?,
                    line_start: false,
                }
            };
            self.top = step.at;
            self.top_at_line_start = step.line_start || self.top <= self.bom_len;
            self.top_line = self.top_line.map(|l| l.saturating_sub(1));
            // Landing on the line above means landing on its **last** row, not
            // its first: `Up` from the top of a line shows the row immediately
            // above it, which is where that line ends.
            self.top_row = self
                .rows_in_line(self.top, per_line, budget)?
                .saturating_sub(1);
        }
        Ok(())
    }

    /// The start of the line before the one beginning at `before`.
    ///
    /// `before` is a line start, so the break that ended the previous line is
    /// immediately behind it: the answer is the start of the line containing
    /// that break.
    pub(crate) fn prev_line_start(&mut self, before: u64) -> Result<Step> {
        let unit = self.line_term().unit() as u64;
        self.line_start_at_or_before(before.saturating_sub(unit))
    }

    /// The start of the line containing `at`, and whether that is a proven line
    /// start or a bounded guess (the honesty rule).
    ///
    /// Bounded by [`NAV_READ_BYTES`], read in windows that start at
    /// [`SCAN_STEP`] and grow - the backward mirror of [`Viewer::scan_forward`],
    /// and cheap for the same reason.
    pub(crate) fn line_start_at_or_before(&mut self, at: u64) -> Result<Step> {
        let proven = |at| {
            Ok(Step {
                at,
                line_start: true,
            })
        };
        if at <= self.bom_len {
            return proven(self.bom_len);
        }
        let term = self.line_term();
        let unit = term.unit() as u64;
        let mut hi = at;
        let mut want = SCAN_STEP as u64;
        let mut budget = NAV_READ_BYTES;
        while budget > 0 {
            let span = want.min(budget);
            let span = span.saturating_sub(span % unit.max(1)).max(unit);
            let lo = hi.saturating_sub(span).max(self.bom_len);
            let len = usize::try_from(hi.saturating_sub(lo)).unwrap_or(0);
            if len == 0 {
                return proven(self.bom_len);
            }
            let w = self.source.read_window(lo, WindowLen::new(len))?;
            budget = budget.saturating_sub(w.len() as u64);
            if let Some(idx) = term.rfind(w.bytes(), w.len()) {
                return proven(w.at().saturating_add(idx as u64).saturating_add(unit));
            }
            if lo <= self.bom_len {
                return proven(self.bom_len);
            }
            hi = lo;
            want = want
                .saturating_mul(SCAN_GROWTH as u64)
                .min(source::MAX_WINDOW as u64);
        }
        // No line break within the budget. Land on a character boundary rather
        // than on a raw byte, and say the position is approximate.
        self.approximate = true;
        let at = self.resync_offset(hi)?;
        Ok(Step {
            at,
            line_start: false,
        })
    }

    /// The 0-based number of the line **containing** `at`, or `None` when the
    /// index has not reached far enough for the answer to be exact.
    ///
    /// Containing, not starting at: a screen can sit in the middle of a line
    /// longer than the read budget, and the line it is in is a fact the index
    /// knows - `Checkpoint::line` is "the 0-based number of the line that
    /// *contains* `offset`" for a mid-line checkpoint exactly as it is for a
    /// line-start one. Refusing to answer there printed `line ?/1` on a file
    /// with one line in it, which is not honesty but a missing subtraction.
    fn line_number_of(&mut self, at: u64) -> Option<u64> {
        if at <= self.bom_len {
            return Some(0);
        }
        if at > self.idx.scanned() {
            return None;
        }
        let cp = self.idx.checkpoint_for_offset(at);
        let term = self.line_term();
        let mut n = cp.line;
        let mut pos = cp.offset;
        // Bounded by the checkpoint spacing, which is the whole point of the
        // index being sparse rather than absent.
        for _ in 0..NAV_READ_BUDGET.saturating_mul(8) {
            if pos >= at {
                return (pos == at).then_some(n);
            }
            let want = usize::try_from(at.saturating_sub(pos))
                .unwrap_or(WindowLen::MAX.get())
                .min(WindowLen::MAX.get());
            let w = self.source.read_window(pos, WindowLen::new(want)).ok()?;
            if w.is_empty() {
                return None;
            }
            let mut i = 0_usize;
            while let Some(brk) = term.find(w.bytes(), i) {
                n = n.saturating_add(1);
                i = brk.saturating_add(term.unit());
            }
            pos = w.end();
        }
        None
    }
}
