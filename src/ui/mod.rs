//! Rendering and theming.
//!
//! [`draw`] paints every row of the layout diagram, in order: the menu bar, the
//! two panel boxes - each with its volume line, tab bar, path and filter line,
//! column header, entries, rule and status line - the command line, and the key
//! bar.
//!
//! Two rules from the design shape this module:
//!
//! * **Both cursors are always visible**. Only one hardware
//!   cursor exists, so it goes to the focused region ([`hardware_cursor`]) and
//!   the other is painted into the buffer. Neither ever disappears.
//! * **Below 60x15 there is one message, not a broken layout**.
//!   At exactly 60x15 the whole layout renders, so every rectangle built here
//!   is checked for zero width or height before anything is written into it.
//!
//! Colours are declared by the theme in RGB against semantic slots and
//! *quantized* here for the session's depth; file-type colouring
//! is a separate, rule-based concern and lives in [`filetype`].
//!
//! The v0.2 operation dialogs - the copy/move, progress, conflict,
//! queue and summary boxes - are in [`dialog`]. They implement the framework's
//! [`crate::dialog::Dialog`] trait and are drawn by [`draw`] like any other
//! dialog on the stack; they live here because they are painting, and because
//! everything they draw that is not a `dialog.*` slot is a progress bar.

pub mod cmdline;
pub mod columns;
pub mod console;
pub mod dialog;
pub mod filetype;
pub mod help;
pub mod panelview;
pub mod quickview;
pub mod text;
pub mod viewer;
pub mod volume;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;
use crate::config::Rgb;
use crate::input::Focus;
use crate::input::KeyModifiers;
use crate::panel::Side;
use crate::term::{MIN_HEIGHT, MIN_WIDTH, too_small};

use self::dialog::menu;
use self::text::{Crop, Glyphs};

/// The seven Total Commander key-bar labels.
pub const KEYBAR: [(&str, &str); 10] = [
    ("F1", "Help"),
    ("F2", "Rename"),
    ("F3", "View"),
    ("F4", "Edit"),
    ("F5", "Copy"),
    ("F6", "Move"),
    ("F7", "NewFldr"),
    ("F8", "Delete"),
    ("F9", "Menu"),
    ("F10", "Quit"),
];

/// What the same seven slots show while `Shift` is held.
///
/// `F7` and `F10` keep their unshifted labels: the design binds no
/// `Shift+F7`, and there is no shifted quit.
///
/// `F9` is the file information dialog, which is what `shift+f9` is bound to
/// in the `[panel]` context. It was blank here while the binding existed,
/// which is the one thing this table must never be: a slot drawn empty is a
/// promise that the key does nothing, and holding `Shift` to find out what a
/// key does is the whole reason the table changes at all.
pub const KEYBAR_SHIFT: [(&str, &str); 10] = [
    ("F1", ""),
    ("F2", "Compare"),
    ("F3", "ViewOne"),
    ("F4", "NewFile"),
    ("F5", "CopyHer"),
    ("F6", "Rename"),
    ("F7", "NewFldr"),
    ("F8", "DelPerm"),
    ("F9", "Info"),
    ("F10", "Context"),
];

/// The menu bar's titles.
///
/// The same six menus, in the same order, as
/// [`crate::ui::dialog::menu::MenuModel::menus`] and
/// [`crate::ui::dialog::menu::LETTERS`] - which is what lets `Alt`+letter open
/// one directly without a second table, and what
/// [`crate::input::Action::menu_index`] indexes.
pub const MENUBAR: [&str; 6] = ["Files", "Mark", "Commands", "Net", "Show", "Configuration"];

/// What the same seven slots show while `Alt` is held.
///
/// `Alt+F1`/`Alt+F2` are the device pickers and fall outside the `F3`-`F8`
/// range these slots address, so they do not appear here. An empty label means
/// the slot has no `Alt` binding and is drawn blank rather than misleadingly
/// carrying its unmodified one.
pub const KEYBAR_ALT: [(&str, &str); 10] = [
    ("F1", "L Vol"),
    ("F2", "R Vol"),
    ("F3", "ViewExt"),
    ("F4", "Quit"),
    ("F5", "Pack"),
    ("F6", "Unpack"),
    ("F7", "Search"),
    ("F8", "History"),
    ("F9", "Queue"),
    ("F10", ""),
];

/// What the same seven slots show while `Ctrl` is held.
///
/// The fixed-field sorts, which are what `Ctrl+F<n>` is for.
pub const KEYBAR_CTRL: [(&str, &str); 10] = [
    ("F1", ""),
    ("F2", ""),
    ("F3", "SortNam"),
    ("F4", "SortExt"),
    ("F5", "SortDat"),
    ("F6", "SortSiz"),
    ("F7", "Unsort"),
    ("F8", ""),
    ("F9", ""),
    ("F10", ""),
];

/// The key-bar labels for the current modifier state.
///
/// A modifier is only ever reported where the terminal reports modifier state;
/// the design says a legacy terminal cannot, and to leave the labels unchanged
/// when it does not.
///
/// Precedence with more than one modifier down is Shift, then Alt, then Ctrl -
/// arbitrary, but fixed, because a combination none of the layers describes has
/// to resolve to *something* and flickering between two is worse than picking
/// one.
pub fn keybar_labels(mods: KeyModifiers) -> [(&'static str, &'static str); 10] {
    if mods.contains(KeyModifiers::SHIFT) {
        KEYBAR_SHIFT
    } else if mods.contains(KeyModifiers::ALT) {
        KEYBAR_ALT
    } else if mods.contains(KeyModifiers::CONTROL) {
        KEYBAR_CTRL
    } else {
        KEYBAR
    }
}

/// Where each row of the layout diagram went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout9 {
    /// The menu bar, zero-height when hidden.
    pub menubar: Rect,
    /// The left panel, borders included.
    pub left: Rect,
    /// The right panel, borders included.
    pub right: Rect,
    /// The command line.
    pub cmdline: Rect,
    /// The key bar, zero-height when hidden.
    pub keybar: Rect,
}

/// Carve the screen into the rows of.
pub fn layout(app: &App, area: Rect) -> Layout9 {
    let menubar_h = u16::from(app.config.ui.show_menubar);
    let keybar_h = u16::from(app.config.ui.show_keybar);

    // a multi-line prompt is drawn as many rows as it needs, and
    // the panels shrink by that much. One row without a shell - every v0.1 and
    // v0.2 layout, unchanged.
    let [menubar, body, cmdline, keybar] = Layout::vertical([
        Constraint::Length(menubar_h),
        Constraint::Min(3),
        Constraint::Length(cmdline::rows(app, area)),
        Constraint::Length(keybar_h),
    ])
    .areas(area);

    // `split_ratio` is the left panel's share of the width.
    let ratio = app.config.ui.split_ratio.clamp(0.1, 0.9);
    let left_pct = (ratio * 100.0).round().clamp(10.0, 90.0);
    let left_pct = u16::try_from(left_pct as i64).unwrap_or(50);
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_pct),
            Constraint::Percentage(100_u16.saturating_sub(left_pct)),
        ])
        .areas(body);

    Layout9 {
        menubar,
        left,
        right,
        cmdline,
        keybar,
    }
}

/// Tell each panel how many entry rows this screen size gives it, so
/// `PgUp`/`PgDn` and scrolling agree with what was drawn.
///
/// `draw` only has `&App`, so the event loop calls this before drawing.
pub fn sync_view_rows(app: &mut App, area: Rect) {
    if too_small(area.width, area.height) {
        app.left.view_rows = 0;
        app.right.view_rows = 0;
        return;
    }
    let l = layout(app, area);
    let setting = app.config.panel.show_tab_bar;
    for (side, rect) in [(Side::Left, l.left), (Side::Right, l.right)] {
        let panel = app.panel_mut(side);
        let tabs = panel.tab_bar_visible(setting);
        panel.view_rows = panelview::entry_row_count(rect, tabs);
    }
    // the quick view is a viewer drawn into a panel body, and a
    // viewer has to be laid out for the size it will be drawn at before the
    // keys waiting behind it are applied. The same measurement `main` makes
    // for the full-screen viewer, taken here because this is where the panel
    // rectangles are.
    if let Some(side) = app.quick_view_side() {
        let rect = match side {
            Side::Left => l.left,
            Side::Right => l.right,
        };
        let tabs = app.panel(side).tab_bar_visible(setting);
        let body = panelview::rows(rect, tabs).entries;
        let cols = app
            .quick_viewer()
            .map_or(body.width, |v| viewer::body_cols_for(v, body));
        app.set_quick_view_geometry(body.height, cols);
    }
}

/// Tell every dialog on the stack the interior it is about to be drawn into.
///
/// `draw` only has `&App`, so the event loop calls this before drawing, exactly
/// as it does for [`sync_view_rows`] and [`crate::app::App::set_viewer_view`].
/// A dialog whose scroll offset outlives the frame writes it down here, which
/// is why [`crate::dialog::Dialog::render`] can keep taking `&self`.
pub fn sync_dialog_layout(app: &mut App, area: Rect) {
    // The rectangles first, under a shared borrow, because `dialog_area` reads
    // the panel layout out of the same `App` the dialogs are then taken from.
    let interiors: Vec<Rect> = app
        .dialogs()
        .iter()
        .map(|frame| {
            let within = dialog_area(app, frame.dialog.as_ref(), area);
            crate::dialog::dialog_interior(within, frame.dialog.as_ref())
        })
        .collect();
    for (frame, inner) in app.dialogs_mut().iter_mut().zip(interiors) {
        if inner.width > 0 && inner.height > 0 {
            frame.dialog.layout(inner);
        }
    }
}

