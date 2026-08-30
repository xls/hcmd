//! `Alt+F8`, command history.
//!
//! ```text
//!            ┌──────────── Command history ─────────────┐
//!            │ git status                               │
//!            │ cargo test                               │
//!            │ ls -la                                   │
//!            └──────────────────────────────────────────┘
//! ```
//!
//! # Whose history this is, and when there is one at all
//!
//! the design gives the history to the shell - "History, completion, `Ctrl+R`,
//! vi or emacs bindings... all of it is whatever the user has configured" - and
//! the design is explicit that "nothing is pushed anywhere here and there is no
//! history file - one history that the shell already maintains beats two that
//! disagree". So **with a live shell there is no list of this program's own to
//! show**, and `Alt+F8` says where the history actually lives instead. That is
//! a decision, not a stub.
//!
//! This dialog is for the other case: `console.enabled = false`, a shell that
//! would not start, a shell that has died, or a headless `App`. the design's
//! fallback command line maintains its own list, capped by
//! `console.history_size`, and that list is what this shows.
//!
//! # It puts the command on the command line, it does not run it
//!
//! `Enter` on the command line is what runs a command. A history
//! dialog that ran things would be a second way to execute, with a different
//! confirmation story, for no gain: the chosen line lands on the command line
//! where it can be edited first, which is what a history is for.
//!
//! # Why the key is `Alt+F8`
//!
//! the function-key table has `| Alt+F8 | command history |` and
//! nothing anywhere in the design binds `Ctrl+Shift+H`.
//!

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle};
use crate::input::quicksearch::quick_match;
use crate::input::{DialogId, KeyCode};

/// What an empty history says.
///
/// A row rather than an empty box, for the reason
/// [`super::openwith::NOTHING_ADVERTISED`] is one: a dialog showing nothing
/// reads as a rendering fault.
pub const EMPTY: &str = "nothing has been run from the command line yet";

/// How many rows the list asks for, and how far `PageUp`/`PageDown` move.
const LIST_ROWS: u16 = 12;

/// `Alt+F8`'s list over the fallback command line's own history.
#[derive(Debug)]
pub struct HistoryDialog {
    /// Newest first, which is the order a history is read in.
    entries: Vec<String>,
    cursor: usize,
    /// The panel-style quick-search buffer over the commands.
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
}

impl HistoryDialog {
    /// Built from `crate::input::CommandLine::history`, which is **oldest
    /// first**; this reverses it, because the newest command is the one being
    /// reached for and it belongs under the cursor rather than at the bottom
    /// of a scroll.
    pub fn new(history: &[String]) -> Self {
        Self {
            entries: history.iter().rev().cloned().collect(),
            cursor: 0,
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
        }
    }

    /// Match the list's quick search to the panel's configured rules.
    ///
    #[must_use]
    pub const fn with_quick_search(mut self, mode: QuickSearchMode, case: QuickSearchCase) -> Self {
        self.mode = mode;
        self.case = case;
        self
    }

    /// The commands it is showing, newest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Which command is selected.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected command, if there is one.
    pub fn selected(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(String::as_str)
    }

    /// The quick-search buffer, for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// Move the cursor, ending any quick search in progress.
    fn move_cursor(&mut self, to: usize) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = to.min(self.entries.len().saturating_sub(1));
        self.quick.clear();
    }

    /// One keystroke of panel quick search over the commands.
    ///
    /// A character that matches nothing is refused rather than typed, exactly
    /// as it is in a panel and in [`super::OpenWithDialog`].
    fn quick_search(&mut self, ch: char) -> bool {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let Some(found) = self
            .entries
            .iter()
            .position(|command| quick_match(command, &candidate, self.mode, self.case))
        else {
            return false;
        };
        self.quick = candidate;
        self.cursor = found;
        true
    }

    /// The rows visible in a body `height` rows tall - the page the cursor is
    /// on, so moving within a page does not scroll.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.entries.is_empty() {
            return 0..0;
        }
        let start = self
            .cursor
            .saturating_div(height)
            .saturating_mul(height)
            .min(self.entries.len().saturating_sub(1));
        start..start.saturating_add(height).min(self.entries.len())
    }
}

impl Dialog for HistoryDialog {
    fn id(&self) -> DialogId {
        DialogId::History
    }

