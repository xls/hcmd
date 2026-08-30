//! The command line at the foot of the panel view.
//!
//! > The command line at the foot of the panel view is **the shell's own
//! > current input line**, rendered from the PTY: the last row of the console,
//! > in place. Whatever the user's shell draws as a prompt is what appears
//! > there - starship, oh-my-posh, a hand-rolled `PS1`, colours, git branch and
//! > dirty markers included.
//!
//! So this module composes no prompt. With a live shell it paints cells copied
//! out of [`vt100::Screen`] with the attributes the shell drew them with, and
//! the only conversion it performs is [`crate::console::CellColor`] → a
//! `ratatui` colour, quantized for the session's depth. Nothing
//! reinterprets a colour, which is the whole reason starship's prompt survives
//! the trip.
//!
//! # Two modes, one rectangle
//!
//! * **A live shell owns it** ([`crate::app::App::console_owns_cmdline`]) - the
//!   rows are [`crate::console::Console::input_block`]'s, the caret is the
//!   shell's, and [`crate::input::CommandLine`] is not read at all.
//! * **Otherwise** - a headless `App`, `console.enabled = false`, a shell that
//!   would not start or has died - the v0.1 command line of the design is
//!   drawn, unchanged. That is not a placeholder; it is the fallback, and it is
//!   normative.
//!
//! # How many rows
//!
//! the design asks for "as many rows as it needs", so a two-line starship
//! prompt is not truncated into nonsense - and gives no upper bound, so
//! [`rows`] imposes one: `console.cmdline_rows` (default 2), and never so many
//! that the panels lose their the design minimum. **The panels shrink by
//! exactly what the prompt takes.** A block taller than the room available
//! keeps its *bottom* rows: losing the top of a prompt is survivable, losing
//! the row being typed on is not.
//!
//! # The caret
//!
//! [`caret`] is where the "both cursors are always visible" is
//! answered for this region: the hardware cursor when the command line has
//! focus, and a painted `cmdline.caret_unfocused` cell when it does not. With a
//! shell it is the shell's own cursor, read off the parsed screen; without one
//! it is [`crate::input::CommandLine`]'s remembered caret.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::config::{ColorDepth, Rgb, Theme};
use crate::console::{CellColor, InputBlock, StableBlock};
use crate::input::Focus;

use super::text::{self, Glyphs};

/// The narrowest editable region the **fallback** command line keeps for
/// itself, whatever the prompt would like.
///
/// the design makes the command line "always present", the design declares
/// 60x15 usable, and the caret to stay visible and meaningful. A deep working
/// directory can be wider than the whole terminal - 60 columns and a
/// 67-character path is not exotic - and a prompt allowed to take all of it
/// leaves zero columns for the text, so what was typed is simply not on screen.
/// The prompt is the part that yields: it is cropped from the left, keeping the
/// deepest directory, which is the informative end of a path.
///
/// None of this applies to a shell's prompt, which is the shell's own problem:
/// it wrapped it onto the screen itself and we copy the result.
pub const MIN_CMDLINE_INPUT: usize = 12;

/// The rows the layout must leave for the command line.
///
/// `area` is the **whole terminal**, because the answer depends on how much
/// room the panels can spare: the design gives the body a floor of three rows
/// and this may not eat into it, however tall a prompt somebody has configured.
///
/// One row without a shell, which is every v0.1 and v0.2 layout unchanged.
pub fn rows(app: &App, area: Rect) -> u16 {
    let wanted = wanted_rows(app);
    if wanted <= 1 {
        return 1;
    }
    // the rows that are not the body, plus the body's own floor.
    let menubar = u16::from(app.config.ui.show_menubar);
    let keybar = u16::from(app.config.ui.show_keybar);
    let overhead = menubar.saturating_add(keybar).saturating_add(BODY_FLOOR);
    let room = area.height.saturating_sub(overhead).max(1);
    wanted.min(room)
}

/// The `Constraint::Min` the body carries in [`super::layout`]. The
/// command line may not push the panels below it.
const BODY_FLOOR: u16 = 3;

