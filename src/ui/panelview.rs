//! One panel's interior.
//!
//! Every row of the layout diagram that lives inside a panel box is drawn
//! here, in order: the volume line as the block's top border title, the tab
//! bar, the path-and-filter line, the column header with the sort arrow
//! prefixed, the entry rows with the three cursor styles of the design, the
//! horizontal rule, and the panel status line with the sort indicator at its
//! right end.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::config::Rgb;
use crate::input::Focus;
use crate::panel::{Panel, Side, SortKey, Tab};
use crate::vfs::Entry;
use crate::vfs::list::ListStatus;

use super::columns::{self, Allocated};
use super::filetype;
use super::quickview;
use super::text::{self, Crop, Glyphs};
use super::volume;

/// Which of the three cursor styles a panel's cursor bar is drawn in.
/// All three are *drawn*; only the style differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// The active panel while a panel has focus: `cursor_bg` / `cursor_fg`.
    Focused,
    /// The active panel while the command line has focus:
    /// `cursor_bg_unfocused` / `cursor_fg_unfocused`.
    Unfocused,
    /// The panel that is not active: `inactive_cursor_bg` - a third, weaker
    /// style, visible enough to show where you left off.
    Inactive,
}

impl CursorStyle {
    /// Which style this panel's cursor bar takes, given the app's focus.
    pub fn of(app: &App, side: Side) -> Self {
        if app.active_side != side {
            Self::Inactive
        } else if matches!(app.focus, Focus::Panel(s) if s == side) {
            Self::Focused
        } else {
            Self::Unfocused
        }
    }

    /// The `(background, foreground)` slots for this style.
    pub fn colors(self, theme: &crate::config::Theme) -> (Rgb, Rgb) {
        match self {
            Self::Focused => (theme.panel.cursor_bg, theme.panel.cursor_fg),
            Self::Unfocused => (
                theme.panel.cursor_bg_unfocused,
                theme.panel.cursor_fg_unfocused,
            ),
            Self::Inactive => (
                theme.panel.inactive_cursor_bg,
                theme.panel.inactive_cursor_fg,
            ),
        }
    }
}

/// Where each interior row of a panel landed.
///
/// A row that did not fit is a zero-height [`Rect`]; every drawing function
/// checks for that before writing, so a terminal that is short by a row
/// degrades instead of panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelRows {
    /// The tab bar. Zero-height when the panel has one tab.
    pub tab_bar: Rect,
    /// The column header.
    pub header: Rect,
    /// The entry rows.
    pub entries: Rect,
    /// The horizontal rule above the status line. Spans the full panel width,
    /// border columns included, so it draws `├──┤`.
    pub rule: Rect,
    /// The panel status line.
    pub status: Rect,
}

/// Carve a panel box - borders included - into its interior rows.
///
/// Allocation order when space is short: header, status, rule, tab bar, and
/// whatever is left becomes entry rows.
///
/// There is no path row. The path lives in the block's top border alongside the
/// volume line - one row carrying both rather than two rows each
/// naming the same directory, which is a whole entry row bought back at every
/// terminal size and matters most at the 60x15 minimum.
pub fn rows(area: Rect, tab_bar: bool) -> PanelRows {
    let empty = Rect::new(area.x, area.y, 0, 0);
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return PanelRows {
            tab_bar: empty,
            header: empty,
            entries: empty,
            rule: empty,
            status: empty,
        };
    }

    let mut budget = inner.height;
    let mut claim = |want: bool| -> u16 {
        if want && budget > 0 {
            budget = budget.saturating_sub(1);
            1
        } else {
            0
        }
    };
    let header_h = claim(true);
    let status_h = claim(true);
    let rule_h = claim(true);
    let tab_h = claim(tab_bar);
    let entries_h = budget;

    let row = |y: u16, h: u16| -> Rect {
        if h == 0 {
            Rect::new(inner.x, y, 0, 0)
        } else {
            Rect::new(inner.x, y, inner.width, h)
        }
    };

    let mut y = inner.y;
    let tab_bar_rect = row(y, tab_h);
    y = y.saturating_add(tab_h);
    let header_rect = row(y, header_h);
    y = y.saturating_add(header_h);
    let entries_rect = row(y, entries_h);
    y = y.saturating_add(entries_h);
    let rule_rect = if rule_h == 0 {
        Rect::new(area.x, y, 0, 0)
    } else {
        Rect::new(area.x, y, area.width, 1)
    };
    y = y.saturating_add(rule_h);
    let status_rect = row(y, status_h);

    PanelRows {
        tab_bar: tab_bar_rect,
        header: header_rect,
        entries: entries_rect,
        rule: rule_rect,
        status: status_rect,
    }
}

/// How many entry rows a panel box of this size has. Written into
/// `Panel::view_rows` by [`super::sync_view_rows`].
pub fn entry_row_count(area: Rect, tab_bar: bool) -> usize {
    usize::from(rows(area, tab_bar).entries.height)
}

