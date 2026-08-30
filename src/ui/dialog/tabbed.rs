//! Two dialog primitives v0.6 needs and no dialog in the tree had: a tab strip
//! and a scrolling, sortable table.
//!
//! * [`TabStrip`] is the three tabs - General, Advanced,
//!   Load/Save.
//! * [`Table`] is the preview: `Old name · Ext · New name · Size ·
//!   Date · Location`, sortable, with a status column after them.
//!
//! # The rules both keep
//!
//! * **They lay out against the rectangle they are given**. Every
//!   `Rect` built here is checked for zero width or height first, and a table
//!   too narrow for a column hides that column rather than drawing half of it.
//! * **Column widths follow the rule rather than a second one**:
//!   fixed minimums first, then the flexible columns take what is left, and a
//!   column that cannot reach its minimum is hidden rather than shown
//!   truncated.
//! * **Colour comes from the `dialog.*` slots only**.
//!
//! # Which column is given up first
//!
//! The **widest** fixed column, then the next widest, breaking a tie in favour
//! of the rightmost. the design ranks the panel's columns by a configured
//! `hide_priority`; a dialog has no such configuration, and the rule that loses
//! the fewest columns is the one that gives back the most room per column lost.
//! On the preview at the narrowest width that draws a table at all,
//! that is the `Date` column and nothing else.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dialog::{DialogKey, DialogStyle};
use crate::input::{KeyCode, KeyModifiers};
use crate::ui::text;

use super::ellipsis;

/// One column of space between two columns.
const GAP: usize = 1;

/// How far `PageUp` and `PageDown` move before a dialog says otherwise.
const DEFAULT_PAGE: usize = 10;

/// A tab strip across the top of a dialog (the three tabs).
///
/// One control in the dialog's [`crate::dialog::FocusRing`]. With focus,
/// `Left`/`Right` move between tabs; `Alt+1`…`Alt+9` select one from anywhere
/// in the dialog, which is the accelerator the design wants for a control a
/// legacy terminal can still reach - `Alt+<n>` is a plain `ESC`-prefixed digit
/// and `Ctrl+Tab` is not. `Tab` is deliberately **not** taken: it belongs to
/// the focus ring in every dialog in the tree, and taking it here would be a
/// second rule.
#[derive(Debug, Clone)]
pub struct TabStrip {
    titles: Vec<String>,
    active: usize,
}

impl TabStrip {
    /// A strip over `titles`, on the first.
    pub fn new(titles: &[&str]) -> Self {
        Self {
            titles: titles.iter().map(|t| (*t).to_string()).collect(),
            active: 0,
        }
    }

    /// Which tab is showing.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Show a tab, clamping. An empty strip stays on zero.
    pub fn set_active(&mut self, index: usize) {
        if self.titles.is_empty() {
            self.active = 0;
        } else {
            self.active = index.min(self.titles.len().saturating_sub(1));
        }
    }

    /// The tab titles, for a dialog that labels its own body.
    pub fn titles(&self) -> &[String] {
        &self.titles
    }

    /// `true` when the key selected a tab.
    pub fn handle(&mut self, key: &DialogKey, focused: bool) -> bool {
        // `Alt+<n>` works from anywhere in the dialog; the arrows only with
        // focus, because they belong to whatever control has it.
        if key.press.mods.contains(KeyModifiers::ALT)
            && let KeyCode::Char(c) = key.press.code
            && let Some(digit) = c.to_digit(10)
            && digit >= 1
        {
            let index = usize::try_from(digit).unwrap_or(1).saturating_sub(1);
            if index < self.titles.len() {
                self.active = index;
                return true;
            }
            return false;
        }
        if !focused || self.titles.is_empty() {
            return false;
        }
        match key.press.code {
            KeyCode::Left => {
                self.active = self
                    .active
                    .saturating_add(self.titles.len())
                    .saturating_sub(1)
                    .rem_euclid(self.titles.len());
                true
            }
            KeyCode::Right => {
                self.active = self.active.saturating_add(1).rem_euclid(self.titles.len());
                true
            }
            _ => false,
        }
    }

