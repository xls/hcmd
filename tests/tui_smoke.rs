//! End-to-end smoke tests: run the real `hcmd` binary inside a pseudo-terminal
//! and look at what it painted.
//!
//! the design's acceptance criteria are behavioural - "verify these by actually
//! running the binary, not by reading the code" - so the harness that makes that
//! checkable belongs in the repository from phase 1. `portable-pty` gives a pty
//! with a real size; `vt100` parses what came back into a screen we can read.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Keeps concurrently running tests off each other's config directory.
static NEXT: AtomicUsize = AtomicUsize::new(0);

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// How to run one `hcmd` under the pty.
struct Run<'a> {
    cols: u16,
    rows: u16,
    /// Keystrokes, written after the first frame has landed.
    input: &'a [u8],
    /// How long to keep reading before giving up on the child exiting.
    settle: Duration,
    /// Working directory. `None` uses the repository root.
    cwd: Option<PathBuf>,
    /// `$XDG_STATE_HOME`. `None` uses a throwaway directory, so a run neither
    /// reads nor keeps saved tab state.
    state_dir: Option<PathBuf>,
    /// Written to `$XDG_CONFIG_HOME/holoscommander/config.toml` before the
    /// child starts. `None` leaves the child on the compiled-in defaults.
    config: Option<String>,
}

impl<'a> Run<'a> {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            input: b"",
            settle: Duration::from_secs(3),
            cwd: None,
            state_dir: None,
            config: None,
        }
    }

    /// Run without a shell (the "until the PTY exists").
    ///
    /// The command line is then this application's own, with the text and the
    /// caret the design specify - which is what a test about *those*
    /// has to assert against. With a live shell the same row is whatever the
    /// user's `PS1` drew and the same keys are its line editor's.
    fn without_console(mut self) -> Self {
        self.config = Some("[console]\nenabled = false\n".to_string());
        self
    }

    fn input(mut self, input: &'a [u8]) -> Self {
        self.input = input;
        self
    }

    fn settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Share saved tab state between two runs.
    fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }
}

/// A directory tree the navigation tests browse, built fresh per test so the
/// listings are known exactly. Removed on drop.
/// Does the filesystem under `dir` tell two names apart by case alone?
///
/// macOS formats APFS case-insensitively by default, where `thorin` and
/// `Thorin` are one file: writing both leaves a single entry, and every count
/// below would be one out.
fn case_sensitive(dir: &Path) -> bool {
    let lower = dir.join(".case-probe");
    let upper = dir.join(".CASE-PROBE");
    if std::fs::write(&lower, b"a").is_err() {
        return false;
    }
    let distinct = std::fs::write(&upper, b"bb").is_ok()
        && std::fs::read(&lower).map(|v| v == b"a").unwrap_or(false);
    let _ = std::fs::remove_file(&lower);
    let _ = std::fs::remove_file(&upper);
    distinct
}

struct Fixture {
    root: PathBuf,
    /// Whether `thorin` and `Thorin` are two entries here.
    cased: bool,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-fix-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("subdir/deeper")).expect("fixture tree");
        let cased = case_sensitive(&root);
        for name in Self::names(cased) {
            std::fs::write(root.join(name), b"").expect("fixture file");
        }
        std::fs::write(root.join("subdir/inner.txt"), b"x").expect("fixture file");
        Self { root, cased }
    }

    /// The files it holds, which is one fewer where case folds.
    fn names(cased: bool) -> Vec<&'static str> {
        let mut names = vec!["thorin", "thunder", "alpha.rs", "zeta.txt"];
        if cased {
            names.insert(1, "Thorin");
        }
        names
    }

    /// How many files the listing has.
    fn file_count(&self) -> usize {
        Self::names(self.cased).len()
    }

    /// Every name on screen, `subdir` included.
    fn listed(&self) -> Vec<&'static str> {
        let mut all = vec!["subdir"];
        all.extend(Self::names(self.cased));
        all
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run `hcmd` in a pty of the given size, feed it `input`, and return the
/// rendered screen plus whether the child exited on its own.
fn run_in_pty(run: Run<'_>) -> (vt100::Parser, bool) {
    let Run {
        cols,
        rows,
        input,
        settle,
        cwd,
        state_dir,
        config,
    } = run;
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hcmd"));
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // A bare pty answers no capability query, so `auto` would spend its whole
    // timeout waiting and then decide "legacy" anyway. Say so up front, and the
    // tests exercise the legacy encodings deliberately rather than by accident.
    //
    cmd.env("HCMD_KEYBOARD_PROTOCOL", "legacy");
    // Keep the test off the developer's real configuration, and off its saved
    // tab state - a restored tab would change what the panel is showing.
    let dir = std::env::temp_dir().join(format!(
        "hcmd-pty-{}-{cols}x{rows}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&dir);
    if let Some(toml) = config {
        let _ = std::fs::create_dir_all(dir.join("holoscommander"));
        std::fs::write(dir.join("holoscommander/config.toml"), toml).expect("write config");
    }
    cmd.env("XDG_CONFIG_HOME", &dir);
    cmd.env("XDG_STATE_HOME", state_dir.as_ref().unwrap_or(&dir));
    cmd.cwd(cwd.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR"))));

    let mut child = pair.slave.spawn_command(cmd).expect("spawn hcmd");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
        }
    });

    let mut parser = vt100::Parser::new(rows, cols, 0);
    let deadline = Instant::now() + settle;

    // Let the first frame land, and the directory listing after it, before
    // typing - the listing is asynchronous, so a key sent too
    // early would act on an empty panel.
    let first_frame = Instant::now() + Duration::from_millis(700);
    while Instant::now() < first_frame.min(deadline) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => parser.process(&chunk),
            Err(_) => continue,
        }
    }

    // The writer is held for the whole run. Dropping it closes the last
    // handle on the master, and the slave's next read then returns 0 - which
    // the application sees as end-of-input, not as "nothing more typed".
    let mut writer = pair.master.take_writer().expect("writer");
    if !input.is_empty() {
        writer.write_all(input).expect("write input");
        writer.flush().expect("flush");
    }

    let mut exited = false;
    while Instant::now() < deadline {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            parser.process(&chunk);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
    }
    if !exited {
        let _ = child.kill();
    }
    drop(writer);
    let _ = std::fs::remove_dir_all(&dir);
    (parser, exited)
}

fn screen_text(parser: &vt100::Parser) -> String {
    String::from_utf8_lossy(&parser.screen().contents_formatted())
        .chars()
        .collect::<String>()
}

fn plain(parser: &vt100::Parser) -> String {
    parser.screen().contents()
}

/// The three cursor-bar styles of the design, as the blue theme paints them.
mod cursor_bg {
    pub const FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
    pub const UNFOCUSED: (u8, u8, u8) = (0x00, 0x78, 0x78);
    pub const INACTIVE: (u8, u8, u8) = (0x20, 0x20, 0xB0);
}

