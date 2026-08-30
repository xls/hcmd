//! The theme picker, with the theme applied as the cursor moves.
//!
//! ```text
//!   ┌─ /home/you/src ──────────────┐┌──── Theme ────┐
//!   │ ..                     <DIR> ││ ayu-dark      │
//!   │ src                    <DIR> ││ ayu-light     │
//!   │ Cargo.toml           1,204   ││ blue          │
//!   │ README.md           18,442   ││▓catppuccin▓▓▓▓│
//!   ├──────────────────────────────┤│ dracula       │
//!   │ 19 k in 3 files, 1 dir       ││ everforest    │
//!   └──────────────────────────────┘└───────────────┘
//! ```
//!
//! # Why it is narrow
//!
//! A theme is judged against a screen, not against a swatch. A picker that
//! covered the panels would show the one thing the theme does not have to get
//! right - its own list - and hide everything it does: the cursor bar, the
//! marked files, the borders of the panel that does not have focus, the key
//! bar. So it asks for the width of its longest name and no more, and the
//! program behind it stays visible and stays live.
//!
//! # Why the preview is the selection
//!
//! Moving the cursor changes the running theme. There is no "apply" step and
//! no second key, because the question a person is asking is "what does this
//! look like", and an answer that needs a keystroke first is an answer to a
//! different question.
//!
//! `Esc` puts back the theme that was running when the picker opened, so
//! looking costs nothing. `Enter` keeps what is on screen and writes it into
//! `config.toml`, so it is still there the next time the program starts.
//!
//! # Why some names have a `+` after them
//!
//! The list opens on what is on disk and never waits for anything. In the
//! background the project's repository is asked what themes it has, and a
//! name it has that this machine does not is added with a `+` after it. Such
//! a row cannot be previewed - there is nothing to apply yet - so the cursor
//! moving onto it leaves the screen as it was, and `Enter` fetches it, writes
//! it into `themes/`, and then applies it like any other. An answer that
//! never comes changes nothing.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle};
use crate::input::quicksearch::quick_match;
use crate::input::{DialogId, KeyCode};

/// How many rows the list asks for, and how far `PageUp`/`PageDown` move.
///
/// Tall enough to show a useful run of the twenty-one shipped names without
/// being so tall that it reaches the key bar on a short terminal. The
/// framework clamps it to the screen either way.
const LIST_ROWS: u16 = 14;

/// The widest the box will ask to be, borders included.
///
/// The point of the picker is to leave the program visible behind it, so it
/// is capped rather than fitted to the longest name at any cost.
const MAX_WIDTH: u16 = 26;

/// What is drawn after a name the repository has and this machine does not.
///
/// A marker rather than a second column or a different colour: the box is
/// narrow on purpose, and a colour would have to come out of the theme being
/// previewed, which is the one thing on screen that is changing. The name in
/// front of it is the real name - it is what quick search matches and what
/// `Enter` chooses - so the marker can be read as "and this one has to be
/// fetched first" and otherwise ignored.
const NOT_INSTALLED: &str = " +";

/// What asking the repository for its themes comes back with: the names it
/// has, or why it could not be asked.
///
/// The failure travels as a string rather than being reported where it
/// happened, because it happens on a worker thread and the only place it is
/// worth saying anything about it is the status line.
pub type CatalogueAnswer = Result<Vec<String>, String>;

/// The theme picker.
#[derive(Debug)]
pub struct ThemeDialog {
    /// Every name that can be chosen, in the order they are offered.
    names: Vec<String>,
    cursor: usize,
    /// What was running when this opened, so `Esc` can put it back.
    original: String,
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
    /// Of those names, the ones only the repository has.
    ///
    /// They are in `names` like any other, because the list is one list and
    /// the cursor walks all of it. This is what says a row cannot be
    /// previewed and has to be fetched before it can be applied.
    remote_only: BTreeSet<String>,
    /// The repository's answer, if one was asked for.
    ///
    /// `try_recv`ed once a frame and never waited on: the picker is drawn the
    /// instant it opens with what is on disk, and the extra names appear
    /// whenever they arrive, or never.
    incoming: Option<Receiver<CatalogueAnswer>>,
}