/// How many rows the shell's prompt block would like, before the panels get a
/// say.
fn wanted_rows(app: &App) -> u16 {
    if !app.console_owns_cmdline() || program_owns_the_screen(app) {
        return 1;
    }
    let max = app.config.console.cmdline_rows.max(1);
    // The **stable** block, the same one the painter uses.
    //
    // `input_block` is the live measurement and it moves: the panel→shell `cd`
    // puts the echoed command on one row and the new prompt on the next, so a
    // height taken from it grows for a frame and the panels are briefly
    // shorter. Measuring the layout from one block while painting from another
    // is how that survived the paint-side fix - the region was pinned and the
    // space reserved for it was not.
    app.console
        .shell
        .as_ref()
        .map_or(1, |console| console.stable_block(max).block.rows())
        .clamp(1, max)
}

/// Draw the command line.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let painted = if program_owns_the_screen(app) {
        draw_program_notice(f, app, area);
        None
    } else if app.console_owns_cmdline() {
        draw_shell(f, app, area)
    } else {
        draw_fallback(f, app, area);
        Some(geometry(app, area).0).map(|x| (x, area.y))
    };

    // unfocused, the caret is a solid block cell painted into
    // the buffer - never hidden, never dimmed to invisibility. Focused, the
    // hardware cursor is there instead and painting one too would show two.
    if app.focus != Focus::CommandLine
        && let Some((x, y)) = painted
    {
        paint_caret(f.buffer_mut(), x, y, app, g);
    }
}

/// Where the terminal's real cursor goes when this region has it.
///
///
/// With a live shell this is the shell's own cursor, mapped out of the parsed
/// screen and into `area`; without one it is the remembered caret of
/// `None` only when there is no rectangle to put it in.
pub fn caret(app: &App, area: Rect) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if program_owns_the_screen(app) {
        // There is no input line to put a caret on. The region is a notice, so
        // the cursor sits at its end rather than on top of a letter of it.
        let x = area
            .x
            .saturating_add(u16::try_from(text::width(notice(app))).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        return Some((x, area.bottom().saturating_sub(1)));
    }
    if app.console_owns_cmdline() {
        let console = app.console.shell.as_ref()?;
        // The stable block again: the caret has to land in the geometry that
        // was reserved and painted, not in a freshly measured one that may
        // differ by a row.
        let block = console
            .stable_block(app.config.console.cmdline_rows.max(1))
            .block;
        return Some(block_caret(area, block));
    }
    Some((geometry(app, area).0, area.y))
}

// ----------------------------------------- a program owns the screen -------

/// What the command line says while a full-screen program is running in the
/// console, and the same sentence for a terminal that cannot draw an em dash
/// (the `ui.ascii_borders`). Both are the same number of columns wide.
const PROGRAM_NOTICE: &str = "a program is using the console - Ctrl+O";
const PROGRAM_NOTICE_ASCII: &str = "a program is using the console - Ctrl+O";

/// The notice, in the glyphs this session can draw.
const fn notice(app: &App) -> &'static str {
    if app.config.ui.ascii_borders {
        PROGRAM_NOTICE_ASCII
    } else {
        PROGRAM_NOTICE
    }
}

/// Whether a program has taken the console's alternate screen.
///
///
/// `vim`, `less`, `fzf`: anything that asks for `ESC[?1049h`. While one is,
/// **there is no shell input line**, and the row `InputBlock` names is a row of
/// that program's own screen - `vt100` routes `cell`, `cursor_position` and
/// `row_wrapped` to the alternate grid, and `OSC 133 ; C` cleared the marks
/// when the program started, so the block falls back to the cursor's row. Copied
/// into this region it reads as a prompt: a fragment of the file being edited,
/// or `-- INSERT --`, presented as the shell's input line, with the caret drawn
/// on it and every key typed there going to the program.
///
/// the region promises to be "the shell's own current input line".
/// When there is not one, saying so is the only true thing to draw.
fn program_owns_the_screen(app: &App) -> bool {
    app.console_owns_cmdline()
        && app
            .console
            .shell
            .as_ref()
            .is_some_and(crate::console::Console::alternate_screen)
}

/// One row saying where the shell's input line has gone.
fn draw_program_notice(f: &mut Frame, app: &App, area: Rect) {
    let row = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
    blank(f.buffer_mut(), area);
    let line = Line::from(Span::styled(
        text::take_front(notice(app), usize::from(area.width)),
        Style::new().fg(color(app, app.theme.cmdline.prompt_fg)),
    ));
    f.render_widget(Paragraph::new(line), row);
}

// --------------------------------------------------------- the shell's -----

