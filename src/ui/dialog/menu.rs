//! the menu bar, and the dropdown `F9` opens under it.
//!
//! > `F9` drops the menu bar of the layout: **Files, Mark, Commands, Net,
//! > Show, Configuration**. `Alt` with the underlined letter opens one
//! > directly, the same mnemonic rule as the dialogs, and the arrow keys
//! > walk them.
//!
//! ```text
//!  Files  Mark  Commands  Net  Show  Configuration
//! +-------------------------------------------+
//! | View the file under the cursor         F3 |
//! | Edit the file under the cursor         F4 |
//! | Copy the selection to the other panel  F5 |
//! +-------------------------------------------+
//! ```
//!
//! # Every row is generated, so no row can go stale
//!
//! > Every item on it is a key that already exists, and the menu **shows that
//! > key** beside the item. That is the menu's real job here: a file manager
//! > whose whole vocabulary is function keys needs somewhere to discover them,
//! > and a menu that listed things reachable only from the menu would be a
//! > second, weaker interface to maintain.
//!
//! So a [`MenuItem`] cannot be built without an [`crate::input::Action`]: its
//! `keys` are read from the **live** keymap through
//! [`crate::config::Keymap::describe`] and its label from
//! [`crate::input::Action::description`], which is the same string the `F1`
//! reference prints. A user who rebinds `F5` sees their own binding, and a
//! row whose wording is edited is edited in one place.
//!
//! The `Show` menu goes further and is named from `panel.columns.order`, so
//! `Sort by Size  Ctrl+3` appears only where size is the third column.
//! That is the same generation rule the page
//! follows and the reason neither can drift.
//!
//! > If an item has no key, it is because it has not been built yet, and that
//! > is a bug in the menu rather than a design.
//!
//! That sentence is a test - `every_item_has_a_key_and_is_implemented` below,
//! which is the invariant I11 - and not a comment.
//!
//! # It is a dialog
//!
//! > `Esc` closes it and gives the panel back. The menu takes focus while it
//! > is open, so the "the viewer consumes all input" applies to it too.
//!
//! That is exactly what [`crate::dialog::Dialog`] is, so this is one: the
//! focus save and restore, consume-all-input, `Esc`, the theming and the
//! mnemonic census all come for free, and the `Alt`+letter rule becomes
//! the rule with no second implementation.
//!
//!
//! # Where the box lands
//!
//! The dialog asks for the full width, draws the six titles across the first
//! row of its own interior and hangs the dropdown under the open one. It
//! declares no [`crate::dialog::Dialog::anchor`],
//! so [`crate::dialog::centred`] places the box
//! itself and the framework keeps the one placement rule
//! the design gave it. `ui.show_menubar`'s permanent bar is drawn
//! by [`crate::ui::draw_menubar`] and is a different surface: this one is the
//! bar that has focus, and it carries the same six titles from the same
//! [`crate::ui::MENUBAR`] so the two cannot disagree.
//!
//! # Colour and glyphs
//!
//! No new theme slot. The open title and the
//! selected row are [`DialogStyle::row_cursor`], which is the panel's cursor
//! bar, and the dropdown's frame is the dialog border. Its two glyphs - the
//! box and the scroll arrows - both have ASCII forms under `ui.ascii_borders`
//! and both come from [`crate::ui::text::Glyphs`], so there is one place to
//! audit.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{ellipsis, row};
use crate::config::KeyContext;
use crate::dialog::{
    Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_text, split_mnemonic,
};
use crate::input::{Action, DialogId, KeyCode};
use crate::panel::ColumnId;
use crate::ui::text::{self, Crop, Glyphs};

/// The underlined letter of each of the six titles, in
/// [`crate::ui::MENUBAR`]'s order (the rule applied to the
/// titles).
///
/// `Show` is `h` and not `s` because `alt+s` is already `search`'s legacy
/// fallback for `alt+f7`, and the requirement that every
/// undeliverable key carry one outranks a mnemonic preference.
/// `Configuration` is `o` because `c` is
/// `Commands`.
///
/// Public because [`crate::ui::draw_menubar`] underlines the same six letters
/// on the permanent bar, and two tables would be one table too many.
pub const LETTERS: [char; 6] = ['f', 'm', 'c', 'n', 'h', 'o'];

/// The gap between a row's label and the key that runs it.
const GAP: usize = 2;

/// The narrowest a label is drawn beside its key.
///
/// Below this the key is dropped and the whole row is the label: `C...  F5`
/// teaches neither half, and the label is the half that says which row this is.
const MIN_LABEL: usize = 8;

/// How many rows the dropdown asks for at most, before the terminal's own
/// height has its say.
///
/// The `Show` menu is as long as the column layout makes it, and a dialog that
/// asked for fifty rows would be a full-screen box on a tall terminal for the
/// sake of one menu. Anything past this scrolls.
const MAX_ROWS: usize = 20;

/// the `Files` menu.
const FILES: [Action; 15] = [
    Action::View,
    Action::Edit,
    Action::ViewSingle,
    Action::EditNew,
    Action::Copy,
    Action::Move,
    Action::RenameInPlace,
    Action::Mkdir,
    Action::Delete,
    Action::DeletePermanent,
    Action::Pack,
    Action::Unpack,
    Action::OpenWith,
    Action::ContextMenu,
    Action::Quit,
];

