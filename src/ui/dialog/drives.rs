//! the device popup and the hotlist, which are one
//! dialog.
//!
//! ```text
//!     +- Left panel drives ------------------------+
//!     |/                    btrfs  102G of 468G ...|
//!     |/boot/efi            vfat   488M of 511M ...|
//!     |/run/media/t/USB  BACKUP  vfat  12G ...  [r]|
//!     |--------------------------------------------|
//!     |media        /srv/media                     |
//!     |archive      /mnt/archive  no such file ... |
//!     |                              search: us [a]|
//!     +--------------------------------------------+
//! ```
//!
//! # It touches nothing
//!
//! The devices arrive already enumerated and the hotlist arrives already
//! stated: the dialog reads no file and makes no `stat`, which is
//! the rule for everything `crate::input::dispatch` can
//! reach. `crate::app::App::service_drives` builds it and
//! `crate::app::App::service_drives_poll` refreshes it
//! ([`DrivesDialog::refresh_devices`]).
//!
//! # Which panel it acts on
//!
//! `Alt+F1` is spatial: it targets the **left** panel whichever panel has
//! focus, and `Alt+F2` the right. That is carried by the
//! [`crate::input::DialogId`] the dialog reports, so the answer is an ordinary
//! [`DialogResult::Text`] and no new result variant exists to be got wrong.
//! `Ctrl+D`'s hotlist acts on the **active** panel and reports
//! [`crate::input::DialogId::Hotlist`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::devices::hotlist::HotlistRow;
use crate::devices::{self, Device, MAX_VISIBLE_ROWS};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_text};
use crate::input::quicksearch::quick_match;
use crate::input::quicksearch::status_label;
use crate::input::{DialogId, KeyCode};
use crate::panel::Side;
use crate::ui::dialog::{ellipsis, row as row_rect};
use crate::ui::text::{self, Crop, Glyphs};

/// How wide the label column of a hotlist row is, matching the connect
/// dialog's saved-host list so two lists in one program line up the same way.
const LABEL_COLUMN: usize = 12;

/// The rows that are not the list: two borders and the status line that
/// carries the quick-search fragment and any refusal.
const CHROME_ROWS: u16 = 3;

/// The widest the popup asks to be. It is a hint: the framework clamps it to
/// the panel it is anchored under.
const MAX_WIDTH: u16 = 64;

/// The narrowest the popup asks to be, so a hotlist of short labels still
/// draws as a box rather than as a column.
const MIN_WIDTH: u16 = 32;

/// The two border columns plus the one a scrollbar may take, so the widest row
/// still fits inside the width [`Dialog::size_hint`] asks for.
const SIDE_PADDING: u16 = 3;

/// One row of the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveRow {
    /// A mounted filesystem.
    Device(Device),
    /// the separator between the devices and the hotlist. Never
    /// selectable, and skipped by the cursor in both directions.
    Separator,
    /// A hotlist entry, greyed when it is missing.
    Hotlist(HotlistRow),
}

impl DriveRow {
    /// Can the cursor rest here? False for the separator alone.
    const fn selectable(&self) -> bool {
        match self {
            Self::Device(_) | Self::Hotlist(_) => true,
            Self::Separator => false,
        }
    }

    /// What quick search matches this row against.
    ///
    /// A device's mount point without its leading `/`, so the own
    /// `us` -> `/usr` example works, and a hotlist entry's label, which is the
    /// name the user gave it.
    fn search_key(&self) -> &str {
        match self {
            Self::Device(device) => devices::search_key(&device.mount_point),
            Self::Separator => "",
            Self::Hotlist(row) => &row.entry.label,
        }
    }
}

/// Which of the two popups this is.
///
/// A field rather than a look at [`DrivesDialog::id`], so [`Dialog::title`]
/// and [`DrivesDialog::refresh_devices`] read one small enum of this module's
/// own instead of matching `DialogId`'s thirty-odd variants for the three that
/// can occur here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `Alt+F1` / `Alt+F2`: devices, a separator, then the hotlist, acting on
    /// **this** side whichever panel has focus.
    Devices(Side),
    /// `Ctrl+D`: the hotlist alone, acting on the active panel.
    Hotlist,
}

