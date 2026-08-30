//! The ten v0.4 acceptance criteria, and the nine that
//! the cursor and selection brought with them, driven through the
//! real `hcmd` binary.
//!
//! The `selection_*` tests at the foot of the file are the second set. They use
//! one thing the first ten do not: [`Session::raw`] as a *clipboard*. An
//! `OSC 52` sequence is written **to** the terminal and never lands in a cell,
//! so the only place a copy can be seen from outside is the byte stream itself.
//!
//! Same shape as `tests/acceptance.rs`, `acceptance_ops.rs` and
//! `acceptance_console.rs`: a `portable-pty` pseudo-terminal, real key bytes
//! written into it, a `vt100` parser on the way back, and assertions on the
//! rendered cells. Nothing here inspects the library - the milestone's claim is
//! about what `F3` puts on a terminal, so that is what is read.
//!
//! Four things about this harness the other three do not need:
//!
//! * **The viewer owns the whole screen**, so its geometry
//!   is fixed and simple: row 0 is the title, the last row is the status line,
//!   and everything between is the file. [`Session::title_row`],
//!   [`Session::status_row`] and [`Session::body`] are that layout, and they are
//!   what makes "the top of the window did not move" an assertion rather than a
//!   guess.
//! * **Colours are read per cell, by column.** Criterion 2 is about a *mapping* -
//!   syntect's scopes onto the `syn.*` slots - and the only way to
//!   see a mapping from outside is to read the foreground of the cell a keyword
//!   landed in. [`Session::row_cells`] rebuilds a row from its cells rather than
//!   from `contents()`, so a character's index in the string really is its
//!   column.
//! * **Criterion 2 runs the same file under two themes.** A keyword painted
//!   `#FF54FF` under `blue` and `#FF79C6` under `dracula` is the theme's colour;
//!   a keyword painted the same under both would be syntect's own, which
//!   the design explicitly forbids ("map syntect styles onto theme slots,
//!   not onto syntect's own themes").
//! * **Criterion 9 does not wait for the screen to settle.** A 40 GB file has a
//!   background index scan repainting `indexing n%` continuously, so "unchanged
//!   for a beat" never comes true - and the criterion is a *stopwatch*, which a
//!   250 ms settle would swamp anyway. [`Session::press_timed`] polls the
//!   predicate every couple of milliseconds and returns how long it took.
//!
//! The console is switched off in every test (`[console] enabled = false`).
//! v0.3's shell is a separate milestone with its own harness, and a live `bash`
//! drawing its own prompt into the bottom rows would make "the panel came back
//! with the cursor where it was" an assertion about somebody's `PS1`.
//!
//! The child gets a throwaway `XDG_CONFIG_HOME` and `XDG_STATE_HOME`, so a run
//! neither reads nor corrupts the developer's own configuration or saved tabs.

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
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Keeps concurrently running tests off each other's temporary directories.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Key encodings, verified against crossterm 0.29's
/// `src/event/sys/unix/parse.rs`.
///
/// The `CSI <n> ~` family is `parse_csi_special_key_code`, which reads `11..=15`
/// as `F(n - 10)` and `17..=21` as `F(n - 11)`: `F2` is `12`, `F3` is `13`,
/// `F4` is `14`, `F7` is `18` and `F8` is `19`.
///
/// The `CSI <codepoint> ; <modifier> u` family is the Kitty protocol's; the
/// codepoint is the key's *unshifted* Unicode value and the modifier field is a
/// bitmask offset by one, so `Ctrl+G` is `ESC [ 103 ; 5 u`.
///
/// `Shift+N` (the step-backwards) is deliberately the **bare
/// uppercase byte**, which is what every terminal sends and what crossterm's
/// `char_code_to_event` turns into `Char('N')` + `SHIFT`. It is the harder of
/// the two spellings for the product to get right - `KeyPress::normalized` has
/// to fold the letter and *keep* the modifier - and it is the one a user
/// actually produces.
mod keys {
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const RIGHT: &[u8] = b"\x1b[C";
    pub const ENTER: &[u8] = b"\r";
    pub const ESC: &[u8] = b"\x1b";
    /// `Ctrl+Home` / `Ctrl+End` - the *file's* first and last page.
    /// Bare `Home`/`End` are the line's edges.
    pub const CTRL_HOME: &[u8] = b"\x1b[1;5H";
    /// See [`CTRL_HOME`].
    pub const CTRL_END: &[u8] = b"\x1b[1;5F";
    pub const PAGE_DOWN: &[u8] = b"\x1b[6~";
    pub const PAGE_UP: &[u8] = b"\x1b[5~";

    /// `F2` - the reload. Wrapping is `w`.
    pub const F2: &[u8] = b"\x1b[12~";

    /// `w` - the wrap toggle.
    pub const WRAP: &[u8] = b"w";
    /// `F3` - open the viewer, and close it again.
    pub const F3: &[u8] = b"\x1b[13~";
    /// `F7` - the quick find bar.
    pub const F7: &[u8] = b"\x1b[18~";
    /// `F8` - cycle the encoding.
    pub const F8: &[u8] = b"\x1b[19~";

    /// `Ctrl+G` - go to a byte offset. `g` is codepoint 103.
    pub const CTRL_G: &[u8] = b"\x1b[103;5u";

    /// `n` - the next match.
    pub const N: &[u8] = b"n";
    /// `Shift+N` - the previous match, as a real terminal sends it.
    pub const SHIFT_N: &[u8] = b"N";

    /// `1` and `2` - text and hex.
    pub const ONE: &[u8] = b"1";
    pub const TWO: &[u8] = b"2";

    /// the `Shift` + a movement. `CSI 1 ; <mods> <final>` is the
    /// modified-arrow form every terminal since xterm sends; the modifier field
    /// is a bitmask offset by one, so `Shift` is `2` and `Ctrl+Shift` is `6`.
    pub const SHIFT_RIGHT: &[u8] = b"\x1b[1;2C";
    /// See [`SHIFT_RIGHT`].
    pub const SHIFT_DOWN: &[u8] = b"\x1b[1;2B";
    /// See [`SHIFT_RIGHT`]: `Ctrl+Shift` is modifier `6`.
    pub const CTRL_SHIFT_RIGHT: &[u8] = b"\x1b[1;6C";

    /// `Tab` - the hex side switch. A bare `0x09`, which is what a
    /// terminal sends and what `[viewer] hex_side` has to answer.
    pub const TAB: &[u8] = b"\t";
    /// `Ctrl+A` - select the whole file. `0x01`.
    pub const CTRL_A: &[u8] = b"\x01";
    /// `Ctrl+C` - copy the selection. `0x03`, which is a *key*
    /// here: raw mode has `ISIG` off, so nothing turns it into a signal.
    pub const CTRL_C: &[u8] = b"\x03";
    /// `Ctrl+Shift+C` - copy the interpretation, in the Kitty
    /// form, because `Ctrl+Shift` + a letter has no legacy encoding at all: the
    /// control byte is the same with or without `Shift`. `c` is codepoint 99
    /// and `Ctrl+Shift` is modifier `6`.
    pub const CTRL_SHIFT_C: &[u8] = b"\x1b[99;6u";
    /// See [`SHIFT_RIGHT`]: `Ctrl+Shift+Down` extends rectangularly.
    pub const CTRL_SHIFT_DOWN: &[u8] = b"\x1b[1;6B";
}

/// The theme colours the assertions read back out of the rendered cells.
///
/// Syntax highlighting is a *foreground* and a quick-find match is a
/// *background*, so neither is visible in the
/// screen's plain text and reading the cell colours is the only honest way to
/// check either. These are the compiled-in defaults, which
/// `themes/blue.toml` mirrors, and `COLORTERM=truecolor` keeps them
/// unquantised.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);

    /// `viewer.match` - a match that is not the current one.
    pub const MATCH: (u8, u8, u8) = (0xFF, 0xFF, 0x54);
    /// `viewer.current_match` - the one the cursor is on.
    pub const CURRENT_MATCH: (u8, u8, u8) = (0xFF, 0x54, 0x54);
    /// `viewer.hex_offset` - the offset column of a hex row.
    pub const HEX_OFFSET: (u8, u8, u8) = (0x54, 0xFF, 0xFF);
    /// `viewer.hex_ascii` - the ASCII gutter beside the bytes.
    pub const HEX_ASCII: (u8, u8, u8) = (0x54, 0xFF, 0x54);
    /// `viewer.selection_bg` - the selection, which is a
    /// **background** for the same reason a match is (
    /// the design).
    pub const SELECTION_BG: (u8, u8, u8) = (0xFF, 0x54, 0xFF);
}

/// The two themes criterion 2 compares.
///
/// Both ship in the binary (`src/config/theme.rs`), so `[ui] theme = "dracula"`
/// needs no file on disk. `cursor_bg` travels with the theme because the
/// harness reads the panel's cursor bar out of the cell colours and a theme
/// changes it - a detail worth stating, since a test that kept the `blue` value
/// would simply never find the cursor under `dracula`.
struct ThemeCase {
    /// `[ui] theme`.
    name: &'static str,
    /// `panel.cursor_bg`, for [`Session::cursor_row`].
    cursor_bg: (u8, u8, u8),
    /// `syn.keyword`.
    keyword: (u8, u8, u8),
    /// `syn.string`.
    string: (u8, u8, u8),
}

/// `themes/blue.toml`, verbatim.
const BLUE: ThemeCase = ThemeCase {
    name: "blue",
    cursor_bg: paint::CURSOR_FOCUSED,
    keyword: (0xFF, 0x54, 0xFF),
    string: (0x54, 0xFF, 0x54),
};

/// `themes/dracula.toml`: the same three slots, deliberately different
/// values.
const DRACULA: ThemeCase = ThemeCase {
    name: "dracula",
    cursor_bg: (0xBD, 0x93, 0xF9),
    keyword: (0xFF, 0x79, 0xC6),
    string: (0x50, 0xFA, 0x7B),
};