/// The text of the row painted in `bg`, trimmed - that is, the entry the panel
/// cursor is sitting on. `None` when no row carries that background.
///
/// Reading the cursor back out of the rendered cells is the only honest way to
/// check "the cursor moved": the bar is a background style, so it does not
/// appear in the screen's plain text at all.
fn row_with_bg(parser: &vt100::Parser, bg: (u8, u8, u8)) -> Option<String> {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let want = vt100::Color::Rgb(bg.0, bg.1, bg.2);
    for row in 0..rows {
        let mut text = String::new();
        let mut hit = false;
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.bgcolor() == want {
                hit = true;
                text.push_str(cell.contents());
            }
        }
        if hit {
            return Some(text.trim().to_string());
        }
    }
    None
}

/// The leftmost column painted in `bg`, which is how a test tells the left
/// panel's cursor bar from the right panel's.
fn column_of_bg(parser: &vt100::Parser, bg: (u8, u8, u8)) -> Option<u16> {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let want = vt100::Color::Rgb(bg.0, bg.1, bg.2);
    for row in 0..rows {
        for col in 0..cols {
            if screen.cell(row, col).is_some_and(|c| c.bgcolor() == want) {
                return Some(col);
            }
        }
    }
    None
}

/// The column header of the left panel, where the sort arrow lives.
fn header_line(parser: &vt100::Parser) -> String {
    plain(parser)
        .lines()
        .find(|l| l.contains("Name"))
        .unwrap_or_default()
        .to_string()
}

// Legacy (non-enhanced) encodings, which is what the pty harness runs under.
const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const LEFT: &[u8] = b"\x1b[D";
const RIGHT: &[u8] = b"\x1b[C";
const ENTER: &[u8] = b"\r";
const BACKSPACE: &[u8] = b"\x7f";
const TAB: &[u8] = b"\t";
const ESC: &[u8] = b"\x1b";

fn keys(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn it_draws_two_panels_a_command_line_and_the_key_bar() {
    let (parser, _) = run_in_pty(Run::new(100, 30));
    let text = plain(&parser);
    assert!(
        text.contains("F3") && text.contains("View"),
        "key bar missing:\n{text}"
    );
    // Ten slots, F1..F10, the same key column in every modifier layer.
    // Labels are abbreviated to fit a fixed field.
    for key in ["F1", "F2", "F7", "F9", "F10"] {
        assert!(text.contains(key), "key bar missing {key}:\n{text}");
    }
    assert!(
        text.contains("NewFldr") || text.contains("NewF"),
        "key bar missing its TC labels:\n{text}"
    );
    assert!(text.contains('>'), "command-line prompt missing:\n{text}");
    // Two panel boxes, so at least two vertical borders on a middle row.
    let borders = text
        .lines()
        .filter(|l| l.matches(['│', '|']).count() >= 3)
        .count();
    assert!(borders > 3, "expected two bordered panels:\n{text}");
    let _ = screen_text(&parser);
}

#[test]
fn f10_quits_and_the_terminal_is_restored() {
    // F10 in the legacy encoding, then `y` for the ui.confirm_exit prompt
    // - on by default, so a bare F10 no longer exits.
    let (parser, exited) = run_in_pty(
        Run::new(100, 30)
            .input(b"\x1b[21~y")
            .settle(Duration::from_secs(5)),
    );
    assert!(exited, "F10 should quit:\n{}", plain(&parser));
    assert!(
        !parser.screen().alternate_screen(),
        "the alternate screen must be left on exit"
    );
}

#[test]
fn alt_q_quits() {
    let (parser, exited) = run_in_pty(
        Run::new(100, 30)
            .input(b"\x1bqy")
            .settle(Duration::from_secs(5)),
    );
    assert!(
        exited,
        "alt+q is the reliable quit key:\n{}",
        plain(&parser)
    );
}

#[test]
fn a_sixty_by_fifteen_terminal_still_lays_out() {
    // the design names 60x15 as the minimum usable size.
    let (parser, _) = run_in_pty(Run::new(60, 15));
    let text = plain(&parser);
    assert!(
        !text.contains("terminal too small"),
        "60x15 is supposed to work:\n{text}"
    );
}

#[test]
fn below_the_minimum_size_says_so_rather_than_breaking() {
    let (parser, _) = run_in_pty(Run::new(40, 10));
    let text = plain(&parser);
    assert!(
        text.contains("terminal too small"),
        "expected the message:\n{text}"
    );
}

#[test]
fn a_one_by_one_terminal_does_not_crash() {
    // "re-layout, never crash on a 1x1 terminal". Still running
    // after the settle is the assertion - a panic would have exited.
    let (parser, exited) = run_in_pty(Run::new(1, 1));
    assert!(
        !exited,
        "1x1 must be laid out, not fatal:\n{}",
        plain(&parser)
    );
}

#[test]
fn a_bracketed_paste_lands_in_the_command_line_as_text() {
    // bracketed paste exists so a pasted path is not interpreted as
    // a sequence of navigation keys. The embedded newline must not submit it
    // and must not survive into the line.
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .without_console()
            .input(b"\x1b[200~/tmp/pasted\ndir\x1b[201~")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("/tmp/pasteddir"),
        "the paste should be one run of text on the command line:\n{text}"
    );
}

#[test]
#[ignore = "prints the rendered screen for eyeballing; run with --ignored"]
fn dump_the_screen() {
    let fix = Fixture::new("dump");
    let keys = std::env::var("HCMD_DUMP_KEYS").unwrap_or_default();
    let cols: u16 = std::env::var("HCMD_DUMP_COLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let (parser, _) = run_in_pty(
        Run::new(cols, 24)
            .cwd(fix.path())
            .input(keys.as_bytes())
            .settle(Duration::from_secs(3)),
    );
    println!("{}", plain(&parser));
}

// ---------------------------------------------------------------------------
// the input model, driven through the real binary against a real
// directory tree. These are the acceptance criteria that cannot be checked by
// reading the code.
// ---------------------------------------------------------------------------

#[test]
fn the_panel_lists_a_real_directory_with_exactly_one_parent_row() {
    let fix = Fixture::new("listing");
    let (parser, _) = run_in_pty(Run::new(100, 30).cwd(fix.path()));
    let text = plain(&parser);
    for name in fix.listed() {
        // `alpha.rs` and `zeta.txt` are drawn with Name and Ext in separate
        // columns, so the stem is what appears contiguously.
        let stem = name.split('.').next().unwrap_or(name);
        assert!(
            text.contains(stem),
            "{stem} missing from the listing:\n{text}"
        );
    }
    // The `..` row is synthesised by the backend and by nothing else. Both
    // panels start in the same directory, so there are exactly two on screen.
    let parents = text.matches("[..]").count();
    assert_eq!(parents, 2, "one `..` row per panel, not two:\n{text}");
}

