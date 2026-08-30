//! the context menu (`Shift+F10`).
//!
//! > The desktop's own menu for the entry under the cursor, or for the marked
//! > entries when there are any.
//! >
//! > There is no portable way to ask for one. `Shift+F10` therefore builds it
//! > from what the design already knows - the associations, "open with" for each -
//! > and adds the operations that are keys elsewhere: copy, move, rename,
//! > delete, properties. It is a menu over this application's own vocabulary,
//! > not the file manager menu a desktop environment would show, and it does
//! > not pretend otherwise.
//!
//! ```text
//! +- Context menu - holiday.jpg ----------+
//! | Open the entry under the cursor Enter |
//! | imv                                   |
//! | Open with...                          |
//! | View the file under the cursor     F3 |
//! | Copy the selection to the other... F5 |
//! | Properties                            |
//! +---------------------------------------+
//! ```
//!
//! # Absent rather than greyed
//!
//! > On a remote panel or inside an archive the entries that
//! > cannot apply are absent rather than greyed: "open with" needs a local
//! > path, and an entry that is never available is noise rather than
//! > information.
//!
//! [`ContextMenuDialog::items_for`] takes `local`, which is
//! `path.local_path().is_some()`, and simply does not build the three kinds of
//! row that need a path the kernel can hand to another program. Nothing here
//! draws a greyed row, and there is no "disabled" state for one to be in -
//! which is the invariant I12, asserted by building the
//! same entry's list twice.
//!
//! # Every operation row shows its key
//!
//! For the reason the menu rows do: this is the other place in the
//! program where an operation is named in words, and naming it without its key
//! would teach the menu instead of the keyboard. The key comes from the live
//! keymap through [`ContextMenuDialog::with_keys`], so a rebinding shows.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::{ellipsis, row};
use crate::config::{Matcher, ModeMatch, OpenConfig};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_text};
use crate::input::{Action, DialogId, KeyCode};
use crate::ui::text::{self, Crop};

/// The gap between a row's label and the key that runs it.
const GAP: usize = 2;

/// The operations the design names, in the order they are offered.
///
/// > and adds the operations that are keys elsewhere: copy, move, rename,
/// > delete
///
/// `View` and `Edit` come before them because they are the two the own
/// execute prompt offers, so the same file answered from either door reads
/// the same way.
const OPERATIONS: [Action; 6] = [
    Action::View,
    Action::Edit,
    Action::Copy,
    Action::Move,
    Action::RenameInPlace,
    Action::Delete,
];

/// One row. Either an application to open with, or an operation that is a key
/// elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextItem {
    /// A user handler from `config.toml` that matches this entry, named by its
    /// program.
    Handler {
        /// Which handler, by index into `config.open.handlers`.
        index: usize,
        /// What the row says: the program's own name.
        label: String,
    },
    /// the "Open with...", which opens
    /// [`crate::ui::dialog::OpenWithDialog`].
    OpenWith,
    /// An operation that has a key: `open`, `view`, `edit`, `copy`, `move`,
    /// `rename_in_place`, `delete`.
    Action(Action),
    /// the `properties`, which is the one row that is not a key
    /// anywhere. See the design.
    Properties,
}

impl ContextItem {
    /// What the row says.
    ///
    /// An operation is named by its own description, which is the string
    /// the page and the menu already use for it: three
    /// spellings of one action would be three to keep in step.
    pub fn label(&self) -> String {
        match self {
            Self::Handler { label, .. } => label.clone(),
            Self::OpenWith => "Open with...".to_string(),
            Self::Action(action) => action.description().to_string(),
            Self::Properties => "Properties".to_string(),
        }
    }
}

/// What the context menu answers with.
///
/// [`crate::dialog::DialogResult`] has no context-shaped variant and gains
/// none for four kinds of row, so the answer travels in
/// [`crate::dialog::DialogResult::Text`] and comes back through
/// [`ContextChoice::parse`] - exactly as [`crate::ui::dialog::JobAction`]
/// already does. The encoding is deliberately boring, a word and an argument,
/// so a test can assert on it and a log line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextChoice {
    /// Run `config.open.handlers[index]` on the subject.
    Handler(usize),
    /// Open the chooser.
    OpenWith,
    /// Dispatch this action, exactly as its key would.
    Action(Action),
    /// Show the properties.
    Properties,
}