/// the `Mark` menu: the marking, the compare and the clipboard, which
/// are the operations that act on a selection.
const MARK: [Action; 9] = [
    Action::SelectAll,
    Action::InvertSelection,
    Action::SelectMask,
    Action::UnselectMask,
    Action::CompareDirs,
    Action::DirSize,
    Action::ClipboardCopy,
    Action::ClipboardCut,
    Action::ClipboardPaste,
];

/// the `Commands` menu.
const COMMANDS: [Action; 13] = [
    Action::Search,
    Action::SearchInPanel,
    Action::BranchView,
    Action::MultiRename,
    Action::RenameResult,
    Action::Hotlist,
    Action::HotlistAdd,
    Action::GotoPath,
    Action::SwapPanels,
    Action::QuickView,
    Action::ConsoleToggle,
    Action::HistoryDialog,
    Action::JobQueue,
];

/// the `Net` menu.
///
/// One item, and that is correct rather than thin: `Ctrl+F` is the whole of
/// the entry point, and inventing a second row to fill the menu would be
/// the "second, weaker interface" the design rules out.
///
const NET: [Action; 1] = [Action::ConnectToggle];

/// The `Show` menu's rows below the sort block: the fixed-field
/// sorts, `show_hidden` and the re-read.
const SHOW_TAIL: [Action; 7] = [
    Action::SortByName,
    Action::SortByExt,
    Action::SortByDate,
    Action::SortBySize,
    Action::SortUnsorted,
    Action::ShowHidden,
    Action::Reread,
];

/// the `Configuration` menu.
///
/// `edit_config` opens `config.toml` in the external editor of the design and
/// re-reads it on exit, which is the "opens the settings that the design
/// keeps in `config.toml`" read literally rather than a settings editor the
/// design describes no control of.
const CONFIGURATION: [Action; 3] = [Action::EditConfig, Action::ReloadConfig, Action::Help];

/// `Ctrl+<n>`, by column position.
const SORT_BY_COLUMN: [Action; 9] = [
    Action::SortByColumn1,
    Action::SortByColumn2,
    Action::SortByColumn3,
    Action::SortByColumn4,
    Action::SortByColumn5,
    Action::SortByColumn6,
    Action::SortByColumn7,
    Action::SortByColumn8,
    Action::SortByColumn9,
];

/// `Ctrl+Shift+<n>`, by column position.
const SORT_SECONDARY: [Action; 9] = [
    Action::SortSecondary1,
    Action::SortSecondary2,
    Action::SortSecondary3,
    Action::SortSecondary4,
    Action::SortSecondary5,
    Action::SortSecondary6,
    Action::SortSecondary7,
    Action::SortSecondary8,
    Action::SortSecondary9,
];

/// The key column of one menu row, from the live keymap.
///
/// [`crate::config::Keymap::describe`] and nothing of its own, so the design
/// the page and this menu cannot spell the same key two ways
/// - with one subtraction, which is why this
///   function exists rather than a bare call:
///
/// **the "no fallback binding" sentence is dropped here.** On a legacy
/// terminal `describe` ends a binding that cannot be delivered *and* has no
/// deliverable alternative with a pointer to the design, which is right on a
/// reference page and is thirty-eight columns of every `Ctrl+Shift+<n>` row on
/// a menu that is itself the route that pointer leads to: the design says
/// "on a legacy terminal the secondary sort is set from the sort menu (`F9`)
/// instead". The `(unavailable)` marker stays, because that is the part that
/// tells the reader why the key they can see is not working.
///
/// Public so [`crate::ui::dialog::context`]'s rows are spelled the same way.
pub fn keys_for(keymap: &crate::config::Keymap, action: Action, enhanced: bool) -> String {
    let text = keymap.describe(KeyContext::Panel, action, enhanced);
    match text.strip_suffix(crate::config::keymap::NO_FALLBACK) {
        Some(head) => head.trim_end_matches([' ', '/']).to_string(),
        None => text,
    }
}

/// One item on a menu.
///
/// > Every item on it is a key that already exists, and the menu **shows that
/// > key** beside the item.
///
/// So there is no way to build one without an [`Action`], and `keys` is read
/// from the **live** keymap rather than written here - a user who rebinds `F5`
/// sees their binding, exactly as the page does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// What the row says.
    pub label: String,
    /// What it does. Dispatched exactly as the key would be.
    pub action: Action,
    /// The key, rendered from the keymap: `F5`, `Ctrl+Shift+3`, `(unbound)`.
    pub keys: String,
}

impl MenuItem {
    /// One row, labelled by the action's own description.
    ///
    /// The description and not a second table of menu wording: the design's
    /// menu exists so the keyboard can be discovered, the page says
    /// the same thing about the same action, and two strings for one action
    /// are two strings to keep in step. The one exception is the `Show` menu's
    /// positional sorts, whose wording is the *column layout* rather than the
    /// action - see [`sort_item`].
    fn new(action: Action, keymap: &crate::config::Keymap, enhanced: bool) -> Self {
        Self {
            label: action.description().to_string(),
            action,
            keys: keys_for(keymap, action, enhanced),
        }
    }