/// Draw one panel.
pub fn draw(f: &mut Frame, app: &App, side: Side, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let panel = app.panel(side);
    let tab = panel.active_tab();
    let theme = &app.theme;
    let depth = app.color_depth;
    let color = |rgb: Rgb| theme.quantize(rgb, depth);

    let active = app.active_side == side;
    let border_rgb = if active {
        theme.panel.border
    } else {
        theme.panel.inactive_border
    };

    // The top border carries BOTH the path and the volume line.
    // The path wins the space it needs: a panel that cannot tell you which
    // directory it is showing is useless, whereas free space is a nicety.
    let inner = usize::from(area.width).saturating_sub(2);
    let path_text = path_title(panel, g);
    // ` path ` + ` volume ` - four cells of padding, plus a gap between them.
    let spare = inner
        .saturating_sub(text::width(&path_text))
        .saturating_sub(5);
    let volume_text = volume_title(tab, spare, g);
    let both_fit = !volume_text.is_empty();
    let path_room = if both_fit {
        inner
    } else {
        inner.saturating_sub(2)
    };
    let path_shown = text::fit_left(&path_text, path_room, Crop::Middle, g.ellipsis());

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_set(g.border_set())
        .border_style(Style::new().fg(color(border_rgb)))
        .title_top(Line::from(Span::styled(
            format!(" {path_shown} "),
            Style::new().fg(color(theme.panel.header_fg)),
        )));
    if both_fit {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" {volume_text} "),
                Style::new().fg(color(theme.panel.status_fg)),
            ))
            .right_aligned(),
        );
    }
    let block = block.style(
        Style::new()
            .bg(color(theme.panel.bg))
            .fg(color(theme.panel.fg)),
    );
    f.render_widget(block, area);

    let show_tabs = panel.tab_bar_visible(app.config.panel.show_tab_bar);
    let r = rows(area, show_tabs);

    // The git column earns its cell only in a repository: a directory whose
    // read found a git state on at least one entry. Elsewhere it would be a
    // blank column stealing a cell from the name.
    let show_git = tab.entries.iter().any(|e| e.git_state.is_some());
    let allocated = columns::allocate(&app.config.panel, usize::from(r.entries.width), show_git);
    let crop = columns::name_crop(&app.config.panel, &allocated);

    draw_tab_bar(f, app, panel, r.tab_bar, g);
    // while this panel is showing a quick view, three of its
    // rows say something else - the column header becomes the file's name, the
    // entry rows become the file, and the status line becomes the viewer's.
    // Everything above is untouched, "because it is still that panel".
    if app.quick_view_side() == Some(side) {
        draw_quick_header(f, app, r.header, g);
        quickview::draw(f, app, r.entries);
        draw_rule(f, app, r.rule, g, border_rgb);
        draw_quick_status(f, app, r.status, g);
        return;
    }
    draw_header(f, app, tab, &allocated, r.header, g);
    draw_entries(f, app, side, &allocated, crop, r.entries, g);
    draw_rule(f, app, r.rule, g, border_rgb);
    draw_status(f, app, side, &allocated, r.status, g);
}

/// The quick view's header row: the file being viewed.
///
/// Drawn in the column header's own slots, because it is the column header's
/// row and a second colour for the same line would be a theme slot that
/// the design does not have.
fn draw_quick_header(f: &mut Frame, app: &App, area: Rect, g: Glyphs) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let style = Style::new()
        .fg(color(theme.panel.header_fg))
        .bg(color(theme.panel.header_bg));
    let text = text::fit_left(
        &quickview::header(app),
        usize::from(area.width),
        Crop::Middle,
        g.ellipsis(),
    );
    f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

/// The quick view's status row: the viewer's own status, fitted to a panel's
/// width.
fn draw_quick_status(f: &mut Frame, app: &App, area: Rect, g: Glyphs) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let width = usize::from(area.width);
    // `status_fit` already drops fields in reverse priority order rather than
    // cropping, which is the rule the design needs; the fit below is the
    // floor under it for a panel narrower than one field.
    let text = text::fit_left(
        &quickview::status(app, width),
        width,
        Crop::End,
        g.ellipsis(),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::new().fg(color(theme.panel.status_fg)),
        )))
        .style(Style::new().bg(color(theme.panel.status_bg))),
        area,
    );
}