#[test]
fn enter_descends_into_a_directory_and_backspace_comes_back() {
    let fix = Fixture::new("enter");
    let down_to_subdir = keys(&[DOWN, ENTER]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(&down_to_subdir)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("inner") && text.contains("deeper"),
        "Enter should have descended into subdir:\n{text}"
    );

    // Backspace with an empty quick-search buffer goes to the parent.
    //
    let there_and_back = keys(&[DOWN, ENTER, BACKSPACE]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(&there_and_back)
            .settle(Duration::from_secs(5)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("thunder") && !text.contains("inner"),
        "Backspace should have gone back up:\n{text}"
    );
}

#[test]
fn ctrl_pgup_goes_to_the_parent_too() {
    let fix = Fixture::new("ctrlpgup");
    // Ctrl+PgUp in the legacy encoding.
    let input = keys(&[DOWN, ENTER, b"\x1b[5;5~"]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(5)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("thunder") && !text.contains("inner"),
        "Ctrl+PgUp is the parent key that is not Backspace:\n{text}"
    );
}

#[test]
fn typing_moves_the_cursor_to_the_matching_entry() {
    let fix = Fixture::new("quick");
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(b"thu")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("search: thu"),
        "the buffer belongs in the panel status line:\n{text}"
    );
    let on = row_with_bg(&parser, cursor_bg::FOCUSED)
        .unwrap_or_else(|| panic!("no focused cursor bar drawn:\n{text}"));
    assert!(
        on.contains("thunder"),
        "typing `thu` should land on `thunder`, not on {on:?}:\n{text}"
    );
}

#[test]
fn a_quick_search_that_matches_nothing_keeps_the_buffer_and_says_so() {
    let fix = Fixture::new("nomatch");
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(b"zzz")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("no match"),
        "a miss flashes rather than silently doing nothing:\n{text}"
    );
}

#[test]
fn backspace_pops_the_search_buffer_before_it_navigates() {
    let fix = Fixture::new("pop");
    // `thu` matches `thunder`; one Backspace leaves `th`, which still matches
    // and must NOT have gone up a directory.
    let input = keys(&[b"thu", BACKSPACE]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("search: th") && text.contains("thunder"),
        "Backspace pops the buffer while one is running:\n{text}"
    );
}

#[test]
fn esc_clears_the_search_buffer() {
    let fix = Fixture::new("esc");
    let input = keys(&[b"thu", ESC]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(!text.contains("search:"), "Esc clears the buffer:\n{text}");
}

#[test]
fn tab_moves_the_active_cursor_to_the_other_panel() {
    let fix = Fixture::new("tabswitch");
    let (parser, _) = run_in_pty(Run::new(100, 30).cwd(fix.path()));
    let before = plain(&parser);
    // Before: the left panel has the focused bar, the right the inactive one.
    assert!(
        row_with_bg(&parser, cursor_bg::FOCUSED).is_some(),
        "the active panel's cursor bar:\n{before}"
    );
    assert!(
        row_with_bg(&parser, cursor_bg::INACTIVE).is_some(),
        "the inactive panel gets a third, weaker style:\n{before}"
    );

    // Both cursors are always visible; Tab swaps which is which. The two panels
    // start in the same directory, so compare positions rather than text.
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(TAB)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    let focused = column_of_bg(&parser, cursor_bg::FOCUSED)
        .unwrap_or_else(|| panic!("no focused bar after Tab:\n{text}"));
    let inactive = column_of_bg(&parser, cursor_bg::INACTIVE)
        .unwrap_or_else(|| panic!("no inactive bar after Tab:\n{text}"));
    assert!(
        focused > inactive,
        "Tab should have made the right panel active:\n{text}"
    );
}

#[test]
fn left_focuses_the_command_line_and_up_leaves_it_with_the_text_intact() {
    let fix = Fixture::new("cmdfocus");
    // Left enters the command line, `cp ` is typed there, Up leaves again.
    let input = keys(&[LEFT, b"cp ", UP]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .without_console()
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("> cp") || text.contains("cp "),
        "the text survives leaving the command line:\n{text}"
    );
    // Focus is back on the panel, so the panel's bar is the focused style
    // again and the command-line caret is the painted block.
    assert!(
        row_with_bg(&parser, cursor_bg::FOCUSED).is_some(),
        "Up returns focus to the panel:\n{text}"
    );
}

#[test]
fn the_panel_cursor_stays_visible_while_the_command_line_has_focus() {
    let fix = Fixture::new("bothcursors");
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(RIGHT)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    // neither cursor ever disappears. With the command line
    // focused the active panel's bar switches to the unfocused style, and the
    // other panel keeps the weaker inactive one.
    assert!(
        row_with_bg(&parser, cursor_bg::UNFOCUSED).is_some(),
        "the active panel's bar must stay visible, dimmed:\n{text}"
    );
    assert!(
        row_with_bg(&parser, cursor_bg::INACTIVE).is_some(),
        "the inactive panel's bar must stay visible too:\n{text}"
    );
}

#[test]
fn ctrl_enter_inserts_the_entry_under_the_cursor_at_the_remembered_caret() {
    let fix = Fixture::new("putselected");
    // The the design walkthrough, exactly: Right into the command line, type
    // `cp `, Up back to the panel, move to a file, Ctrl+Enter.
    // Ctrl+Enter has no legacy encoding, so this uses the bound alias the
    // shipped keymap provides for terminals without the enhanced protocol.
    let input = keys(&[RIGHT, b"cp ", UP, DOWN, DOWN, DOWN, b"\x1b\r"]);
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .without_console()
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("cp Thorin") || text.contains("cp thorin") || text.contains("cp alpha.rs"),
        "Ctrl+Enter should have inserted a filename after `cp `:\n{text}"
    );
}

#[test]
fn ctrl_digits_sort_the_panel_positionally() {
    let fix = Fixture::new("sort");
    // Ctrl+2 is the second column of the default order, `ext`.
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(b"\x1b[50;5u")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("[ext ") || text.contains("Ext"),
        "the sort indicator names the column:\n{text}"
    );
}

#[test]
fn columns_drop_by_priority_as_the_panel_narrows() {
    let fix = Fixture::new("narrow");
    // Wide: every configured column is drawn.
    let (parser, _) = run_in_pty(Run::new(140, 30).cwd(fix.path()));
    let wide = header_line(&parser);
    assert!(
        wide.contains("Ext") && wide.contains("Size") && wide.contains("Date"),
        "a wide panel shows the whole column set:\n{wide}"
    );

    // Narrow: `hide_priority` is attr, ext, size, date - so attr and ext go
    // first and date outlives size.
    let (parser, _) = run_in_pty(Run::new(72, 30).cwd(fix.path()));
    let narrow = header_line(&parser);
    assert!(
        !narrow.contains("Attr"),
        "attr is first in hide_priority:\n{narrow}"
    );
    assert!(narrow.contains("Date"), "date is kept longest:\n{narrow}");
    assert!(
        wide.len() >= narrow.len(),
        "narrowing must not widen the header"
    );
}

