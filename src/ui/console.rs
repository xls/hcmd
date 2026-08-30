//! the console view - the shell's screen, drawn cell for cell.
//!
//! > `Ctrl+O` **hides the panels**, so the screen is the shell and nothing
//! > else - the full scrollback, exactly as if the file manager were not
//! > running. … It is not a split, not a pane, and not a shrunken terminal: it
//! > is the same screen the shell would have had on its own.
//!
//! This is the second of the two views of one [`vt100::Screen`]. The first is
//! [`super::cmdline`], which draws the command line - the bottom
//! rows of that same screen, in place at the foot of the panel view. This one
//! draws all of it, over the whole terminal, and the panels are not drawn at
//! all: [`is_backdrop`] is the question a frame asks.
//!
//! Both paint from [`crate::console::Console::screen`] directly, so nothing is
//! copied to draw a frame and nothing re-themes what the shell already styled.
//! **The shell's colours are the shell's**: the design declares no `console.*`
//! slot group, deliberately, and the conversion is
//! [`super::cmdline::style_of`] - the one in the codebase,
//! so a prompt drawn by starship comes out of
//! here looking exactly as it does in the terminal this application was started
//! from.
//!
//! The three pieces of chrome this module *does* draw are the ones that are not
//! shell output and could never be mistaken for it: the badge shown while the
//! view is scrolled back rather than following output, the notice over a shell
//! that has died, and the completion indicator in the key bar. Each
//! borrows an existing the design slot rather than inventing one.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;
use crate::config::{ColorDepth, Theme};
use crate::console::Console;
use crate::input::Focus;

use super::text::Glyphs;

/// Whether the shell's screen is what this frame is painted on.
///
/// True while `Ctrl+O` has the panels hidden, and **also** while a dialog is
/// open over that state. the design says the panels stay hidden while a command
/// holds the terminal, and a job finishing raises its summary with no keystroke
/// at all; without this, a background copy completing would
/// pull the panels back up behind the dialog and leave the user somewhere else
/// when it was dismissed. `App::push_dialog` records where focus came from,
/// which is exactly the question.
pub fn is_backdrop(app: &App) -> bool {
    if app.console_is_shown() {
        return true;
    }
    app.dialogs()
        .first()
        .is_some_and(|frame| frame.restore == Focus::Console)
}

/// The full-screen console.
///
/// `area` is the whole terminal, and every cell of it is written: the shell's
/// screen where the emulator has one, the terminal's own default colours where
/// it does not - which is only ever true for the single frame between a resize
/// and the PTY catching up with it.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The terminal's own default, not `panel.bg`: this screen is the shell's.
    f.render_widget(Block::new().style(Style::reset()), area);

    let Some(console) = app.console.shell.as_ref() else {
        draw_notice(
            f,
            app,
            area,
            &[
                "no shell is running".to_string(),
                "Ctrl+O returns to the panels".to_string(),
            ],
        );
        return;
    };

    if !console.is_alive() {
        // a shell that has gone is reported and a new one is on
        // offer, rather than the application becoming unusable. `Ctrl+O` here
        // asks for it - see `input::console_toggle`.
        draw_notice(
            f,
            app,
            area,
            &[death_line(console), "Ctrl+O starts a new shell".to_string()],
        );
        return;
    }

    // `Screen::cell` is already relative to the scrollback offset, so this one
    // call draws the live screen and any page of history alike.
    paint_screen(
        f.buffer_mut(),
        console.screen(),
        area,
        &app.theme,
        app.color_depth,
    );
    draw_scrollback_badge(f, app, area, console);
}