impl Kind {
    /// The popup's title, which names the panel it will change because
    /// the design makes `Alt+F1` spatial and this is the last chance to see
    /// which side that is.
    const fn title(self) -> &'static str {
        match self {
            Self::Devices(Side::Left) => "Left panel drives",
            Self::Devices(Side::Right) => "Right panel drives",
            Self::Hotlist => "Directory hotlist",
        }
    }

    /// Which dialog the framework and `crate::input::dialog_accepted` see.
    const fn id(self) -> DialogId {
        match self {
            Self::Devices(side) => DialogId::Drive(side),
            Self::Hotlist => DialogId::Hotlist,
        }
    }

    /// What an empty popup says instead of a list.
    ///
    /// `Ctrl+D` with nothing bookmarked is the ordinary first run and the
    /// place to learn the key that fills it. A device popup
    /// with no rows at all means `sysinfo` enumerated nothing, which is a
    /// container with no `/proc`, not an empty hotlist.
    const fn empty(self) -> &'static str {
        match self {
            Self::Devices(_) => "no mounted filesystems to show",
            Self::Hotlist => "the hotlist is empty - Ctrl+Shift+D adds this directory",
        }
    }

    /// Whether this popup has a device half at all.
    const fn has_devices(self) -> bool {
        match self {
            Self::Devices(_) => true,
            Self::Hotlist => false,
        }
    }
}

/// the popup, and the hotlist, which are one dialog.
///
/// > `Ctrl+D` opens the hotlist alone - the same list 17.1 shows below the
/// > devices, without the devices above it.
///
/// One type with two constructors rather than two dialogs, because the design
/// 17.3 defines the second as the first minus its top half.
pub struct DrivesDialog {
    /// Devices, then the separator, then the hotlist. The order the design
    /// draws them in and the order the cursor walks.
    rows: Vec<DriveRow>,
    /// Never a [`DriveRow::Separator`], and never past the end.
    cursor: usize,
    /// The panel-style quick-search buffer.
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
    /// Which of the two popups this is, which is also which panel the answer
    /// changes.
    kind: Kind,
    /// The panel the popup hangs under.
    anchor: Option<Side>,
    /// The reason the last `Enter` on a greyed row refused.
    refusal: Option<String>,
}

impl DrivesDialog {
    /// devices, a separator, then the hotlist. `side` decides
    /// the `DialogId` and the anchor, and is the panel that will be changed -
    /// which is `Alt+F1`'s left or `Alt+F2`'s right, never "the active one".
    ///
    pub fn devices(side: Side, devices: Vec<Device>, hotlist: Vec<HotlistRow>) -> Self {
        let mut rows: Vec<DriveRow> = devices.into_iter().map(DriveRow::Device).collect();
        // The separator separates two halves, so an empty hotlist gets none:
        // a rule under the last device would promise a list that is not there.
        if !rows.is_empty() && !hotlist.is_empty() {
            rows.push(DriveRow::Separator);
        }
        rows.extend(hotlist.into_iter().map(DriveRow::Hotlist));
        Self::build(rows, Kind::Devices(side), Some(side))
    }

    /// the `Ctrl+D`: the hotlist alone, acting on the **active**
    /// panel, which is why this one takes no `Side`.
    pub fn hotlist(hotlist: Vec<HotlistRow>) -> Self {
        let rows: Vec<DriveRow> = hotlist.into_iter().map(DriveRow::Hotlist).collect();
        Self::build(rows, Kind::Hotlist, None)
    }

    /// The two constructors' shared half.
    fn build(rows: Vec<DriveRow>, kind: Kind, anchor: Option<Side>) -> Self {
        let mut dialog = Self {
            rows,
            cursor: 0,
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
            kind,
            anchor,
            refusal: None,
        };
        dialog.cursor = dialog.first_selectable();
        dialog
    }

    /// Match the list's quick search to the panel's own configured rules,
    /// the way `ConnectDialog::with_quick_search` does.
    #[must_use]
    pub const fn with_quick_search(mut self, mode: QuickSearchMode, case: QuickSearchCase) -> Self {
        self.mode = mode;
        self.case = case;
        self
    }

    /// Hang `Ctrl+D`'s popup under the active panel.
    ///
    /// the design asks for [`Dialog::anchor`] to be
    /// `Some(active_side)` for the hotlist, and [`DrivesDialog::hotlist`]
    /// takes no `Side` because the `Ctrl+D` is not spatial. The
    /// event loop knows which panel is active when it builds the popup, and
    /// this is where it says so. Without it the hotlist is centred, which is
    /// the framework's default and never wrong, only less informative.
    #[must_use]
    pub const fn with_anchor(mut self, side: Side) -> Self {
        self.anchor = Some(side);
        self
    }

