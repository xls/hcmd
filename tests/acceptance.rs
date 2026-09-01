//! The nine v0.1 acceptance criteria (KICKOFF.md), driven through
//! the real `hcmd` binary.
//!
//! Every criterion is behavioural - "verify these by actually running the
//! binary, not by reading the code" - so nothing here inspects the library.
//! Each test spawns `env!("CARGO_BIN_EXE_hcmd")` on a pseudo-terminal, writes
//! key bytes into it, parses what came back with `vt100`, and asserts on the
//! rendered cells.
//!
//! Three things about the harness are worth knowing before reading a test:
//!
//! * **The keyboard protocol is forced to `enhanced`.** A bare pty cannot answer
//!   crossterm's capability query, so `auto` would spend its whole timeout and
//!   then decide "legacy" - and criteria 2 and 6 need `Ctrl+Enter` and `Ctrl+3`,
//!   which only exist as distinguishable sequences under the Kitty protocol.
//!   `HCMD_KEYBOARD_PROTOCOL=enhanced` says so up front.
//! * **Keys go in as bytes.** crossterm parses `CSI … u` whether or not the
//!   flags were pushed, so the Kitty encodings in [`keys`] can be injected
//!   directly. Each one is written against crossterm 0.29's own
//!   `parse_csi_u_encoded_key_code`, not from memory.
//! * **No test waits on a clock.** [`Session::wait`] polls the parsed screen
//!   until it stops changing *and* the expected thing is on it, so a slow
//!   machine makes a test slower rather than flaky. Cursor movement changes no
//!   text, so [`Session::press_to`] waits on the painted cursor bar instead.
//!   (The one real sleep is in [`Fixture::staggered`], and it is about file
//!   mtimes, not about the UI.)
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use holoscommander::term;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Keeps concurrently running tests off each other's temporary directories.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Key encodings, all verified against crossterm 0.29's
/// `src/event/sys/unix/parse.rs`.
///
/// The `CSI <codepoint> ; <modifier> u` form is the Kitty protocol's:
/// `parse_modifiers` subtracts one from the modifier field and reads it as a
/// bitmask - `1` shift, `2` alt, `4` control - so `;5u` is Control and `;3u` is
/// Alt. The codepoint is the *unshifted* Unicode value of the key, which is why
/// `Ctrl+3` is `51` (`'3'`) and `Ctrl+Enter` is `13` (`'\r'`, which the parser
/// maps back to `KeyCode::Enter`).
mod keys {
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const RIGHT: &[u8] = b"\x1b[C";
    pub const LEFT: &[u8] = b"\x1b[D";
    pub const ENTER: &[u8] = b"\r";
    pub const BACKSPACE: &[u8] = b"\x7f";
    pub const SPACE: &[u8] = b" ";
    pub const INSERT: &[u8] = b"\x1b[2~";
    pub const F10: &[u8] = b"\x1b[21~";
    /// `F3` - the viewer. `CSI 13 ~`, because crossterm's
    /// `parse_csi_tilde` reads `11..=15` as `F(v - 10)`.
    pub const F3: &[u8] = b"\x1b[13~";

    /// `F9` - the menu bar. `CSI 20 ~`, which crossterm's
    /// `parse_csi_tilde` reads as `F(9)`.
    pub const F9: &[u8] = b"\x1b[20~";
    /// `Esc`, which closes the menu bar and gives the panel back.
    ///
    pub const ESC: &[u8] = b"\x1b";
    /// `Alt+D` - the legacy fallback for `Alt+F1`, the left
    /// panel's device picker (`d` is codepoint 100, Alt is modifier field 3).
    pub const ALT_D: &[u8] = b"\x1b[100;3u";
    /// `Alt+G` - the same for `Alt+F2`, the right panel's.
    pub const ALT_G: &[u8] = b"\x1b[103;3u";
    /// `Ctrl+D` - the hotlist alone (`d` is codepoint 100).
    pub const CTRL_D: &[u8] = b"\x1b[100;5u";
    /// `Ctrl+Q` - the quick view (`q` is codepoint 113).
    pub const CTRL_Q: &[u8] = b"\x1b[113;5u";

    /// `Ctrl+Enter` - the "insert the filename at the caret".
    pub const CTRL_ENTER: &[u8] = b"\x1b[13;5u";
    /// `Ctrl+T` - new tab (`t` is codepoint 116).
    pub const CTRL_T: &[u8] = b"\x1b[116;5u";
    /// `Ctrl+C`, in the Kitty encoding (`c` is codepoint 99).
    pub const CTRL_C_CSI_U: &[u8] = b"\x1b[99;5u";
    /// `Ctrl+C` as a legacy terminal sends it: the raw control byte.
    pub const CTRL_C_BYTE: &[u8] = b"\x03";

    /// `Ctrl+<n>`: the positional column sort of.
    ///
    /// The codepoint is the digit character's own value - `'1'` is 49, `'3'` is
    /// 51 - written out in decimal, which is what the `CSI … u` grammar asks
    /// for. `Ctrl+3` is therefore `ESC [ 51 ; 5 u`.
    pub fn ctrl_digit(n: u8) -> Vec<u8> {
        format!("\x1b[{};5u", 48 + u32::from(n)).into_bytes()
    }

    /// `Alt+<n>`: the tab switch of the design. Alt is modifier bit 2, so
    /// the modifier field is 3.
    pub fn alt_digit(n: u8) -> Vec<u8> {
        format!("\x1b[{};3u", 48 + u32::from(n)).into_bytes()
    }
}

/// The theme colours the assertions read back out of the rendered cells.
///
/// A cursor bar is a *background style*, so it does not appear in the screen's
/// plain text at all; reading the cell colours is the only honest way to check
/// "the cursor is here, drawn like this". These are `themes/blue.toml`
/// verbatim, and `COLORTERM=truecolor` keeps them unquantised.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
    /// `panel.cursor_bg_unfocused` - the active panel's bar while the command
    /// line has focus. Dimmer, never absent.
    pub const CURSOR_UNFOCUSED: (u8, u8, u8) = (0x00, 0x78, 0x78);
    /// `panel.inactive_cursor_bg` - the third, weaker style on the panel that
    /// is not active.
    pub const CURSOR_INACTIVE: (u8, u8, u8) = (0x20, 0x20, 0xB0);
    /// `cmdline.caret_unfocused` - the painted command-line caret.
    pub const CARET_UNFOCUSED: (u8, u8, u8) = (0xA0, 0xA0, 0xA0);
}

// ---------------------------------------------------------------------------
// The fixture directory
// ---------------------------------------------------------------------------