/// Where the terminal's own cursor goes while the console has the screen.
///
///
/// **The shell's cursor is the terminal's cursor.** Three states have no cursor
/// to place, and in each of them `None` - which leaves the cursor hidden - is
/// the faithful answer rather than a block parked somewhere arbitrary:
///
/// * the shell asked for it to be hidden (`DECTCEM`), which a full-screen
///   program does constantly while it redraws. the design promises "a program
///   that wants a terminal gets a real one", and a real one honours this;
/// * the view is scrolled back, so the row the caret is on is not on the page.
///   `vt100` reports the cursor in live-screen coordinates and it does not move
///   when the view does;
/// * there is no live shell, so what is drawn is a notice, which has no caret.
pub fn cursor(app: &App, area: Rect) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let console = app.console.shell.as_ref()?;
    if !console.is_alive() || console.scroll_offset() != 0 {
        return None;
    }
    let screen = console.screen();
    if screen.hide_cursor() {
        return None;
    }
    let (row, col) = screen.cursor_position();
    let x = area
        .x
        .saturating_add(col)
        .min(area.right().saturating_sub(1));
    let y = area
        .y
        .saturating_add(row)
        .min(area.bottom().saturating_sub(1));
    Some((x, y))
}

// ----------------------------------------------------------------- chrome ----

/// the completion indicator, as one cell of the key bar.
///
/// > While in panel mode the PTY keeps running; output is buffered. A
/// > completion indicator in the key bar shows when a background command has
/// > produced output.
///
/// the design makes the key bar's slots **a fixed geometry** - every slot the
/// same width, the boundaries from the terminal width alone, "pressing or
/// releasing a modifier therefore changes the text *in place* and moves
/// nothing". An indicator that took a slot, borrowed space from a label or
/// shortened a field would break exactly the rule that layout exists to keep.
/// So it takes no space at all: it is painted over the bar's **last cell**,
/// after the bar has been drawn, and nothing else on the screen changes.
///
/// That cell is blank wherever the bar has its full geometry - past the last
/// slot on a wide terminal, or the last slot's own trailing pad, since every
/// label is at most seven cells in a nine-cell field. At the middle widths
/// where the fields are scaled down it is instead the last cell of `F10`'s
/// label: the ellipsis or the final letter of an abbreviation the width has
/// already cropped, which is the least informative cell in the bar. Losing it
/// costs less than moving anything would.
///
/// Drawn in the key bar's own two colours, inverted, so it reads as part of the
/// bar rather than as a stray coloured dot - and needs no theme slot of its
/// own, which the design does not offer.
pub fn draw_activity(f: &mut Frame, app: &App, keybar: Rect, active: bool) {
    if !active || keybar.width == 0 || keybar.height == 0 {
        return;
    }
    let x = keybar.right().saturating_sub(1);
    let y = keybar.y;
    let fg = app
        .theme
        .quantize(app.theme.keybar.label_bg, app.color_depth);
    let bg = app
        .theme
        .quantize(app.theme.keybar.number_fg, app.color_depth);
    let glyph = if app.config.ui.ascii_borders {
        "*"
    } else {
        "\u{25cf}"
    };
    if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
        cell.set_symbol(glyph);
        cell.set_style(Style::reset().fg(fg).bg(bg).add_modifier(Modifier::BOLD));
    }
}

/// How far back the view is, while it is not following the live screen
/// (the "the full scrollback").
///
/// Top right, out of the way of a prompt, and only while it has something to
/// say: scrolling back to the bottom resumes following output and the badge
/// goes with it. It names the key that resumes, because a view that has quietly
/// stopped following output is the one way this feature can look broken.
fn draw_scrollback_badge(f: &mut Frame, app: &App, area: Rect, console: &Console) {
    let offset = console.scroll_offset();
    if offset == 0 {
        return;
    }
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let text = format!(" {} {offset} rows  Shift+PgDn ", g.arrow_up());
    let width = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
    if width >= area.width {
        return;
    }
    let style = Style::reset()
        .fg(app.theme.quantize(app.theme.dialog.title, app.color_depth))
        .bg(app.theme.quantize(app.theme.dialog.bg, app.color_depth));
    let x = area.right().saturating_sub(width);
    let _ = f
        .buffer_mut()
        .set_stringn(x, area.y, text, usize::from(width), style);
}