impl ThemeDialog {
    /// Open on `current`, which is the theme the session is running.
    ///
    /// The cursor starts on it rather than at the top: the first thing a
    /// person wants to know is what they are looking at now, and stepping away
    /// from it and back is how a comparison is made.
    pub fn new(names: Vec<String>, current: &str) -> Self {
        let cursor = names.iter().position(|n| n == current).unwrap_or(0);
        Self {
            names,
            cursor,
            original: current.to_string(),
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
            remote_only: BTreeSet::new(),
            incoming: None,
        }
    }

    /// Attach the answer the repository is being asked for.
    #[must_use]
    pub fn expecting(mut self, incoming: Receiver<CatalogueAnswer>) -> Self {
        self.incoming = Some(incoming);
        self
    }

    /// The repository's answer, once, if it has arrived.
    ///
    /// The receiver is dropped with the answer, so the question is asked and
    /// read exactly once per opening of the picker.
    pub fn take_answer(&mut self) -> Option<CatalogueAnswer> {
        let answer = self.incoming.as_ref()?.try_recv();
        match answer {
            Ok(answer) => {
                self.incoming = None;
                Some(answer)
            }
            // Still in flight; ask again next frame.
            Err(TryRecvError::Empty) => None,
            // The task ended without sending, which nothing here does, but a
            // receiver that can never produce is worth dropping either way.
            Err(TryRecvError::Disconnected) => {
                self.incoming = None;
                None
            }
        }
    }

    /// Add the names the repository has that this machine does not.
    ///
    /// Returns how many were added. The cursor stays on the name it was on
    /// rather than on the row it was on, because the list it was in has just
    /// grown underneath it and the preview must not jump.
    pub fn offer_remote(&mut self, names: Vec<String>) -> usize {
        let here = self.names.get(self.cursor).cloned();
        let before = self.names.len();
        for name in names {
            if self.names.contains(&name) {
                continue;
            }
            self.remote_only.insert(name.clone());
            self.names.push(name);
        }
        let added = self.names.len().saturating_sub(before);
        if added > 0 {
            self.names.sort();
            if let Some(here) = here
                && let Some(at) = self.names.iter().position(|n| *n == here)
            {
                self.cursor = at;
            }
        }
        added
    }

    /// Is this a name the repository offered that is not on this machine?
    pub fn is_remote_only(&self, name: &str) -> bool {
        self.remote_only.contains(name)
    }

    /// How a name is drawn: itself, and a marker if it is not here yet.
    fn label(&self, name: &str) -> String {
        if self.is_remote_only(name) {
            format!("{name}{NOT_INSTALLED}")
        } else {
            name.to_string()
        }
    }

    /// Match the list's quick search to the panel's configured rules.
    #[must_use]
    pub const fn with_quick_search(mut self, mode: QuickSearchMode, case: QuickSearchCase) -> Self {
        self.mode = mode;
        self.case = case;
        self
    }

    /// The name under the cursor: what the screen should be showing.
    ///
    /// This is what the event loop reads every frame to keep the preview in
    /// step, so it is the whole interface between the picker and the theme.
    pub fn selected(&self) -> Option<&str> {
        self.names.get(self.cursor).map(String::as_str)
    }

    /// The theme that was running when this opened.
    pub fn original(&self) -> &str {
        &self.original
    }

    /// Which row the cursor is on, for tests.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The quick-search buffer, for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// Move the cursor, ending any quick search in progress.
    fn move_cursor(&mut self, to: usize) {
        if self.names.is_empty() {
            return;
        }
        self.cursor = to.min(self.names.len().saturating_sub(1));
        self.quick.clear();
    }

    /// One keystroke of panel quick search over the names.
    ///
    /// A character that matches nothing is refused rather than typed, exactly
    /// as it is in a panel: twenty-one names is enough that typing `sol` is
    /// faster than arrowing, and a buffer that can hold a name that is not
    /// there would move the cursor nowhere and preview nothing.
    fn quick_search(&mut self, ch: char) -> bool {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let Some(found) = self
            .names
            .iter()
            .position(|name| quick_match(name, &candidate, self.mode, self.case))
        else {
            return false;
        };
        self.quick = candidate;
        self.cursor = found;
        true
    }