    /// The same row with wording of its own, for a label the action cannot
    /// know: `Sort by Size` is a fact about `panel.columns.order`, not about
    /// `sort_by_column_3`.
    fn labelled(
        action: Action,
        label: String,
        keymap: &crate::config::Keymap,
        enhanced: bool,
    ) -> Self {
        Self {
            label,
            action,
            keys: keys_for(keymap, action, enhanced),
        }
    }

    /// The row as one string, label left and key right, in `width` columns.
    ///
    /// The key is never cropped and the label is: a row reading
    /// `Copy the selection to the other pa...  F5` still teaches `F5`, and one
    /// reading `Copy the selection to the other panel  F...` teaches nothing.
    /// Where there is not room for [`MIN_LABEL`] columns of label beside it,
    /// the key is dropped instead, because the label is the half that says
    /// which row this is.
    fn text(&self, width: usize, ascii: bool) -> String {
        let keys = text::width(&self.keys);
        let label = width.saturating_sub(keys.saturating_add(GAP));
        if label < MIN_LABEL {
            return text::fit_left(&self.label, width, Crop::End, ellipsis(ascii));
        }
        format!(
            "{}{}{}",
            text::fit_left(&self.label, label, Crop::End, ellipsis(ascii)),
            " ".repeat(GAP),
            self.keys
        )
    }

    /// The columns this row would like: label, gap, key.
    fn natural_width(&self) -> usize {
        text::width(&self.label)
            .saturating_add(GAP)
            .saturating_add(text::width(&self.keys))
    }
}

/// One menu (the six).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// `Files`, `Mark`, `Commands`, `Net`, `Show`, `Configuration`.
    pub title: &'static str,
    /// The underlined letter, folded to lower case (the rule
    /// applied to the titles). See the design for why `Show` is
    /// `h`.
    pub letter: char,
    /// Its items, in order.
    pub items: Vec<MenuItem>,
}

impl Menu {
    /// The widest row it would like to draw.
    fn natural_width(&self) -> usize {
        self.items
            .iter()
            .map(MenuItem::natural_width)
            .max()
            .unwrap_or(0)
            .max(text::width(self.title))
    }
}

/// The whole bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// Exactly six, in the order.
    pub menus: Vec<Menu>,
}

impl MenuModel {
    /// Which menu the `Alt`+letter names, if any.
    ///
    /// The letter is folded to lower case first, because
    /// [`crate::dialog::DialogKey::mnemonic`] already is and a terminal that
    /// reports `Alt+Shift+F` is describing the same keystroke as one that
    /// reports `Alt+F`.
    pub fn index_of_letter(&self, letter: char) -> Option<usize> {
        let want = letter.to_ascii_lowercase();
        self.menus.iter().position(|menu| menu.letter == want)
    }

    /// Every row of every menu, for the census and for tests.
    pub fn items(&self) -> impl Iterator<Item = &MenuItem> {
        self.menus.iter().flat_map(|menu| menu.items.iter())
    }
}

/// The name the `Show` menu gives one column's sort row.
///
/// `Sort by Size` rather than `Sort by the 3rd configured column`: the row is
/// about the layout the user can see, and the design makes the layout the
/// thing that decides what `Ctrl+3` does.
fn sort_item(
    action: Action,
    prefix: &str,
    column: ColumnId,
    keymap: &crate::config::Keymap,
    enhanced: bool,
) -> MenuItem {
    MenuItem::labelled(
        action,
        format!("{prefix} {}", column.header()),
        keymap,
        enhanced,
    )
}

/// The `Show` menu, which is the one menu whose length is a setting.
///
///
/// Its `sort_secondary_*` rows are drawn and dispatchable **even where the key
/// that names them is undeliverable**, because the design makes this menu
/// the way to reach them there: "on a legacy terminal the secondary sort is
/// set from the sort menu (`F9`) instead". The binding still exists and the
/// row still shows it, marked the way the page marks it - the
/// binding existing and the terminal being able to send it are different
/// things.
fn show_menu(order: &[ColumnId], keymap: &crate::config::Keymap, enhanced: bool) -> Vec<MenuItem> {
    let mut items = Vec::with_capacity(order.len().saturating_mul(2).saturating_add(8));
    for (column, action) in order.iter().zip(SORT_BY_COLUMN) {
        items.push(sort_item(action, "Sort by", *column, keymap, enhanced));
    }
    for (column, action) in order.iter().zip(SORT_SECONDARY) {
        items.push(sort_item(
            action,
            "Secondary sort by",
            *column,
            keymap,
            enhanced,
        ));
    }
    items.push(MenuItem::new(Action::SortSecondaryClear, keymap, enhanced));
    for action in SHOW_TAIL {
        items.push(MenuItem::new(action, keymap, enhanced));
    }
    items
}