/// the design is v0.3's milestone and has its own harness; a live shell here
/// would draw its own prompt over the rows these criteria read.
const NO_CONSOLE: &str = "[console]\nenabled = false\n";

// ---------------------------------------------------------------------------
// The fixture directory
// ---------------------------------------------------------------------------

/// A directory built per test and removed on drop.
///
/// Every criterion is "open *this* file", so the tree is whatever the test puts
/// in it and nothing else: a panel holding exactly the files under test makes
/// `Down` a deterministic way to reach one, with no sort order to reason about
/// beyond "`..` first, then names ascending".
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-view-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    /// Write one file and hand back its path.
    fn file(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, bytes.as_ref()).expect("fixture file");
        path
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// How to start one `hcmd`.
struct Launch {
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    config: String,
    /// `panel.cursor_bg` for whatever theme this child will load, because the
    /// panel's cursor bar is a background and is read back out of the cells.
    cursor_bg: (u8, u8, u8),
}

impl Launch {
    fn new(cols: u16, rows: u16, cwd: impl Into<PathBuf>) -> Self {
        Self {
            cols,
            rows,
            cwd: cwd.into(),
            config: NO_CONSOLE.to_string(),
            cursor_bg: paint::CURSOR_FOCUSED,
        }
    }

    /// Append to the throwaway `config.toml` the child is given.
    fn config(mut self, toml: &str) -> Self {
        self.config.push_str(toml);
        self
    }

    /// Select one of the shipped themes, and tell the harness what
    /// its cursor bar looks like.
    fn theme(mut self, theme: &ThemeCase) -> Self {
        self.config
            .push_str(&format!("[ui]\ntheme = \"{}\"\n", theme.name));
        self.cursor_bg = theme.cursor_bg;
        self
    }
}

/// A running `hcmd` and the screen it has painted so far.
struct Session {
    master: Box<dyn MasterPty + Send>,
    cols: u16,
    rows: u16,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    home: PathBuf,
    cursor_bg: (u8, u8, u8),
    /// Every byte the child has written.
    ///
    /// Needed because `vt100` is not a faithful mirror in one place this
    /// milestone cares about: `perform.rs` routes `U+FFFD` to
    /// `Callbacks::unhandled_char` and never puts it in a cell, so the "invalid
    /// sequences render as a replacement glyph" is invisible on the parsed
    /// screen however well the product does it. The raw stream is where that
    /// claim can be checked.
    raw: Vec<u8>,
}