/// `d [dev]  1,062,892,164 k of 3,907,000,316 k free`.
/// The panel's directory and its active file mask, for the top border.
///
/// A mask other than `*` is how a quick filter shows itself, so it
/// is appended rather than given a row of its own.
fn path_title(panel: &Panel, _g: Glyphs) -> String {
    let tab = panel.active_tab();
    // "The panel header shows the virtual state, e.g.
    // `[search: *.rs "TODO" in ~/dev]`, so it is never mistaken for a real
    // directory." It goes here, on the top border, because the design settled
    // that there is no separate path row - and the path it would otherwise
    // show is `/7`, which names nothing a user could act on. The header is
    // already ASCII, so `ui.ascii_borders` changes nothing about it.
    let path = match (tab.virtual_view(), tab.remote_view()) {
        (Some(view), _) => view.header.clone(),
        // "The header shows
        // `sftp://thorin@nas.local:2222/srv/media`". The authority is on the
        // view and the directory is the path's own tail, so the header needs
        // no live connection to draw - which matters, because it has to stay
        // right after the connection has dropped.
        (None, Some(view)) => {
            let dir = tab.path.tail().to_string_lossy();
            let mut header = format!("{}{dir}", view.authority);
            if view.disconnected {
                // the resolution: `Ctrl+R`, never `F2`.
                header.push_str(" (disconnected - Ctrl+R to reconnect)");
            }
            header
        }
        (None, None) => tab.path.to_string(),
    };
    let mask = panel.filter_mask.as_str();
    if mask.is_empty() || mask == "*" {
        path
    } else {
        format!("{path}  {mask}")
    }
}

/// The longest volume rendering that fits in `max` cells, or `""` if none does.
///
/// Never truncated: a half-written byte count is worse than no byte count, the
/// same rule the design applies to columns.
fn volume_title(tab: &Tab, max: usize, _g: Glyphs) -> String {
    // A virtual listing has no volume and no name a volume line could carry:
    // its `list:/7` path renders as `7`, so the degraded line would read
    // `7 [_none_]`, which tells the user nothing at all. The header beside it
    // already says what the panel is showing, and it is the
    // half that keeps the room when the border is cropped.
    if tab.is_virtual() {
        return String::new();
    }
    // A remote has no volume line this program can compute - `statvfs` is not
    // a thing either protocol offers - and the header beside it already says
    // which host and which directory.
    if tab.is_remote() {
        return String::new();
    }
    let Some(v) = tab.path.local_path().and_then(volume::for_path) else {
        // Not a local path - an archive or a virtual listing - or a machine
        // whose mounts could not be enumerated.
        let body = volume::unknown_line(&tab.path.display_title());
        return if text::width(&body) <= max {
            body
        } else {
            String::new()
        };
    };
    volume::lines(&v)
        .into_iter()
        .find(|s| text::width(s) <= max)
        .unwrap_or_default()
}

