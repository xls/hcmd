//! the "Open with..." chooser.
//!
//! > **Open with...** offers a chooser listing the applications the desktop
//! > advertises for that type, read with `freedesktop-desktop-entry`. It is
//! > reachable from the execute prompt and from the context menu
//! > (`Shift+F10`).
//!
//! ```text
//!            ┌───────── Open with: holiday.jpg ─────────┐
//!            │ Image Viewer                             │
//!            │ GNU Image Manipulation Program           │
//!            │ Firefox                                  │
//!            │            [ OK ]   [ Cancel ]           │
//!            └──────────────────────────────────────────┘
//! ```
//!
//! The list is read by [`crate::ops::open::applications_for`] **before** the
//! dialog is built, because a dialog may not touch the filesystem. Typing
//! quick-searches the names on the panel's own rules, which is the same
//! behaviour the design gives the device picker and the design gives the
//! host list.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::dialog::{
    Accel, Accelerated, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing,
    draw_mnemonic_buttons, draw_text,
};
use crate::input::quicksearch::quick_match;
use crate::input::{DialogId, KeyCode};
use crate::ops::open::DesktopApp;

/// The list, which is where the dialog opens.
const LIST: usize = 0;
/// The `OK` button.
const OK: usize = 1;
/// The `Cancel` button.
const CANCEL: usize = 2;

/// What an empty list says.
///
/// A row rather than an empty box: the design promises a chooser, and a
/// chooser that shows nothing looks broken rather than informative. A machine
/// with no desktop installed is the ordinary way to get here.
///
pub const NOTHING_ADVERTISED: &str = "the desktop advertises no application for this type";

/// the `Alt` mnemonics for this dialog.
///
/// The list itself has **no** letter: its letters are what quick search types,
/// and spending one on a mnemonic would be a letter the search could no longer
/// reach (the pattern the design established). `o` is `OK`
/// and `n` is `Cancel`, both their program-wide meanings.
pub const MNEMONICS: &[(usize, char)] = &[(OK, 'o'), (CANCEL, 'n')];

/// How many rows the list asks for, and how far `PageUp`/`PageDown` move.
const LIST_ROWS: u16 = 8;

/// the "Open with..." chooser.
#[derive(Debug)]
pub struct OpenWithDialog {
    name: String,
    apps: Vec<DesktopApp>,
    cursor: usize,
    /// The panel-style quick-search buffer over the application names.
    ///
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
    ring: FocusRing,
}

impl OpenWithDialog {
    /// Built with the list already read, because a dialog may not touch the
    /// filesystem.
    pub fn new(name: String, apps: Vec<DesktopApp>) -> Self {
        Self {
            name,
            apps,
            cursor: 0,
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
            ring: FocusRing::new(MNEMONICS.len().saturating_add(1)),
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

    /// The applications it is showing.
    pub fn apps(&self) -> &[DesktopApp] {
        &self.apps
    }

    /// Which application is selected.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected application, if there is one.
    pub fn selected(&self) -> Option<&DesktopApp> {
        self.apps.get(self.cursor)
    }

    /// The quick-search buffer, for the status fragment and for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// Which control has focus.
    pub const fn focused(&self) -> usize {
        self.ring.index()
    }

    /// Move the cursor, ending any quick search in progress.
    fn move_cursor(&mut self, to: usize) {
        if self.apps.is_empty() {
            return;
        }
        self.cursor = to.min(self.apps.len().saturating_sub(1));
        self.quick.clear();
    }

    /// One keystroke of panel quick search over the names.
    ///
    /// A character that matches nothing is **refused** rather than typed, so
    /// the buffer always names an application that is on the screen - which is
    /// what makes `Backspace` step back through matches rather than through
    /// characters that never matched. The same rule
    /// `crate::remote::connect::ConnectDialog` keeps for the host list.
    fn quick_search(&mut self, ch: char) -> bool {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let Some(found) = self
            .apps
            .iter()
            .position(|app| quick_match(&app.name, &candidate, self.mode, self.case))
        else {
            return false;
        };
        self.quick = candidate;
        self.cursor = found;
        true
    }

    /// The answer: the selected application's id, which the event loop looks
    /// up in the list it built.
    ///
    /// The **id** and not the index, because the event loop rebuilt nothing
    /// between the two and an index would be a promise that it did not.
    fn accept(&self) -> DialogOutcome {
        match self.selected() {
            Some(app) => DialogOutcome::Accept(DialogResult::Text(app.id.clone())),
            // Nothing to choose: `Enter` on the empty row closes without an
            // answer rather than answering with an empty string, which
            // `dialog_accepted` would have to guard against.
            None => DialogOutcome::Cancel,
        }
    }

    /// The rows visible in a body `height` rows tall - the page the cursor is
    /// on, so moving within a page does not scroll.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.apps.is_empty() {
            return 0..0;
        }
        let start = self
            .cursor
            .saturating_div(height)
            .saturating_mul(height)
            .min(self.apps.len().saturating_sub(1));
        start..start.saturating_add(height).min(self.apps.len())
    }
}