#[test]
fn f9_drops_the_menu_bar_and_esc_gives_the_panel_back() {
    // "F9 drops a menu bar of six menus - Files, Mark, Commands,
    // Net, Show, Configuration ... Esc closes it and gives the panel back."
    //
    // This test used to press F9 to demonstrate an out-of-scope key reporting
    // itself; v0.7 is the last line, so F9 now does its job and the
    // test asserts that instead of the message it used to raise.
    //
    let fix = Fixture::new("menubar");
    let (parser, _) = run_in_pty(
        Run::new(200, 30)
            .cwd(fix.path())
            .input(b"\x1b[20~")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    for title in ["Files", "Mark", "Commands", "Net", "Show", "Configuration"] {
        assert!(text.contains(title), "the bar shows {title}:\n{text}");
    }

    let (parser, _) = run_in_pty(
        Run::new(200, 30)
            .cwd(fix.path())
            .input(&keys(&[b"\x1b[20~", ESC]))
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("thunder"),
        "Esc gives the panel back:\n{text}"
    );
}

#[test]
fn a_status_message_keeps_its_verdict_when_the_panel_is_too_narrow_for_it() {
    // The message is middle-cropped, so the half that says why nothing
    // happened survives. End-cropping would keep only the subject.
    //
    // A failed quick search rather than F9: F9 opens the menu bar now that
    // v0.7 has landed, and `no match: <query>` is the message that is still
    // one keystroke away and is as long as the query typed into it.
    //
    let fix = Fixture::new("notimpl-narrow");
    // A file whose name is the long run, so the run *matches* and the buffer
    // really does grow. Since 2026-08-30 a character that matches nothing is
    // refused rather than typed, so 44 junk keys no longer build a long
    // message - they build no message at all beyond the first refusal. The
    // subject here is the cropping, so the message still has to be long.
    let long = "q".repeat(41);
    std::fs::write(fix.path().join(format!("{long}.txt")), b"x").expect("long name");
    let query = format!("{long}X").into_bytes();
    let query = query.as_slice();
    let (parser, _) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .input(query)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    // The case indicator is the last thing in the message, so its presence is
    // the proof that the *end* survived the crop. End-cropping would have
    // thrown it away and kept only "no match: qqqq...".
    assert!(
        text.contains("[aa]"),
        "the tail of the message survives, which end-cropping would throw \
         away:\n{text}"
    );
    assert!(
        text.contains("no match: q"),
        "and so does its head:\n{text}"
    );
}

#[test]
fn tabs_are_saved_on_quit_and_restored_on_the_next_start() {
    // "Tabs are persisted per panel to
    // ~/.local/state/holoscommander/ and restored on start."
    let fix = Fixture::new("persist");
    let state = std::env::temp_dir().join(format!("hcmd-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);

    // Run one: descend into subdir, open a second tab there, quit with F10 -
    // and answer the ui.confirm_exit prompt.
    let input = keys(&[DOWN, ENTER, b"\x14", b"\x1b[21~", b"y"]);
    let (_, exited) = run_in_pty(
        Run::new(100, 30)
            .cwd(fix.path())
            .state_dir(&state)
            .input(&input)
            .settle(Duration::from_secs(6)),
    );
    assert!(exited, "the first run should have quit on F10");

    let saved = state.join("holoscommander/tabs.toml");
    let text = std::fs::read_to_string(&saved).unwrap_or_else(|e| {
        panic!("no tab state at {}: {e}", saved.display());
    });
    assert!(
        text.contains("subdir"),
        "the saved state should name the directory that was open:\n{text}"
    );

    // Run two starts somewhere else entirely. The saved paths win, which is
    // what the state file is for.
    let (parser, _) = run_in_pty(
        Run::new(110, 30)
            .cwd("/")
            .state_dir(&state)
            .settle(Duration::from_secs(4)),
    );
    let screen = plain(&parser);
    assert!(
        screen.contains("subdir"),
        "the restored panel should be back in subdir, not in `/`:\n{screen}"
    );
    // Two tabs on the left panel means the tab bar is drawn.
    assert!(
        screen.contains("inner"),
        "and it should have read the restored tab's listing:\n{screen}"
    );

    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn holding_shift_swaps_the_key_bar_labels() {
    // the key bar shows the Shift+F<n> actions while Shift is held,
    // "where the terminal reports modifier state". Under the enhanced keyboard
    // protocol a bare Shift arrives as `CSI 57441 u`, with `:3` for the
    // release. Feeding those bytes exercises the same path a
    // kitty-protocol terminal drives.
    let fix = Fixture::new("shiftbar");
    let (parser, _) = run_in_pty(Run::new(130, 30).cwd(fix.path()));
    let unshifted = plain(&parser);
    assert!(
        unshifted.contains("F5 Copy") || unshifted.contains("Copy"),
        "the unshifted key bar:\n{unshifted}"
    );

    let (parser, _) = run_in_pty(
        Run::new(130, 30)
            .cwd(fix.path())
            .input(b"\x1b[57441u")
            .settle(Duration::from_secs(4)),
    );
    let held = plain(&parser);
    assert!(
        held != unshifted,
        "the key bar should have changed while Shift is held:\n{held}"
    );
    assert!(
        held.contains("Rename") || held.contains("Compare") || held.contains("DelPerm"),
        "expected the Shift+F<n> operations:\n{held}"
    );

    // ...and released, it goes back.
    let (parser, _) = run_in_pty(
        Run::new(130, 30)
            .cwd(fix.path())
            .input(b"\x1b[57441u\x1b[57441;1:3u")
            .settle(Duration::from_secs(4)),
    );
    let released = plain(&parser);
    assert!(
        released.contains("Copy"),
        "releasing Shift restores the labels:\n{released}"
    );
}

#[test]
fn the_status_line_reports_the_directory_then_the_selection() {
    // the two forms, through the real binary.
    let fix = Fixture::new("counts");
    let (parser, _) = run_in_pty(Run::new(120, 30).cwd(fix.path()));
    let text = plain(&parser);
    let totals = format!("in {} files, 1 dir", fix.file_count());
    assert!(
        text.contains(&totals),
        "nothing marked: the directory's own totals ({totals}):\n{text}"
    );
    assert!(
        !text.contains(" selected"),
        "nothing is marked, so there is no selection to report:\n{text}"
    );

    // Insert marks the entry under the cursor and moves down. The cursor starts
    // on `[..]`, which never marks, so move onto a file first.
    let input = keys(&[DOWN, DOWN, b"\x1b[2~"]);
    let (parser, _) = run_in_pty(
        Run::new(120, 30)
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    let selected = format!("1 of {} selected", fix.file_count() + 1);
    assert!(
        text.contains(&selected),
        "the selection against the total ({selected}):\n{text}"
    );
}

#[test]
fn a_marked_directory_makes_the_selection_size_a_lower_bound() {
    // a directory contributes to the counts always and to the
    // size only once sized, and v0.1 sizes none - so the total says so.
    let fix = Fixture::new("bound");
    // `[subdir]` sorts first among the real entries, right after `[..]`.
    let input = keys(&[DOWN, b"\x1b[2~"]);
    let (parser, _) = run_in_pty(
        Run::new(120, 30)
            .cwd(fix.path())
            .input(&input)
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    let selected = format!("1 of {} selected", fix.file_count() + 1);
    assert!(
        text.contains(&selected),
        "the directory is marked ({selected}):\n{text}"
    );
    assert!(
        text.contains('\u{2265}'),
        "an unsized directory in the selection makes the size a bound:\n{text}"
    );
}

#[test]
fn the_sort_tag_appears_only_when_its_column_is_not_drawn() {
    // Wide: `name` is sorted and drawn, so its header carries
    // the arrow and the status line keeps its width for the counts.
    let fix = Fixture::new("sorttag");
    let (parser, _) = run_in_pty(Run::new(120, 30).cwd(fix.path()));
    let wide = plain(&parser);
    assert!(wide.contains("\u{25B2}Name"), "the header arrow:\n{wide}");
    assert!(
        !wide.contains("[name \u{25B2}]"),
        "no redundant tag beside a visible arrow:\n{wide}"
    );

    // Ctrl+4 sorts by `date`, then narrowing hides it - and the tag appears.
    let (parser, _) = run_in_pty(
        Run::new(64, 20)
            .cwd(fix.path())
            .input(b"\x1b[52;5u")
            .settle(Duration::from_secs(4)),
    );
    let narrow = plain(&parser);
    assert!(
        !narrow.contains("Date"),
        "`date` should be hidden at this width:\n{narrow}"
    );
    assert!(
        narrow.contains("[date "),
        "a hidden sorted column is exactly what the tag is for:\n{narrow}"
    );
}

#[test]
fn f3_opens_the_viewer_over_the_whole_screen() {
    // the design: the viewer consumes all input and, like the console,
    // is not a pane - the panels are not drawn at all.
    let fix = Fixture::new("viewer");
    std::fs::write(
        fix.path().join("viewme.txt"),
        b"first streamed line\nsecond streamed line\n",
    )
    .expect("fixture file");

    // Quick-search to the file, then F3.
    let (parser, _) = run_in_pty(
        Run::new(120, 24)
            .cwd(fix.path())
            .input(b"viewme\x1bOR")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("first streamed line"),
        "the file's content is on screen:\n{text}"
    );
    assert!(
        text.contains("viewme.txt"),
        "and the title says what is being viewed:\n{text}"
    );
    assert!(
        !text.contains("Ext"),
        "the panels are gone, not shrunk:\n{text}"
    );
    assert!(
        text.contains("UTF-8"),
        "the status line names the active encoding:\n{text}"
    );
}

#[test]
fn the_viewer_switches_to_hex_and_closes_again() {
    // the `2`, and the `Esc`.
    let fix = Fixture::new("viewer-hex");
    std::fs::write(fix.path().join("viewme.txt"), b"AB\n").expect("fixture file");

    let (parser, _) = run_in_pty(
        Run::new(120, 24)
            .cwd(fix.path())
            .input(b"viewme\x1bOR2")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("41 42 0a"),
        "hex mode shows the bytes:\n{text}"
    );
    assert!(text.contains("00000000"), "with an offset column:\n{text}");

    // Esc gives the panels back.
    let (parser, _) = run_in_pty(
        Run::new(120, 24)
            .cwd(fix.path())
            .input(b"viewme\x1bOR\x1b")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("Ext"),
        "Esc closes the viewer and the panels come back:\n{text}"
    );
}

/// The foreground of the first cell on `row` whose text is `want`.
///
/// By row, because the title row carries the file's whole path and would
/// otherwise answer for any letter that happens to be in it.
fn fg_in_row(parser: &vt100::Parser, row: u16, want: &str) -> Option<(u8, u8, u8)> {
    let screen = parser.screen();
    let (_, cols) = screen.size();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.contents() == want
            && let vt100::Color::Rgb(r, g, b) = cell.fgcolor()
        {
            return Some((r, g, b));
        }
    }
    None
}

/// The text of every cell painted with background `bg`, in screen order.
///
/// A quick-find match is a **background** (the `viewer.match`), so
/// this is the only honest way to ask "was it highlighted": the highlight does
/// not appear in the screen's plain text at all.
fn cells_with_bg(parser: &vt100::Parser, bg: (u8, u8, u8)) -> String {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let want = vt100::Color::Rgb(bg.0, bg.1, bg.2);
    let mut out = String::new();
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.bgcolor() == want {
                out.push_str(cell.contents());
            }
        }
    }
    out
}

/// The blue theme's `syn.*` and `viewer.*` slots, as it paints them.
mod viewer_colors {
    pub const KEYWORD: (u8, u8, u8) = (0xFF, 0x54, 0xFF);
    pub const STRING: (u8, u8, u8) = (0x54, 0xFF, 0x54);
    pub const COMMENT: (u8, u8, u8) = (0x80, 0x80, 0x80);
    pub const MATCH: (u8, u8, u8) = (0xFF, 0xFF, 0x54);
    pub const CURRENT_MATCH: (u8, u8, u8) = (0xFF, 0x54, 0x54);
}

/// The four shapes of file the design has to survive, in one fixture.
fn viewer_fixture(tag: &str) -> Fixture {
    let fix = Fixture::new(tag);
    std::fs::write(
        fix.path().join("code.rs"),
        b"fn main() {\n\tlet x = \"hello world\";\n\t// a comment\n\tprintln!(\"{x}\");\n}\n",
    )
    .expect("source fixture");
    std::fs::write(
        fix.path().join("bin.dat"),
        [0x7f, 0x45, 0x4c, 0x46, 0x00, 0x01, 0x02, 0x03, 0x00, 0xff],
    )
    .expect("binary fixture");
    std::fs::write(fix.path().join("empty.txt"), b"").expect("empty fixture");
    std::fs::write(fix.path().join("nonl.txt"), b"no trailing newline here")
        .expect("unterminated fixture");
    fix
}

#[test]
fn a_source_file_is_highlighted_through_the_theme_not_syntects() {
    // "Map syntect styles onto theme slots, not onto syntect's own themes".
    // So the assertion is about the *theme's* colours.
    let fix = viewer_fixture("viewer-highlight");
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"code\x1bOR")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("fn main() {"),
        "the source is on screen:\n{text}"
    );
    assert_eq!(
        fg_in_row(&parser, 1, "f"),
        Some(viewer_colors::KEYWORD),
        "`fn` is painted in `syn.keyword`:\n{text}"
    );
    assert_eq!(
        fg_in_row(&parser, 2, "w"),
        Some(viewer_colors::STRING),
        "and the string literal in `syn.string`:\n{text}"
    );
    assert_eq!(
        fg_in_row(&parser, 3, "/"),
        Some(viewer_colors::COMMENT),
        "and the comment in `syn.comment` - including the `//` that opens it,\n\
         which is the `punctuation.definition.comment` trap:\n{text}"
    );
    // The tab-indented lines are the ones that catch a mapping done before tab
    // expansion: `let` is four columns in, and colouring it at byte 1 would
    // paint the indent instead.
    assert!(
        text.contains("    let x ="),
        "tabs are expanded to `viewer.tab_width`:\n{text}"
    );
}

#[test]
fn a_binary_file_opens_in_hex_with_both_offset_bases() {
    // "A file detected as binary opens in hex automatically",
    // and "byte offsets are shown in both hex and decimal".
    let fix = viewer_fixture("viewer-binary");
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"bin\x1bOR")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("7f 45 4c 46 00 01 02 03 00 ff"),
        "hex mode, with no key pressed to ask for it:\n{text}"
    );
    assert!(
        text.contains("00000000  ( 0)"),
        "the row offset is in both bases:\n{text}"
    );
    assert!(
        text.contains(".ELF......"),
        "and the ASCII gutter is ASCII, whatever the encoding:\n{text}"
    );
    assert!(
        text.contains("binary"),
        "the status line says why it opened this way:\n{text}"
    );

    // And `1` overrides it, which is the "unless overridden" half.
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"bin\x1bOR1")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("text"),
        "`1` switches a binary file to text mode:\n{text}"
    );
    assert!(
        !text.contains("7f 45 4c 46"),
        "and the hex rows are gone:\n{text}"
    );
}