    /// Replace the device rows in place, keeping the cursor on the same mount
    /// point where it still exists (the live refresh; see
    /// the design).
    ///
    /// A no-op on `Ctrl+D`'s hotlist, which has no device half to refresh.
    pub fn refresh_devices(&mut self, devices: Vec<Device>) {
        if !self.kind.has_devices() {
            return;
        }
        // What the cursor is on now, so it can be found again afterwards. A
        // hotlist row is identified by its offset within the hotlist, because
        // that half is untouched by a refresh.
        let was_mount = self.selected_device().map(|d| d.mount_point.clone());
        let was_hotlist = self.hotlist_offset(self.cursor);

        let hotlist: Vec<DriveRow> = self
            .rows
            .iter()
            .filter(|row| matches!(row, DriveRow::Hotlist(_)))
            .cloned()
            .collect();
        let mut rows: Vec<DriveRow> = devices.into_iter().map(DriveRow::Device).collect();
        let device_count = rows.len();
        if !rows.is_empty() && !hotlist.is_empty() {
            rows.push(DriveRow::Separator);
        }
        rows.extend(hotlist);
        self.rows = rows;

        self.cursor = match (was_mount, was_hotlist) {
            // The same mount is still there: stay on it, wherever it moved to.
            (Some(mount), _) => self
                .rows
                .iter()
                .position(|row| match row {
                    DriveRow::Device(device) => device.mount_point == mount,
                    DriveRow::Separator | DriveRow::Hotlist(_) => false,
                })
                // Unmounted while the popup was open: the nearest row, which
                // is where the list closed over the gap.
                .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1))),
            // A hotlist row keeps its place under whatever devices there are
            // now, separator included.
            (None, Some(offset)) => {
                let separator = usize::from(device_count > 0);
                device_count
                    .saturating_add(separator)
                    .saturating_add(offset)
            }
            (None, None) => self.cursor,
        };
        self.settle_cursor();
    }

    /// The rows, in order, for the renderer and for tests.
    pub fn rows(&self) -> &[DriveRow] {
        &self.rows
    }

    /// Which row the cursor is on. Never a [`DriveRow::Separator`].
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The quick-search buffer, for the status fragment and for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// The reason the last `Enter` on a greyed row refused, if any.
    /// Shown in the popup rather than closing it.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// The row under the cursor.
    pub fn selected(&self) -> Option<&DriveRow> {
        self.rows.get(self.cursor)
    }

    /// The device under the cursor, when the cursor is on one.
    fn selected_device(&self) -> Option<&Device> {
        match self.rows.get(self.cursor)? {
            DriveRow::Device(device) => Some(device),
            DriveRow::Separator | DriveRow::Hotlist(_) => None,
        }
    }

    /// How far into the hotlist half row `index` is, when it is in it.
    fn hotlist_offset(&self, index: usize) -> Option<usize> {
        match self.rows.get(index)? {
            DriveRow::Hotlist(_) => Some(
                self.rows
                    .iter()
                    .take(index)
                    .filter(|row| matches!(row, DriveRow::Hotlist(_)))
                    .count(),
            ),
            DriveRow::Device(_) | DriveRow::Separator => None,
        }
    }

    /// The first row the cursor may rest on, or zero when there are none.
    fn first_selectable(&self) -> usize {
        self.rows
            .iter()
            .position(DriveRow::selectable)
            .unwrap_or_default()
    }

    /// Put the cursor somewhere legal: inside the list, and never on the
    /// separator.
    fn settle_cursor(&mut self) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if self.rows.get(self.cursor).is_some_and(DriveRow::selectable) {
            return;
        }
        // On the separator: the row after it, or the one before it when the
        // separator is somehow last.
        let after = self
            .rows
            .iter()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, row)| row.selectable())
            .map(|(at, _)| at);
        self.cursor = match after {
            Some(at) => at,
            None => self.first_selectable(),
        };
    }

    /// Move the cursor `delta` rows, skipping the separator, stopping at the
    /// ends. A list that wraps would put the hotlist above the devices.
    fn step(&mut self, up: bool, count: usize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len().saturating_sub(1);
        let mut at = self.cursor;
        for _ in 0..count.max(1) {
            let next = if up {
                at.saturating_sub(1)
            } else {
                at.saturating_add(1).min(last)
            };
            if next == at {
                break;
            }
            at = next;
        }
        // Past the separator rather than onto it, in the direction of travel.
        while !self.rows.get(at).is_some_and(DriveRow::selectable) {
            let next = if up {
                at.saturating_sub(1)
            } else {
                at.saturating_add(1).min(last)
            };
            if next == at {
                // Nothing selectable that way: leave the cursor where it was.
                return;
            }
            at = next;
        }
        self.move_to(at);
    }

    /// Land the cursor on `at` and end whatever the last keystroke was saying.
    fn move_to(&mut self, at: usize) {
        self.cursor = at;
        // A moved cursor ends the search, exactly as it does in a panel,
        // and clears a refusal that was about another row.
        self.quick.clear();
        self.refusal = None;
        self.settle_cursor();
    }

    /// One keystroke of panel quick search over the rows.
    ///
    /// A character that matches nothing is **refused** rather than typed, so
    /// the buffer always names a row that is on the screen - which is what the
    /// panel does and what the connect dialog's list does.
    fn quick_search(&mut self, ch: char) {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let found = self.rows.iter().position(|row| {
            row.selectable() && quick_match(row.search_key(), &candidate, self.mode, self.case)
        });
        let Some(at) = found else {
            return;
        };
        self.quick = candidate;
        self.cursor = at;
        self.refusal = None;
    }

    /// `Enter`.
    ///
    /// A greyed hotlist row refuses **in place**, naming the reason, rather
    /// than closing the popup or navigating: the design keeps the entry
    /// precisely because a missing path is usually an unmounted disk, and
    /// closing the popup would hide the answer to the question that was asked.
    fn accept(&mut self) -> DialogOutcome {
        match self.rows.get(self.cursor) {
            Some(DriveRow::Device(device)) => {
                DialogOutcome::Accept(DialogResult::Text(device.mount_point.clone()))
            }
            Some(DriveRow::Hotlist(row)) => match (&row.missing, &row.resolved) {
                (Some(why), _) => {
                    self.refusal = Some(format!("{}: {why}", row.entry.path));
                    DialogOutcome::Consumed
                }
                (None, Some(path)) => {
                    DialogOutcome::Accept(DialogResult::Text(path.to_string_lossy().into_owned()))
                }
                // Nothing built by `hotlist::rows` reaches here: a row with no
                // resolved path always carries the reason it has none. Refuse
                // rather than navigate to a path that was never worked out.
                (None, None) => {
                    self.refusal = Some(format!("{}: cannot be expanded", row.entry.path));
                    DialogOutcome::Consumed
                }
            },
            // The separator is never under the cursor, and an empty popup has
            // nothing to accept.
            Some(DriveRow::Separator) | None => DialogOutcome::Consumed,
        }
    }

    /// How many rows the list itself gets in an interior of this height.
    const fn list_height(area: Rect) -> u16 {
        // One row of the interior is the status line; the borders are already
        // outside `area`.
        area.height.saturating_sub(1)
    }

    /// The window of rows visible in `height` rows, scrolled the least amount
    /// that keeps the cursor on screen.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.rows.is_empty() {
            return 0..0;
        }
        let start = self
            .cursor
            .saturating_add(1)
            .saturating_sub(height)
            .min(self.rows.len().saturating_sub(1));
        start..start.saturating_add(height).min(self.rows.len())
    }

    /// One row's text, fitted to `width`.
    ///
    /// The device half is `crate::devices::row`, so the popup and any other
    /// reader of a mount agree on what a mount looks like. A hotlist row is
    /// its label, its path, and - when it is missing - the reason, which is
    /// what the design asks to be shown beside it.
    fn row_text(row: &DriveRow, width: usize, ascii: bool) -> String {
        match row {
            DriveRow::Device(device) => devices::row(device, width, ascii),
            DriveRow::Separator => Glyphs::new(ascii).horizontal().repeat(width),
            DriveRow::Hotlist(entry) => {
                let label =
                    text::fit_left(&entry.entry.label, LABEL_COLUMN, Crop::End, ellipsis(ascii));
                let body = match &entry.missing {
                    Some(why) => format!("{label} {}  {why}", entry.entry.path),
                    None => format!("{label} {}", entry.entry.path),
                };
                text::truncate(&body, width, Crop::End, ellipsis(ascii))
            }
        }
    }

    /// The scrollbar column the design asks for above nine rows.
    ///
    /// > A row is one line; the list never scrolls past nine entries without a
    /// > scrollbar.
    ///
    /// A single cell marking where the visible window sits in the whole list,
    /// which is as much as one column can honestly say.
    fn draw_scrollbar(&self, f: &mut Frame, area: Rect, height: u16, style: &DialogStyle) {
        let total = self.rows.len();
        let visible = usize::from(height);
        if visible == 0 || total <= visible || area.width == 0 {
            return;
        }
        let glyphs = Glyphs::new(style.ascii);
        let x = area.right().saturating_sub(1);
        let last_row = visible.saturating_sub(1);
        // Where the thumb sits: the cursor's position through the list,
        // scaled to the column. Integer arithmetic, so the thumb reaches the
        // bottom only when the cursor is on the last row.
        let thumb = self
            .cursor
            .saturating_mul(last_row)
            .checked_div(total.saturating_sub(1))
            .unwrap_or(0);
        for offset in 0..visible {
            let Some(y) = u16::try_from(offset).ok().map(|o| area.y.saturating_add(o)) else {
                break;
            };
            if y >= area.y.saturating_add(height) {
                break;
            }
            let cell = if offset == thumb {
                glyphs.caret_block()
            } else {
                glyphs.vertical()
            };
            draw_text(f, Rect::new(x, y, 1, 1), cell, style.body(), style.ascii);
        }
    }
}