impl Session {
    fn start(launch: Launch) -> Self {
        let Launch {
            cols,
            rows,
            cwd,
            config,
            cursor_bg,
        } = launch;

        let home = std::env::temp_dir().join(format!(
            "hcmd-view-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("holoscommander")).expect("throwaway config dir");
        std::fs::write(home.join("holoscommander/config.toml"), config).expect("write config");

        let pair = native_pty_system()
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
        // Pin the locale so `ui.ascii_borders` resolves the same way on every
        // machine: criterion 7 reads `»`, the marker for a line that runs past
        // the right edge.
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        // a bare pty answers no capability query, and `Ctrl+G`
        // only exists as a distinguishable sequence under the Kitty protocol.
        cmd.env("HCMD_KEYBOARD_PROTOCOL", "enhanced");
        cmd.env("XDG_CONFIG_HOME", &home);
        cmd.env("XDG_STATE_HOME", &home);
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd).expect("spawn hcmd");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0_u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");

        Self {
            master: pair.master,
            cols,
            rows,
            child,
            writer,
            rx,
            parser: vt100::Parser::new(rows, cols, 0),
            home,
            cursor_bg,
            raw: Vec::new(),
        }
    }

    // -- reading -----------------------------------------------------------

    /// Feed whatever has arrived into the parser, for up to `budget`.
    fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => {
                    self.raw.extend_from_slice(&chunk);
                    self.parser.process(&chunk);
                }
                Err(_) => return,
            }
        }
    }

    /// What the screen currently reads, as plain text.
    fn text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Everything that distinguishes one frame from the next.
    fn snapshot(&self) -> (String, (u16, u16), bool) {
        let screen = self.parser.screen();
        (
            screen.contents(),
            screen.cursor_position(),
            screen.hide_cursor(),
        )
    }

    /// Poll until the rendered screen has been unchanged for a beat.
    fn settle(&mut self) {
        let mut last = self.snapshot();
        let mut stable_since = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(60));
            let now = self.snapshot();
            if now == last {
                if stable_since.elapsed() >= Duration::from_millis(250) {
                    return;
                }
            } else {
                last = now;
                stable_since = Instant::now();
            }
        }
    }

    /// Wait until the screen has settled *and* `pred` holds, or fail loudly.
    fn wait(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.settle();
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- screen ---\n{}",
                    self.dump()
                );
            }
            self.pump(Duration::from_millis(150));
        }
    }

    /// [`Session::wait`] against the screen's plain text.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        self.wait(what, |s| pred(&s.text()));
    }

    /// The panel listing has arrived and `needle` is in it.
    fn wait_for_listing(&mut self, needle: &str) {
        let needle = needle.to_string();
        self.wait_for("the fixture listing", move |t| {
            t.contains(&needle) && t.contains("Name")
        });
    }

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// Send a key and wait for the screen to reflect it.
    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&Self) -> bool) {
        self.send(bytes);
        self.wait(what, pred);
    }

    /// Send a key and wait until the focused panel's cursor bar has landed on a
    /// row containing `needle`.
    ///
    /// Cursor movement changes no text, so "the screen settled" is not enough
    /// on its own - the key may not have been processed yet and the next one
    /// would act on the wrong entry. This waits for the thing that moved.
    fn press_to(&mut self, bytes: &[u8], needle: &'static str) {
        self.send(bytes);
        self.wait(&format!("the cursor to reach {needle:?}"), move |s| {
            s.cursor_row().contains(needle)
        });
    }

    /// [`Session::wait`] without the settle.
    ///
    /// For the one file whose screen never settles: a 40 GB index scan
    /// repaints `indexing n%` continuously, so "unchanged for a beat" is never
    /// true and every `wait` would spend its whole timeout. It also matters
    /// straight after [`Session::press_timed`], which returns the instant its
    /// predicate holds - mid-frame, with the rows below the one it was watching
    /// still carrying the previous picture.
    fn wait_now(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump(Duration::from_millis(20));
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- screen ---\n{}",
                    self.dump()
                );
            }
        }
    }

    /// Send a key and return how long the child took to satisfy `pred`.
    ///
    /// Deliberately does **not** settle: criterion 9's file has a background
    /// index scan repainting the status line continuously, so "unchanged for a
    /// beat" never comes true - and a 250 ms settle would swamp the very
    /// measurement the criterion is about ("a 40 GB file must
    /// open as fast as a 4 KB one").
    fn press_timed(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&Self) -> bool) -> Duration {
        let started = Instant::now();
        self.send(bytes);
        let deadline = started + Duration::from_secs(120);
        loop {
            self.pump(Duration::from_millis(2));
            if pred(self) {
                return started.elapsed();
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- screen ---\n{}",
                    self.dump()
                );
            }
        }
    }

    // -- reading the painted cells ----------------------------------------

    /// One screen row, rebuilt from its cells.
    ///
    /// Not `contents().lines()`: that trims and can collapse, and every colour
    /// assertion here needs a string whose byte index *is* the column. Every
    /// character the viewer draws is ASCII or a one-cell glyph, so the two
    /// coincide.
    fn row_cells(&self, row: u16) -> String {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let mut out = String::with_capacity(usize::from(cols));
        for col in 0..cols {
            match screen.cell(row, col) {
                Some(cell) if !cell.contents().is_empty() => out.push_str(cell.contents()),
                _ => out.push(' '),
            }
        }
        out
    }

    /// Row 0: the viewer's title, which is the file's path.
    fn title_row(&self) -> String {
        self.row_cells(0)
    }

    /// The last row: the viewer's status line, or its find bar.
    ///
    fn status_row(&self) -> String {
        let (rows, _) = self.parser.screen().size();
        self.row_cells(rows.saturating_sub(1))
    }

    /// The rows between the title and the status line: the file itself.
    fn body(&self) -> Vec<String> {
        let (rows, _) = self.parser.screen().size();
        (1..rows.saturating_sub(1))
            .map(|r| self.row_cells(r))
            .collect()
    }

    /// The body as one string, for `contains`.
    fn body_text(&self) -> String {
        self.body().join("\n")
    }

    /// The index of the first body row containing `needle`.
    fn body_row_of(&self, needle: &str) -> Option<u16> {
        let (rows, _) = self.parser.screen().size();
        (1..rows.saturating_sub(1)).find(|r| self.row_cells(*r).contains(needle))
    }

    /// The foreground of one cell.
    fn fg(&self, row: u16, col: u16) -> Option<vt100::Color> {
        self.parser
            .screen()
            .cell(row, col)
            .map(vt100::Cell::fgcolor)
    }

    /// The foreground of the cell `needle` starts in, on the first body row
    /// that holds it.
    fn fg_of(&self, needle: &str) -> Option<vt100::Color> {
        let row = self.body_row_of(needle)?;
        let col = u16::try_from(self.row_cells(row).find(needle)?).ok()?;
        self.fg(row, col)
    }

    /// Every body row carrying at least one cell with background `bg`, as
    /// `(row, the text of the cells that carry it)`.
    ///
    /// A quick-find match is painted as a background (the design's
    /// `viewer.match` / `viewer.current_match`), so this is how a match is seen
    /// from outside at all.
    fn rows_with_bg(&self, bg: (u8, u8, u8)) -> Vec<(u16, String)> {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let want = rgb(bg);
        let mut out = Vec::new();
        for row in 1..rows.saturating_sub(1) {
            let mut text = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col)
                    && cell.bgcolor() == want
                {
                    text.push_str(cell.contents());
                }
            }
            if !text.is_empty() {
                out.push((row, text));
            }
        }
        out
    }

    /// The one body row carrying the current-match highlight, if any.
    fn current_match_row(&self) -> Option<(u16, String)> {
        self.rows_with_bg(paint::CURRENT_MATCH).into_iter().next()
    }

    /// The row index of the key bar: the last row carrying `F10`.
    fn keybar_row(&self) -> u16 {
        let (rows, _) = self.parser.screen().size();
        let last = rows.saturating_sub(1);
        if self.row_cells(last).contains("F10") {
            last
        } else {
            rows
        }
    }

    /// The panel entry the focused cursor bar is sitting on.
    ///
    /// Only the rows above the key bar are searched: the blue theme paints
    /// `keybar.label_bg` in the same `#00A8A8` as `panel.cursor_bg`, so a scan
    /// of the whole screen would report the key bar as a cursor bar.
    fn cursor_row(&self) -> String {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let want = rgb(self.cursor_bg);
        for row in 0..self.keybar_row().saturating_sub(1) {
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
                return text.trim().to_string();
            }
        }
        String::new()
    }

    /// True once the viewer is holding the screen over `name`.
    ///
    /// The title row is the file's path and the panel's own top border is the
    /// *directory's*, so the file's name appearing on row 0 is exactly the
    /// transition - and the column header disappearing is the other half of it.
    fn viewing(&self, name: &str) -> bool {
        self.title_row().contains(name) && !self.text().contains("Name")
    }

    /// Resize the pty the way a window manager does, and the parser with it.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.settle();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        self.settle();
    }

    /// Make the parsed screen agree with what the product actually drew.
    ///
    /// Needed exactly once, and only because of the `vt100` limitation
    /// documented on [`Session::raw`]: a dropped `U+FFFD` leaves the parser's
    /// grid one column out of step with ratatui's buffer for that row, and
    /// ratatui only re-sends the cells it believes changed - so every later
    /// frame is diffed against a screen the parser no longer mirrors. A resize
    /// is what breaks that: ratatui's `Terminal::resize` clears the viewport
    /// and repaints every cell, so the two agree again. Out and back, so the
    /// geometry the assertions were written against is what they get.
    fn resync(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        self.resize(cols.saturating_add(1), rows);
        self.resize(cols, rows);
    }

    /// Everything the child has written, since it was written.
    fn raw_since(&self, at: usize) -> String {
        String::from_utf8_lossy(self.raw.get(at..).unwrap_or_default()).into_owned()
    }

    /// The whole screen, for a failure message.
    fn dump(&self) -> String {
        let (rows, _) = self.parser.screen().size();
        (0..rows)
            .map(|r| self.row_cells(r).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> vt100::Color {
    vt100::Color::Rgb(r, g, b)
}

/// Start a session in `tree`, wait for the listing, walk the cursor down to
/// `stem` and open it with `F3`.
///
/// One `Down` per entry, checked as it goes, because the panel's sort is `..`
/// first and then names ascending - so the walk is deterministic and a wrong
/// step fails on the step rather than three assertions later.
///
/// **A panel row is not a file name.** the design gives the extension its own
/// column, so `notes.txt` is drawn as `notes` … `txt` and no row on the screen
/// ever contains the string `notes.txt`. The walk is therefore by *stem*, and
/// the whole name is what the viewer's title row is checked against - that row
/// is the file's path, where the name is whole.
fn open(session: &mut Session, before: &[&'static str], stem: &'static str, name: &'static str) {
    session.wait_for_listing(stem);
    for entry in before {
        session.press_to(keys::DOWN, entry);
    }
    session.press_to(keys::DOWN, stem);
    session.press(keys::F3, "the viewer to open", move |s| s.viewing(name));
}

// ---------------------------------------------------------------------------
// 1. F3 opens the file at its first lines; Esc gives the panel back unmoved
// ---------------------------------------------------------------------------

#[test]
fn criterion_1_f3_opens_a_text_file_and_esc_returns_the_cursor_where_it_was() {
    let tree = Tree::new("c1");
    let body: String = (1..=80)
        .map(|n| format!("line {n:03} of the sample file\n"))
        .collect();
    tree.file("alpha.txt", "unrelated\n");
    tree.file("notes.txt", &body);
    tree.file("zeta.txt", "unrelated\n");

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &["alpha"], "notes", "notes.txt");

    // the viewer takes the whole screen, so the panels are gone.
    assert!(
        !s.text().contains("alpha") && !s.text().contains("zeta"),
        "the viewer takes the whole screen:\n{}",
        s.dump()
    );
    assert!(
        s.title_row().contains("notes.txt"),
        "the title row names the file that was opened:\n{}",
        s.dump()
    );

    // **Its first lines, in order, starting at the first.** A viewer that
    // opened somewhere else, or that dropped a line, would pass a bare
    // `contains`; this does not.
    let body_rows = s.body();
    for (i, row) in body_rows.iter().enumerate().take(20) {
        let want = format!("line {:03} of the sample file", i.saturating_add(1));
        assert!(
            row.contains(&want),
            "body row {i} should hold {want:?}, got {row:?}:\n{}",
            s.dump()
        );
    }
    // The line-number gutter numbers them from 1.
    assert!(
        body_rows
            .first()
            .is_some_and(|r| r.trim_start().starts_with('1')),
        "the first row is line 1 in the gutter, got {:?}",
        body_rows.first()
    );
    // the status line: the offset under the cursor, in both bases.
    let status = s.status_row();
    assert!(
        status.contains("0x0 (0)") && status.contains("text") && status.contains("line 1/"),
        "the status line reports the position and the mode, got {status:?}"
    );

    // Esc closes the viewer and the panel comes back with the cursor where it
    // was left.
    s.press(keys::ESC, "the panel to come back", |s| {
        s.text().contains("Name") && s.text().contains("zeta")
    });
    assert!(
        s.cursor_row().contains("notes"),
        "Esc returns to the panel with the cursor on the file that was viewed, \
         got {:?}:\n{}",
        s.cursor_row(),
        s.dump()
    );
    assert!(s.is_running(), "the binary is still alive");
}

// ---------------------------------------------------------------------------
// 2. A Rust file is highlighted - in the *theme's* colours
// ---------------------------------------------------------------------------

/// "Map syntect styles onto theme slots, not onto syntect's own
/// themes, so the blue theme controls highlighting."
///
/// So the same file is opened under two themes and the same two cells are read.
/// A keyword that is `#FF54FF` under `blue` and `#FF79C6` under `dracula` is
/// being coloured by the `syn.keyword`; a keyword that is the same colour under
/// both is being coloured by syntect, which is the thing the design rules out.
#[test]
fn criterion_2_a_rust_file_is_highlighted_in_the_themes_own_syn_slots() {
    let tree = Tree::new("c2");
    tree.file(
        "sample.rs",
        "fn main() {\n    let greeting = \"spectacle\";\n    println!(\"{greeting}\");\n}\n",
    );

    for case in [&BLUE, &DRACULA] {
        let ThemeCase {
            name: theme,
            keyword,
            string,
            ..
        } = *case;
        let mut s = Session::start(Launch::new(120, 30, tree.path()).theme(case));
        open(&mut s, &[], "sample", "sample.rs");

        assert!(
            s.body_text().contains("let greeting = \"spectacle\";"),
            "{theme}: the file is on screen:\n{}",
            s.dump()
        );

        // `let` is `storage.type.rust` / `keyword.declaration.*`, both of which
        // SELECTORS maps to `SynSlot::Keyword` (the design's
        // "colouring `storage.type` as a type paints every `let` the colour of
        // `Vec`").
        assert_eq!(
            s.fg_of("let greeting"),
            Some(rgb(keyword)),
            "{theme}: `let` carries syn.keyword ({keyword:?}), not syntect's own \
:\n{}",
            s.dump()
        );
        // The string's *body*. Its quotes are
        // `punctuation.definition.string`, which the mapping deliberately
        // abstains on so they take the colour of the string they open - asked
        // about separately below.
        assert_eq!(
            s.fg_of("spectacle"),
            Some(rgb(string)),
            "{theme}: a string carries syn.string ({string:?}):\n{}",
            s.dump()
        );
        assert_eq!(
            s.fg_of("\"spectacle"),
            Some(rgb(string)),
            "{theme}: the opening quote is part of the string, not punctuation:\n{}",
            s.dump()
        );
        assert_eq!(
            s.fg_of("fn main"),
            Some(rgb(keyword)),
            "{theme}: `fn` carries syn.keyword too:\n{}",
            s.dump()
        );
        // And the ordinary body of the file is *not* coloured: `source.rust`
        // carries no slot, so `viewer.fg` shows through and the screen is not a
        // wall of colour.
        assert_ne!(
            s.fg_of("greeting = "),
            Some(rgb(keyword)),
            "{theme}: a plain identifier is not a keyword:\n{}",
            s.dump()
        );
    }
}

// ---------------------------------------------------------------------------
// 3. `2` is hex and `1` is back
// ---------------------------------------------------------------------------

#[test]
fn criterion_3_two_switches_to_hex_with_offsets_bytes_and_an_ascii_gutter() {
    let tree = Tree::new("c3");
    // 576 bytes of known, printable content, so every column of a hex row has a
    // checkable answer - and more rows than the terminal has, so `Down` has
    // somewhere to go.
    let body = "Hello, hex world!\n".repeat(32);
    assert_eq!(body.len(), 576);
    tree.file("data.txt", &body);

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &[], "data", "data.txt");
    assert!(
        s.status_row().contains("text"),
        "a text file opens in text mode, got {:?}",
        s.status_row()
    );

    s.press(keys::TWO, "hex mode", |s| s.status_row().contains("hex"));

    let first = s
        .body()
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("no body:\n{}", s.dump()));
    // an offset column, `viewer.hex_width` bytes per row, and an
    // ASCII gutter - with the offset shown "in both hex and decimal".
    assert!(
        first.starts_with("00000000"),
        "the row starts with its offset in hex, got {first:?}"
    );
    assert!(
        first.contains("(  0)"),
        "and the same offset in decimal, got {first:?}"
    );
    // Lowercase digits in the byte column and uppercase in the offset column
    // is `hex::nibble` against `{offset:0width$X}` - deliberate in the product,
    // and read here exactly as it is drawn rather than as it might have been.
    assert!(
        first.contains("48 65 6c 6c 6f 2c 20 68 65 78 20 77 6f 72 6c 64"),
        "sixteen bytes of `Hello, hex world`, got {first:?}"
    );
    assert!(
        first.trim_end().ends_with("Hello, hex world"),
        "and the ASCII gutter beside them, got {first:?}"
    );
    // The second row proves `hex_width` really is the stride rather than a
    // coincidence of the first row.
    let second = s.body().get(1).cloned().unwrap_or_default();
    assert!(
        second.starts_with("00000010") && second.contains("( 16)"),
        "the next row is sixteen bytes on, got {second:?}"
    );

    // The three columns are three colours, which is what makes
    // the gutter a gutter rather than more bytes.
    let offsets = s.rows_with_bg(paint::HEX_OFFSET);
    assert!(
        offsets.is_empty(),
        "the offset column is a foreground, not a background"
    );
    assert_eq!(
        s.fg(1, 0),
        Some(rgb(paint::HEX_OFFSET)),
        "the offset column is painted viewer.hex_offset:\n{}",
        s.dump()
    );
    let gutter_col = u16::try_from(
        first
            .rfind("Hello, hex world")
            .unwrap_or_else(|| panic!("no ascii gutter in {first:?}")),
    )
    .expect("column fits");
    assert_eq!(
        s.fg(1, gutter_col),
        Some(rgb(paint::HEX_ASCII)),
        "the ASCII gutter is painted viewer.hex_ascii:\n{}",
        s.dump()
    );

    // "the current offset under the cursor is in the status
    // line". `Right` moves the cursor a byte at a time in hex, so the status
    // line is the only place that can show it.
    assert!(
        s.status_row().contains("0x0 (0)"),
        "the cursor starts at 0, got {:?}",
        s.status_row()
    );
    s.press(keys::RIGHT, "the cursor to step one byte", |s| {
        s.status_row().contains("0x1 (1)")
    });
    // A vertical move is a row move and it **carries the column** with it.
    // v0.4 took the cursor to the row's first byte, because `scroll` ended with
    // `cursor = top`; the rectangular selection makes that no
    // longer tenable - `Shift+Down` has to take a column with it or a column
    // block cannot be made at all.
    s.press(keys::DOWN, "the cursor to step one row", |s| {
        s.status_row().contains("0x11 (17)")
    });
    s.press(keys::RIGHT, "the cursor inside the second row", |s| {
        s.status_row().contains("0x12 (18)")
    });

    // `1` switches back.
    s.press(keys::ONE, "text mode", |s| s.status_row().contains("text"));
    assert!(
        s.body_text().contains("Hello, hex world!"),
        "text mode shows the file as text again:\n{}",
        s.dump()
    );
    assert!(
        !s.body_text().contains("48 65 6c"),
        "and no longer as bytes:\n{}",
        s.dump()
    );
}