#[test]
fn an_empty_file_and_an_unterminated_one_both_open() {
    // The two file shapes with no line to stand on: nothing at all, and a last
    // line with no terminator. Neither may refuse to open and neither may
    // invent a line that is not there.
    let fix = viewer_fixture("viewer-edges");

    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"empty\x1bOR")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("empty.txt") && text.contains("of 0"),
        "an empty file opens and says it is empty:\n{text}"
    );

    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"nonl\x1bOR")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("no trailing newline here"),
        "an unterminated last line is still shown:\n{text}"
    );
    assert!(
        text.contains("line 1/1"),
        "and counts as one line, not two:\n{text}"
    );
}

#[test]
fn quick_find_searches_as_you_type_and_n_steps() {
    // "typing searches immediately, incrementally, with the first
    // match highlighted as you type", and the `n`.
    let fix = viewer_fixture("viewer-find");

    // `F7`, then the pattern. Nothing else - the search is the typing.
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"code\x1bOR\x1b[18~hello")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("find: hello [aa]"),
        "the bar shows the pattern and its effective case mode \
         (the shape, the content):\n{text}"
    );
    assert!(
        text.contains("1/1"),
        "and the background counter has filled the count in behind it \
:\n{text}"
    );
    assert_eq!(
        cells_with_bg(&parser, viewer_colors::CURRENT_MATCH),
        "hello",
        "the match is painted in `viewer.current_match`:\n{text}"
    );

    // `Enter` leaves the bar keeping the position, then `n` steps. Five `l`s in
    // the file: one is the current match and the other four are plain matches.
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"code\x1bOR\x1b[18~l\rn")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        !text.contains("find:"),
        "the bar is closed and the numbers are back:\n{text}"
    );
    assert_eq!(
        cells_with_bg(&parser, viewer_colors::CURRENT_MATCH).len(),
        1,
        "exactly one match is the current one:\n{text}"
    );
    assert_eq!(
        cells_with_bg(&parser, viewer_colors::MATCH),
        "llll",
        "and the other four are painted in `viewer.match`:\n{text}"
    );
    assert!(
        text.contains("0x18 (24)"),
        "`n` moved the cursor onto the second match, and the status line says \
         where in both bases:\n{text}"
    );
}