/// Draw one frame.
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    // `Ctrl+O` hides the panels, so the screen "is the same screen
    // the shell would have had on its own". Before the `too_small` check,
    // deliberately: the console has no panel layout to be too small for, and a
    // 50x12 shell is exactly as usable as it would be without this application
    // running.
    // `F3` consumes all input and takes the whole
    // screen, exactly as `Ctrl+O` does. Before the console check, because a
    // viewer opened from the command line is what the user is looking at.
    if viewer::is_backdrop(app) {
        viewer::draw(f, app, area);
        if app.dialog_is_open() {
            let style = dialog_style(app);
            for frame in app.dialogs() {
                let within = dialog_area(app, frame.dialog.as_ref(), area);
                crate::dialog::draw(f, frame.dialog.as_ref(), within, &style);
            }
        }
        if let Some((x, y)) = hardware_cursor(app, area) {
            f.set_cursor_position((x, y));
        }
        return;
    }

    if console::is_backdrop(app) {
        console::draw(f, app, area);
        if app.dialog_is_open() {
            let style = dialog_style(app);
            for frame in app.dialogs() {
                let within = dialog_area(app, frame.dialog.as_ref(), area);
                crate::dialog::draw(f, frame.dialog.as_ref(), within, &style);
            }
        }
        if let Some((x, y)) = hardware_cursor(app, area) {
            f.set_cursor_position((x, y));
        }
        return;
    }

    if too_small(area.width, area.height) {
        draw_too_small(f, app, area);
        return;
    }

    let l = layout(app, area);
    let depth = app.color_depth;

    // The panel background covers the whole screen, as in Total Commander.
    let bg = Style::new().bg(app.theme.quantize(app.theme.panel.bg, depth));
    f.render_widget(Block::new().style(bg), area);

    if l.menubar.height > 0 {
        draw_menubar(f, app, l.menubar);
    }
    panelview::draw(f, app, Side::Left, l.left);
    panelview::draw(f, app, Side::Right, l.right);
    draw_activity_indicator(f, app, l.right);
    cmdline::draw(f, app, l.cmdline);
    if l.keybar.height > 0 {
        draw_keybar(f, app, l.keybar);
        // the completion indicator. Painted *over* the bar's last
        // cell after the bar is drawn, so the fixed seven-slot geometry of
        // the design is not disturbed by a single column.
        console::draw_activity(f, app, l.keybar, app.console.activity);
    }

    // The dialog stack goes on top of everything, bottom frame first, so a
    // dialog opened from another sits over its parent.
    if app.dialog_is_open() {
        let style = dialog_style(app);
        for frame in app.dialogs() {
            let within = dialog_area(app, frame.dialog.as_ref(), area);
            crate::dialog::draw(f, frame.dialog.as_ref(), within, &style);
        }
    }

    // the hardware cursor goes to the focused region and the
    // other cursor is painted. It is never hidden.
    if let Some((x, y)) = hardware_cursor(app, area) {
        f.set_cursor_position((x, y));
    }
}

/// The rectangle a dialog is placed inside.
///
/// The whole screen for every dialog but one, which is what
/// the design made the framework's job. A dialog that declares a
/// [`crate::dialog::Dialog::anchor`] gets that panel's rectangle from the row
/// **below its header** downward instead, so the popup "hangs
/// under the target panel's header" and it is visually obvious which side it
/// will act on.
///
/// One function, called by both [`draw`] and `dialog_cursor`, so the drawn box
/// and the hardware cursor cannot disagree.
pub fn dialog_area(app: &App, dialog: &dyn crate::dialog::Dialog, area: Rect) -> Rect {
    let Some(side) = dialog.anchor() else {
        return area;
    };
    // A screen with no panel layout on it has no header to hang under: the
    // console and the full-screen viewer both draw over the panels, and
    // `too_small` draws no panels at all. Centring on the whole screen is the
    // answer in all three, and it is what every other dialog does there.
    if too_small(area.width, area.height) || viewer::is_backdrop(app) || console::is_backdrop(app) {
        return area;
    }
    let l = layout(app, area);
    let panel = match side {
        Side::Left => l.left,
        Side::Right => l.right,
    };
    // The header is the panel's top border row, which carries the path.
    // Below it is where the popup starts.
    let below = panel.y.saturating_add(HEADER_ROWS);
    let height = panel.height.saturating_sub(HEADER_ROWS);
    Rect::new(panel.x, below, panel.width, height)
}

/// How many rows of a panel the popup hangs below: the top border, which is
/// the header the design names.
const HEADER_ROWS: u16 = 1;

/// The `dialog.*` slots, quantized once for this frame.
pub fn dialog_style(app: &App) -> crate::dialog::DialogStyle {
    crate::dialog::DialogStyle::new(&app.theme, app.color_depth, app.config.ui.ascii_borders)
}

/// below 60x15, one message rather than a broken layout.
fn draw_too_small(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::new()
        .fg(color(app, app.theme.panel.fg))
        .bg(color(app, app.theme.panel.bg));
    f.render_widget(Block::new().style(style), area);

    let lines = [
        "terminal too small".to_string(),
        format!(
            "{}x{}, need {MIN_WIDTH}x{MIN_HEIGHT}",
            area.width, area.height
        ),
        "Esc / F10 / q to quit".to_string(),
    ];
    let rows = u16::try_from(lines.len()).unwrap_or(1).min(area.height);
    let top = area.y.saturating_add(area.height.saturating_sub(rows) / 2);
    let block = Rect::new(area.x, top, area.width, rows);
    if block.width == 0 || block.height == 0 {
        return;
    }
    let body: Vec<Line<'static>> = lines
        .iter()
        .map(|l| Line::from(Span::raw(l.clone())))
        .collect();
    f.render_widget(Paragraph::new(body).centered().style(style), block);

    // the hardware cursor is never hidden while the
    // application is in the foreground. There is no layout to put it in at this
    // size, so it parks on the message.
    f.set_cursor_position((area.x, top));
}

/// A small `[·●···]` in the right panel's **bottom-right** corner while
/// background jobs are running - the only hint they exist without opening the
/// queue.
///
/// Drawn over the bottom border just before the corner, where nothing else is
/// drawn, and only when the panel is wide enough that it does not crowd the
/// border. Its animation is the size walk's, so the two read the same; when
/// that is turned off it still shows dots, because here the point is only
/// "something is happening".
fn draw_activity_indicator(f: &mut Frame, app: &App, panel: Rect) {
    const WIDTH: usize = 5; // animation cells between the brackets
    const CELLS: u16 = 7; // `[` + five animation cells + `]`.
    if !app.jobs.any_active() || panel.height == 0 || panel.width < CELLS.saturating_add(12) {
        return;
    }
    // Deliberately the ASCII animation, never the panel's chosen glyph style.
    // The size column can afford a decorated glyph because it owns its cell;
    // this indicator sits one column from the box corner, and the block-drawing
    // and dot glyphs the pretty styles use are *ambiguous width* - a terminal
    // set to render them two cells wide pushes the closing bracket off its
    // column and the animation bleeds into it. `#` and `.` are one cell in
    // every terminal, so the bracket always closes where it is drawn.
    let anim = panelview::walk_indicator(
        crate::config::SizeWalkStyle::Snake,
        app.animation.elapsed(),
        true,
    );
    // A belt-and-braces clamp: whatever the animation returns is forced to
    // exactly five columns, so the field can never grow and shove the bracket.
    let anim = text::fit_left(&anim, WIDTH, Crop::End, "");
    let depth = app.color_depth;
    let style = Style::new()
        .fg(app.theme.quantize(app.theme.panel.marked_fg, depth))
        .bg(app.theme.quantize(app.theme.panel.bg, depth));
    // The bottom border row of the panel box, right end. Two columns are left
    // between the `]` and the box corner: one blank so the bracket does not
    // butt against the corner, and the corner glyph itself so the box still
    // closes cleanly.
    let x = panel.right().saturating_sub(CELLS.saturating_add(2));
    let y = panel.bottom().saturating_sub(1);
    let rect = Rect::new(x, y, CELLS, 1);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!("[{anim}]"), style))),
        rect,
    );
}

