//! the result list.
//!
//! > **Result list** shows what happened per file, including failures, and is
//! > what the bottom status line points at after a run.
//!
//! ```text
//! ┌──────────────── Rename results: 3 of 4 ─────────────────┐
//! │ …/media/a.txt              -> b.txt                     │
//! │ …/media/c.txt              -> d.txt                     │
//! │ …/media/e.txt              -> f.txt                     │
//! │ …/media/g.txt              -> h.txt   h.txt already ex… │
//! │                          [ Close ]                      │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! The lines come from [`crate::rename::exec::result_lines`], which is pure and
//! is built from the pairs the job was given and the summary it produced - so
//! what this shows and what happened cannot drift apart.
//!
//! # Why it is not a [`super::tabbed::Table`]
//!
//! A table's cells are handed no width, and the `from` column of this list is a
//! path that has to be cropped **from the left** so the filename survives.
//! The list is one line per file with no sortable columns and
//! no counter to renumber, so it draws its own rows and keeps the crop where
//! the width is known.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{crop_left, ellipsis, row};
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle,
    draw_mnemonic_buttons,
};
use crate::input::{DialogId, KeyCode};
use crate::rename::exec::ResultLine;
use crate::ui::text;

/// The narrowest the `from` column is drawn before the arrow and the target
/// are dropped and the line becomes the source alone.
const MIN_FROM: usize = 12;

/// How many columns the `-> ` separator costs.
const ARROW_WIDTH: usize = 4;

/// The one button, which is also the whole of this dialog's focus.
const CLOSE: usize = 0;

/// the `Alt` mnemonics for this dialog.
///
/// One control and one letter: `c` is the program-wide `Close`. `Enter` and
/// `Esc` press the same button, and now so does `Alt+C`.
pub const MNEMONICS: &[(usize, char)] = &[(CLOSE, 'c')];

/// the result list.
#[derive(Debug)]
pub struct RenameResultDialog {
    lines: Vec<ResultLine>,
    cursor: usize,
    /// How far `PageUp`/`PageDown` move; the visible window is otherwise a
    /// pure function of the cursor, exactly as [`super::tabbed::Table`]'s is.
    page: usize,
}

impl RenameResultDialog {
    /// A list over `lines`, in the order the job was given them.
    pub fn new(lines: Vec<ResultLine>) -> Self {
        Self {
            lines,
            cursor: 0,
            page: 10,
        }
    }

    /// The lines it is showing.
    pub fn lines(&self) -> &[ResultLine] {
        &self.lines
    }

    /// Which line the cursor is on.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// How many of the lines succeeded.
    pub fn succeeded(&self) -> usize {
        self.lines.iter().filter(|l| l.error.is_none()).count()
    }

    /// The rows visible in a body `height` rows tall - the page the cursor is
    /// on, so moving within a page does not scroll.
    pub fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.lines.is_empty() {
            return 0..0;
        }
        let start = self
            .cursor
            .saturating_div(height)
            .saturating_mul(height)
            .min(self.lines.len().saturating_sub(1));
        start..start.saturating_add(height).min(self.lines.len())
    }

    /// One line, laid out for a row `width` cells wide.
    ///
    /// The source is cropped from the left so its filename survives;
    /// the target and the error take what is left, and a row
    /// too narrow for all three drops them from the right rather than showing
    /// three unreadable fragments.
    pub fn line_text(&self, index: usize, width: usize, ascii: bool) -> String {
        let Some(line) = self.lines.get(index) else {
            return String::new();
        };
        if width <= MIN_FROM {
            return crop_left(&line.from, width, ascii);
        }
        let arrow = if ascii { "-> " } else { "\u{2192} " };
        let tail = match &line.error {
            Some(error) => format!("{}{}  {error}", arrow, line.to),
            None => format!("{}{}", arrow, line.to),
        };
        let tail_room = width.saturating_sub(MIN_FROM).saturating_sub(1);
        let tail = text::truncate(&tail, tail_room, text::Crop::End, ellipsis(ascii));
        let from_room = width
            .saturating_sub(text::width(&tail))
            .saturating_sub(1)
            .max(MIN_FROM);
        let from = crop_left(&line.from, from_room, ascii);
        let from = text::fit_left(&from, from_room, text::Crop::End, ellipsis(ascii));
        format!("{from} {tail}")
    }

    /// The title, which is the "what happened" in one line.
    fn heading(&self) -> String {
        format!(
            "Rename results: {} of {}",
            self.succeeded(),
            self.lines.len()
        )
    }

    /// Body rows, once the button row is taken out.
    const fn body_rows(area: Rect) -> usize {
        (area.height as usize).saturating_sub(1)
    }
}