#[test]
fn ctrl_g_refuses_an_offset_past_the_end_rather_than_clamping() {
    // "`Ctrl+G` jumps to an offset, accepting `0x` notation."
    // An offset that is not in the file is a question with no answer, and
    // landing somewhere else would be a wrong answer that looked right.
    let fix = viewer_fixture("viewer-goto");
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"bin\x1bOR\x07999999\r")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("is past the end") && text.contains("10 bytes"),
        "the refusal names the size, in both bases:\n{text}"
    );
    assert!(
        text.contains("7f 45 4c 46"),
        "and the viewer did not move:\n{text}"
    );

    // A legal offset in `0x` notation lands.
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"bin\x1bOR\x070x4\r")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(text.contains("0x4 (4)"), "`0x4` is byte four:\n{text}");
}

#[test]
fn f8_cycles_the_encoding_and_says_it_was_chosen() {
    // "`F8` cycles through a configurable shortlist … so a
    // mis-detected file is one keystroke from readable", and the status line
    // shows the active encoding.
    let fix = viewer_fixture("viewer-encoding");
    let (parser, _) = run_in_pty(
        Run::new(100, 14)
            .cwd(fix.path())
            .input(b"code\x1bOR\x1b[19~")
            .settle(Duration::from_secs(4)),
    );
    let text = plain(&parser);
    assert!(
        text.contains("[chosen]"),
        "the status line distinguishes a chosen encoding from a detected one \
:\n{text}"
    );
    assert!(
        !text.contains("UTF-8 [auto]"),
        "and it is no longer the detected one:\n{text}"
    );
}

/// "never crash on a 1x1 terminal", asked of the viewer's four
/// painters at once - hex geometry, the line-number gutter, the syntax spans
/// and the match runs all crop independently and all crop against the same
/// width.
///
/// `#[ignore]`d because it is twenty pty starts and takes forty seconds. Run it
/// with `cargo test --test tui_smoke tiny_terminals -- --ignored` after
/// touching anything that measures.
#[test]
#[ignore]
fn tiny_terminals_do_not_crash_the_viewer() {
    let fix = viewer_fixture("viewer-tiny");
    for (cols, rows) in [(1, 1), (2, 3), (8, 4), (20, 5), (40, 6)] {
        for keys in [
            &b"code\x1bOR"[..],
            &b"bin\x1bOR"[..],
            &b"code\x1bOR\x1b[18~l"[..],
            &b"code\x1bOR\x1b[18~l\rn"[..],
        ] {
            let (_, exited) = run_in_pty(
                Run::new(cols, rows)
                    .cwd(fix.path())
                    .input(keys)
                    .settle(Duration::from_secs(2)),
            );
            assert!(!exited, "{cols}x{rows} died on {keys:?}");
        }
    }
}

/// A directory holding one PNG of known size, for the resize dialog.
///
/// The image is real rather than a stub: the dialog's top line reports the
/// pixel size and the format, and both are read from the file itself.
struct ImageFixture {
    root: PathBuf,
}

impl ImageFixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-img-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        let image = image::RgbImage::new(400, 300);
        image.save(root.join("holiday.png")).expect("fixture image");
        Self { root }
    }
}

impl Drop for ImageFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_resize_dialog_says_what_it_is_resizing_and_what_it_will_write() {
    let fixture = ImageFixture::new("resize");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Down off `..` and onto the image, then Shift+R.
    let input = keys(&[DOWN, b"R"]);
    run.input = &input;
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);

    // The top line: 50% is meaningless without "of what".
    assert!(
        text.contains("holiday.png"),
        "resize dialog does not name its subject:\n{text}"
    );
    assert!(
        text.contains("400 x 300"),
        "resize dialog does not show the current pixel size:\n{text}"
    );

    // The two name fields say they are names, and which end they go on.
    assert!(
        text.contains("prefix filename"),
        "prefix checkbox does not say what it does:\n{text}"
    );
    assert!(
        text.contains("append to filename"),
        "postfix checkbox does not say what it does:\n{text}"
    );

    // And the output name is stated, not inferred.
    assert!(
        text.contains("Saved as:"),
        "resize dialog does not state the output name:\n{text}"
    );
    // The name field is drawn as a box even when its checkbox is off, so the
    // row shows there is a name to type. The box is a background run rather
    // than characters, which `plain` cannot see, so read the cells: after the
    // label there must be a wide run in a colour the dialog body does not use.
    let row = text
        .lines()
        .position(|l| l.contains("prefix filename"))
        .expect("prefix row on screen");
    let row = u16::try_from(row).expect("row fits");
    let screen = parser.screen();
    let label_col = u16::try_from(
        text.lines()
            .nth(usize::from(row))
            .and_then(|l| l.find("prefix filename"))
            .expect("label column"),
    )
    .expect("column fits");
    let body_bg = screen
        .cell(row, label_col)
        .map(vt100::Cell::bgcolor)
        .expect("a cell under the label");
    // Bounded at the dialog's own right border. Without that the count runs
    // out over the panel behind the dialog, which has a different colour for
    // reasons that have nothing to do with this field, and the assertion
    // passes whether the box is drawn or not.
    let line = text.lines().nth(usize::from(row)).expect("the row");
    let right = u16::try_from(
        line[usize::from(label_col)..]
            .find('\u{2502}')
            .map_or(line.len(), |at| usize::from(label_col) + at),
    )
    .expect("border column fits");
    let field = (label_col..right)
        .filter(|col| {
            screen
                .cell(row, *col)
                .is_some_and(|c| c.bgcolor() != body_bg)
        })
        .count();
    assert!(
        field >= 10,
        "the unticked prefix row draws no field box, so nothing shows a name \
         can be typed there: only {field} cells differ from the body colour"
    );
}