/// The menu bar.
fn draw_menubar(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::new()
        .fg(color(app, app.theme.panel.header_fg))
        .bg(color(app, app.theme.panel.header_bg));
    // the mnemonic rule, applied outside a dialog: the letter
    // `Alt` opens the menu with is the letter drawn underlined, and there is
    // one table of those letters rather than two.
    let underline = style.add_modifier(Modifier::UNDERLINED);
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut drawn = 0_usize;
    let width = usize::from(area.width);
    for (index, title) in MENUBAR.iter().enumerate() {
        let letter = menu::LETTERS.get(index).copied();
        let parts = letter.and_then(|letter| crate::dialog::split_mnemonic(title, letter));
        let pieces: Vec<(String, Style)> = match parts {
            Some((before, at, after)) => vec![
                (format!(" {before}"), style),
                (at.to_string(), underline),
                (format!("{after} "), style),
            ],
            // No letter of this title is its mnemonic, which the census
            // forbids; drawing the plain title is still better than dropping
            // the menu off the bar.
            None => vec![(format!(" {title} "), style)],
        };
        for (piece, piece_style) in pieces {
            let room = width.saturating_sub(drawn);
            if room == 0 {
                break;
            }
            // Cropped, never padded: the pieces of one title are drawn one
            // after another and a padded piece would push the rest of the
            // title off the bar.
            let piece = text::truncate(&piece, room, Crop::End, g.ellipsis());
            drawn = drawn.saturating_add(text::width(&piece));
            spans.push(Span::styled(piece, piece_style));
        }
    }
    // The bar's own colour runs to the edge of the screen, as it did before
    // the titles were split into spans.
    if drawn < width {
        spans.push(Span::styled(" ".repeat(width.saturating_sub(drawn)), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The key bar.
fn draw_keybar(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let number = Style::new()
        .fg(color(app, app.theme.keybar.number_fg))
        .bg(color(app, app.theme.panel.bg));
    let label = Style::new()
        .fg(color(app, app.theme.keybar.label_fg))
        .bg(color(app, app.theme.keybar.label_bg));

    let mut spans = Vec::with_capacity(KEYBAR.len().saturating_mul(2));
    let g = Glyphs::new(app.config.ui.ascii_borders);
    for (key, text) in keybar_slots(
        app.keyboard.mods_held,
        usize::from(area.width),
        g.ellipsis(),
    ) {
        spans.push(Span::styled(key, number));
        if !text.is_empty() {
            spans.push(Span::styled(text, label));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The shortest label worth drawing: an ellipsis is one cell, so anything
/// below this is a letter and a marker rather than a word.
const MIN_KEYBAR_LABEL: usize = 3;

/// The longest a key-bar label may be, in cells, in **any** layer.
///
/// Uniform lengths keep the slots looking alike as the modifier changes, and
/// seven is what the longest genuinely useful word needs - `History`,
/// `NewFldr`, `ViewExt`, `SortNam`. Anything that will not fit is abbreviated in the tables
/// rather than cropped at render time, so the abbreviation is a decision
/// somebody made and not an accident of the terminal width.
pub const KEYBAR_LABEL_MAX: usize = 7;

/// The key column, in cells: `F10` and one of spacing.
///
/// The key is always the **bare** function key, in every layer. Printing
/// `Shift+F6` while the user is holding Shift spends five cells telling them
/// what their own hand is doing; the modifier is the question, the operation is
/// the answer. Only the operation name changes as a modifier goes down.
const KEYBAR_KEY_FIELD: usize = 4;

/// The operation column: one cell of leading space, seven of text, one of
/// trailing space - so the longest name never runs straight into the next
/// slot's key.
const KEYBAR_LABEL_FIELD: usize = 9;

/// A whole slot.
const KEYBAR_SLOT: usize = KEYBAR_KEY_FIELD + KEYBAR_LABEL_FIELD;
/// The seven key-bar slots, at a **fixed geometry**.
///
/// Every slot is the same width and every layer has seven of them, so the
/// boundaries depend only on the terminal width - never on what the labels
/// happen to say. Holding or releasing a modifier therefore swaps the *text* in
/// place and moves nothing: an earlier version shared the leftover space out
/// among the labels by need, which made `F7` sit somewhere different under
/// `Ctrl` than under `Shift` and turned every modifier press into a visible
/// shuffle of the whole bar.
///
/// A layer with fewer than seven bindings (`Ctrl` has five) leaves the spare
/// slots **blank rather than closing the gap**, for the same reason: the six
/// that do exist must not move because the seventh does not.
///
/// Labels are at most [`KEYBAR_LABEL_MAX`] cells in every layer, so a slot is
/// wide enough for its text at any size worth rendering, and cropping is a
/// fallback rather than the normal case.
fn keybar_slots(mods: KeyModifiers, width: usize, ellipsis: &str) -> Vec<(String, String)> {
    let labels = keybar_labels(mods);
    let count = labels.len();

    // Two fixed columns per slot: the key, then the operation. Both are padded
    // to their field width, so the operation always begins at the same offset
    // and neither column can be widened by whatever the other layer happens to
    // say. Below the full width the fields scale together, still from the
    // terminal width alone and never from the labels.
    let slot = width.checked_div(count).unwrap_or(0).min(KEYBAR_SLOT);
    if slot < 3 {
        return labels
            .iter()
            .map(|_| (String::new(), String::new()))
            .collect();
    }
    let key_field = KEYBAR_KEY_FIELD.min(slot.saturating_sub(1)).max(1);
    let label_field = slot.saturating_sub(key_field);

    labels
        .iter()
        .map(|(key, label)| {
            // Every layer shows the same ten keys. A layer that binds nothing
            // to one of them leaves the *operation* blank and keeps the key and
            // its button, painted in the key-bar colours like every other. A
            // slot that vanished would be a gap in the bar, which is the shuffle
            // this layout exists to prevent, only worse for being intermittent.
            let key_text = pad_to(&format!(" {key}"), key_field, ellipsis);
            // A label field too small for a readable word shows nothing rather
            // than an ellipsis: `F3  …` spends three cells saying "there was a
            // word here", which the user cannot act on. Keys alone is the
            // honest degradation, and the slots still do not move.
            let label_text = if label_field.saturating_sub(1) < MIN_KEYBAR_LABEL {
                " ".repeat(label_field)
            } else {
                pad_to(&format!(" {label}"), label_field, ellipsis)
            };
            (key_text, label_text)
        })
        .collect()
}

/// Crop to `field` cells, then pad with spaces to exactly that many.
fn pad_to(text: &str, field: usize, ellipsis: &str) -> String {
    let cropped = if text::width(text) > field {
        text::truncate(text, field, Crop::End, ellipsis)
    } else {
        text.to_string()
    };
    let pad = field.saturating_sub(text::width(&cropped));
    format!("{cropped}{}", " ".repeat(pad))
}

/// Where the top dialog wants the cursor, if it has a field to put it in.
///
/// A message box or a confirmation has none and answers `None`, so the caller
/// falls back to whatever is behind it - the command line, or the shell's own
/// screen - and the cursor is never hidden.
fn dialog_cursor(app: &App, area: Rect) -> Option<(u16, u16)> {
    app.top_dialog().and_then(|dialog| {
        let rect = crate::dialog::dialog_rect(dialog_area(app, dialog, area), dialog);
        if rect.width < crate::dialog::MIN_DIALOG_WIDTH
            || rect.height < crate::dialog::MIN_DIALOG_HEIGHT
        {
            return None;
        }
        // The interior is the rectangle inside the one-cell border, which is
        // what `dialog::draw` hands the dialog.
        let inner = Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        dialog.cursor(inner)
    })
}

/// Where the terminal's real cursor goes.
///
/// The hardware cursor is positioned in the **focused** region, so terminals
/// that track it - and assistive tooling that follows it - land in the right
/// place. The unfocused region's cursor is painted as a styled cell instead;
/// neither ever disappears.
///
/// `area` is the whole terminal, the same rectangle [`draw`] was given.
pub fn hardware_cursor(app: &App, area: Rect) -> Option<(u16, u16)> {
    // the hardware cursor is never hidden while the application
    // is in the foreground. the design gave the viewer a cursor of its own,
    // so it goes on that cursor's cell; with `viewer.cursor = false`, or with
    // the cursor scrolled off screen, it parks on the first cell of the status
    // row as v0.4 left it - out of the text and still somewhere a screen reader
    // can find it.
    if viewer::is_backdrop(app) {
        if matches!(app.focus, Focus::Dialog(_))
            && app.dialog_is_open()
            && let Some(placed) = dialog_cursor(app, area)
        {
            return Some(placed);
        }
        if area.width == 0 || area.height == 0 {
            return None;
        }
        // Only while the viewer itself has focus: a dialog raised over it owns
        // the cursor, and one with no field of its own leaves it parked rather
        // than putting it back in text the user is not editing.
        if app.focus == Focus::Viewer
            && let Some(placed) = viewer::cursor_cell(app, area)
        {
            return Some(placed);
        }
        return Some((area.x, area.y.saturating_add(area.height.saturating_sub(1))));
    }

    // with the panels hidden, the shell's cursor **is** the
    // terminal's. Answered before the `too_small` guard, because [`draw`]
    // paints the console before it too - a shell on a 50x12 terminal is a
    // working shell, and one with no cursor is not.
    if console::is_backdrop(app) {
        // A dialog raised over that screen still owns the cursor while it has
        // a field of its own; a message box has none, and the
        // cursor falls back to the shell's rather than to a command line that
        // is not on screen.
        if matches!(app.focus, Focus::Dialog(_))
            && app.dialog_is_open()
            && let Some(placed) = dialog_cursor(app, area)
        {
            return Some(placed);
        }
        return console::cursor(app, area);
    }

    if too_small(area.width, area.height) {
        return None;
    }
    let l = layout(app, area);

    match app.focus {
        Focus::CommandLine => {
            if l.cmdline.width == 0 || l.cmdline.height == 0 {
                return None;
            }
            cmdline::caret(app, l.cmdline)
        }
        // **hidden while a panel has focus.** The full-width
        // cursor bar already shows the position, so the terminal's own block
        // sitting on the first cell of the name column adds nothing and reads
        // as a stray white box on top of an already-highlighted row. The rule
        // that both *cursors* stay visible is unaffected - the bar is one and
        // the painted caret is the other; it is the hardware cursor, a third
        // thing, that goes away.
        Focus::Panel(_) => None,
        // A dialog owns the cursor while it is open: the field it is editing is
        // where the user is looking. A dialog with no field of its own - a
        // message box, a confirmation - returns `None` and the cursor falls
        // through to the command line, so it is never hidden.
        Focus::Dialog(_) if app.dialog_is_open() => {
            if let Some(placed) = dialog_cursor(app, area) {
                return Some(placed);
            }
            if l.cmdline.width == 0 || l.cmdline.height == 0 {
                return None;
            }
            cmdline::caret(app, l.cmdline)
        }
        // The viewer and the console are later milestones; until they draw
        // their own caret the command line keeps the hardware cursor, so it is
        // never hidden.
        _ => {
            if l.cmdline.width == 0 || l.cmdline.height == 0 {
                return None;
            }
            cmdline::caret(app, l.cmdline)
        }
    }
}

/// Quantize a theme colour for this session.
fn color(app: &App, rgb: Rgb) -> ratatui::style::Color {
    app.theme.quantize(rgb, app.color_depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Config, Keymap, Theme};
    use crate::panel::{ColumnId, SortKey};
    use crate::vfs::{Entry, VfsPath};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    fn app() -> App {
        let mut a = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        a.color_depth = ColorDepth::TrueColor;
        for side in [Side::Left, Side::Right] {
            let tab = a.panel_mut(side).active_tab_mut();
            tab.path = VfsPath::local("/home/thorin/Development");
            tab.entries = vec![
                Entry::parent_entry(),
                Entry::dir("undermarquee"),
                {
                    let mut e = Entry::file("1 - BASE PANEL.3mf");
                    e.size = 362_333;
                    e.mode = 0o644;
                    e
                },
                {
                    let mut e = Entry::file("a-really-quite-extraordinarily-long-file-name.txt");
                    e.size = 12;
                    e.mode = 0o644;
                    e
                },
            ];
        }
        a
    }

    fn render(a: &App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| draw(f, a)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// Every border glyph of one panel, wherever it is drawn from.
    ///
    /// The frame comes from a ratatui `Block` but the `├──┤` rule above the
    /// status line is painted separately, so "the panel's border colour" is
    /// established in two places and they have to agree.
    fn border_colours(buf: &Buffer, x0: u16, x1: u16) -> std::collections::BTreeSet<String> {
        let area = *buf.area();
        let mut out = std::collections::BTreeSet::new();
        for y in area.y..area.bottom() {
            for x in x0..=x1 {
                let Some(cell) = buf.cell((x, y)) else {
                    continue;
                };
                // Box-drawing glyphs only. The ASCII fallback set (`+-|`)
                // would also match hyphens and pipes inside filenames.
                if "┌┐└┘├┤─│".contains(cell.symbol()) {
                    out.insert(format!("{:?}", cell.fg));
                }
            }
        }
        out
    }

    /// Every dialog the design adds, built the way `input::dispatch` builds
    /// it, so the audit below sees what a user would.
    fn v02_dialogs() -> Vec<(&'static str, Box<dyn crate::dialog::Dialog>)> {
        use crate::dialog::{ConfirmDialog, InputDialog, MessageDialog};
        use crate::input::DialogId;
        use crate::ops::walk::SelectionStats;
        use crate::ops::{ConflictRequest, JobFailure, JobId, JobKind, JobStatus, JobSummary};
        use crate::panel::mask::MaskDialog;
        use crate::ui::dialog::{
            ConflictDialog, CopyMoveDialog, ProgressDialog, QueueDialog, SummaryDialog,
        };

        let cfg = crate::config::PanelConfig::default();
        let stats = SelectionStats {
            bytes: 19_056_913_612,
            files: 523,
            dirs: 95,
            unsized_dirs: 0,
        };

        // A job far enough along to have every field the progress dialog can
        // draw: two bars, a rate, an ETA and a long name to crop.
        let mut running = JobStatus::queued(JobId(1), JobKind::Copy);
        running.started = true;
        running.file = "/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf".to_string();
        running.file_bytes_done = 3_000_000;
        running.file_bytes_total = 7_000_000;
        running.files_done = 10;
        running.files_total = 200;
        running.bytes_done = 2_100_000_000;
        running.bytes_total = 8_700_000_000;
        running.throughput = Some(12_400_000);
        running.eta = Some(std::time::Duration::from_secs(89));
        running.elapsed = std::time::Duration::from_secs(34);

        let mut waiting = JobStatus::queued(JobId(2), JobKind::Move);
        waiting.files_total = 4;
        let mut failed = JobStatus::queued(JobId(3), JobKind::Delete { trash: false });
        failed.failures.push(JobFailure {
            path: VfsPath::local("/srv/media/locked.bin"),
            error: "Permission denied (os error 13)".to_string(),
        });

        let summary = JobSummary {
            kind: JobKind::Copy,
            files_done: 199,
            dirs_done: 12,
            bytes_done: 8_700_000_000,
            skipped: 1,
            failures: vec![
                JobFailure {
                    path: VfsPath::local("/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf"),
                    error: "Permission denied (os error 13)".to_string(),
                },
                JobFailure {
                    path: VfsPath::local("/srv/media/other.bin"),
                    error: "No space left on device (os error 28)".to_string(),
                },
            ],
            cancelled: false,
            elapsed: std::time::Duration::from_secs(212),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        };

        let request = ConflictRequest {
            source: VfsPath::local("/srv/media/Arcade/Leap/stl/10 - POWER PANEL.3mf"),
            dest: VfsPath::local("/backup/Arcade/Leap/stl/10 - POWER PANEL.3mf"),
            source_size: 7_000_000,
            dest_size: 6_500_000,
            source_mtime: Some(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_770_000_000),
            ),
            dest_mtime: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            both_dirs: false,
            dest_is_dir: false,
        };

        vec![
            (
                "copy",
                Box::new(CopyMoveDialog::new(
                    JobKind::Copy,
                    523,
                    "/srv/media/*.*".to_string(),
                    stats,
                    &cfg,
                )),
            ),
            (
                "move",
                Box::new(CopyMoveDialog::new(
                    JobKind::Move,
                    3,
                    "/srv/media/*.*".to_string(),
                    stats,
                    &cfg,
                )),
            ),
            (
                "progress",
                Box::new(ProgressDialog::new(running.clone(), 1024 * 1024)),
            ),
            (
                "conflict",
                Box::new(ConflictDialog::new(
                    JobId(1),
                    Box::new(request),
                    "10 - POWER PANEL Copy 1.3mf",
                    &cfg,
                )),
            ),
            (
                "queue",
                Box::new(QueueDialog::new(vec![running, waiting, failed])),
            ),
            ("summary", Box::new(SummaryDialog::new(JobId(1), summary))),
            (
                "mkdir",
                Box::new(InputDialog::new(
                    DialogId::Mkdir,
                    "Create directory",
                    "Name:",
                    "",
                )),
            ),
            (
                "goto",
                Box::new(InputDialog::new(DialogId::GotoPath, "Go to", "Path:", "")),
            ),
            (
                "select_mask",
                Box::new(MaskDialog::new(DialogId::SelectMask, "*.*")),
            ),
            (
                "confirm_delete",
                Box::new(
                    ConfirmDialog::new(
                        DialogId::ConfirmDelete,
                        "Delete permanently",
                        crate::ops::delete::confirm_lines(
                            &[
                                VfsPath::local("/srv/media/a.bin"),
                                VfsPath::local("/srv/media/b.bin"),
                            ],
                            false,
                        ),
                    )
                    .with_buttons("Delete", "Cancel"),
                ),
            ),
            (
                "confirm_quit",
                Box::new(
                    ConfirmDialog::new(DialogId::ConfirmQuit, "Quit", {
                        let mut lines = vec!["Still running:".to_string()];
                        lines.extend(crate::ops::running_job_lines(&[
                            JobStatus::queued(JobId(1), JobKind::Copy),
                            JobStatus::queued(JobId(2), JobKind::Move),
                        ]));
                        lines.push("Quitting stops it where it is.".to_string());
                        lines
                    })
                    .with_buttons("Quit", "Cancel"),
                ),
            ),
            (
                "message",
                Box::new(MessageDialog::line(
                    "Copy",
                    "Permission denied (os error 13)",
                )),
            ),
        ]
    }

    /// Every dialog the design adds, with the strings it really produces.
    ///
    /// The two rewrite prompts carry `RewriteLimits::gate`'s own text rather
    /// than a shortened stand-in: the refusal names two sizes, a config key and
    /// a whole sentence of advice, and it is the longest thing this milestone
    /// ever puts in a box. Inventing a shorter message here would audit a
    /// dialog the user never sees.
    ///
    /// **`rewrite_warn` is ahead of its wiring.** `RewriteGate::Warn` has no
    /// caller yet - only `RewriteLimits::check`'s refusal backstop is connected
    /// (`ArchiveFs::open_write`), so step 3's "warn between
    /// `rewrite_warn_size` and `rewrite_max_size`, with a cancel that is the
    /// default button" is still to be built. It is audited here against the
    /// widget the design describes so that the layout is known-good when it
    /// is, and so that this gap is recorded somewhere that runs.
    ///
    /// One thing that audit already shows: at 60 columns a whole gate message
    /// is one cropped line. `MessageDialog` and `ConfirmDialog` take a line per
    /// `String` and crop rather than wrap - the same contract
    /// `App::show_message`, `ops::delete::confirm_lines` and
    /// `ops::running_job_lines` are written to - so whoever wires these must
    /// split the message into lines. the design wants the refusal to state
    /// "the reason ... and the suggestion to extract, modify and repack
    /// deliberately", and on one 54-column line it states neither.
    fn v05_dialogs() -> Vec<(&'static str, Box<dyn crate::dialog::Dialog>)> {
        use crate::dialog::{ConfirmDialog, Dialog as _, DialogKey, MessageDialog};
        use crate::input::{DialogId, KeyPress};
        use crate::ops::{JobFailure, JobId, JobKind, JobSummary};
        use crate::ui::dialog::{SummaryDialog, pack::PackDialog};
        use crate::vfs::BackendKind;
        use crate::vfs::archive::{RewriteGate, RewriteLimits};

        let limits = RewriteLimits::default();
        let temp = std::env::temp_dir();
        // Between `rewrite_warn_size` and `rewrite_max_size`, and above
        // `rewrite_max_size`: the warn and refuse.
        let warn = match limits.gate(300 * 1024 * 1024, &temp, "backup.tar.gz") {
            RewriteGate::Warn(text) => text,
            other => panic!("300 MiB should warn, got {other:?}"),
        };
        let refuse = match limits.gate(4 * 1024 * 1024 * 1024, &temp, "backup.tar.gz") {
            RewriteGate::Refuse(text) => text,
            other => panic!("4 GiB should be refused, got {other:?}"),
        };

        // An extraction that went wrong the ways the design says it can:
        // a Zip Slip refusal, a lying header, and a member that will not
        // decompress. The paths are inside an archive, so they are long.
        let summary = JobSummary {
            kind: JobKind::Copy,
            files_done: 412,
            dirs_done: 37,
            bytes_done: 1_900_000_000,
            skipped: 2,
            failures: vec![
                JobFailure {
                    path: VfsPath::local("/srv/media/backup.tar.gz")
                        .with_segment(BackendKind::Archive, "/etc/../../../root/.ssh/id_rsa"),
                    error: "the entry would be written outside the destination \
                            directory"
                        .to_string(),
                },
                JobFailure {
                    path: VfsPath::local("/srv/media/backup.tar.gz")
                        .with_segment(BackendKind::Archive, "/payload/big.bin"),
                    error: "the archive ends 4194304 bytes before the entry it declares does"
                        .to_string(),
                },
            ],
            cancelled: false,
            elapsed: std::time::Duration::from_secs(96),
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        };

        vec![
            (
                "pack",
                Box::new(PackDialog::new(
                    523,
                    "/srv/media/backup/photographs.zip".to_string(),
                )),
            ),
            (
                "pack_error",
                Box::new({
                    let mut d = PackDialog::new(1, String::new());
                    // The in-dialog refusal of an empty name, on screen at once.
                    let _ = d.handle_key(&DialogKey::raw(KeyPress::plain(
                        crossterm::event::KeyCode::Enter,
                    )));
                    d
                }),
            ),
            (
                "rewrite_warn",
                Box::new(
                    ConfirmDialog::new(DialogId::Message, "Rewrite archive", vec![warn])
                        // "with a cancel that is the default
                        // button", which `ConfirmDialog` focuses already.
                        .with_buttons("Rewrite", "Cancel"),
                ),
            ),
            (
                "rewrite_refuse",
                Box::new(MessageDialog::new("Cannot rewrite", vec![refuse])),
            ),
            (
                "archive_summary",
                Box::new(SummaryDialog::new(JobId(7), summary)),
            ),
        ]
    }

    #[test]
    #[ignore = "visual aid: cargo test -- --ignored --nocapture v02_dialogs_at_the_minimum"]
    fn v02_dialogs_at_the_minimum() {
        for (name, dialog) in v02_dialogs() {
            let mut a = app();
            a.push_dialog(dialog);
            println!("--- {name} at 60x15 ---\n{}", dump(&render(&a, 60, 15)));
        }
    }

    #[test]
    fn every_v02_dialog_renders_inside_the_minimum_terminal() {
        // "Minimum usable size is 60x15." Every dialog the design
        // adds has to fit inside it - laid out by `draw`, on top of the real
        // panels, which is the only place the clamping in `dialog::centred` and
        // each dialog's own `rows` split meet.
        for ascii in [false, true] {
            for (name, dialog) in v02_dialogs() {
                let mut a = app();
                a.config.ui.ascii_borders = ascii;
                let title = dialog.title();
                a.push_dialog(dialog);
                let buf = render(&a, 60, 15);
                let out = dump(&buf);

                // 1. Nothing is drawn outside the terminal: `TestBackend` would
                //    have panicked, so reaching here is that assertion. What is
                //    left is whether anything was drawn *at all*.
                assert!(
                    out.lines().any(|l| !l.trim().is_empty()),
                    "{name} ascii={ascii}: blank screen"
                );

                // 2. The dialog is on screen and says what it is. The title is
                //    cropped to the box, so the first word is what is checked.
                let head = title.split_whitespace().next().unwrap_or("");
                assert!(
                    out.contains(head),
                    "{name} ascii={ascii}: no sign of the title {title:?}:\n{out}"
                );

                // 3. It is a box, not a bare paragraph over the panels.
                let corners = if ascii { "+" } else { "\u{256d}\u{2570}" };
                assert!(
                    out.chars().any(|c| corners.contains(c)),
                    "{name} ascii={ascii}: no dialog frame:\n{out}"
                );

                // 4. Under `ui.ascii_borders` nothing non-ASCII survives,
                // bars and spinners included.
                if ascii {
                    assert!(out.is_ascii(), "{name}: a non-ASCII glyph at 60x15:\n{out}");
                }
            }
        }
    }

    #[test]
    #[ignore = "visual aid: cargo test -- --ignored --nocapture v05_dialogs_at_the_minimum"]
    fn v05_dialogs_at_the_minimum() {
        for (name, dialog) in v05_dialogs() {
            let mut a = app();
            a.push_dialog(dialog);
            println!("--- {name} at 60x15 ---\n{}", dump(&render(&a, 60, 15)));
        }
    }

    #[test]
    fn every_v05_dialog_renders_inside_the_minimum_terminal() {
        // "Minimum usable size is 60x15." The same audit the dialogs
        // get, for the three the design adds: the `Alt+F5` pack dialog,
        // the rewrite warning and refusal, and the failure summary an
        // extraction produces.
        for ascii in [false, true] {
            for (name, dialog) in v05_dialogs() {
                let mut a = app();
                a.config.ui.ascii_borders = ascii;
                let title = dialog.title();
                a.push_dialog(dialog);
                let out = dump(&render(&a, 60, 15));

                // 1. Drawing outside the terminal panics `TestBackend`, so
                //    getting here is that assertion; this is whether anything
                //    was drawn at all.
                assert!(
                    out.lines().any(|l| !l.trim().is_empty()),
                    "{name} ascii={ascii}: blank screen"
                );

                // 2. The box is on screen and says what it is.
                let head = title.split_whitespace().next().unwrap_or("");
                assert!(
                    out.contains(head),
                    "{name} ascii={ascii}: no sign of the title {title:?}:\n{out}"
                );

                // 3. A framed box, not a paragraph over the panels.
                let corners = if ascii { "+" } else { "\u{256d}\u{2570}" };
                assert!(
                    out.chars().any(|c| corners.contains(c)),
                    "{name} ascii={ascii}: no dialog frame:\n{out}"
                );

                // 4. Under `ui.ascii_borders` nothing non-ASCII survives
                //    - the checkbox and the `< >` selectors of the
                //    pack dialog included.
                if ascii {
                    assert!(out.is_ascii(), "{name}: a non-ASCII glyph at 60x15:\n{out}");
                }

                // 5. Every dialog here asks a question, so the buttons that
                //    answer it have to be reachable. A message that scrolled
                //    its own OK off the bottom is not a message.
                let has_buttons = ["OK", "Cancel", "Rewrite", "Close", "Retry"]
                    .iter()
                    .any(|b| out.contains(b));
                assert!(
                    has_buttons,
                    "{name} ascii={ascii}: no button on screen at 60x15:\n{out}"
                );
            }
        }
    }

    /// What the screen says at a size the layout cannot fit, cropped to the
    /// width there is: at 1x1 that is the single letter `t`.
    ///
    /// `dump` ends every row with a newline whatever was painted, so a
    /// non-empty dump says nothing at all about what was drawn. This is the
    /// string that does.
    fn too_small_marker(w: u16) -> String {
        "terminal too small".chars().take(w as usize).collect()
    }

    /// The corner glyphs of a frame, ASCII fallback included.
    ///
    /// Below the minimum nothing is framed: the message has the screen to
    /// itself, so a corner anywhere is a half-drawn layout underneath it.
    const FRAME_CORNERS: &str = "\u{256d}\u{256e}\u{2570}\u{256f}\u{250c}\u{2510}\u{2514}\u{2518}";

    #[test]
    fn every_v05_dialog_degrades_rather_than_breaks_below_the_minimum() {
        // "re-layout, never crash on a 1x1 terminal": below the minimum the
        // message wins the whole screen, dialog or no dialog, and as much of
        // it as there is room for is what is drawn.
        for (w, h) in [(40u16, 10u16), (20, 6), (4, 3), (1, 1)] {
            for (name, dialog) in v05_dialogs() {
                let mut a = app();
                a.push_dialog(dialog);
                let out = dump(&render(&a, w, h));
                let want = too_small_marker(w);
                assert!(
                    out.contains(&want),
                    "{name} at {w}x{h}: no sign of {want:?}:\n{out}"
                );
                assert!(
                    !out.chars().any(|c| FRAME_CORNERS.contains(c)),
                    "{name} at {w}x{h}: a half-drawn frame under the message:\n{out}"
                );
            }
        }
    }

    #[test]
    fn every_v02_dialog_degrades_rather_than_breaks_below_the_minimum() {
        // "re-layout, never crash on a 1x1 terminal". Below `MIN_DIALOG_*` the
        // framework draws no box at all rather than a frame with nothing in
        // it; what is under test is that the too-small message wins the screen
        // whatever was on the dialog stack, down to the one letter that fits
        // on a 1x1 terminal.
        for (w, h) in [(40u16, 10u16), (20, 6), (4, 3), (1, 1)] {
            for (name, dialog) in v02_dialogs() {
                let mut a = app();
                a.push_dialog(dialog);
                let out = dump(&render(&a, w, h));
                let want = too_small_marker(w);
                assert!(
                    out.contains(&want),
                    "{name} at {w}x{h}: no sign of {want:?}:\n{out}"
                );
                assert!(
                    !out.chars().any(|c| FRAME_CORNERS.contains(c)),
                    "{name} at {w}x{h}: a half-drawn frame under the message:\n{out}"
                );
            }
        }
    }

    #[test]
    fn an_inactive_panel_dims_every_border_glyph_including_the_rule() {
        // The rule's ends are ├ and ┤: they sit in the block's own border
        // columns and join up with it, so drawing them in the active colour on
        // an inactive panel leaves two lit corners in a dimmed frame. the design
        // the design keeps `border` and `inactive_border` as separate slots precisely
        // so an unfocused panel recedes; it has to recede all the way.
        let mut a = app();
        a.active_side = Side::Left;
        a.focus = Focus::Panel(Side::Left);
        let buf = render(&a, 100, 30);

        let left = border_colours(&buf, 0, 49);
        let right = border_colours(&buf, 50, 99);
        assert_eq!(
            left.len(),
            1,
            "active panel: one border colour, got {left:?}"
        );
        assert_eq!(
            right.len(),
            1,
            "inactive panel: one border colour, got {right:?}"
        );
        assert_ne!(left, right, "the two panels must not look the same");

        // And the other way round, so neither side is special-cased.
        a.active_side = Side::Right;
        a.focus = Focus::Panel(Side::Right);
        let buf = render(&a, 100, 30);
        assert_eq!(border_colours(&buf, 0, 49), right, "left now dimmed");
        assert_eq!(border_colours(&buf, 50, 99), left, "right now lit");
    }

    fn dump(buf: &Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    fn styles(buf: &Buffer) -> Vec<(Color, Color)> {
        buf.content().iter().map(|c| (c.fg, c.bg)).collect()
    }

    // ---------------------------------------------------------------- sizes --

    #[test]
    fn every_size_draws_the_layout_or_the_reason_it_cannot() {
        // Drawing outside the terminal panics `TestBackend`, so reaching the
        // assertions at all is the "never crash" half. The assertions are the
        // other half: at or above the minimum the real layout is on screen,
        // and below it the sentence that says why - never a blank screen,
        // which is what a size the layout silently gave up on would leave.
        let a = app();
        for (w, h, big_enough) in [
            (200u16, 50u16, true),
            (80, 24, true),
            (60, 15, true),
            (59, 14, false),
            (20, 5, false),
            (1, 1, false),
            (2, 2, false),
        ] {
            let out = dump(&render(&a, w, h));
            if big_enough {
                assert!(out.contains("Name"), "{w}x{h}: no column header:\n{out}");
                assert!(out.contains("[..]"), "{w}x{h}: no entries:\n{out}");
                assert!(out.contains(" F3 "), "{w}x{h}: no key bar:\n{out}");
            } else {
                let want = too_small_marker(w);
                assert!(out.contains(&want), "{w}x{h}: no sign of {want:?}:\n{out}");
                assert!(!out.contains("Name"), "{w}x{h}: a panel as well:\n{out}");
            }
        }
    }

    #[test]
    fn the_too_small_message_appears_below_the_minimum_and_not_above_it() {
        let a = app();
        for (w, h) in [(59u16, 14u16), (20, 5), (59, 15), (60, 14)] {
            let out = dump(&render(&a, w, h));
            assert!(
                out.contains("terminal too small") || w < 18,
                "{w}x{h} should say so:\n{out}"
            );
        }
        for (w, h) in [(60u16, 15u16), (80, 24), (200, 50)] {
            let out = dump(&render(&a, w, h));
            assert!(
                !out.contains("terminal too small"),
                "{w}x{h} is big enough:\n{out}"
            );
        }
    }

    #[test]
    fn a_one_by_one_terminal_draws_the_one_cell_it_has_and_survives() {
        // One cell is still a screen: it gets the first letter of the reason
        // the layout will not fit, which is all that can be said in it.
        let a = app();
        assert_eq!(dump(&render(&a, 1, 1)), "t\n");
    }

    #[test]
    fn the_whole_layout_renders_at_exactly_the_minimum_size() {
        let a = app();
        let out = dump(&render(&a, 60, 15));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 15);
        // Top border with the volume line, then path, header, entries, rule,
        // status, bottom border, command line, key bar.
        assert!(
            lines.first().is_some_and(|l| l.contains('\u{250C}')),
            "{out}"
        );
        assert!(out.contains("Name"), "the column header is drawn:\n{out}");
        assert!(out.contains("[..]"), "the entries are drawn:\n{out}");
        assert!(out.contains(" F3 "), "the key bar is drawn:\n{out}");
        assert!(
            out.contains('>'),
            "the command line prompt is drawn:\n{out}"
        );

        // Budget check: at least one entry row survives.
        let l = layout(&a, Rect::new(0, 0, 60, 15));
        assert!(panelview::rows(l.left, false).entries.height >= 1);
    }

    // -------------------------------------------------------------- cursors --

    #[test]
    fn both_cursors_are_present_under_panel_focus() {
        let a = app();
        let area = Rect::new(0, 0, 100, 30);
        let buf = render(&a, 100, 30);
        let t = &a.theme;
        let depth = a.color_depth;
        let all = styles(&buf);

        // The active panel's bar, in the focused style.
        let focused_bg = t.quantize(t.panel.cursor_bg, depth);
        assert!(
            all.iter().any(|(_, bg)| *bg == focused_bg),
            "the focused cursor bar is missing"
        );
        // The inactive panel's bar, in the third, weaker style.
        let inactive_bg = t.quantize(t.panel.inactive_cursor_bg, depth);
        assert!(
            all.iter().any(|(_, bg)| *bg == inactive_bg),
            "the inactive panel's cursor bar is missing"
        );
        // The command line's caret, painted because it is not focused.
        let caret = t.quantize(t.cmdline.caret_unfocused, depth);
        assert!(
            all.iter().any(|(fg, bg)| *fg == caret || *bg == caret),
            "the painted command-line caret is missing"
        );
        // And the terminal's own cursor is *hidden*: the full-width bar already
        // shows the position, so a block on top of it is a stray white box.
        //
        assert_eq!(hardware_cursor(&a, area), None);
    }

    #[test]
    fn both_cursors_are_present_under_command_line_focus() {
        let mut a = app();
        a.set_focus(Focus::CommandLine);
        let area = Rect::new(0, 0, 100, 30);
        let buf = render(&a, 100, 30);
        let t = &a.theme;
        let depth = a.color_depth;
        let all = styles(&buf);

        // The active panel keeps its bar, now in the unfocused style.
        let unfocused_bg = t.quantize(t.panel.cursor_bg_unfocused, depth);
        assert!(
            all.iter().any(|(_, bg)| *bg == unfocused_bg),
            "the active panel's bar vanished when the command line took focus"
        );
        let inactive_bg = t.quantize(t.panel.inactive_cursor_bg, depth);
        assert!(
            all.iter().any(|(_, bg)| *bg == inactive_bg),
            "the inactive panel's cursor bar is missing"
        );
        // The hardware cursor moved to the command line.
        let (_, y) = hardware_cursor(&a, area).expect("hardware cursor");
        assert_eq!(y, layout(&a, area).cmdline.y);
    }

    #[test]
    fn the_hardware_cursor_belongs_to_the_command_line_alone() {
        // it is shown where the command line has focus, and
        // hidden where a panel does - the bar is that region's cursor, and the
        // terminal's block on top of it is redundant.
        let mut a = app();
        for focus in [Focus::Panel(Side::Left), Focus::Panel(Side::Right)] {
            a.set_focus(focus);
            assert_eq!(
                hardware_cursor(&a, Rect::new(0, 0, 80, 24)),
                None,
                "{focus:?}"
            );
        }
        a.set_focus(Focus::CommandLine);
        assert!(hardware_cursor(&a, Rect::new(0, 0, 80, 24)).is_some());
    }

    #[test]
    fn the_panel_cursor_bar_lands_on_the_right_entry_row() {
        // The panel's cursor is the *bar*, not the terminal's block, so this is
        // asserted on the rendered cells.
        let mut a = app();
        a.move_cursor_to(2);
        let area = Rect::new(0, 0, 80, 24);
        let buf = render(&a, 80, 24);
        let l = layout(&a, area);
        let entries = panelview::rows(l.left, false).entries;
        let want = a.theme.quantize(a.theme.panel.cursor_bg, a.color_depth);
        let row = (entries.y..entries.bottom()).find(|y| {
            (entries.x..entries.right()).all(|x| buf.cell((x, *y)).is_some_and(|c| c.bg == want))
        });
        assert_eq!(
            row,
            Some(entries.y.saturating_add(2)),
            "the bar is on the third entry row"
        );
    }

    #[test]
    fn the_hardware_cursor_is_never_hidden_even_when_the_terminal_is_too_small() {
        let a = app();
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| draw(f, &a)).expect("draw");
        let pos = terminal.get_cursor_position().expect("a cursor position");
        assert!(pos.x < 30 && pos.y < 8, "{pos:?}");
    }

    #[test]
    fn a_tiny_terminal_has_no_hardware_cursor_and_does_not_panic() {
        let a = app();
        assert_eq!(hardware_cursor(&a, Rect::new(0, 0, 1, 1)), None);
        assert_eq!(hardware_cursor(&a, Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn the_cursor_stays_inside_the_command_line_however_long_the_text() {
        let mut a = app();
        a.set_focus(Focus::CommandLine);
        a.cmdline.set_text("x".repeat(500));
        a.cmdline.move_end();
        let area = Rect::new(0, 0, 80, 24);
        let (x, _) = hardware_cursor(&a, area).expect("caret");
        assert!(x < 80, "x = {x}");
        let _ = render(&a, 80, 24);
    }

    // ---------------------------------------------------------------- rows ---

    #[test]
    fn the_layout_covers_the_screen_without_overlapping() {
        let a = app();
        let area = Rect::new(0, 0, 120, 40);
        let l = layout(&a, area);
        assert_eq!(l.left.height, l.right.height);
        assert_eq!(l.left.x, 0);
        assert_eq!(l.left.right(), l.right.x);
        assert_eq!(l.right.right(), 120);
        assert_eq!(l.cmdline.height, 1);
        assert_eq!(l.keybar.height, 1, "show_keybar defaults to true");
        assert_eq!(l.menubar.height, 0, "show_menubar defaults to false");
    }

    #[test]
    fn the_menu_bar_is_hidden_by_default_and_permanent_when_asked_for() {
        let mut a = app();
        assert!(!dump(&render(&a, 100, 30)).contains("Configuration"));
        a.config.ui.show_menubar = true;
        let out = dump(&render(&a, 100, 30));
        assert!(out.contains("Files"), "{out}");
        assert!(out.contains("Configuration"), "{out}");
    }

    #[test]
    fn the_tab_bar_appears_only_with_more_than_one_tab() {
        let mut a = app();
        assert!(!dump(&render(&a, 100, 30)).contains("2 tmp"));
        a.left.open_tab(VfsPath::local("/tmp"), 9);
        let out = dump(&render(&a, 100, 30));
        assert!(out.contains("2 tmp"), "{out}");
    }

    #[test]
    fn the_top_border_carries_both_the_path_and_the_volume_line() {
        // one row, not two. The path is left-aligned and wins the
        // space it needs; the volume line takes the right when it fits.
        let a = app();
        let out = dump(&render(&a, 120, 30));
        let first = out.lines().next().unwrap_or_default().to_string();
        assert!(
            first.contains("/home/thorin/Development"),
            "the top border names the directory: {first}"
        );
        assert!(
            first.contains("free") || first.contains("[_none_]"),
            "and still carries a volume line at this width: {first}"
        );
        // Named exactly once in the panel chrome - there is no separate path
        // row any more. The command-line prompt names it too, legitimately, so
        // the last two rows are excluded.
        let rows: Vec<&str> = out.lines().collect();
        let chrome = rows.len().saturating_sub(2);
        let path_rows = rows
            .iter()
            .take(chrome)
            .filter(|l| l.contains("/home/thorin/Development"))
            .count();
        assert_eq!(path_rows, 1, "one path row, not two:\n{out}");
    }

    #[test]
    fn a_narrow_panel_keeps_the_path_and_drops_the_volume_line() {
        // Free space is a nicety; a panel that cannot say which directory it is
        // showing is useless. At 60 columns the path survives.
        let a = app();
        let out = dump(&render(&a, 60, 15));
        let first = out.lines().next().unwrap_or_default().to_string();
        assert!(
            first.contains("Development") || first.contains("\u{2026}"),
            "the path is still there, cropped if need be: {first}"
        );
    }

    #[test]
    fn the_sort_arrow_is_prefixed_to_the_header_and_tagged_only_when_hidden() {
        let mut a = app();
        a.config.ui.ascii_borders = true;

        // Wide: `name` is drawn, so the arrow is on its header and the status
        // tag stays out of the way.
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("^Name"), "prefixed, not appended:\n{out}");
        assert!(!out.contains("Name^"), "never appended:\n{out}");
        assert!(
            !out.contains("[name ^]"),
            "the tag is redundant beside a visible arrow:\n{out}"
        );

        // Sort by a column narrow enough to be dropped, and the tag is the only
        // thing left that explains the order.
        a.sort_active(SortKey::Column(ColumnId::Size));
        let out = dump(&render(&a, 60, 15));
        assert!(
            !out.contains("Size"),
            "`size` should be hidden at this width:\n{out}"
        );
        assert!(
            out.contains("[size ^]"),
            "a hidden sorted column is what the tag is for:\n{out}"
        );

        // `unsorted` has no header arrow anywhere, so its tag always shows.
        a.sort_active(SortKey::Unsorted);
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("[unsorted]"), "{out}");
    }

    #[test]
    fn the_status_line_shows_the_counts_and_then_its_overrides() {
        let mut a = app();
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("in 2 files, 1 dir"), "\n{out}");

        // `und` is all lowercase, so smart case is *insensitive* and the
        // marker is `[aa]`. the example is `search: Tho [Aa]`, and
        // `Tho` under smart case is sensitive - which is what fixes `[Aa]` to
        // the sensitive half.
        a.active_panel_mut().quick.buffer = "und".to_string();
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("search: und [aa]"), "{out}");

        a.active_panel_mut().quick.buffer = "Und".to_string();
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("search: Und [Aa]"), "{out}");

        a.message = Some("View file: not implemented until v0.4".to_string());
        let out = dump(&render(&a, 120, 30));
        assert!(out.contains("not implemented until v0.4"), "{out}");
    }

    #[test]
    fn a_cropped_name_does_not_displace_the_panel_counts() {
        let mut a = app();
        a.move_cursor_to(3);
        let out = dump(&render(&a, 60, 15));
        let lines: Vec<&str> = out.lines().collect();
        // The entry row crops the name in the middle, which is what keeps the
        // extension. The row is found by the head of the name, which survives
        // either crop direction - exactly where a middle crop splits is
        // `panel::text`'s business, not this test's.
        let row = lines
            .iter()
            .find(|l| l.starts_with("\u{2502}a-really-quite"))
            .copied()
            .unwrap_or_default();
        assert!(row.contains('\u{2026}'), "the row is cropped: {row}");
        assert!(
            row.contains(".txt"),
            "a middle crop keeps the extension: {row}"
        );
        // And the status line is not where the rest of it goes. It used to be,
        // and a long name then filled the line end to end and pushed out the
        // one thing only that line says. `Shift+F9` is where a full name is
        // read, on a box built to hold one.
        // Both panels' status lines sit side by side on one rendered row, so
        // this counts occurrences rather than rows.
        let counts = out.matches(" in 2 files, 1 dir").count();
        assert_eq!(
            counts, 2,
            "both panels keep their counts with the cursor on a long name:\n{out}"
        );
        assert!(
            !out.contains("\u{2502}a-really-quite-extr"),
            "the status line has been taken over by the filename:\n{out}"
        );
    }

    // -------------------------------------------------------------- keybar ---

    #[test]
    #[ignore = "visual aid: cargo test -- --ignored --nocapture visual_dump"]
    fn visual_dump() {
        let mut a = app();
        a.left.open_tab(VfsPath::local("/tmp"), 9);
        for (w, h) in [(100u16, 24u16), (60, 15), (40, 10)] {
            println!("--- {w}x{h} ---\n{}", dump(&render(&a, w, h)));
        }
    }

    /// A blank slot promises the key does nothing. It has to be true.
    ///
    /// `Shift+F9` was bound to the file information dialog while this table
    /// drew `F9` blank under `Shift`, so holding the modifier to find out what
    /// the key did answered "nothing" about a key that did something. The
    /// tables and the keymap are two statements of the same fact, and this is
    /// what keeps them one.
    #[test]
    fn no_key_bar_slot_is_blank_while_the_key_is_bound() {
        use crate::config::keymap::{KeyContext, Resolution};
        use crate::input::{KeyCode, KeyPress};
        let keymap = crate::config::Keymap::default();
        for (mods, table) in [
            (KeyModifiers::SHIFT, KEYBAR_SHIFT),
            (KeyModifiers::ALT, KEYBAR_ALT),
            (KeyModifiers::CONTROL, KEYBAR_CTRL),
        ] {
            for (n, (key, label)) in table.iter().enumerate() {
                let code = KeyCode::F(u8::try_from(n.saturating_add(1)).unwrap_or(1));
                let press = KeyPress::new(code, mods);
                let bound = keymap.resolve(KeyContext::Panel, press);
                if label.is_empty() {
                    assert!(
                        matches!(bound, Resolution::Unbound),
                        "{key} is blank under {mods:?} but resolves to {bound:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_key_bar_is_the_total_commander_labels() {
        let a = app();
        let out = dump(&render(&a, 200, 50));
        let bar = out
            .lines()
            .nth(49)
            .unwrap_or_default()
            .trim_end()
            .to_string();
        // The seven labels, spread evenly across the width - the reference
        // screenshot's bar spans the whole bottom row rather than packing to
        // the left, and even slots are what stop the bar shuffling when a
        // modifier goes down (see `keybar_slots`).
        for (key, label) in KEYBAR {
            assert!(bar.contains(key), "{key} missing from {bar:?}");
            assert!(bar.contains(label), "{label} missing from {bar:?}");
        }
        let starts: Vec<usize> = KEYBAR
            .iter()
            .filter_map(|(k, _)| bar.find(&format!(" {k} ")))
            .collect();
        assert_eq!(starts.len(), KEYBAR.len(), "all located in {bar:?}");
        let gaps: Vec<usize> = starts.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.windows(2).all(|g| g[0] == g[1]),
            "evenly spaced, got gaps {gaps:?} in {bar:?}"
        );
    }

    #[test]
    #[ignore = "prints the degraded key bars for eyeballing; run with --ignored"]
    fn dump_the_key_bars() {
        for w in [60usize, 70, 80, 100, 120, 200] {
            for (name, mods) in [
                ("none", KeyModifiers::NONE),
                ("shift", KeyModifiers::SHIFT),
                ("alt", KeyModifiers::ALT),
                ("ctrl", KeyModifiers::CONTROL),
            ] {
                let bar: String = keybar_slots(mods, w, "\u{2026}")
                    .into_iter()
                    .map(|(k, l)| format!("{k}{l}"))
                    .collect();
                println!("{w:3} {name:5} |{bar}|");
            }
        }
    }

    #[test]
    fn the_key_bar_keeps_all_seven_slots_at_the_minimum_size() {
        // the design lists seven labels and the design declares 60x15 usable.
        // The full bar is 74 columns, so at 60 it degrades - but every slot
        // survives, Alt+F4 included: it is the only on-screen hint of how to
        // leave the program.
        let a = app();
        for w in [60u16, 70, 80, 100, 200] {
            let out = dump(&render(&a, w, 15));
            let bar = out.lines().nth(14).unwrap_or_default().to_string();
            for (key, _) in KEYBAR {
                assert!(bar.contains(key), "{w} columns: {key} is missing: {bar:?}");
            }
            assert!(
                text::width(bar.trim_end()) <= usize::from(w),
                "{w} columns: the bar overflows: {bar:?}"
            );
        }
        // The modified bars are wider still and degrade the same way.
        for w in [60u16, 100, 200] {
            for mods in [
                KeyModifiers::SHIFT,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ] {
                let slots = keybar_slots(mods, usize::from(w), "\u{2026}");
                assert_eq!(slots.len(), KEYBAR_SHIFT.len());
                let used: usize = slots
                    .iter()
                    .map(|(k, l)| text::width(k) + text::width(l))
                    .sum();
                assert!(
                    used <= usize::from(w),
                    "{w} columns, {mods:?}: the bar needs {used}"
                );
            }
        }
    }

    #[test]
    fn every_label_in_every_layer_fits_the_cap() {
        // Uniform lengths are what keep the slots looking alike as the modifier
        // changes; an over-long one would be cropped at render time instead of
        // abbreviated by someone who thought about it.
        for table in [KEYBAR, KEYBAR_SHIFT, KEYBAR_ALT, KEYBAR_CTRL] {
            for (key, label) in table {
                assert!(
                    text::width(label) <= KEYBAR_LABEL_MAX,
                    "{key} {label:?} is {} cells, over the {KEYBAR_LABEL_MAX} cap",
                    text::width(label)
                );
            }
        }
    }

    #[test]
    fn the_key_bar_slots_never_move_when_the_modifier_changes() {
        // The flicker: slot boundaries used to come from label lengths, so the
        // whole bar shuffled every time a modifier went down or came up.
        for w in [60usize, 70, 80, 100, 120, 200] {
            let mut boundaries: Option<Vec<usize>> = None;
            for mods in [
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ] {
                let slots = keybar_slots(mods, w, "\u{2026}");
                assert_eq!(
                    slots.len(),
                    KEYBAR.len(),
                    "{w} cols, {mods:?}: always the same count"
                );
                // Where each slot starts, accumulated left to right.
                let mut at = 0usize;
                let starts: Vec<usize> = slots
                    .iter()
                    .map(|(k, l)| {
                        let here = at;
                        at += text::width(k) + text::width(l);
                        here
                    })
                    .collect();
                match &boundaries {
                    None => boundaries = Some(starts),
                    Some(first) => assert_eq!(&starts, first, "{w} cols: {mods:?} moved the slots"),
                }
            }
        }
    }

    #[test]
    fn holding_a_modifier_swaps_the_labels() {
        assert_eq!(keybar_labels(KeyModifiers::NONE), KEYBAR);
        assert_eq!(keybar_labels(KeyModifiers::SHIFT), KEYBAR_SHIFT);
        assert_eq!(keybar_labels(KeyModifiers::ALT), KEYBAR_ALT);
        assert_eq!(keybar_labels(KeyModifiers::CONTROL), KEYBAR_CTRL);
        // Every layer shows the same ten keys; only the operation changes.
        for mods in [
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            let keys: Vec<&str> = keybar_labels(mods).iter().map(|(k, _)| *k).collect();
            assert_eq!(
                keys,
                ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10"],
                "{mods:?}"
            );
        }
        // the design binds no Shift+F7, so it keeps the unshifted operation.
        assert_eq!(keybar_labels(KeyModifiers::SHIFT)[6], ("F7", "NewFldr"));
        // ...and binds nothing to Shift+F1, which leaves the operation blank
        // while the key and its button stay.
        assert_eq!(keybar_labels(KeyModifiers::SHIFT)[0], ("F1", ""));
        // Precedence is fixed, so a combination resolves rather than flickers.
        assert_eq!(
            keybar_labels(KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            KEYBAR_SHIFT
        );
        assert_eq!(
            keybar_labels(KeyModifiers::ALT | KeyModifiers::CONTROL),
            KEYBAR_ALT
        );
    }

    // --------------------------------------------------------------- ascii ---

    #[test]
    fn ascii_borders_emit_no_non_ascii_glyph_anywhere() {
        let mut a = app();
        a.config.ui.ascii_borders = true;
        a.left.open_tab(VfsPath::local("/tmp"), 9);
        a.config.ui.show_menubar = true;
        a.active_panel_mut().filter_mask = "*.rs".to_string();
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            let out = dump(&render(&a, w, h));
            assert!(out.is_ascii(), "{w}x{h} emitted a non-ASCII glyph:\n{out}");
            assert!(out.contains('+') && out.contains('|'), "{out}");
        }
    }

    #[test]
    fn the_unicode_border_is_box_drawing() {
        let a = app();
        let out = dump(&render(&a, 80, 24));
        assert!(out.contains('\u{2502}'), "vertical border:\n{out}");
        assert!(out.contains('\u{2514}'), "bottom-left corner:\n{out}");
        assert!(out.contains('\u{251C}'), "the status rule's tee:\n{out}");
    }

    // ------------------------------------------------------------- theming ---

    #[test]
    fn every_depth_renders_and_quantizes() {
        let mut a = app();
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Indexed256,
            ColorDepth::Ansi16,
        ] {
            a.color_depth = depth;
            let buf = render(&a, 100, 30);
            let seen = styles(&buf);
            match depth {
                ColorDepth::TrueColor => {
                    assert!(seen.iter().any(|(fg, _)| matches!(fg, Color::Rgb(..))));
                }
                ColorDepth::Indexed256 => {
                    assert!(seen.iter().any(|(fg, _)| matches!(fg, Color::Indexed(_))));
                    assert!(!seen.iter().any(|(fg, _)| matches!(fg, Color::Rgb(..))));
                }
                ColorDepth::Ansi16 => {
                    assert!(!seen.iter().any(|(fg, _)| matches!(fg, Color::Rgb(..))));
                    assert!(!seen.iter().any(|(fg, _)| matches!(fg, Color::Indexed(_))));
                }
            }
        }
    }

    #[test]
    fn a_deep_path_never_squeezes_the_command_line_off_the_screen() {
        // the design declares 60x15 usable; the design makes the command line
        // always present and the design makes the caret always meaningful.
        // A prompt wider than the terminal must not take the typing with it.
        let mut a = app();
        let deep = "/home/thorin/Development/github/holoscommander/src/ui/widgets/panel";
        for side in [Side::Left, Side::Right] {
            a.panel_mut(side).active_tab_mut().path = VfsPath::local(deep);
        }
        a.set_focus(Focus::CommandLine);
        a.cmdline.set_text("cp file.txt /tmp");
        a.cmdline.move_end();

        for (w, h) in [(60u16, 15u16), (80, 24), (200, 50)] {
            let buf = render(&a, w, h);
            let out = dump(&buf);
            let row = out
                .lines()
                .nth(usize::from(h).saturating_sub(2))
                .unwrap_or_default()
                .to_string();
            // The tail of the line, where the caret is, is always on screen -
            // at 60 columns the input region is MIN_CMDLINE_INPUT wide and the
            // text scrolls within it.
            assert!(
                row.contains("/tmp"),
                "{w}x{h}: the typed command is not on screen: {row:?}"
            );
            // The prompt keeps its tail - the deepest directory - and says it
            // was cropped.
            assert!(
                row.contains("panel> "),
                "{w}x{h}: the prompt lost its informative end: {row:?}"
            );
            // The hardware cursor lands in the input region, not on some
            // character of the path.
            let area = Rect::new(0, 0, w, h);
            let l = layout(&a, area);
            let (caret_x, prompt_w, _) = cmdline::geometry(&a, l.cmdline);
            assert!(
                usize::from(caret_x) >= prompt_w,
                "{w}x{h}: the caret is inside the prompt"
            );
            assert!(
                usize::from(w).saturating_sub(prompt_w)
                    >= cmdline::MIN_CMDLINE_INPUT.min(usize::from(w)),
                "{w}x{h}: the editable region was squeezed out"
            );
        }
    }

    #[test]
    fn nine_tabs_still_leave_the_active_one_drawn_and_highlighted() {
        // the design (tab bar row) and 5.5 (nine tabs per panel). Dividing the
        // bar without paying for the separators first pushed the last tab off
        // the end, and the last tab is the active one right after Ctrl+T.
        use ratatui::style::Modifier;
        let mut a = app();
        for i in 1..crate::panel::MAX_TABS {
            assert!(
                a.left
                    .open_tab(VfsPath::local(format!("/dir{i}")), crate::panel::MAX_TABS),
                "tab {i}"
            );
        }
        assert_eq!(a.left.tab_count(), crate::panel::MAX_TABS);
        assert_eq!(a.left.active_index(), crate::panel::MAX_TABS - 1);

        for (w, h) in [(100u16, 20u16), (60, 15), (200, 50)] {
            let buf = render(&a, w, h);
            let bold = buf
                .content()
                .iter()
                .filter(|c| c.modifier.contains(Modifier::BOLD))
                .count();
            assert!(
                bold > 0,
                "{w}x{h}: no tab is highlighted, so the active one was dropped"
            );
            let out = dump(&buf);
            assert!(
                out.contains(&format!("{} ", crate::panel::MAX_TABS)),
                "{w}x{h}: tab {} is missing from the bar:\n{out}",
                crate::panel::MAX_TABS
            );
        }
    }

    #[test]
    fn a_marked_entry_still_shows_its_mark_under_the_cursor() {
        // There is no mark glyph, so a mark under the cursor needs its own
        // indicator - a cursor row that showed nothing made a marked entry
        // indistinguishable from an unmarked one. The bar's BACKGROUND stays
        // `cursor_bg`; only the foreground changes, to a dark accent of the mark
        // colour that reads on the bar in every theme.
        let mut a = app();
        let slot = |c: crate::config::Rgb| Color::from_u32(u32::from_be_bytes([0, c.r, c.g, c.b]));
        let cursor_bg = slot(a.theme.panel.cursor_bg);
        let accent = slot(crate::ui::panelview::dark_mark_accent(
            a.theme.panel.marked_fg,
            a.theme.panel.cursor_bg,
        ));
        {
            let tab = a.left.active_tab_mut();
            tab.move_to(1, 10);
            assert!(tab.toggle_mark(), "the row under the cursor is markable");
        }
        let buf = render(&a, 100, 20);
        let marked_cursor_cells = buf
            .content()
            .iter()
            .filter(|c| c.bg == cursor_bg && c.fg == accent)
            .count();
        assert!(
            marked_cursor_cells > 0,
            "the marked entry under the cursor lost its dark mark accent on the unchanged bar"
        );
    }

    /// "a 16-color session must still be legible". Every pair of
    /// slots this renderer actually puts together must stay distinguishable
    /// after quantization.
    #[test]
    fn no_slot_pair_used_together_collapses_at_sixteen_colours() {
        let t = Theme::blue();
        let q = |rgb: Rgb| t.quantize(rgb, ColorDepth::Ansi16);

        let p = &t.panel;
        let c = &t.cmdline;
        let k = &t.keybar;
        let pairs: [(&str, Rgb, Rgb); 16] = [
            ("panel.fg on panel.bg", p.fg, p.bg),
            ("panel.dir_fg on panel.bg", p.dir_fg, p.bg),
            ("panel.exec_fg on panel.bg", p.exec_fg, p.bg),
            ("panel.link_fg on panel.bg", p.link_fg, p.bg),
            ("panel.archive_fg on panel.bg", p.archive_fg, p.bg),
            ("panel.marked_fg on panel.bg", p.marked_fg, p.bg),
            ("panel.border on panel.bg", p.border, p.bg),
            ("panel.inactive_border on panel.bg", p.inactive_border, p.bg),
            (
                "panel.header_fg on panel.header_bg",
                p.header_fg,
                p.header_bg,
            ),
            (
                "panel.status_fg on panel.status_bg",
                p.status_fg,
                p.status_bg,
            ),
            ("cursor_fg on cursor_bg", p.cursor_fg, p.cursor_bg),
            (
                "cursor_fg_unfocused on cursor_bg_unfocused",
                p.cursor_fg_unfocused,
                p.cursor_bg_unfocused,
            ),
            (
                "inactive_cursor_fg on inactive_cursor_bg",
                p.inactive_cursor_fg,
                p.inactive_cursor_bg,
            ),
            ("cmdline.fg on cmdline.bg", c.fg, c.bg),
            ("cmdline.prompt_fg on cmdline.bg", c.prompt_fg, c.bg),
            ("keybar.label_fg on keybar.label_bg", k.label_fg, k.label_bg),
        ];
        for (what, fg, bg) in pairs {
            assert_ne!(
                q(fg),
                q(bg),
                "{what} quantizes to one colour at 16: {fg} / {bg}"
            );
        }

        // The painted caret is drawn as caret_unfocused against cmdline.bg.
        assert_ne!(q(c.caret_unfocused), q(c.bg));
        // The three cursor styles must stay three styles at 16 colours, and
        // none of them may be the panel background - a bar the same colour as
        // the rows around it is not a bar ("Neither cursor ever
        // disappears", and the inactive one is "visible enough to show where
        // you left off").
        let bars = [
            ("cursor_bg", q(p.cursor_bg)),
            ("cursor_bg_unfocused", q(p.cursor_bg_unfocused)),
            ("inactive_cursor_bg", q(p.inactive_cursor_bg)),
        ];
        for (name, bar) in bars {
            assert_ne!(bar, q(p.bg), "{name} is panel.bg at 16 colours");
        }
        for (i, (a_name, a)) in bars.iter().enumerate() {
            for (b_name, b) in bars.iter().skip(i + 1) {
                assert_ne!(a, b, "{a_name} and {b_name} collapsed at 16 colours");
            }
        }
        // And the text on each bar has to stay legible against it.
        for (fg, bg, what) in [
            (p.cursor_fg, p.cursor_bg, "cursor"),
            (
                p.cursor_fg_unfocused,
                p.cursor_bg_unfocused,
                "unfocused cursor",
            ),
            (
                p.inactive_cursor_fg,
                p.inactive_cursor_bg,
                "inactive cursor",
            ),
            (p.marked_fg, p.cursor_bg, "marked entry under the cursor"),
        ] {
            assert_ne!(q(fg), q(bg), "{what} bar is unreadable at 16 colours");
        }
    }

    #[test]
    fn the_activity_indicator_sits_on_the_bottom_border_not_the_volume_line() {
        // Twice reported drawn over the top border's free-space figure; it
        // belongs on the bottom border of the right panel, right end, clear of
        // the corner. This pins the row so it cannot drift back up.
        use crate::ops::JobSpec;
        let mut a = app();
        a.request_job(JobSpec::size(vec![VfsPath::local("/etc")]));
        let buf = render(&a, 100, 30);
        let area = *buf.area();

        let right_half = |y: u16| -> String {
            (area.right().saturating_sub(20)..area.right())
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect()
        };
        let row_with_bracket = (area.y..area.bottom())
            .find(|&y| right_half(y).contains('['))
            .expect("the indicator is drawn somewhere on the right");

        // The volume line is the top border of the panel box: it carries the
        // free-space figure, and the indicator must not be on it.
        let top = right_half(area.y);
        assert!(
            top.contains("free"),
            "the top border carries the volume line"
        );
        assert!(
            !top.contains('['),
            "the indicator bled onto the volume line: |{top}|"
        );
        // The bracket's row is a horizontal border, not a text line.
        let bracket_row = right_half(row_with_bracket);
        assert!(
            !bracket_row.contains("free"),
            "the indicator landed on the volume line: |{bracket_row}|"
        );
        assert!(
            bracket_row.contains(']'),
            "the closing bracket is drawn, not bled off: |{bracket_row}|"
        );
        // The animation between the brackets is ASCII on purpose: an
        // ambiguous-width glyph would render two cells wide in some terminals
        // and shove the closing bracket off its column.
        let start = bracket_row.find('[').expect("an opening bracket");
        let end = bracket_row[start..].find(']').expect("a closing bracket") + start;
        let inside = &bracket_row[start + 1..end];
        assert!(
            inside.is_ascii(),
            "the indicator animation must be ASCII, not |{inside}|"
        );
    }

    #[test]
    fn the_blue_theme_is_legible_at_sixteen_colours_on_screen() {
        let mut a = app();
        a.color_depth = ColorDepth::Ansi16;
        let buf = render(&a, 100, 30);
        for cell in buf.content() {
            if cell.symbol().trim().is_empty() {
                continue;
            }
            assert_ne!(
                cell.fg,
                cell.bg,
                "an invisible cell at 16 colours: {:?}",
                cell.symbol()
            );
        }
    }
}
