//! Turning the file into the rows a frame will draw.
//!
//! A layout is a **pure function of the window**: the size, the mode, the
//! wrap setting and where the cursor is. Nothing here remembers anything, so
//! laying the same viewer out twice at the same size costs the same and
//! produces the same rows, which is what lets the event loop lay a viewer out
//! the moment it is pushed and then again before it is drawn.
//!
//! The unit a row is measured in differs by mode and that is the whole
//! difficulty: text rows are grapheme clusters wrapped to a width, and hex
//! rows are a fixed number of bytes. Everything here that looks like
//! duplication between the two is the two units, not a missing abstraction.

use super::*;

impl Viewer {
    /// Build the rows for a screen `rows` high and `cols` wide.
    ///
    /// Called once a frame by the event loop, before drawing, for the same
    /// reason [`crate::ui::sync_view_rows`] is: the renderer only has `&App`,
    /// and reading is the model's job.
    ///
    /// **The bounded step.** Everything this reads is a function of `rows` and
    /// `cols`; nothing is a function of the file's size. That is the invariant
    /// to hold on to when changing this.
    pub fn layout(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.view_rows = rows;
        self.view_cols = cols;
        self.rows.clear();
        self.decode_errors = false;
        // Refreshed from the window this layout is about to read and from
        // nothing else, so a selection adds no read however far it spans
        // (the design invariant 4).
        self.sel_preview = None;
        // A function of the cursor and the template and of nothing this layout
        // reads, so it is refreshed before the early return: a zero-sized
        // window still has a cursor, and the next frame that does have room
        // must not have to wait for a cursor movement to colour anything.
        self.refresh_template_spans();
        if rows == 0 || cols == 0 {
            return Ok(());
        }
        match self.mode {
            ViewerMode::Hex => self.layout_hex(rows)?,
            ViewerMode::Render => self.layout_render(),
            ViewerMode::Text => self.layout_to_the_end(rows, cols)?,
        }
        // Invariant 3's floor. Laying out sets `cursor_row` exactly whenever
        // the cursor is on a row it produced; the clamp is for the case it is
        // not on one at all - a bare [`Viewer::scroll`] moves the window and
        // leaves the cursor where the user put it - where the honest answer is
        // still a row this screen has.
        if !self.rows.is_empty() {
            self.cursor_row = self.cursor_row.min(self.rows.len().saturating_sub(1));
        }
        Ok(())
    }

    /// [`Viewer::layout_text`], with the window kept on the file.
    ///
    /// The last page is the one with the last line at the bottom, and `Down`
    /// must stop there rather than march the window off the end into a blank
    /// screen. [`Viewer::scroll`] cannot enforce that on its own: it walks line
    /// starts one at a time and has no idea how many are left without reading
    /// ahead, and reading ahead on every keystroke is exactly what the design
    /// forbids.
    ///
    /// So the limit is enforced here, where the file has just been read and the
    /// answer is already paid for. A layout that ran out of file with rows to
    /// spare is one that has scrolled too far, and the shortfall is how far;
    /// scrolling back by it and laying out again lands on the last page.
    ///
    /// Hex mode needs none of this - a hex row is a fixed stride, so the last
    /// page is arithmetic and [`Viewer::clamp_hex_top`] does it up front.
    fn layout_to_the_end(&mut self, rows: u16, cols: u16) -> Result<()> {
        // Ragged lines mean scrolling back by the shortfall does not always
        // recover exactly that many rows, so this converges rather than
        // assuming. Bounded, and it only ever runs on the last page: away from
        // the end the first layout fills the screen and this is one comparison.
        for _ in 0..MAX_EOF_PULLBACKS {
            let short = self.layout_text(rows, cols)?;
            let Some(missing) = short.filter(|m| *m > 0) else {
                return Ok(());
            };
            // Already showing the top of the file: the file is simply shorter
            // than the window, and the blank rows below it are the truth.
            if self.top <= self.bom_len {
                return Ok(());
            }
            let back = isize::try_from(missing).unwrap_or(isize::MAX);
            self.scroll(-back)?;
            // The one place the window moves without a key. The cursor has not
            // moved, so its row index rises by exactly what the window fell by.
            //
            self.cursor_row = self.cursor_row.saturating_add(usize::from(missing));
            self.rows.clear();
        }
        self.layout_text(rows, cols)?;
        Ok(())
    }