/// Build the bar from the live keymap and the live column order.
///
///
/// `Show`'s sort entries are named from `panel.columns.order`, so
/// `Sort by Size  Ctrl+3` appears only where size is the third column - which
/// is the same generation rule the page follows and the reason
/// neither can go stale.
pub fn model(app: &crate::app::App) -> MenuModel {
    let keymap = &app.keymap;
    let enhanced = app.keyboard.enhanced;
    let fixed = |actions: &[Action]| -> Vec<MenuItem> {
        actions
            .iter()
            .map(|action| MenuItem::new(*action, keymap, enhanced))
            .collect()
    };
    let items: [Vec<MenuItem>; 6] = [
        fixed(&FILES),
        fixed(&MARK),
        fixed(&COMMANDS),
        fixed(&NET),
        show_menu(&app.config.panel.columns.order, keymap, enhanced),
        fixed(&CONFIGURATION),
    ];
    // Zipped rather than indexed: the titles are the own array and the
    // letters are the, and zipping is what makes a sixth title without a
    // letter impossible rather than merely caught.
    let menus = crate::ui::MENUBAR
        .iter()
        .copied()
        .zip(LETTERS)
        .zip(items)
        .map(|((title, letter), items)| Menu {
            title,
            letter,
            items,
        })
        .collect();
    MenuModel { menus }
}

/// the menu bar. A dialog, because the design says "The menu takes focus
/// while it is open, so the 'the viewer consumes all input' applies to it
/// too" - which is what [`crate::dialog::Dialog`] already is.
pub struct MenuDialog {
    model: MenuModel,
    open: usize,
    cursor: usize,
}

impl MenuDialog {
    /// `open` is which menu is dropped: `F9` opens the first, `Alt`+letter
    /// opens the one that letter names.
    ///
    /// An index past the end opens the last menu rather than none: the bar is
    /// always on screen while the dialog is, so there is no state in which no
    /// menu is dropped.
    pub fn new(model: MenuModel, open: usize) -> Self {
        let open = open.min(model.menus.len().saturating_sub(1));
        Self {
            model,
            open,
            cursor: 0,
        }
    }

    /// Which menu is dropped, for the renderer and for tests.
    pub const fn open_menu(&self) -> usize {
        self.open
    }

    /// Which item the cursor is on within it.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The bar this dialog is showing.
    pub const fn model(&self) -> &MenuModel {
        &self.model
    }

    /// The open menu, which is always one of the six.
    fn menu(&self) -> Option<&Menu> {
        self.model.menus.get(self.open)
    }

    /// The item under the cursor.
    fn selected(&self) -> Option<&MenuItem> {
        self.menu().and_then(|menu| menu.items.get(self.cursor))
    }

    /// Open another menu, wrapping in both directions, and start at its top.
    ///
    /// `Left` on the first title goes to the last, which is what a menu bar
    /// has always done and what stops `Left` from being a key that sometimes
    /// does nothing.
    fn walk_menus(&mut self, forward: bool) {
        let len = self.model.menus.len();
        if len == 0 {
            return;
        }
        self.open = if forward {
            self.open.saturating_add(1).rem_euclid(len)
        } else {
            self.open.checked_sub(1).unwrap_or(len.saturating_sub(1))
        };
        self.cursor = 0;
    }

    /// Move the cursor within the open menu, wrapping.
    fn walk_items(&mut self, delta: isize) {
        let Some(len) = self.menu().map(|menu| menu.items.len()) else {
            return;
        };
        if len == 0 {
            return;
        }
        let last = len.saturating_sub(1);
        self.cursor = match delta {
            -1 => self.cursor.checked_sub(1).unwrap_or(last),
            1 => {
                if self.cursor >= last {
                    0
                } else {
                    self.cursor.saturating_add(1)
                }
            }
            d if d < 0 => self.cursor.saturating_sub(d.unsigned_abs()),
            d => self.cursor.saturating_add(d.unsigned_abs()).min(last),
        };
    }

    /// Where each title sits on the bar: its first column and the cell drawn
    /// for it, which is the title with one space either side - the same cell
    /// [`crate::ui::draw_menubar`] draws, so the dropped menu lines up with
    /// the permanent bar underneath it.
    fn cells(&self) -> Vec<(usize, String)> {
        let mut out = Vec::with_capacity(self.model.menus.len());
        let mut at = 0usize;
        for menu in &self.model.menus {
            let cell = format!(" {} ", menu.title);
            let width = text::width(&cell);
            out.push((at, cell));
            at = at.saturating_add(width);
        }
        out
    }

    /// The rows of `items` visible in a window of `rows`, keeping the cursor
    /// inside it.
    ///
    /// A pure function of the cursor and the height rather than a remembered
    /// scroll offset: `Dialog::render` takes `&self`, and a menu that is
    /// walked with `Up` and `Down` has no state worth an interior-mutable cell
    /// (house style: restructure rather than reach for `Cell`).
    fn window(cursor: usize, len: usize, rows: usize) -> std::ops::Range<usize> {
        if rows == 0 || len == 0 {
            return 0..0;
        }
        if len <= rows {
            return 0..len;
        }
        let half = rows / 2;
        let start = cursor.saturating_sub(half).min(len.saturating_sub(rows));
        start..start.saturating_add(rows).min(len)
    }