/// One line naming how the shell ended.
///
/// [`Console::death_notice`] says the same thing for the status line and ends
/// with the offer; here the offer is the line below, so this is only the half
/// that names the shell and the reason.
fn death_line(console: &Console) -> String {
    let how = console.failure().map_or_else(
        || {
            console
                .exit()
                .map_or_else(|| "exited".to_string(), ToString::to_string)
        },
        ToString::to_string,
    );
    format!("{}: {how}", console.program())
}

/// A centred notice over an empty console - a shell that has died, or one that
/// never started (report it, and do not become unusable).
fn draw_notice(f: &mut Frame, app: &App, area: Rect, lines: &[String]) {
    let rows = u16::try_from(lines.len()).unwrap_or(1).min(area.height);
    if rows == 0 || area.width == 0 {
        return;
    }
    let top = area.y.saturating_add(area.height.saturating_sub(rows) / 2);
    let block = Rect::new(area.x, top, area.width, rows);
    let style = Style::reset()
        .fg(app.theme.quantize(app.theme.dialog.fg, app.color_depth))
        .bg(app.theme.quantize(app.theme.dialog.bg, app.color_depth));
    let body: Vec<Line<'static>> = lines
        .iter()
        .map(|l| Line::from(Span::raw(l.clone())))
        .collect();
    f.render_widget(Paragraph::new(body).centered().style(style), block);
}

// ------------------------------------------------------------- the cells ----

