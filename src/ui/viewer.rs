//! the viewer, drawn.
//!
//! `F3` takes the whole screen, exactly as `Ctrl+O` does for the console
//! ([`super::console`]), and for the same reason: the design says the viewer
//! consumes all input, and a viewer sharing the screen with two panels would
//! show three lines of a file.
//!
//! Everything drawn here comes out of [`crate::viewer::Viewer::rows`], which
//! the event loop refreshed before this frame - see
//! [`crate::app::App::service_viewer`]. **This module never reads the file.**
//! It has `&App`, and reading is the model's job; that split is what keeps
//! the memory rule checkable in one place.
//!
//! Colours are the `viewer.*` and `syn.*` slots, quantized for the
//! session's depth like everything else. No syntect theme is ever loaded.
//!

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;
use crate::config::ViewerMode;
use crate::input::Focus;
use crate::viewer::{MatchRun, Row, Status, Viewer, hex};

use super::text::{Crop, Glyphs};

/// Whether the viewer is what this frame is painted on.
///
/// True while `F3` has the screen, and **also** while a dialog is open over
/// that state - the viewer's own `Ctrl+G` prompt is one, and a background job
/// finishing raises its summary with no keystroke at all.
/// Without the second half, a copy completing while a file was being read would
/// pull the panels back up behind the dialog. `App::push_dialog` records where
/// focus came from, which is exactly the question.
pub fn is_backdrop(app: &App) -> bool {
    if app.viewer_is_shown() {
        return true;
    }
    app.dialogs()
        .first()
        .is_some_and(|frame| frame.restore == Focus::Viewer)
}

/// The full-screen viewer.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bg = Style::new()
        .fg(super::color(app, app.theme.viewer.fg))
        .bg(super::color(app, app.theme.viewer.bg));
    f.render_widget(Block::new().style(bg), area);

    let Some(viewer) = app.viewer() else {
        return;
    };

    // Title row, body, status row. On a terminal too short for chrome the body
    // takes everything, because a file with no room to be read is worse than
    // one with no room for its name.
    let (title, body, status) = split(area);
    if title.height > 0 {
        draw_title(f, app, viewer, title);
    }
    draw_body(f, app, viewer, body);
    // Over the body rather than beside it. The event loop lays the rows out
    // for `body_cols`, so taking columns for a panel here would reflow every
    // row behind it and move the bytes the panel is reading.
    if viewer.inspecting() && matches!(viewer.mode(), ViewerMode::Hex) {
        draw_inspector(f, app, viewer, body);
    }
    if status.height > 0 {
        draw_status(f, app, &viewer.status(), status);
    }
}

/// The readings of the bytes at the cursor, in a box over the top right.
///
/// Top right because the cursor is usually being walked down the left of the
/// dump and through the offsets; a box over the bottom would sit where the
/// eye is. It is clamped to the body, so a narrow terminal gets a narrower box
/// rather than a panic or a box drawn off the screen.
fn draw_inspector(f: &mut Frame, app: &App, viewer: &crate::viewer::Viewer, body: Rect) {
    use crate::viewer::inspect;

    let bytes = inspect::bytes_at(viewer.rows(), viewer.cursor());
    let readings = inspect::readings(&bytes);
    if readings.is_empty() || body.width < 12 || body.height < 3 {
        return;
    }

    let labels = inspect::label_width(&readings);
    let values = readings
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(0);
    // label, two spaces, value, one space either side, two borders.
    let want = labels
        .saturating_add(values)
        .saturating_add(6)
        .try_into()
        .unwrap_or(u16::MAX);
    let width = want.min(body.width);
    let rows: u16 = readings.len().try_into().unwrap_or(u16::MAX);
    let height = rows.saturating_add(2).min(body.height);

    let area = Rect {
        x: body.x.saturating_add(body.width.saturating_sub(width)),
        y: body.y,
        width,
        height,
    };

    let style = Style::new()
        .fg(super::color(app, app.theme.dialog.fg))
        .bg(super::color(app, app.theme.dialog.bg));
    let glyphs = crate::ui::text::Glyphs::new(app.config.ui.ascii_borders);
    let block = Block::bordered()
        .border_set(glyphs.border_set())
        .border_style(style)
        .title(format!(" 0x{:X} ", viewer.cursor()))
        .style(style);
    let inner = block.inner(area);
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(block, area);

    for (i, reading) in readings.iter().enumerate() {
        let Ok(row) = u16::try_from(i) else { break };
        if row >= inner.height {
            break;
        }
        let text = format!(
            " {:<labels$}  {}",
            reading.label,
            reading.value,
            labels = labels
        );
        let line = crate::ui::text::fit_left(
            &text,
            usize::from(inner.width),
            crate::ui::text::Crop::End,
            glyphs.ellipsis(),
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(line, style))),
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(row),
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// The rows the body gets, which the event loop needs before it lays out.
///
/// **The same answer [`split`] gives**, including the two-row case where the
/// chrome is dropped and the body takes everything: the event loop lays out for
/// this number and the renderer draws whatever was laid out, so a disagreement
/// between them is a blank screen. It was one - `split` gave the body all of a
/// two-row terminal while this returned zero, so `Viewer::layout` produced no
/// rows and the viewer painted nothing at all on a 1- or 2-row terminal, where
/// the panels print "terminal too small".
pub const fn body_rows(area: Rect) -> u16 {
    if area.height <= 2 {
        area.height
    } else {
        area.height.saturating_sub(2)
    }
}

/// The columns the body gets, once the line-number gutter is taken out.
///
/// Hex mode gets the **whole** width: its geometry is a [`hex::HexPlan`], which
/// already accounts for the offset gutter and crops itself to the terminal, so
/// subtracting the gutter here would take it out twice.
pub fn body_cols(app: &App, area: Rect) -> u16 {
    let Some(viewer) = app.viewer() else {
        return area.width;
    };
    body_cols_for(viewer, area)
}

/// [`body_cols`] for a viewer that is not the stack's top.
///
/// the quick view is a `Viewer` drawn into a panel body rather
/// than into the screen, and it has to be laid out for the columns it will
/// actually get - line-number gutter included - or its rows are wider than the
/// panel that draws them. The stack version delegates here, so there is one
/// rule and not two.
pub fn body_cols_for(viewer: &Viewer, area: Rect) -> u16 {
    if matches!(viewer.mode(), ViewerMode::Hex) {
        return area.width;
    }
    area.width.saturating_sub(gutter_width(viewer, area.width))
}

/// The narrowest text column worth having a gutter beside.
///
/// Below this the line numbers are dropped and the whole width goes to the
/// file: a 1x1 terminal must render *something*, and five columns of gutter on
/// a six-column screen would leave one column of text. the "never
/// crash on a 1x1 terminal" is the floor; this is what makes the floor legible
/// rather than merely alive. Deliberately small - a 20-column terminal is
/// unpleasant but usable and keeps its numbers; the rule is for the terminals
/// where the gutter would be most of the screen.
const MIN_TEXT_COLS: u16 = 8;

fn split(area: Rect) -> (Rect, Rect, Rect) {
    if area.height <= 2 {
        return (
            Rect::new(area.x, area.y, area.width, 0),
            area,
            Rect::new(area.x, area.y, area.width, 0),
        );
    }
    let title = Rect::new(area.x, area.y, area.width, 1);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );
    let status = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    (title, body, status)
}

/// How wide the left gutter is: line numbers in text mode, the offset column in
/// hex.
///
/// `width` is the whole body's width, because a gutter that leaves no room for
/// the file is not a gutter. On a screen too narrow for both, the file wins -
/// the line number is a label on the text, and a label with nothing to label is
/// the wrong half to keep.
fn gutter_width(viewer: &Viewer, width: u16) -> u16 {
    match viewer.mode() {
        ViewerMode::Hex => {
            // One plan decides the whole screen's geometry, so the gutter here
            // and the rows below cannot disagree about where the bytes start -
            // including about whether the decimal offset column fits at all.
            let plan =
                hex::HexPlan::with_hex(viewer.hex(), viewer.len(), width, viewer.hex_config());
            let gutter = u16::try_from(plan.gutter_width()).unwrap_or(10);
            if width <= gutter { 0 } else { gutter }
        }
        ViewerMode::Text if viewer.line_numbers() => {
            // The widest number the index has proven, so the gutter does not
            // jump a column wider every time the scan crosses a power of ten
            // mid-file - four digits is the floor for the same reason.
            let widest = viewer.index().known_lines().max(1);
            let digits = format!("{widest}").len().max(4);
            let gutter = u16::try_from(digits.saturating_add(1)).unwrap_or(7);
            if width < gutter.saturating_add(MIN_TEXT_COLS) {
                0
            } else {
                gutter
            }
        }
        // Mode 3 numbers nothing: a rendered line is not a line of the file,
        // and a number that did not point at anything would be worse than the
        // space it took.
        ViewerMode::Text | ViewerMode::Render => 0,
    }
}

fn draw_title(f: &mut Frame, app: &App, viewer: &Viewer, area: Rect) {
    let style = Style::new()
        .fg(super::color(app, app.theme.panel.header_fg))
        .bg(super::color(app, app.theme.panel.header_bg));
    let glyphs = Glyphs::new(app.config.ui.ascii_borders);
    // The applied template is named here rather than in the status line
    // because the status line is already three claimants deep and drops its
    // fields by rank, and a colouring whose explanation can be squeezed out is
    // a colouring nobody can account for. `Crop::Middle` keeps both ends, so
    // the name survives a narrow terminal that has to cut the file name.
    let title = match viewer.template_name() {
        Some(name) => format!("{}  [{name}]", viewer.title()),
        None => viewer.title().to_string(),
    };
    let text = crate::panel::text::fit_with(
        &title,
        usize::from(area.width),
        Crop::Middle,
        crate::panel::text::Align::Left,
        glyphs.ellipsis(),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::raw(text))).style(style),
        area,
    );
}