    /// Mode 3's rows, taken straight from the rendered document.
    ///
    /// No read and no decode: the document was built once when the mode was
    /// entered and the lines are already the lines. This is the whole of why
    /// mode 3 is fast to scroll despite having been slow to open, and it is
    /// the trade the size ceiling pays for.
    ///
    /// A collapsed region draws its summary in place of its opening line, so
    /// the fold is visible as what it hides rather than as a line that
    /// silently means something else.
    fn layout_render(&mut self) {
        let Some(document) = self.rendered.as_ref() else {
            return;
        };
        let visible = self
            .folded()
            .window(self.render_top, usize::from(self.view_rows.max(1)));
        let cursor_line = self.render_cursor;
        let mut cursor_row = 0_usize;
        let mut built: Vec<Row> = Vec::with_capacity(visible.len());
        for (row, line) in visible.into_iter().enumerate() {
            let Some(source) = document.lines.get(line) else {
                break;
            };
            let collapsed = self.render_folds.contains(&line);
            let (text, spans) = match source.fold.as_ref().filter(|_| collapsed) {
                // A summary is one run of its own: the colour runs of the
                // expanded line describe text that is not being drawn.
                Some(fold) => (fold.summary.clone(), Vec::new()),
                None => (source.text.clone(), source.spans.clone()),
            };
            if line == cursor_line {
                cursor_row = row;
            }
            built.push(Row::Text {
                // The rendered line's own number, which is what a person
                // counting rows on screen is counting. It is deliberately not
                // a line of the file: the file's line 400 may be the middle of
                // one rendered paragraph.
                line: Some(line as u64),
                offset: 0,
                first: true,
                text,
                spans,
                matches: Vec::new(),
                sel: None,
                cursor: (line == cursor_line && self.cursor_enabled).then_some(0),
                cut: false,
            });
        }
        self.rows = built;
        self.cursor_row = cursor_row;
    }

    fn layout_hex(&mut self, rows: u16) -> Result<()> {
        let stride = self.hex.stride();
        let matcher = self.find.matcher().cloned();
        // The screen's bytes, plus enough slack that a match straddling the
        // bottom edge is still found whole and painted as far as it shows.
        let slack = matcher.as_ref().map_or(0, |m| m.overlap()) as u64;
        let want = usize::try_from(stride.saturating_mul(u64::from(rows)).saturating_add(slack))
            .unwrap_or(usize::MAX);
        let w = self.source.read_window(self.top, WindowLen::new(want))?;
        // Matched byte ranges over the whole window, once, as absolute file
        // offsets. Clipping them per row is arithmetic.
        let hits: Vec<(u64, u64, bool)> = match matcher.as_ref() {
            Some(m) => m
                .matches_in(w.bytes())
                .into_iter()
                .map(|r| {
                    let from = w.at().saturating_add(r.start as u64);
                    let to = w.at().saturating_add(r.end as u64);
                    (from, to, self.find.current() == Some(from))
                })
                .collect(),
            None => Vec::new(),
        };
        let live = self.sel;
        let with_cursor = self.cursor_enabled;
        let at_cursor = self.cursor;
        let width = self.hex.width();
        // the readout needs at most eight bytes at the selection's
        // low end, and this window has already been read.
        if let Some(sel) = live {
            let (lo, _) = sel.range();
            // Only when the window actually starts at or below the selection:
            // `Window::slice` clamps its low end to the window's own start, so
            // for a selection above the top of the screen it would answer with
            // the bytes at `top` and the status line would read out bytes the
            // user never selected (the design invariant 15). The
            // readout is documented as being for a selection "whose low end the
            // last layout had in its window", and this is that condition.
            if lo >= w.at() {
                let preview = w.slice(lo, lo.saturating_add(SEL_PREVIEW_BYTES as u64));
                if !preview.is_empty() {
                    self.sel_preview = Some(preview.to_vec());
                }
            }
        }
        for r in 0..u64::from(rows) {
            let at = self.top.saturating_add(r.saturating_mul(stride));
            let end = at.saturating_add(stride);
            let bytes = w.slice(at, end);
            if bytes.is_empty() && at >= w.end() {
                break;
            }
            let taken = bytes.len() as u64;
            let matches = hits
                .iter()
                .filter_map(|(from, to, current)| {
                    let lo = (*from).max(at);
                    let hi = (*to).min(at.saturating_add(taken));
                    (lo < hi).then(|| MatchRun {
                        range: (lo.saturating_sub(at) as usize)..(hi.saturating_sub(at) as usize),
                        current: *current,
                    })
                })
                .collect();
            let sel = live.and_then(|s| hex_row_sel(&s, at, taken, width));
            // A hex row is a fixed stride, so which row the cursor is on is
            // arithmetic and no search of `rows` is involved.
            let cursor = (with_cursor && at_cursor >= at && at_cursor < at.saturating_add(taken))
                .then(|| usize::try_from(at_cursor.saturating_sub(at)).unwrap_or(0));
            if cursor.is_some() {
                self.cursor_row = usize::try_from(r).unwrap_or(0);
            }
            self.rows.push(Row::Hex {
                offset: at,
                bytes: bytes.to_vec(),
                matches,
                sel,
                cursor,
            });
        }
        // The last row's end, which in hex is arithmetic: rows are a fixed
        // stride and the final one is short only at the end of the file.
        self.window_end = match self.rows.last() {
            Some(Row::Hex { offset, bytes, .. }) => offset.saturating_add(bytes.len() as u64),
            _ => self.top,
        };
        Ok(())
    }