impl Accelerated for RenameResultDialog {
    /// Ring indices rather than an enum: one control is well under
    /// the five-control floor. There is no
    /// [`crate::dialog::FocusRing`] at all here - the list is a report, and the
    /// button is always the thing `Enter` presses.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    fn accel(&self, control: usize) -> Accel<usize> {
        match control {
            CLOSE => Accel::Press,
            _ => Accel::Focus,
        }
    }

    /// There is one control and it always has the keyboard, so there is
    /// nothing to move.
    fn focus_control(&mut self, _control: usize) {}

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            CLOSE => DialogOutcome::Accept(DialogResult::None),
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for RenameResultDialog {
    fn id(&self) -> DialogId {
        DialogId::RenameResult
    }

    fn title(&self) -> String {
        self.heading()
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .lines
            .iter()
            .map(|l| {
                text::width(&l.from)
                    .saturating_add(text::width(&l.to))
                    .saturating_add(ARROW_WIDTH)
                    .saturating_add(l.error.as_deref().map_or(0, text::width))
            })
            .max()
            .unwrap_or(40);
        let w = u16::try_from(widest.saturating_add(4)).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.lines.len().saturating_add(3)).unwrap_or(u16::MAX);
        (w.clamp(44, 100), rows.clamp(5, 26))
    }

    /// `Alt+C` alone.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action`.
        //
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if key.is_cancel() || key.is_accept() {
            // One button, and every key presses it: the list is a report and
            // there is nothing to answer.
            return self.press(CLOSE);
        }
        if self.lines.is_empty() {
            return DialogOutcome::Ignored;
        }
        let last = self.lines.len().saturating_sub(1);
        let page = self.page.max(1);
        self.cursor = match key.press.code {
            KeyCode::Up => self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor.saturating_add(1).min(last),
            KeyCode::PageUp => self.cursor.saturating_sub(page),
            KeyCode::PageDown => self.cursor.saturating_add(page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return DialogOutcome::Ignored,
        };
        DialogOutcome::Consumed
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body = Self::body_rows(area);
        let failed = Style::new()
            .fg(style.button_focus)
            .bg(style.bg)
            .add_modifier(Modifier::BOLD);
        for (offset, index) in self.window(body).enumerate() {
            let Some(rect) = row(area, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                break;
            };
            let text = self.line_text(index, usize::from(rect.width), style.ascii);
            let mut line_style = if self.lines.get(index).is_some_and(|l| l.error.is_some()) {
                failed
            } else {
                style.body()
            };
            if index == self.cursor {
                line_style = line_style.add_modifier(Modifier::REVERSED);
            }
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(text, line_style))).style(style.body()),
                rect,
            );
        }
        if let Some(rect) = row(area, u16::try_from(body).unwrap_or(u16::MAX)) {
            draw_mnemonic_buttons(f, rect, &[("Close", Some('c'))], CLOSE, style);
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

    fn line(from: &str, to: &str, error: Option<&str>) -> ResultLine {
        ResultLine {
            from: from.to_string(),
            to: to.to_string(),
            error: error.map(str::to_string),
        }
    }

    /// `Alt`+letter, the keystroke the design is about.
    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(
            KeyCode::Char(c),
            crate::input::KeyModifiers::ALT,
        ))
    }

    /// Every character drawn with [`Modifier::UNDERLINED`], folded to lower
    /// case. the underline, read off the buffer rather than off the
    /// table.
    fn underlined(d: &RenameResultDialog, w: u16, h: u16) -> Vec<char> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    out.extend(cell.symbol().chars());
                }
            }
        }
        out.iter().map(|c| c.to_ascii_lowercase()).collect()
    }

    fn dialog() -> RenameResultDialog {
        RenameResultDialog::new(vec![
            line("/srv/media/a.txt", "b.txt", None),
            line("/srv/media/c.txt", "d.txt", None),
            line("/srv/media/e.txt", "f.txt", Some("f.txt already exists")),
        ])
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    #[test]
    fn the_title_counts_what_happened() {
        assert_eq!(dialog().title(), "Rename results: 2 of 3");
        assert_eq!(
            RenameResultDialog::new(Vec::new()).title(),
            "Rename results: 0 of 0"
        );
    }

    #[test]
    fn a_line_keeps_the_filename_when_the_path_will_not_fit() {
        // the tail of a path is what identifies it.
        let d = dialog();
        let text = d.line_text(0, 40, false);
        assert!(text.contains("a.txt"), "{text:?}");
        assert!(text.contains("b.txt"), "{text:?}");
        assert!(text::width(&text) <= 40, "{text:?}");

        let narrow = d.line_text(0, 20, false);
        assert!(narrow.contains("a.txt"), "{narrow:?}");
        assert!(text::width(&narrow) <= 20, "{narrow:?}");

        // Every width is safe, including the hopeless ones.
        for width in 0usize..80 {
            for index in 0..4 {
                let out = d.line_text(index, width, false);
                assert!(text::width(&out) <= width, "{width}: {out:?}");
            }
        }
    }

    #[test]
    fn a_failure_says_why_on_its_own_line() {
        let d = dialog();
        let text = d.line_text(2, 60, false);
        assert!(text.contains("already exists"), "{text:?}");
    }

    #[test]
    fn the_cursor_walks_the_list_and_both_keys_close_it() {
        let mut d = dialog();
        assert_eq!(d.cursor(), 0);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Down)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.cursor(), 1);
        assert!(matches!(
            d.handle_key(&key(KeyCode::End)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.cursor(), 2);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Down)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.cursor(), 2, "and it stops at the end");

        for code in [KeyCode::Esc, KeyCode::Enter] {
            let mut d = dialog();
            assert!(matches!(
                d.handle_key(&key(code)),
                DialogOutcome::Accept(DialogResult::None)
            ));
        }
    }

    #[test]
    fn an_empty_result_list_is_still_a_dialog() {
        let mut d = RenameResultDialog::new(Vec::new());
        assert!(matches!(
            d.handle_key(&key(KeyCode::Down)),
            DialogOutcome::Ignored
        ));
        assert_eq!(d.window(5), 0..0);
        assert_eq!(d.line_text(0, 20, false), "");
    }

    #[test]
    fn it_draws_at_every_size_the_spec_declares_usable() {
        // 60x15 is a supported size, not a degraded one.
        let d = dialog();
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| {
                    crate::dialog::draw(f, &d, f.area(), &style);
                })
                .expect("draw");
            let buf = terminal.backend().buffer().clone();
            let screen: String = (0..h)
                .map(|y| {
                    (0..w)
                        .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("a.txt"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("Close"), "{w}x{h}:\n{screen}");
        }
    }

    #[test]
    fn the_one_button_answers_to_its_alt_letter_too() {
        // one control, one letter, and `c` is the program-wide `Close`.
        // `Enter`, `Esc` and `Alt+C` all press the same button through
        // `Accelerated::press`.
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&alt('c')),
            DialogOutcome::Accept(DialogResult::None)
        ));
        assert_eq!(d.mnemonic_letters(), vec!['c']);
    }

    #[test]
    fn an_unclaimed_alt_letter_neither_moves_the_cursor_nor_closes() {
        // A dialog consumes all input, and `Alt`+letter is never
        // a list movement: `Alt+Down` is not a letter and `Alt+J` is not bound.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 1);
        assert!(matches!(d.handle_key(&alt('z')), DialogOutcome::Ignored));
        assert_eq!(d.cursor(), 1);
    }

    #[test]
    fn every_mnemonic_is_underlined_in_its_own_label() {
        let d = dialog();
        let mut want = d.mnemonic_letters();
        let mut got = underlined(&d, 80, 24);
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want, "underlines on screen");
    }
}