/// The file's rows, painted into `area`.
///
/// `pub(crate)` for the quick view, which draws the same viewer
/// into a panel body rather than into the whole screen.
/// It takes the viewer explicitly rather than
/// reading it off `app`, so the caller decides which one is being painted and
/// this function has no opinion about where it came from.
pub(crate) fn draw_body(f: &mut Frame, app: &App, viewer: &Viewer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fg = super::color(app, app.theme.viewer.fg);
    let bg = super::color(app, app.theme.viewer.bg);
    let numbers = super::color(app, app.theme.viewer.line_numbers);
    let gutter = usize::from(gutter_width(viewer, area.width));
    let cols = usize::from(area.width).saturating_sub(gutter);
    let ascii = app.config.ui.ascii_borders;
    // With wrap on, `layout` already broke the line at the screen's width and
    // the row is the whole row. With wrap off the row is the whole *line* and
    // this is the window onto it (the "optional wrap").
    let hscroll = if viewer.wrap() { 0 } else { viewer.hscroll() };

    // One plan per frame, so every hex row on screen agrees about where the
    // bytes start and about whether the decimal offset column fits.
    let plan = hex::HexPlan::with_hex(viewer.hex(), viewer.len(), area.width, viewer.hex_config());
    let paint = RowPaint {
        bg: super::color(app, app.theme.viewer.bg),
        found: super::color(app, app.theme.viewer.match_),
        current: super::color(app, app.theme.viewer.current_match),
        sel_bg: super::color(app, app.theme.viewer.selection_bg),
        sel_fg: super::color(app, app.theme.viewer.selection_fg),
        // The marked-files foreground: a template makes a run of bytes known,
        // which is the same thing marking makes a file, and reusing the slot
        // means every theme already has an answer for it.
        covered: super::color(app, app.theme.panel.marked_fg),
    };
    let cfg = viewer.hex_config();
    // Empty unless a template is applied, in which case this is every field's
    // extent in the file, ascending and non-overlapping.
    let spans = viewer.template_spans();

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewer.rows().len());
    for row in viewer.rows() {
        lines.push(match row {
            Row::Text {
                line,
                first,
                text,
                spans,
                matches,
                sel,
                cut,
                ..
            } => {
                let cell = TextCell {
                    gutter,
                    cols,
                    hscroll,
                    wrap: viewer.wrap(),
                    ascii,
                    line: *line,
                    first: *first,
                    cut: *cut,
                    fg,
                    numbers,
                    paint,
                };
                text_line(app, &cell, text, spans, matches, sel.as_ref())
            }
            Row::Hex {
                offset,
                bytes,
                matches,
                sel,
                ..
            } => {
                // Once per row, from a binary search over the whole file's
                // field list - never per byte, and never rebuilt here: the
                // spans are a function of the cursor and the template, and
                // `Viewer::layout` has already made them current.
                let covered = crate::viewer::template::coverage(spans, *offset, bytes.len());
                hex_line(
                    app,
                    plan,
                    cfg,
                    *offset,
                    bytes,
                    matches,
                    sel.as_ref(),
                    fg,
                    paint,
                    &covered,
                )
            }
        });
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::new().fg(fg).bg(bg)),
        area,
    );
}

/// Everything about *where* a text row is drawn, so the row itself is the only
/// other argument.
struct TextCell {
    gutter: usize,
    cols: usize,
    hscroll: usize,
    wrap: bool,
    ascii: bool,
    line: Option<u64>,
    first: bool,
    cut: bool,
    fg: ratatui::style::Color,
    numbers: ratatui::style::Color,
    paint: RowPaint,
}

/// How a quick-find match and the selection are painted
/// (the `viewer.match`, `viewer.current_match`, `viewer.selection_*`).
///
/// **Both as a background.** For a match the foreground is already spoken for:
/// syntax highlighting owns it, and a match painted in
/// `viewer.match` inside a string literal would be indistinguishable from a
/// different `syn.*` slot. Every shipped theme gives both slots a bright,
/// saturated colour against a dark `viewer.bg`, which is exactly the classic
/// search highlight - and it survives the 16-colour quantizer, where two
/// similar foregrounds would not.
///
/// The selection **replaces** the syntax foreground inside it rather than
/// tinting it, for the same reason: two foregrounds do not survive the
/// 16-colour quantizer and the design requires that session to stay legible.
///
/// # Where the template's coverage fits
///
/// A byte a binary struct template explains is drawn in `covered`, which is
/// the marked-files foreground - a **foreground**, and therefore a third kind
/// of layer rather than a third background. It is the base a covered byte is
/// painted with, exactly where the syntax colour is the base of a text row, so
/// the two layers above it keep winning without knowing it is there:
///
/// ```text
/// current match  >  match  >  selection  >  template coverage  >  plain
/// ```
///
/// The selection is the thing the user is actively pointing at and it wins,
/// which is also the only order that works: the selection replaces both
/// colours of the cell, so a covered byte inside it would otherwise be a
/// foreground fighting a background chosen to go with a different one. A
/// covered byte that is also a match reads as a match for the same reason and
/// because finding is why a match is highlighted at all. The coverage is not
/// lost in either case - it is the rest of the field, which does not stop at
/// the selection's edge, that says the field is there.
#[derive(Debug, Clone, Copy)]
struct RowPaint {
    bg: ratatui::style::Color,
    found: ratatui::style::Color,
    current: ratatui::style::Color,
    sel_bg: ratatui::style::Color,
    sel_fg: ratatui::style::Color,
    covered: ratatui::style::Color,
}

impl RowPaint {
    /// The style for one match run.
    fn style(self, current: bool) -> Style {
        let bg = if current { self.current } else { self.found };
        Style::new().fg(self.bg).bg(bg)
    }

    /// The style for a selected cell.
    fn selection(self) -> Style {
        Style::new().fg(self.sel_fg).bg(self.sel_bg)
    }
}

