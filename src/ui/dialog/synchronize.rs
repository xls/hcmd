//! The synchronise dialog: a dry run you can edit, applied only on accept.
//!
//! It lists every action the plan would take and takes none. Each row shows a
//! direction that cannot be misread - an arrow towards the side that gets the
//! copy, or the word `del` on the side that loses the file - and `Space` cycles
//! one item through its sensible choices. `Tab` changes the global direction,
//! which re-defaults every row. `Enter` hands the plan to the copy and delete
//! engines; nothing on disk changes until then.
//!
//! ```text
//! ┌ Synchronize directories ─────────────────────────────────────┐
//! │ /home/a  ->  /home/b                          Both  L <-> R   │
//! │ copy >   report.txt                        1,204        512   │
//! │ < copy   notes.md                            256        980   │
//! │ del >    old.log                                 -      4,096  │
//! │ skip     same.txt                          1,000      1,000   │
//! │ 1 >   1 <   1 del   1 skip    Space Tab Enter apply  Esc      │
//! └──────────────────────────────────────────────────────────────┘
//! ```

use ratatui::Frame;
use ratatui::layout::Rect;

use super::{ellipsis, row};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_text};
use crate::input::{DialogId, KeyCode};
use crate::ops::sync::{SyncAction, SyncItem, SyncMode, SyncPlan};
use crate::ui::text;

/// the synchronise dialog.
#[derive(Debug)]
pub struct SynchronizeDialog {
    /// The left root, for the header line.
    left: String,
    /// The right root.
    right: String,
    /// The plan: the rows, their chosen actions, and the mode they default to.
    plan: SyncPlan,
    cursor: usize,
    /// The first row on screen, written once per frame by [`Dialog::layout`]
    /// so walking the cursor back up does not jump the view.
    scroll: usize,
}

impl SynchronizeDialog {
    /// A dialog over a built plan.
    pub fn new(left: &crate::vfs::VfsPath, right: &crate::vfs::VfsPath, plan: SyncPlan) -> Self {
        Self {
            left: left.to_string(),
            right: right.to_string(),
            plan,
            cursor: 0,
            scroll: 0,
        }
    }

    /// The plan, for tests and for the accept path.
    pub fn plan(&self) -> &SyncPlan {
        &self.plan
    }

    /// The direction word an action carries, in a fixed-width field so the
    /// names line up: an arrow towards the side that receives the copy, or
    /// `del` beside the side that loses the file.
    fn action_word(action: SyncAction) -> &'static str {
        match action {
            SyncAction::CopyRight => "copy >",
            SyncAction::CopyLeft => "< copy",
            SyncAction::DeleteRight => "del  >",
            SyncAction::DeleteLeft => "<  del",
            SyncAction::Skip => "skip  ",
        }
    }

    /// One row: the action, the relative path, and the size on each side.
    fn row_text(item: &SyncItem) -> String {
        let left = item.left_size.map_or_else(|| "-".to_string(), size);
        let right = item.right_size.map_or_else(|| "-".to_string(), size);
        format!(
            "{}  {:<34}  {left:>11}  {right:>11}",
            Self::action_word(item.action),
            item.rel
        )
    }

    /// The header: the direction the mode runs in, then the tally. The two
    /// roots are in the title, so this line has room for the counts even on a
    /// narrow terminal.
    fn header_text(&self) -> String {
        let mode = match self.plan.mode() {
            SyncMode::Both => "Both  L <-> R",
            SyncMode::ToRight => "Mirror  L --> R",
            SyncMode::ToLeft => "Mirror  L <-- R",
        };
        let c = self.plan.counts();
        format!(
            "{mode}    > {}  < {}  del {}  skip {}",
            c.copy_right, c.copy_left, c.delete, c.skip
        )
    }

    /// The footer: the keys, short enough to survive a 60-column box whole.
    fn footer_text() -> &'static str {
        "Space cycle  Tab direction  Enter apply  Esc cancel"
    }

    /// Move the cursor, clamped.
    fn move_by(&mut self, delta: isize) {
        let items = self.plan.items().len();
        if items == 0 {
            return;
        }
        let last = items.saturating_sub(1);
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta.unsigned_abs()).min(last)
        };
    }

    /// `Tab`: the next direction, wrapping, which re-defaults every row.
    fn next_mode(&mut self) {
        let next = match self.plan.mode() {
            SyncMode::Both => SyncMode::ToRight,
            SyncMode::ToRight => SyncMode::ToLeft,
            SyncMode::ToLeft => SyncMode::Both,
        };
        self.plan.set_mode(next);
    }

    /// Header, list and footer rectangles.
    fn regions(area: Rect) -> (Option<Rect>, Rect, Option<Rect>) {
        if area.height <= 2 {
            return (None, area, None);
        }
        let header = row(area, 0);
        let footer = row(area, area.height.saturating_sub(1));
        let list = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
        (header, list, footer)
    }

    /// The window of rows the current cursor and scroll put on screen.
    fn visible(&self, rows: usize) -> std::ops::Range<usize> {
        let items = self.plan.items().len();
        if rows == 0 || items == 0 {
            return 0..0;
        }
        let mut scroll = self.scroll.min(self.cursor);
        if self.cursor >= scroll.saturating_add(rows) {
            scroll = self.cursor.saturating_add(1).saturating_sub(rows);
        }
        let scroll = scroll.min(items.saturating_sub(1));
        let end = scroll.saturating_add(rows).min(items);
        scroll..end
    }
}