    /// Draw it into one row.
    pub fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let width = usize::from(area.width);
        for (index, title) in self.titles.iter().enumerate() {
            let body = format!(" {title} ");
            let w = text::width(&body);
            if used.saturating_add(w) > width {
                break;
            }
            used = used.saturating_add(w);
            let on = index == self.active;
            let mut span_style = if on {
                Style::new()
                    .fg(style.bg)
                    .bg(style.title)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(style.fg).bg(style.bg)
            };
            if on && focused {
                span_style = span_style.add_modifier(Modifier::UNDERLINED);
            }
            spans.push(Span::styled(body, span_style));
        }
        if used < width {
            spans.push(Span::styled(
                " ".repeat(width.saturating_sub(used)),
                style.body(),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(style.body()), area);
    }

    /// One row.
    pub const fn height() -> u16 {
        1
    }
}

/// One column of a [`Table`].
#[derive(Debug, Clone, Copy)]
pub struct TableColumn {
    /// The heading, and what a sort marker is attached to.
    pub header: &'static str,
    /// The fewest cells worth drawing this column in. Below it the column is
    /// hidden.
    pub min: u16,
    /// How a flexible column shares the leftover. Ignored when `flexible` is
    /// false.
    pub weight: u16,
    /// True for a column that grows into whatever the fixed ones leave.
    pub flexible: bool,
    /// True for a column whose cells are flush right - a size.
    pub right: bool,
    /// True for a column cropped from the **left**, keeping its tail.
    ///
    /// A sixth field beyond the five the design lists, and it is there
    /// because of the same document requires it: the preview's
    /// `Location` column is "flexible, cropped from the **left** by
    /// `ui::dialog::crop_left`", for the reason - the tail of a path is
    /// what identifies it. `cell` is handed no width, so the crop cannot
    /// happen at the call site.
    pub crop_left: bool,
}

/// A scrolling, sortable table inside a dialog (the preview).
///
/// The table holds no rows: it holds how many there are, where the cursor is
/// and how it is sorted, and asks the caller for a cell when it draws one. A
/// ten-thousand-row preview therefore costs a screenful, which is what makes
/// the "updates on every keystroke" affordable.
///
/// # There is no scroll offset
///
/// [`Table::window`] is a **pure function of the cursor and the height**: the
/// table shows the page the cursor is on, and moving within that page does not
/// move the view. The queue view does keep a scroll offset, because moving its
/// cursor back up must not move the window; it writes that offset down in
/// [`crate::dialog::Dialog::layout`] rather than behind a `&self` render. This
/// one needs neither, and a type with no hidden interior mutability is a type
/// whose behaviour a test can pin down from its inputs alone.
#[derive(Debug, Clone)]
pub struct Table {
    columns: &'static [TableColumn],
    rows: usize,
    cursor: usize,
    /// How far `PageUp` and `PageDown` move. The dialog sets it from its own
    /// layout, because `handle` is not given a rectangle.
    page: usize,
    sort: (usize, bool),
}

impl Table {
    /// A table over `columns`, empty, sorted by the first column ascending.
    pub fn new(columns: &'static [TableColumn]) -> Self {
        Self {
            columns,
            rows: 0,
            cursor: 0,
            page: DEFAULT_PAGE,
            sort: (0, false),
        }
    }

    /// Which row the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// How many rows there are.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// How far `PageUp` and `PageDown` move.
    ///
    /// The dialog sets it from the height it gave the table last frame; zero
    /// is read as one, so a dialog that never calls this still pages.
    pub fn set_page(&mut self, rows: usize) {
        self.page = rows.max(1);
    }

    /// The rows visible in a body `height` rows tall.
    ///
    /// The page the cursor is on, which is what makes the view a pure function
    /// of the cursor: moving within a page does not scroll, and crossing its
    /// edge turns to the next one.
    pub fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.rows == 0 {
            return 0..0;
        }
        let start = self.cursor.saturating_div(height).saturating_mul(height);
        let start = start.min(self.rows.saturating_sub(1));
        start..start.saturating_add(height).min(self.rows)
    }

    /// `(column index, reversed)`.
    pub fn sort(&self) -> (usize, bool) {
        self.sort
    }

    /// Sort by a column, reversing when it is already the sort column - the
    /// same rule the panel's column headers follow.
    pub fn set_sort(&mut self, column: usize) {
        let (current, reverse) = self.sort;
        self.sort = if current == column {
            (column, !reverse)
        } else {
            (column, false)
        };
    }