/// Two files with identical contents and one that differs, for `Ctrl+F2`.
struct ComparePair {
    root: PathBuf,
}

impl ComparePair {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-cmp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        // Named so the listing order is a.txt, b.txt, c.txt: the test steps
        // the cursor by position, so the order has to be the one it assumes.
        std::fs::write(root.join("a.txt"), b"identical contents").expect("a");
        std::fs::write(root.join("b.txt"), b"identical contents").expect("b");
        std::fs::write(root.join("c.txt"), b"identical contentX").expect("c");
        Self { root }
    }
}

impl Drop for ComparePair {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `Ctrl+F2`, in the encoding a terminal without the Kitty protocol sends.
const CTRL_F2: &[u8] = b"\x1b[1;5Q";

#[test]
fn ctrl_f2_says_two_files_with_the_same_bytes_are_identical() {
    let fixture = ComparePair::new("same");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Left cursor onto a.txt, then across and down onto b.txt.
    let input = keys(&[DOWN, TAB, DOWN, DOWN, CTRL_F2]);
    run.input = &input;
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    assert!(
        text.contains("The two files are identical."),
        "comparing two files with the same bytes did not say so:\n{text}"
    );
    assert!(
        text.contains("Compare files") && text.contains("a.txt") && text.contains("b.txt"),
        "the verdict does not say which two files it is about:\n{text}"
    );
}

#[test]
fn ctrl_f2_says_where_two_files_stop_agreeing() {
    let fixture = ComparePair::new("differ");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Left cursor onto a.txt, then across and down onto c.txt, which differs
    // from it in its last byte only.
    let input = keys(&[DOWN, TAB, DOWN, DOWN, DOWN, CTRL_F2]);
    run.input = &input;
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    // The last byte of eighteen, so the offset is 17 and not the length.
    assert!(
        text.contains("The two files differ, from byte 17."),
        "comparing two files that differ did not say where:\n{text}"
    );
    assert!(
        text.contains("Compare files") && text.contains("a.txt") && text.contains("c.txt"),
        "the verdict does not say which two files it is about:\n{text}"
    );
}

/// A directory holding one very long name and one very large file.
struct WideFixture {
    root: PathBuf,
}

impl WideFixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-wide-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        std::fs::write(
            root.join("a-name-so-long-that-it-cannot-possibly-fit-inside-one-panel-column.txt"),
            b"x",
        )
        .expect("the long name");
        // Sparse, so the fixture costs no disk: the listing reads the length.
        let big = std::fs::File::create(root.join("huge.bin")).expect("the big file");
        big.set_len(3_221_225_472).expect("a length of 3 GB");
        Self { root }
    }
}

impl Drop for WideFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_long_name_does_not_take_over_the_panel_status_line() {
    let fixture = WideFixture::new("status");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Onto the long-named file, which is where the status line used to be
    // replaced by the name itself.
    let input = keys(&[DOWN]);
    run.input = &input;
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    let status = text
        .lines()
        .find(|l| l.contains(" in ") && l.contains("file"))
        .unwrap_or_else(|| panic!("no counts on either status line:\n{text}"));
    assert!(
        !status.contains("a-name-so-long"),
        "the status line shows the filename instead of the counts:\n{text}"
    );
}

#[test]
fn a_size_too_wide_for_its_column_steps_down_to_a_human_one() {
    let fixture = WideFixture::new("size");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    let row = text
        .lines()
        .find(|l| l.contains("huge"))
        .unwrap_or_else(|| panic!("no row for the big file:\n{text}"));
    // 3,221,225,472 does not fit the size column, and an end-crop of it would
    // read as a far smaller number.
    assert!(
        row.contains("3.0 G"),
        "a size too wide for its column was not stepped down:\n{row}"
    );
}

/// A zip under a name no extension table knows, which is the `.apkm` case.
struct OddArchive {
    root: PathBuf,
}

impl OddArchive {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-odd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        // A store-only zip holding one file, written by hand so the fixture
        // needs no zip tool on the machine running the test.
        let name = b"inside-the-apkm.txt";
        let body = b"hello from inside";
        let crc = crc32(body);
        let mut zip: Vec<u8> = Vec::new();
        let mut local = Vec::new();
        local.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00");
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(body.len() as u32).to_le_bytes());
        local.extend_from_slice(&(body.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name);
        local.extend_from_slice(body);
        let offset = zip.len() as u32;
        zip.extend_from_slice(&local);

        let start = zip.len() as u32;
        let mut central = Vec::new();
        central.extend_from_slice(b"PK\x01\x02\x14\x00\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00");
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]);
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
        let central_len = central.len() as u32;
        zip.extend_from_slice(&central);

        zip.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00");
        zip.extend_from_slice(&central_len.to_le_bytes());
        zip.extend_from_slice(&start.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());

        std::fs::write(root.join("chrome.apkm"), &zip).expect("the odd archive");
        Self { root }
    }
}

/// CRC-32, so the fixture's zip is well formed rather than merely zip-shaped.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

impl Drop for OddArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn enter_opens_an_archive_whose_extension_says_nothing() {
    let fixture = OddArchive::new("apkm");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    let input = keys(&[DOWN, ENTER]);
    run.input = &input;
    run.settle = Duration::from_secs(5);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    assert!(
        text.contains("inside-the-apkm"),
        "Enter on a zip called .apkm did not open it:\n{text}"
    );
}

#[test]
fn alt_f6_asks_where_to_unpack_with_the_other_panel_prefilled() {
    let fixture = OddArchive::new("unpackdlg");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Onto the archive, then Alt+F6.
    let input = keys(&[DOWN, b"\x1b[17;3~"]);
    run.input = &input;
    run.settle = Duration::from_secs(5);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    assert!(
        text.contains("Unpack chrome.apkm to"),
        "Alt+F6 unpacked without asking where:\n{text}"
    );
    // The last component, not the whole path: a macOS `$TMPDIR` is
    // `/private/var/folders/g3/...`, long enough that the dialog elides the
    // middle of it, so waiting for the full path waits for text that is never
    // painted.
    let tail = fixture
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .expect("the fixture directory has a name");
    assert!(
        text.contains(&tail),
        "the destination is not prefilled with a directory:\n{text}"
    );
    // Nothing has read the archive, so there is no count to print, and
    // `0 files, 0 folders` would be a wrong one rather than a missing one.
    assert!(
        !text.contains("0 files"),
        "the dialog states a count nothing measured:\n{text}"
    );
}

/// A JSON with a word that survives rendering, for mode 3's find.
struct JsonDoc {
    root: PathBuf,
}

impl JsonDoc {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-doc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        std::fs::write(
            root.join("doc.json"),
            br#"{"alpha":1,"beta":"needle","gamma":[1,2,3],"delta":"needle again"}"#,
        )
        .expect("the document");
        Self { root }
    }
}