// ---------------------------------------------------------------------------
// 4. A binary file opens in hex on its own
// ---------------------------------------------------------------------------

#[test]
fn criterion_4_a_binary_file_opens_in_hex_without_being_asked() {
    let tree = Tree::new("c4");
    // NUL bytes and a dense run of control characters: the "a file
    // detected as binary", by either half of the rule.
    let mut blob: Vec<u8> = Vec::new();
    for i in 0..512_u32 {
        blob.push(u8::try_from(i % 256).unwrap_or(0));
    }
    tree.file("blob.bin", &blob);
    // A text file beside it, so the assertion is about *this* file rather than
    // about the viewer only ever opening in hex.
    tree.file("plain.txt", "just words, nothing binary here\n");

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &[], "blob", "blob.bin");

    let status = s.status_row();
    assert!(
        status.contains("hex"),
        "a binary file opens in hex automatically, got {status:?}:\n{}",
        s.dump()
    );
    assert!(
        status.contains("binary"),
        "and the status line says why, got {status:?}"
    );
    let first = s.body().first().cloned().unwrap_or_default();
    assert!(
        first.starts_with("00000000") && first.contains("00 01 02 03 04 05 06 07"),
        "the bytes are there, got {first:?}"
    );

    // The other half: the text file beside it opens in text. Without this, a
    // viewer that always opened in hex would pass.
    s.press(keys::ESC, "the panel to come back", |s| {
        s.text().contains("Name")
    });
    s.press_to(keys::DOWN, "plain");
    s.press(keys::F3, "the viewer over plain.txt", |s| {
        s.viewing("plain.txt")
    });
    assert!(
        s.status_row().contains("text") && !s.status_row().contains("binary"),
        "a text file still opens in text, got {:?}:\n{}",
        s.status_row(),
        s.dump()
    );
    assert!(
        s.body_text().contains("just words, nothing binary here"),
        "and reads as text:\n{}",
        s.dump()
    );
}

// ---------------------------------------------------------------------------
// 5. F7 finds, incrementally; n and Shift+N step; Esc keeps the position
// ---------------------------------------------------------------------------

#[test]
fn criterion_5_f7_finds_as_you_type_and_n_steps_through_the_matches() {
    let tree = Tree::new("c5");
    // The matches are deliberately far down and far apart: the first is past
    // the first screenful, so "the viewer moved to it" is observable, and the
    // second is more than a screen below the first, so `n` has to scroll.
    let body: String = (1..=200)
        .map(|n| match n {
            100 | 140 | 180 => format!("line {n:03} needle here\n"),
            _ => format!("line {n:03} filler\n"),
        })
        .collect();
    tree.file("haystack.txt", &body);

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &[], "haystack", "haystack.txt");
    assert!(
        s.body_text().contains("line 001 filler") && !s.body_text().contains("line 100"),
        "the file opens at its start:\n{}",
        s.dump()
    );

    // "typing searches immediately, incrementally, with the
    // first match highlighted as you type". A *partial* pattern, so this is
    // about the typing and not about pressing Enter.
    s.press(keys::F7, "the find bar", |s| {
        s.status_row().starts_with("find:")
    });
    s.send(b"need");
    s.wait("the first match while still typing", |s| {
        s.body_text().contains("line 100 needle here") && s.current_match_row().is_some()
    });
    let (row, painted) = s
        .current_match_row()
        .unwrap_or_else(|| panic!("no current match:\n{}", s.dump()));
    assert_eq!(
        painted, "need",
        "exactly what has been typed is highlighted, not the whole word"
    );
    assert!(
        s.row_cells(row).contains("line 100 needle here"),
        "and it is the *first* match in the file, got row {row}:\n{}",
        s.dump()
    );

    // Finishing the word keeps it the current match and extends the highlight.
    s.send(b"le");
    s.wait("the completed pattern", |s| {
        s.status_row().contains("find: needle")
            && s.current_match_row().is_some_and(|(_, t)| t == "needle")
    });
    // The matches that are not current are painted too, in the other slot -
    // and on this screen the next one is 40 lines away, so there is exactly one
    // highlight of each kind.
    assert!(
        s.rows_with_bg(paint::MATCH).is_empty(),
        "only the current match is on screen at 100:\n{}",
        s.dump()
    );

    // "`Esc` closes the bar and keeps position."
    let before = s.body();
    s.press(keys::ESC, "the find bar to close", |s| {
        !s.status_row().starts_with("find:")
    });
    assert_eq!(
        s.body(),
        before,
        "Esc keeps the position - it does not go back to where the search \
         started:\n{}",
        s.dump()
    );
    assert!(
        s.body_text().contains("line 100 needle here"),
        "the match is still on screen:\n{}",
        s.dump()
    );
    assert!(
        s.current_match_row().is_some(),
        "and still highlighted (the `keeps position`):\n{}",
        s.dump()
    );
    assert!(
        s.status_row().contains("0x") && s.status_row().contains("line "),
        "the status line is back to the numbers, got {:?}",
        s.status_row()
    );

    // `n` steps forward - to the match 40 lines below, which is off this
    // screen, so the viewer has to move.
    s.press(keys::N, "the next match", |s| {
        s.body_text().contains("line 140 needle here")
    });
    assert!(
        s.current_match_row()
            .is_some_and(|(r, t)| t == "needle" && s.row_cells(r).contains("line 140")),
        "`n` makes the next match the current one:\n{}",
        s.dump()
    );
    assert!(
        !s.body_text().contains("line 100 needle"),
        "and the old one is behind us:\n{}",
        s.dump()
    );

    // `Shift+N` steps back to it.
    s.press(keys::SHIFT_N, "the previous match", |s| {
        s.body_text().contains("line 100 needle here")
    });
    assert!(
        s.current_match_row()
            .is_some_and(|(r, t)| t == "needle" && s.row_cells(r).contains("line 100")),
        "`Shift+N` steps back:\n{}",
        s.dump()
    );
}

// ---------------------------------------------------------------------------
// 6. Ctrl+G in hex mode, in either base, and refused past the end
// ---------------------------------------------------------------------------