/// Paint the shell's prompt block. Returns where its caret landed.
fn draw_shell(f: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    let console = app.console.shell.as_ref()?;
    // The *stable* block, never momentarily blank: a shell redraws its prompt
    // as carriage-return, erase-line, then text, and a frame landing between
    // the erase and the text would paint the command line black - a blink on
    // every prompt redraw. See `Console::stable_input_block`.
    let stable = console.stable_block(app.config.console.cmdline_rows.max(1));
    Some(paint_block(
        f.buffer_mut(),
        area,
        &stable,
        &app.theme,
        app.color_depth,
    ))
}

/// Where in `area` the shell's caret sits.
///
/// The block is drawn against the **bottom** of `area`, and the cursor is on
/// its last row ("the last row is the shell's
/// cursor row, always"), so the caret is on the last row drawn.
fn block_caret(area: Rect, block: InputBlock) -> (u16, u16) {
    let shown = block.rows().min(area.height).max(1);
    let top_row = block.last_row.saturating_sub(shown.saturating_sub(1));
    let dy = block
        .cursor_row
        .saturating_sub(top_row)
        .min(shown.saturating_sub(1));
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(shown))
        .saturating_add(dy)
        .min(area.bottom().saturating_sub(1));
    let x = area
        .x
        .saturating_add(block.cursor_col)
        .min(area.right().saturating_sub(1));
    (x, y)
}

/// Copy the prompt block out of the parsed screen and into the frame, cell for
/// cell and attribute for attribute.
///
/// `scroll` is [`crate::console::Console::scroll_offset`]. It matters because
/// `vt100`'s [`vt100::Screen::cell`] is *visible*-row addressed while
/// [`vt100::Screen::cursor_position`] - and therefore [`InputBlock`] - is
/// addressed in the live grid: scrolled back by `n`, live row `r` has moved
/// down to visible row `r + n`, and past the bottom it is not on screen at all.
/// Rows that have scrolled off are drawn blank rather than drawn from whatever
/// row happens to be there now, which would be somebody else's output wearing
/// the prompt's position. Any keystroke returns the console to the live screen
/// (`Console::write`), so the state is brief and self-healing.
///
/// Returns where the caret goes.
fn paint_block(
    buf: &mut Buffer,
    area: Rect,
    stable: &StableBlock,
    theme: &Theme,
    depth: ColorDepth,
) -> (u16, u16) {
    let block = stable.block;
    // The shell's screen is not the panel's: the design wants "the same screen
    // the shell would have had on its own", so the region starts from the
    // terminal's own defaults rather than from `panel.bg`.
    blank(buf, area);

    let screen_cols = stable
        .rows
        .first()
        .map_or(0, |row| u16::try_from(row.len()).unwrap_or(u16::MAX));
    let shown = block.rows().min(area.height).max(1);
    let first_y = area.y.saturating_add(area.height.saturating_sub(shown));
    let width = area.width.min(screen_cols);

    for i in 0..shown {
        let Some(y) = first_y.checked_add(i).filter(|y| *y < area.bottom()) else {
            break;
        };
        // `stable.rows` is the block itself, top row first, so the index is
        // relative to the block rather than to the screen. Scrolling back moves
        // the console view, not this docked region.
        let Some(cells) = stable
            .rows
            .len()
            .checked_sub(usize::from(shown))
            .and_then(|skip| stable.rows.get(skip.saturating_add(usize::from(i))))
        else {
            continue;
        };
        paint_row(buf, area, y, width, cells, theme, depth);
    }

    block_caret(area, block)
}

/// One row of the parsed screen into one row of the frame.
fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    width: u16,
    cells: &[Option<vt100::Cell>],
    theme: &Theme,
    depth: ColorDepth,
) {
    for col in 0..width {
        let Some(cell) = cells.get(usize::from(col)).and_then(Option::as_ref) else {
            continue;
        };
        // The second half of a wide character was written by the half before
        // it; `ratatui` requires the follower to stay blank.
        if cell.is_wide_continuation() {
            continue;
        }
        let x = area.x.saturating_add(col);
        let style = style_of(cell, theme, depth);
        if let Some(target) = buf.cell_mut((x, y)) {
            if cell.has_contents() {
                target.set_symbol(cell.contents());
            }
            target.set_style(style);
        }
        if cell.is_wide()
            && let Some(next) = x.checked_add(1).filter(|x| *x < area.right())
            && let Some(target) = buf.cell_mut((next, y))
        {
            // Blank, but wearing the same colours, so a wide glyph does not
            // leave a hole in the prompt's background.
            target.reset();
            target.set_style(style);
        }
    }
}