impl Dialog for DrivesDialog {
    fn id(&self) -> DialogId {
        self.kind.id()
    }

    fn title(&self) -> String {
        self.kind.title().to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        // As wide as its widest row wants to be, between the two bounds: a
        // hotlist of three short labels should not open as a full-width box,
        // and a list of long mount points should not be cropped while there is
        // room beside it. The framework clamps this to the anchor rectangle,
        // so it is a wish and not a demand.
        let widest = self
            .rows
            .iter()
            // The separator is a rule drawn to whatever width it is given, so
            // it has no opinion about how wide the popup should be.
            .filter(|row| !matches!(row, DriveRow::Separator))
            .map(|row| text::width(&Self::row_text(row, usize::from(MAX_WIDTH), false)))
            .max()
            .unwrap_or(0);
        let want = u16::try_from(widest)
            .unwrap_or(MAX_WIDTH)
            .saturating_add(SIDE_PADDING);
        let rows = u16::try_from(self.rows.len().min(MAX_VISIBLE_ROWS)).unwrap_or(1);
        (
            want.clamp(MIN_WIDTH, MAX_WIDTH),
            rows.saturating_add(CHROME_ROWS),
        )
    }

    /// the popup hangs under the panel it will change, rather than
    /// being centred like every other dialog.
    fn anchor(&self) -> Option<Side> {
        self.anchor
    }