    /// The six titles across one row, the open one on the cursor bar and every
    /// letter underlined.
    fn draw_bar(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut left = usize::from(area.width);
        for (index, ((_, cell), menu)) in self.cells().iter().zip(&self.model.menus).enumerate() {
            if left == 0 {
                break;
            }
            let cell = text::truncate(cell, left, Crop::End, ellipsis(style.ascii));
            left = left.saturating_sub(text::width(&cell));
            let row_style = if index == self.open {
                style.row_cursor(true)
            } else {
                style.body()
            };
            match split_mnemonic(&cell, menu.letter) {
                Some((head, letter, tail)) => {
                    spans.push(Span::styled(head.to_string(), row_style));
                    spans.push(Span::styled(
                        letter.to_string(),
                        row_style.add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(tail.to_string(), row_style));
                }
                None => spans.push(Span::styled(cell, row_style)),
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(style.body()), area);
    }

    /// Where the dropdown hangs, inside the interior `area`: under the open
    /// title, shifted left only as far as it must be to fit.
    fn dropdown_rect(&self, area: Rect) -> Option<Rect> {
        let menu = self.menu()?;
        if area.height <= 1 || area.width == 0 {
            return None;
        }
        let start = self.cells().get(self.open).map_or(0, |(start, _)| *start);
        let available = usize::from(area.width);
        // Two for the frame it is drawn in.
        let want = menu.natural_width().saturating_add(2).min(available);
        let x = start.min(available.saturating_sub(want));
        let rows = usize::from(area.height).saturating_sub(1);
        let height = menu
            .items
            .len()
            .min(MAX_ROWS)
            .saturating_add(2)
            .min(rows)
            .max(1);
        let rect = Rect::new(
            area.x
                .saturating_add(u16::try_from(x).unwrap_or(u16::MAX))
                .min(area.right()),
            area.y.saturating_add(1),
            u16::try_from(want).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
        );
        (rect.width > 0 && rect.height > 0).then_some(rect)
    }

    /// The dropdown: a frame when there is room for one, and the rows.
    fn draw_dropdown(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let Some(menu) = self.menu() else { return };
        let Some(rect) = self.dropdown_rect(area) else {
            return;
        };
        let glyphs = Glyphs::new(style.ascii);
        // A frame needs a cell of border on each side and one row of content
        // between them. Below that the rows are drawn bare, which is still a
        // usable menu on a terminal the design barely supports.
        let inner = if rect.width >= 4 && rect.height >= 3 {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_set(glyphs.border_set())
                .border_style(Style::new().fg(style.border).bg(style.bg))
                .style(Style::new().bg(style.bg));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            inner
        } else {
            rect
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let rows = usize::from(inner.height);
        let range = Self::window(self.cursor, menu.items.len(), rows);
        // One column is given up to the scroll markers only when there is
        // something off the top or the bottom to mark.
        let scrolls = menu.items.len() > rows;
        let width = usize::from(inner.width).saturating_sub(usize::from(scrolls));
        for (offset, index) in range.clone().enumerate() {
            let Some(rect) = row(inner, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                break;
            };
            let Some(item) = menu.items.get(index) else {
                break;
            };
            let selected = index == self.cursor;
            let row_style = if selected {
                style.row_cursor(true)
            } else {
                style.body()
            };
            let text = text::fit_left(
                &item.text(width, style.ascii),
                width,
                Crop::End,
                ellipsis(style.ascii),
            );
            draw_text(f, rect, &text, row_style, style.ascii);
        }
        if !scrolls {
            return;
        }
        let marker_x = inner.right().saturating_sub(1);
        if range.start > 0
            && let Some(top) = row(inner, 0)
        {
            let at = Rect::new(marker_x, top.y, 1, 1);
            draw_text(f, at, glyphs.arrow_up(), style.body(), style.ascii);
        }
        if range.end < menu.items.len()
            && let Some(bottom) = row(inner, inner.height.saturating_sub(1))
        {
            let at = Rect::new(marker_x, bottom.y, 1, 1);
            draw_text(f, at, glyphs.arrow_down(), style.body(), style.ascii);
        }
    }
}

impl Dialog for MenuDialog {
    fn id(&self) -> DialogId {
        DialogId::Menu
    }

    fn title(&self) -> String {
        "Menu".to_string()
    }

    /// The full width, and a row for the bar plus the longest menu.
    ///
    /// The height is the **longest** menu's and not the open one's, so walking
    /// the bar with `Left` and `Right` does not move the box up and down under
    /// the reader: [`crate::dialog::centred`] places it from the size asked
    /// for, and a size that changed per menu would make every `Right` a jump.
    fn size_hint(&self) -> (u16, u16) {
        let longest = self
            .model
            .menus
            .iter()
            .map(|menu| menu.items.len())
            .max()
            .unwrap_or(0)
            .min(MAX_ROWS);
        let height = u16::try_from(longest.saturating_add(5)).unwrap_or(u16::MAX);
        (u16::MAX, height)
    }

    /// The six title letters, always.
    ///
    /// Always, and not "the open menu's", because the bar is on screen for as
    /// long as the dialog is: there is one screen here and not seven, and
    /// `Alt+C` opens `Commands` whichever menu is dropped.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.model.menus.iter().map(|menu| menu.letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // "Esc closes it and gives the panel back."
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        // the rule, applied to the titles. Before the accept test and before
        // the text keys, because a dialog's own mnemonics win over everything
        // while it is open.
        if let Some(letter) = key.mnemonic()
            && let Some(index) = self.model.index_of_letter(letter)
        {
            self.open = index;
            self.cursor = 0;
            return DialogOutcome::Consumed;
        }
        if key.is_accept() {
            // The answer is the action's id, and `dialog_accepted` runs it
            // through `run_action` - so a menu item and its key run the same
            // code, which is what makes the "a menu that listed
            // things reachable only from the menu would be a second, weaker
            // interface" true by construction.
            return match self.selected() {
                Some(item) => {
                    DialogOutcome::Accept(DialogResult::Text(item.action.id().to_string()))
                }
                None => DialogOutcome::Consumed,
            };
        }
        match key.press.code {
            KeyCode::Left => {
                self.walk_menus(false);
                DialogOutcome::Consumed
            }
            KeyCode::Right => {
                self.walk_menus(true);
                DialogOutcome::Consumed
            }
            KeyCode::Up => {
                self.walk_items(-1);
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.walk_items(1);
                DialogOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.walk_items(-10);
                DialogOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.walk_items(10);
                DialogOutcome::Consumed
            }
            KeyCode::Home => {
                self.cursor = 0;
                DialogOutcome::Consumed
            }
            KeyCode::End => {
                self.cursor = self
                    .menu()
                    .map_or(0, |menu| menu.items.len().saturating_sub(1));
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some(bar) = row(area, 0) {
            self.draw_bar(f, bar, style);
        }
        self.draw_dropdown(f, area, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::{ColorDepth, Config, Keymap, Theme};
    use crate::input::{KeyModifiers, KeyPress, Milestone};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn app() -> App {
        App::headless(Config::default(), Keymap::builtin(), Theme::blue())
    }

    fn app_with(order: Vec<ColumnId>) -> App {
        let mut config = Config::default();
        config.panel.columns.order = order;
        App::headless(config, Keymap::builtin(), Theme::blue())
    }

    fn dialog() -> MenuDialog {
        MenuDialog::new(model(&app()), 0)
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn alt(letter: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(letter), KeyModifiers::ALT))
    }

    fn render(d: &MenuDialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The dialog's own interior, without the framework's border, so an ASCII
    /// failure is this dialog's own and not the box's.
    fn render_inner(d: &MenuDialog, w: u16, h: u16, ascii: bool) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let st = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                let area = f.area();
                d.render(f, area, &st);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
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

    #[test]
    fn the_bar_is_spec_8_6s_six_titles_in_order() {
        // "Files, Mark, Commands, Net, Show, Configuration", and
        // they are the own array so the permanent bar and the dropped
        // one cannot disagree.
        let bar = model(&app());
        let titles: Vec<&str> = bar.menus.iter().map(|menu| menu.title).collect();
        assert_eq!(titles, crate::ui::MENUBAR.to_vec());
        let letters: Vec<char> = bar.menus.iter().map(|menu| menu.letter).collect();
        assert_eq!(letters, LETTERS.to_vec());
    }

    #[test]
    fn every_letter_is_in_the_title_it_underlines() {
        // "The letter is shown underlined in that control's
        // label, so the whole set is readable off the screen rather than
        // memorised." A letter that is not in the word can never be underlined.
        for menu in &model(&app()).menus {
            assert!(
                split_mnemonic(menu.title, menu.letter).is_some(),
                "{}: no `{}` to underline",
                menu.title,
                menu.letter
            );
        }
    }

    #[test]
    fn the_six_letters_are_unique_and_are_what_the_census_reads() {
        // "Mnemonics are unique within a dialog. A duplicate is a
        // bug rather than a first-one-wins rule."
        let letters = dialog().mnemonic_letters();
        assert_eq!(letters.len(), LETTERS.len());
        let mut sorted = letters.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), letters.len(), "{letters:?}");
    }

    #[test]
    fn every_item_resolves_to_at_least_one_binding() {
        // "Every item on it is a key that already exists, and the
        // menu shows that key beside the item. ... If an item has no key, it is
        // because it has not been built yet, and that is a bug in the menu
        // rather than a design." Invariant I11, half one.
        let app = app();
        let bar = model(&app);
        for item in bar.items() {
            let bindings = app.keymap.bindings_in(KeyContext::Panel, item.action);
            assert!(
                !bindings.is_empty(),
                "{}: on the menu with no binding behind it",
                item.action.id()
            );
            assert_ne!(item.keys, crate::config::keymap::UNBOUND, "{}", item.label);
        }
    }

    #[test]
    fn every_item_is_an_action_this_release_implements() {
        // Invariant I11, half two. Split from the binding half and written
        // against the milestone rather than against `implemented()` because
        // `Milestone::CURRENT` moves **last** in this milestone:
        // the menu may never carry a row from a
        // release after this one, and once `CURRENT` has moved that is exactly
        // `Action::implemented`.
        let app = app();
        for item in model(&app).items() {
            assert!(
                item.action.milestone() <= Milestone::V07,
                "{}: belongs to {}",
                item.action.id(),
                item.action.milestone()
            );
            if Milestone::V07.is_current() {
                assert!(item.action.implemented(), "{}", item.action.id());
            }
        }
    }

    #[test]
    fn no_action_is_offered_twice_on_the_same_menu() {
        for menu in &model(&app()).menus {
            let mut seen: Vec<Action> = menu.items.iter().map(|item| item.action).collect();
            let count = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), count, "{} lists something twice", menu.title);
        }
    }

    #[test]
    fn show_names_the_columns_the_configuration_lists() {
        // `Ctrl+<n>` sorts by the nth *configured* column, so
        // the menu row that names it is a fact about `panel.columns.order` and
        // is generated from it. `Sort by Size Ctrl+3` appears only where size
        // is the third column.
        let app = app_with(vec![ColumnId::Name, ColumnId::Ext, ColumnId::Size]);
        let bar = model(&app);
        let show = bar.menus.get(4).expect("the Show menu");
        assert_eq!(show.title, "Show");
        let labels: Vec<&str> = show.items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels.first().copied(), Some("Sort by Name"));
        assert_eq!(labels.get(2).copied(), Some("Sort by Size"));
        assert_eq!(labels.get(3).copied(), Some("Secondary sort by Name"));
        // Three columns, three secondaries, the clear, and SHOW_TAIL.
        assert_eq!(show.items.len(), 3 + 3 + 1 + SHOW_TAIL.len());

        let app = app_with(vec![ColumnId::Size, ColumnId::Name]);
        let bar = model(&app);
        let show = bar.menus.get(4).expect("the Show menu");
        assert_eq!(
            show.items.first().map(|item| item.label.as_str()),
            Some("Sort by Size"),
            "the same key now names a different column"
        );
        assert_eq!(show.items.len(), 2 + 2 + 1 + SHOW_TAIL.len());
    }

    #[test]
    fn a_rebound_key_shows_the_users_binding() {
        // The `keys` column is read from the live keymap, so it cannot drift
        // from what the key actually is.
        let mut config = Config::default();
        config.panel.columns.order = vec![ColumnId::Name];
        let mut keymap = Keymap::builtin();
        keymap.bind(
            None,
            crate::input::Binding::Key(KeyPress::plain(KeyCode::F(12))),
            Action::Copy,
        );
        let app = App::headless(config, keymap, Theme::blue());
        let bar = model(&app);
        let copy = bar
            .items()
            .find(|item| item.action == Action::Copy)
            .expect("Copy is on the Files menu");
        assert!(copy.keys.contains("F12"), "{}", copy.keys);
    }

    #[test]
    fn alt_letter_opens_the_menu_that_letter_names() {
        // "Alt with the underlined letter opens one directly, the same
        // mnemonic rule as the dialogs."
        let mut d = dialog();
        assert_eq!(d.open_menu(), 0);
        assert!(matches!(d.handle_key(&alt('c')), DialogOutcome::Consumed));
        assert_eq!(d.open_menu(), 2, "Commands");
        assert!(matches!(d.handle_key(&alt('h')), DialogOutcome::Consumed));
        assert_eq!(d.open_menu(), 4, "Show, not `s`: the design");
        assert!(matches!(d.handle_key(&alt('O')), DialogOutcome::Consumed));
        assert_eq!(d.open_menu(), 5, "the letter is folded to lower case");
        assert!(matches!(d.handle_key(&alt('z')), DialogOutcome::Ignored));
        assert_eq!(
            d.open_menu(),
            5,
            "a letter that names no menu changes nothing"
        );
    }

    #[test]
    fn opening_a_menu_starts_at_its_first_item() {
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 2);
        d.handle_key(&alt('m'));
        assert_eq!(d.cursor(), 0, "a menu opens on its first row");
    }