/// A [`vt100::Cell`]'s attributes as a `ratatui` style.
///
/// `inverse` becomes `REVERSED` rather than being resolved into swapped colours
/// here: the terminal we are drawing to does the swap, and doing it twice on a
/// cell that is already `Color::Reset` would have to invent the default
/// foreground and background this process does not know.
pub fn style_of(cell: &vt100::Cell, theme: &Theme, depth: ColorDepth) -> Style {
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier.insert(Modifier::BOLD);
    }
    if cell.dim() {
        modifier.insert(Modifier::DIM);
    }
    if cell.italic() {
        modifier.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        modifier.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        modifier.insert(Modifier::REVERSED);
    }
    Style::new()
        .fg(color_of(cell.fgcolor().into(), theme, depth))
        .bg(color_of(cell.bgcolor().into(), theme, depth))
        .add_modifier(modifier)
}

/// The one conversion from a terminal colour to a drawn one.
///
///
/// [`CellColor::Default`] is `Color::Reset` - the terminal's *own* default, and
/// deliberately not a theme slot, because the design asks for "the same screen
/// the shell would have had on its own". A direct colour is quantized for the
/// session's depth exactly as a theme colour is, so a
/// truecolor prompt still says something on a 16-colour terminal.
pub fn color_of(color: CellColor, theme: &Theme, depth: ColorDepth) -> Color {
    match color {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(i) => Color::Indexed(i),
        CellColor::Rgb(r, g, b) => theme.quantize(Rgb::new(r, g, b), depth),
    }
}