#[test]
fn criterion_6_ctrl_g_jumps_to_an_offset_and_refuses_one_past_the_end() {
    let tree = Tree::new("c6");
    // 4 KiB, mostly NUL - so it opens in hex on its own - with a legible marker
    // at exactly 0x400, which is what makes "it really went there" an assertion
    // about the file rather than about the status line. Well past the first
    // screenful, so it is not visible until something jumps to it.
    let mut blob = vec![0_u8; 4096];
    blob[0x400..0x410].copy_from_slice(b"MARKER-AT-0x400!");
    tree.file("offsets.bin", &blob);

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &[], "offsets", "offsets.bin");
    assert!(
        s.status_row().contains("hex"),
        "the file opens in hex, got {:?}",
        s.status_row()
    );
    assert!(
        !s.body_text().contains("MARKER-AT-0x400!"),
        "and the marker is not on the first screen yet:\n{}",
        s.dump()
    );

    // Decimal first ("accepting `0x` notation" - so plain
    // decimal has to work as well, or the sentence says nothing).
    s.press(keys::CTRL_G, "the go-to-offset prompt", |s| {
        s.text().contains("Go to offset")
    });
    s.send(b"1024");
    s.press(keys::ENTER, "the jump to 1024", |s| {
        s.status_row().contains("0x400 (1024)")
    });
    assert!(
        s.body_text().contains("MARKER-AT-0x400!"),
        "the viewer really moved to byte 1024:\n{}",
        s.dump()
    );
    assert!(
        s.body()
            .first()
            .is_some_and(|r| r.starts_with("00000400") && r.contains("(1024)")),
        "and 0x400 is the top row, got {:?}",
        s.body().first()
    );

    // Then `0x` notation, from somewhere else, so arriving at the same place is
    // the answer to the question rather than the position not having changed.
    s.press(keys::CTRL_HOME, "the top of the file", |s| {
        s.status_row().contains("0x0 (0)")
    });
    s.press(keys::CTRL_G, "the prompt again", |s| {
        s.text().contains("Go to offset")
    });
    s.send(b"0x400");
    s.press(keys::ENTER, "the jump to 0x400", |s| {
        s.status_row().contains("0x400 (1024)")
    });
    assert!(
        s.body_text().contains("MARKER-AT-0x400!"),
        "`0x400` is the same byte as `1024`:\n{}",
        s.dump()
    );

    // Past the end is **refused, with the size** - not clamped. A silent clamp
    // is a wrong answer that looks like a right one.
    s.press(keys::CTRL_G, "the prompt a third time", |s| {
        s.text().contains("Go to offset")
    });
    s.send(b"0x2000");
    s.press(keys::ENTER, "the refusal", |s| {
        s.status_row().contains("past the end")
    });
    let status = s.status_row();
    assert!(
        status.contains("0x2000 (8192)"),
        "the refusal names what was asked for, got {status:?}"
    );
    assert!(
        status.contains("4096") && status.contains("0x1000"),
        "and the file's size, in both bases, got {status:?}"
    );
    // And the position did not move.
    assert!(
        s.body_text().contains("MARKER-AT-0x400!"),
        "a refused jump leaves the viewer where it was:\n{}",
        s.dump()
    );
}

// ---------------------------------------------------------------------------
// 7. w wraps, and the top of the window stays put
// ---------------------------------------------------------------------------

#[test]
fn criterion_7_w_toggles_wrap_without_moving_the_top_of_the_window() {
    let tree = Tree::new("c7");
    // A line three times the terminal's width, deliberately *not* the first
    // line in the file: the top of the window is only checkably unmoved if it
    // is somewhere other than the top of the file.
    let long: String = std::iter::repeat_n("0123456789", 30).collect();
    assert_eq!(long.len(), 300);
    let mut body = String::new();
    body.push_str("first line\nsecond line\nthird line\n");
    body.push_str("LONG-");
    body.push_str(&long);
    body.push('\n');
    for n in 5..=60 {
        body.push_str(&format!("tail {n:02}\n"));
    }
    tree.file("wide.txt", &body);

    let mut s = Session::start(Launch::new(120, 30, tree.path()));
    open(&mut s, &[], "wide", "wide.txt");
    assert!(
        !s.status_row().contains("wrap"),
        "wrap is off to start with (viewer.wrap defaults false), got {:?}",
        s.status_row()
    );

    // Scroll the long line to the top of the window. the design gives the
    // arrows to the **cursor** - "the view follows when it reaches an edge" -
    // so the window moves a row at a time only once the cursor has reached the
    // bottom of it, and how many presses that takes is a fact about the
    // terminal rather than about the file. Press until it has happened.
    //
    for _ in 0..200 {
        if s.body().first().is_some_and(|r| r.contains("LONG-01234")) {
            break;
        }
        s.send(keys::DOWN);
        s.pump(Duration::from_millis(5));
    }
    s.wait("the long line at the top of the window", |s| {
        s.body().first().is_some_and(|r| r.contains("LONG-01234"))
    });
    let top_status = s.status_row();
    assert!(
        top_status.contains("line 4/"),
        "the top of the window is line 4, got {top_status:?}"
    );

    // With wrap off the line is cut at the right edge and says so - the
    // "optional wrap", and the `»` is the marker for "this line continues"
    // (text::MORE_MARK).
    let unwrapped = s.body();
    let first = unwrapped.first().cloned().unwrap_or_default();
    assert!(
        first.trim_end().ends_with('\u{00bb}'),
        "with wrap off the line runs past the right edge and is marked, \
         got {first:?}"
    );
    assert!(
        unwrapped.get(1).is_some_and(|r| r.contains("tail 05")),
        "and the next row is the next *line*, got {:?}",
        unwrapped.get(1)
    );
    // Everything that fits is one screen row of a 300-character line, so a
    // whole run of it is missing from the screen.
    assert!(
        !s.body_text().contains("0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789"),
        "the tail of the line is off screen with wrap off:\n{}",
        s.dump()
    );

    // `w`: F2 reloads in the viewer, it does not wrap.
    s.press(keys::WRAP, "wrap to come on", |s| {
        s.status_row().contains("wrap")
    });

    let wrapped = s.body();
    // **The top of the window did not move.**
    assert!(
        wrapped.first().is_some_and(|r| r.contains("LONG-01234")),
        "the top row is still the same line, got {:?}:\n{}",
        wrapped.first(),
        s.dump()
    );
    let after = s.status_row();
    assert!(
        after.contains("line 4/"),
        "and the status line still says line 4, got {after:?}"
    );
    assert_eq!(
        after
            .split("  ")
            .find(|f| f.starts_with("0x"))
            .unwrap_or_default(),
        top_status
            .split("  ")
            .find(|f| f.starts_with("0x"))
            .unwrap_or_default(),
        "the byte offset at the top of the window is unchanged"
    );

    // And the line really is wrapped: no `»`, the continuation rows carry no
    // line number (the design numbers a line once, not a row), and the tail
    // that was off screen is now on it.
    assert!(
        !wrapped.iter().any(|r| r.contains('\u{00bb}')),
        "nothing is cut at the right edge any more:\n{}",
        s.dump()
    );
    assert!(
        wrapped
            .get(1)
            .is_some_and(|r| r.contains("6789") && r.chars().take(5).all(char::is_whitespace)),
        "the continuation row carries the text and no line number, got {:?}",
        wrapped.get(1)
    );
    // The whole line, reassembled from its rows: the gutter is five columns
    // (four digits and a separator), so what is past it is the file's own
    // bytes, and three rows of that concatenated must be the line exactly.
    // Character-counting would not do - a wrapped row can split a repeat - and
    // this is the assertion that a row is dropped or duplicated by.
    let rejoined: String = wrapped
        .iter()
        .take(3)
        .map(|r| r.get(5..).unwrap_or("").trim_end())
        .collect();
    assert_eq!(
        rejoined,
        format!("LONG-{long}"),
        "the wrapped rows are the whole line and nothing else:\n{}",
        s.dump()
    );

    // `F2` is the viewer's *reload*, not its wrap toggle. It
    // used to be the wrap key, and pressing it here proves the rebinding took:
    // a reload of an unchanged file leaves the wrapped rows exactly as they
    // are, where the old binding would unwrap them.
    s.press(keys::F2, "the reload to finish", |s| {
        s.body().first().is_some_and(|r| !r.trim().is_empty())
    });
    assert_eq!(
        s.body()
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        wrapped
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "F2 reloads and does not toggle wrap:\n{}",
        s.dump()
    );

    // `w` again puts it back, still without moving.
    s.press(keys::WRAP, "wrap to go off again", |s| {
        !s.status_row().contains("wrap")
    });
    assert_eq!(
        s.body(),
        unwrapped,
        "`w` is a toggle and the window is where it was:\n{}",
        s.dump()
    );
}

// ---------------------------------------------------------------------------
// 8. F8 cycles the encoding, and one press makes a latin-1 file readable
// ---------------------------------------------------------------------------

#[test]
fn criterion_8_f8_cycles_the_encoding_and_names_the_active_one() {
    let tree = Tree::new("c8");
    // Latin-1 bytes: `é` is 0xE9 and `è` is 0xE8, neither of which is valid
    // UTF-8 on its own.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"Caf");
    bytes.push(0xE9);
    bytes.extend_from_slice(b" de la Cr");
    bytes.push(0xE8);
    bytes.extend_from_slice(b"me\nune autre ligne\n");
    tree.file("latin1.txt", &bytes);

    // Pinned to UTF-8 rather than left on `auto`, because `auto` sniffs with
    // chardetng and would get this right - and the criterion is about the file
    // that was got *wrong* being one keystroke from readable.
    let mut s = Session::start(
        Launch::new(120, 30, tree.path()).config("[viewer.encoding]\ndefault = \"utf-8\"\n"),
    );
    open(&mut s, &[], "latin1", "latin1.txt");

    let status = s.status_row();
    assert!(
        status.contains("UTF-8 [config]"),
        "the status line names the active encoding and how it was chosen \
, got {status:?}"
    );
    assert!(
        status.contains("invalid bytes"),
        "and says the bytes did not decode, got {status:?}"
    );
    assert!(
        !s.body_text().contains("Café de la Crème"),
        "mis-decoded as UTF-8, the file does not read:\n{}",
        s.dump()
    );
    // What it renders *instead* is the "invalid sequences render as
    // a replacement glyph rather than failing the open" - asked of the byte
    // stream rather than of the parsed screen, because `vt100` drops `U+FFFD`
    // before it reaches a cell (see `Session::raw`).
    assert!(
        s.raw_since(0).contains('\u{fffd}'),
        "the two bad bytes are drawn as replacement glyphs, not dropped and \
         not fatal"
    );

    // **One press.**
    let before_f8 = s.raw.len();
    s.press(keys::F8, "the next encoding", |s| {
        s.status_row().contains("windows-1252 [chosen]")
    });
    assert!(
        !s.raw_since(before_f8).contains('\u{fffd}'),
        "and nothing is drawn as a replacement glyph once the encoding is right"
    );
    s.resync();
    assert!(
        s.body_text().contains("Café de la Crème"),
        "one F8 makes a mis-detected file readable:\n{}",
        s.dump()
    );
    assert!(
        !s.status_row().contains("invalid bytes"),
        "and nothing is invalid any more, got {:?}",
        s.status_row()
    );

    // The rest of the shortlist, in order, and back round to the
    // start: UTF-8, the detected one, windows-1252, cp437, utf-16le - with the
    // detected one de-duplicated because it *is* UTF-8 here.
    for want in ["cp437 [chosen]", "UTF-16LE [chosen]", "UTF-8 [chosen]"] {
        s.press(keys::F8, want, move |s| s.status_row().contains(want));
    }
    s.resync();
    assert!(
        !s.body_text().contains("Café de la Crème") && s.status_row().contains("invalid bytes"),
        "a full cycle is back where it started:\n{}",
        s.dump()
    );
    assert!(s.is_running(), "no encoding in the ring killed the binary");
}