    #[test]
    fn the_arrow_keys_walk_the_menus_and_their_items() {
        // "and the arrow keys walk them."
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.open_menu(), 1);
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.open_menu(), 0);
        d.handle_key(&key(KeyCode::Left));
        assert_eq!(d.open_menu(), 5, "and it wraps rather than doing nothing");
        d.handle_key(&key(KeyCode::Right));
        assert_eq!(d.open_menu(), 0);

        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 1);
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.cursor(), 0);
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.cursor(), FILES.len() - 1, "the item list wraps too");
        d.handle_key(&key(KeyCode::Home));
        assert_eq!(d.cursor(), 0);
        d.handle_key(&key(KeyCode::End));
        assert_eq!(d.cursor(), FILES.len() - 1);
    }

    #[test]
    fn enter_answers_with_the_action_id_and_esc_closes() {
        // The answer is the action's own id, so `dialog_accepted` runs it
        // through the same `run_action` the key does.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        let outcome = d.handle_key(&key(KeyCode::Enter));
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(text, Action::Edit.id());
                assert_eq!(
                    Action::from_id(&text),
                    Some(Action::Edit),
                    "and it parses back"
                );
            }
            other => panic!("expected the action id, got {other:?}"),
        }
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn the_dropdown_hangs_under_the_open_title() {
        // the bar, drawn as a bar: the open menu's box starts at the
        // column its title starts at, and only slides left when it would
        // otherwise run off the edge.
        let mut d = dialog();
        d.handle_key(&alt('m'));
        let area = Rect::new(0, 0, 120, 30);
        let cells = d.cells();
        let start = cells.get(1).map(|(start, _)| *start).expect("Mark's cell");
        let rect = d.dropdown_rect(area).expect("a dropdown");
        assert_eq!(usize::from(rect.x), start);
        assert_eq!(rect.y, 1, "directly under the bar");

        // The last menu is far enough right that its box has to slide left.
        d.handle_key(&alt('o'));
        let rect = d.dropdown_rect(area).expect("a dropdown");
        assert!(rect.right() <= area.right(), "{rect:?}");
    }

    #[test]
    fn the_window_keeps_the_cursor_in_view_and_never_runs_past_the_end() {
        for len in 0usize..25 {
            for rows in 0usize..12 {
                for cursor in 0..len.max(1) {
                    let range = MenuDialog::window(cursor, len, rows);
                    assert!(range.end <= len, "{len}/{rows}/{cursor}");
                    assert!(range.start <= range.end);
                    assert!(range.len() <= rows.min(len));
                    if rows > 0 && len > 0 {
                        assert!(
                            range.contains(&cursor),
                            "cursor {cursor} outside {range:?} of {len} in {rows} rows"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_row_shows_its_label_and_its_key_and_gives_the_key_up_last() {
        let item = MenuItem {
            label: "Copy the selection to the other panel".to_string(),
            action: Action::Copy,
            keys: "F5".to_string(),
        };
        let wide = item.text(40, false);
        assert!(wide.ends_with("F5"), "{wide:?}");
        assert_eq!(text::width(&wide), 40);
        // Too narrow for both: the label is the half that says which row this
        // is, so it is the half that stays and the key is dropped whole.
        let narrow = item.text(6, false);
        assert_eq!(text::width(&narrow), 6, "{narrow:?}");
        assert!(narrow.starts_with("Cop"), "{narrow:?}");
        assert!(!narrow.contains("F5"), "{narrow:?}");
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        // 60x15 is a supported size.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(), w, h, ascii));
                assert!(out.contains("Files"), "{w}x{h} ascii={ascii}:\n{out}");
                assert!(out.contains("Mark"), "{w}x{h} ascii={ascii}:\n{out}");
                assert!(out.contains("F5"), "{w}x{h} ascii={ascii}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        // `ui.ascii_borders = true` falls back to `+-|`, and that
        // covers the dropdown's frame and its scroll arrows.
        let mut d = dialog();
        d.handle_key(&alt('h'));
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (30, 8)] {
            let inner = dump(&render_inner(&d, w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn it_draws_nothing_outside_the_area_it_is_given() {
        // The rule `src/ui/dialog/mod.rs` states for every dialog in it: no
        // `Rect` without checking for zero width or height, and none that
        // leaves the interior.
        let mut d = dialog();
        for index in 0..6 {
            d.open = index;
            for h in 0u16..10 {
                for w in [0u16, 1, 4, 12, 40, 76] {
                    let area = Rect::new(3, 2, w, h);
                    if let Some(rect) = d.dropdown_rect(area) {
                        assert!(rect.width > 0 && rect.height > 0, "{w}x{h}: {rect:?}");
                        assert!(rect.right() <= area.right(), "{w}x{h}: {rect:?}");
                        assert!(rect.bottom() <= area.bottom(), "{w}x{h}: {rect:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_long_menu_scrolls_rather_than_growing_the_box() {
        // Nine columns is nine sort rows and nine secondaries, which is more
        // than the box asks for; the window scrolls and the box does not grow.
        let app = app_with(ColumnId::ALL.to_vec());
        let mut d = MenuDialog::new(model(&app), 4);
        let (_, height) = d.size_hint();
        assert!(usize::from(height) <= MAX_ROWS + 5, "{height}");
        let last = d
            .menu()
            .map(|menu| menu.items.len().saturating_sub(1))
            .expect("the Show menu");
        d.handle_key(&key(KeyCode::End));
        assert_eq!(d.cursor(), last);
        let out = dump(&render(&d, 120, 30, false));
        assert!(out.contains("Re-read"), "the last row is on screen:\n{out}");
    }

    #[test]
    fn an_undeliverable_key_is_marked_and_not_lectured_about() {
        // "on a legacy terminal the secondary sort is set from
        // the sort menu (F9) instead", so the row is drawn there whatever the
        // terminal can send, marked with why - and without `describe`'s
        // pointer to the design, which on this menu points at the menu the
        // reader is already looking at.
        let app = app();
        assert!(!app.keyboard.enhanced, "a headless App is the legacy case");
        let bar = model(&app);
        let row = bar
            .items()
            .find(|item| item.action == Action::SortSecondary1)
            .expect("the Show menu lists the secondary sorts");
        assert!(row.keys.contains("Ctrl+Shift+1"), "{}", row.keys);
        assert!(
            row.keys.contains(crate::config::keymap::UNAVAILABLE),
            "{}",
            row.keys
        );
        assert!(
            !row.keys.contains(crate::config::keymap::NO_FALLBACK),
            "{}",
            row.keys
        );
    }

    #[test]
    fn an_out_of_range_menu_opens_the_last_one_rather_than_none() {
        let d = MenuDialog::new(model(&app()), 99);
        assert_eq!(d.open_menu(), 5);
    }
}