    fn layout_text(&mut self, rows: u16, cols: u16) -> Result<Option<u16>> {
        let term = self.line_term();
        let columns = usize::from(cols);
        // How many bytes of each line are worth materialising: what the screen
        // can show, plus the horizontal scroll, times the widest encoding of a
        // character. A function of the terminal - never of the file.
        let per_line = self.per_line();

        let mut states = Vec::new();
        let mut hl = self.resume_highlighter();
        let matcher = self.find.matcher().cloned();
        let current = self.find.current();
        let enc = self.encoding;
        let mut at = self.top;
        let mut line_no = self.top_line;
        let mut produced = 0_usize;
        // Set when the loop stops because the file ended, as opposed to because
        // the screen filled or a budget ran out. Only the first means the
        // window has run off the end - see [`Viewer::layout_to_the_end`].
        let mut ran_out = false;
        // Two ceilings, both [`MAX_LAYOUT_BYTES`] and both a function of the
        // terminal: what this screen may **materialise**, and what it may
        // **read** chasing line starts. Only the first used to be counted, and
        // a screen of cut lines therefore read two megabytes a row while
        // believing it had spent five hundred bytes.
        let mut budget = MAX_LAYOUT_BYTES;
        let mut scan = MAX_LAYOUT_BYTES;
        // Is the row about to be produced the first row of its line? Only then
        // does the gutter print a number.
        let mut line_first = self.top_at_line_start;
        // The cursor and the selection, read once. Both are plain numbers, so
        // carrying them through the loop costs nothing and reads nothing
        // (the design invariant 4).
        let live = self.sel;
        let with_cursor = self.cursor_enabled;
        let at_cursor = self.cursor;
        let tab_width = self.tab_width;
        let ascii = self.ascii;

        while produced < usize::from(rows) && budget > 0 {
            let slice = self.read_line(at, per_line, &mut scan)?;
            if slice.bytes.is_empty() && slice.eof {
                ran_out = true;
                break;
            }
            budget = budget.saturating_sub(slice.bytes.len() as u64);
            let body = term.trim_break(&slice.bytes);
            // A **cut** line stops mid-file, so its last bytes may be half a
            // character; `decode_window` holds those back rather than inventing
            // a U+FFFD out of good bytes.
            let decoded = encoding::decode_window(enc, body, !slice.cut);
            self.decode_errors |= decoded.had_errors;
            let decoded = decoded.text;

            // Not while stalled: a stalled highlighter never advances its
            // parse position, so the state would describe the line it stopped
            // on rather than this row, and `hl_marks` would file that wrong
            // position under this row's offset for a later jump to resume from.
            if let Some(h) = hl.as_mut()
                && !h.is_stalled()
            {
                states.push((at, h.checkpoint()));
            }
            let spans = match hl.as_mut() {
                Some(h) => h.line(&decoded),
                None => Vec::new(),
            };
            // Matches, in the same decoded coordinates the spans are in. Both
            // are then carried across tab expansion together, so a tab-indented
            // line is coloured and highlighted where the columns actually are.
            let hits = match matcher.as_ref() {
                Some(m) => {
                    let raw = m.matches_in(body);
                    let dec = find::match_ranges_in_line(m, enc, body);
                    raw.into_iter()
                        .zip(dec)
                        .map(|(r, d)| (d, current == Some(at.saturating_add(r.start as u64))))
                        .collect::<Vec<_>>()
                }
                None => Vec::new(),
            };

            let (expanded, spans, hits) =
                expand_row(&decoded, self.tab_width, self.ascii, &spans, &hits);

            // Where this line ends, in the same terms `layout_text` steps in.
            let line_end = slice.next.max(at.saturating_add(1));
            let covered = live.filter(|s| s.covers_row(at, line_end));
            // A line wholly inside a **linear** selection needs no character
            // table: every one of its rows is selected end to end. That is what
            // keeps `Ctrl+A` over a 16 MB file costing the same layout as no
            // selection at all.
            let whole = covered.is_some_and(|s| {
                let (lo, hi) = s.range();
                matches!(s.kind, SelectKind::Linear) && lo <= at && line_end <= hi
            });
            let cursor_here = with_cursor && at_cursor >= at && at_cursor < line_end;
            // the readout, gathered from the lines this layout is
            // reading anyway (the design invariant 4). It is
            // **accumulated across lines**: a four-byte selection that steps
            // over a line break is four bytes like any other, and taking the
            // preview from the cursor's line alone left it short and dropped
            // the readout the status line owes it. The bytes are appended from
            // where the preview has got to, so what is gathered is contiguous
            // from the selection's low end whatever the lines do.
            if let Some(sel) = live {
                let (lo, _) = sel.range();
                let want = usize::try_from(sel.len())
                    .unwrap_or(SEL_PREVIEW_BYTES)
                    .min(SEL_PREVIEW_BYTES);
                let have = self.sel_preview.as_ref().map_or(0, Vec::len);
                let next = lo.saturating_add(have as u64);
                if have < want
                    && next >= at
                    && let Ok(from) = usize::try_from(next.saturating_sub(at))
                    && let Some(tail) = slice.bytes.get(from..)
                {
                    let take = tail.len().min(want.saturating_sub(have));
                    if let Some(more) = tail.get(..take) {
                        self.sel_preview
                            .get_or_insert_with(Vec::new)
                            .extend_from_slice(more);
                    }
                }
            }

            let ranges = if self.wrap {
                text::wrap(&expanded, columns)
            } else {
                // One row, the whole line; the renderer scrolls it sideways
                // with `hscroll` (the "optional wrap").
                std::iter::once(0..expanded.len()).collect()
            };
            // The character table is what turns a file offset into a column and
            // back, and it is built only for the rows that actually need one:
            // the cursor's, and a row a selection covers in part.
            let map = (cursor_here || (covered.is_some() && !whole)).then(|| {
                let chars = cursor::cells_of(enc, body, &decoded, at, tab_width, ascii).1;
                cursor::LineMap {
                    start: at,
                    line_start: line_first,
                    body_end: at.saturating_add(body.len() as u64),
                    next: line_end,
                    broke: slice.broke,
                    eof: slice.eof,
                    empty: false,
                    expanded: expanded.clone(),
                    chars,
                    rows: ranges.clone(),
                }
            });

            // The window may start part-way down a wrapped line (`top_row`), and
            // only ever down the *first* line it lays out.
            let skip = if at == self.top { self.top_row } else { 0 };
            for (i, r) in ranges.iter().enumerate().skip(skip) {
                if produced >= usize::from(rows) {
                    break;
                }
                let piece = expanded.get(r.clone()).unwrap_or("").to_string();
                let cursor = if cursor_here {
                    map.as_ref().and_then(|m| m.cursor_in_row(i, at_cursor))
                } else {
                    None
                };
                if cursor.is_some() {
                    self.cursor_row = produced;
                }
                let sel = text_row_sel(covered, map.as_ref(), i, &piece, whole);
                self.rows.push(Row::Text {
                    line: line_no,
                    offset: at,
                    first: line_first && i == 0,
                    text: piece,
                    // Row-local, so a continuation row of a wrapped line is
                    // highlighted from the part of the line it can see.
                    spans: text::slice_spans(&spans, r),
                    matches: slice_row_matches(&hits, r),
                    sel,
                    cursor,
                    cut: slice.cut && i.saturating_add(1) == ranges.len(),
                });
                produced = produced.saturating_add(1);
            }

            if slice.eof && slice.next <= at {
                ran_out = true;
                break;
            }
            at = slice.next.max(at.saturating_add(1));
            if slice.broke {
                line_no = line_no.map(|l| l.saturating_add(1));
                line_first = true;
            } else {
                // The row below is this same line continuing: the same number,
                // printed once. And the chase is over for this screen - the
                // rows below are all inside the line the last one gave up on,
                // so asking again would be the same read for the same answer.
                line_first = false;
                scan = 0;
            }
        }

        self.window_end = at.max(self.top);

        // Beside the index's checkpoints, so a jump back here resumes rather
        // than starting fresh. `save` is sparse and capped and
        // decides for itself which of these are worth keeping.
        for (offset, state) in &states {
            self.hl_marks.save(*offset, state);
        }
        self.hl_states = states;
        // How far short of a full screen the file ran out, if it did.
        Ok(ran_out
            .then(|| usize::from(rows).saturating_sub(produced))
            .and_then(|missing| u16::try_from(missing).ok()))
    }