/// Files with distinct, known sizes, so a sort by size has a checkable answer.
/// `two words.txt` is deliberately first in this list: [`Fixture::staggered`]
/// writes it before sleeping, which makes it the oldest file and therefore the
/// checkable answer for a sort by date.
const FILES: &[(&str, usize)] = &[
    ("two words.txt", 13),
    ("zeta.txt", 2),
    ("thorin", 3),
    ("hoax.txt", 4),
    ("Thorin", 5),
    ("2026-budget.xlsx", 6),
    ("thunder", 7),
    ("alpha.rs", 11),
];

/// How many filler entries to add so the listing is longer than the panel.
///
/// They are named `zz-pad-NN.dat` so they sort *after* everything the criteria
/// care about: the listing is longer than the panel - which is the point - but
/// the named files stay above the fold where a test can read them.
const PAD: usize = 30;

/// Every file in the fixture except `..`: [`FILES`], the padding, and `subdir`.
/// Does the filesystem under `dir` tell two names apart by case alone?
///
/// macOS formats APFS case-insensitively by default, so this is `false` on a
/// stock Mac and the case-sensitive half of criterion 1 describes a condition
/// the machine cannot produce. Asked of the real directory rather than of the
/// target name, because a developer's `$TMPDIR` may differ from the volume the
/// rest of the machine uses.
fn case_sensitive(dir: &Path) -> bool {
    let lower = dir.join(".case-probe");
    let upper = dir.join(".CASE-PROBE");
    if std::fs::write(&lower, b"a").is_err() {
        return false;
    }
    // If the write below lands on the same file, the read sees `b`.
    let distinct = std::fs::write(&upper, b"bb").is_ok()
        && std::fs::read(&lower).map(|v| v == b"a").unwrap_or(false);
    let _ = std::fs::remove_file(&lower);
    let _ = std::fs::remove_file(&upper);
    distinct
}

/// A directory tree built fresh per test, removed on drop.
///
/// It carries the names the criteria name - `thorin`, `Thorin`, `thunder`,
/// `alpha.rs`, `two words.txt`, a subdirectory - plus two deliberate decoys and
/// enough entries to scroll:
///
/// * `hoax.txt` catches a quick search that drops the first character typed:
///   an implementation that treated `tho` as `ho` would land on it.
/// * `2026-budget.xlsx` is the own example of why bare digits have
///   to reach the quick-search buffer rather than switching tabs.
struct Fixture {
    root: PathBuf,
    /// Whether the filesystem under it tells `thorin` from `Thorin`.
    cased: bool,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        Self::build(tag, false)
    }

    /// The same tree, but with `two words.txt` written a full second before
    /// everything else, so mtimes are far enough apart for a sort by date to
    /// have one unambiguous answer.
    fn staggered(tag: &str) -> Self {
        Self::build(tag, true)
    }

    fn build(tag: &str, stagger: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-acc-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");

        // `thorin` and `Thorin` are one file where the filesystem folds case,
        // and writing both leaves a single entry holding the second one's
        // bytes: an entry short, and a size that belongs to the other name.
        let cased = case_sensitive(&root);
        let mut files = FILES.iter().filter(|(name, _)| cased || *name != "Thorin");
        let (first, size) = files.next().expect("FILES is not empty");
        std::fs::write(root.join(first), vec![b'x'; *size]).expect("fixture file");
        if stagger {
            // Coarse on purpose: one second is the worst mtime granularity a
            // filesystem is likely to have, so this cannot tie.
            std::thread::sleep(Duration::from_millis(1_100));
        }
        for (name, size) in files {
            std::fs::write(root.join(name), vec![b'x'; *size]).expect("fixture file");
        }
        for i in 0..PAD {
            std::fs::write(root.join(format!("zz-pad-{i:02}.dat")), vec![b'x'; 100 + i])
                .expect("fixture padding");
        }
        std::fs::create_dir_all(root.join("subdir")).expect("fixture subdir");
        std::fs::write(root.join("subdir/inner.txt"), b"inner").expect("fixture inner file");
        Self { root, cased }
    }

    /// Does this tree hold `thorin` and `Thorin` as two entries?
    const fn has_case_pair(&self) -> bool {
        self.cased
    }

    /// Entries in the listing, `..` excluded - one fewer where the case pair
    /// collapsed into a single file.
    const fn entry_count(&self) -> usize {
        if self.cased {
            FILES.len() + PAD + 1
        } else {
            FILES.len() + PAD
        }
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

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// How to start one `hcmd`.
struct Launch {
    cols: u16,
    rows: u16,
    cwd: PathBuf,
    /// Written to `$XDG_CONFIG_HOME/holoscommander/config.toml` before the
    /// child starts. `None` leaves the child on the compiled-in defaults.
    config: Option<String>,
    /// Sets `HCMD_PANIC_TEST`, the hidden switch that makes the binary panic
    /// deliberately once the terminal is in raw mode. The value
    /// names *when*: `start` (what a bare `1` also means) or `viewer`.
    panic_test: Option<&'static str>,
}

impl Launch {
    fn new(cols: u16, rows: u16, cwd: impl Into<PathBuf>) -> Self {
        Self {
            cols,
            rows,
            cwd: cwd.into(),
            config: None,
            panic_test: None,
        }
    }

    fn config(mut self, toml: impl Into<String>) -> Self {
        self.config = Some(toml.into());
        self
    }

    fn panic_test(mut self) -> Self {
        self.panic_test = Some("1");
        self
    }

    /// Panic on the first frame the viewer holds the screen.
    fn panic_test_in_viewer(mut self) -> Self {
        self.panic_test = Some("viewer");
        self
    }
}

/// A running `hcmd` and the screen it has painted so far.
struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    /// Every byte the child has written, for the assertions in criterion 9 that
    /// are about escape sequences rather than about the screen.
    raw: Vec<u8>,
    home: PathBuf,
}

impl Session {
    fn start(launch: Launch) -> Self {
        let Launch {
            cols,
            rows,
            cwd,
            config,
            panic_test,
        } = launch;

        let home = std::env::temp_dir().join(format!(
            "hcmd-acc-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("holoscommander")).expect("throwaway config dir");
        if let Some(toml) = config {
            std::fs::write(home.join("holoscommander/config.toml"), toml).expect("write config");
        }

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
        // machine: the assertions read `▲`, `≥` and `│`.
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        // a bare pty answers no capability query, and criteria 2
        // and 6 need keys that only the Kitty protocol can express.
        cmd.env("HCMD_KEYBOARD_PROTOCOL", "enhanced");
        // No filesystem watch under the harness: an inotify thread per session,
        // times the number running in parallel, perturbs the screen-settle
        // timing these tests poll on. What the watch does is covered by unit
        // tests, not here.
        cmd.env("HCMD_NO_FS_WATCH", "1");
        cmd.env("XDG_CONFIG_HOME", &home);
        cmd.env("XDG_STATE_HOME", &home);
        if let Some(stage) = panic_test {
            cmd.env("HCMD_PANIC_TEST", stage);
        }
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
            child,
            writer,
            rx,
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
            home,
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
    ///
    /// The cursor position is part of it because ratatui re-issues it on every
    /// frame whether or not anything changed, so "no more bytes arrived" is not
    /// a usable definition of settled - "the picture stopped changing" is.
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
    ///
    /// The predicate gets the whole session, so a test can wait on a painted
    /// cursor bar as readily as on the text - which matters, because "the
    /// cursor moved" is invisible in the plain text.
    fn wait(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.settle();
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- screen ---\n{}",
                    self.text()
                );
            }
            self.pump(Duration::from_millis(150));
        }
    }