impl ContextChoice {
    /// The choice a row stands for.
    pub const fn of(item: &ContextItem) -> Self {
        match item {
            ContextItem::Handler { index, .. } => Self::Handler(*index),
            ContextItem::OpenWith => Self::OpenWith,
            ContextItem::Action(action) => Self::Action(*action),
            ContextItem::Properties => Self::Properties,
        }
    }

    /// The wire form carried in [`crate::dialog::DialogResult::Text`].
    pub fn encode(&self) -> String {
        match self {
            Self::Handler(index) => format!("handler {index}"),
            Self::OpenWith => "open_with".to_string(),
            Self::Action(action) => format!("action {}", action.id()),
            Self::Properties => "properties".to_string(),
        }
    }

    /// Read one back, or `None` when the text is not one of ours.
    pub fn parse(text: &str) -> Option<Self> {
        match text.split_once(' ') {
            Some(("handler", index)) => index.parse().ok().map(Self::Handler),
            Some(("action", id)) => Action::from_id(id).map(Self::Action),
            Some(_) => None,
            None => match text {
                "open_with" => Some(Self::OpenWith),
                "properties" => Some(Self::Properties),
                _ => None,
            },
        }
    }
}

/// The extension of a bare file name, without the dot and without a leading
/// dot counting as one, so `.bashrc` has none.
///
/// The same rule [`crate::vfs::Entry::split_name`] applies, over a name rather
/// than over an entry, because this list is built from what the caller read
/// and not from a listing.
fn extension_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(at) => name.get(at.saturating_add(1)..).unwrap_or(""),
    }
}

/// Does a `mime = "text/*"` pattern cover this type?
///
/// A trailing `*` is a prefix and everything else is an exact, case-folded
/// match. That is the whole of the pattern language the example
/// uses, and a fuller glob would be a second matcher for one wildcard.
fn mime_matches(pattern: &str, mime: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => mime
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix)),
        None => pattern.eq_ignore_ascii_case(mime),
    }
}

/// Does one `[[open.handlers]]` rule match this entry?
///
/// The same shape [`crate::ui::filetype`] applies to a `[[filetypes]]` rule -
/// a rule may constrain the extension, the MIME type, the mode bits or any
/// combination, all stated ones must hold, and a rule that constrains nothing
/// matches nothing rather than everything.
///
/// `mode` is the entry's `st_mode`, so `mode = "dir"` and `mode = "symlink"`
/// are answered from the file-type bits. A backend that reports `0` - an
/// archive, a remote listing - says nothing about the type, so such a rule
/// simply does not match there, which is the same answer as "no handler".
fn handler_matches(matcher: &Matcher, name: &str, mime: &str, mode: u32) -> bool {
    /// The `st_mode` file-type mask.
    const KIND: u32 = 0o170_000;
    /// `S_IFDIR`.
    const DIR: u32 = 0o040_000;
    /// `S_IFLNK`.
    const LINK: u32 = 0o120_000;

    let mut said_something = false;

    if let Some(want) = matcher.mode {
        said_something = true;
        let ok = match want {
            ModeMatch::Exec => mode & 0o111 != 0 && mode & KIND != DIR,
            ModeMatch::Dir => mode & KIND == DIR,
            ModeMatch::Symlink => mode & KIND == LINK,
        };
        if !ok {
            return false;
        }
    }

    if !matcher.ext.is_empty() {
        said_something = true;
        let ext = extension_of(name);
        if ext.is_empty()
            || !matcher
                .ext
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        {
            return false;
        }
    }

    if let Some(pattern) = matcher.mime.as_deref() {
        said_something = true;
        if !mime_matches(pattern, mime) {
            return false;
        }
    }

    said_something
}

/// What a handler row is called: the program, without the directories in
/// front of it.
///
/// A `$VAR` program is shown as it is written, because expanding it is
/// [`crate::ops::open::expand_command`]'s job and happens when the row is
/// chosen - a menu that guessed at `$EDITOR` and guessed differently from the
/// launch would be worse than one that shows the rule.
fn handler_label(command: &[String]) -> String {
    let Some(program) = command.first() else {
        return "(no command)".to_string();
    };
    program
        .rsplit('/')
        .next()
        .filter(|word| !word.is_empty())
        .unwrap_or(program)
        .to_string()
}