    fn title(&self) -> String {
        "Command history".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .entries
            .iter()
            .map(|line| crate::ui::text::width(line))
            .max()
            .unwrap_or(crate::ui::text::width(EMPTY))
            .saturating_add(4);
        let want = u16::try_from(widest).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.entries.len()).unwrap_or(u16::MAX);
        (
            want.clamp(44, 100),
            rows.clamp(1, LIST_ROWS).saturating_add(2),
        )
    }

    /// **No letters.**
    ///
    /// Every row is chosen with an arrow or by typing, and the typing is
    /// the quick search over the commands themselves - so a letter
    /// spent on a mnemonic would be a letter the search could no longer type.
    /// There is no button here to give one to either: `Enter` takes the
    /// selected line and `Esc` closes. The pattern is
    /// the design's, and the design lists
    /// this dialog among the three that declare none.
    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            // `Esc` ends a running quick search first and closes only when
            // there is none (the rule, inside a dialog).
            if self.quick.is_empty() {
                return DialogOutcome::Cancel;
            }
            self.quick.clear();
            return DialogOutcome::Consumed;
        }
        if key.is_accept() {
            return match self.selected() {
                // The command goes **on** the command line, not to the shell;
                // see the module documentation.
                Some(command) => DialogOutcome::Accept(DialogResult::Text(command.to_string())),
                None => DialogOutcome::Cancel,
            };
        }
        let last = self.entries.len().saturating_sub(1);
        let page = usize::from(LIST_ROWS);
        match key.press.code {
            KeyCode::Up => self.move_cursor(self.cursor.saturating_sub(1)),
            KeyCode::Down => self.move_cursor(self.cursor.saturating_add(1)),
            KeyCode::PageUp => self.move_cursor(self.cursor.saturating_sub(page)),
            KeyCode::PageDown => self.move_cursor(self.cursor.saturating_add(page)),
            KeyCode::Home => self.move_cursor(0),
            KeyCode::End => self.move_cursor(last),
            KeyCode::Backspace => {
                self.quick.pop();
            }
            _ => {
                let Some(ch) = key.text() else {
                    return DialogOutcome::Ignored;
                };
                if !self.quick_search(ch) {
                    return DialogOutcome::Ignored;
                }
            }
        }
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.entries.is_empty() {
            if let Some(rect) = super::row(area, 0) {
                crate::dialog::draw_text(f, rect, EMPTY, style.body(), style.ascii);
            }
            return;
        }
        let rows = usize::from(area.height);
        for (offset, index) in self.window(rows).enumerate() {
            let Some(rect) = super::row(area, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                break;
            };
            let Some(command) = self.entries.get(index) else {
                break;
            };
            let text = crate::ui::text::fit_left(
                command,
                usize::from(rect.width),
                crate::ui::text::Crop::End,
                super::ellipsis(style.ascii),
            );
            let mut row_style = style.body();
            if index == self.cursor {
                row_style = style
                    .body()
                    .fg(style.cursor_fg)
                    .bg(style.cursor_bg)
                    .add_modifier(Modifier::BOLD);
            }
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, row_style))).style(style.body()),
                rect,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::KeyPress;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn history() -> Vec<String> {
        ["ls -la", "cargo test", "git status"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn dialog() -> HistoryDialog {
        HistoryDialog::new(&history())
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn text(ch: char) -> DialogKey {
        DialogKey::raw(KeyPress::plain(KeyCode::Char(ch)))
    }

    fn chosen(outcome: &DialogOutcome) -> Option<String> {
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(line)) => Some(line.clone()),
            _ => None,
        }
    }

    fn screen(d: &HistoryDialog, w: u16, h: u16, ascii: bool) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, ascii);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).map_or(" ", ratatui::buffer::Cell::symbol))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_newest_command_is_the_one_under_the_cursor() {
        // `CommandLine::history` is oldest first; a history is read newest
        // first.
        let d = dialog();
        assert_eq!(d.selected(), Some("git status"));
        assert_eq!(
            d.entries(),
            &[
                "git status".to_string(),
                "cargo test".to_string(),
                "ls -la".to_string()
            ]
        );
    }

    #[test]
    fn enter_hands_back_the_command_and_never_runs_it() {
        // `Enter` on the command line is what runs a command, so
        // the answer is text for the command line and nothing else.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(
            chosen(&d.handle_key(&key(KeyCode::Enter))),
            Some("cargo test".to_string())
        );
    }

    #[test]
    fn typing_quick_searches_the_commands() {
        // the rules, applied to the list.
        let mut d = dialog();
        assert!(matches!(d.handle_key(&text('c')), DialogOutcome::Consumed));
        assert_eq!(d.selected(), Some("cargo test"));
        assert!(matches!(d.handle_key(&text('z')), DialogOutcome::Ignored));
        assert_eq!(d.quick_buffer(), "c");
        // `Esc` ends the search before it closes the dialog.
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Consumed
        ));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.cursor(), 0);
        d.handle_key(&key(KeyCode::End));
        assert_eq!(d.cursor(), 2);
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 2);
        d.handle_key(&key(KeyCode::Home));
        assert_eq!(d.cursor(), 0);
    }

    #[test]
    fn an_empty_history_is_a_row_that_says_so() {
        let mut d = HistoryDialog::new(&[]);
        let out = screen(&d, 80, 24, false);
        assert!(out.contains("nothing has been run"), "{out}");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
        assert_eq!(d.window(5), 0..0);
    }

    #[test]
    fn it_declares_no_mnemonic_letters() {
        // the rows are chosen by arrows and quick
        // search, and a letter spent on a mnemonic is a letter the search could
        // no longer type.
        assert!(dialog().mnemonic_letters().is_empty());
    }

    #[test]
    fn it_draws_at_every_size_the_spec_declares_usable() {
        // invariant I20.
        let d = dialog();
        for ascii in [false, true] {
            for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
                let out = screen(&d, w, h, ascii);
                assert!(out.contains("git status"), "{w}x{h}:\n{out}");
                assert!(out.contains("Command history"), "{w}x{h}:\n{out}");
            }
        }
    }
}