/// Each tab's directory basename with its index, active one highlighted,
/// truncated with an ellipsis.
///
/// The layout is [`Panel::tab_bar_labels`]'s: it budgets the separators before
/// dividing the bar, and scrolls the window when nine tabs will not fit, so the
/// active tab is always among the labels and therefore always the highlighted
/// one.
fn draw_tab_bar(f: &mut Frame, app: &App, panel: &Panel, area: Rect, g: Glyphs) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let labels = panel.tab_bar_labels(usize::from(area.width), g.is_ascii());

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(labels.len().saturating_mul(2));
    for (slot, label) in labels.iter().enumerate() {
        if slot > 0 {
            spans.push(Span::styled(
                g.vertical().to_string(),
                Style::new().fg(color(theme.panel.inactive_border)),
            ));
        }
        let style = if label.active {
            Style::new()
                .fg(color(theme.panel.header_fg))
                .bg(color(theme.panel.header_bg))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(color(theme.panel.fg))
        };
        spans.push(Span::styled(label.text.clone(), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The column header, with the sort arrow prefixed to the sorted column's name.
///
fn draw_header(
    f: &mut Frame,
    app: &App,
    tab: &Tab,
    allocated: &[Allocated],
    area: Rect,
    g: Glyphs,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let style = Style::new()
        .fg(color(theme.panel.header_fg))
        .bg(color(theme.panel.header_bg));

    let mut body = String::new();
    for (i, col) in allocated.iter().enumerate() {
        if i > 0 {
            body.push(' ');
        }
        let sorted = tab.sort.key == SortKey::Column(col.id);
        let head = columns::header_text(col.id, sorted, tab.sort.reverse, g);
        body.push_str(&columns::fit_cell(&head, *col, Crop::End, g));
    }
    let body = text::fit_left(&body, usize::from(area.width), Crop::End, g.ellipsis());
    f.render_widget(Paragraph::new(Line::from(Span::styled(body, style))), area);
}

/// One line per entry, with the cursor bar in whichever of the three styles
/// the design calls for.
fn draw_entries(
    f: &mut Frame,
    app: &App,
    side: Side,
    allocated: &[Allocated],
    crop: Crop,
    area: Rect,
    g: Glyphs,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let panel = app.panel(side);
    let tab = panel.active_tab();
    let (cursor_bg, cursor_fg) = CursorStyle::of(app, side).colors(theme);
    let ext_rendered = allocated
        .iter()
        .any(|c| c.id == crate::panel::ColumnId::Ext);
    // the Owner and Group columns are resolved against this
    // machine's passwd database, which is the right answer only for a row that
    // came from this machine.
    let local_ids = tab.path.local_path().is_some();
    // the disconnected state: "the panel shows the last listing
    // greyed out". Every row, cursor row included, so the panel reads as a
    // record of what was there rather than as a live listing.
    let disconnected = tab.is_disconnected();
    let row_format = RowFormat {
        cfg: &app.config.panel,
        crop,
        g,
        ext_rendered,
        local_ids,
    };

    for row in 0..usize::from(area.height) {
        let index = tab.scroll.saturating_add(row);
        let Some(entry) = tab.entries.get(index) else {
            break;
        };
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if y >= area.bottom() {
            break;
        }
        let is_cursor = index == tab.cursor;
        let marked = tab.is_marked(entry);
        let fg = filetype::entry_fg(entry, marked, &app.config, theme);

        let style = if is_cursor {
            // The cursor bar's BACKGROUND is ALWAYS `cursor_bg` and never
            // changes; only the foreground does. A marked file under the bar
            // takes a dark accent of the mark colour - the same hue, darkened
            // just enough to read on this theme's bar. A fixed darkening does
            // not work: a mid-tone bar and a half-dark yellow land on the same
            // luminance and vanish, so the darkening is chosen per theme to
            // clear a real contrast bar.
            let fg = if marked {
                dark_mark_accent(theme.panel.marked_fg, cursor_bg)
            } else {
                cursor_fg
            };
            Style::new().bg(color(cursor_bg)).fg(color(fg))
        } else {
            Style::new().bg(color(theme.panel.bg)).fg(color(fg))
        };
        let style = if disconnected {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        };
        // a directory that has been walked shows its byte count
        // in the `size` column instead of `<DIR>`.
        let sized = if entry.is_dir() && !entry.is_parent {
            app.jobs
                .sizes
                .get(&tab.path.join(&entry.name))
                .map(|s| s.bytes)
        } else {
            None
        };
        let body = entry_line(entry, allocated, &row_format, sized);
        let body = text::fit_left(&body, usize::from(area.width), Crop::End, g.ellipsis());
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(body, style))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

/// A dark accent of the mark colour that reads on the cursor bar.
///
/// The mark's own hue, scaled toward black by as little as it takes to clear a
/// contrast bar against `bar`, so a marked file under the cursor keeps the mark
/// colour rather than the bar's plain text. A fixed darkening does not work: a
/// mid-tone bar and a half-dark yellow can share a luminance and vanish, so the
/// amount is chosen per theme. Where the bar is too dark for any dark accent to
/// read, the mark is lightened toward white instead.
pub(crate) fn dark_mark_accent(mark: Rgb, bar: Rgb) -> Rgb {
    const TARGET: f64 = 3.5;
    // From the full mark colour toward black; the lightest step that reads
    // keeps the most of the hue.
    for step in (0..=20).rev() {
        let candidate = scale_rgb(mark, f64::from(step) / 20.0);
        if contrast_ratio(candidate, bar) >= TARGET {
            return candidate;
        }
    }
    for step in 1..=20 {
        let candidate = lighten_rgb(mark, f64::from(step) / 20.0);
        if contrast_ratio(candidate, bar) >= TARGET {
            return candidate;
        }
    }
    mark
}

/// Clamp a computed channel into a byte.
fn channel_byte(value: f64) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255 before the cast, so nothing is lost"
    )]
    {
        value.round().clamp(0.0, 255.0) as u8
    }
}

/// Scale a colour toward black: `f == 1.0` is the colour, `f == 0.0` is black.
fn scale_rgb(c: Rgb, f: f64) -> Rgb {
    let s = |v: u8| channel_byte(f64::from(v) * f);
    Rgb {
        r: s(c.r),
        g: s(c.g),
        b: s(c.b),
    }
}

/// Blend a colour toward white: `f == 0.0` is the colour, `f == 1.0` is white.
fn lighten_rgb(c: Rgb, f: f64) -> Rgb {
    let l = |v: u8| {
        let v = f64::from(v);
        channel_byte(v + (255.0 - v) * f)
    };
    Rgb {
        r: l(c.r),
        g: l(c.g),
        b: l(c.b),
    }
}

/// The relative luminance of a colour, for [`contrast_ratio`].
fn relative_luminance(c: Rgb) -> f64 {
    fn channel(v: u8) -> f64 {
        let s = f64::from(v) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// The WCAG contrast ratio between two colours.
fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The formatted, padded cells of one entry, joined by single spaces.
fn entry_line(
    entry: &Entry,
    allocated: &[Allocated],
    row: &RowFormat<'_>,
    sized: Option<u64>,
) -> String {
    let mut out = String::new();
    for (i, col) in allocated.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // The size cell is the one that is formatted against its own width:
        // an exact count that does not fit is cropped into a wrong number,
        // so it steps down to `1.0 G` instead. Every other cell is formatted
        // once and cropped if it must be.
        let body = if col.id == crate::panel::ColumnId::Size {
            crate::panel::format::size_text_fitting(entry, row.cfg, sized, col.width)
        } else {
            columns::cell_text_with(
                entry,
                col.id,
                row.cfg,
                row.ext_rendered,
                sized,
                row.local_ids,
            )
        };
        let cell_crop = if col.id.is_flexible() {
            row.crop
        } else {
            Crop::End
        };
        out.push_str(&columns::fit_cell(&body, *col, cell_crop, row.g));
    }
    out
}

/// Everything about how a row is rendered that is the same for every row of
/// one frame.
///
/// One struct rather than five arguments, which is what
/// `clippy::too_many_arguments` is asking for and is also the honest shape:
/// these five are decided once per panel and then never vary down the column.
struct RowFormat<'a> {
    /// `[panel]`, for the column formatting rules.
    cfg: &'a crate::config::PanelConfig,
    /// Where a too-long name loses its middle.
    crop: Crop,
    /// ASCII or Unicode box drawing.
    g: Glyphs,
    /// Whether the `ext` column is on screen, so the `name` column knows
    /// whether it carries the extension too.
    ext_rendered: bool,
    /// Whether `uid` and `gid` may be resolved against this machine's passwd
    /// database.
    local_ids: bool,
}

/// The `├────┤` rule between the entries and the panel status line.
///
/// `border_rgb` is the caller's already-resolved border colour, not
/// `theme.panel.border`. The rule's two ends are `├` and `┤` - they sit in the
/// block's own border columns and join up with it, so drawing them in the
/// active colour on an inactive panel leaves two lit corners in an otherwise
/// dimmed frame (`border` and `inactive_border` are separate slots
/// precisely so an unfocused panel recedes).
fn draw_rule(f: &mut Frame, app: &App, area: Rect, g: Glyphs, border_rgb: Rgb) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let mid = usize::from(area.width).saturating_sub(2);
    let mut body = String::with_capacity(usize::from(area.width).saturating_mul(3));
    body.push_str(g.tee_left());
    for _ in 0..mid {
        body.push_str(g.horizontal());
    }
    if area.width >= 2 {
        body.push_str(g.tee_right());
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            body,
            Style::new().fg(color(border_rgb)).bg(color(theme.panel.bg)),
        ))),
        area,
    );
}