    /// How many rows there are now, clamping the cursor.
    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows;
        self.cursor = if rows == 0 {
            0
        } else {
            self.cursor.min(rows.saturating_sub(1))
        };
    }

    /// `true` when the key moved the cursor, scrolled, or changed the sort.
    pub fn handle(&mut self, key: &DialogKey, focused: bool) -> bool {
        // `Ctrl+<n>` sorts by column *n*, from anywhere in the dialog, which is
        // the accelerator the design already gives the panel's columns.
        if key.press.mods.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.press.code
            && let Some(digit) = c.to_digit(10)
            && digit >= 1
        {
            let index = usize::try_from(digit).unwrap_or(1).saturating_sub(1);
            if index < self.columns.len() {
                self.set_sort(index);
                return true;
            }
            return false;
        }
        if !focused || self.rows == 0 {
            return false;
        }
        let last = self.rows.saturating_sub(1);
        let page = self.page.max(1);
        let moved = match key.press.code {
            KeyCode::Up => self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor.saturating_add(1).min(last),
            KeyCode::PageUp => self.cursor.saturating_sub(page),
            KeyCode::PageDown => self.cursor.saturating_add(page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return false,
        };
        self.cursor = moved;
        true
    }

    /// The rows this height can show, for [`Table::set_rows`] and paging.
    ///
    /// One row of the area is the header, always.
    pub const fn body_rows(area: Rect) -> usize {
        (area.height as usize).saturating_sub(1)
    }

    /// Which columns fit in `width`, and how wide each one is.
    ///
    /// The returned pairs are `(index into columns, cells)`, in column order.
    /// A column not in the list is hidden.
    pub fn layout(&self, width: usize) -> Vec<(usize, usize)> {
        let mut visible: Vec<usize> = (0..self.columns.len()).collect();
        loop {
            let needed = self.needed(&visible);
            if needed <= width || visible.is_empty() {
                break;
            }
            // The widest fixed column first: it gives back the most room, so
            // the fewest columns are lost. Ties go to the rightmost.
            let victim = visible
                .iter()
                .copied()
                .filter(|i| self.columns.get(*i).is_some_and(|c| !c.flexible))
                .max_by_key(|i| self.columns.get(*i).map_or(0, |c| c.min))
                .or_else(|| visible.iter().copied().next_back());
            match victim {
                Some(index) => visible.retain(|i| *i != index),
                None => break,
            }
        }
        if visible.is_empty() {
            return Vec::new();
        }

        let gaps = visible.len().saturating_sub(1).saturating_mul(GAP);
        let fixed: usize = visible
            .iter()
            .filter_map(|i| self.columns.get(*i))
            .filter(|c| !c.flexible)
            .map(|c| usize::from(c.min))
            .sum();
        let flexible: Vec<usize> = visible
            .iter()
            .copied()
            .filter(|i| self.columns.get(*i).is_some_and(|c| c.flexible))
            .collect();
        let flexible_min: usize = flexible
            .iter()
            .filter_map(|i| self.columns.get(*i))
            .map(|c| usize::from(c.min))
            .sum();
        let spare = width
            .saturating_sub(gaps)
            .saturating_sub(fixed)
            .saturating_sub(flexible_min);
        let total_weight: usize = flexible
            .iter()
            .filter_map(|i| self.columns.get(*i))
            .map(|c| usize::from(c.weight).max(1))
            .sum();

        let mut handed = 0usize;
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(visible.len());
        for index in &visible {
            let Some(column) = self.columns.get(*index) else {
                continue;
            };
            if !column.flexible {
                out.push((*index, usize::from(column.min)));
                continue;
            }
            let is_last = flexible.last() == Some(index);
            let share = if is_last {
                // The last flexible column takes the rounding, so the row is
                // exactly `width` cells wide rather than one short.
                spare.saturating_sub(handed)
            } else {
                let weight = usize::from(column.weight).max(1);
                let share = spare.saturating_mul(weight) / total_weight.max(1);
                handed = handed.saturating_add(share);
                share
            };
            out.push((*index, usize::from(column.min).saturating_add(share)));
        }
        out
    }

    /// How many cells `visible` would need at their minimums.
    fn needed(&self, visible: &[usize]) -> usize {
        let gaps = visible.len().saturating_sub(1).saturating_mul(GAP);
        visible
            .iter()
            .filter_map(|i| self.columns.get(*i))
            .map(|c| usize::from(c.min))
            .sum::<usize>()
            .saturating_add(gaps)
    }

    /// Draw the table.
    ///
    /// `cell(row, column)` is called only for the rows and columns actually
    /// drawn, and `column` is the index into the `columns` slice this table was
    /// built with - not the index among the visible ones - so a caller can
    /// match on its own column enum without tracking what is hidden.
    pub fn render(
        &self,
        f: &mut Frame,
        area: Rect,
        style: &DialogStyle,
        focused: bool,
        cell: &dyn Fn(usize, usize) -> (String, Option<Style>),
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let widths = self.layout(usize::from(area.width));
        if widths.is_empty() {
            return;
        }

        // The header, with the sort marker on the sorted column.
        if let Some(rect) = super::row(area, 0) {
            let header = Style::new()
                .fg(style.title)
                .bg(style.bg)
                .add_modifier(Modifier::BOLD);
            let marker = if style.ascii {
                if self.sort.1 { "^" } else { "v" }
            } else if self.sort.1 {
                "\u{25b2}"
            } else {
                "\u{25bc}"
            };
            let spans = self.row_spans(&widths, style, |index, width| {
                let column = self.columns.get(index);
                let mut label = column.map_or(String::new(), |c| c.header.to_string());
                if index == self.sort.0 {
                    label.push_str(marker);
                }
                (fit(&label, width, column, style.ascii), Some(header))
            });
            f.render_widget(Paragraph::new(Line::from(spans)).style(style.body()), rect);
        }

        let window = self.window(Table::body_rows(area));
        for (offset, row_index) in window.enumerate() {
            let Some(rect) = super::row(
                area,
                u16::try_from(offset.saturating_add(1)).unwrap_or(u16::MAX),
            ) else {
                break;
            };
            let selected = row_index == self.cursor;
            let base = if selected && focused {
                // The panel's cursor bar, which is what a selected row is
                // everywhere else in the program. It used to fill with the
                // dialog's red, which read as an error on the row.
                style.row_cursor(true).add_modifier(Modifier::BOLD)
            } else if selected {
                style.body().add_modifier(Modifier::REVERSED)
            } else {
                style.body()
            };
            let ascii = style.ascii;
            let spans = self.row_spans(&widths, style, |index, width| {
                let (body, cell_style) = cell(row_index, index);
                // A cell's own colour is honoured only on a row that is not
                // the cursor: the cursor bar has to stay one colour to read as
                // one row.
                let paint = if selected {
                    Some(base)
                } else {
                    cell_style.or(Some(base))
                };
                (fit(&body, width, self.columns.get(index), ascii), paint)
            });
            f.render_widget(Paragraph::new(Line::from(spans)).style(base), rect);
        }
    }

    /// One row's spans, gaps included.
    fn row_spans(
        &self,
        widths: &[(usize, usize)],
        style: &DialogStyle,
        mut cell: impl FnMut(usize, usize) -> (String, Option<Style>),
    ) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(widths.len().saturating_mul(2));
        for (position, (index, width)) in widths.iter().enumerate() {
            if position > 0 {
                spans.push(Span::styled(" ".repeat(GAP), style.body()));
            }
            let (body, cell_style) = cell(*index, *width);
            spans.push(Span::styled(
                body,
                cell_style.unwrap_or_else(|| style.body()),
            ));
        }
        spans
    }
}