    /// [`Session::wait`] against the screen's plain text.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        self.wait(what, |s| pred(&s.text()));
    }

    /// The listing has arrived and the panels are drawn.
    fn wait_for_listing(&mut self) {
        self.wait_for("the fixture listing", |t| {
            t.contains("thunder") && t.contains("[subdir]")
        });
    }

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// Send a key and wait for the screen to reflect it.
    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&str) -> bool) {
        self.send(bytes);
        self.wait_for(what, pred);
    }

    /// Send a key and wait until the focused panel's cursor bar has landed on a
    /// row containing `needle`.
    ///
    /// Cursor movement changes no text, so waiting for "the screen settled" is
    /// not enough on its own: the key may not have been processed yet, and the
    /// next key would then act on the wrong entry. This waits for the thing
    /// that actually moved.
    fn press_to(&mut self, bytes: &[u8], needle: &'static str) {
        self.send(bytes);
        self.wait(&format!("the cursor to reach {needle:?}"), move |s| {
            s.cursor_row().contains(needle)
        });
    }

    /// The entry the focused panel's cursor bar is sitting on.
    fn cursor_row(&self) -> String {
        self.row_with_bg(paint::CURSOR_FOCUSED).unwrap_or_default()
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
        self.settle();
    }

    // -- reading the painted cells ----------------------------------------

    /// The row index of the command line: the last row carrying the prompt's
    /// `>`.
    fn cmdline_row(&self) -> u16 {
        let (rows, _) = self.parser.screen().size();
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        // Found structurally, not by looking for the `<cwd>> ` this application
        // used to compose: the design made the command line the **shell's**
        // own input line, so its prompt is whatever the user's shell draws -
        // here a real `bash`, whose `PS1` need not contain a `>` at all, and
        // whose block may be more than one row tall. (`<DIR>` in the size
        // column does contain one, which is what the old rule found.)
        //
        // The layout is fixed and is the reliable answer: the key bar is the
        // last row, and the command line is the rows above it. The caret is on
        // the last of those, so it is the row above the key bar.
        let last = rows.saturating_sub(1);
        let has_keybar = lines
            .get(usize::from(last))
            .is_some_and(|line| line.contains("F10"));
        if has_keybar {
            last.saturating_sub(1)
        } else {
            last
        }
    }

    /// The column of the painted (unfocused) command-line caret, if one is
    /// drawn. it is a styled cell, so it is invisible in the
    /// plain text and has to be read out of the colours.
    fn painted_caret_col(&self) -> Option<u16> {
        let row = self.cmdline_row();
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let want = rgb(paint::CARET_UNFOCUSED);
        (0..cols).find(|col| {
            screen
                .cell(row, *col)
                .is_some_and(|cell| cell.fgcolor() == want || cell.bgcolor() == want)
        })
    }

    /// The trimmed text of the row painted in `bg` - that is, the entry a
    /// cursor bar is sitting on. `None` when nothing carries that background.
    ///
    /// Only the rows above the command line are searched. The blue theme paints
    /// `keybar.label_bg` in the same `#00A8A8` as `panel.cursor_bg`, so a scan
    /// of the whole screen would report the key bar as a cursor bar and the
    /// "the focused style is gone" half of the design could never be
    /// checked.
    fn row_with_bg(&self, bg: (u8, u8, u8)) -> Option<String> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows = self.cmdline_row();
        let want = rgb(bg);
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

    /// Where the terminal's real cursor is, as `(row, col)`.
    fn hardware_cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Whether the terminal's real cursor is drawn at all.
    ///
    /// the design hides it while a panel has focus - the full-width bar is
    /// that region's cursor and a block on top of it is a stray box - so
    /// "where is it" is only a meaningful question when the command line has
    /// focus.
    fn hardware_cursor_hidden(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    /// Screen row 1: the tab bar when a panel has more than one tab, and the
    /// path-and-filter line otherwise.
    fn tab_bar(&self) -> String {
        self.text().lines().nth(1).unwrap_or_default().to_string()
    }

    /// The **left** panel's column header - the row the sort arrow lives on.
    ///
    /// One screen row carries both panels' headers, so the right panel's
    /// `▲Name` would otherwise answer every question asked about the left one.
    fn header(&self) -> String {
        let text = self.text();
        let line = text
            .lines()
            .find(|line| line.contains("Name"))
            .unwrap_or_default();
        left_cell(line)
    }

    /// The left panel's entry rows, in order, starting with `[..]`.
    ///
    /// The panel is a box, so each screen row holds both panels; splitting on
    /// the border glyph and taking the first half is what isolates the left
    /// one.
    fn left_entries(&self) -> Vec<String> {
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        let Some(header) = lines.iter().position(|line| line.contains("Name")) else {
            return Vec::new();
        };
        lines
            .iter()
            .skip(header + 1)
            .map(|line| left_cell(line))
            .take_while(|cell| !cell.trim().is_empty())
            .collect()
    }

    /// The whole byte stream the child has written.
    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    /// Wait for the child to exit; `None` if it outlived the timeout.
    fn wait_exit(&mut self, timeout: Duration) -> Option<bool> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(100));
            if let Ok(Some(status)) = self.child.try_wait() {
                // Drain whatever the teardown wrote on its way out.
                self.pump(Duration::from_millis(300));
                return Some(status.success());
            }
        }
        None
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

/// The left panel's half of one screen row.
///
/// Both panels are drawn side by side inside their own boxes, so every row
/// holds two cells separated by the border glyph; the first is the left one.
fn left_cell(line: &str) -> String {
    line.split('│')
        .nth(1)
        .unwrap_or(line)
        .trim_end()
        .to_string()
}