/// the `Shift+F10`.
///
/// > It is a menu over this application's own vocabulary, not the file manager
/// > menu a desktop environment would show, and it does not pretend otherwise.
pub struct ContextMenuDialog {
    subject: String,
    marked: usize,
    items: Vec<ContextItem>,
    /// The key beside each row, in `items`' order. Empty until
    /// [`ContextMenuDialog::with_keys`] fills it, because a dialog is built
    /// with everything it needs and never holds an `&App`.
    keys: Vec<String>,
    cursor: usize,
}

impl ContextMenuDialog {
    /// Built from what the design already knows plus the operations that
    /// are keys elsewhere.
    ///
    /// `marked` is how many entries the menu acts on, which is what
    /// the "or for the marked entries when there are any" makes
    /// the subject; `subject` names the one under the cursor.
    pub fn new(subject: String, marked: usize, items: Vec<ContextItem>) -> Self {
        Self {
            subject,
            marked,
            items,
            keys: Vec::new(),
            cursor: 0,
        }
    }

    /// Fill in the key shown beside each operation row, from the live keymap.
    ///
    /// A builder rather than two more arguments to [`ContextMenuDialog::new`],
    /// whose shape the design fixes. Without it the rows draw
    /// without their keys, which is a poorer menu but never a broken one.
    #[must_use]
    pub fn with_keys(mut self, keymap: &crate::config::Keymap, enhanced: bool) -> Self {
        self.keys = self
            .items
            .iter()
            .map(|item| match item {
                // A handler, the chooser and `Properties` are the three rows
                // that are not a key anywhere, and an empty column is how they
                // say so.
                ContextItem::Handler { .. } | ContextItem::OpenWith | ContextItem::Properties => {
                    String::new()
                }
                ContextItem::Action(action) => super::menu::keys_for(keymap, *action, enhanced),
            })
            .collect();
        self
    }

    /// The items the design gives this entry, in its order.
    ///
    /// Pure and separate from the dialog, so the absent-rather-than-greyed
    /// rule is tested without building a dialog.
    ///
    /// `local` is `path.local_path().is_some()`. **On a remote panel or inside
    /// an archive the entries that cannot apply are absent rather than
    /// greyed**: "an entry that is never available is noise
    /// rather than information". Those are exactly the handler rows,
    /// `Open with...` and `Edit` - the first two because launching another
    /// program needs a path the kernel can pass it, and `Edit` because
    /// the temp-file round trip for a non-local file is not built.
    /// Everything else works through the `Vfs`
    /// on any backend.
    pub fn items_for(
        cfg: &OpenConfig,
        name: &str,
        mime: &str,
        mode: u32,
        local: bool,
    ) -> Vec<ContextItem> {
        let mut items = vec![ContextItem::Action(Action::Open)];
        if local {
            for (index, handler) in cfg.handlers.iter().enumerate() {
                if handler_matches(&handler.matcher, name, mime, mode) {
                    items.push(ContextItem::Handler {
                        index,
                        label: handler_label(&handler.command),
                    });
                }
            }
            items.push(ContextItem::OpenWith);
        }
        for action in OPERATIONS {
            if action == Action::Edit && !local {
                continue;
            }
            items.push(ContextItem::Action(action));
        }
        items.push(ContextItem::Properties);
        items
    }

    /// The rows, for the renderer and for tests.
    pub fn items(&self) -> &[ContextItem] {
        &self.items
    }

    /// Which row the cursor is on.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The row under the cursor.
    pub fn selected(&self) -> Option<&ContextItem> {
        self.items.get(self.cursor)
    }

    /// The key drawn beside one row, which is empty for the three kinds that
    /// have none.
    fn keys_of(&self, index: usize) -> &str {
        self.keys.get(index).map_or("", String::as_str)
    }

    /// One row as it is drawn: label left, key right.
    fn row_text(&self, index: usize, width: usize, ascii: bool) -> String {
        let Some(item) = self.items.get(index) else {
            return String::new();
        };
        let label = item.label();
        let keys = self.keys_of(index);
        let keys_w = text::width(keys);
        if keys_w == 0 {
            return text::fit_left(&label, width, Crop::End, ellipsis(ascii));
        }
        let room = width.saturating_sub(keys_w.saturating_add(GAP));
        if room == 0 {
            return text::fit_left(&label, width, Crop::End, ellipsis(ascii));
        }
        format!(
            "{}{}{keys}",
            text::fit_left(&label, room, Crop::End, ellipsis(ascii)),
            " ".repeat(GAP)
        )
    }