/// The panel status line, with the sort indicator at its right end.
///
fn draw_status(
    f: &mut Frame,
    app: &App,
    side: Side,
    allocated: &[Allocated],
    area: Rect,
    g: Glyphs,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = &app.theme;
    let color = |rgb: Rgb| theme.quantize(rgb, app.color_depth);
    let tab = app.panel(side).active_tab();
    // the tag is for the one case it exists for - the sorted column is not
    // drawn, so its header arrow went with it and nothing else would explain
    // the order. When the column *is* drawn the arrow is already on it, and
    // the status line's whole width goes to the counts.
    let tag = if sorted_column_is_drawn(tab.sort.key, allocated) {
        String::new()
    } else {
        tab.sort.indicator(g.is_ascii())
    };
    // what the clipboard holds is named while it is not empty,
    // so `Ctrl+V` is never a guess about what is about to land. It goes on the
    // *active* panel only - one clipboard, and naming it twice would suggest
    // two - and after the sort tag, which is diagnosing something the user did
    // not ask about and so has the stronger claim on the space.
    let tag = match (&app.clipboard, side == app.active_side) {
        (Some(held), true) => {
            let held = format!("[{}]", held.describe());
            if tag.is_empty() {
                held
            } else {
                format!("{tag} {held}")
            }
        }
        _ => tag,
    };
    let tag_w = text::width(&tag).min(usize::from(area.width));
    let left_w = usize::from(area.width).saturating_sub(tag_w);

    let left = status_text(app, side);
    // A message is middle-cropped, everything else end-cropped. End-cropping a
    // message throws away its verdict and keeps only its subject - a narrow
    // panel would show `Copy the selection to the other panel: …` and hide the
    // `not implemented until v0.4` that is the entire reason for the line.
    // Counts read from the left, so they keep the end crop.
    let crop = if status_is_message(app, side) {
        Crop::Middle
    } else {
        Crop::End
    };
    let left = text::fit_left(&left, left_w, crop, g.ellipsis());

    let line = Line::from(vec![
        Span::styled(left, Style::new().fg(color(theme.panel.status_fg))),
        Span::styled(
            text::take_front(&tag, tag_w),
            Style::new().fg(color(theme.panel.header_fg)),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::new().bg(color(theme.panel.status_bg))),
        area,
    );
}

/// The left-hand text of the panel status line, before the sort tag.
///
/// Two overrides on the counts, highest priority first:
///
/// 1. a transient message - an error, or a refusal with its reason - on the
///    active panel, since that is where the eye is;
/// 2. an active quick search, showing the buffer and its case mode.
///
/// It never shows the name of the entry under the cursor. It used to, whenever
/// the name cell had cropped it, and that was a bad trade: a long name is
/// exactly the case where this line is worth the most, and it replaced the
/// counts with a name the row above was already showing most of. A long enough
/// name filled the line end to end and the panel reported nothing about itself
/// at all.
pub fn status_text(app: &App, side: Side) -> String {
    if app.active_side == side
        && let Some(message) = app.message.as_deref()
    {
        return message.to_string();
    }
    let panel = app.panel(side);
    // One implementation of the search label, in `input::quicksearch`, so the
    // status line and the `no match:` message cannot disagree about what the
    // case marker means - they did: `[Aa]` was rendered as *insensitive* here
    // and as *sensitive* there. the only example is
    // `search: Tho [Aa]`, and `Tho` under the default smart mode is
    // case-sensitive, so `[Aa]` is the sensitive marker.
    if let Some(label) = crate::input::panel_status(panel, app.config.panel.quick_search_case) {
        return label;
    }
    // the live count, above the cropped name: a search that is
    // still filling is a statement about what the panel is *doing*, and a
    // cropped filename is a statement about one row.
    if let Some(text) = virtual_status(app, side) {
        return text;
    }
    // **Entries** stream in and are drawn as they arrive; the
    // **counts** wait for the listing to finish.
    //
    // A total is a statement about a whole directory, so a partial one is not a
    // smaller true answer - it is a wrong answer, and it is wrong differently
    // in each frame. Navigating showed `0 k in 0 files, 0 dirs`, then a count
    // of whatever had arrived, then the real one: three claims in as many
    // frames, two of them false. One honest `reading…` and then the answer
    // costs nothing and says what is actually happening.
    let tab = panel.active_tab();
    if tab.loading {
        return if app.config.ui.ascii_borders {
            "reading...".to_string()
        } else {
            "reading\u{2026}".to_string()
        };
    }
    counts_text(
        tab,
        &app.config.panel,
        app.config.ui.ascii_borders,
        &app.jobs.sizes,
    )
}

/// The status line of a panel showing a virtual listing.
///
/// > results stream back over a channel, with a live count
///
/// `None` once the walk has finished cleanly, at which point the ordinary
/// counts are the honest answer and the rule that a total waits
/// for the listing to finish applies again. The count itself is an atomic
/// read, so asking on every frame costs nothing.
fn virtual_status(app: &App, side: Side) -> Option<String> {
    let panel = app.panel(side);
    let tab = panel.active_tab();
    let kind = tab.virtual_view()?.kind.id();
    let listing = app.listing(side, panel.active_index())?;
    let found = listing.len();
    let ascii = app.config.ui.ascii_borders;
    match listing.status() {
        // Nothing yet is not "nothing found": the walk has only started, and
        // `0 found` would be a claim it is not in a position to make.
        ListStatus::Filling if found == 0 => Some(if ascii {
            format!("{kind}: searching...")
        } else {
            format!("{kind}: searching\u{2026}")
        }),
        ListStatus::Filling => Some(format!("{kind}: {found} found")),
        // `Esc` "keeps what was found", so the line says how
        // many were kept and that the walk was stopped rather than finished.
        ListStatus::Cancelled if tab.loading => Some(format!("{kind}: {found} found, stopped")),
        ListStatus::Cancelled => Some(format!(
            "{}  stopped",
            counts_text(tab, &app.config.panel, ascii, &app.jobs.sizes)
        )),
        ListStatus::Complete | ListStatus::Failed(_) => None,
    }
}

/// Is the sorted column among the ones this panel is currently rendering?
///
/// `Unsorted` is never "drawn" - no column carries an arrow for it - so
/// `[unsorted]` always shows, which is the only way to see that state at all.
fn sorted_column_is_drawn(key: crate::panel::SortKey, allocated: &[Allocated]) -> bool {
    match key {
        crate::panel::SortKey::Unsorted => false,
        crate::panel::SortKey::Column(id) => allocated.iter().any(|a| a.id == id),
    }
}

/// Is the status line currently showing [`App::message`] rather than counts?
///
/// The crop rule differs, so the two callers have to agree on which it is.
pub fn status_is_message(app: &App, side: Side) -> bool {
    app.active_side == side && app.message.is_some()
}
/// The panel status line's counts, forwarded to the model.
///
/// `sizes` is the session's directory-size cache: it is what decides whether
/// the selection's size renders as a number or as a `≥` lower bound.
pub fn counts_text(
    tab: &Tab,
    cfg: &crate::config::PanelConfig,
    ascii: bool,
    sizes: &crate::ops::SizeCache,
) -> String {
    crate::panel::format::status_text(tab, cfg, ascii, sizes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_listing_still_arriving_says_so_rather_than_counting_to_zero() {
        // A total is a statement about a whole directory, so a partial one is
        // not a smaller true answer - it is a wrong answer, and wrong
        // differently in each frame. Entries still stream; only
        // the counts wait.
        let mut app = App::headless(
            crate::config::Config::default(),
            crate::config::Keymap::builtin(),
            crate::config::Theme::blue(),
        );
        app.navigate(Side::Left, crate::vfs::VfsPath::local("/somewhere"));
        assert!(app.left.active_tab().loading);
        assert_eq!(status_text(&app, Side::Left), "reading\u{2026}");

        // ...and the ASCII fallback, like every other glyph.
        app.config.ui.ascii_borders = true;
        assert_eq!(status_text(&app, Side::Left), "reading...");

        // Once the read finishes the real counts appear.
        app.config.ui.ascii_borders = false;
        app.left.active_tab_mut().loading = false;
        assert!(status_text(&app, Side::Left).contains("in "));
    }

    use super::*;
    use crate::config::{Config, Keymap, Theme};
    use crate::vfs::Entry;
    use crate::vfs::list::ListSink;

    fn app() -> App {
        App::headless(Config::default(), Keymap::builtin(), Theme::blue())
    }

    /// Point the left panel at a virtual listing through the code `Alt+F7`
    /// uses, and hand back its producer end.
    fn showing(app: &mut App, kind: crate::panel::VirtualKind, header: &str) -> ListSink {
        let origin = app.left.active_tab().path.clone();
        let (listing, sink) = crate::vfs::ListFs::streaming(header, std::slice::from_ref(&origin));
        assert!(app.show_listing(
            Side::Left,
            0,
            listing,
            crate::app::PendingView {
                kind,
                header: header.to_string(),
                find: None,
                origin,
                origin_cursor: None,
                previous: None,
            }
        ));
        sink
    }

    #[test]
    fn the_top_border_says_which_listing_it_is_rather_than_list_slash_7() {
        // "The panel header shows the virtual state, e.g.
        // `[search: *.rs \"TODO\" in ~/dev]`, so it is never mistaken for a
        // real directory."
        let mut a = app();
        a.left.active_tab_mut().path = crate::vfs::VfsPath::local("/root");
        assert_eq!(path_title(&a.left, Glyphs::new(false)), "/root");

        let _sink = showing(
            &mut a,
            crate::panel::VirtualKind::Search,
            "[search: *.rs \"TODO\" in /root]",
        );
        assert_eq!(
            path_title(&a.left, Glyphs::new(false)),
            "[search: *.rs \"TODO\" in /root]"
        );
        // It is ASCII already, so the border glyph setting changes nothing.
        assert_eq!(
            path_title(&a.left, Glyphs::new(true)),
            "[search: *.rs \"TODO\" in /root]"
        );
    }

    #[test]
    fn a_virtual_listing_has_no_volume_line() {
        // A `list:/7` path is on no mount and its `display_title` is `7`, so
        // the degraded volume line would read `7 [_none_]` - a byte count that
        // is not a byte count, about a volume that is not a volume. The header
        // beside it already says what the panel is showing.
        let mut a = app();
        a.left.active_tab_mut().path = crate::vfs::VfsPath::local("/root");
        let before = volume_title(a.left.active_tab(), 40, Glyphs::new(false));
        assert!(!before.is_empty(), "a real directory still has one");

        let _sink = showing(
            &mut a,
            crate::panel::VirtualKind::Search,
            "[search: * in /root]",
        );
        assert_eq!(
            volume_title(a.left.active_tab(), 40, Glyphs::new(false)),
            ""
        );
    }

    #[test]
    fn the_status_line_counts_up_while_the_search_fills() {
        // "results stream back over a channel, with a live
        // count". Nothing found yet is not `0 found`: the walk has only
        // started, and that is a claim it is not in a position to make.
        let mut a = app();
        a.left.active_tab_mut().path = crate::vfs::VfsPath::local("/root");
        let sink = showing(
            &mut a,
            crate::panel::VirtualKind::Search,
            "[search: * in /root]",
        );
        assert_eq!(status_text(&a, Side::Left), "search: searching\u{2026}");
        a.config.ui.ascii_borders = true;
        assert_eq!(status_text(&a, Side::Left), "search: searching...");
        a.config.ui.ascii_borders = false;

        for i in 0..7u32 {
            assert!(sink.push(Entry::file(format!("f{i}"))));
        }
        assert_eq!(status_text(&a, Side::Left), "search: 7 found");
        // Even while a row's name is cropped: a search that is still filling is
        // a statement about the panel, and a cropped name about one row.
        a.left.active_tab_mut().entries = vec![Entry::file("a-very-long-name.txt")];
        assert_eq!(status_text(&a, Side::Left), "search: 7 found");
    }

    #[test]
    fn a_branch_view_says_branch_and_a_stopped_walk_says_stopped() {
        let mut a = app();
        a.left.active_tab_mut().path = crate::vfs::VfsPath::local("/root");
        let sink = showing(&mut a, crate::panel::VirtualKind::Branch, "[branch: /root]");
        assert!(sink.push(Entry::file("one.rs")));
        assert_eq!(status_text(&a, Side::Left), "branch: 1 found");

        // the `Esc`: what was found is kept, and the line says the
        // walk was stopped rather than finished.
        sink.cancel();
        assert_eq!(status_text(&a, Side::Left), "branch: 1 found, stopped");
        // Once the read has drained, the ordinary counts come back with the
        // same note beside them.
        a.left.active_tab_mut().entries = vec![Entry::file("one.rs")];
        a.left.active_tab_mut().loading = false;
        let line = status_text(&a, Side::Left);
        assert!(line.contains("in 1 file"), "{line}");
        assert!(line.ends_with("stopped"), "{line}");
    }

    #[test]
    fn a_finished_search_reports_the_ordinary_counts() {
        // Nothing special once the walk is over: the counts are
        // the honest answer, and they are the same counts a directory gets.
        let mut a = app();
        a.left.active_tab_mut().path = crate::vfs::VfsPath::local("/root");
        let sink = showing(
            &mut a,
            crate::panel::VirtualKind::Search,
            "[search: * in /root]",
        );
        sink.finish(crate::vfs::list::ListStatus::Complete);
        a.left.active_tab_mut().entries = vec![Entry::file("one.rs")];
        a.left.active_tab_mut().loading = false;
        assert!(status_text(&a, Side::Left).contains("in 1 file"));
    }

    #[test]
    fn the_rows_of_the_layout_diagram_all_fit_at_the_minimum_size() {
        // 60x15 leaves the body 13 rows; one panel gets all of them.
        let r = rows(Rect::new(0, 0, 30, 13), false);
        assert_eq!(r.header.height, 1);
        assert_eq!(r.rule.height, 1);
        assert_eq!(r.status.height, 1);
        assert!(r.entries.height >= 1, "at least one entry row");
        // One row more than before: the path moved into the top border.
        assert_eq!(r.entries.height, 8);
        assert_eq!(r.tab_bar.height, 0, "one tab hides the bar");
        assert_eq!(r.rule.width, 30, "the rule spans the border columns");
    }

    #[test]
    fn a_degenerate_box_produces_only_zero_sized_rects() {
        for (w, h) in [(0u16, 0u16), (1, 1), (2, 2), (3, 3), (5, 4)] {
            let r = rows(Rect::new(0, 0, w, h), true);
            for rect in [r.tab_bar, r.header, r.entries, r.rule, r.status] {
                assert!(rect.width <= w && rect.height <= h, "{w}x{h}");
            }
        }
    }

    #[test]
    fn counts_read_like_the_reference_panel() {
        let mut tab = Tab::new(crate::vfs::VfsPath::local_root());
        tab.entries.push(Entry::parent_entry());
        tab.entries.push(Entry::dir("sub"));
        for i in 0..43 {
            let mut e = Entry::file(format!("f{i}"));
            e.size = 100;
            tab.entries.push(e);
        }
        let cfg = crate::config::Config::default().panel;
        let got = counts_text(&tab, &cfg, false, &crate::ops::SizeCache::new());
        assert_eq!(got, "5 k in 43 files, 1 dir", "the first form");
    }

    #[test]
    fn a_message_outranks_a_quick_search_which_outranks_the_counts() {
        let mut a = app();
        let side = a.active_side;
        a.panel_mut(side).active_tab_mut().entries = vec![Entry::file("a-very-long-name.txt")];
        // The counts, and only the counts. A long name in the listing does not
        // take the line over: the row above already shows the name, and the
        // line is the only place the panel says anything about itself.
        assert!(status_text(&a, side).contains("in 1 file"));
        assert!(
            !status_text(&a, side).contains("a-very-long-name.txt"),
            "the status line is not a place to repeat a filename: {}",
            status_text(&a, side)
        );
        a.panel_mut(side).quick.buffer = "Tho".to_string();
        assert_eq!(status_text(&a, side), "search: Tho [Aa]");
        a.message = Some("View file: not implemented until v0.4".to_string());
        assert_eq!(
            status_text(&a, side),
            "View file: not implemented until v0.4"
        );
    }

    #[test]
    fn a_message_does_not_hijack_the_other_panel() {
        let mut a = app();
        a.message = Some("boom".to_string());
        let other = a.active_side.other();
        assert!(!status_text(&a, other).contains("boom"));
    }

    #[test]
    fn the_three_cursor_styles_are_distinct() {
        let mut a = app();
        let t = Theme::blue();
        let left = CursorStyle::of(&a, Side::Left);
        let right = CursorStyle::of(&a, Side::Right);
        assert_eq!(left, CursorStyle::Focused);
        assert_eq!(right, CursorStyle::Inactive);
        a.set_focus(Focus::CommandLine);
        assert_eq!(CursorStyle::of(&a, Side::Left), CursorStyle::Unfocused);
        assert_eq!(CursorStyle::of(&a, Side::Right), CursorStyle::Inactive);

        let styles = [
            CursorStyle::Focused.colors(&t),
            CursorStyle::Unfocused.colors(&t),
            CursorStyle::Inactive.colors(&t),
        ];
        assert_ne!(styles[0].0, styles[1].0);
        assert_ne!(styles[1].0, styles[2].0);
        assert_ne!(styles[0].0, styles[2].0);
    }
}