fn rgb((r, g, b): (u8, u8, u8)) -> vt100::Color {
    vt100::Color::Rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// 1. Quick search: prefix matching, first letter included, smart case
// ---------------------------------------------------------------------------

#[test]
fn criterion_1_typing_navigates_with_the_first_letter_and_smart_case() {
    let fix = Fixture::new("c1");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    // `tho` - all lowercase, so smart case matches insensitively, and the
    // cursor goes to the **first** matching entry. The fixture
    // holds both `thorin` and `Thorin`; which of the two that is, is decided by
    // the listing order, so the assertion is written against the listing rather
    // than against a guess about it.
    s.press(b"tho", "the quick-search buffer", |t| {
        t.contains("search: tho")
    });
    let expected = s
        .left_entries()
        .into_iter()
        .find(|row| row.trim().to_lowercase().starts_with("tho"))
        .unwrap_or_else(|| panic!("no `tho` entry in the listing:\n{}", s.text()));
    let on = s
        .row_with_bg(paint::CURSOR_FOCUSED)
        .unwrap_or_else(|| panic!("no focused cursor bar:\n{}", s.text()));
    assert_eq!(
        on.trim(),
        expected.trim(),
        "`tho` selects the first `thorin` in the listing:\n{}",
        s.text()
    );
    assert!(
        on.to_lowercase().contains("thorin"),
        "...which is a `thorin`, not {on:?}:\n{}",
        s.text()
    );
    // The decoy is the point: an implementation that dropped the first
    // character typed would have searched `ho` and landed on `hoax.txt`.
    assert!(
        !on.contains("hoax"),
        "the first letter typed must be part of the buffer, got {on:?}"
    );
    assert!(
        s.text().contains("search: tho [aa]"),
        "an all-lowercase buffer matches case-insensitively:\n{}",
        s.text()
    );

    // A capital flips smart case to sensitive, for this buffer only.
    //
    // This half needs two names that differ by case alone, which a
    // case-folding filesystem cannot hold: writing `thorin` and `Thorin` there
    // leaves one file. The rule is still exercised below, where `thu` matches
    // and `Thu` does not - that half needs no pair, because the absence of a
    // `Thunder` is a fact on every filesystem.
    if fix.has_case_pair() {
        let mut s = Session::start(Launch::new(120, 30, fix.path()));
        s.wait_for_listing();
        s.press(b"Tho", "the case-sensitive buffer", |t| {
            t.contains("search: Tho [Aa]")
        });
        let on = s
            .row_with_bg(paint::CURSOR_FOCUSED)
            .unwrap_or_else(|| panic!("no focused cursor bar:\n{}", s.text()));
        assert!(
            on.contains("Thorin") && !on.contains("thorin"),
            "`Tho` matches only `Thorin`, got {on:?}:\n{}",
            s.text()
        );
    } else {
        eprintln!(
            "criterion 1: the `Tho` case pair was not exercised; {} folds case",
            fix.path().display()
        );
    }

    // And the other half of "only": `thu` finds `thunder`, `Thu` finds nothing,
    // because no `Thunder` exists. Without the case rule both would match.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press(b"thu", "a lowercase match", |t| t.contains("search: thu"));
    let on = s.row_with_bg(paint::CURSOR_FOCUSED).unwrap_or_default();
    assert!(
        on.contains("thunder"),
        "`thu` selects `thunder`, got {on:?}"
    );

    //
    // Quick search refuses a character that would leave the buffer matching
    // nothing, so *which* character is the dead end depends on the listing.
    // Where `Thorin` survives, `T` and `Th` both match and `u` is the dead
    // end. Where the filesystem folded the pair away there is no capitalised
    // name at all, and the very first `T` is refused - which is the same rule
    // reaching the same answer one character sooner, not a different one.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    let (typed, expected): (&[u8], &str) = if fix.has_case_pair() {
        (b"Thu", "no match: Thu")
    } else {
        (b"T", "no match: T")
    };
    s.press(typed, "the no-match flash", |t| t.contains(expected));
    assert!(
        s.text().contains("[Aa]"),
        "the status line says why it did not match:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 2. The command line is a focus target with a persistent caret
// ---------------------------------------------------------------------------

/// The the design walkthrough, with one addition: the caret is deliberately
/// left in the *middle* of the line, so "inserted at the caret" and "appended at
/// the end" have different answers and the test can tell them apart.
fn command_line_round_trip(fix: &Fixture, enter_with: &[u8], label: &str) {
    // **No shell.** This criterion is about the command line the design
    // specifies - this application owning the text and the caret - which
    // the design keeps as what happens when there is no PTY. With a live
    // shell the same keys are the shell's own line editor's and the row at the
    // foot of the screen is whatever it drew, so asserting on an exact caret
    // column here would be asserting on somebody's `PS1`. The console's own
    // acceptance criteria are v0.3's, and they are separate.
    let mut s =
        Session::start(Launch::new(120, 30, fix.path()).config("[console]\nenabled = false\n"));
    s.wait_for_listing();
    let cmd_row = s.cmdline_row();

    // 1. Left/Right focuses the command line: the hardware cursor appears there.
    assert!(
        s.hardware_cursor_hidden(),
        "{label}: the panel starts with focus, so the block is hidden"
    );
    s.send(enter_with);
    s.wait("focus to reach the command line", |s| {
        !s.hardware_cursor_hidden() && s.hardware_cursor().0 == cmd_row
    });

    // 2. Type `cp X`, then Left, leaving the caret between the space and the X.
    s.press(b"cp X", "the typed command", |t| t.contains("cp X"));
    let end_col = s.hardware_cursor().1;
    s.send(keys::LEFT);
    s.wait("the caret to move left", move |s| {
        s.hardware_cursor() == (cmd_row, end_col - 1)
    });
    let caret_col = s.hardware_cursor().1;

    // 3. Up returns to the panel. The text and the caret both survive, and the
    //    caret is now drawn as a painted cell rather than the hardware cursor.
    //
    s.press(keys::UP, "focus to return to the panel", |t| {
        t.contains("cp X")
    });
    assert!(
        s.hardware_cursor_hidden(),
        "Up leaves the command line, so the block goes:\n{}",
        s.text()
    );
    assert_eq!(
        s.painted_caret_col(),
        Some(caret_col),
        "the caret survives the round trip in place:\n{}",
        s.text()
    );

    // 4. Move the panel cursor to a file and press Ctrl+Enter.
    s.press(b"thu", "the quick search", |t| t.contains("search: thu"));
    s.press(keys::CTRL_ENTER, "the inserted filename", |t| {
        t.contains("cp thunder X")
    });
    assert!(
        !s.text().contains("cp Xthunder") && !s.text().contains("cp X thunder"),
        "the name goes at the caret, not at the end:\n{}",
        s.text()
    );
    assert_eq!(
        s.hardware_cursor().0,
        cmd_row,
        "Ctrl+Enter takes focus to the command line:\n{}",
        s.text()
    );
    assert!(
        s.row_with_bg(paint::CURSOR_UNFOCUSED).is_some(),
        "the panel cursor bar is still drawn, one step back:\n{}",
        s.text()
    );

    // 5. Focus is already on the command line, and the caret is where the
    //    insertion left it - just past `thunder `, still before the `X`.
    s.wait("the caret to settle after the insertion", move |s| {
        s.hardware_cursor().0 == cmd_row
    });
    assert_eq!(
        s.hardware_cursor(),
        (cmd_row, caret_col + 8),
        "the caret advanced past what was inserted:\n{}",
        s.text()
    );
}

#[test]
fn criterion_2_right_and_left_focus_the_command_line_at_its_remembered_caret() {
    let fix = Fixture::new("c2");
    command_line_round_trip(&fix, keys::RIGHT, "Right");
    command_line_round_trip(&fix, keys::LEFT, "Left");
}

// ---------------------------------------------------------------------------
// 3. Both cursors are visible at all times
// ---------------------------------------------------------------------------

#[test]
fn criterion_3_both_cursors_are_visible_at_all_times() {
    let fix = Fixture::new("c3");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    let cmd_row = s.cmdline_row();

    // Panel focused. Both *cursors* are drawn - the panel's bar and the painted
    // command-line caret - while the terminal's own block is hidden, because
    // the bar is already the panel's cursor and a block on top of it is a stray
    // box.
    assert!(
        s.hardware_cursor_hidden(),
        "the block is hidden while a panel has focus:\n{}",
        s.text()
    );
    assert!(
        s.row_with_bg(paint::CURSOR_FOCUSED).is_some(),
        "the panel's cursor bar is drawn:\n{}",
        s.text()
    );
    let caret = s.painted_caret_col();
    assert!(
        caret.is_some(),
        "the command-line caret is drawn while the panel has focus:\n{}",
        s.text()
    );
    let cell_is_styled = {
        let col = caret.unwrap_or(0);
        let screen = s.parser.screen();
        let want = rgb(paint::CARET_UNFOCUSED);
        screen
            .cell(cmd_row, col)
            .is_some_and(|c| c.fgcolor() == want || c.bgcolor() == want)
    };
    assert!(
        cell_is_styled,
        "the caret cell carries cmdline.caret_unfocused"
    );
    assert!(
        s.row_with_bg(paint::CURSOR_FOCUSED).is_some(),
        "the active panel's bar:\n{}",
        s.text()
    );
    assert!(
        s.row_with_bg(paint::CURSOR_INACTIVE).is_some(),
        "the inactive panel's bar, a third weaker style:\n{}",
        s.text()
    );

    // Command line focused. Now the hardware cursor is on the prompt row and
    // the panel bar is drawn in the dimmed style - visible, not gone.
    s.send(keys::RIGHT);
    s.wait("focus to reach the command line", move |s| {
        s.hardware_cursor().0 == cmd_row
    });
    assert!(
        !s.parser.screen().hide_cursor(),
        "still not hidden with the command line focused"
    );
    let bar = s.row_with_bg(paint::CURSOR_UNFOCUSED);
    assert!(
        bar.is_some(),
        "the panel cursor bar stays visible, dimmed:\n{}",
        s.text()
    );
    assert!(
        s.row_with_bg(paint::CURSOR_FOCUSED).is_none(),
        "and it is no longer the focused style:\n{}",
        s.text()
    );
    assert!(
        s.row_with_bg(paint::CURSOR_INACTIVE).is_some(),
        "the other panel keeps its own bar too:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 4. Backspace: parent directory, or edit the search buffer
// ---------------------------------------------------------------------------

#[test]
fn criterion_4_backspace_goes_up_unless_a_quick_search_is_running() {
    let fix = Fixture::new("c4");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    // No search running: Backspace navigates. Get somewhere to come back from.
    s.press_to(keys::DOWN, "[subdir]");
    s.press(keys::ENTER, "the subdirectory listing", |t| {
        t.contains("inner")
    });
    s.press(keys::BACKSPACE, "the parent listing", |t| {
        t.contains("thunder") && !t.contains("inner")
    });

    // A search running: Backspace edits the buffer and stays put.
    s.press(b"thu", "the quick-search buffer", |t| {
        t.contains("search: thu")
    });
    s.press(keys::BACKSPACE, "the shortened buffer", |t| {
        t.contains("search: th")
    });
    let text = s.text();
    assert!(
        !text.contains("search: thu"),
        "Backspace popped a character:\n{text}"
    );
    assert!(
        text.contains("thunder") && text.contains("[subdir]"),
        "and it did not navigate:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// 5. Space marks or extends the search; Insert always marks
// ---------------------------------------------------------------------------

#[test]
fn criterion_5_space_is_ambiguous_and_insert_is_not() {
    let fix = Fixture::new("c5");
    let selected = format!("1 of {} selected", fix.entry_count());

    // Space with an empty buffer marks the entry under the cursor.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press_to(keys::DOWN, "[subdir]");
    s.press_to(keys::DOWN, "2026-budget");
    s.press(keys::SPACE, "the selection count", |t| {
        t.contains(&selected)
    });

    // Space with a buffer extends it - filenames contain spaces.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press(b"two", "the quick-search buffer", |t| {
        t.contains("search: two")
    });
    s.send(keys::SPACE);
    s.press(b"w", "the buffer with a space in it", |t| {
        t.contains("search: two w")
    });
    let on = s.row_with_bg(paint::CURSOR_FOCUSED).unwrap_or_default();
    assert!(
        on.contains("two words"),
        "the extended search selects the file with the space, got {on:?}:\n{}",
        s.text()
    );
    assert!(
        !s.text().contains(" selected"),
        "Space must not have marked anything:\n{}",
        s.text()
    );

    // Insert marks whatever the buffer holds.
    s.press(keys::INSERT, "the selection count", |t| {
        t.contains(&selected)
    });
    assert!(
        !s.text().contains("search:"),
        "Insert marks and moves down, which clears the buffer:\n{}",
        s.text()
    );

    // ...and with an empty buffer too, so it is never the ambiguous one.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press_to(keys::DOWN, "[subdir]");
    s.press(keys::INSERT, "the selection count", |t| {
        t.contains(&selected)
    });
}

// ---------------------------------------------------------------------------
// 6. Ctrl+<n> is positional over the configured column order
// ---------------------------------------------------------------------------

#[test]
fn criterion_6_ctrl_3_sorts_by_whatever_the_third_column_is() {
    // One fixture for both children: `two words.txt` is the oldest file and
    // `zeta.txt` is the smallest, so "sorted by date" and "sorted by size" have
    // different, checkable first rows.
    let fix = Fixture::staggered("c6");
    let ctrl3 = keys::ctrl_digit(3);

    // Default order - name, ext, size, date, attr - so the third column is
    // `size`.
    let mut s = Session::start(Launch::new(140, 30, fix.path()));
    s.wait_for_listing();
    assert!(
        s.header().contains("\u{25B2}Name"),
        "the panel starts sorted by name:\n{}",
        s.header()
    );
    s.send(&ctrl3);
    s.wait("the sort arrow on Size", |s| {
        s.header().contains("\u{25B2}Size")
    });
    let header = s.header();
    assert!(
        header.contains("\u{25B2}Size"),
        "Ctrl+3 sorts by size with the default order:\n{header}"
    );
    assert!(
        !header.contains("\u{25B2}Name") && !header.contains("\u{25B2}Date"),
        "and only by size:\n{header}"
    );
    let rows = s.left_entries();
    let first_file = rows
        .iter()
        .find(|row| !row.contains("[.."))
        .cloned()
        .unwrap_or_default();
    assert!(
        first_file.contains("subdir"),
        "directories_first still applies on top of the sort:\n{rows:#?}"
    );
    let smallest = rows
        .iter()
        .find(|row| !row.contains('['))
        .cloned()
        .unwrap_or_default();
    assert!(
        smallest.contains("zeta"),
        "ascending by size puts the 2-byte file first, got {smallest:?}:\n{rows:#?}"
    );

    // Same key, same binding, a different `panel.columns.order`: `date` is now
    // third and Ctrl+3 follows it, with nothing rebound.
    let config = "[panel.columns]\norder = [\"name\", \"ext\", \"date\", \"size\", \"attr\"]\n";
    let mut s = Session::start(Launch::new(140, 30, fix.path()).config(config));
    s.wait_for_listing();
    assert!(
        s.header().find("Date").unwrap_or(usize::MAX) < s.header().find("Size").unwrap_or(0),
        "the config really did move `date` before `size`:\n{}",
        s.header()
    );
    s.send(&ctrl3);
    s.wait("the sort arrow on Date", |s| {
        s.header().contains("\u{25B2}Date")
    });
    let header = s.header();
    assert!(
        header.contains("\u{25B2}Date"),
        "Ctrl+3 follows the configured order:\n{header}"
    );
    assert!(
        !header.contains("\u{25B2}Size"),
        "and it is not still sorting by size:\n{header}"
    );
    let rows = s.left_entries();
    let oldest = rows
        .iter()
        .find(|row| !row.contains('['))
        .cloned()
        .unwrap_or_default();
    assert!(
        oldest.contains("two words"),
        "ascending by date puts the oldest file first, got {oldest:?}:\n{rows:#?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Narrowing hides columns by priority; a hidden sort still works
// ---------------------------------------------------------------------------

#[test]
fn criterion_7_columns_drop_by_priority_and_a_hidden_sort_says_so() {
    let fix = Fixture::new("c7");
    let mut s = Session::start(Launch::new(140, 30, fix.path()));
    s.wait_for_listing();

    let wide = s.header();
    for column in ["Name", "Ext", "Size", "Date", "Attr"] {
        assert!(
            wide.contains(column),
            "a wide panel shows the whole column set, missing {column}:\n{wide}"
        );
    }

    // `hide_priority` is attr, ext, size, date - so they go in that order and
    // date outlives all of them.
    s.resize(104, 30);
    let no_attr = s.header();
    assert!(!no_attr.contains("Attr"), "attr goes first:\n{no_attr}");
    assert!(
        no_attr.contains("Ext") && no_attr.contains("Size") && no_attr.contains("Date"),
        "and nothing else went with it:\n{no_attr}"
    );

    s.resize(88, 30);
    let no_ext = s.header();
    assert!(
        !no_ext.contains("Ext") && !no_ext.contains("Attr"),
        "ext goes second:\n{no_ext}"
    );
    assert!(
        no_ext.contains("Size") && no_ext.contains("Date"),
        "size and date are still there:\n{no_ext}"
    );

    s.resize(80, 30);
    let no_size = s.header();
    assert!(!no_size.contains("Size"), "size goes third:\n{no_size}");
    assert!(no_size.contains("Date"), "date is kept longest:\n{no_size}");
    assert!(
        no_size.contains("Name"),
        "name is never dropped:\n{no_size}"
    );

    // Sorting by a column that is not drawn keeps working, and the status-line
    // tag is what says so.
    s.press(&keys::ctrl_digit(3), "the hidden-column sort tag", |t| {
        t.contains("[size \u{25B2}]")
    });
    assert!(
        !s.header().contains("Size"),
        "the sorted column really is hidden:\n{}",
        s.header()
    );
    let rows = s.left_entries();
    let smallest = rows
        .iter()
        .find(|row| !row.contains('['))
        .cloned()
        .unwrap_or_default();
    assert!(
        smallest.contains("zeta"),
        "the sort itself still ran, got {smallest:?}:\n{rows:#?}"
    );

    // Widening brings the column back, and the tag goes away again - the design
    // it exists only for the case where the header arrow cannot.
    s.resize(140, 30);
    let back = s.header();
    assert!(
        back.contains("\u{25B2}Size"),
        "the header arrow returns with the column:\n{back}"
    );
    assert!(
        !s.text().contains("[size \u{25B2}]"),
        "and the tag is not redundant beside it:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 8. Alt+<n> switches tabs; bare digits do not
// ---------------------------------------------------------------------------

#[test]
fn criterion_8_alt_digits_switch_tabs_and_bare_digits_search() {
    let fix = Fixture::new("c8");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    // Two more tabs, so there is something for Alt+1..Alt+3 to address. The
    // tab bar appears once a panel has more than one, which is
    // how the test knows the tabs exist before it starts switching between
    // them.
    assert!(
        !s.tab_bar().contains("2 "),
        "one tab, so no tab bar yet:\n{}",
        s.tab_bar()
    );
    s.send(keys::CTRL_T);
    s.send(keys::CTRL_T);
    s.wait("a three-tab tab bar", |s| s.tab_bar().contains("3 "));
    let bar = s.tab_bar();
    for index in ["1 ", "2 ", "3 "] {
        assert!(
            bar.contains(index),
            "the tab bar should number every tab, missing {index:?}:\n{bar}"
        );
    }

    // Send the third tab somewhere else, so the tabs are distinguishable.
    s.press(b"sub", "the cursor on [subdir]", |t| {
        t.contains("search: sub")
    });
    s.press(keys::ENTER, "the third tab in subdir", |t| {
        t.contains("inner")
    });

    s.press(&keys::alt_digit(1), "tab 1", |t| {
        t.contains("thunder") && !t.contains("inner")
    });
    s.press(&keys::alt_digit(3), "tab 3", |t| t.contains("inner"));
    s.press(&keys::alt_digit(2), "tab 2", |t| {
        t.contains("thunder") && !t.contains("inner")
    });

    // A bare digit is a search, not a tab switch - which is
    // the whole reason `2026-budget.xlsx` is in the fixture.
    s.press(b"2", "the digit in the quick-search buffer", |t| {
        t.contains("search: 2")
    });
    let on = s.row_with_bg(paint::CURSOR_FOCUSED).unwrap_or_default();
    assert!(
        on.contains("2026-budget"),
        "a bare digit navigates, got {on:?}:\n{}",
        s.text()
    );
    assert!(
        !s.text().contains("inner"),
        "and it did not switch to tab 2's neighbour:\n{}",
        s.text()
    );

    // Alt+<n> really is addressing tabs and not something that happens to
    // work: the tenth one does not exist, and says so rather than doing
    // nothing (the rule, applied to tabs).
    s.press(&keys::alt_digit(9), "the out-of-range tab message", |t| {
        t.contains("there is no tab 9")
    });
}

// ---------------------------------------------------------------------------
// 9. Ctrl+C and a panic both leave the terminal usable
// ---------------------------------------------------------------------------

/// The bytes `Term::restore` writes, in the order it writes them.
mod restore {
    /// `EnterAlternateScreen`.
    pub const ALT_ON: &str = "\x1b[?1049h";
    /// `LeaveAlternateScreen`.
    pub const ALT_OFF: &str = "\x1b[?1049l";
    /// `Show`, which `Term::restore` emits *after* `disable_raw_mode`.
    pub const CURSOR_SHOWN: &str = "\x1b[?25h";
}

#[test]
fn criterion_9_ctrl_c_and_a_panic_both_leave_the_terminal_usable() {
    let fix = Fixture::new("c9");

    // -- Ctrl+C ------------------------------------------------------------
    // Nothing in the design binds Ctrl+C, and raw mode means it arrives as a
    // key event rather than a signal - so "usable" here means the application
    // is unharmed by it and still quits cleanly afterwards.
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.send(keys::CTRL_C_BYTE);
    s.send(keys::CTRL_C_CSI_U);
    s.settle();
    assert!(
        s.is_running(),
        "Ctrl+C is not bound, so it must not take the application down:\n{}",
        s.text()
    );
    assert!(
        s.text().contains("thunder"),
        "the panels are still drawn after Ctrl+C:\n{}",
        s.text()
    );
    // Still responsive, which is the difference between "survived" and "wedged".
    s.press(b"thu", "the quick search after Ctrl+C", |t| {
        t.contains("search: thu")
    });

    // `ui.confirm_exit` is on by default, so `F10` asks first.
    // Not just "Quit" - the key bar carries that already. The prompt's own
    // question is what says it opened.
    s.press(keys::F10, "the quit prompt", |t| {
        t.contains("Quit Holos Commander?")
    });
    s.send(b"y");
    let ok = s.wait_exit(Duration::from_secs(10));
    assert_eq!(ok, Some(true), "F10 should quit cleanly:\n{}", s.text());
    assert!(
        !s.parser.screen().alternate_screen(),
        "the alternate screen is left on exit"
    );
    let raw = s.raw_text();
    let entered = raw.find(restore::ALT_ON);
    let left = raw.rfind(restore::ALT_OFF);
    let shown = raw.rfind(restore::CURSOR_SHOWN);
    assert!(
        entered.is_some() && left > entered,
        "the alternate screen was entered and then left"
    );
    assert!(
        shown > left,
        "the cursor is shown last, after disable_raw_mode"
    );

    // -- a panic -----------------------------------------------------------
    // `HCMD_PANIC_TEST` panics deliberately once raw mode and the alternate
    // screen are on, which is the only state in which the rule - "a
    // panic hook that leaves the user's terminal in raw mode is a bug" - has
    // anything to say.
    let mut s = Session::start(Launch::new(120, 30, fix.path()).panic_test());
    let ok = s.wait_exit(Duration::from_secs(15));
    assert_eq!(
        ok,
        Some(false),
        "the panic should end the process, unsuccessfully:\n{}",
        s.raw_text()
    );

    let raw = s.raw_text();
    assert!(
        raw.contains("HCMD_PANIC_TEST") && raw.contains("panicked at"),
        "the panic message has to be readable on the terminal:\n{raw:?}"
    );
    assert!(
        s.text().contains("panicked at"),
        "and readable on the *screen*, not just in the byte stream:\n{}",
        s.text()
    );
    assert!(
        !s.parser.screen().alternate_screen(),
        "a panic leaves the alternate screen"
    );

    let entered = raw.find(restore::ALT_ON);
    let left = raw.rfind(restore::ALT_OFF);
    let shown = raw.rfind(restore::CURSOR_SHOWN);
    assert!(
        entered.is_some(),
        "the panic happens after the alternate screen is entered:\n{raw:?}"
    );
    assert!(
        left > entered,
        "the panic hook left the alternate screen:\n{raw:?}"
    );
    // `Term::restore` shows the cursor only after `disable_raw_mode` returns,
    // and writes nothing afterwards - so this byte arriving last is the
    // observable proof that raw mode was turned off. Raw mode itself is a
    // termios flag and emits no sequence of its own.
    assert!(
        shown > left,
        "raw mode was disabled before the terminal was handed back:\n{raw:?}"
    );
    assert!(
        raw.rfind("panicked at") > shown,
        "the message is printed on a restored terminal, not a raw one:\n{raw:?}"
    );
}

#[test]
fn a_panic_with_the_viewer_open_still_gives_the_terminal_back() {
    // the restore rule against the viewer. The viewer is the
    // second state in which raw mode and the alternate screen are on, and it is
    // a *different* second state: it takes the whole screen, it is drawn before
    // the too-small check, and behind it sit an open file handle and a running
    // background index scan. `HCMD_PANIC_TEST=viewer` panics on the first frame
    // the viewer holds the screen, which is the only way to observe that path
    // from outside - there is no reachable input that panics, and that is the
    // point of the application rather than an omission from this test.
    let fix = Fixture::new("panic-viewer");
    let mut s = Session::start(Launch::new(120, 30, fix.path()).panic_test_in_viewer());
    s.wait_for_listing();

    // Quick search onto a real file, then `F3`. Not `..`, which is a directory
    // and would not open a viewer at all.
    s.send(b"alpha");
    s.settle();
    s.send(keys::F3);

    let ok = s.wait_exit(Duration::from_secs(15));
    assert_eq!(
        ok,
        Some(false),
        "the panic should end the process, unsuccessfully:\n{}",
        s.raw_text()
    );

    let raw = s.raw_text();
    assert!(
        raw.contains("panicked at") && raw.contains(term::PANIC_STAGE_VIEWER),
        "the panic happened in the viewer, not at start-up:\n{raw:?}"
    );
    assert!(
        !s.parser.screen().alternate_screen(),
        "a panic with the viewer open leaves the alternate screen"
    );
    assert!(
        s.text().contains("panicked at"),
        "and the message is readable on the screen:\n{}",
        s.text()
    );

    let entered = raw.find(restore::ALT_ON);
    let left = raw.rfind(restore::ALT_OFF);
    assert!(
        entered.is_some(),
        "the alternate screen was entered:\n{raw:?}"
    );
    assert!(left > entered, "the panic hook left it again:\n{raw:?}");
    // The *first* `Show` after leaving the alternate screen is the restore's,
    // and it has to come before the message. Unwinding past `Drop for Term`
    // shows the cursor a second time afterwards, which is why this is not an
    // `rfind`: idempotence is the point of `Term::restore`, so a second,
    // later copy of the sequence proves nothing either way.
    let shown = left.and_then(|at| {
        raw.get(at..)
            .and_then(|tail| tail.find(restore::CURSOR_SHOWN))
            .map(|i| i.saturating_add(at))
    });
    assert!(
        shown > left,
        "raw mode was disabled before the terminal was handed back:\n{raw:?}"
    );
    assert!(
        raw.find("panicked at") > shown,
        "the message is printed on a restored terminal, not a raw one:\n{raw:?}"
    );
}

// ---------------------------------------------------------------------------
// v0.7 (the last line), through the real binary
// ---------------------------------------------------------------------------

#[test]
fn ctrl_q_puts_a_file_in_the_other_panel_and_ctrl_q_again_puts_the_listing_back() {
    // "`Ctrl+Q` opens the viewer in the *other* panel, following
    // the active panel's cursor ... `Ctrl+Q` again gives the panel its listing
    // back."
    //
    // Through the binary rather than through `App`, because the two halves a
    // unit test cannot see are exactly the ones that matter: the debounce is
    // serviced by the event loop against a real clock, and the viewer is drawn
    // into a panel body whose geometry only the renderer knows.
    //
    let fix = Fixture::new("quickview");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    // Onto a file with known contents. `alpha.rs` is 11 bytes of the fixture's
    // own filler, so the assertion below is on the *header*, which names the
    // file the quick view is showing - the one thing that is true whatever the
    // bytes are.
    s.send(b"alpha");
    s.settle();

    s.press(keys::CTRL_Q, "the quick view to open", |t| {
        // The other panel's header carries the file's path instead of the
        // directory's, and its status row is the viewer's own.
        t.contains("alpha.rs") && t.contains("line 1/1")
    });

    s.press(keys::CTRL_Q, "the listing to come back", |t| {
        // Both panels list the fixture again, so `subdir` is on screen twice.
        t.matches("[subdir]").count() >= 2
    });
}

#[test]
fn f9_then_down_then_enter_runs_the_same_thing_the_items_key_runs() {
    // "every item on the menu bar is a key that already exists
    // and the menu shows that key beside the item". So walking to an item and
    // pressing `Enter` has to do what pressing that key does - the menu is a
    // second route to the same code, not a second copy of it.
    //
    //
    // `Mark` -> `Invert selection` is the item used, because it is observable
    // on the screen and starts nothing: `Files` -> `Edit` would spawn an
    // editor, and a test that hands the terminal to `nano` is a test that
    // hangs.
    let fix = Fixture::new("menu");

    // -- through the menu ---------------------------------------------------
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press(keys::F9, "the menu bar", |t| {
        t.contains("Files") && t.contains("Configuration")
    });
    // `Right` walks to `Mark`, whose second item is `Invert the marks`. The
    // row is read off the screen rather than assumed, so a reordered menu
    // fails here instead of silently testing a different item.
    s.press(keys::RIGHT, "the Mark menu", |t| {
        t.contains("Invert the marks")
    });
    s.send(keys::DOWN);
    s.settle();
    s.press(keys::ENTER, "the marks after the menu ran it", |t| {
        t.contains("selected")
    });
    let through_the_menu = s.text();

    // -- through the key ----------------------------------------------------
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();
    s.press(b"*", "the marks after the key ran it", |t| {
        t.contains("selected")
    });
    let through_the_key = s.text();

    // The same status line, from the same starting state, which is the whole
    // claim: one implementation behind two routes.
    let status = |screen: &str| -> String {
        screen
            .lines()
            .find(|line| line.contains("selected"))
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(
        status(&through_the_menu),
        status(&through_the_key),
        "F9 Right Down Enter must do what `*` does:\n{through_the_menu}"
    );
    assert!(
        !status(&through_the_key).is_empty(),
        "and it must do something:\n{through_the_key}"
    );
}

#[test]
fn f9_drops_the_bar_and_esc_gives_the_panel_back() {
    // the six menus, and its "Esc closes it and gives the panel
    // back". The bar takes focus while it is open, which is why the panel's
    // own keys do nothing until it is closed.
    let fix = Fixture::new("menu-esc");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    s.press(keys::F9, "the menu bar", |t| {
        ["Files", "Mark", "Commands", "Net", "Show", "Configuration"]
            .iter()
            .all(|title| t.contains(title))
    });
    s.press(keys::ESC, "the panel back", |t| t.contains("thunder"));

    // Focus really came back: a printable key is a quick search again.
    s.press(b"thu", "the quick search after Esc", |t| {
        t.contains("search: thu")
    });
}

#[test]
fn alt_f1_hangs_the_device_picker_under_the_left_panel_whichever_one_has_focus() {
    // "`Alt+F1` always targets the left panel and `Alt+F2` always
    // the right, independent of which panel has focus", and the
    // popup is "anchored under the target panel's header, so it is visually
    // obvious which side it will act on".
    //
    // The anchoring is the half a unit test cannot see: `crate::ui::dialog_area`
    // narrows the rectangle to one panel and `crate::dialog::dialog_rect`
    // top-left-aligns inside it, and only a rendered frame says whether the box
    // landed where the design asks.
    let fix = Fixture::new("drives");
    let mut s = Session::start(Launch::new(120, 30, fix.path()));
    s.wait_for_listing();

    // Focus the right panel first, so "independent of which panel has focus"
    // is actually being tested.
    s.send(b"\t");
    s.settle();

    s.press(keys::ALT_D, "the left panel's device picker", |t| {
        t.contains("Left panel drives")
    });
    // Anchored under the *left* panel: the title starts in the left half of
    // the screen, not centred across it.
    let title_column = s
        .text()
        .lines()
        .find_map(|line| line.find("Left panel drives"))
        .unwrap_or(usize::MAX);
    assert!(
        title_column < 60,
        "the popup hangs under the left panel, not centred:\n{}",
        s.text()
    );
    s.press(keys::ESC, "the panel back", |t| t.contains("thunder"));

    // And Alt+F2's is the right panel's, from the same focus.
    s.press(keys::ALT_G, "the right panel's device picker", |t| {
        t.contains("Right panel drives")
    });
    let title_column = s
        .text()
        .lines()
        .find_map(|line| line.find("Right panel drives"))
        .unwrap_or(0);
    assert!(
        title_column >= 60,
        "and that one hangs under the right panel:\n{}",
        s.text()
    );
    s.press(keys::ESC, "the panel back", |t| t.contains("thunder"));

    // Ctrl+D is the hotlist alone. Nothing has been added to
    // it, so it is an empty list rather than the devices-and-hotlist popup -
    // which is exactly the difference the two constructors draw.
    s.press(keys::CTRL_D, "the hotlist", |t| {
        t.contains("Directory hotlist") && !t.contains("panel drives")
    });
    s.press(keys::ESC, "the panel back", |t| t.contains("thunder"));
}