// ---------------------------------------------------------------------------
// 9. A 40 GB file opens as fast as a 4 KB one
// ---------------------------------------------------------------------------

/// the headline requirement of this milestone:
///
/// > Memory is capped and is a function of the window size, not the file size.
/// > **A 40 GB file must open as fast as a 4 KB one.**
///
/// Measured through the pty, which is the only place an implementation that
/// reads the file to find its lines cannot hide: such a viewer would spend
/// minutes here whatever it did on a small file.
///
/// The 4 KB file is opened first, in the same session, as the control. Both are
/// binary so both open in hex - otherwise the small one would
/// load the syntax set and the large one would not, and the comparison would be
/// measuring syntect.
#[test]
fn criterion_9_a_forty_gigabyte_file_opens_in_well_under_a_second() {
    const HUGE: u64 = 40 * 1024 * 1024 * 1024;
    /// Above this many allocated bytes the file is not sparse and the test
    /// cannot run without filling the disk.
    const SPARSE_LIMIT: u64 = 64 * 1024 * 1024;

    let tree = Tree::new("c9");
    let small = tree.file("aaa-small.bin", vec![0_u8; 4096]);
    assert!(small.exists());

    // `truncate(2)`, which is what `truncate -s 40G` calls: a file with a hole
    // in it, so nothing is written and nothing is read back but zeroes.
    let path = tree.path().join("zzz-huge.bin");
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!(
                "SKIPPED criterion 9: cannot create {}: {err}",
                path.display()
            );
            return;
        }
    };
    if let Err(err) = file.set_len(HUGE) {
        eprintln!(
            "SKIPPED criterion 9: {} cannot hold a 40 GB sparse file \
             (ftruncate: {err}) - this filesystem does not do sparse files",
            tree.path().display()
        );
        return;
    }
    drop(file);
    let allocated = match std::fs::metadata(&path) {
        Ok(m) => m.blocks().saturating_mul(512),
        Err(err) => {
            eprintln!("SKIPPED criterion 9: cannot stat {}: {err}", path.display());
            return;
        }
    };
    if allocated > SPARSE_LIMIT {
        let _ = std::fs::remove_file(&path);
        eprintln!(
            "SKIPPED criterion 9: {} materialised {allocated} bytes for a 40 GB \
             hole - this filesystem does not do sparse files",
            tree.path().display()
        );
        return;
    }

    // Wide, so the status line can carry the size as well as the position
    // without the rank-ordered dropping taking it away.
    let mut s = Session::start(Launch::new(160, 40, tree.path()));
    s.wait_for_listing("zzz-huge");

    s.press_to(keys::DOWN, "aaa-small");
    let small_open = s.press_timed(keys::F3, "the 4 KB file to render", |s| {
        s.title_row().contains("aaa-small.bin") && s.body_text().contains("00 00 00 00 00 00 00 00")
    });
    s.press(keys::ESC, "the panel to come back", |s| {
        s.text().contains("Name")
    });

    s.press_to(keys::DOWN, "zzz-huge");
    let huge_open = s.press_timed(keys::F3, "the 40 GB file to render", |s| {
        s.title_row().contains("zzz-huge.bin") && s.body_text().contains("00 00 00 00 00 00 00 00")
    });

    // Reported, not just asserted: the numbers are the point of the criterion
    // and are worth reading when it is run by hand.
    eprintln!("criterion 9: 4 KB opened in {small_open:?}, 40 GB in {huge_open:?}");
    // The assertion the milestone is about.
    assert!(
        huge_open < Duration::from_secs(1),
        "a 40 GB file must open as fast as a 4 KB one. \
         4 KB took {small_open:?}, 40 GB took {huge_open:?}"
    );
    // And the two are of a kind, which is the sentence's actual claim: nothing
    // on the open path may be a function of the size.
    assert!(
        huge_open < small_open + Duration::from_millis(500),
        "opening 40 GB ({huge_open:?}) is not `as fast as` opening 4 KB \
         ({small_open:?}) - something on the open path scales with the file"
    );

    // It rendered, and it rendered the *right* file. The whole frame is waited
    // for here rather than settled for: the background index scan is walking
    // 40 GB and repainting its progress, so the screen is never still.
    s.wait_now("the whole first frame of a 40 GB file", |s| {
        s.status_row().contains("of 42949672960")
    });
    let first = s.body().first().cloned().unwrap_or_default();
    assert!(
        first.starts_with("000000000  ("),
        "the first hex row of a 40 GB file needs nine offset digits, got {first:?}:\n{}",
        s.dump()
    );
    assert!(
        first.contains("................"),
        "and an ASCII gutter of sixteen holes, got {first:?}"
    );
    assert!(
        s.status_row().contains("hex") && s.status_row().contains("binary"),
        "a hole is binary and opens in hex, got {:?}",
        s.status_row()
    );

    // Navigating a 40 GB file is bounded too: `End` is a seek, not a scan.
    // the `Ctrl+End` is the **last byte**, which with a cursor is
    // where the cursor lands rather than the last row's first byte.
    //
    let to_end = s.press_timed(keys::CTRL_END, "the end of a 40 GB file", |s| {
        s.status_row().contains("(42949672959)")
    });
    assert!(
        to_end < Duration::from_secs(2),
        "`End` on a 40 GB file is a seek, not a scan; took {to_end:?}"
    );
    s.wait_now("the last rows of a 40 GB file", |s| {
        s.body().last().is_some_and(|r| r.starts_with("9FFFFFFF0"))
    });
    assert!(s.is_running(), "the binary survived a 40 GB file");
}

// ---------------------------------------------------------------------------
// 10. The three shapes that break a pager
// ---------------------------------------------------------------------------