impl Drop for JsonDoc {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn mode_three_searches_the_document_it_draws() {
    let fixture = JsonDoc::new("find");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Onto doc.json, F3 to view, 3 for the document, then search.
    let input = keys(&[DOWN, b"\x1b[13~", b"3", b"/needle"]);
    run.input = &input;
    run.settle = Duration::from_secs(5);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    assert!(
        text.contains("find: needle"),
        "the find bar did not open in mode 3:\n{text}"
    );
    // Two matches, and the count is exact: the whole document was read to
    // produce it, so it never wears the streaming search's `+`.
    assert!(
        text.contains("1/2"),
        "mode 3 did not count the matches it can see:\n{text}"
    );
    assert!(
        !text.contains("not available in mode 3"),
        "find is still refused in mode 3:\n{text}"
    );
}

#[test]
fn ctrl_f_opens_an_empty_bar_even_after_a_search() {
    let fixture = JsonDoc::new("clearbar");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // View, document mode, search for `needle`, then Ctrl+F again.
    let input = keys(&[DOWN, b"\x1b[13~", b"3", b"/needle", b"\x06"]);
    run.input = &input;
    run.settle = Duration::from_secs(5);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);
    assert!(
        text.contains("find:"),
        "the find bar is not showing:\n{text}"
    );
    assert!(
        !text.contains("find: needle"),
        "Ctrl+F reopened the bar still holding the last pattern:\n{text}"
    );
}

/// Two files that differ in the middle of a long identical run.
struct DiffPair {
    root: PathBuf,
}

impl DiffPair {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("hcmd-diff-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture dir");
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..30 {
            let line = format!("shared line {i}\n");
            old.push_str(&line);
            new.push_str(&line);
        }
        old.push_str("the old text\n");
        new.push_str("the new text\n");
        for i in 30..60 {
            let line = format!("shared line {i}\n");
            old.push_str(&line);
            new.push_str(&line);
        }
        // `a.txt` sorts first, so it is the left panel's cursor row.
        std::fs::write(root.join("a.txt"), old).expect("old");
        std::fs::write(root.join("b.txt"), new).expect("new");
        Self { root }
    }
}

impl Drop for DiffPair {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `Alt+Shift+F2`, in the encoding a legacy terminal sends.
const ALT_SHIFT_F2: &[u8] = b"\x1b[1;4Q";

#[test]
fn alt_shift_f2_shows_the_two_files_as_a_diff() {
    let fixture = DiffPair::new("view");
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Left cursor onto a.txt, across and down onto b.txt, then diff.
    let input = keys(&[DOWN, TAB, DOWN, DOWN, ALT_SHIFT_F2]);
    run.input = &input;
    run.settle = Duration::from_secs(6);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);

    assert!(
        text.contains("--- a.txt") && text.contains("+++ b.txt"),
        "the diff does not name its two sides:\n{text}"
    );
    assert!(
        text.contains("-the old text") && text.contains("+the new text"),
        "the changed lines are not marked in column zero:\n{text}"
    );
    // The unchanged runs are folded **and collapsed**, which is what a diff
    // means by showing a diff: 30 identical lines either side of the change,
    // less three of context, behind one line saying so.
    assert!(
        text.contains("... 27 unchanged lines"),
        "the identical run was not collapsed:\n{text}"
    );
    assert!(
        text.contains(" shared line 29") && !text.contains(" shared line 5"),
        "context is kept and the rest is hidden:\n{text}"
    );
}

/// A git repository holding one committed file that has since been edited.
struct GitTree {
    root: PathBuf,
}

impl GitTree {
    fn new(tag: &str) -> Option<Self> {
        let root = std::env::temp_dir().join(format!("hcmd-gt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).ok()?;
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q"]) {
            return None;
        }
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(
            root.join("notes.txt"),
            "first line\ncommitted body\nlast line\n",
        )
        .ok()?;
        std::fs::write(root.join("readme.md"), "# Title\n\ncommitted body\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "one"]) {
            return None;
        }
        // Edited since. This is the file F3 should show as a diff.
        std::fs::write(
            root.join("notes.txt"),
            "first line\nworking body\nlast line\n",
        )
        .ok()?;
        std::fs::write(root.join("readme.md"), "# Title\n\nworking body\n").ok()?;
        Some(Self { root })
    }
}

impl Drop for GitTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn f3_on_a_modified_tracked_file_shows_its_diff() {
    let Some(fixture) = GitTree::new("f3diff") else {
        eprintln!("SKIPPING f3_on_a_modified_tracked_file: git is not installed");
        return;
    };
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Past [..] and [.git], onto notes.txt.
    let input = keys(&[DOWN, DOWN, b"\x1b[13~"]);
    run.input = &input;
    run.settle = Duration::from_secs(6);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);

    assert!(
        text.contains("notes.txt (HEAD)"),
        "F3 did not open the diff against HEAD:\n{text}"
    );
    assert!(
        text.contains("-committed body") && text.contains("+working body"),
        "the change is not shown:\n{text}"
    );
    // `1` still gives the file's own text, so F3 has not taken that away.
    assert!(
        !text.contains("first line\nfirst line"),
        "the file is shown once, as a diff:\n{text}"
    );
}

#[test]
fn a_modified_markdown_file_still_opens_as_markdown() {
    // The file's own format wins. A diff that displaced the document left a
    // modified `.md` with no way to be read as markdown at all, which is the
    // renderer doing the opposite of its job.
    let Some(fixture) = GitTree::new("mdfirst") else {
        eprintln!("SKIPPING a_modified_markdown_file: git is not installed");
        return;
    };
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    // Past [..] and [.git], onto notes.txt, then down again to readme.md.
    let input = keys(&[DOWN, DOWN, DOWN, b"\x1b[13~"]);
    run.input = &input;
    run.settle = Duration::from_secs(6);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);

    assert!(
        text.contains("render [markdown]"),
        "a modified markdown file did not open as markdown:\n{text}"
    );
    assert!(
        !text.contains("--- readme.md (HEAD)"),
        "the diff displaced the document:\n{text}"
    );
    // And git still says what it knows, so the reader can tell "unmodified"
    // from "not in a repository" when the toggle appears to do nothing.
    assert!(
        text.contains("git modified"),
        "the status line does not say what git knows:\n{text}"
    );
}

#[test]
fn ctrl_d_swaps_the_document_for_the_diff() {
    let Some(fixture) = GitTree::new("toggle") else {
        eprintln!("SKIPPING ctrl_d_swaps: git is not installed");
        return;
    };
    let mut run = Run::new(100, 30);
    run.cwd = Some(fixture.root.clone());
    let input = keys(&[DOWN, DOWN, DOWN, b"\x1b[13~", b"\x04"]);
    run.input = &input;
    run.settle = Duration::from_secs(6);
    let (parser, _) = run_in_pty(run);
    let text = plain(&parser);

    assert!(
        text.contains("--- readme.md (HEAD)"),
        "Ctrl+D did not swap the document for the diff:\n{text}"
    );
    assert!(
        text.contains("-committed body") && text.contains("+working body"),
        "the diff is not showing the change:\n{text}"
    );
}