    /// **None**, deliberately (the pattern): every
    /// row is chosen with an arrow key or by typing, and a mnemonic letter
    /// would be a letter the quick search could no longer type.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    /// So `App::service_drives_poll` can reach [`DrivesDialog::refresh_devices`]
    /// on the popup that is on screen.
    ///
    /// the design wants the list live while it is open, and the refresh is a
    /// re-enumeration the event loop performs and hands *in* - the direction
    /// [`crate::dialog::DialogResult`] does not travel. The same escape hatch
    /// the `+ F7` already uses, and for the same reason.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            // A running quick search takes the first `Esc`, exactly as it does
            // in a panel; the second closes the popup.
            if !self.quick.is_empty() {
                self.quick.clear();
                return DialogOutcome::Consumed;
            }
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return self.accept();
        }
        match key.press.code {
            KeyCode::Up => {
                self.step(true, 1);
                return DialogOutcome::Consumed;
            }
            KeyCode::Down => {
                self.step(false, 1);
                return DialogOutcome::Consumed;
            }
            KeyCode::PageUp => {
                self.step(true, MAX_VISIBLE_ROWS);
                return DialogOutcome::Consumed;
            }
            KeyCode::PageDown => {
                self.step(false, MAX_VISIBLE_ROWS);
                return DialogOutcome::Consumed;
            }
            KeyCode::Home => {
                self.move_to(0);
                return DialogOutcome::Consumed;
            }
            KeyCode::End => {
                self.move_to(self.rows.len().saturating_sub(1));
                return DialogOutcome::Consumed;
            }
            KeyCode::Backspace => {
                self.quick.pop();
                return DialogOutcome::Consumed;
            }
            _ => {}
        }
        if let Some(ch) = key.text() {
            // A character that matches nothing is swallowed rather than typed,
            // which is what the quick search does.
            self.quick_search(ch);
            return DialogOutcome::Consumed;
        }
        DialogOutcome::Ignored
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let height = Self::list_height(area);
        let scrollbar = self.rows.len() > usize::from(height);
        let width = usize::from(area.width).saturating_sub(usize::from(scrollbar));

        if self.rows.is_empty() {
            if let Some(rect) = row_rect(area, 0) {
                draw_text(f, rect, self.kind.empty(), style.body(), style.ascii);
            }
            return;
        }

        for (offset, index) in self.window(usize::from(height)).enumerate() {
            let Some(rect) = row_rect(area, u16::try_from(offset).unwrap_or(0)) else {
                break;
            };
            let Some(row) = self.rows.get(index) else {
                break;
            };
            let text = Self::row_text(row, width, style.ascii);
            let line_style = if index == self.cursor {
                // The panel's cursor bar: a selected row in a list is the same
                // idea as the cursor bar on a panel.
                style.row_cursor(true)
            } else {
                match row {
                    // "shown greyed with the reason". No new
                    // theme slot for it - the
                    // body colour, dimmed, which is what every terminal
                    // renders as grey.
                    DriveRow::Hotlist(entry) if entry.missing.is_some() => {
                        style.body().add_modifier(Modifier::DIM)
                    }
                    DriveRow::Device(_) | DriveRow::Separator | DriveRow::Hotlist(_) => {
                        style.body()
                    }
                }
            };
            let cropped = Rect::new(
                rect.x,
                rect.y,
                u16::try_from(width).unwrap_or(rect.width).min(rect.width),
                1,
            );
            draw_text(f, cropped, &text, line_style, style.ascii);
        }
        self.draw_scrollbar(f, area, height, style);

        // The status line: the refusal wins it, because it answers the key
        // that was just pressed.
        if let Some(rect) = row_rect(area, height) {
            match (
                self.refusal.as_deref(),
                status_label(&self.quick, self.case),
            ) {
                (Some(why), _) => draw_text(f, rect, why, style.button(true), style.ascii),
                (None, Some(fragment)) => {
                    draw_text(f, rect, &fragment, style.body(), style.ascii);
                }
                (None, None) => {}
            }
        }
    }
}