    /// Resume highlighting for the row at the top of the screen.
    ///
    /// "highlighting can start from a checkpoint and cover just
    /// the visible window". The checkpoints are the parse states saved by the
    /// previous layout, which covers every ordinary scroll. A jump has no
    /// checkpoint to resume from and starts fresh at the top of the window -
    /// bounded and occasionally wrong about a multi-line construct that began
    /// off-screen, which is the trade the design makes everywhere else too.
    fn resume_highlighter(&mut self) -> Option<Highlighter> {
        if !self.highlighting {
            return None;
        }
        if self.source.len().is_some_and(|l| l > self.highlight_limit) {
            return None;
        }
        if let Some((_, state)) = self.hl_states.iter().find(|(at, _)| *at == self.top) {
            return Some(state.checkpoint());
        }
        // A jump has no state from the last layout. The kept checkpoints are
        // the next-best position: exact if this offset has been the top of a
        // screen before, otherwise the nearest one behind it plus a bounded
        // walk forward (the "start from a checkpoint").
        if let Some(state) = self.hl_marks.exact(self.top) {
            return Some(state);
        }
        if let Some((from, mut state)) = self.hl_marks.resume(self.top, HL_CATCH_UP)
            && self.catch_up(&mut state, from, self.top, HL_CATCH_UP_TIME)
        {
            // A checkpoint of the caught-up position, so the visible window
            // parses on its own budget rather than on what the walk left of
            // one (see [`HL_CATCH_UP_TIME`]).
            let mut resumed = state.checkpoint();
            resumed.set_budget(highlight::PARSE_BUDGET);
            return Some(resumed);
        }
        let name = self
            .path
            .as_ref()
            .and_then(VfsPath::file_name)
            .unwrap_or_else(|| self.title.clone());
        let first = self.first_line_text().unwrap_or_default();
        Highlighter::for_file(&name, &first)
    }