/// An empty file, a file with no trailing newline, and a file that is one
/// 10 MB line: the three shapes the streaming model has to answer
/// without a special case, and the three that a line-based viewer built on
/// `read_to_string` and `split('\n')` gets wrong - off the end of an empty
/// `Vec`, a phantom last line, and a single allocation the size of the file.
#[test]
fn criterion_10_an_empty_file_a_missing_newline_and_a_ten_megabyte_line() {
    // -- empty ------------------------------------------------------------
    {
        let tree = Tree::new("c10-empty");
        tree.file("empty.txt", "");
        let mut s = Session::start(Launch::new(120, 30, tree.path()));
        open(&mut s, &[], "empty", "empty.txt");

        assert!(
            s.status_row().contains("of 0"),
            "an empty file opens and says it is empty, got {:?}:\n{}",
            s.status_row(),
            s.dump()
        );
        assert!(
            s.body().iter().all(|r| r.trim().is_empty()),
            "and shows nothing:\n{}",
            s.dump()
        );
        // Every navigation key, on a file with nowhere to go.
        for key in [
            keys::CTRL_END,
            keys::PAGE_DOWN,
            keys::DOWN,
            keys::CTRL_HOME,
            keys::PAGE_UP,
            keys::UP,
            keys::RIGHT,
        ] {
            s.send(key);
        }
        s.settle();
        assert!(
            s.is_running() && s.viewing("empty.txt"),
            "navigating an empty file does not end the process:\n{}",
            s.dump()
        );
        assert!(
            s.status_row().contains("0x0 (0)"),
            "the position of an empty file is 0, got {:?}",
            s.status_row()
        );
        // Hex has the same question with a different answer shape: one row,
        // no bytes in it.
        s.press(keys::TWO, "hex over an empty file", |s| {
            s.status_row().contains("hex")
        });
        s.press(keys::CTRL_END, "the end of an empty file", |s| {
            s.status_row().contains("0x0 (0)")
        });
        assert!(
            s.is_running(),
            "hex over an empty file survives `End`:\n{}",
            s.dump()
        );
    }

    // -- no trailing newline ----------------------------------------------
    {
        let tree = Tree::new("c10-nonl");
        let mut body: String = (1..=60).map(|n| format!("line {n:03}\n")).collect();
        body.push_str("omega-last-with-no-newline");
        let bytes = body.len();
        tree.file("nonl.txt", &body);

        let mut s = Session::start(Launch::new(120, 30, tree.path()));
        open(&mut s, &[], "nonl", "nonl.txt");
        assert!(
            s.body_text().contains("line 001"),
            "it opens at the start:\n{}",
            s.dump()
        );

        s.press(keys::CTRL_END, "the end of the file", |s| {
            s.body_text().contains("omega-last-with-no-newline")
        });
        assert!(
            !s.body_text().contains("line 001"),
            "`End` really went to the end:\n{}",
            s.dump()
        );
        // 61 lines, not 62: a file ending without a break has no empty last
        // line, and inventing one is the classic off-by-one here.
        assert!(
            s.status_row().contains("/61"),
            "a file with no trailing newline has 61 lines, not 62, got {:?}:\n{}",
            s.status_row(),
            s.dump()
        );
        // The last row of the body is the last line, with nothing under it.
        let last_text = s
            .body()
            .iter()
            .rev()
            .find(|r| !r.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        assert!(
            last_text.contains("omega-last-with-no-newline"),
            "and it is the bottom-most thing on screen, got {last_text:?}"
        );
        // Hex proves the byte count: the last byte is at len - 1, and there is
        // no phantom `0A` after it.
        s.press(keys::TWO, "hex", |s| s.status_row().contains("hex"));
        s.press(keys::CTRL_END, "the last hex row", |s| {
            s.status_row().contains(&format!("of {bytes}"))
        });
        let tail = s
            .body()
            .iter()
            .rev()
            .find(|r| !r.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        // Six bytes on the last row, `65 77 6c 69 6e 65` - `ewline` - and no
        // `0a` after them. A viewer that invented a trailing break would have
        // one here, and a viewer that dropped the unterminated tail would have
        // no row at all.
        assert!(
            tail.contains("65 77 6c 69 6e 65") && !tail.contains("0a"),
            "the file's last byte is `e`, not a line break, got {tail:?}:\n{}",
            s.dump()
        );
        assert!(
            tail.trim_end().ends_with("ewline"),
            "and the gutter stops with it rather than padding, got {tail:?}"
        );
    }

    // -- one 10 MB line ---------------------------------------------------
    {
        const LEN: u64 = 10 * 1024 * 1024;
        let tree = Tree::new("c10-long");
        // Ten mebibytes with not one line break in it, so nothing about the
        // window can be found by looking for one.
        let line: Vec<u8> = "0123456789abcdef"
            .bytes()
            .cycle()
            .take(usize::try_from(LEN).unwrap_or(0))
            .collect();
        tree.file("one-line.txt", &line);

        let mut s = Session::start(Launch::new(120, 30, tree.path()));
        open(&mut s, &[], "one-line", "one-line.txt");

        assert!(
            s.status_row().contains(&format!("of {LEN}")),
            "the whole file is one line and it opens anyway, got {:?}:\n{}",
            s.status_row(),
            s.dump()
        );
        assert!(
            s.body()
                .first()
                .is_some_and(|r| r.contains("0123456789abcdef")),
            "the first row shows the start of it:\n{}",
            s.dump()
        );
        // the bound made visible: the row is cut at
        // `text::MAX_LINE_BYTES` and says so with `↴`, rather than the viewer
        // trying to hold 10 MB of expanded text for one screen row.
        assert!(
            s.body()
                .iter()
                .any(|r| r.contains('\u{00bb}') || r.contains('\u{21b4}')),
            "a line wider than the terminal is marked, not silently truncated:\n{}",
            s.dump()
        );

        // Navigating to the end. In text mode a one-line file has one line, so
        // hex is where `End` has somewhere to go - and it is the mode in which
        // "the end" is a byte rather than a line.
        s.press(keys::TWO, "hex", |s| s.status_row().contains("hex"));
        // The last byte, not the last row's first: the `Ctrl+End`
        // with a cursor to put there.
        s.press(keys::CTRL_END, "the last byte of a 10 MB line", |s| {
            s.status_row().contains("(10485759)")
        });
        assert!(
            s.body().last().is_some_and(|r| r.starts_with("009FFFF0")),
            "the last row is the file's last sixteen bytes, got {:?}:\n{}",
            s.body().last(),
            s.dump()
        );
        s.press(keys::ONE, "text again", |s| s.status_row().contains("text"));
        s.press(keys::CTRL_END, "the end in text mode", |s| {
            s.status_row().contains("line 1/1")
        });
        assert!(
            s.is_running(),
            "a 10 MB line survives being navigated:\n{}",
            s.dump()
        );
    }
}

// ---------------------------------------------------------------------------
// the cursor and the selection
//
// The v0.6 criteria, beside v0.4's ten and driven the same way: real key bytes
// into a real pseudo-terminal, and assertions on what came back. The two
// clipboard ones read `Session::raw`, because an `OSC 52` sequence is written
// *to* the terminal and never lands in a cell.
// ---------------------------------------------------------------------------

#[test]
fn selection_1_the_arrows_move_a_cursor_and_the_view_follows_only_at_the_edge() {
    // "the arrow keys move it rather than scrolling the page;
    // the view follows when it reaches an edge."
    let tree = Tree::new("sel1");
    let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
    tree.file("long.txt", &body);
    let mut s = Session::start(Launch::new(100, 20, tree.path()));
    open(&mut s, &[], "long", "long.txt");

    let top = s.body().first().cloned().unwrap_or_default();
    assert!(
        top.contains("line 1"),
        "the file opens at its first line: {top:?}"
    );
    // One `Down` moves the cursor, not the window: the status offset advances
    // by a line and the top row is unchanged.
    s.press(keys::DOWN, "the cursor to move to the second line", |s| {
        s.status_row().contains("0x7 (7)")
    });
    assert_eq!(
        s.body().first().cloned().unwrap_or_default(),
        top,
        "the window has not moved:\n{}",
        s.dump()
    );
    // Held to the bottom and past it, the view follows.
    for _ in 0..25 {
        s.send(keys::DOWN);
    }
    s.settle();
    assert_ne!(
        s.body().first().cloned().unwrap_or_default(),
        top,
        "at the edge the view follows the cursor:\n{}",
        s.dump()
    );
    assert!(
        s.body_text().contains("line 26"),
        "and the cursor's own line is on screen:\n{}",
        s.dump()
    );
}

#[test]
fn selection_2_five_bytes_in_hex_survive_tab_and_the_next_shift_makes_it_six() {
    // the diagram, exactly: "Selecting five bytes on the left and
    // pressing `Tab` leaves five characters selected on the right. Pressing
    // `Shift+Right` again extends the same selection to six."
    let tree = Tree::new("sel2");
    tree.file("bytes.bin", &b"0123456789abcdef0123456789abcdef"[..]);
    let mut s = Session::start(Launch::new(120, 20, tree.path()));
    open(&mut s, &[], "bytes", "bytes.bin");
    s.press(keys::TWO, "hex mode", |s| {
        s.status_row().contains("hex [bytes]")
    });

    for _ in 0..5 {
        s.send(keys::SHIFT_RIGHT);
    }
    s.wait("five bytes selected", |s| {
        s.status_row().contains("sel 5 bytes")
    });
    // The selection is painted, on both sides, in `viewer.selection_bg`.
    let lit = s.rows_with_bg(paint::SELECTION_BG);
    assert_eq!(lit.len(), 1, "one row carries it:\n{}", s.dump());
    let painted = lit.first().map(|(_, t)| t.clone()).unwrap_or_default();
    assert_eq!(
        painted.len(),
        5 * 2 + 5,
        "five two-digit columns and five gutter glyphs, got {painted:?}:\n{}",
        s.dump()
    );

    // `Tab` moves the focus and *nothing else changes*.
    s.press(keys::TAB, "the characters side", |s| {
        s.status_row().contains("hex [chars]")
    });
    assert!(
        s.status_row().contains("sel 5 bytes"),
        "the same five bytes, got {:?}",
        s.status_row()
    );
    assert_eq!(
        s.rows_with_bg(paint::SELECTION_BG)
            .first()
            .map(|(_, t)| t.clone())
            .unwrap_or_default(),
        painted,
        "and the same cells are lit:\n{}",
        s.dump()
    );

    s.press(keys::SHIFT_RIGHT, "a sixth byte", |s| {
        s.status_row().contains("sel 6 bytes")
    });
}

#[test]
fn selection_3_esc_clears_the_selection_and_the_next_esc_closes_the_viewer() {
    // "`Esc` clears the selection; **if there is none, close the
    // viewer**" - and the own criterion 1, that the panel comes back
    // with its cursor where it was.
    let tree = Tree::new("sel3");
    tree.file("alpha.txt", "alpha beta gamma\ndelta\n");
    tree.file("beta.txt", "unused\n");
    let mut s = Session::start(Launch::new(100, 20, tree.path()));
    open(&mut s, &[], "alpha", "alpha.txt");

    for _ in 0..4 {
        s.send(keys::SHIFT_RIGHT);
    }
    s.wait("a selection", |s| s.status_row().contains("sel 4 bytes"));
    s.press(keys::ESC, "the selection to be cleared", |s| {
        s.status_row().contains("selection cleared")
    });
    assert!(
        s.viewing("alpha.txt"),
        "the viewer is still up:\n{}",
        s.dump()
    );
    s.press(keys::ESC, "the panel to come back", |s| {
        s.text().contains("Name")
    });
    assert!(
        s.cursor_row().contains("alpha"),
        "and the panel cursor is where it was, got {:?}",
        s.cursor_row()
    );
}

#[test]
fn selection_4_ctrl_c_writes_osc_52_and_names_the_internal_clipboard_once() {
    // the "Where a copy goes": `OSC 52`, plus the internal
    // clipboard, and a terminal is "**told about once**, not on every copy".
    let tree = Tree::new("sel4");
    tree.file("words.txt", "abcdefghij\nsecond line\n");
    let mut s = Session::start(Launch::new(120, 20, tree.path()));
    open(&mut s, &[], "words", "words.txt");

    let mark = s.raw.len();
    for _ in 0..4 {
        s.send(keys::SHIFT_RIGHT);
    }
    s.wait("four bytes", |s| s.status_row().contains("sel 4 bytes"));
    s.press(keys::CTRL_C, "the copy to be reported", |s| {
        s.status_row().contains("copied 4 bytes")
    });
    let wrote = s.raw_since(mark);
    // `abcd` in standard base64 (RFC 4648), inside the sequence the module
    // writes: `ESC ] 52 ; c ; <payload> ESC \`.
    assert!(
        wrote.contains("\x1b]52;c;YWJjZA==\x1b\\"),
        "the OSC 52 payload is the selection, got {:?}",
        wrote
            .split('\x1b')
            .filter(|p| p.starts_with("]52"))
            .collect::<Vec<_>>()
    );
    assert!(
        s.status_row().contains("internal clipboard"),
        "the first copy of a session says so once, got {:?}",
        s.status_row()
    );

    // The second copy does not.
    s.press(keys::SHIFT_RIGHT, "a fifth byte", |s| {
        s.status_row().contains("sel 5 bytes")
    });
    s.press(keys::CTRL_C, "the second copy", |s| {
        s.status_row().contains("copied 5 bytes")
    });
    assert!(
        !s.status_row().contains("internal clipboard"),
        "told about once, not on every copy, got {:?}",
        s.status_row()
    );
}

#[test]
fn selection_5_ctrl_a_is_instant_and_the_copy_after_it_is_refused_with_the_size() {
    // "selecting 40 GB is instant, and copying it is refused
    // with the size". The fixture is a hole, exactly as criterion 9's is, so it
    // costs no disk - and the test skips rather than fails where the
    // filesystem would materialise it.
    const HUGE: u64 = 40 * 1024 * 1024 * 1024;
    const SPARSE_LIMIT: u64 = 64 * 1024 * 1024;

    let tree = Tree::new("sel5");
    let path = tree.path().join("huge.bin");
    let Ok(file) = std::fs::File::create(&path) else {
        eprintln!("SKIPPED: cannot create {}", path.display());
        return;
    };
    if file.set_len(HUGE).is_err() {
        eprintln!("SKIPPED: this filesystem does not do sparse files");
        return;
    }
    drop(file);
    let allocated = match std::fs::metadata(&path) {
        Ok(m) => m.blocks().saturating_mul(512),
        Err(_) => return,
    };
    if allocated > SPARSE_LIMIT {
        let _ = std::fs::remove_file(&path);
        eprintln!("SKIPPED: a 40 GB hole materialised {allocated} bytes here");
        return;
    }

    let mut s = Session::start(Launch::new(160, 30, tree.path()));
    open(&mut s, &[], "huge", "huge.bin");

    let took = s.press_timed(keys::CTRL_A, "the whole file to be selected", |s| {
        s.status_row().contains("sel 42949672960 bytes")
    });
    eprintln!("Ctrl+A over 40 GB took {took:?}");
    assert!(
        took < Duration::from_secs(2),
        "a selection is a byte range, not a read: Ctrl+A took {took:?}"
    );
    s.press(keys::CTRL_C, "the copy to be refused", |s| {
        s.status_row().contains("refused rather than truncated")
    });
    assert!(
        s.status_row().contains("42949672960") && s.status_row().contains("1048576"),
        "refused with both numbers, got {:?}",
        s.status_row()
    );
}

#[test]
fn selection_6_pure_scrolling_is_what_it_was_and_says_why_it_cannot_select() {
    // "`viewer.cursor = false` restores pure scrolling for
    // anyone who wants it" - and the rule that a key which cannot act
    // says why rather than doing nothing.
    let tree = Tree::new("sel6");
    let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
    tree.file("long.txt", &body);
    let mut s =
        Session::start(Launch::new(100, 20, tree.path()).config("[viewer]\ncursor = false\n"));
    open(&mut s, &[], "long", "long.txt");

    let top = s.body().first().cloned().unwrap_or_default();
    s.press(keys::DOWN, "the window to scroll", |s| {
        s.body().first().cloned().unwrap_or_default() != top
    });
    assert!(
        s.body().first().is_some_and(|r| r.contains("line 2")),
        "one Down is one row of window, as v0.4 had it:\n{}",
        s.dump()
    );
    s.press(keys::SHIFT_DOWN, "the refusal", |s| {
        s.status_row().contains("no cursor")
    });
    assert!(
        s.status_row()
            .contains("set viewer.cursor = true to select"),
        "it says what to do about it, got {:?}",
        s.status_row()
    );
    s.press(keys::CTRL_SHIFT_RIGHT, "the same refusal", |s| {
        s.status_row().contains("no cursor")
    });
    assert!(s.is_running() && s.viewing("long.txt"));
}

#[test]
fn selection_7_ctrl_shift_takes_one_column_out_of_aligned_output() {
    // "`Ctrl+Shift` + any of those - extend a **rectangular**
    // selection - a column block, which is how you take one field out of
    // aligned output", and `Ctrl+C` writes exactly that block.
    let tree = Tree::new("sel7");
    tree.file(
        "table.txt",
        "alpha  001\nbeta   002\ngamma  003\ndelta  004\n",
    );
    let mut s = Session::start(Launch::new(120, 20, tree.path()));
    open(&mut s, &[], "table", "table.txt");

    let mark = s.raw.len();
    for _ in 0..2 {
        s.send(keys::CTRL_SHIFT_DOWN);
    }
    for _ in 0..4 {
        s.send(keys::CTRL_SHIFT_RIGHT);
    }
    s.wait("a block four columns wide", |s| {
        s.status_row().contains("sel block 4 cols over")
    });
    // A block reports a span and a width, never a byte count it would have to
    // read every line between the ends to know.
    assert!(
        !s.status_row().contains("sel 4 bytes"),
        "a block is not a count, got {:?}",
        s.status_row()
    );
    // Three rows carry it, four columns of each.
    let lit = s.rows_with_bg(paint::SELECTION_BG);
    assert_eq!(lit.len(), 3, "three rows are in the block:\n{}", s.dump());
    for (row, text) in &lit {
        assert_eq!(
            text.chars().count(),
            4,
            "row {row} shows four columns: {text:?}"
        );
    }

    s.press(keys::CTRL_C, "the block to be copied", |s| {
        s.status_row().contains("copied")
    });
    // `alph\nbeta\ngamm`, which is the band of each covered row, `\n` joined.
    let wrote = s.raw_since(mark);
    assert!(
        wrote.contains("\x1b]52;c;YWxwaApiZXRhCmdhbW0=\x1b\\"),
        "the payload is the block itself:\n{}",
        s.dump()
    );
}

#[test]
fn selection_8_a_half_covered_word_is_half_lit_and_copies_hex_digits() {
    // "The bytes side shows the partly-covered columns partly
    // highlighted, and a copy from that side falls back to **hex digits for the
    // whole covered range**, saying so, rather than printing a value for a word
    // it only holds half of."
    let tree = Tree::new("sel8");
    tree.file(
        "words.bin",
        &b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10"[..],
    );
    let mut s =
        Session::start(Launch::new(140, 20, tree.path()).config("[viewer.hex]\ngroup = 32\n"));
    open(&mut s, &[], "words", "words.bin");
    s.press(keys::TWO, "hex mode", |s| {
        s.status_row().contains("hex [bytes]")
    });
    // The characters side is the only one that can land inside a word: on the
    // bytes side one press is a whole column.
    s.press(keys::TAB, "the characters side", |s| {
        s.status_row().contains("hex [chars]")
    });
    s.press(keys::RIGHT, "one byte in", |s| {
        s.status_row().contains("0x1 (1)")
    });
    for _ in 0..2 {
        s.send(keys::SHIFT_RIGHT);
    }
    s.wait("two bytes across the word", |s| {
        s.status_row().contains("sel 2 bytes")
    });

    // Four of the cell's eight digits, and the two gutter glyphs.
    let painted = s
        .rows_with_bg(paint::SELECTION_BG)
        .first()
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    assert_eq!(
        painted.chars().count(),
        4 + 2,
        "half a 32-bit column's digits, got {painted:?}:\n{}",
        s.dump()
    );

    let mark = s.raw.len();
    s.press(keys::TAB, "back to the bytes side", |s| {
        s.status_row().contains("hex [bytes]")
    });
    s.press(keys::CTRL_C, "the copy to say why", |s| {
        s.status_row().contains("copied 2 bytes")
    });
    assert!(
        s.status_row()
            .contains("not aligned to the 32-bit columns - copied as hex digits"),
        "it says why, got {:?}",
        s.status_row()
    );
    // `02 03`, base64 `MDIgMDM=`.
    assert!(
        s.raw_since(mark).contains("\x1b]52;c;MDIgMDM=\x1b\\"),
        "the payload is the covered range as hex digits:\n{}",
        s.dump()
    );
}

#[test]
fn selection_9_ctrl_shift_c_copies_exactly_the_reading_on_the_status_line() {
    // "`Ctrl+Shift+C` copies that reading instead", and
    // the design invariant 15 - character for character the string
    // the status line is showing, both orders included at `group = 8`.
    let tree = Tree::new("sel9");
    tree.file("word.bin", &b"\x2a\x00\x00\x00\xff\xfe\xfd\xfc"[..]);
    // Wide, so the rank-ordered dropping leaves the reading on the
    // line: it is `RANK_DETAIL` and a narrow terminal gives it up first.
    let mut s = Session::start(Launch::new(200, 20, tree.path()));
    open(&mut s, &[], "word", "word.bin");
    s.press(keys::TWO, "hex mode", |s| {
        s.status_row().contains("hex [bytes]")
    });

    let mark = s.raw.len();
    for _ in 0..4 {
        s.send(keys::SHIFT_RIGHT);
    }
    s.wait("four bytes", |s| s.status_row().contains("sel 4 bytes"));
    const READING: &str = "4 bytes  2a 00 00 00  =  42 (LE)  ·  704643072 (BE)  ·  5.89e-44 (f32 LE)  ·  1.14e-13 (f32 BE)";
    assert!(
        s.status_row().contains(READING),
        "the status line reads the bytes out both ways round, got {:?}",
        s.status_row()
    );
    s.press(keys::CTRL_SHIFT_C, "the reading to be copied", |s| {
        s.status_row().contains("copied 4 bytes")
    });
    assert!(
        s.raw_since(mark).contains(
            "\x1b]52;c;NCBieXRlcyAgMmEgMDAgMDAgMDAgID0gIDQyIChMRSkgIMK3ICA3MDQ2NDMwNzIgKEJFKSAgwrcgIDUuODllLTQ0IChmMzIgTEUpICDCtyAgMS4xNGUtMTMgKGYzMiBCRSk=\x1b\\"
        ),
        "what is shown is what is copied:\n{}",
        s.dump()
    );
}