impl Dialog for SynchronizeDialog {
    fn id(&self) -> DialogId {
        DialogId::Synchronize
    }

    fn title(&self) -> String {
        format!("Synchronize: {}  ->  {}", self.left, self.right)
    }

    fn size_hint(&self) -> (u16, u16) {
        let rows = u16::try_from(self.plan.items().len().max(1)).unwrap_or(u16::MAX);
        (78, rows.saturating_add(4).clamp(7, 24))
    }

    /// None: the dialog is a list plus two whole-list keys (`Space`, `Tab`),
    /// all printed in the footer. There is no focusable control to jump to,
    /// exactly as the queue view has none.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return DialogOutcome::Accept(DialogResult::Synchronize(Box::new(self.plan.clone())));
        }
        if key.is_next_control() {
            self.next_mode();
            return DialogOutcome::Consumed;
        }
        match key.press.code {
            KeyCode::Char(' ') => {
                self.plan.cycle(self.cursor);
                DialogOutcome::Consumed
            }
            KeyCode::Up => {
                self.move_by(-1);
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.move_by(1);
                DialogOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.move_by(-10);
                DialogOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.move_by(10);
                DialogOutcome::Consumed
            }
            KeyCode::Home => {
                self.cursor = 0;
                DialogOutcome::Consumed
            }
            KeyCode::End => {
                self.cursor = self.plan.items().len().saturating_sub(1);
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn layout(&mut self, area: Rect) {
        let (_, list, _) = Self::regions(area);
        self.scroll = self.visible(usize::from(list.height)).start;
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, list, footer) = Self::regions(area);
        let fit = |s: &str, w: u16| {
            text::fit_left(s, usize::from(w), text::Crop::End, ellipsis(style.ascii))
        };
        if let Some(rect) = header {
            draw_text(
                f,
                rect,
                &fit(&self.header_text(), rect.width),
                style.body(),
                style.ascii,
            );
        }
        if self.plan.items().is_empty() {
            if let Some(rect) = row(list, 0) {
                draw_text(
                    f,
                    rect,
                    "the two trees are already the same",
                    style.body(),
                    style.ascii,
                );
            }
        } else {
            let range = self.visible(usize::from(list.height));
            for (offset, index) in range.enumerate() {
                let Some(rect) = row(list, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                    break;
                };
                let Some(item) = self.plan.items().get(index) else {
                    break;
                };
                let selected = index == self.cursor;
                draw_text(
                    f,
                    rect,
                    &fit(&Self::row_text(item), rect.width),
                    style.button(selected),
                    style.ascii,
                );
            }
        }
        if let Some(rect) = footer {
            draw_text(
                f,
                rect,
                &fit(Self::footer_text(), rect.width),
                style.body(),
                style.ascii,
            );
        }
    }
}

/// A byte count with thousands separators, the panel's own spelling.
fn size(bytes: u64) -> String {
    crate::panel::format::thousands(bytes)
}

#[cfg(test)]
#[path = "synchronize_tests.rs"]
mod tests;