    /// Walk a resumed highlighter forward from `from` to `to`, one line at a
    /// time.
    ///
    /// False when it could not get there inside [`HL_CATCH_UP`] bytes or
    /// [`HL_CATCH_UP_TIME`], or when a read failed, or when the walk stepped
    /// **past** `to` - a parse state for the wrong line is worse than no parse
    /// state, because it is wrong silently. The caller starts fresh on false.
    ///
    /// A walk that **stalls** counts as a failure too. A stalled highlighter
    /// returns every line plain and stops advancing its parse position, so
    /// carrying on would spend the rest of the walk learning nothing and then
    /// hand back a position belonging to the line it stopped on.
    pub(super) fn catch_up(
        &mut self,
        hl: &mut Highlighter,
        from: u64,
        to: u64,
        parse: std::time::Duration,
    ) -> bool {
        let term = self.line_term();
        let enc = self.encoding;
        // The walk's own time budget, so the visible window keeps its own.
        hl.set_budget(parse);
        let mut at = from;
        let mut budget = HL_CATCH_UP;
        let mut scan = HL_CATCH_UP;
        while at < to {
            if budget == 0 {
                return false;
            }
            let Ok(slice) = self.read_line(at, text::MAX_LINE_BYTES.min(budget), &mut scan) else {
                return false;
            };
            if slice.bytes.is_empty() && slice.eof {
                return false;
            }
            budget = budget.saturating_sub(slice.bytes.len() as u64);
            let body = term.trim_break(&slice.bytes);
            // Window-local decoding, never `Encoding::decode`: a line inside a
            // cp437 file that happens to begin `FF FE` is content, not a BOM,
            // and the BOM-sniffing spelling would decode it as UTF-16 and hand
            // the parser a different file's worth of text (and
            // `encoding::decode_body`, which exists to say so).
            let decoded = encoding::decode_window(enc, body, !slice.cut).text;
            let _ = hl.line(&decoded);
            if hl.is_stalled() {
                return false;
            }
            let next = slice.next.max(at.saturating_add(1));
            if next <= at {
                return false;
            }
            at = next;
        }
        at == to
    }