impl std::fmt::Debug for DrivesDialog {
    /// By hand, because the row list is long and what a reader of a `Debug`
    /// wants from a popup is which one it is and where the cursor is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrivesDialog")
            .field("kind", &self.kind)
            .field("rows", &self.rows.len())
            .field("cursor", &self.cursor)
            .field("quick", &self.quick)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::devices::hotlist::HotlistEntry;
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The text a [`DialogOutcome::Accept`] carries, for the assertions that
    /// are about what the panel is told to do. [`DialogOutcome`] has no
    /// `PartialEq` - it can hold a `Box<dyn Dialog>` - so every test in this
    /// file reads it through `matches!` or through this.
    fn accepted(outcome: &DialogOutcome) -> Option<String> {
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(text)) => Some(text.clone()),
            DialogOutcome::Accept(_)
            | DialogOutcome::Act(_)
            | DialogOutcome::Consumed
            | DialogOutcome::Ignored
            | DialogOutcome::Cancel
            | DialogOutcome::Push(_)
            | DialogOutcome::Replace(_) => None,
        }
    }

    fn device(mount: &str) -> Device {
        Device {
            mount_point: mount.to_string(),
            label: mount.to_string(),
            fs_type: "ext4".to_string(),
            free: 1_000_000_000,
            total: 4_000_000_000,
            removable: false,
            read_only: false,
        }
    }

    fn live(label: &str, path: &str) -> HotlistRow {
        HotlistRow {
            entry: HotlistEntry {
                label: label.to_string(),
                path: path.to_string(),
            },
            resolved: Some(std::path::PathBuf::from(path)),
            missing: None,
        }
    }

    fn gone(label: &str, path: &str, why: &str) -> HotlistRow {
        HotlistRow {
            entry: HotlistEntry {
                label: label.to_string(),
                path: path.to_string(),
            },
            resolved: Some(std::path::PathBuf::from(path)),
            missing: Some(why.to_string()),
        }
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::new(code, KeyModifiers::NONE))
    }

    fn typed(ch: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    fn popup() -> DrivesDialog {
        DrivesDialog::devices(
            Side::Left,
            vec![device("/"), device("/usr"), device("/home")],
            vec![
                live("media", "/srv/media"),
                gone("old", "/mnt/old", "no such file or directory"),
            ],
        )
    }

    #[test]
    // "Below a separator, the same popup lists bookmarks and the
    // directory hotlist."
    fn the_popup_is_devices_a_separator_then_the_hotlist() {
        let dialog = popup();
        assert!(matches!(dialog.rows().first(), Some(DriveRow::Device(_))));
        assert!(matches!(dialog.rows().get(3), Some(DriveRow::Separator)));
        assert!(matches!(dialog.rows().get(4), Some(DriveRow::Hotlist(_))));
        assert_eq!(dialog.rows().len(), 6);
    }

    #[test]
    // "`Ctrl+D` opens the hotlist alone - the same list 17.1
    // shows below the devices, without the devices above it."
    fn ctrl_d_is_the_hotlist_alone() {
        let dialog = DrivesDialog::hotlist(vec![live("media", "/srv/media")]);
        assert_eq!(dialog.rows().len(), 1);
        assert!(matches!(dialog.rows().first(), Some(DriveRow::Hotlist(_))));
        assert_eq!(dialog.id(), DialogId::Hotlist);
    }

    #[test]
    // A rule under the last device would promise a hotlist that is not there.
    fn an_empty_hotlist_gets_no_separator() {
        let dialog = DrivesDialog::devices(Side::Right, vec![device("/")], Vec::new());
        assert_eq!(dialog.rows().len(), 1);
        assert_eq!(dialog.id(), DialogId::Drive(Side::Right));
    }

    #[test]
    // Invariant I4, and the own example: "typing `us` jumps to
    // `/usr`" under the default `panel.quick_search = "prefix"`.
    fn typing_us_selects_usr() {
        let mut dialog = popup();
        assert!(matches!(
            dialog.handle_key(&typed('u')),
            DialogOutcome::Consumed
        ));
        assert!(matches!(
            dialog.handle_key(&typed('s')),
            DialogOutcome::Consumed
        ));
        assert_eq!(dialog.quick_buffer(), "us");
        assert_eq!(
            dialog.selected(),
            Some(&DriveRow::Device(device("/usr"))),
            "cursor at {}",
            dialog.cursor()
        );
        // And `Enter` hands that mount point back for the panel to go to.
        let out = dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(accepted(&out).as_deref(), Some("/usr"), "{out:?}");
    }

    #[test]
    // A hotlist row is matched by the name the user gave it, not by its path.
    fn quick_search_matches_a_hotlist_row_by_label() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&typed('m'));
        assert_eq!(dialog.quick_buffer(), "m");
        assert!(matches!(dialog.selected(), Some(DriveRow::Hotlist(_))));
    }

    #[test]
    // a character that matches nothing is swallowed rather than
    // typed, so the buffer always names a row that is on the screen.
    fn a_character_that_matches_nothing_is_refused() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&typed('z'));
        assert_eq!(dialog.quick_buffer(), "");
        assert_eq!(dialog.cursor(), 0);
    }

    #[test]
    // The panel's rule, which the connect dialog's list keeps
    // too: the first `Esc` ends the search, the second closes the popup.
    fn esc_clears_the_search_before_it_closes_the_popup() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&typed('u'));
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Consumed
        ));
        assert_eq!(dialog.quick_buffer(), "");
        assert!(matches!(
            dialog.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    // the separator is "never selectable, and skipped by the
    // cursor in both directions".
    fn the_cursor_steps_over_the_separator() {
        let mut dialog = popup();
        for _ in 0..3 {
            let _ = dialog.handle_key(&key(KeyCode::Down));
        }
        assert_eq!(dialog.cursor(), 4, "the separator at 3 is skipped");
        assert!(matches!(dialog.selected(), Some(DriveRow::Hotlist(_))));
        let _ = dialog.handle_key(&key(KeyCode::Up));
        assert_eq!(dialog.cursor(), 2, "and skipped going back up");
    }

    #[test]
    // A list that wraps would put the hotlist above the devices, which is not
    // the order the design draws.
    fn the_cursor_stops_at_both_ends() {
        let mut dialog = popup();
        for _ in 0..20 {
            let _ = dialog.handle_key(&key(KeyCode::Up));
        }
        assert_eq!(dialog.cursor(), 0);
        for _ in 0..20 {
            let _ = dialog.handle_key(&key(KeyCode::Down));
        }
        assert_eq!(dialog.cursor(), 5);
    }

    #[test]
    // Invariant I5: "`Enter` on it refuses in place and does not close the
    // popup or navigate".
    fn enter_on_a_missing_hotlist_row_refuses_with_the_reason() {
        let mut dialog = DrivesDialog::hotlist(vec![gone("old", "/mnt/old", "not plugged in")]);
        let out = dialog.handle_key(&key(KeyCode::Enter));
        assert!(matches!(out, DialogOutcome::Consumed), "{out:?}");
        let refusal = dialog.refusal().expect("a refusal");
        assert!(refusal.contains("/mnt/old"), "{refusal}");
        assert!(refusal.contains("not plugged in"), "{refusal}");
    }

    #[test]
    // The path the panel goes to is the expanded one, because `~/x` is not a
    // directory anyone can `cd` to.
    fn enter_on_a_live_hotlist_row_hands_back_the_resolved_path() {
        let mut dialog = DrivesDialog::hotlist(vec![HotlistRow {
            entry: HotlistEntry {
                label: "home".to_string(),
                path: "~/media".to_string(),
            },
            resolved: Some(std::path::PathBuf::from("/home/thorin/media")),
            missing: None,
        }]);
        let out = dialog.handle_key(&key(KeyCode::Enter));
        assert_eq!(
            accepted(&out).as_deref(),
            Some("/home/thorin/media"),
            "{out:?}"
        );
    }

    #[test]
    // the live list: "The cursor stays
    // on the same mount point across a refresh where that mount still exists."
    fn a_refresh_keeps_the_cursor_on_the_same_mount() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&key(KeyCode::Down));
        let _ = dialog.handle_key(&key(KeyCode::Down));
        assert_eq!(dialog.selected(), Some(&DriveRow::Device(device("/home"))));
        // A stick appears above it in mount-point order.
        dialog.refresh_devices(vec![
            device("/"),
            device("/boot"),
            device("/home"),
            device("/usr"),
        ]);
        assert_eq!(
            dialog.selected(),
            Some(&DriveRow::Device(device("/home"))),
            "the cursor followed the mount it was on"
        );
    }

    #[test]
    // The other half of the same rule: a mount that goes away leaves the
    // cursor on the nearest row rather than on nothing.
    fn a_refresh_that_loses_the_selected_mount_falls_to_the_nearest_row() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&key(KeyCode::Down));
        assert_eq!(dialog.selected(), Some(&DriveRow::Device(device("/usr"))));
        dialog.refresh_devices(vec![device("/")]);
        assert!(dialog.selected().is_some_and(DriveRow::selectable));
    }

    #[test]
    // The hotlist half is untouched by a device refresh, so a cursor in it
    // stays on the same entry however many devices appear above it.
    fn a_refresh_keeps_a_hotlist_cursor_on_its_entry() {
        let mut dialog = popup();
        let _ = dialog.handle_key(&key(KeyCode::End));
        let before = dialog.selected().cloned();
        dialog.refresh_devices(vec![device("/"), device("/boot"), device("/usr")]);
        assert_eq!(dialog.selected().cloned(), before);
    }

    #[test]
    // `Ctrl+D`'s popup has no device half, and a poll must not grow it one.
    fn refreshing_the_hotlist_alone_does_nothing() {
        let mut dialog = DrivesDialog::hotlist(vec![live("media", "/srv/media")]);
        dialog.refresh_devices(vec![device("/")]);
        assert_eq!(dialog.rows().len(), 1);
    }

    #[test]
    // "the list never scrolls past nine entries without a
    // scrollbar", so the popup never asks to be taller than nine rows of list.
    fn the_popup_never_asks_for_more_than_nine_rows() {
        let many: Vec<Device> = (0..40).map(|n| device(&format!("/mnt/{n:02}"))).collect();
        let dialog = DrivesDialog::devices(Side::Left, many, Vec::new());
        let (_, height) = dialog.size_hint();
        assert_eq!(
            height,
            u16::try_from(MAX_VISIBLE_ROWS).unwrap_or(9) + CHROME_ROWS
        );
    }

    /// Draw the dialog at a size, so a panic or an overflow in the renderer is
    /// a failing test. The sizes are the three.
    fn draw_at(dialog: &DrivesDialog, w: u16, h: u16, ascii: bool) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                let area = f.area();
                crate::dialog::draw(f, dialog, area, &style);
            })
            .expect("a frame");
    }

    #[test]
    fn it_draws_at_every_size_in_both_glyph_sets() {
        let many: Vec<Device> = (0..40).map(|n| device(&format!("/mnt/{n:02}"))).collect();
        let scrolling = DrivesDialog::devices(Side::Left, many, vec![live("media", "/srv/media")]);
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (20, 6)] {
            for ascii in [false, true] {
                draw_at(&popup(), w, h, ascii);
                draw_at(&scrolling, w, h, ascii);
                draw_at(&DrivesDialog::hotlist(Vec::new()), w, h, ascii);
            }
        }
    }
}