    /// The widest row it would like.
    fn natural_width(&self) -> usize {
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let keys = text::width(self.keys_of(index));
                let gap = if keys == 0 { 0 } else { GAP };
                text::width(&item.label())
                    .saturating_add(gap)
                    .saturating_add(keys)
            })
            .max()
            .unwrap_or(0)
    }

    /// Move the cursor, wrapping at both ends the way the menu bar does.
    fn walk(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len().saturating_sub(1);
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

    /// The rows visible in a window of `rows`, keeping the cursor inside it.
    ///
    /// Pure, for the reason [`crate::ui::dialog::menu::MenuDialog`]'s twin is:
    /// `render` takes `&self` and this dialog keeps no interior-mutable
    /// scroll offset.
    fn window(cursor: usize, len: usize, rows: usize) -> std::ops::Range<usize> {
        if rows == 0 || len == 0 {
            return 0..0;
        }
        if len <= rows {
            return 0..len;
        }
        let start = cursor
            .saturating_sub(rows / 2)
            .min(len.saturating_sub(rows));
        start..start.saturating_add(rows).min(len)
    }
}

impl Dialog for ContextMenuDialog {
    fn id(&self) -> DialogId {
        DialogId::ContextMenu
    }

    /// Named for what it acts on, which the design makes the marked
    /// entries when there are any and the entry under the cursor otherwise.
    fn title(&self) -> String {
        if self.marked > 1 {
            format!("Context menu - {} marked entries", self.marked)
        } else {
            format!("Context menu - {}", self.subject)
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        let width = u16::try_from(self.natural_width().saturating_add(4)).unwrap_or(u16::MAX);
        let height = u16::try_from(self.items.len().saturating_add(2)).unwrap_or(u16::MAX);
        (width.max(24), height)
    }

    /// None, and that is a decision rather than an omission.
    ///
    ///
    /// The rows are a list walked by the arrow keys, each already showing its
    /// own key, and there is no control for `Alt`+letter to jump to. A letter
    /// spent on a mnemonic here would be a letter the list could not offer to
    /// a quick search later.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.selected() {
                Some(item) => {
                    DialogOutcome::Accept(DialogResult::Text(ContextChoice::of(item).encode()))
                }
                None => DialogOutcome::Consumed,
            };
        }
        match key.press.code {
            KeyCode::Up => {
                self.walk(-1);
                DialogOutcome::Consumed
            }
            KeyCode::Down => {
                self.walk(1);
                DialogOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.walk(-10);
                DialogOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.walk(10);
                DialogOutcome::Consumed
            }
            KeyCode::Home => {
                self.cursor = 0;
                DialogOutcome::Consumed
            }
            KeyCode::End => {
                self.cursor = self.items.len().saturating_sub(1);
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows = usize::from(area.height);
        let range = Self::window(self.cursor, self.items.len(), rows);
        let width = usize::from(area.width);
        for (offset, index) in range.enumerate() {
            let Some(rect) = row(area, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                break;
            };
            let row_style = if index == self.cursor {
                style.row_cursor(true)
            } else {
                style.body()
            };
            let text = text::fit_left(
                &self.row_text(index, width, style.ascii),
                width,
                Crop::End,
                ellipsis(style.ascii),
            );
            draw_text(f, rect, &text, row_style, style.ascii);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Keymap, OpenHandler, Theme};
    use crate::dialog::DialogKey;
    use crate::input::{KeyPress, Milestone};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// `config.toml`'s own example, plus a mode rule.
    fn handlers() -> OpenConfig {
        OpenConfig {
            handlers: vec![
                OpenHandler {
                    matcher: Matcher {
                        ext: vec!["png".to_string(), "jpg".to_string()],
                        mime: None,
                        mode: None,
                    },
                    command: vec!["/usr/bin/imv".to_string(), "{file}".to_string()],
                },
                OpenHandler {
                    matcher: Matcher {
                        ext: Vec::new(),
                        mime: Some("text/*".to_string()),
                        mode: None,
                    },
                    command: vec!["$EDITOR".to_string(), "{file}".to_string()],
                },
                OpenHandler {
                    matcher: Matcher {
                        ext: Vec::new(),
                        mime: None,
                        mode: Some(ModeMatch::Exec),
                    },
                    command: vec!["gdb".to_string(), "{file}".to_string()],
                },
            ],
            ..OpenConfig::default()
        }
    }

    fn dialog(local: bool) -> ContextMenuDialog {
        let items = ContextMenuDialog::items_for(
            &handlers(),
            "holiday.jpg",
            "image/jpeg",
            0o100_644,
            local,
        );
        ContextMenuDialog::new("holiday.jpg".to_string(), 1, items)
            .with_keys(&Keymap::builtin(), true)
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn render(d: &ContextMenuDialog, w: u16, h: u16, ascii: bool) -> Buffer {
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

    fn render_inner(d: &ContextMenuDialog, w: u16, h: u16, ascii: bool) -> Buffer {
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
    fn it_lists_what_spec_7_6_3_says_it_lists() {
        // built "from what the design already knows - the associations,
        // 'open with' for each - and adds the operations that are keys
        // elsewhere: copy, move, rename, delete, properties".
        let items =
            ContextMenuDialog::items_for(&handlers(), "holiday.jpg", "image/jpeg", 0o100_644, true);
        assert_eq!(
            items,
            vec![
                ContextItem::Action(Action::Open),
                ContextItem::Handler {
                    index: 0,
                    label: "imv".to_string()
                },
                ContextItem::OpenWith,
                ContextItem::Action(Action::View),
                ContextItem::Action(Action::Edit),
                ContextItem::Action(Action::Copy),
                ContextItem::Action(Action::Move),
                ContextItem::Action(Action::RenameInPlace),
                ContextItem::Action(Action::Delete),
                ContextItem::Properties,
            ]
        );
    }

    #[test]
    fn a_remote_or_in_archive_entry_loses_exactly_three_kinds_of_row() {
        // Invariant I12. "On a remote panel or inside an archive the entries
        // that cannot apply are absent rather than greyed", and an entry that
        // is never available "is noise rather than information" - so the
        // difference is exactly the handlers, the chooser and Edit, and
        // nothing else moves.
        let local =
            ContextMenuDialog::items_for(&handlers(), "notes.txt", "text/plain", 0o100_644, true);
        let remote =
            ContextMenuDialog::items_for(&handlers(), "notes.txt", "text/plain", 0o100_644, false);
        let dropped: Vec<&ContextItem> =
            local.iter().filter(|item| !remote.contains(item)).collect();
        assert_eq!(
            dropped,
            vec![
                &ContextItem::Handler {
                    index: 1,
                    label: "$EDITOR".to_string()
                },
                &ContextItem::OpenWith,
                &ContextItem::Action(Action::Edit),
            ]
        );
        assert!(
            remote.iter().all(|item| local.contains(item)),
            "nothing appears only on the remote list"
        );
        // And what is left works through the Vfs on any backend.
        assert_eq!(
            remote,
            vec![
                ContextItem::Action(Action::Open),
                ContextItem::Action(Action::View),
                ContextItem::Action(Action::Copy),
                ContextItem::Action(Action::Move),
                ContextItem::Action(Action::RenameInPlace),
                ContextItem::Action(Action::Delete),
                ContextItem::Properties,
            ]
        );
    }

    #[test]
    fn no_row_is_ever_greyed_because_there_is_no_such_row() {
        // The other half of I12, stated as a type: a row has no disabled
        // state to be drawn in, so "absent rather than greyed" cannot be got
        // wrong by a renderer.
        for local in [true, false] {
            for item in
                ContextMenuDialog::items_for(&handlers(), "a.png", "image/png", 0o100_755, local)
            {
                match item {
                    ContextItem::Handler { .. }
                    | ContextItem::OpenWith
                    | ContextItem::Action(_)
                    | ContextItem::Properties => {}
                }
            }
        }
    }

    #[test]
    fn every_operation_row_is_an_action_this_release_implements() {
        // The same rule the menu bar lives under: a row that
        // names an operation names one that exists.
        for item in ContextMenuDialog::items_for(&handlers(), "a.png", "image/png", 0o100_644, true)
        {
            if let ContextItem::Action(action) = item {
                assert!(
                    action.milestone() <= Milestone::V07,
                    "{}: belongs to {}",
                    action.id(),
                    action.milestone()
                );
            }
        }
    }

    #[test]
    fn a_matching_handler_is_named_by_its_program() {
        // step 1's own example: `match = { ext = ["png", ...] }`
        // with `command = ["imv", "{file}"]`.
        let items =
            ContextMenuDialog::items_for(&handlers(), "a.PNG", "image/png", 0o100_644, true);
        assert!(
            items.contains(&ContextItem::Handler {
                index: 0,
                label: "imv".to_string()
            }),
            "the extension match folds case, and the directories are dropped"
        );
        // A `text/*` rule matches by MIME and not by name.
        let items =
            ContextMenuDialog::items_for(&handlers(), "notes", "text/plain", 0o100_644, true);
        assert!(items.contains(&ContextItem::Handler {
            index: 1,
            label: "$EDITOR".to_string()
        }));
        // And nothing matches a type no rule names.
        let items = ContextMenuDialog::items_for(
            &handlers(),
            "a.bin",
            "application/x-tar",
            0o100_644,
            true,
        );
        assert!(
            !items
                .iter()
                .any(|item| matches!(item, ContextItem::Handler { .. })),
            "{items:?}"
        );
    }

    #[test]
    fn a_mode_rule_reads_the_mode_bits_and_never_the_extension() {
        // "Executability is determined only from the mode bits
        // - never from the extension." Invariant I16 as it reaches this menu.
        let items =
            ContextMenuDialog::items_for(&handlers(), "deploy", "text/plain", 0o100_755, true);
        assert!(items.contains(&ContextItem::Handler {
            index: 2,
            label: "gdb".to_string()
        }));
        let items =
            ContextMenuDialog::items_for(&handlers(), "deploy.sh", "text/plain", 0o100_644, true);
        assert!(
            !items.contains(&ContextItem::Handler {
                index: 2,
                label: "gdb".to_string()
            }),
            "a .sh without +x is data"
        );
        // A directory's mode bits say so even though the name does not.
        let dir = Matcher {
            ext: Vec::new(),
            mime: None,
            mode: Some(ModeMatch::Dir),
        };
        assert!(handler_matches(&dir, "src", "inode/directory", 0o040_755));
        assert!(!handler_matches(&dir, "src", "inode/directory", 0o100_755));
        // A backend with no mode bits at all says nothing, which is the same
        // answer as "no handler" rather than a wrong one.
        assert!(!handler_matches(&dir, "src", "inode/directory", 0));
    }

    #[test]
    fn a_rule_that_constrains_nothing_matches_nothing() {
        // The same rule `ui::filetype` applies to a `[[filetypes]]` rule: an
        // empty `match = {}` is inert rather than covering every file.
        let empty = Matcher {
            ext: Vec::new(),
            mime: None,
            mode: None,
        };
        assert!(!handler_matches(&empty, "a.png", "image/png", 0o100_644));
    }

    #[test]
    fn a_mime_pattern_is_a_prefix_or_an_exact_match() {
        assert!(mime_matches("text/*", "text/plain"));
        assert!(mime_matches("TEXT/*", "text/plain"));
        assert!(!mime_matches("text/*", "image/png"));
        assert!(mime_matches("image/png", "image/png"));
        assert!(!mime_matches("image/png", "image/pn"));
        assert!(mime_matches("*", "anything/at-all"));
    }

    #[test]
    fn an_extension_is_what_the_panel_calls_one() {
        assert_eq!(extension_of("holiday.jpg"), "jpg");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of(".bashrc"), "");
        assert_eq!(extension_of("README"), "");
        assert_eq!(extension_of("trailing."), "");
    }

    #[test]
    fn every_operation_row_shows_its_key_and_the_others_show_none() {
        // The reason the rows do: this is the other place an
        // operation is named in words, and naming it without its key would
        // teach the menu instead of the keyboard.
        let d = dialog(true);
        for (index, item) in d.items().iter().enumerate() {
            match item {
                ContextItem::Action(_) => assert!(
                    !d.keys_of(index).is_empty(),
                    "{index}: {:?} has no key beside it",
                    item.label()
                ),
                ContextItem::Handler { .. } | ContextItem::OpenWith | ContextItem::Properties => {
                    assert_eq!(d.keys_of(index), "", "{index}: {:?}", item.label());
                }
            }
        }
        let out = dump(&render(&d, 90, 20, false));
        assert!(out.contains("F5"), "{out}");
        assert!(out.contains("Enter"), "{out}");
    }

    #[test]
    fn a_dialog_built_without_keys_still_draws_every_row() {
        // `with_keys` is a builder, so a caller that forgets it gets a poorer
        // menu and never a broken one.
        let items =
            ContextMenuDialog::items_for(&handlers(), "a.png", "image/png", 0o100_644, true);
        let d = ContextMenuDialog::new("a.png".to_string(), 1, items);
        let out = dump(&render(&d, 90, 20, false));
        assert!(out.contains("Properties"), "{out}");
        assert!(out.contains("Open with..."), "{out}");
    }

    #[test]
    fn enter_answers_with_the_row_and_esc_closes() {
        let mut d = dialog(true);
        // Row 1 is the `imv` handler.
        d.handle_key(&key(KeyCode::Down));
        let outcome = d.handle_key(&key(KeyCode::Enter));
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(text)) => {
                assert_eq!(ContextChoice::parse(&text), Some(ContextChoice::Handler(0)));
            }
            other => panic!("expected a handler, got {other:?}"),
        }
        let mut d = dialog(true);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn every_choice_survives_the_round_trip() {
        // The encoding is what `dialog_accepted` reads, so it is asserted
        // rather than assumed.
        for choice in [
            ContextChoice::Handler(0),
            ContextChoice::Handler(41),
            ContextChoice::OpenWith,
            ContextChoice::Action(Action::Delete),
            ContextChoice::Properties,
        ] {
            assert_eq!(ContextChoice::parse(&choice.encode()), Some(choice));
        }
        assert_eq!(ContextChoice::parse(""), None);
        assert_eq!(ContextChoice::parse("handler x"), None);
        assert_eq!(ContextChoice::parse("action nonsuch"), None);
        assert_eq!(ContextChoice::parse("something else"), None);
    }

    #[test]
    fn the_arrows_walk_the_rows_and_wrap() {
        let mut d = dialog(true);
        assert_eq!(d.cursor(), 0);
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.cursor(), d.items().len() - 1);
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 0);
        d.handle_key(&key(KeyCode::End));
        assert_eq!(d.cursor(), d.items().len() - 1);
        d.handle_key(&key(KeyCode::Home));
        assert_eq!(d.cursor(), 0);
    }

    #[test]
    fn it_is_named_for_what_it_acts_on() {
        // "for the entry under the cursor, or for the marked
        // entries when there are any".
        assert_eq!(dialog(true).title(), "Context menu - holiday.jpg");
        let items = ContextMenuDialog::items_for(&handlers(), "a", "text/plain", 0, true);
        let many = ContextMenuDialog::new("a".to_string(), 4, items);
        assert_eq!(many.title(), "Context menu - 4 marked entries");
    }

    #[test]
    fn it_declares_no_mnemonics_and_that_is_a_decision() {
        assert!(dialog(true).mnemonic_letters().is_empty());
    }

    #[test]
    fn it_renders_at_every_size_the_spec_names() {
        // 60x15 is a supported size.
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            for ascii in [false, true] {
                let out = dump(&render(&dialog(true), w, h, ascii));
                assert!(out.contains("Properties"), "{w}x{h} ascii={ascii}:\n{out}");
            }
        }
    }

    #[test]
    fn every_glyph_it_draws_has_an_ascii_form() {
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (24, 6)] {
            let inner = dump(&render_inner(&dialog(true), w, h, true));
            assert!(inner.is_ascii(), "{w}x{h}:\n{inner}");
        }
    }

    #[test]
    fn the_window_keeps_the_cursor_in_view_and_never_runs_past_the_end() {
        for len in 0usize..14 {
            for rows in 0usize..8 {
                for cursor in 0..len.max(1) {
                    let range = ContextMenuDialog::window(cursor, len, rows);
                    assert!(range.end <= len, "{len}/{rows}/{cursor}");
                    assert!(range.len() <= rows.min(len));
                    if rows > 0 && len > 0 {
                        assert!(range.contains(&cursor), "{cursor} outside {range:?}");
                    }
                }
            }
        }
    }
}