/// Copy the visible page of the parsed screen into `area`.
///
/// The cells are the shell's, with the shell's colours and attributes:
/// nothing here reinterprets them, and the only
/// conversion is [`super::cmdline::style_of`]'s.
fn paint_screen(
    buf: &mut Buffer,
    screen: &vt100::Screen,
    area: Rect,
    theme: &Theme,
    depth: ColorDepth,
) {
    let (rows, cols) = screen.size();
    let blank = Style::reset();
    for dy in 0..area.height {
        let y = area.y.saturating_add(dy);
        for dx in 0..area.width {
            let x = area.x.saturating_add(dx);
            // Off the emulator's grid: the terminal is momentarily larger than
            // the PTY, which lasts the one frame before the resize lands.
            let cell = if dy < rows && dx < cols {
                screen.cell(dy, dx)
            } else {
                None
            };
            let Some(src) = cell else {
                if let Some(out) = buf.cell_mut((x, y)) {
                    out.set_symbol(" ");
                    out.set_style(blank);
                }
                continue;
            };
            // The right-hand half of a wide character: written already, by the
            // cell that owns it, and `ratatui` requires it to stay blank.
            if src.is_wide_continuation() {
                continue;
            }
            let style = super::cmdline::style_of(src, theme, depth);
            // A wide character with one column left cannot be drawn without
            // spilling onto the next row. The space is what the shell would
            // have left there too, having wrapped.
            let fits = !src.is_wide() || dx.saturating_add(1) < area.width;
            let contents = src.contents();
            let symbol = if contents.is_empty() || !fits {
                " "
            } else {
                contents
            };
            if let Some(out) = buf.cell_mut((x, y)) {
                out.set_symbol(symbol);
                out.set_style(style);
            }
            if src.is_wide()
                && fits
                && let Some(out) = buf.cell_mut((x.saturating_add(1), y))
            {
                // Blank, wearing the wide cell's own colours, so a highlighted
                // glyph is highlighted across both of its halves.
                out.set_symbol(" ");
                out.set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap};
    use crate::console::ConsoleEvent;
    use crate::input::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::style::Color;

    fn app() -> App {
        let mut a = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        a.color_depth = ColorDepth::TrueColor;
        a
    }

    /// A parsed screen with `text` written into it, and no PTY in sight.
    fn screen(rows: u16, cols: u16, text: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 200);
        parser.process(text);
        parser
    }

    fn paint(parser: &vt100::Parser, area: Rect, fill: &'static str) -> Buffer {
        let mut buf = Buffer::filled(Rect::new(0, 0, 12, 6), Cell::new(fill));
        paint_screen(
            &mut buf,
            parser.screen(),
            area,
            &Theme::blue(),
            ColorDepth::TrueColor,
        );
        buf
    }

    fn dump(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    fn render<F: FnOnce(&mut Frame)>(w: u16, h: u16, f: F) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(f).expect("draw");
        terminal.backend().buffer().clone()
    }

    // ------------------------------------------------------------- cells ----

    #[test]
    fn the_shells_screen_is_copied_cell_for_cell() {
        let p = screen(3, 12, b"one\r\ntwo\r\nthree");
        let buf = paint(&p, Rect::new(0, 0, 12, 3), ".");
        let out = dump(&buf);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.first().copied(), Some("one         "));
        assert_eq!(rows.get(1).copied(), Some("two         "));
        assert_eq!(rows.get(2).copied(), Some("three       "));
    }

    #[test]
    fn the_shells_colours_and_attributes_survive_untouched() {
        // "starship, oh-my-posh, a hand-rolled PS1, colours, git
        // branch and dirty markers included". Nothing here re-themes them.
        let p = screen(
            1,
            12,
            b"\x1b[1;31mr\x1b[0m\x1b[38;2;10;20;30mx\x1b[0m\x1b[4mu\x1b[7mv",
        );
        let buf = paint(&p, Rect::new(0, 0, 12, 1), " ");

        let red = buf.cell((0, 0)).expect("a cell");
        assert_eq!(red.fg, Color::Indexed(1), "the shell's red, not a theme's");
        assert!(red.modifier.contains(Modifier::BOLD));
        let rgb = buf.cell((1, 0)).expect("a cell");
        assert_eq!(rgb.fg, Color::Rgb(10, 20, 30), "24-bit at TrueColor depth");
        assert!(
            buf.cell((2, 0))
                .is_some_and(|c| c.modifier.contains(Modifier::UNDERLINED))
        );
        assert!(
            buf.cell((3, 0))
                .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
        );
        // A cell the shell never touched keeps the terminal's own default,
        // which is what makes this look like the shell's own screen and not
        // like a panel with text on it.
        let blank = buf.cell((8, 0)).expect("a cell");
        assert_eq!(blank.fg, Color::Reset);
        assert_eq!(blank.bg, Color::Reset);
    }

    #[test]
    fn a_wide_character_keeps_its_second_cell_blank() {
        // `ratatui`'s buffer diff assumes "no double-width cell is followed by
        // a non-blank cell"; `vt100` marks the second half as a continuation
        // with no contents at all, which would otherwise be written as an empty
        // symbol of width zero.
        let p = screen(1, 6, "\u{65e5}\u{672c}ab".as_bytes());
        let buf = paint(&p, Rect::new(0, 0, 6, 1), ".");
        assert_eq!(buf.cell((0, 0)).map(Cell::symbol), Some("\u{65e5}"));
        assert_eq!(buf.cell((1, 0)).map(Cell::symbol), Some(" "));
        assert_eq!(buf.cell((2, 0)).map(Cell::symbol), Some("\u{672c}"));
        assert_eq!(buf.cell((3, 0)).map(Cell::symbol), Some(" "));
        assert_eq!(buf.cell((4, 0)).map(Cell::symbol), Some("a"));
    }

    #[test]
    fn a_wide_character_that_does_not_fit_the_rectangle_is_not_half_drawn() {
        let p = screen(1, 4, "a\u{65e5}".as_bytes());
        // Two columns of the rectangle only: the wide glyph would spill.
        let buf = paint(&p, Rect::new(0, 0, 2, 1), ".");
        assert_eq!(buf.cell((0, 0)).map(Cell::symbol), Some("a"));
        assert_eq!(buf.cell((1, 0)).map(Cell::symbol), Some(" "));
    }

    #[test]
    fn a_screen_smaller_than_the_rectangle_is_blanked_rather_than_left_stale() {
        let p = screen(2, 4, b"ab");
        let buf = paint(&p, Rect::new(0, 0, 8, 4), "Z");
        let out = dump(&buf);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.first().copied(), Some("ab      ZZZZ"));
        assert_eq!(rows.get(1).copied(), Some("        ZZZZ"));
        assert_eq!(rows.get(3).copied(), Some("        ZZZZ"));
    }

    #[test]
    fn nothing_is_written_outside_the_rectangle() {
        let p = screen(4, 8, b"aaaaaaaa\r\nbbbbbbbb\r\ncccccccc");
        let buf = paint(&p, Rect::new(2, 1, 4, 2), ".");
        let out = dump(&buf);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(
            rows.first().copied(),
            Some("............"),
            "row 0 untouched"
        );
        assert_eq!(rows.get(1).copied(), Some("..aaaa......"));
        assert_eq!(rows.get(2).copied(), Some("..bbbb......"));
        assert_eq!(
            rows.get(3).copied(),
            Some("............"),
            "row 3 untouched"
        );
    }

    // ------------------------------------------------------------ chrome ----

    #[test]
    fn the_completion_indicator_does_not_move_a_single_key_bar_slot() {
        // the slots are a fixed geometry, and "pressing or
        // releasing a modifier therefore changes the text in place and moves
        // nothing". The indicator is one cell at the end of the bar, so the
        // whole screen is identical with it and without it apart from that one.
        let a = app();
        for (w, h) in [(60_u16, 15_u16), (80, 24), (130, 24), (200, 50)] {
            let area = Rect::new(0, 0, w, h);
            let bar = super::super::layout(&a, area).keybar;
            assert_eq!(bar.height, 1, "the key bar is drawn at {w}x{h}");

            let quiet = render(w, h, |f| super::super::draw(f, &a));
            let busy = render(w, h, |f| {
                super::super::draw(f, &a);
                draw_activity(f, &a, bar, true);
            });

            let marked = (bar.right().saturating_sub(1), bar.y);
            for y in 0..h {
                for x in 0..w {
                    if (x, y) == marked {
                        continue;
                    }
                    assert_eq!(
                        quiet.cell((x, y)).map(Cell::symbol),
                        busy.cell((x, y)).map(Cell::symbol),
                        "cell ({x},{y}) moved at {w}x{h}"
                    );
                }
            }
            if w >= 130 {
                // Wide enough for the slots' full geometry, where the cell the
                // mark uses is a pad cell and nothing at all is lost.
                assert_eq!(
                    quiet.cell(marked).map(Cell::symbol),
                    Some(" "),
                    "the cell the mark uses is a pad cell at {w}x{h}"
                );
            }
            assert_eq!(
                busy.cell(marked).map(Cell::symbol),
                Some("\u{25cf}"),
                "and the mark is on it at {w}x{h}"
            );
        }
    }

    #[test]
    fn no_indicator_is_drawn_when_there_is_nothing_to_report() {
        let a = app();
        let bar = Rect::new(0, 0, 80, 1);
        let quiet = render(80, 1, |f| draw_activity(f, &a, bar, false));
        assert!(!dump(&quiet).contains('\u{25cf}'));
    }

    #[test]
    fn the_indicator_is_ascii_where_the_rest_of_the_interface_is() {
        // under `ui.ascii_borders` nothing non-ASCII is drawn.
        let mut a = app();
        a.config.ui.ascii_borders = true;
        let bar = Rect::new(0, 0, 80, 1);
        let out = dump(&render(80, 1, |f| draw_activity(f, &a, bar, true)));
        assert!(out.is_ascii(), "{out}");
        assert!(out.contains('*'), "{out}");
    }

    #[test]
    fn a_console_with_no_shell_says_so_rather_than_showing_an_empty_screen() {
        let a = app();
        let out = dump(&render(60, 15, |f| draw(f, &a, Rect::new(0, 0, 60, 15))));
        assert!(out.contains("no shell is running"), "{out}");
        assert!(out.contains("Ctrl+O"), "it says the way out:\n{out}");
    }

    #[test]
    fn the_console_view_survives_every_size() {
        // "re-layout, never crash on a 1x1 terminal".
        let a = app();
        for (w, h) in [
            (200_u16, 50_u16),
            (80, 24),
            (60, 15),
            (20, 5),
            (2, 2),
            (1, 1),
        ] {
            let buf = render(w, h, |f| draw(f, &a, Rect::new(0, 0, w, h)));
            assert_eq!(buf.area().width, w);
        }
        assert_eq!(cursor(&a, Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_dialog_over_the_console_keeps_the_console_as_the_backdrop() {
        // "the panels stay hidden while it holds the terminal". A
        // job finishing raises its summary with no keystroke at all.
        let mut a = app();
        a.set_focus(Focus::Console);
        assert!(is_backdrop(&a));
        a.push_dialog(Box::new(crate::dialog::MessageDialog::line("Copy", "done")));
        assert!(
            is_backdrop(&a),
            "the panels must not reappear behind the dialog"
        );
        let mut b = app();
        b.push_dialog(Box::new(crate::dialog::MessageDialog::line("Copy", "done")));
        assert!(!is_backdrop(&b), "and a dialog over the panels is not this");
    }

    // ------------------------------------------------------ a live shell ----
    //
    // A `Console` cannot be built without a PTY, so the assertions that need a
    // real one start `/bin/sh` - the shell `tests/console_input.rs` uses, for
    // the same reasons. **Nothing here waits on the shell**: the screen holds
    // exactly what the test fed the emulator, so there is no race and no
    // prompt to depend on.

    /// A headless app with a live shell, or `None` where there is no `/bin/sh`.
    fn app_with_shell() -> Option<(App, tokio::sync::mpsc::Receiver<ConsoleEvent>)> {
        if !std::path::Path::new("/bin/sh").exists() {
            return None;
        }
        let mut config = Config::default();
        config.console.shell = "/bin/sh".to_string();
        // No snippet: the screen must hold what this test wrote and nothing
        // else.
        config.console.inject_hooks = false;
        let mut a = App::headless(config, Keymap::builtin(), Theme::blue());
        a.color_depth = ColorDepth::TrueColor;

        let (tx, rx) = tokio::sync::mpsc::channel::<ConsoleEvent>(64);
        let console =
            Console::spawn(&a.config.console, std::path::Path::new("/"), (24, 80), tx).ok()?;
        a.set_console(Some(console));
        assert!(a.console_owns_cmdline(), "the shell is running");
        Some((a, rx))
    }

    fn feed(app: &mut App, bytes: &[u8]) {
        app.apply_console_event(ConsoleEvent::Output(bytes.to_vec()));
    }

    /// How far back the console's view is.
    fn offset(app: &App) -> usize {
        app.console.shell.as_ref().map_or(0, Console::scroll_offset)
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        crate::input::dispatch(app, crate::input::KeyEvent::new(code, mods))
            .expect("dispatch never fails on a synthetic key");
    }

    #[test]
    fn the_console_shows_the_shells_screen_and_nothing_of_the_panels() {
        // "the screen is the shell and nothing else … not a split,
        // not a pane, and not a shrunken terminal".
        let Some((mut a, _rx)) = app_with_shell() else {
            return;
        };
        a.set_focus(Focus::Console);
        feed(
            &mut a,
            b"$ cargo build\r\n   Compiling holoscommander\r\n$ ",
        );

        let out = dump(&render(80, 24, |f| draw(f, &a, Rect::new(0, 0, 80, 24))));
        assert!(out.contains("cargo build"), "{out}");
        assert!(out.contains("Compiling holoscommander"), "{out}");
        assert!(
            !out.contains("F3 View"),
            "no key bar over the shell:\n{out}"
        );
        assert!(!out.contains('\u{2502}'), "no panel borders:\n{out}");
    }

    #[test]
    fn the_shells_cursor_is_the_terminals_cursor() {
        let Some((mut a, _rx)) = app_with_shell() else {
            return;
        };
        a.set_focus(Focus::Console);
        let area = Rect::new(0, 0, 80, 24);
        feed(&mut a, b"$ ls -la");
        assert_eq!(cursor(&a, area), Some((8, 0)), "on the row being typed on");

        feed(&mut a, b"\r\n$ ");
        assert_eq!(cursor(&a, area), Some((2, 1)));

        // A full-screen program that hides the cursor gets to hide it: the design
        // promises a real terminal, and a real one honours DECTCEM.
        feed(&mut a, b"\x1b[?25l");
        assert_eq!(cursor(&a, area), None);
        feed(&mut a, b"\x1b[?25h");
        assert_eq!(cursor(&a, area), Some((2, 1)));
    }

    #[test]
    fn scrolling_back_stops_following_and_scrolling_to_the_bottom_resumes() {
        // the "the full scrollback", through the real key path.
        let Some((mut a, _rx)) = app_with_shell() else {
            return;
        };
        a.set_focus(Focus::Console);
        for i in 0..60 {
            feed(&mut a, format!("line {i}\r\n").as_bytes());
        }
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(offset(&a), 0, "the live screen, to start with");
        let live = dump(&render(80, 24, |f| draw(f, &a, area)));
        assert!(live.contains("line 59"), "the newest output:\n{live}");

        press(&mut a, KeyCode::PageUp, KeyModifiers::SHIFT);
        assert!(offset(&a) > 0, "Shift+PgUp walked back");
        let back = dump(&render(80, 24, |f| draw(f, &a, area)));
        assert!(
            back.contains("line 20") && !back.contains("line 59"),
            "older output is on screen and the newest is not:\n{back}"
        );
        assert!(
            back.contains("rows") && back.contains("Shift+PgDn"),
            "the view says it is not following output, and how to resume:\n{back}"
        );
        assert_eq!(
            cursor(&a, area),
            None,
            "the shell's caret is not on this page"
        );

        press(&mut a, KeyCode::PageDown, KeyModifiers::SHIFT);
        assert_eq!(offset(&a), 0, "and at the bottom it follows output again");
        assert!(cursor(&a, area).is_some());
        let out = dump(&render(80, 24, |f| draw(f, &a, area)));
        assert!(
            !out.contains("Shift+PgDn"),
            "the badge goes with it:\n{out}"
        );
    }

    #[test]
    fn typing_snaps_the_view_back_to_the_bottom() {
        // What every terminal does, and what the "a program that wants
        // a terminal gets a real one" needs: the keystroke has to land where
        // the user can see the answer to it.
        let Some((mut a, _rx)) = app_with_shell() else {
            return;
        };
        a.set_focus(Focus::Console);
        for i in 0..60 {
            feed(&mut a, format!("line {i}\r\n").as_bytes());
        }
        press(&mut a, KeyCode::PageUp, KeyModifiers::SHIFT);
        assert!(offset(&a) > 0);

        press(&mut a, KeyCode::Char('x'), KeyModifiers::NONE);
        // `dispatch` only queues; the event loop writes, and writing is what
        // returns the view to the live screen (`Console::write`).
        let (bytes, _) = a.take_pending_shell();
        assert_eq!(
            bytes, b"x",
            "the key reached the shell, not this application"
        );
        if let Some(console) = a.console.shell.as_mut() {
            console.write(&bytes, crate::console::Origin::Typed);
        }
        assert_eq!(offset(&a), 0, "the view snapped back to the bottom");
    }

    #[test]
    fn a_shell_that_has_died_is_reported_and_a_new_one_is_offered() {
        // the application is still a file manager, and the way to
        // a new shell is on the screen rather than in the documentation.
        let Some((mut a, _rx)) = app_with_shell() else {
            return;
        };
        a.set_focus(Focus::Console);
        feed(&mut a, b"$ exit\r\n");
        if let Some(console) = a.console.shell.as_mut() {
            console.closed(None);
        }
        assert!(!a.console_owns_cmdline());

        let area = Rect::new(0, 0, 80, 24);
        let out = dump(&render(80, 24, |f| draw(f, &a, area)));
        assert!(out.contains("/bin/sh"), "it names the shell:\n{out}");
        assert!(out.contains("Ctrl+O"), "and offers a new one:\n{out}");
        assert_eq!(cursor(&a, area), None, "a notice has no caret");

        // And the offer is real: `Ctrl+O` on this screen asks for a new shell
        // rather than dropping back to the panels for a second press.
        press(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(a.console.restart_requested);
        assert_eq!(a.focus, Focus::Console, "and stays to watch it come up");
    }
}