/// Reset a rectangle to the terminal's own defaults.
fn blank(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

// ------------------------------------------------- the v0.1 fallback -------

/// The prompt: the **active** panel's directory.
///
/// Only ever drawn when no shell owns the command line. With one, the prompt is
/// the shell's and this application composes none.
pub fn prompt(app: &App) -> String {
    format!("{}> ", app.active_panel().active_tab().path)
}

/// Where the fallback caret sits on screen, and where its visible text starts.
///
/// Returned as `(caret_x, prompt_width, scroll_columns)`. Split out so the
/// painted caret and the hardware cursor can never disagree.
pub fn geometry(app: &App, area: Rect) -> (u16, usize, usize) {
    let width = usize::from(area.width);
    // The input's floor comes off the top, so `avail` is only ever zero on a
    // command line that has no room for anything at all.
    let cap = width.saturating_sub(MIN_CMDLINE_INPUT.min(width));
    let prompt_w = text::width(&prompt(app)).min(cap);
    let avail = width.saturating_sub(prompt_w);
    let caret_col = app.cmdline.display_width_to_caret();
    // Keep the caret on screen, leaving its own cell free.
    let scroll = caret_col.saturating_sub(avail.saturating_sub(1));
    let x_off = prompt_w.saturating_add(caret_col.saturating_sub(scroll));
    let x = area
        .x
        .saturating_add(u16::try_from(x_off).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(1));
    (x, prompt_w, scroll)
}

/// the own command line, for every state that has no shell.
///
fn draw_fallback(f: &mut Frame, app: &App, area: Rect) {
    let g = Glyphs::new(app.config.ui.ascii_borders);
    let (_caret_x, prompt_w, scroll) = geometry(app, area);
    let full = prompt(app);
    // Cropped from the *left*: `…ui/widgets/panel> ` says where you are;
    // `/home/thorin/Devel` does not.
    let shown = if text::width(&full) > prompt_w {
        let marker = text::take_front(g.ellipsis(), prompt_w);
        let rest = text::take_back(&full, prompt_w.saturating_sub(text::width(&marker)));
        format!("{marker}{rest}")
    } else {
        full
    };
    let avail = usize::from(area.width).saturating_sub(prompt_w);
    let body = text::slice_columns(app.cmdline.text(), scroll, avail);

    let line = Line::from(vec![
        Span::styled(
            shown,
            Style::new().fg(color(app, app.theme.cmdline.prompt_fg)),
        ),
        Span::styled(body, Style::new().fg(color(app, app.theme.cmdline.fg))),
    ]);
    // One row: the several rows are the shell's prompt, and this is
    // the state with no shell.
    let row = Rect::new(area.x, area.y, area.width, 1);
    f.render_widget(
        Paragraph::new(line).style(Style::new().bg(color(app, app.theme.cmdline.bg))),
        row,
    );
}

/// the painted caret, for the region that does not hold the
/// hardware cursor.
///
/// It goes *over* whatever is underneath, prompt cell or shell output alike: a
/// cell that already has a glyph on it is inverted so the glyph stays readable,
/// and an empty one gets a solid block. Either way the caret is where
/// `Ctrl+Enter` is about to insert, which is the whole point of drawing it.
fn paint_caret(buf: &mut Buffer, x: u16, y: u16, app: &App, g: Glyphs) {
    let caret = color(app, app.theme.cmdline.caret_unfocused);
    let Some(cell) = buf.cell_mut((x, y)) else {
        return;
    };
    if cell.symbol().trim().is_empty() {
        let bg = cell.bg;
        cell.set_symbol(g.caret_block());
        cell.set_style(Style::new().fg(caret).bg(bg));
    } else {
        let under = cell.bg;
        cell.set_style(Style::new().fg(under).bg(caret));
    }
}

/// Quantize a theme colour for this session.
fn color(app: &App, rgb: Rgb) -> Color {
    app.theme.quantize(rgb, app.color_depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap};
    use crate::console::InputBlock;
    use crate::vfs::VfsPath;

    fn app() -> App {
        let mut a = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        a.color_depth = ColorDepth::TrueColor;
        a.active_panel_mut().active_tab_mut().path = VfsPath::local("/home/thorin");
        a
    }

    /// A parsed screen, with no PTY anywhere near it.
    fn screen_of(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 100);
        parser.process(bytes);
        parser
    }

    fn block(first: u16, last: u16, col: u16) -> InputBlock {
        InputBlock {
            first_row: first,
            last_row: last,
            cursor_row: last,
            cursor_col: col,
            input_start: None,
        }
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// A [`StableBlock`] over a parsed screen, for the render tests.
    ///
    /// Goes through the same `block_rows` the real path does, so the scroll
    /// handling under test is the shipped one and not a second copy.
    fn stable(parser: &vt100::Parser, blk: InputBlock) -> StableBlock {
        stable_scrolled(parser, blk, 0)
    }

    fn stable_scrolled(parser: &vt100::Parser, blk: InputBlock, scroll: usize) -> StableBlock {
        StableBlock {
            block: blk,
            rows: crate::console::block_rows(parser.screen(), blk, scroll),
        }
    }

    /// A headless app with a live `/bin/sh`, or `None` where there is none.
    fn app_with_shell() -> Option<App> {
        if !std::path::Path::new("/bin/sh").exists() {
            return None;
        }
        let mut config = Config::default();
        config.console.shell = "/bin/sh".to_string();
        config.console.inject_hooks = false;
        let mut a = App::headless(config, Keymap::builtin(), Theme::blue());
        a.color_depth = ColorDepth::TrueColor;
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::console::ConsoleEvent>(64);
        let console = crate::console::Console::spawn(
            &a.config.console,
            std::path::Path::new("/"),
            (24, 80),
            tx,
        )
        .ok()?;
        // The receiver is leaked deliberately: dropping it would close the
        // channel and the reader thread would stop, which is not what any of
        // these tests are about.
        std::mem::forget(rx);
        a.set_console(Some(console));
        Some(a)
    }

    #[test]
    fn a_program_on_the_alternate_screen_is_not_drawn_as_a_prompt() {
        // the command line is "the shell's own current input
        // line". While `vim` or `fzf` owns the console's alternate screen there
        // is no input line at all - `OSC 133 ; C` cleared the marks and `vt100`
        // routes every read to the alternate grid - so the block falls back to
        // the cursor's row and copies a row of somebody's file into the
        // command line, caret and all, with every key typed there going to that
        // program.
        let Some(mut a) = app_with_shell() else {
            return;
        };
        a.apply_console_event(crate::console::ConsoleEvent::Output(
            b"\x1b]133;C\x1b\\\x1b[?1049h\x1b[Hthe quick brown fox -- INSERT --".to_vec(),
        ));

        let area = Rect::new(0, 0, 40, 1);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1))
            .expect("test backend");
        terminal
            .draw(|f| draw(f, &a, area))
            .expect("the command line renders");
        let drawn = row_text(terminal.backend().buffer(), 0);
        assert!(
            !drawn.contains("quick brown fox") && !drawn.contains("INSERT"),
            "no row of the program's screen is presented as a prompt: {drawn:?}"
        );
        assert!(
            drawn.contains("Ctrl+O"),
            "it says where the shell's input line has gone: {drawn:?}"
        );
        assert_eq!(rows(&a, Rect::new(0, 0, 40, 24)), 1, "one row, not a block");
    }

    #[test]
    fn the_prompt_is_the_shells_own_cells_in_the_shells_own_colours() {
        // "whatever the user's shell draws as a prompt is what
        // appears there", colours included. Nothing here reinterprets them.
        let parser = screen_of(4, 20, b"\x1b[38;2;255;0;0mred\x1b[0m$ ls -la");
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 0, 10)),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(row_text(&buf, 0), "red$ ls -la");
        let r = buf.cell((0, 0)).map(|c| c.fg);
        assert_eq!(r, Some(Color::Rgb(255, 0, 0)), "the shell's own red");
        let plain = buf.cell((4, 0)).map(|c| c.fg);
        assert_eq!(plain, Some(Color::Reset), "default stays the terminal's");
    }

    #[test]
    fn a_direct_colour_is_quantized_for_the_session_like_any_other() {
        // a truecolor prompt still says something on a
        // 16-colour terminal.
        let theme = Theme::blue();
        let indexed = color_of(CellColor::Rgb(255, 0, 0), &theme, ColorDepth::Indexed256);
        assert!(matches!(indexed, Color::Indexed(_)));
        assert_eq!(
            color_of(CellColor::Indexed(9), &theme, ColorDepth::TrueColor),
            Color::Indexed(9),
            "an indexed colour is passed through, never re-derived"
        );
        assert_eq!(
            color_of(CellColor::Default, &theme, ColorDepth::TrueColor),
            Color::Reset,
            "the terminal's own default, not a theme slot"
        );
    }

    #[test]
    fn attributes_survive_the_copy() {
        let parser = screen_of(2, 12, b"\x1b[1;3;4;7mx\x1b[0m");
        let area = Rect::new(0, 0, 12, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 0, 1)),
            &theme,
            ColorDepth::TrueColor,
        );
        let m = buf.cell((0, 0)).map(|c| c.modifier).unwrap_or_default();
        assert!(m.contains(Modifier::BOLD));
        assert!(m.contains(Modifier::ITALIC));
        assert!(m.contains(Modifier::UNDERLINED));
        assert!(m.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_two_line_prompt_gets_two_rows_rather_than_nonsense() {
        // "a multi-line prompt is drawn as many rows as it needs,
        // so a two-line starship prompt is not truncated into nonsense".
        let parser = screen_of(4, 20, b"~/dev  master\r\n\xe2\x9d\xaf ls");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        let caret = paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 1, 5)),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(row_text(&buf, 0), "~/dev  master");
        assert_eq!(row_text(&buf, 1), "❯ ls");
        assert_eq!(caret, (5, 1), "the caret is on the row being typed on");
    }

    #[test]
    fn a_block_taller_than_the_room_keeps_its_bottom_rows() {
        // Losing the top of a prompt is survivable; losing the row being typed
        // on is not.
        let parser = screen_of(4, 20, b"first\r\nsecond\r\nthird$ ");
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        let caret = paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 2, 7)),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(row_text(&buf, 0), "third$");
        assert_eq!(caret, (7, 0));
    }

    #[test]
    fn the_block_sits_against_the_bottom_of_its_rectangle() {
        // The panels are above it; a prompt shorter than the reserved rows
        // leaves the gap where the panels were, not under the input line.
        let parser = screen_of(4, 20, b"$ ");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        let caret = paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 0, 2)),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(row_text(&buf, 0), "");
        assert_eq!(row_text(&buf, 1), "$");
        assert_eq!(caret, (2, 1));
    }

    #[test]
    fn a_wide_character_leaves_its_follower_blank() {
        // ratatui assumes no double-width cell is followed by a non-blank one;
        // vt100 records the continuation cell separately.
        let parser = screen_of(2, 12, "日本$ ".as_bytes());
        let area = Rect::new(0, 0, 12, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        paint_block(
            &mut buf,
            area,
            &stable(&parser, block(0, 0, 6)),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(buf.cell((0, 0)).map(|c| c.symbol()), Some("日"));
        assert_eq!(buf.cell((1, 0)).map(|c| c.symbol()), Some(" "));
        assert_eq!(buf.cell((2, 0)).map(|c| c.symbol()), Some("本"));
        assert_eq!(buf.cell((4, 0)).map(|c| c.symbol()), Some("$"));
    }

    #[test]
    fn a_scrolled_back_console_does_not_draw_somebody_elses_output() {
        // vt100 addresses cells by *visible* row and the cursor by live row.
        // Scrolled back, the live row has moved down; a renderer that ignored
        // the offset would paint whatever output is now standing in its place
        // and call it the prompt.
        let mut parser = vt100::Parser::new(2, 20, 100);
        parser.process(b"old output\r\nnewer\r\n$ ls");
        parser.screen_mut().set_scrollback(1);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let theme = Theme::blue();
        paint_block(
            &mut buf,
            area,
            &stable_scrolled(&parser, block(1, 1, 5), 1),
            &theme,
            ColorDepth::TrueColor,
        );
        assert_eq!(
            row_text(&buf, 0),
            "",
            "the input line has scrolled off; blank rather than wrong"
        );
    }

    #[test]
    fn without_a_shell_the_command_line_is_one_row_and_v0_1s() {
        // the fallback is not a placeholder. A
        // headless App has no PTY and every input-model test drives it.
        let mut a = app();
        a.cmdline.set_text("cp file.txt");
        a.cmdline.move_end();
        assert_eq!(rows(&a, Rect::new(0, 0, 80, 24)), 1);

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1))
            .expect("a test backend never fails to build");
        terminal
            .draw(|f| draw(f, &a, Rect::new(0, 0, 40, 1)))
            .expect("drawing into a test backend never fails");
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 0).starts_with("/home/thorin> cp file.txt"),
            "got {:?}",
            row_text(&buf, 0)
        );
    }

    #[test]
    fn the_unfocused_caret_is_painted_and_the_focused_one_is_not() {
        // the caret is rendered at all times, and only one
        // hardware cursor exists, so the unfocused region paints its own.
        let mut a = app();
        a.set_focus(Focus::Panel(a.active_side));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1))
            .expect("a test backend never fails to build");
        terminal
            .draw(|f| draw(f, &a, Rect::new(0, 0, 40, 1)))
            .expect("drawing into a test backend never fails");
        let (x, _, _) = geometry(&a, Rect::new(0, 0, 40, 1));
        let buf = terminal.backend().buffer().clone();
        let caret = a
            .theme
            .quantize(a.theme.cmdline.caret_unfocused, a.color_depth);
        let painted = buf.cell((x, 0)).map(|c| (c.symbol().to_string(), c.fg));
        assert_eq!(painted, Some(("\u{2588}".to_string(), caret)));

        a.set_focus(Focus::CommandLine);
        terminal
            .draw(|f| draw(f, &a, Rect::new(0, 0, 40, 1)))
            .expect("drawing into a test backend never fails");
        let buf = terminal.backend().buffer().clone();
        assert_ne!(
            buf.cell((x, 0)).map(|c| c.symbol()),
            Some("\u{2588}"),
            "the focused command line has the hardware cursor instead"
        );
        assert_eq!(caret_position(&a, 40), Some((x, 0)));
    }

    fn caret_position(a: &App, width: u16) -> Option<(u16, u16)> {
        caret(a, Rect::new(0, 0, width, 1))
    }

    #[test]
    fn the_command_line_never_takes_the_panels_last_rows() {
        // the design gives a prompt as many rows as it needs and no upper
        // bound; the design gives the body a floor. The floor wins.
        let mut a = app();
        a.config.console.cmdline_rows = 40;
        // No shell: one row whatever the setting says.
        assert_eq!(rows(&a, Rect::new(0, 0, 60, 15)), 1);
    }

    #[test]
    fn a_zero_sized_command_line_draws_nothing_and_places_no_caret() {
        // at 60x15 everything renders, and below it every rectangle
        // is checked rather than indexed into.
        let a = app();
        assert_eq!(caret(&a, Rect::new(0, 0, 0, 0)), None);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1))
            .expect("a test backend never fails to build");
        terminal
            .draw(|f| draw(f, &a, Rect::new(0, 0, 0, 0)))
            .expect("drawing into a test backend never fails");
    }
}