/// Paint `text`, cut at the boundaries of every layer over it.
///
/// The painting order is the design, innermost wins:
///
/// ```text
/// current match  >  match  >  selection  >  syntax span  >  plain
/// ```
///
/// A match inside a selection still paints as a match, because finding is why
/// the match is highlighted at all; `plain` is whatever the caller has already
/// decided the syntax layer is, so this function never needs to know about it.
///
/// `sel` and `runs` are byte ranges into `text`, clipped to it, and every slice
/// goes through `get` - a coordinate mismatch must recolour, never panic.
fn paint_layers(
    out: &mut Vec<Span<'static>>,
    text: &str,
    plain: Style,
    sel: &[std::ops::Range<usize>],
    runs: &[MatchRun],
    paint: RowPaint,
) {
    // Every layer's edges, so each piece between two of them has one answer.
    let mut cuts: Vec<usize> =
        Vec::with_capacity(4 + sel.len().saturating_mul(2) + runs.len().saturating_mul(2));
    cuts.push(0);
    cuts.push(text.len());
    for r in sel.iter().chain(runs.iter().map(|m| &m.range)) {
        cuts.push(r.start.min(text.len()));
        cuts.push(r.end.min(text.len()));
    }
    cuts.sort_unstable();
    cuts.dedup();

    // Adjacent pieces with the same answer are one span: a selection over a row
    // of plain text is one attribute run, not one per character.
    let mut pending: Option<(usize, usize, Style)> = None;
    for pair in cuts.windows(2) {
        let (Some(&from), Some(&to)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        if from >= to {
            continue;
        }
        let style = layer_style(from, plain, sel, runs, paint);
        match pending {
            Some((was, end, had)) if had == style && end == from => {
                pending = Some((was, to, had));
            }
            Some((was, end, had)) => {
                push_piece(out, text, was..end, had);
                pending = Some((from, to, style));
            }
            None => pending = Some((from, to, style)),
        }
    }
    if let Some((from, to, style)) = pending {
        push_piece(out, text, from..to, style);
    }
}

/// Which layer owns the byte at `at`.
fn layer_style(
    at: usize,
    plain: Style,
    sel: &[std::ops::Range<usize>],
    runs: &[MatchRun],
    paint: RowPaint,
) -> Style {
    // The current match outranks an ordinary one where the two overlap, which
    // is the whole point of having two slots.
    if let Some(run) = runs
        .iter()
        .filter(|m| m.range.start <= at && at < m.range.end)
        .max_by_key(|m| m.current)
    {
        return paint.style(run.current);
    }
    if sel.iter().any(|r| r.start <= at && at < r.end) {
        return paint.selection();
    }
    plain
}

/// One span, or nothing at all when the range is not a slice of `text`.
fn push_piece(
    out: &mut Vec<Span<'static>>,
    text: &str,
    range: std::ops::Range<usize>,
    style: Style,
) {
    if let Some(piece) = text.get(range)
        && !piece.is_empty()
    {
        out.push(Span::styled(piece.to_string(), style));
    }
}

/// Clip a row-local byte range to the slice of the row that is on screen, and
/// rebase it onto that slice.
///
/// The twin of [`crate::viewer::slice_row_matches`], for the one range that is
/// not a match.
fn clip_range(
    range: Option<&std::ops::Range<usize>>,
    within: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let range = range?;
    let from = range.start.max(within.start);
    let to = range.end.min(within.end);
    (from < to).then(|| from.saturating_sub(within.start)..to.saturating_sub(within.start))
}

/// The slice of a text row that is on screen, and the marker its last column
/// carries when there is more of the line than fits.
///
/// One answer, used by [`text_line`] to draw the row and by [`cursor_cell`] to
/// place the terminal cursor in it: two computations of "which bytes are on
/// screen" would put the cursor a column away from the character it is on.
fn visible_slice(cell: &TextCell, text: &str) -> (std::ops::Range<usize>, Option<&'static str>) {
    // With wrap off this is where horizontal scrolling happens; with wrap on
    // `hscroll` is zero and the whole row is visible.
    let full = crate::viewer::text::column_range(text, cell.hscroll, cell.cols);
    // Two different facts, and the row can only carry one of them. A line cut
    // at `MAX_LINE_BYTES` has been given up on; a line that
    // merely runs past the right edge with wrap off is the ordinary case and
    // `Right` will show the rest. Saying neither would make a long line and a
    // short one look the same.
    let mark = if cell.cut && full.end >= text.len() {
        Some(crate::viewer::text::cut_mark(cell.ascii))
    } else if !cell.wrap && full.end < text.len() {
        Some(crate::viewer::text::more_mark(cell.ascii))
    } else {
        None
    };
    // The marker is drawn *in* the row rather than past its right edge, because
    // past the right edge is where the terminal throws it away.
    let visible = if mark.is_some() {
        crate::viewer::text::column_range(text, cell.hscroll, cell.cols.saturating_sub(1))
    } else {
        full
    };
    (visible, mark)
}

fn text_line(
    app: &App,
    cell: &TextCell,
    text: &str,
    spans: &[crate::viewer::highlight::Span],
    matches: &[MatchRun],
    sel: Option<&std::ops::Range<usize>>,
) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();

    let (visible, mark) = visible_slice(cell, text);
    let piece = text.get(visible.clone()).unwrap_or("");
    let spans = crate::viewer::text::slice_spans(spans, &visible);
    let matches = crate::viewer::slice_row_matches(matches, &visible);
    // the selection, already a byte range into this row's text and
    // sliced for the horizontal scroll by the same window the row is drawn
    // through. A rectangular block's column band was turned into this range by
    // `Viewer::layout`, not here.
    let sel = clip_range(sel, &visible);

    if cell.gutter > 0 {
        // The column that separates the number from the text says whether the
        // start of the line is off the left edge - free, where a marker in the
        // text would cost a column of file.
        let sep = if visible.start > 0 {
            crate::viewer::text::less_mark(cell.ascii)
        } else {
            " "
        };
        // A continuation row of a wrapped line leaves the gutter blank, so the
        // number is printed once per line rather than once per row.
        let label = match (cell.first, cell.line) {
            (true, Some(n)) => format!(
                "{:>w$}{sep}",
                n.saturating_add(1),
                w = cell.gutter.saturating_sub(1)
            ),
            // The index has not reached here, so the number is not known - and
            // a guess in a gutter is worse than a gap.
            (true, None) => format!("{:>w$}{sep}", "?", w = cell.gutter.saturating_sub(1)),
            (false, _) => " ".repeat(cell.gutter),
        };
        out.push(Span::styled(label, Style::new().fg(cell.numbers)));
    }

    // Three layers over the same bytes: syntax owns the foreground, and a
    // selection and a quick-find match each own the background - in that order
    // of precedence. So the text is cut at the
    // syntax boundaries here and at the other two inside [`paint_layers`], and
    // every piece is asked which layer it is in. Everything goes through `get`,
    // so a coordinate mismatch recolours and never panics.
    if spans.is_empty() {
        paint_layers(
            &mut out,
            piece,
            Style::new().fg(cell.fg),
            sel.as_slice(),
            &matches,
            cell.paint,
        );
    } else {
        let mut at = 0_usize;
        for s in &spans {
            let end = s.range.end.min(piece.len());
            let start = s.range.start.min(end).max(at);
            if start >= end {
                continue;
            }
            if let Some(gap) = piece.get(at..start)
                && !gap.is_empty()
            {
                paint_layers(
                    &mut out,
                    gap,
                    Style::new().fg(cell.fg),
                    clip_range(sel.as_ref(), &(at..start)).as_slice(),
                    &crate::viewer::slice_row_matches(&matches, &(at..start)),
                    cell.paint,
                );
            }
            let Some(run) = piece.get(start..end) else {
                continue;
            };
            let color = s
                .slot
                .map_or(cell.fg, |slot| super::color(app, slot.color(&app.theme)));
            paint_layers(
                &mut out,
                run,
                Style::new().fg(color),
                clip_range(sel.as_ref(), &(start..end)).as_slice(),
                &crate::viewer::slice_row_matches(&matches, &(start..end)),
                cell.paint,
            );
            at = end;
        }
        if let Some(rest) = piece.get(at..)
            && !rest.is_empty()
        {
            paint_layers(
                &mut out,
                rest,
                Style::new().fg(cell.fg),
                clip_range(sel.as_ref(), &(at..piece.len())).as_slice(),
                &crate::viewer::slice_row_matches(&matches, &(at..piece.len())),
                cell.paint,
            );
        }
    }

    if let Some(mark) = mark {
        out.push(Span::styled(
            mark.to_string(),
            Style::new().fg(cell.numbers).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(out)
}

/// One hex row, laid out by [`hex::HexPlan`] and coloured here.
///
///
/// The geometry - how many hex digits the offset takes, whether the decimal
/// offset column fits, where the crop falls on a narrow terminal - is entirely
/// the plan's, and the colours are entirely this function's. That split is why
/// a 40-column terminal drops the decimal column rather than wrapping a row.
fn hex_line(
    app: &App,
    plan: hex::HexPlan,
    cfg: crate::config::HexConfig,
    offset: u64,
    bytes: &[u8],
    matches: &[MatchRun],
    sel: Option<&std::ops::Range<usize>>,
    fg: ratatui::style::Color,
    paint: RowPaint,
    covered: &[std::ops::Range<usize>],
) -> Line<'static> {
    let ascii = super::color(app, app.theme.viewer.hex_ascii);
    let offsets = super::color(app, app.theme.viewer.hex_offset);
    // The row's **own** byte count, not the layout's width: on the file's last
    // row `hex::value_column` writes the short trailing word as the digits it
    // has rather than as a value it cannot have, and `hex::value_span` gives
    // the same answer only when it is told the same number.
    //
    let held = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    let mut out: Vec<Span<'static>> = Vec::with_capacity(8);
    for piece in plan.row(offset, bytes) {
        match piece.part {
            // the "byte offsets are shown in both hex and decimal":
            // the same fact twice, so the same colour twice.
            hex::HexPart::Offset | hex::HexPart::Decimal => {
                out.push(Span::styled(piece.text, Style::new().fg(offsets)));
            }
            hex::HexPart::Gap => out.push(Span::styled(piece.text, Style::new().fg(fg))),
            // Where byte `i` is written is `hex::value_span`'s answer and
            // nobody else's: at `group = 32` a cell is eight digits and cells
            // are nine characters apart, so the renderer's old "three
            // characters per byte" painted every match and every selection in
            // the wrong place above the default grouping. It is also what
            // makes the "partly-covered columns partly highlighted" exact:
            // under `format = "hex"` only the covered bytes' own digits light
            // up inside an otherwise plain cell.
            // The coverage goes through `hex::value_spans` exactly as a
            // match does, and for the same reason: which characters byte `i`
            // is written as is that function's answer and nobody else's, so a
            // field boundary inside a grouped cell lights up the digits it
            // really owns rather than the whole cell.
            hex::HexPart::Bytes => paint_covered(
                &mut out,
                &piece.text,
                Style::new().fg(fg),
                &value_ranges(covered, held, cfg),
                &sel.map(|r| hex::value_spans(r.clone(), held, cfg))
                    .unwrap_or_default(),
                &value_runs(matches, held, cfg),
                paint,
            ),
            // In the gutter byte `i` is character `i`: every glyph
            // `hex::ascii_glyph` draws is one column wide and ASCII, so a
            // character index is a byte index and the arithmetic is exact. A
            // multi-byte character selected on the characters side therefore
            // lights up all of its gutter cells, which is the honest drawing of
            // "this character is these bytes".
            hex::HexPart::Ascii => paint_covered(
                &mut out,
                &piece.text,
                Style::new().fg(ascii),
                covered,
                sel.cloned().as_slice(),
                matches,
                paint,
            ),
        }
    }
    Line::from(out)
}

/// Paint one piece of a hex row, cut at the template's field boundaries first.
///
/// The covered runs are the **base** layer, not a layer over the others: each
/// piece goes on to [`paint_layers`], which puts the selection and the matches
/// on top of whichever base it is given. That is what makes the precedence
/// documented on [`RowPaint`] fall out of the structure rather than out of a
/// rule written twice - a selected byte inside a field is painted by exactly
/// the same code as a selected byte outside one.
///
/// `covered`, `sel` and `runs` are all indexes into `text`, and every slice
/// goes through `get`: a coordinate mismatch must recolour, never panic.
fn paint_covered(
    out: &mut Vec<Span<'static>>,
    text: &str,
    plain: Style,
    covered: &[std::ops::Range<usize>],
    sel: &[std::ops::Range<usize>],
    runs: &[MatchRun],
    paint: RowPaint,
) {
    if covered.is_empty() {
        paint_layers(out, text, plain, sel, runs, paint);
        return;
    }
    let lit = Style::new().fg(paint.covered);
    let mut at = 0_usize;
    let piece = |out: &mut Vec<Span<'static>>, from: usize, to: usize, style: Style| {
        let Some(slice) = text.get(from..to) else {
            return;
        };
        if slice.is_empty() {
            return;
        }
        paint_layers(
            out,
            slice,
            style,
            &clip_ranges(sel, &(from..to)),
            &crate::viewer::slice_row_matches(runs, &(from..to)),
            paint,
        );
    };
    for range in covered {
        let end = range.end.min(text.len());
        let start = range.start.min(end).max(at);
        if start >= end {
            continue;
        }
        piece(out, at, start, plain);
        piece(out, start, end, lit);
        at = end;
    }
    piece(out, at, text.len(), plain);
}

/// Clip row-local ranges to one slice of the row and rebase them onto it.
///
/// The many-ranges twin of [`clip_range`], which takes the one range a
/// selection is.
fn clip_ranges(
    ranges: &[std::ops::Range<usize>],
    within: &std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    ranges
        .iter()
        .filter_map(|r| clip_range(Some(r), within))
        .collect()
}

/// Turn byte-index coverage runs into character-index runs in the value
/// column, through the same mapping the matches go through.
///
/// Sorted and merged on the way out. Each field maps to its own disjoint
/// character spans and the fields are already in order, so this changes
/// nothing in practice; it is here because [`paint_covered`] walks the list
/// once and in order, and a list that arrived out of order would silently
/// drop a run rather than draw it wrong.
fn value_ranges(
    covered: &[std::ops::Range<usize>],
    width: u16,
    cfg: crate::config::HexConfig,
) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = covered
        .iter()
        .filter(|r| r.start < r.end)
        .flat_map(|r| hex::value_spans(r.clone(), width, cfg))
        .filter(|r| r.start < r.end)
        .collect();
    out.sort_unstable_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(out.len());
    for range in out {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Turn byte-index match runs into character-index runs in the value column,
/// through the one mapping that knows the grouping, the format and the byte
/// order.
fn value_runs(matches: &[MatchRun], width: u16, cfg: crate::config::HexConfig) -> Vec<MatchRun> {
    matches
        .iter()
        .filter(|m| m.range.start < m.range.end)
        .flat_map(|m| {
            hex::value_spans(m.range.clone(), width, cfg)
                .into_iter()
                .map(|range| MatchRun {
                    range,
                    current: m.current,
                })
        })
        .collect()
}

/// The status line.
///
/// the honesty rule lives here: an `End` or a percentage seek
/// answered from an index that has not finished is **marked approximate**, and
/// the index's own progress is shown, rather than the seek being refused or the
/// number being made up.
pub fn draw_status(f: &mut Frame, app: &App, status: &Status, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::new()
        .fg(super::color(app, app.theme.panel.status_fg))
        .bg(super::color(app, app.theme.panel.status_bg));
    let glyphs = Glyphs::new(app.config.ui.ascii_borders);
    // Three claimants on one row, in this order:
    //
    // 1. **The find bar**, while it is open. It is live input - what is being
    //    typed and what it has found - and the panel's quick search takes over
    //    the panel status line for exactly the same reason.
    //    A find error renders *inside* the bar, so nothing is lost by
    //    it outranking a message.
    // 2. **A message**, which belongs to the key that produced it and outranks
    //    the numbers, the same precedence the panel status line uses.
    //
    // 3. The numbers.
    let bar = app
        .viewer()
        .map(Viewer::find)
        .filter(|find| find.is_open())
        .map(crate::viewer::find::Find::bar_text);
    let text = match (bar, app.message.as_deref()) {
        (Some(bar), _) => bar,
        (None, Some(message)) => message.to_string(),
        (None, None) => status_fit(status, usize::from(area.width)),
    };
    let text = crate::panel::text::fit_with(
        &text,
        usize::from(area.width),
        Crop::End,
        crate::panel::text::Align::Left,
        glyphs.ellipsis(),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::raw(text))).style(style),
        area,
    );
}