/// Pad or crop `body` to exactly `width` cells.
fn fit(body: &str, width: usize, column: Option<&TableColumn>, ascii: bool) -> String {
    let marker = ellipsis(ascii);
    if column.is_some_and(|c| c.crop_left) {
        let cropped = super::crop_left(body, width, ascii);
        return text::fit_left(&cropped, width, text::Crop::End, marker);
    }
    if column.is_some_and(|c| c.right) {
        text::fit_right(body, width, text::Crop::End, marker)
    } else {
        text::fit_left(body, width, text::Crop::End, marker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const COLUMNS: &[TableColumn] = &[
        TableColumn {
            header: "Name",
            min: 10,
            weight: 3,
            flexible: true,
            right: false,
            crop_left: false,
        },
        TableColumn {
            header: "Ext",
            min: 6,
            weight: 0,
            flexible: false,
            right: false,
            crop_left: false,
        },
        TableColumn {
            header: "Size",
            min: 10,
            weight: 0,
            flexible: false,
            right: true,
            crop_left: false,
        },
        TableColumn {
            header: "Date",
            min: 16,
            weight: 0,
            flexible: false,
            right: false,
            crop_left: false,
        },
    ];

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> DialogKey {
        DialogKey::raw(KeyPress::new(code, mods))
    }

    fn style() -> DialogStyle {
        DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false)
    }

    #[test]
    fn a_tab_strip_moves_on_the_arrows_and_on_alt_digits() {
        let mut strip = TabStrip::new(&["General", "Advanced", "Load/Save"]);
        assert_eq!(strip.active(), 0);
        assert!(strip.handle(&key(KeyCode::Right), true));
        assert_eq!(strip.active(), 1);
        assert!(strip.handle(&key(KeyCode::Left), true));
        assert_eq!(strip.active(), 0);
        assert!(strip.handle(&key(KeyCode::Left), true));
        assert_eq!(strip.active(), 2, "the arrows wrap");

        // `Alt+<n>` works without focus; the arrows do not.
        assert!(strip.handle(&with_mods(KeyCode::Char('1'), KeyModifiers::ALT), false));
        assert_eq!(strip.active(), 0);
        assert!(!strip.handle(&key(KeyCode::Right), false));
        assert_eq!(strip.active(), 0);
        // A digit past the end changes nothing rather than clamping silently.
        assert!(!strip.handle(&with_mods(KeyCode::Char('9'), KeyModifiers::ALT), true));
        assert_eq!(strip.active(), 0);

        strip.set_active(99);
        assert_eq!(strip.active(), 2, "set_active clamps");
    }

    #[test]
    fn an_empty_tab_strip_is_inert_rather_than_a_panic() {
        let mut strip = TabStrip::new(&[]);
        assert!(!strip.handle(&key(KeyCode::Right), true));
        strip.set_active(3);
        assert_eq!(strip.active(), 0);
        assert!(strip.titles().is_empty());
    }

    #[test]
    fn the_columns_add_up_to_exactly_the_width_they_are_given() {
        let table = Table::new(COLUMNS);
        for width in 0usize..200 {
            let widths = table.layout(width);
            if widths.is_empty() {
                continue;
            }
            let gaps = widths.len().saturating_sub(1);
            let total: usize = widths.iter().map(|(_, w)| *w).sum::<usize>() + gaps;
            assert_eq!(total, width, "{width} columns gave {widths:?}");
        }
    }

    #[test]
    fn a_column_that_cannot_reach_its_minimum_is_hidden_and_the_widest_goes_first() {
        let table = Table::new(COLUMNS);
        // Everything: 10 + 6 + 10 + 16 + 3 gaps = 45.
        let all = table.layout(45);
        assert_eq!(all.len(), 4);
        // One cell short, and the widest fixed column - Date - is what goes.
        let narrower = table.layout(44);
        assert_eq!(
            narrower.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "Date is the widest fixed column and buys back the most room"
        );
        // Narrower still and the fixed columns keep going, widest first.
        assert_eq!(
            table.layout(20).iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            table.layout(10).iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0]
        );
        // And a width that fits nothing draws nothing rather than a fragment.
        assert!(table.layout(0).is_empty());
    }

    #[test]
    fn the_flexible_column_takes_what_is_left() {
        let table = Table::new(COLUMNS);
        let widths = table.layout(60);
        let name = widths.iter().find(|(i, _)| *i == 0).map(|(_, w)| *w);
        assert_eq!(name, Some(60 - 6 - 10 - 16 - 3));
    }

    #[test]
    fn the_cursor_moves_pages_and_stays_on_screen() {
        let mut table = Table::new(COLUMNS);
        table.set_rows(100);
        assert!(table.handle(&key(KeyCode::Down), true));
        assert_eq!(table.cursor(), 1);
        assert!(table.handle(&key(KeyCode::End), true));
        assert_eq!(table.cursor(), 99);
        assert!(table.handle(&key(KeyCode::Home), true));
        assert_eq!(table.cursor(), 0);
        assert!(
            table.handle(&key(KeyCode::Up), true),
            "and it does not go negative"
        );
        assert_eq!(table.cursor(), 0);

        // Without focus the arrows are somebody else's.
        assert!(!table.handle(&key(KeyCode::Down), false));

        // Fewer rows than the cursor clamps rather than pointing past the end.
        table.set_rows(3);
        assert!(table.handle(&key(KeyCode::End), true));
        assert_eq!(table.cursor(), 2);
        table.set_rows(1);
        assert_eq!(table.cursor(), 0);
        table.set_rows(0);
        assert_eq!(table.cursor(), 0);
        assert!(
            !table.handle(&key(KeyCode::Down), true),
            "no rows, no cursor"
        );
    }

    #[test]
    fn the_visible_window_is_the_page_the_cursor_is_on() {
        // No stored scroll offset: the view is a function of the cursor and
        // the height, so the same cursor always shows the same rows.
        let mut table = Table::new(COLUMNS);
        table.set_rows(25);
        table.set_page(10);
        assert_eq!(table.window(10), 0..10);
        assert!(table.handle(&key(KeyCode::PageDown), true));
        assert_eq!(table.cursor(), 10);
        assert_eq!(table.window(10), 10..20);
        assert!(table.handle(&key(KeyCode::End), true));
        assert_eq!(table.cursor(), 24);
        assert_eq!(table.window(10), 20..25, "the last page is short");
        assert!(table.handle(&key(KeyCode::PageUp), true));
        assert_eq!(table.cursor(), 14);
        assert_eq!(table.window(10), 10..20);

        // Degenerate heights and an empty table answer with an empty range
        // rather than a panic.
        assert_eq!(table.window(0), 0..0);
        table.set_rows(0);
        assert_eq!(table.window(10), 0..0);
        table.set_rows(3);
        assert_eq!(table.window(10), 0..3);
        assert_eq!(table.rows(), 3);
    }

    #[test]
    fn ctrl_digit_sorts_and_the_same_column_again_reverses() {
        let mut table = Table::new(COLUMNS);
        assert_eq!(table.sort(), (0, false));
        assert!(table.handle(&with_mods(KeyCode::Char('3'), KeyModifiers::CONTROL), false));
        assert_eq!(table.sort(), (2, false));
        assert!(table.handle(&with_mods(KeyCode::Char('3'), KeyModifiers::CONTROL), false));
        assert_eq!(table.sort(), (2, true), "the same column reverses");
        assert!(table.handle(&with_mods(KeyCode::Char('1'), KeyModifiers::CONTROL), false));
        assert_eq!(table.sort(), (0, false));
        // A digit past the last column is not this table's key.
        assert!(!table.handle(&with_mods(KeyCode::Char('9'), KeyModifiers::CONTROL), false));
    }

    #[test]
    fn a_ten_thousand_row_table_draws_a_screenful_and_asks_for_no_more() {
        // The reason `render` takes a closure: the design rebuilds this on
        // every keystroke, and a preview that formatted ten thousand rows per
        // frame would not be a preview.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = AtomicUsize::new(0usize);
        let mut table = Table::new(COLUMNS);
        table.set_rows(10_000);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = style();
        terminal
            .draw(|f| {
                table.render(f, f.area(), &style, true, &|row, column| {
                    asked.fetch_add(1, Ordering::Relaxed);
                    (format!("r{row}c{column}"), None)
                });
            })
            .expect("draw");
        // Eleven rows of body, four columns.
        let asked = asked.load(Ordering::Relaxed);
        assert_eq!(asked, 11 * 4, "asked for {asked} cells");
    }

    #[test]
    fn a_table_draws_its_header_its_rows_and_its_cursor() {
        let mut table = Table::new(COLUMNS);
        table.set_rows(3);
        let backend = TestBackend::new(50, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = style();
        terminal
            .draw(|f| {
                table.render(f, f.area(), &style, true, &|row, column| {
                    (format!("r{row}c{column}"), None)
                });
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let line = |y: u16| -> String {
            (0..50u16)
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        };
        assert!(line(0).starts_with("Name"), "{:?}", line(0));
        assert!(line(0).contains("Ext"), "{:?}", line(0));
        assert!(line(1).starts_with("r0c0"), "{:?}", line(1));
        assert!(line(3).starts_with("r2c0"), "{:?}", line(3));
        // Row 4 is past the rows, so it is blank rather than a repeat.
        assert_eq!(line(4).trim(), "");
    }

    #[test]
    fn a_table_in_a_hopeless_rectangle_draws_nothing_rather_than_panicking() {
        let mut table = Table::new(COLUMNS);
        table.set_rows(5);
        let style = style();
        for (w, h) in [(0u16, 0u16), (1, 1), (3, 2), (60, 1)] {
            let backend = TestBackend::new(w.max(1), h.max(1));
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| {
                    let area = Rect::new(0, 0, w, h);
                    table.render(f, area, &style, false, &|_, _| (String::new(), None));
                })
                .expect("draw");
        }
        assert_eq!(Table::body_rows(Rect::new(0, 0, 10, 0)), 0);
        assert_eq!(Table::body_rows(Rect::new(0, 0, 10, 1)), 0);
        assert_eq!(Table::body_rows(Rect::new(0, 0, 10, 9)), 8);
    }
}