    /// The first line of the file, for syntax detection by shebang.
    fn first_line_text(&mut self) -> Option<String> {
        let w = self
            .source
            .read_window(self.bom_len, WindowLen::new(1024))
            .ok()?;
        let term = self.line_term();
        let found = term.find(w.bytes(), 0);
        let end = found.unwrap_or(w.len());
        let body = w.bytes().get(..end).unwrap_or(&[]);
        // Window-local, for the same reason [`Viewer::catch_up`] is: this is a
        // window, and `Encoding::decode` would read its first bytes as a BOM.
        Some(encoding::decode_window(self.encoding, body, found.is_some()).text)
    }

    /// Read one line's bytes, capped at `max`.
    ///
    /// `budget` is what may be spent finding where the *next* row begins when
    /// the line runs past `max`. It belongs to the caller and is shared across
    /// the screen, because that search is the one part of laying a screen out
    /// whose cost is a function of the file rather than of the terminal.
    ///
    pub(super) fn read_line(&mut self, at: u64, max: u64, budget: &mut u64) -> Result<LineSlice> {
        let term = self.line_term();
        let want = usize::try_from(max).unwrap_or(WindowLen::MAX.get());
        let w = self.source.read_window(at, WindowLen::new(want))?;
        if let Some(idx) = term.find(w.bytes(), 0) {
            let take = idx.saturating_add(term.unit()).min(w.len());
            return Ok(LineSlice {
                bytes: w.bytes().get(..take).unwrap_or(&[]).to_vec(),
                cut: false,
                next: w.at().saturating_add(take as u64),
                eof: false,
                broke: true,
            });
        }
        if w.hit_eof() {
            self.remember_no_break(at, w.end(), true);
            return Ok(LineSlice {
                bytes: w.bytes().to_vec(),
                cut: false,
                next: w.end(),
                eof: true,
                broke: true,
            });
        }
        // The line runs past what is worth materialising. Show what fits, say
        // it was cut, and let [`Viewer::continue_after`] decide whether the row
        // below is the next line or the rest of this one - without keeping any
        // of what lies between (the memory rule).
        let (step, used) = self.continue_after(&w, budget)?;
        Ok(LineSlice {
            bytes: w.bytes().get(..used).unwrap_or(&[]).to_vec(),
            cut: true,
            next: step.at,
            eof: false,
            broke: step.line_start,
        })
    }
}