/// What the status line separates its fields with.
const SEPARATOR: &str = "  ";

/// One field of the status line, and how hard it fights for the space.
///
/// the design settled this question for the panel - what the line reports
/// when it cannot report everything - and the viewer's line has the same
/// problem with more claimants: on 60 columns (the minimum) the whole
/// of it does not fit. The ranks are the answer, and they are the decision to
/// argue with:
///
/// * **[`RANK_TRUTH`] - the position and the honesty markers.** Dropped never.
///   the design allows an approximate answer *provided the line says so*, so
///   a line that cropped the word "approximate" away would turn a permitted
///   approximation into a lie. It goes before the fields that qualify nothing.
/// * **[`RANK_NAMED`] - the mode and the encoding.** the design
///   each name their field as belonging in this line, and `F8`'s whole purpose
///   is to be able to see the encoding change.
/// * **[`RANK_PROGRESS`] - the line number, the percentage, the index's
///   progress.** Where in the file this is. Useful, and reconstructible by
///   looking at the text.
/// * **[`RANK_DETAIL`] - the length, wrap, binary, invalid bytes.** Facts about
///   the file that do not change as you move through it.
/// * **[`RANK_TITLE`] - the name.** Last, and deliberately: it is the one thing
///   on this screen that is *also* somewhere else, on the title row a couple of
///   dozen lines above. Dropping it costs the least of anything here.
struct Field {
    rank: u8,
    text: String,
}

/// Never dropped: the position, and the markers that qualify it.
const RANK_TRUTH: u8 = 0;
/// Named by the design as belonging in this line.
const RANK_NAMED: u8 = 1;
/// Where in the file this is.
const RANK_PROGRESS: u8 = 2;
/// Facts about the file rather than about the position.
const RANK_DETAIL: u8 = 3;
/// The name, which the title row also carries.
const RANK_TITLE: u8 = 4;

/// Compose the status line's text. Separate from the painting so it can be
/// asserted on without a terminal.
///
/// This is everything, in display order and regardless of width;
/// [`status_fit`] is the same fields with the ones that do not fit dropped.
pub fn status_text(status: &Status) -> String {
    join(&fields(status))
}

/// [`status_text`], narrowed to `width` columns by dropping fields in reverse
/// priority order rather than by cropping the right-hand end off.
///
/// Cropping would take the honesty markers first - they are last in display
/// order - which is exactly backwards.
pub fn status_fit(status: &Status, width: usize) -> String {
    let mut fields = fields(status);
    if crate::panel::text::width(&join(&fields)) <= width {
        return join(&fields);
    }
    // Drop order: least important first, and within one rank the rightmost
    // first, so the line shortens from the end it reads to.
    let mut order: Vec<usize> = (0..fields.len()).collect();
    order.sort_by_key(|i| {
        (
            std::cmp::Reverse(fields.get(*i).map_or(0, |f| f.rank)),
            std::cmp::Reverse(*i),
        )
    });
    for i in order {
        if fields.get(i).is_none_or(|f| f.rank == RANK_TRUTH) {
            continue;
        }
        if let Some(f) = fields.get_mut(i) {
            f.text.clear();
        }
        if crate::panel::text::width(&join(&fields)) <= width {
            break;
        }
    }
    join(&fields)
}