impl Accelerated for OpenWithDialog {
    /// Ring indices rather than an enum: three controls is under
    /// the five-control floor.
    type Control = usize;

    fn mnemonics(&self) -> &'static [(usize, char)] {
        MNEMONICS
    }

    fn accel(&self, control: usize) -> Accel<usize> {
        match control {
            OK if self.apps.is_empty() => Accel::Absent,
            OK | CANCEL => Accel::Press,
            _ => Accel::Focus,
        }
    }

    fn focus_control(&mut self, control: usize) {
        self.ring.set(control);
    }

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, _control: usize) {}

    fn press(&mut self, control: usize) -> DialogOutcome {
        match control {
            OK => self.accept(),
            CANCEL => DialogOutcome::Cancel,
            _ => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for OpenWithDialog {
    fn id(&self) -> DialogId {
        DialogId::OpenWith
    }

    fn title(&self) -> String {
        format!("Open with: {}", self.name)
    }

    fn size_hint(&self) -> (u16, u16) {
        let widest = self
            .apps
            .iter()
            .map(|app| crate::ui::text::width(&app.name))
            .max()
            .unwrap_or(crate::ui::text::width(NOTHING_ADVERTISED))
            .saturating_add(4);
        let want = u16::try_from(widest).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.apps.len()).unwrap_or(u16::MAX);
        (
            want.clamp(44, 90),
            rows.clamp(1, LIST_ROWS).saturating_add(3),
        )
    }

    /// `OK` and `Cancel`; the list declares none.
    ///
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action`.
        //
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            // `Esc` ends a running quick search first, and closes the dialog
            // only when there is none - the rule the design gives a panel and
            // `ConnectDialog` already keeps for a list inside a dialog.
            if self.quick.is_empty() {
                return DialogOutcome::Cancel;
            }
            self.quick.clear();
            return DialogOutcome::Consumed;
        }
        if key.is_accept() {
            return match self.ring.index() {
                CANCEL => DialogOutcome::Cancel,
                _ => self.accept(),
            };
        }
        let last = self.apps.len().saturating_sub(1);
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
                    // a dialog consumes all input, so an
                    // unclaimed key is swallowed rather than passed on.
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
        let body_rows = usize::from(area.height.saturating_sub(1));
        if self.apps.is_empty() {
            if let Some(rect) = super::row(area, 0) {
                draw_text(f, rect, NOTHING_ADVERTISED, style.body(), style.ascii);
            }
        } else {
            let focused = self.ring.is(LIST);
            for (offset, index) in self.window(body_rows).enumerate() {
                let Some(rect) = super::row(area, u16::try_from(offset).unwrap_or(u16::MAX)) else {
                    break;
                };
                let Some(app) = self.apps.get(index) else {
                    break;
                };
                let text = crate::ui::text::fit_left(
                    &app.name,
                    usize::from(rect.width),
                    crate::ui::text::Crop::End,
                    super::ellipsis(style.ascii),
                );
                let mut row_style = style.body();
                if index == self.cursor {
                    row_style = if focused {
                        style.body().fg(style.cursor_fg).bg(style.cursor_bg)
                    } else {
                        style
                            .body()
                            .fg(style.cursor_fg_unfocused)
                            .bg(style.cursor_bg_unfocused)
                    }
                    .add_modifier(Modifier::BOLD);
                }
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, row_style))).style(style.body()),
                    rect,
                );
            }
        }
        let buttons = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        // `usize::MAX` is "no button has focus", which is the truth while the
        // list does: highlighting `OK` then would say `Enter` presses a button
        // when it actually opens the selected row.
        let focused = if self.ring.is(LIST) {
            usize::MAX
        } else {
            self.ring.index().saturating_sub(OK)
        };
        draw_mnemonic_buttons(
            f,
            buttons,
            &[("OK", Some('o')), ("Cancel", Some('n'))],
            focused,
            style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app(name: &str, id: &str) -> DesktopApp {
        DesktopApp {
            name: name.to_string(),
            id: id.to_string(),
            exec: vec![id.to_string()],
        }
    }

    fn dialog() -> OpenWithDialog {
        OpenWithDialog::new(
            "holiday.jpg".to_string(),
            vec![
                app("Image Viewer", "imv.desktop"),
                app("Inkscape", "inkscape.desktop"),
                app("Firefox", "firefox.desktop"),
            ],
        )
    }

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn text(ch: char) -> DialogKey {
        DialogKey::raw(KeyPress::plain(KeyCode::Char(ch)))
    }

    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    fn chosen(outcome: &DialogOutcome) -> Option<String> {
        match outcome {
            DialogOutcome::Accept(DialogResult::Text(id)) => Some(id.clone()),
            _ => None,
        }
    }

    fn screen(d: &OpenWithDialog, w: u16, h: u16, ascii: bool) -> String {
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
    fn it_answers_with_the_chosen_applications_id() {
        // "answers `DialogResult::Text(app_id)`;
        // the event loop looks the id up in the list it built."
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(&key(KeyCode::Down)),
            DialogOutcome::Consumed
        ));
        assert_eq!(
            chosen(&d.handle_key(&key(KeyCode::Enter))),
            Some("inkscape.desktop".to_string())
        );
    }

    #[test]
    fn typing_quick_searches_the_names_on_the_panels_rules() {
        // applied to a list inside a dialog.
        let mut d = dialog();
        assert!(matches!(d.handle_key(&text('f')), DialogOutcome::Consumed));
        assert_eq!(d.quick_buffer(), "f");
        assert_eq!(d.selected().map(|a| a.id.as_str()), Some("firefox.desktop"));
        // A character that matches nothing is refused rather than typed.
        assert!(matches!(d.handle_key(&text('z')), DialogOutcome::Ignored));
        assert_eq!(d.quick_buffer(), "f");
        // And an arrow ends the search.
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.quick_buffer(), "");
    }

    #[test]
    fn esc_ends_a_running_quick_search_before_it_closes_the_dialog() {
        let mut d = dialog();
        d.handle_key(&text('f'));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.quick_buffer(), "");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn an_empty_list_is_a_row_that_says_so_and_never_an_empty_box() {
        // "An empty list draws one unselectable
        // row saying the desktop advertises nothing for this type, so the
        // dialog is never an empty box."
        let mut d = OpenWithDialog::new("a.bin".to_string(), Vec::new());
        let out = screen(&d, 80, 24, false);
        assert!(out.contains("advertises no application"), "{out}");
        assert_eq!(d.selected(), None);
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Cancel
        ));
        assert!(matches!(
            d.handle_key(&key(KeyCode::Down)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.cursor(), 0);
        // `Alt+O` is swallowed rather than answering with nothing.
        assert!(matches!(d.handle_key(&alt('o')), DialogOutcome::Consumed));
    }

    #[test]
    fn ok_and_cancel_answer_to_their_alt_letters() {
        let mut d = dialog();
        assert_eq!(
            chosen(&d.handle_key(&alt('o'))),
            Some("imv.desktop".to_string())
        );
        let mut d = dialog();
        assert!(matches!(d.handle_key(&alt('n')), DialogOutcome::Cancel));
        assert_eq!(d.mnemonic_letters(), vec!['o', 'n']);
    }

    #[test]
    fn it_draws_at_every_size_the_spec_declares_usable() {
        // invariant I20.
        let d = dialog();
        for ascii in [false, true] {
            for (w, h) in [(200u16, 50u16), (80, 24), (60, 15)] {
                let out = screen(&d, w, h, ascii);
                assert!(out.contains("Image Viewer"), "{w}x{h}:\n{out}");
                assert!(out.contains("Cancel"), "{w}x{h}:\n{out}");
            }
        }
    }
}