    /// The rows visible in a body `height` rows tall: the page the cursor is
    /// on, so moving within a page does not scroll.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.names.is_empty() {
            return 0..0;
        }
        let page = self.cursor / height;
        let start = page.saturating_mul(height);
        let end = start.saturating_add(height).min(self.names.len());
        start..end
    }
}

impl Dialog for ThemeDialog {
    fn id(&self) -> DialogId {
        DialogId::Theme
    }

    fn title(&self) -> String {
        "Theme".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .names
            .iter()
            .map(|n| u16::try_from(self.label(n).chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(8);
        // Two for the borders and two for the padding either side of a name.
        let want = widest.saturating_add(4).min(MAX_WIDTH);
        (want, LIST_ROWS.saturating_add(2))
    }

    /// Without this the preview does nothing at all.
    ///
    /// `Dialog::as_any` defaults to `None`, and the event loop reaches this
    /// dialog by downcasting to ask which name is selected. A `None` here is
    /// not a compile error and not a panic: the downcast simply fails, the
    /// preview never runs, and the picker looks like a list that does not work.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    /// The repository's answer is read through this.
    ///
    /// Adding names is a change to the dialog, and the event loop is the only
    /// thing holding it, so the same downcast the preview does is done
    /// mutably once a frame.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        // None, deliberately: every letter is quick search over the names, and
        // an accelerator would take one of them away from the thing the list
        // is for.
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.selected() {
                Some(name) => DialogOutcome::Accept(DialogResult::Text(name.to_string())),
                None => DialogOutcome::Cancel,
            };
        }
        let last = self.names.len().saturating_sub(1);
        let page = usize::from(LIST_ROWS);
        match key.press.code {
            KeyCode::Up => self.move_cursor(self.cursor.saturating_sub(1)),
            KeyCode::Down => self.move_cursor(self.cursor.saturating_add(1)),
            KeyCode::PageUp => self.move_cursor(self.cursor.saturating_sub(page)),
            KeyCode::PageDown => self.move_cursor(self.cursor.saturating_add(page)),
            KeyCode::Home => self.move_cursor(0),
            KeyCode::End => self.move_cursor(last),
            KeyCode::Backspace => {
                // Same as a panel's: rubbing out a character re-runs the
                // shorter search rather than leaving the cursor where the
                // longer one put it.
                self.quick.pop();
                let buffer = self.quick.clone();
                if !buffer.is_empty()
                    && let Some(found) = self
                        .names
                        .iter()
                        .position(|name| quick_match(name, &buffer, self.mode, self.case))
                {
                    self.cursor = found;
                }
            }
            KeyCode::Char(c) => {
                if !self.quick_search(c) {
                    return DialogOutcome::Ignored;
                }
            }
            _ => return DialogOutcome::Ignored,
        }
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let height = usize::from(area.height);
        let visible = self.window(height);
        let width = usize::from(area.width);

        for (offset, index) in visible.clone().enumerate() {
            let Ok(row) = u16::try_from(offset) else {
                break;
            };
            if row >= area.height {
                break;
            }
            let Some(name) = self.names.get(index) else {
                break;
            };
            let selected = index == self.cursor;
            // The panel's own cursor bar, which is what a selected row is
            // everywhere else in the program.
            let row_style = if selected {
                style.row_cursor(true).add_modifier(Modifier::BOLD)
            } else {
                style.body()
            };
            // Padded to the full width so the cursor bar is a bar rather than
            // a highlight the length of the word.
            let label = self.label(name);
            let text = format!(" {label:<width$}", width = width.saturating_sub(1));
            let text = crate::ui::text::fit_left(
                &text,
                width,
                crate::ui::text::Crop::End,
                super::ellipsis(style.ascii),
            );
            let rect = Rect {
                x: area.x,
                y: area.y.saturating_add(row),
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, row_style))),
                rect,
            );
        }
    }
}