fn join(fields: &[Field]) -> String {
    fields
        .iter()
        .filter(|f| !f.text.is_empty())
        .map(|f| f.text.as_str())
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

/// The status line's fields, in display order.
fn fields(status: &Status) -> Vec<Field> {
    let mut out: Vec<Field> = Vec::new();
    let mut push = |rank: u8, text: String| {
        if !text.is_empty() {
            out.push(Field { rank, text });
        }
    };

    push(RANK_TITLE, status.title.clone());
    // "Byte offsets are shown in both hex and decimal, and the
    // current offset under the cursor is in the status line."
    push(
        RANK_TRUTH,
        format!("0x{:X} ({})", status.offset, status.offset),
    );
    if let Some(len) = status.len {
        push(RANK_DETAIL, format!("of {len}"));
    }
    if let Some(p) = status.percent {
        push(RANK_PROGRESS, format!("{p}%"));
    }
    if status.mode == ViewerMode::Text {
        let line = match status.line {
            Some(n) => format!("line {}", n.saturating_add(1)),
            // The index has not reached here. A number would be a guess and
            // the design would rather have the gap.
            None => "line ?".to_string(),
        };
        // An unfinished index knows a *lower bound* on the line count and says
        // so with a `+`, the way the selection size says `≥`.
        let total = if status.indexed {
            format!("/{}", status.lines)
        } else {
            format!("/{}+", status.lines)
        };
        push(RANK_PROGRESS, format!("{line}{total}"));
    }
    push(
        RANK_NAMED,
        match status.mode {
            ViewerMode::Text => "text".to_string(),
            // the "`Tab` switches focus" has to be visible
            // somewhere, and the mode field is where the mode's own state
            // belongs.
            ViewerMode::Hex => format!("hex [{}]", status.side.label()),
            // Which renderer is showing, not merely that mode 3 is on: a JSON
            // file and an HTML one look nothing alike and the field is the one
            // place that says which of the three produced what is on screen.
            ViewerMode::Render => match status.render.as_deref() {
                Some(what) => format!("render [{what}]"),
                None => "render".to_string(),
            },
        },
    );
    // What the cursor is standing in, when a template explains it. Ranked
    // with the mode and the encoding rather than above them: it is the answer
    // this whole feature exists to give, but it describes the offset, and an
    // offset dropped to make room for the thing describing it would leave a
    // reading of nowhere. It outranks the line number, the percentage, the
    // file's own facts and the name.
    if let Some(field) = status.field.as_deref() {
        push(RANK_NAMED, field.to_string());
    }
    push(
        RANK_NAMED,
        format!("{} [{}]", status.encoding, status.encoding_how.label()),
    );
    // "A `width` that is not a whole number of words is
    // rounded down to one, and says so." Only in hex mode, and only when the
    // rounding actually took something away - a row narrower than the
    // configuration asked for is not something to discover by counting
    // columns. `RANK_DETAIL`, because the position comes first.
    if status.mode == ViewerMode::Hex
        && let Some((asked, used)) = status.hex_width_rounded
    {
        push(RANK_DETAIL, format!("hex_width {asked} rounded to {used}"));
    }
    // "the byte count in the status line is what says how many".
    // `RANK_NAMED` because it qualifies what `Ctrl+C` will do; a block reports
    // a span and a width and never a count it would have to read a hundred
    // megabytes of ragged lines to know.
    if let Some(sel) = status.selection.as_ref() {
        push(RANK_NAMED, sel.label());
    }
    // the readout, for 1, 2, 4 or 8 bytes - the same string
    // `Ctrl+Shift+C` copies. `RANK_DETAIL` because it is long and must not push
    // the position off the 60-column minimum.
    if let Some(reading) = status.interpretation.as_deref() {
        push(RANK_DETAIL, reading.to_string());
    }
    if status.wrap {
        push(RANK_DETAIL, "wrap".to_string());
    }
    if status.binary {
        push(RANK_DETAIL, "binary".to_string());
    }
    if status.decode_errors {
        push(RANK_DETAIL, "invalid bytes".to_string());
    }
    // The honesty markers. `indexing` is progress - it will
    // finish on its own - but `approximate` qualifies the position itself and
    // travels with it.
    if let Some(p) = status.index_percent {
        push(RANK_PROGRESS, format!("indexing {p}%"));
    }
    if status.approximate {
        push(RANK_TRUTH, "approximate".to_string());
    }
    if let Some(err) = status.error.as_deref() {
        push(RANK_NAMED, format!("index stopped: {err}"));
    }
    out
}

/// Where the terminal's own cursor goes inside the viewer.
///
///
/// The cursor's own cell when `viewer.cursor` is on and the cursor is on
/// screen; `None` otherwise, and [`super::hardware_cursor`] then parks it on
/// the status row exactly as the design describes.
///
/// **Pure.** It reads `Row::cursor`, the gutter and the hex plan, and nothing
/// else - the same three things the row was drawn from, so the cursor cannot
/// land a column away from the character it is on.
///
/// In hex mode the cell is on the **focused** side: the byte's own digits on
/// the bytes side, its gutter glyph on the characters side. That is the only
/// visible difference `Tab` makes, and it is enough - the selection is painted
/// on both sides whatever the focus.
pub fn cursor_cell(app: &App, area: Rect) -> Option<(u16, u16)> {
    let viewer = app.viewer()?;
    if !viewer.cursor_enabled() {
        return None;
    }
    let (_, body, _) = split(area);
    if body.width == 0 || body.height == 0 {
        return None;
    }
    // At most one row of a layout carries the cursor, and `Viewer::layout` is
    // what put it there.
    let index = viewer.rows().iter().position(|row| match row {
        Row::Text { cursor, .. } | Row::Hex { cursor, .. } => cursor.is_some(),
    })?;
    let down = u16::try_from(index).ok()?;
    if down >= body.height {
        return None;
    }
    let y = body.y.checked_add(down)?;
    let x = match viewer.rows().get(index)? {
        Row::Text {
            line,
            first,
            text,
            cursor,
            cut,
            ..
        } => {
            let at = (*cursor)?;
            let gutter = usize::from(gutter_width(viewer, body.width));
            let cell = TextCell {
                gutter,
                cols: usize::from(body.width).saturating_sub(gutter),
                hscroll: if viewer.wrap() { 0 } else { viewer.hscroll() },
                wrap: viewer.wrap(),
                ascii: app.config.ui.ascii_borders,
                line: *line,
                first: *first,
                cut: *cut,
                fg: super::color(app, app.theme.viewer.fg),
                numbers: super::color(app, app.theme.viewer.line_numbers),
                paint: RowPaint {
                    bg: super::color(app, app.theme.viewer.bg),
                    found: super::color(app, app.theme.viewer.match_),
                    current: super::color(app, app.theme.viewer.current_match),
                    sel_bg: super::color(app, app.theme.viewer.selection_bg),
                    sel_fg: super::color(app, app.theme.viewer.selection_fg),
                    // Never read on this path: placing the terminal cursor is
                    // arithmetic over widths and asks nothing about colour.
                    covered: super::color(app, app.theme.panel.marked_fg),
                },
            };
            let (visible, _) = visible_slice(&cell, text);
            if at < visible.start || at > visible.end {
                return None;
            }
            // Display columns, not bytes: a tab is the stops it draws as and a
            // wide character is two cells.
            let ahead = crate::panel::text::width(text.get(visible.start..at)?);
            if ahead >= cell.cols.max(1) {
                return None;
            }
            gutter.checked_add(ahead)?
        }
        Row::Hex {
            offset,
            bytes,
            cursor,
            ..
        } => {
            let at = (*cursor)?;
            let cfg = viewer.hex_config();
            let plan = hex::HexPlan::with_hex(viewer.hex(), viewer.len(), body.width, cfg);
            // The row's own byte count, for the same reason [`hex_line`] uses
            // it: the file's last row is short and its trailing word is drawn
            // as the digits it has.
            let held = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
            let side = viewer.hex_side();
            let want = match side {
                crate::viewer::select::HexSide::Bytes => hex::HexPart::Bytes,
                crate::viewer::select::HexSide::Chars => hex::HexPart::Ascii,
            };
            let mut left = 0_usize;
            let mut found: Option<usize> = None;
            for piece in plan.row(*offset, bytes) {
                let drawn = piece.text.chars().count();
                if piece.part == want {
                    let within = match side {
                        crate::viewer::select::HexSide::Bytes => {
                            hex::value_span(at, held, cfg)?.start
                        }
                        crate::viewer::select::HexSide::Chars => at,
                    };
                    // The plan crops itself to the terminal, so a cursor past
                    // the crop is genuinely not on screen.
                    if within >= drawn {
                        return None;
                    }
                    found = left.checked_add(within);
                    break;
                }
                left = left.saturating_add(drawn);
            }
            found?
        }
    };
    let x = u16::try_from(x).ok()?;
    if x >= body.width {
        return None;
    }
    body.x.checked_add(x).map(|x| (x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Config, Keymap, Loaded, Theme};
    use crate::viewer::select::{Extend, Motion};
    use crate::viewer::{Viewer, ViewerId, decode::Detected};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn app() -> App {
        let mut a = App::new(
            Loaded {
                config: Config::default(),
                keymap: Keymap::builtin(),
                theme: Theme::blue(),
                warnings: Vec::new(),
                ..Loaded::default()
            },
            crate::vfs::VfsPath::local("/tmp"),
        );
        a.color_depth = ColorDepth::TrueColor;
        a
    }

    fn with_viewer(body: &str) -> App {
        let mut a = app();
        let v = Viewer::open_memory(
            ViewerId(1),
            "sample.txt",
            body.to_string(),
            &a.config.viewer,
        )
        .expect("open");
        a.push_viewer(v);
        a
    }

    /// Lay out and draw exactly the way the event loop does (`src/main.rs`):
    /// `body_rows`/`body_cols` first, then the frame.
    fn shown(a: &mut App, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let rows = body_rows(area);
        let cols = body_cols(a, area);
        a.service_viewer(rows, cols);
        dump(&render(a, w, h))
    }

    fn render(a: &App, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| super::super::draw(f, a)).expect("draw");
        term.backend().buffer().clone()
    }

    fn dump(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_viewer_takes_the_whole_screen_and_the_panels_are_not_drawn() {
        let mut a = with_viewer("alpha\nbeta\ngamma\n");
        assert!(is_backdrop(&a));
        a.service_viewer(body_rows(Rect::new(0, 0, 60, 15)), 60);
        let out = dump(&render(&a, 60, 15));
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("sample.txt"), "{out}");
        assert!(
            !out.contains("Name"),
            "no panel column header is on screen:\n{out}"
        );
    }

    #[test]
    fn the_status_line_marks_an_approximate_seek_rather_than_hiding_it() {
        // "marked approximate in the status line rather than
        // blocked".
        let status = Status {
            title: "x".to_string(),
            mode: ViewerMode::Text,
            offset: 0x1F,
            len: Some(1024),
            percent: Some(50),
            line: None,
            lines: 40,
            indexed: false,
            index_percent: Some(12),
            approximate: true,
            encoding: "UTF-8",
            encoding_how: Detected::Sniffed,
            decode_errors: false,
            wrap: false,
            binary: false,
            highlighted: true,
            render: None,
            field: None,
            error: None,
            selection: None,
            interpretation: None,
            side: crate::viewer::select::HexSide::Bytes,
            hex_width_rounded: None,
        };
        let text = status_text(&status);
        assert!(text.contains("approximate"), "{text}");
        assert!(text.contains("indexing 12%"), "{text}");
        assert!(text.contains("line ?"), "{text}");
        assert!(text.contains("/40+"), "a lower bound says so: {text}");
        assert!(text.contains("0x1F (31)"), "both bases: {text}");
        assert!(text.contains("UTF-8 [auto]"), "{text}");
    }

    #[test]
    fn a_completed_index_stops_hedging() {
        let status = Status {
            title: "x".to_string(),
            mode: ViewerMode::Text,
            offset: 0,
            len: Some(10),
            percent: Some(0),
            line: Some(3),
            lines: 40,
            indexed: true,
            index_percent: None,
            approximate: false,
            encoding: "UTF-8",
            encoding_how: Detected::Bom,
            decode_errors: false,
            wrap: false,
            binary: false,
            highlighted: true,
            render: None,
            field: None,
            error: None,
            selection: None,
            interpretation: None,
            side: crate::viewer::select::HexSide::Bytes,
            hex_width_rounded: None,
        };
        let text = status_text(&status);
        assert!(text.contains("line 4/40"), "{text}");
        assert!(!text.contains('+'), "{text}");
        assert!(!text.contains("approximate"), "{text}");
        assert!(!text.contains("indexing"), "{text}");
    }

    #[test]
    fn hex_mode_draws_the_offset_the_bytes_and_the_gutter() {
        let mut a = with_viewer("AB\n");
        if let Some(v) = a.viewer_mut() {
            v.set_mode(ViewerMode::Hex).expect("mode");
        }
        a.service_viewer(body_rows(Rect::new(0, 0, 80, 10)), 80);
        let out = dump(&render(&a, 80, 10));
        assert!(out.contains("00000000"), "an offset column:\n{out}");
        assert!(out.contains("41 42 0a"), "the bytes:\n{out}");
        assert!(out.contains("AB."), "an ASCII gutter:\n{out}");
        assert!(
            out.contains("hex"),
            "and the mode in the status line:\n{out}"
        );
    }

    #[test]
    fn line_numbers_are_drawn_once_per_line_not_once_per_row() {
        let mut a = with_viewer("abcdefghij\nnext\n");
        if let Some(v) = a.viewer_mut() {
            v.toggle_wrap();
        }
        a.service_viewer(4, 6);
        let out = dump(&render(&a, 20, 6));
        let numbered = out
            .lines()
            .filter(|l| l.contains('1') && l.contains("abcd"))
            .count();
        assert_eq!(numbered, 1, "{out}");
    }

    #[test]
    fn a_tiny_terminal_draws_the_body_rather_than_only_chrome() {
        // Through `body_rows`, which is what the event loop calls: asking
        // `service_viewer` for two rows directly tested a path the program does
        // not take, and passed while a real 2-row terminal drew nothing at all.
        for (w, h) in [(1_u16, 1_u16), (10, 2), (20, 2), (60, 2)] {
            let mut a = with_viewer("alpha\nbeta\n");
            let area = Rect::new(0, 0, w, h);
            let rows = body_rows(area);
            let cols = body_cols(&a, area);
            a.service_viewer(rows, cols);
            let out = dump(&render(&a, w, h));
            assert!(
                out.chars().any(|c| !c.is_whitespace()),
                "{w}x{h} drew nothing at all:\n{out}"
            );
        }
        let mut a = with_viewer("alpha\nbeta\n");
        a.service_viewer(body_rows(Rect::new(0, 0, 10, 2)), 10);
        let out = dump(&render(&a, 10, 2));
        assert!(out.contains("alpha"), "{out}");
    }

    #[test]
    fn the_status_line_gives_up_the_name_before_it_gives_up_the_position() {
        // the minimum terminal is 60 columns and the whole line does
        // not fit in it. What survives is the decision (see `Field`).
        let status = Status {
            title: "/home/someone/projects/holoscommander/src/viewer/mod.rs".to_string(),
            mode: ViewerMode::Text,
            offset: 0x1F,
            len: Some(102_400),
            percent: Some(50),
            line: Some(120),
            lines: 4000,
            indexed: false,
            index_percent: Some(12),
            approximate: true,
            encoding: "UTF-8",
            encoding_how: Detected::Sniffed,
            decode_errors: false,
            wrap: false,
            binary: false,
            highlighted: true,
            render: None,
            field: None,
            error: None,
            selection: None,
            interpretation: None,
            side: crate::viewer::select::HexSide::Bytes,
            hex_width_rounded: None,
        };
        let full = status_text(&status);
        assert!(full.contains("mod.rs"), "everything fits when asked for it");

        let narrow = status_fit(&status, 60);
        assert!(
            crate::panel::text::width(&narrow) <= 60,
            "{narrow:?} is {} wide",
            crate::panel::text::width(&narrow)
        );
        assert!(
            !narrow.contains("mod.rs"),
            "the name goes first, the title row has it:\n{narrow}"
        );
        assert!(narrow.contains("0x1F (31)"), "{narrow}");
        assert!(narrow.contains("approximate"), "{narrow}");
        assert!(narrow.contains("UTF-8"), "\n{narrow}");
        assert!(narrow.contains("text"), "\n{narrow}");
    }

    #[test]
    fn a_status_line_with_almost_no_room_still_refuses_to_lie() {
        // the design permits an approximate answer *provided the line says
        // so*, which makes the marker the last thing that may be dropped -
        // and it is last in display order, so plain cropping would take it
        // first. That is the whole reason `status_fit` exists.
        let status = Status {
            title: "big.log".to_string(),
            mode: ViewerMode::Text,
            offset: 4096,
            len: None,
            percent: None,
            line: None,
            lines: 7,
            indexed: false,
            index_percent: Some(3),
            approximate: true,
            encoding: "UTF-8",
            encoding_how: Detected::Fallback,
            decode_errors: false,
            wrap: false,
            binary: false,
            highlighted: false,
            render: None,
            field: None,
            error: None,
            selection: None,
            interpretation: None,
            side: crate::viewer::select::HexSide::Bytes,
            hex_width_rounded: None,
        };
        for width in [24_usize, 30, 40] {
            let got = status_fit(&status, width);
            assert!(got.contains("approximate"), "at {width} columns: {got:?}");
            assert!(got.contains("0x1000"), "at {width} columns: {got:?}");
        }
        // Narrower than the two of them together, `status_fit` stops dropping
        // rather than dropping either: what it hands back is still both facts,
        // and the crop in `draw_status` is what the width finally costs. There
        // is no honest 4-column status line, and pretending otherwise by
        // dropping the position would only move which half is missing.
        let tiny = status_fit(&status, 4);
        assert!(tiny.starts_with("0x1000"), "{tiny:?}");
        assert!(tiny.contains("approximate"), "{tiny:?}");
    }

    #[test]
    fn wrap_off_scrolls_sideways_and_says_the_line_goes_on() {
        // the "optional wrap": with wrap off the row is the whole
        // line and the screen is a window onto it.
        let mut a = with_viewer("0123456789abcdefghij\nshort\n");
        let first = shown(&mut a, 20, 6);
        assert!(first.contains("012345678"), "{first}");
        assert!(
            first.contains('\u{00bb}'),
            "the line continues past the edge and says so:\n{first}"
        );

        if let Some(v) = a.viewer_mut() {
            for _ in 0..19 {
                v.scroll_horizontal(1).expect("right");
            }
        }
        let scrolled = shown(&mut a, 20, 6);
        assert!(scrolled.contains("6789ab"), "{scrolled}");
        assert!(
            !scrolled.contains("0123"),
            "the window moved, it did not widen:\n{scrolled}"
        );
        assert!(
            scrolled.contains("   1\u{00ab}"),
            "the gutter keeps the number and says the start is off the left:\n{scrolled}"
        );
    }

    #[test]
    fn toggling_wrap_keeps_the_top_of_the_window() {
        let body = "alpha\nbravo charlie delta echo foxtrot golf hotel\nindia\n";
        let mut a = with_viewer(body);
        if let Some(v) = a.viewer_mut() {
            v.scroll(1).expect("scroll");
        }
        let before = shown(&mut a, 30, 8);
        let top = a.viewer().map(Viewer::top);
        assert!(before.contains("bravo"), "{before}");

        if let Some(v) = a.viewer_mut() {
            v.toggle_wrap();
        }
        let after = shown(&mut a, 30, 8);
        assert_eq!(
            top,
            a.viewer().map(Viewer::top),
            "the top byte is the top byte"
        );
        assert!(
            after.contains("bravo"),
            "wrapping changes which bytes are visible, never which byte is first:\n{after}"
        );
        assert!(
            after.contains("hotel"),
            "and the rest of the line is now on screen:\n{after}"
        );
        let status = a.viewer().map(Viewer::status).expect("a viewer");
        assert!(
            status_text(&status).contains("wrap"),
            "and the status line says which mode it is in"
        );
    }

    #[test]
    fn the_three_shapes_that_break_a_naive_line_indexer_all_render() {
        // An empty file, a file that is one enormous line, and a file with no
        // trailing newline.
        let mut empty = with_viewer("");
        let out = shown(&mut empty, 60, 15);
        assert!(out.contains("sample.txt"), "{out}");
        assert!(out.contains("0x0 (0)"), "and a status line:\n{out}");

        let mut huge = with_viewer(&"x".repeat(200_000));
        let out = shown(&mut huge, 60, 15);
        assert!(out.contains("xxxxx"), "{out}");
        assert!(
            huge.viewer().is_some_and(|v| v.rows().len() <= 13),
            "a file with no line break in it costs a screenful of rows and no \
             more - the memory rule is about the window, and a \
             row per 200,000 bytes would be the file"
        );
        assert!(
            huge.viewer()
                .and_then(|v| v.rows().first().map(crate::viewer::Row::offset))
                == Some(0),
            "and it starts at the top of the file"
        );

        let mut bare = with_viewer("alpha\nomega");
        let out = shown(&mut bare, 60, 15);
        assert!(out.contains("alpha"), "{out}");
        assert!(
            out.contains("omega"),
            "a last line without a newline is still a line:\n{out}"
        );
    }

    #[test]
    fn every_size_from_one_by_one_up_draws_without_panicking() {
        // "never crash on a 1x1 terminal", and the viewer is drawn
        // before the too-small check because `F3` has the whole screen.
        //
        // Every size against **both modes and the find bar**, because they do
        // not share a narrow path: text mode gives the gutter back to the file
        // below `MIN_TEXT_COLS`, hex mode drops the decimal offset column and
        // then crops the row inside `HexPlan`, and the find bar takes the
        // status row away from the numbers entirely. 60x15 is the design's
        // documented floor and 1x1 is the one below which there is nothing.
        for (w, h) in [
            (1_u16, 1_u16),
            (2, 3),
            (7, 2),
            (13, 4),
            (60, 15),
            (79, 24),
            (200, 60),
        ] {
            for hex in [false, true] {
                for find in [false, true] {
                    let mut a = with_viewer("alpha\nbeta\tgamma\n\u{65e5}\u{672c}\u{8a9e}\nlast\n");
                    if let Some(v) = a.viewer_mut() {
                        if hex {
                            v.set_mode(ViewerMode::Hex).expect("mode");
                        }
                        if find {
                            v.open_find();
                            for c in "am".chars() {
                                v.find_type(c).expect("typing into the bar cannot fail");
                            }
                        }
                    }
                    let out = shown(&mut a, w, h);
                    assert_eq!(
                        out.lines().count(),
                        usize::from(h),
                        "{w}x{h} hex={hex} find={find} drew the wrong number of rows"
                    );
                    // And nothing raw reached the screen: a C0 control painted
                    // into a cell would move the terminal's cursor rather than
                    // draw (the control pictures).
                    assert!(
                        !out.chars()
                            .any(|c| c != '\n' && ((c as u32) < 0x20 || c as u32 == 0x7F)),
                        "{w}x{h} hex={hex} find={find} painted a raw control: {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_status_line_survives_every_width_down_to_nothing() {
        // The status row is the one place where several facts compete for a
        // width that may be zero (the honesty markers against the floor).
        // `status_fit` drops fields; the draw then crops what is left.
        // Neither may panic and neither may overrun.
        let mut a = with_viewer("alpha\nbeta\n");
        if let Some(v) = a.viewer_mut() {
            v.goto_end().expect("end");
        }
        let status = a.viewer().map(Viewer::status).expect("a viewer");
        for width in 0..=200_usize {
            let text = status_fit(&status, width);
            assert!(
                crate::panel::text::width(&text) <= width || width < 32,
                "{width} columns produced {text:?}"
            );
        }
        // And the position is never the field that gets dropped, however
        // narrow it gets: the design permits an approximate answer only
        // while the line says so.
        assert!(status_fit(&status, 1).contains("0x"));
    }

    #[test]
    fn a_narrow_screen_gives_the_gutter_back_to_the_file() {
        let mut a = with_viewer("alpha\nbeta\n");
        let wide = shown(&mut a, 60, 6);
        assert!(wide.contains("   1 alpha"), "{wide}");
        let narrow = shown(&mut a, 10, 6);
        assert!(
            narrow.contains("alpha"),
            "ten columns of gutter and no text is not a viewer:\n{narrow}"
        );
        assert!(!narrow.contains("   1 alpha"), "{narrow}");
    }

    #[test]
    fn ascii_borders_leaves_no_glyph_the_terminal_cannot_draw() {
        // every glyph has an ASCII counterpart.
        let mut a = with_viewer("0123456789abcdefghij\n");
        a.config.ui.ascii_borders = true;
        let out = shown(&mut a, 16, 5);
        assert!(
            out.is_ascii(),
            "a non-ASCII glyph reached the screen:\n{out:?}"
        );
        assert!(out.contains('>'), "the marker is still drawn:\n{out}");
    }

    #[test]
    fn highlighting_reaches_the_screen_in_the_themes_colours_not_syntects() {
        // syntect is driven for its scopes and the colours come
        // from the `syn.*` slots.
        let mut a = app();
        let v = Viewer::open_memory(
            ViewerId(2),
            "sample.rs",
            "fn main() {}\n".to_string(),
            &a.config.viewer,
        )
        .expect("open");
        a.push_viewer(v);
        let area = Rect::new(0, 0, 60, 8);
        let cols = body_cols(&a, area);
        a.service_viewer(body_rows(area), cols);
        let buf = render(&a, 60, 8);
        let row = 1_u16;
        let colours: Vec<_> = (0..60)
            .filter_map(|x| buf.cell((x, row)).map(|c| (c.symbol().to_string(), c.fg)))
            .collect();
        let keyword = colours
            .iter()
            .find(|(s, _)| s == "f")
            .map(|(_, c)| *c)
            .expect("the keyword is on screen");
        let body = colours
            .iter()
            .find(|(s, _)| s == "(")
            .map(|(_, c)| *c)
            .expect("punctuation is on screen");
        assert_ne!(
            keyword, body,
            "the keyword is painted from `syn.keyword`, not in the body colour"
        );
        let want = super::super::color(
            &a,
            crate::viewer::highlight::SynSlot::Keyword.color(&a.theme),
        );
        assert_eq!(keyword, want, "and it is that slot's colour");
    }

    #[test]
    fn paging_home_and_end_move_the_window_and_end_admits_it_is_approximate() {
        // the navigation keys, seen from the screen, and the "End … marked
        // approximate in the status line rather than blocked": nothing here
        // has run the background scan, so every line number is a lower bound
        // and `End` says so instead of waiting for the index.
        let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        let mut a = with_viewer(&body);
        let top = shown(&mut a, 60, 15);
        assert!(top.contains("line 1"), "{top}");
        assert!(!top.contains("line 60"), "{top}");

        if let Some(v) = a.viewer_mut() {
            v.page(true).expect("page down");
        }
        let paged = shown(&mut a, 60, 15);
        assert!(paged.contains("line 13"), "{paged}");
        assert!(
            !paged.contains("line 1 "),
            "a page down is a page, not a nudge:\n{paged}"
        );

        if let Some(v) = a.viewer_mut() {
            v.goto_end().expect("end");
        }
        let end = shown(&mut a, 60, 15);
        assert!(end.contains("line 200"), "{end}");
        assert!(
            end.contains("approximate"),
            "the index has not run, so the landing is approximate and says so:\n{end}"
        );

        if let Some(v) = a.viewer_mut() {
            v.goto_start().expect("start");
        }
        let home = shown(&mut a, 60, 15);
        assert!(home.contains("line 1"), "{home}");
        assert!(
            !home.contains("approximate"),
            "the top of the file is not a guess:\n{home}"
        );
    }

    #[test]
    fn a_percentage_seek_lands_and_the_screen_shows_where() {
        // the "percentage seeks", which are allowed to be
        // approximate but not to be refused.
        let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        let mut a = with_viewer(&body);
        if let Some(v) = a.viewer_mut() {
            v.goto_percent(50).expect("halfway");
        }
        let out = shown(&mut a, 60, 15);
        assert!(out.contains("line 1"), "{out}");
        // Halfway in, plus the screenful now counted below it: the
        // percentage is measured at the bottom of the window.
        assert!(
            (50..=60).any(|p| out.contains(&format!("{p}%"))),
            "the status line says how far in this is:\n{out}"
        );
    }

    #[test]
    fn a_message_outranks_the_numbers_in_the_status_line() {
        let mut a = with_viewer("alpha\n");
        a.message = Some("nothing bound to do yet".to_string());
        a.service_viewer(body_rows(Rect::new(0, 0, 80, 10)), 80);
        let out = dump(&render(&a, 80, 10));
        assert!(out.contains("nothing bound to do yet"), "{out}");
    }

    // ------------------------------------------- the design, painted ------

    /// The background of one cell of the rendered screen.
    fn bg_at(buf: &Buffer, x: u16, y: u16) -> ratatui::style::Color {
        buf.cell((x, y))
            .map_or(ratatui::style::Color::Reset, |c| c.bg)
    }

    /// The foreground of one cell.
    fn fg_at(buf: &Buffer, x: u16, y: u16) -> ratatui::style::Color {
        buf.cell((x, y))
            .map_or(ratatui::style::Color::Reset, |c| c.fg)
    }

    #[test]
    fn a_selection_is_painted_in_the_themes_own_slots_and_stops_where_it_ends() {
        // the design requires a selection to be visible; the design says
        // colours are theme slots and never literal colours, so this reads the
        // two new slots back out of the cells.
        //
        let mut a = with_viewer("abcdefghij\nsecond\n");
        let area = Rect::new(0, 0, 60, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let viewer = a.viewer_mut().expect("a viewer");
        for _ in 0..4 {
            viewer
                .move_cursor(Motion::Right, Extend::Linear)
                .expect("Shift+Right");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));

        let buf = render(&a, 60, 8);
        let gutter = 5_u16;
        let want_bg = super::super::color(&a, a.theme.viewer.selection_bg);
        let want_fg = super::super::color(&a, a.theme.viewer.selection_fg);
        for x in 0..4_u16 {
            assert_eq!(
                bg_at(&buf, gutter + x, 1),
                want_bg,
                "column {x} of the first row is selected:\n{}",
                dump(&buf)
            );
            assert_eq!(fg_at(&buf, gutter + x, 1), want_fg);
        }
        assert_ne!(
            bg_at(&buf, gutter + 4, 1),
            want_bg,
            "and the fifth byte is not"
        );
    }

    /// The layer order documented on [`RowPaint`], asserted directly.
    #[test]
    fn a_match_and_a_selection_both_beat_the_template_coverage() {
        use ratatui::style::Color;
        let paint = RowPaint {
            bg: Color::Black,
            found: Color::Yellow,
            current: Color::Magenta,
            sel_bg: Color::Blue,
            sel_fg: Color::White,
            covered: Color::Green,
        };
        let plain = Style::new().fg(Color::Gray);
        let mut out = Vec::new();
        // The whole run is covered; the middle two are selected and the last
        // two are a match.
        paint_covered(
            &mut out,
            "abcdef",
            plain,
            std::slice::from_ref(&(0..6)),
            std::slice::from_ref(&(2..4)),
            &[MatchRun {
                range: 4..6,
                current: false,
            }],
            paint,
        );
        let seen: Vec<(String, Option<Color>, Option<Color>)> = out
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg, s.style.bg))
            .collect();
        assert_eq!(
            seen,
            vec![
                // Covered and nothing else: the coverage foreground.
                ("ab".to_string(), Some(Color::Green), None),
                // Covered and selected: the selection owns both colours.
                ("cd".to_string(), Some(Color::White), Some(Color::Blue)),
                // Covered and matched: the match owns both colours.
                ("ef".to_string(), Some(Color::Black), Some(Color::Yellow)),
            ]
        );
    }

    /// Outside the covered runs nothing changes at all.
    #[test]
    fn coverage_leaves_the_bytes_it_does_not_explain_exactly_as_they_were() {
        use ratatui::style::Color;
        let paint = RowPaint {
            bg: Color::Black,
            found: Color::Yellow,
            current: Color::Magenta,
            sel_bg: Color::Blue,
            sel_fg: Color::White,
            covered: Color::Green,
        };
        let plain = Style::new().fg(Color::Gray);
        let mut with = Vec::new();
        paint_covered(&mut with, "abcdef", plain, &[], &[], &[], paint);
        let mut without = Vec::new();
        paint_layers(&mut without, "abcdef", plain, &[], &[], paint);
        assert_eq!(with.len(), without.len());
        assert_eq!(
            with.first().map(|s| (s.content.to_string(), s.style.fg)),
            Some(("abcdef".to_string(), Some(Color::Gray)))
        );

        // A covered run in the middle leaves both sides plain.
        let mut split = Vec::new();
        paint_covered(
            &mut split,
            "abcdef",
            plain,
            std::slice::from_ref(&(2..4)),
            &[],
            &[],
            paint,
        );
        let seen: Vec<(String, Option<Color>)> = split
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("ab".to_string(), Some(Color::Gray)),
                ("cd".to_string(), Some(Color::Green)),
                ("ef".to_string(), Some(Color::Gray)),
            ]
        );
    }

    /// A four-byte template, applied at the cursor.
    fn toy_template() -> crate::viewer::template::Template {
        crate::viewer::template::Template::parse(
            "name = \"Toy\"\n[[field]]\nname = \"head\"\ntype = \"u32\"\n",
        )
        .expect("parses")
    }

    #[test]
    fn a_template_colours_the_bytes_it_explains_on_both_hex_sides() {
        let mut a = with_viewer("0123456789abcdef0123456789abcdef");
        let area = Rect::new(0, 0, 80, 8);
        {
            let viewer = a.viewer_mut().expect("a viewer");
            viewer
                .set_mode(crate::config::ViewerMode::Hex)
                .expect("hex");
            viewer.set_template(Some(toy_template()));
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));

        let want = super::super::color(&a, a.theme.panel.marked_fg);
        let buf = render(&a, 80, 8);
        // Four two-digit columns on the bytes side and four gutter glyphs,
        // wherever the plan put them.
        let painted: Vec<u16> = (0..80).filter(|x| fg_at(&buf, *x, 1) == want).collect();
        assert_eq!(
            painted.len(),
            4 * 2 + 4,
            "four covered bytes, both sides:\n{}",
            dump(&buf)
        );
        // And nothing on the second row, which the template does not reach.
        assert!(
            (0..80).all(|x| fg_at(&buf, x, 2) != want),
            "the row past the template is untouched:\n{}",
            dump(&buf)
        );
    }

    /// The colouring marks where the structure is, so it does not move when
    /// the cursor does. It slid with the cursor once; a header that walked
    /// sideways under the arrow keys was the bug that found the rule.
    #[test]
    fn the_colouring_stays_put_when_the_cursor_moves() {
        let mut a = with_viewer("0123456789abcdef0123456789abcdef");
        let area = Rect::new(0, 0, 80, 8);
        {
            let viewer = a.viewer_mut().expect("a viewer");
            viewer
                .set_mode(crate::config::ViewerMode::Hex)
                .expect("hex");
            viewer.set_template(Some(toy_template()));
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let want = super::super::color(&a, a.theme.panel.marked_fg);
        let before: Vec<u16> = (0..80)
            .filter(|x| fg_at(&render(&a, 80, 8), *x, 1) == want)
            .collect();

        let viewer = a.viewer_mut().expect("a viewer");
        for _ in 0..2 {
            viewer
                .move_cursor(Motion::Right, Extend::None)
                .expect("Right");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let after: Vec<u16> = (0..80)
            .filter(|x| fg_at(&render(&a, 80, 8), *x, 1) == want)
            .collect();

        assert_eq!(after, before, "the colouring moved with the cursor");
    }

    #[test]
    fn the_selection_wins_over_the_template_where_the_two_meet() {
        let mut a = with_viewer("0123456789abcdef0123456789abcdef");
        let area = Rect::new(0, 0, 80, 8);
        {
            let viewer = a.viewer_mut().expect("a viewer");
            viewer
                .set_mode(crate::config::ViewerMode::Hex)
                .expect("hex");
            viewer.set_template(Some(toy_template()));
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let viewer = a.viewer_mut().expect("a viewer");
        // Out to byte 4 and then back two with `Shift`, which leaves the
        // cursor on 2 and bytes 2 and 3 selected. The template is applied at
        // the cursor, so it covers 2 to 5 and the selection is the first half
        // of it - which is the overlap this is about.
        for _ in 0..4 {
            viewer
                .move_cursor(Motion::Right, Extend::None)
                .expect("Right");
        }
        for _ in 0..2 {
            viewer
                .move_cursor(Motion::Left, Extend::Linear)
                .expect("Shift+Left");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));

        let buf = render(&a, 80, 8);
        let covered = super::super::color(&a, a.theme.panel.marked_fg);
        let sel_bg = super::super::color(&a, a.theme.viewer.selection_bg);
        let sel_fg = super::super::color(&a, a.theme.viewer.selection_fg);
        // Every selected cell is the selection's own pair, not the coverage
        // foreground over the selection's background.
        let selected: Vec<u16> = (0..80).filter(|x| bg_at(&buf, *x, 1) == sel_bg).collect();
        assert_eq!(selected.len(), 2 * 2 + 2, "two bytes, both sides");
        for x in &selected {
            assert_eq!(
                fg_at(&buf, *x, 1),
                sel_fg,
                "the selection owns both colours at {x}:\n{}",
                dump(&buf)
            );
            assert_ne!(fg_at(&buf, *x, 1), covered);
        }
        // The rest of the field is still coloured, which is what says the
        // field did not stop at the selection's edge.
        let still: Vec<u16> = (0..80).filter(|x| fg_at(&buf, *x, 1) == covered).collect();
        assert_eq!(
            still.len(),
            2 * 2 + 2,
            "the other two bytes:\n{}",
            dump(&buf)
        );
    }

    #[test]
    fn no_template_paints_nothing_and_the_title_says_which_one_is_applied() {
        let mut a = with_viewer("0123456789abcdef");
        let area = Rect::new(0, 0, 80, 8);
        if let Some(viewer) = a.viewer_mut() {
            viewer
                .set_mode(crate::config::ViewerMode::Hex)
                .expect("hex");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let want = super::super::color(&a, a.theme.panel.marked_fg);
        let buf = render(&a, 80, 8);
        assert!(
            (0..80).all(|x| fg_at(&buf, x, 1) != want),
            "nothing is coloured without a template:\n{}",
            dump(&buf)
        );
        assert!(!dump(&buf).contains("[Toy]"), "and nothing is named");

        if let Some(viewer) = a.viewer_mut() {
            viewer.set_template(Some(toy_template()));
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let out = dump(&render(&a, 80, 8));
        assert!(out.contains("[Toy]"), "the title names it:\n{out}");

        // And taking it away puts both back.
        if let Some(viewer) = a.viewer_mut() {
            viewer.set_template(None);
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let buf = render(&a, 80, 8);
        assert!(!dump(&buf).contains("[Toy]"));
        assert!((0..80).all(|x| fg_at(&buf, x, 1) != want));
    }

    #[test]
    fn both_hex_sides_show_the_same_selection_whichever_has_focus() {
        // "Selecting five bytes on the left and pressing `Tab`
        // leaves five characters selected on the right." Both sides are painted
        // whatever the focus is - a selection that vanished from one side on
        // `Tab` would be the exact thing the two sides exist to avoid.
        //
        let mut a = with_viewer("0123456789abcdef0123456789abcdef");
        a.viewer_mut()
            .expect("a viewer")
            .set_mode(crate::config::ViewerMode::Hex)
            .expect("hex");
        let area = Rect::new(0, 0, 80, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let viewer = a.viewer_mut().expect("a viewer");
        for _ in 0..5 {
            viewer
                .move_cursor(Motion::Right, Extend::Linear)
                .expect("Shift+Right");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));

        let want_bg = super::super::color(&a, a.theme.viewer.selection_bg);
        let buf = render(&a, 80, 8);
        let row = a.viewer().and_then(|v| v.rows().first().cloned());
        assert!(matches!(row, Some(Row::Hex { .. })), "a hex row");
        // Five bytes on the bytes side and five glyphs in the gutter, wherever
        // the plan put them: found by scanning the row for the slot's colour.
        let painted: Vec<u16> = (0..80).filter(|x| bg_at(&buf, *x, 1) == want_bg).collect();
        assert_eq!(
            painted.len(),
            5 * 2 + 5,
            "five two-digit columns and five gutter glyphs:\n{}",
            dump(&buf)
        );

        // `Tab` moves the focus and nothing else, so the same cells are lit.
        a.viewer_mut().expect("a viewer").switch_hex_side();
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let after = render(&a, 80, 8);
        let painted_after: Vec<u16> = (0..80)
            .filter(|x| bg_at(&after, *x, 1) == want_bg)
            .collect();
        assert_eq!(painted, painted_after, "Tab painted nothing differently");
    }

    #[test]
    fn a_partly_covered_column_is_partly_highlighted() {
        // "The bytes side shows the partly-covered columns
        // partly highlighted." At `group = 32` a cell is eight digits, and a
        // selection of the word's middle two bytes lights exactly four of them
        // - which is also the defect the design records: the
        // renderer's old "three characters per byte" was right only at
        // `group = 8`.
        let mut a = app();
        a.config.viewer.hex.group = crate::config::HexGroup::Bits32;
        a.config.viewer.default_mode = crate::config::ViewerMode::Hex;
        let v = Viewer::open_memory(
            ViewerId(3),
            "sample.bin",
            "0123456789abcdef".to_string(),
            &a.config.viewer,
        )
        .expect("open");
        a.push_viewer(v);
        a.viewer_mut()
            .expect("a viewer")
            .set_mode(crate::config::ViewerMode::Hex)
            .expect("hex");
        let area = Rect::new(0, 0, 100, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        // Bytes 1 and 2 of the first 32-bit word, made from the characters
        // side, which is the only side that can land inside a word.
        let viewer = a.viewer_mut().expect("a viewer");
        viewer.switch_hex_side();
        viewer
            .move_cursor(Motion::Right, Extend::None)
            .expect("one character in");
        for _ in 0..2 {
            viewer
                .move_cursor(Motion::Right, Extend::Linear)
                .expect("Shift+Right");
        }
        assert_eq!(
            a.viewer().and_then(|v| v.selection()).map(|s| s.range()),
            Some((1, 3))
        );
        a.service_viewer(body_rows(area), body_cols(&a, area));

        let want_bg = super::super::color(&a, a.theme.viewer.selection_bg);
        let buf = render(&a, 100, 8);
        let lit: Vec<u16> = (0..100).filter(|x| bg_at(&buf, *x, 1) == want_bg).collect();
        // Four of the cell's eight digits, plus the two gutter glyphs.
        assert_eq!(
            lit.len(),
            4 + 2,
            "half a word's digits and its two glyphs:\n{}",
            dump(&buf)
        );
        // And they are contiguous inside the cell rather than three characters
        // apart, which is what the deleted `scale_matches` would have drawn.
        let digits: Vec<u16> = lit.iter().copied().take(4).collect();
        for pair in digits.windows(2) {
            let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            assert_eq!(b.saturating_sub(*a), 1, "the digits are one cell apart");
        }
    }

    #[test]
    fn the_status_line_reports_the_selection_and_what_it_reads_as() {
        // "the byte count in the status line is what says how
        // many", and the interpretations readout for 1, 2, 4 or 8 bytes.
        //
        let mut a = with_viewer("abcdefghij\n");
        let area = Rect::new(0, 0, 200, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let viewer = a.viewer_mut().expect("a viewer");
        for _ in 0..4 {
            viewer
                .move_cursor(Motion::Right, Extend::Linear)
                .expect("Shift+Right");
        }
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let status = a.viewer().expect("a viewer").status();
        let line = status_text(&status);
        assert!(line.contains("sel 4 bytes"), "{line}");
        // `abcd` little-endian is 0x64636261; both orders are given because
        // `group = 8` has declared none (the design invariant 16).
        assert!(line.contains("4 bytes  61 62 63 64  ="), "{line}");
        assert!(line.contains("(LE)") && line.contains("(BE)"), "{line}");
        assert_eq!(
            status.interpretation.as_deref(),
            crate::viewer::copy::interpretations(
                b"abcd",
                a.viewer().expect("v").hex_config(),
                false
            )
            .as_deref(),
            "the reading shown is the reading Ctrl+Shift+C copies"
        );

        // One byte is singular, and a block reports a span and a width rather
        // than a count it would have to read to know.
        let viewer = a.viewer_mut().expect("a viewer");
        viewer.clear_selection();
        viewer
            .move_cursor(Motion::Right, Extend::Linear)
            .expect("one byte");
        a.service_viewer(body_rows(area), body_cols(&a, area));
        assert!(
            status_text(&a.viewer().expect("v").status()).contains("sel 1 byte"),
            "one byte is singular"
        );
    }

    #[test]
    fn the_mode_field_says_which_hex_side_has_focus() {
        // the `Tab` has to be visible somewhere.
        let mut a = with_viewer("abcdefghij\n");
        a.viewer_mut()
            .expect("a viewer")
            .set_mode(crate::config::ViewerMode::Hex)
            .expect("hex");
        let area = Rect::new(0, 0, 200, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        assert!(status_text(&a.viewer().expect("v").status()).contains("hex [bytes]"));
        a.viewer_mut().expect("a viewer").switch_hex_side();
        assert!(status_text(&a.viewer().expect("v").status()).contains("hex [chars]"));
        // And nothing of the kind is said in text mode, where there are no
        // sides.
        a.viewer_mut()
            .expect("a viewer")
            .set_mode(crate::config::ViewerMode::Text)
            .expect("text");
        let line = status_text(&a.viewer().expect("v").status());
        assert!(line.contains("  text  "), "{line}");
        assert!(
            !line.contains("[bytes]") && !line.contains("[chars]"),
            "{line}"
        );
    }

    #[test]
    fn the_hardware_cursor_sits_on_the_cursors_own_cell() {
        // the design keeps the hardware cursor visible; the design gave
        // the viewer somewhere better to put it than the status row.
        //
        let mut a = with_viewer("abcdefghij\nsecond\n");
        a.set_focus(crate::input::Focus::Viewer);
        let area = Rect::new(0, 0, 60, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        assert_eq!(
            cursor_cell(&a, area),
            Some((5, 1)),
            "the gutter, then byte 0"
        );

        let viewer = a.viewer_mut().expect("a viewer");
        viewer
            .move_cursor(Motion::Right, Extend::None)
            .expect("right");
        viewer
            .move_cursor(Motion::Down, Extend::None)
            .expect("down");
        a.service_viewer(body_rows(area), body_cols(&a, area));
        assert_eq!(cursor_cell(&a, area), Some((6, 2)));

        // With `viewer.cursor = false` there is no cursor to place and the
        // hardware one parks on the status row as v0.4 left it.
        let mut off = app();
        off.config.viewer.cursor = false;
        off.set_focus(crate::input::Focus::Viewer);
        let v = Viewer::open_memory(
            ViewerId(9),
            "s.txt",
            "abc\n".to_string(),
            &off.config.viewer,
        )
        .expect("open");
        off.push_viewer(v);
        off.service_viewer(body_rows(area), body_cols(&off, area));
        assert_eq!(cursor_cell(&off, area), None);
        assert_eq!(
            super::super::hardware_cursor(&off, area),
            Some((0, 7)),
            "the first cell of the status row"
        );
    }

    #[test]
    fn in_hex_the_cursor_cell_is_on_the_focused_side() {
        // the only visible difference `Tab` makes.
        let mut a = with_viewer("0123456789abcdef0123456789abcdef");
        a.set_focus(crate::input::Focus::Viewer);
        a.viewer_mut()
            .expect("a viewer")
            .set_mode(crate::config::ViewerMode::Hex)
            .expect("hex");
        let area = Rect::new(0, 0, 80, 8);
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let on_bytes = cursor_cell(&a, area).expect("a cell");
        a.viewer_mut().expect("a viewer").switch_hex_side();
        a.service_viewer(body_rows(area), body_cols(&a, area));
        let on_chars = cursor_cell(&a, area).expect("a cell");
        assert_ne!(on_bytes, on_chars, "the cell moved to the other side");
        assert_eq!(on_bytes.1, on_chars.1, "and stayed on the same row");
        assert!(
            on_chars.0 > on_bytes.0,
            "the gutter is to the right of the bytes"
        );
    }
}
