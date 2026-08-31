//! The ten v0.2 acceptance criteria, driven through the real
//! `hcmd` binary.
//!
//! Same harness as `tests/acceptance.rs` - a pty, real key bytes, a `vt100`
//! screen - with one difference that is the whole point of this milestone:
//!
//! > **every criterion asserts on the filesystem as well as on the screen.**
//!
//! v0.2's output is files on disk. A copy dialog that says `copied 2 files` and
//! a destination directory that holds two byte-identical files with the
//! source's mode and mtime are two different claims, and only the second one is
//! the feature. So each test drives the keys, waits for the screen to say the
//! job finished, and then reads the bytes.
//!
//! Three things about this harness that `tests/acceptance.rs` does not need:
//!
//! * **`XDG_DATA_HOME` is a throwaway directory too.** the design puts `F8`
//!   in the XDG trash, and the freedesktop layout puts the home trash under
//!   `$XDG_DATA_HOME`. Pointing it at a temporary directory is what makes
//!   criterion 3 assertable *and* what keeps a test run out of the developer's
//!   own wastebasket.
//! * **[`Session::wait_now`] does not wait for the screen to settle.** A
//!   progress dialog repaints continuously, so [`Session::wait`]'s "unchanged
//!   for a beat" never comes true while a copy is running - and criterion 6 has
//!   to press `Esc` *during* one. `wait_now` polls the predicate on every pump
//!   instead.
//! * **Permission-dependent criteria probe first.** Running as root defeats the
//!   mode bits entirely, so criteria 9 and 10 check that a mode-555 directory
//!   really does refuse this process before they assert that the product
//!   noticed. Where it does not, the test says so loudly rather than passing on
//!   a premise that was not true.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Keeps concurrently running tests off each other's temporary directories.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Key encodings, verified against crossterm 0.29's
/// `src/event/sys/unix/parse.rs`.
///
/// The `CSI <n> ~` family is `parse_csi_special_key_code`: `11..=15` are `F1`
/// to `F5` and `17..=21` are `F6` to `F10`, so `F5` is `15` and `F8` is `19`.
/// A modifier goes in a second field and is a bitmask offset by one - `;2` is
/// Shift - which is what makes `Shift+F8` `ESC [ 19 ; 2 ~`.
///
/// The `CSI <codepoint> ; <modifier> u` family is the Kitty protocol's, and the
/// codepoint is the key's *unshifted* Unicode value: `Ctrl+L` is `108` (`'l'`).
/// crossterm parses these whether or not the flags were pushed, so they can be
/// injected directly (`HCMD_KEYBOARD_PROTOCOL=enhanced` is set regardless,
/// because the binary decides its own encoding from it).
mod keys {
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const ENTER: &[u8] = b"\r";
    pub const ESC: &[u8] = b"\x1b";
    pub const TAB: &[u8] = b"\t";
    pub const BACKSPACE: &[u8] = b"\x7f";
    pub const SPACE: &[u8] = b" ";
    pub const INSERT: &[u8] = b"\x1b[2~";

    pub const F5: &[u8] = b"\x1b[15~";
    pub const F6: &[u8] = b"\x1b[17~";
    pub const F7: &[u8] = b"\x1b[18~";
    pub const F8: &[u8] = b"\x1b[19~";
    /// `Shift+F8` - the permanent delete.
    pub const SHIFT_F8: &[u8] = b"\x1b[19;2~";

    /// `Ctrl+G` - go to a path, which is how a test points the
    /// other panel somewhere without a command line.
    pub const CTRL_G: &[u8] = b"\x1b[103;5u";
    /// `Ctrl+L` - calculate the occupied space of the selection.
    pub const CTRL_L: &[u8] = b"\x1b[108;5u";

    /// `+` and `-`: the mask prompts.
    pub const PLUS: &[u8] = b"+";
    pub const MINUS: &[u8] = b"-";
}

/// The theme colours the assertions read back out of the rendered cells.
///
/// A cursor bar and a marked entry are *styles*, invisible in the screen's
/// plain text, so reading the cell colours is the only honest way to ask "which
/// row is the cursor on" and "which entries are marked". These are the
/// compiled-in defaults, which `themes/blue.toml` mirrors, and
/// `COLORTERM=truecolor` keeps them unquantised.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
    /// `panel.marked_fg` - a marked entry, whole row.
    pub const MARKED_FG: (u8, u8, u8) = (0xFF, 0xFF, 0x54);
}

// ---------------------------------------------------------------------------
// The fixture tree
// ---------------------------------------------------------------------------

/// A `<root>/src` and `<root>/dst` pair, built per test and removed on drop.
///
/// Every criterion is "put these there", so a source directory and a
/// destination directory is the shape of all ten. Tests add whatever they need
/// under either.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-ops-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = relax_and_remove(&root);
        fs::create_dir_all(root.join("src")).expect("fixture src");
        fs::create_dir_all(root.join("dst")).expect("fixture dst");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn src(&self) -> PathBuf {
        self.root.join("src")
    }

    fn dst(&self) -> PathBuf {
        self.root.join("dst")
    }

    /// Write a file under the root, creating its parents.
    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, bytes).expect("fixture file");
        path
    }

    /// Make a directory under the root.
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).expect("fixture dir");
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = relax_and_remove(&self.root);
    }
}

/// Remove a tree that may contain deliberately unreadable directories.
///
/// Criteria 9 and 10 chmod things to 0 on purpose; `remove_dir_all` cannot
/// enter those, so every directory gets its bits back on the way down.
fn relax_and_remove(root: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(root).is_err() {
        return Ok(());
    }
    relax(root);
    fs::remove_dir_all(root)
}

fn relax(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    if !meta.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        relax(&entry.path());
    }
}

// ---------------------------------------------------------------------------
// Filesystem assertions
// ---------------------------------------------------------------------------

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .mode()
        & 0o7777
}

fn mtime_of(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .modified()
        .unwrap_or_else(|e| panic!("mtime {}: {e}", path.display()))
}

fn inode_of(path: &Path) -> u64 {
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .ino()
}

fn device_of(path: &Path) -> u64 {
    fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .dev()
}

/// The names directly inside a directory, sorted. `[]` for one that is not
/// there at all, which is what "nothing was created" looks like.
fn listing(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every regular file under `dir`, recursively, as paths.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(_) => out.push(path),
                Err(_) => {}
            }
        }
    }
    out
}

/// "must leave no half-written destination".
///
/// The implementation writes every regular file to `.<name>.hcmd-part` beside
/// its destination and renames it into place when it is complete,
/// so a leftover partial is exactly what a
/// half-written destination looks like on disk.
fn assert_no_partials(dir: &Path) {
    let partials: Vec<String> = walk_files(dir)
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| n.contains("hcmd-part"))
        .collect();
    assert!(
        partials.is_empty(),
        "a half-written destination was left behind: {partials:?}"
    );
}

/// Does a mode-555 directory actually stop this process writing into it?
///
/// Running as root it does not, and criteria 9 and 10 are then asserting
/// against a premise that is false. Rather than guess at the uid, ask the
/// filesystem the same question the product will ask it.
fn permissions_bite(near: &Path) -> bool {
    let probe = near.join(format!(
        ".perm-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&probe);
    if fs::create_dir(&probe).is_err() {
        return false;
    }
    let _ = fs::set_permissions(&probe, fs::Permissions::from_mode(0o555));
    let blocked = fs::write(probe.join("x"), b"x").is_err();
    let _ = fs::set_permissions(&probe, fs::Permissions::from_mode(0o755));
    let _ = fs::remove_dir_all(&probe);
    blocked
}

// ---------------------------------------------------------------------------
// The pty session
// ---------------------------------------------------------------------------

/// How to start one `hcmd`.
struct Launch {
    cols: u16,
    rows: u16,
    cwd: PathBuf,
}

impl Launch {
    fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cols: 120,
            rows: 30,
            cwd: cwd.into(),
        }
    }
}

/// A running `hcmd`, the screen it has painted, and the throwaway `$HOME`-ish
/// directories it was given.
struct Session {
    #[expect(dead_code, reason = "held so the pty outlives the reader thread")]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    home: PathBuf,
}

impl Session {
    fn start(launch: Launch) -> Self {
        let Launch { cols, rows, cwd } = launch;

        let home = std::env::temp_dir().join(format!(
            "hcmd-ops-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("holoscommander")).expect("throwaway config dir");

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
        // machine: criterion 8 reads `≥` and the row splitting reads `│`.
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        cmd.env("HCMD_KEYBOARD_PROTOCOL", "enhanced");
        cmd.env("XDG_CONFIG_HOME", &home);
        cmd.env("XDG_STATE_HOME", &home);
        // the `F8`. The freedesktop layout puts the home trash at
        // `$XDG_DATA_HOME/Trash`, so this is both how criterion 3 finds what was
        // trashed and how a test run stays out of the developer's own trash.
        cmd.env("XDG_DATA_HOME", &home);
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
            home,
        }
    }

    /// The throwaway `$XDG_DATA_HOME`, and therefore the trash's parent.
    fn data_home(&self) -> &Path {
        &self.home
    }

    // -- reading -----------------------------------------------------------

    fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.parser.process(&chunk),
                Err(_) => return,
            }
        }
    }

    fn text(&self) -> String {
        self.parser.screen().contents()
    }

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
        let deadline = Instant::now() + Duration::from_secs(20);
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
        let deadline = Instant::now() + Duration::from_secs(60);
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

    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        self.wait(what, |s| pred(&s.text()));
    }

    /// Wait for a predicate **without** waiting for the screen to settle.
    ///
    /// A progress dialog repaints ten times a second, so [`Session::settle`]
    /// never returns while a job is running and [`Session::wait`] would only
    /// look once the copy had finished - which is precisely the moment
    /// criterion 6 must not be at. This one checks after every pump, so a key
    /// can be sent *during* an operation.
    fn wait_now(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.pump(Duration::from_millis(20));
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting (unsettled) for {what}\n--- screen ---\n{}",
                    self.text()
                );
            }
        }
    }

    // -- writing -----------------------------------------------------------

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    fn press(&mut self, bytes: &[u8], what: &str, pred: impl Fn(&str) -> bool) {
        self.send(bytes);
        self.wait_for(what, pred);
    }

    /// Type a string one byte at a time, then let the screen catch up.
    fn type_text(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.settle();
    }

    /// Walk the **left** panel's cursor onto the entry whose row contains
    /// `needle`.
    ///
    /// Deliberately arrow keys and not a quick search: the design binds `-`
    /// to the unmark-by-mask prompt, so a filename cannot be typed at a panel
    /// without risking a dialog, and this has to work for any name.
    ///
    /// It goes to `[..]` first so the direction is never wrong, and checks the
    /// painted cursor bar between steps - cursor movement changes no text, so
    /// "the screen settled" alone would let the next key act on the wrong row.
    fn focus_entry(&mut self, needle: &str) {
        for _ in 0..80 {
            self.settle();
            assert!(
                self.cursor_is_left(),
                "the left panel does not have focus:\n{}",
                self.text()
            );
            if self.cursor_row().contains("[..]") {
                break;
            }
            self.send(keys::UP);
        }
        for _ in 0..80 {
            self.settle();
            if self.cursor_row().contains(needle) {
                return;
            }
            self.send(keys::DOWN);
        }
        panic!(
            "the cursor never reached {needle:?}\n--- screen ---\n{}",
            self.text()
        );
    }

    /// Wait for the conflict dialog about one destination.
    ///
    /// The dialog names the destination that is in the way, which is what makes
    /// "asked about this one" checkable: `dst/<name>` appears nowhere else on
    /// screen - the panel headers carry the directory, and the progress
    /// dialog's own line carries the *source*.
    fn wait_for_conflict(&mut self, name: &str) {
        let needle = format!("dst/{name}");
        self.wait(&format!("the conflict dialog for {name}"), move |s| {
            let text = s.text();
            text.contains("File exists") && text.contains(&needle)
        });
    }

    /// Is the focused cursor bar painted in the left panel?
    ///
    /// Both panels draw a cursor at all times and on a fresh
    /// listing both sit on `[..]`, so the text cannot tell them apart. The
    /// column can: the two panels split the screen down the middle.
    fn cursor_is_left(&self) -> bool {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let rows = self.cmdline_row();
        let want = rgb(paint::CURSOR_FOCUSED);
        for row in 0..rows {
            for col in 0..cols {
                if screen
                    .cell(row, col)
                    .is_some_and(|cell| cell.bgcolor() == want)
                {
                    return col < cols / 2;
                }
            }
        }
        false
    }

    // -- reading the painted cells ----------------------------------------

    /// The row index of the command line's caret. Everything the panels draw
    /// is above it; the key bar is below.
    ///
    /// Found structurally, not by looking for the `<cwd>> ` this application
    /// used to compose: the design made the command line the **shell's** own
    /// input line, so its prompt is whatever the user's shell draws - and need
    /// not contain a `>` at all, while `<DIR>` in the size column does.
    fn cmdline_row(&self) -> u16 {
        let (rows, _) = self.parser.screen().size();
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
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

    /// The entry the focused panel's cursor bar is sitting on.
    fn cursor_row(&self) -> String {
        self.row_with_bg(paint::CURSOR_FOCUSED).unwrap_or_default()
    }

    /// The trimmed text of the row painted in `bg`.
    ///
    /// Only the rows above the command line are searched: the key bar paints
    /// its labels in the same `#00A8A8` as `panel.cursor_bg`, so a scan of the
    /// whole screen would report the key bar as a cursor bar.
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

    /// The left panel's entries that are **marked**: `panel.marked_fg` as the
    /// whole row's foreground, or - for a marked row under the cursor, which
    /// keeps the cursor bar - the underline the mark takes there instead.
    ///
    /// A mark is a style and not a glyph, so this is the only honest way to ask
    /// which entries a mask caught. The scan starts below the column header,
    /// which is painted in the same colour, and stops at the first row whose
    /// left half is blank - the end of the listing.
    fn marked_left_entries(&self) -> Vec<String> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let last = self.cmdline_row();
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        let Some(header) = lines.iter().position(|line| line.contains("Name")) else {
            return Vec::new();
        };
        let border = usize::from(cols) / 2;
        let want = rgb(paint::MARKED_FG);
        let mut out = Vec::new();
        for row in (header + 1)..usize::from(last) {
            let Some(line) = lines.get(row) else { break };
            if left_cell(line).trim().is_empty() {
                break;
            }
            let mut marked = String::new();
            for col in 0..border.min(usize::from(cols)) {
                let Some(cell) = screen.cell(
                    u16::try_from(row).unwrap_or(u16::MAX),
                    u16::try_from(col).unwrap_or(u16::MAX),
                ) else {
                    continue;
                };
                if cell.fgcolor() == want || cell.underline() {
                    marked.push_str(cell.contents());
                }
            }
            let marked = marked.trim().to_string();
            if !marked.is_empty() {
                out.push(marked);
            }
        }
        out
    }

    // -- driving -----------------------------------------------------------

    /// Point the **other** panel at `dir` with `Ctrl+G`, then come back.
    ///
    /// the design pre-fills the copy dialog's target with "the other panel's
    /// path", so half of this milestone needs two panels pointing at different
    /// places, and `Ctrl+G` is the keyboard route to that.
    /// Returning to the left panel is deliberately not waited on here - the
    /// caller's first `press_to` waits for the left cursor to land on a name
    /// only the left panel has, which cannot come true until the `Tab` has been
    /// processed.
    fn point_other_panel_at(&mut self, dir: &Path) {
        let shown = dir.display().to_string();
        // By its last component: a long enough path is painted elided, so
        // waiting for the whole of it hangs on a machine whose only
        // peculiarity is a long temporary directory (macOS spells $TMPDIR as
        // /var/folders/<22 chars>/T/).
        let leaf = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| shown.clone());
        self.send(keys::TAB);
        self.press(keys::CTRL_G, "the Go to prompt", |t| t.contains("Go to"));
        self.type_text(&shown);
        let typed = leaf.clone();
        self.wait("the destination in the Go to prompt", move |s| {
            s.text().contains(&typed)
        });
        self.send(keys::ENTER);
        self.wait("the Go to prompt to close on the destination", move |s| {
            let t = s.text();
            !t.contains("Go to") && t.contains(&leaf)
        });
        self.send(keys::TAB);
        self.wait("the left panel to take focus back", Self::cursor_is_left);
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// The left panel's half of one screen row.
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

/// The panel status line's spelling of a byte count.
///
/// `panel.human_sizes` is off by default, so it is kibibytes rounded up with a
/// `k`, grouped by `panel.thousands_separator`. Written out here rather than
/// imported, because a test that shares the product's arithmetic cannot catch
/// the product getting the arithmetic wrong.
fn status_k(bytes: u64) -> String {
    let digits = bytes.div_ceil(1024).to_string();
    let leading = match digits.len() % 3 {
        0 => 3,
        n => n,
    };
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i >= leading && (i - leading) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    let _ = write!(out, " k");
    out
}

// ---------------------------------------------------------------------------
// 1. F5 copies a marked set, byte for byte, with mode and mtime
// ---------------------------------------------------------------------------

#[test]
fn criterion_1_f5_copies_the_marked_set_with_its_mode_and_mtime() {
    let t = Tree::new("c1");
    let alpha = t.file("src/alpha.rs", b"alpha payload, thirty-one bytes");
    let beta = t.file("src/beta.rs", b"beta");
    let gamma = t.file("src/gamma.txt", b"gamma must stay behind");

    // Distinct, non-default modes and a distinctly old mtime, so "preserved"
    // cannot be confused with "whatever the destination happened to get".
    fs::set_permissions(&alpha, fs::Permissions::from_mode(0o640)).expect("chmod alpha");
    fs::set_permissions(&beta, fs::Permissions::from_mode(0o600)).expect("chmod beta");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for path in [&alpha, &beta] {
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_times");
        file.set_times(fs::FileTimes::new().set_modified(old).set_accessed(old))
            .expect("set_times");
    }
    let want_mode = [mode_of(&alpha), mode_of(&beta)];
    let want_mtime = [mtime_of(&alpha), mtime_of(&beta)];

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| {
        t.contains("alpha") && t.contains("gamma")
    });
    s.point_other_panel_at(&t.dst());

    // Mark alpha and beta with Insert, which marks and steps down - and never
    // sizes anything, so nothing else is running.
    s.focus_entry("alpha");
    s.send(keys::INSERT);
    s.press(keys::INSERT, "two entries marked", |t| {
        t.contains("2 of 3 selected")
    });

    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 2 file(s)")
    });
    s.press(keys::ENTER, "the copy to finish", |t| {
        t.contains("copied 2 files")
    });

    // The screen said so; the disk is the claim.
    assert_eq!(
        listing(&t.dst()),
        vec!["alpha.rs".to_string(), "beta.rs".to_string()],
        "F5 copied the marked set and only the marked set"
    );
    assert!(
        !t.dst().join("gamma.txt").exists(),
        "the unmarked file must not have been copied"
    );
    assert_no_partials(&t.dst());

    for (i, name) in ["alpha.rs", "beta.rs"].iter().enumerate() {
        let src = t.src().join(name);
        let dst = t.dst().join(name);
        assert_eq!(read(&dst), read(&src), "{name} arrived byte for byte");
        assert_eq!(
            mode_of(&dst),
            want_mode[i],
            "{name} kept its mode (the preservation)"
        );
        assert_eq!(
            mtime_of(&dst),
            want_mtime[i],
            "{name} kept its mtime (the preservation)"
        );
    }

    // A copy leaves the sources where they are.
    assert!(alpha.exists() && beta.exists() && gamma.exists());
}

// ---------------------------------------------------------------------------
// 2. F6 moves: a rename on one device, copy-then-delete across two
// ---------------------------------------------------------------------------

#[test]
fn criterion_2_f6_renames_on_one_device_and_copies_across_two() {
    let t = Tree::new("c2");
    let one = t.file("src/one.txt", b"one, to be moved");
    let before = inode_of(&one);

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("one"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("one");

    s.press(keys::F6, "the move dialog", |t| {
        t.contains("Rename/Move 1 file(s)")
    });
    s.press(keys::ENTER, "the move to finish", |t| {
        t.contains("moved 1 file")
    });

    let moved = t.dst().join("one.txt");
    assert_eq!(read(&moved), b"one, to be moved");
    assert!(!one.exists(), "a move removes the source");
    // The inode is what tells a rename from a copy: `rename(2)` keeps it and a
    // copy-then-delete cannot. the design only *degrades* to copy-then-delete
    // across a device, so on one device this must be the cheap path.
    assert_eq!(
        inode_of(&moved),
        before,
        "a same-device move is a rename, not a copy"
    );

    // ---- and now across a device boundary --------------------------------
    //
    // Not simulated: `/dev/shm` is a second tmpfs on any ordinary Linux system,
    // so `rename(2)` between it and `/tmp` really does return `EXDEV` and the
    // product really does take the degraded path. Where that is not true, the
    // test says so rather than asserting something it did not exercise.
    let far_root = PathBuf::from("/dev/shm").join(format!(
        "hcmd-ops-xdev-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if fs::create_dir_all(&far_root).is_err() {
        // On Linux this is a real failure: /dev/shm is a second tmpfs on any
        // ordinary system, and its absence means the degraded path went
        // untested on a machine where it could have been tested. macOS has no
        // equivalent that is always mounted - a second filesystem there means
        // building a disk image - so the gap is reported rather than pretended
        // away, and rather than failing a run over a platform's layout.
        if cfg!(target_os = "linux") {
            panic!(
                "cannot create {} - the cross-device half was not exercised \
                 on this machine",
                far_root.display()
            );
        }
        eprintln!(
            "no second filesystem at {}; skipping the cross-device half",
            far_root.display()
        );
        return;
    }
    let cross_device = device_of(&far_root) != device_of(t.root());
    assert!(
        cross_device,
        "{} and {} are on the same filesystem, so the cross-device half of \
         the design cannot be exercised here",
        far_root.display(),
        t.root().display()
    );

    let two = t.file("src/two.txt", b"two, across a device boundary");
    let two_inode = inode_of(&two);
    // A second source that cannot be read. It is what proves the *ordering* in
    // "the delete happening only after a successful copy": a copy that failed
    // must leave its source exactly where it was.
    let locked = t.file("src/locked.txt", b"locked, must survive");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod locked");
    let bites = permissions_bite(t.root());

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the second source listing", |t| t.contains("two"));
    s.point_other_panel_at(&far_root);
    s.focus_entry("locked");
    s.send(keys::INSERT);
    s.press(keys::INSERT, "both sources marked", |t| {
        t.contains("2 of 2 selected")
    });
    s.press(keys::F6, "the cross-device move dialog", |t| {
        t.contains("Rename/Move 2 file(s)")
    });
    s.send(keys::ENTER);
    if bites {
        s.wait_for("the failure summary", |t| t.contains("Retry failures"));
    } else {
        s.wait_for("the cross-device move to finish", |t| {
            t.contains("moved 2 files")
        });
    }

    let landed = far_root.join("two.txt");
    assert_eq!(
        read(&landed),
        b"two, across a device boundary",
        "the cross-device move copied every byte"
    );
    assert_ne!(
        inode_of(&landed),
        two_inode,
        "a cross-device move cannot be a rename; it degraded to a copy"
    );
    assert!(
        !two.exists(),
        "the source is removed once the copy succeeded"
    );
    assert_no_partials(&far_root);

    if bites {
        assert!(
            locked.exists(),
            "the source of a copy that FAILED is not deleted - the \
             \"only after a successful copy\""
        );
        assert!(
            !far_root.join("locked.txt").exists(),
            "and nothing of it reached the destination"
        );
    } else {
        eprintln!(
            "criterion 2: this process can read a mode-000 file, so the \
             \"delete only after a successful copy\" half was not exercised"
        );
    }

    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));
    let _ = fs::remove_dir_all(&far_root);
}

// ---------------------------------------------------------------------------
// 3. F8 trashes; Shift+F8 unlinks after a confirmation naming the count
// ---------------------------------------------------------------------------

#[test]
fn criterion_3_f8_trashes_and_shift_f8_unlinks_after_naming_the_count() {
    let t = Tree::new("c3");
    let doomed = t.file("src/doomed.txt", b"into the trash");
    t.file("src/goneone.txt", b"1");
    t.file("src/gonetwo.txt", b"2");

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("doomed"));

    // `F8`: the confirmation names what is going, and its affirmative is the
    // default button, so Enter answers it.
    s.focus_entry("doomed");
    s.press(keys::F8, "the trash confirmation", |t| {
        t.contains("Move to the trash") && t.contains("doomed.txt")
    });
    s.press(keys::ENTER, "the trash job to finish", |t| {
        t.contains("trashed 1 file")
    });

    assert!(!doomed.exists(), "F8 took the file out of the directory");
    // ...and it is findable in the XDG trash, which is the half that makes F8
    // different from Shift+F8. The freedesktop layout is `files/` beside
    // `info/` under `$XDG_DATA_HOME/Trash`.
    //
    // ...where there is a freedesktop trash to look in. macOS trashes through
    // NSFileManager, which honours neither `$XDG_DATA_HOME` nor the
    // `files/`-beside-`info/` layout and offers no way to enumerate what it
    // holds - which is why `ops::delete` reports the trash as unenumerable
    // there. The `Shift+F8` half below is asked on every platform, because
    // unlinking is the same everywhere.
    let trash = s.data_home().join("Trash");
    let trashed: Vec<String> = if cfg!(target_os = "macos") {
        eprintln!("this platform's trash cannot be listed; F8's removal was still checked");
        Vec::new()
    } else {
        let trashed = listing(&trash.join("files"));
        assert!(
            trashed.iter().any(|n| n.contains("doomed")),
            "the file is in the XDG trash at {}: {trashed:?}\n--- screen ---\n{}",
            trash.display(),
            s.text()
        );
        let info = listing(&trash.join("info"));
        assert!(
            info.iter().any(|n| n.ends_with(".trashinfo")),
            "with its freedesktop trashinfo record: {info:?}"
        );
        trashed
    };

    // `Shift+F8`: unlinks, and its confirmation names the count. Its default
    // button is `Cancel` - nothing gets this back - so the affirmative has to
    // be pressed on purpose.
    s.focus_entry("goneone");
    s.send(keys::INSERT);
    s.press(keys::INSERT, "two entries marked", |t| {
        t.contains("2 of 2 selected")
    });
    s.press(keys::SHIFT_F8, "the permanent-delete confirmation", |t| {
        t.contains("Delete permanently: 2 selected items?")
    });
    s.press(b"y", "the delete to finish", |t| {
        t.contains("deleted 2 files")
    });

    assert_eq!(
        listing(&t.src()),
        Vec::<String>::new(),
        "Shift+F8 unlinked both files"
    );
    if !cfg!(target_os = "macos") {
        let after = listing(&trash.join("files"));
        assert_eq!(
            after.len(),
            trashed.len(),
            "and put nothing in the trash on the way: {after:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. F7 creates a directory and the cursor lands on it
// ---------------------------------------------------------------------------

#[test]
fn criterion_4_f7_creates_a_directory_and_the_cursor_lands_on_it() {
    let t = Tree::new("c4");
    // Two neighbours the new directory sorts between, so "the cursor is on it"
    // cannot be satisfied by the cursor simply not moving.
    t.dir("src/aaafirst");
    t.dir("src/zzzlast");
    t.file("src/afile.txt", b"x");

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("[zzzlast]"));

    s.press(keys::F7, "the create-directory prompt", |t| {
        t.contains("Create directory")
    });
    s.type_text("mmmnew");
    s.press(keys::ENTER, "the directory to appear in the listing", |t| {
        t.contains("[mmmnew]")
    });

    let made = t.src().join("mmmnew");
    assert!(made.is_dir(), "F7 created the directory (the mkdir)");
    assert!(
        s.cursor_row().contains("mmmnew"),
        "and the cursor landed on it after the re-read, got {:?}:\n{}",
        s.cursor_row(),
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 5. A collision prompts; skip, overwrite, and an "all" that is asked once
// ---------------------------------------------------------------------------

/// Four names that collide, in the order the panel lists them and therefore in
/// the order the conflicts arrive.
const COLLIDING: [&str; 4] = ["aa.txt", "bb.txt", "cc.txt", "dd.txt"];

/// Set up a `src`/`dst` pair in which every one of [`COLLIDING`] is present on
/// both sides with different contents, so a destination that still says `dest`
/// was not written and one that says `source` was.
fn colliding_tree(tag: &str, names: &[&str]) -> Tree {
    let t = Tree::new(tag);
    for name in names {
        t.file(&format!("src/{name}"), format!("source {name}").as_bytes());
        t.file(&format!("dst/{name}"), format!("dest {name}").as_bytes());
    }
    t
}

fn dest_bytes(name: &str) -> Vec<u8> {
    format!("dest {name}").into_bytes()
}

fn source_bytes(name: &str) -> Vec<u8> {
    format!("source {name}").into_bytes()
}

#[test]
fn criterion_5_a_collision_prompts_and_an_all_answer_is_asked_once() {
    // ---- one decision per file: skip leaves it, overwrite replaces it -----
    let t = colliding_tree("c5a", &COLLIDING);
    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("dd"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("aa");
    for _ in 0..COLLIDING.len() {
        s.send(keys::INSERT);
    }
    s.wait_for("all four marked", |t| t.contains("4 of 4 selected"));

    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 4 file(s)")
    });
    s.send(keys::ENTER);

    // `aa`: skip. The dialog opens on `Skip` already, but the answer is pressed
    // by its accelerator so the test is not asserting the default by accident.
    s.wait_for_conflict("aa.txt");
    s.send(b"s");
    // `bb`: overwrite.
    s.wait_for_conflict("bb.txt");
    s.send(b"o");
    // `cc`: "apply to all remaining conflicts", then skip. `dd` must not be
    // asked - "Decisions apply for the remainder of the batch".
    s.wait_for_conflict("cc.txt");
    s.send(b"a");
    s.settle();
    s.send(b"s");

    s.wait_for("the batch to finish without asking again", |t| {
        t.contains("copied 1 file, 0 dirs; 3 skipped")
    });

    assert_eq!(
        read(&t.dst().join("aa.txt")),
        dest_bytes("aa.txt"),
        "skip left the destination untouched"
    );
    assert_eq!(
        read(&t.dst().join("bb.txt")),
        source_bytes("bb.txt"),
        "overwrite replaced it"
    );
    assert_eq!(
        read(&t.dst().join("cc.txt")),
        dest_bytes("cc.txt"),
        "the `all` answer applied to the file it was given on"
    );
    assert_eq!(
        read(&t.dst().join("dd.txt")),
        dest_bytes("dd.txt"),
        "and to the rest of the batch, which was never asked about"
    );
    assert_no_partials(&t.dst());

    // ---- and the same for an "all" that overwrites ------------------------
    //
    // The proof that it was asked only once is that the job *finished*: an
    // unanswered second conflict parks the worker, and the wait below would
    // time out on a progress dialog that never completes.
    let t = colliding_tree("c5b", &COLLIDING[..3]);
    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("cc"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("aa");
    for _ in 0..3 {
        s.send(keys::INSERT);
    }
    s.wait_for("all three marked", |t| t.contains("3 of 3 selected"));
    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 3 file(s)")
    });
    s.send(keys::ENTER);

    s.wait_for_conflict("aa.txt");
    s.send(b"a");
    s.settle();
    s.send(b"o");
    s.wait_for("the batch to finish on one answer", |t| {
        t.contains("copied 3 files")
    });

    for name in &COLLIDING[..3] {
        assert_eq!(
            read(&t.dst().join(name)),
            source_bytes(name),
            "`overwrite` + `apply to all` replaced {name} without asking again"
        );
    }
    assert_no_partials(&t.dst());
}

// ---------------------------------------------------------------------------
// 6. Esc mid-copy cancels and leaves no half-written destination
// ---------------------------------------------------------------------------

/// How many files the cancelled batch holds.
///
/// It has to be large enough that the copy is still running by the time `Esc`
/// has crossed the pty, and small enough that building it is not the slowest
/// thing in the suite. Measured here, the cancel lands after about 250 files -
/// six tenths of one per cent of the batch - so the margin is two orders of
/// magnitude, not a coin toss.
///
/// The test cannot quietly pass if the batch finishes first: it waits for a
/// progress dialog that is *partway through*, and a batch that never showed one
/// times out rather than skipping the assertions.
const CANCEL_FILES: usize = 40_000;

/// Every file in that batch, so a survivor can be checked for completeness.
const CANCEL_PAYLOAD: &[u8] = b"sixty-four bytes of payload, repeated in every one of the files.";

#[test]
fn criterion_6_esc_mid_copy_cancels_and_leaves_no_partial() {
    let t = Tree::new("c6");
    let payload = t.dir("src/payload");
    for i in 0..CANCEL_FILES {
        fs::write(payload.join(format!("f{i:06}.dat")), CANCEL_PAYLOAD).expect("fixture payload");
    }

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("[payload]"));
    s.point_other_panel_at(&t.dst());
    s.focus_entry("payload");

    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 1 file(s)")
    });
    s.send(keys::ENTER);

    // Partway through, and not merely started: the counts line reads
    // `<done> / <total> files`, so a non-zero `done` against
    // the full total is the moment this criterion is about. `wait_now` is what
    // makes that reachable - a repainting progress dialog never settles.
    let total = CANCEL_FILES.to_string();
    s.wait_now("the copy to be partway through", move |s| {
        let text = s.text();
        progress_files_done(&text, &total).is_some_and(|done| done > 100)
    });
    s.send(keys::ESC);

    s.wait_for("the cancelled copy to report", |t| t.contains("cancelled"));

    let landed = t.dst().join("payload");
    let copied = walk_files(&landed);
    assert!(
        copied.len() < CANCEL_FILES,
        "the copy was cancelled, so it cannot have finished: {} of {CANCEL_FILES}",
        copied.len()
    );
    assert!(
        !copied.is_empty(),
        "...and it had genuinely started, so this is a cancel mid-copy"
    );
    // "must leave no half-written destination". Two halves -
    // no leftover partial, and nothing that arrived is short.
    assert_no_partials(&t.dst());
    for path in &copied {
        let bytes = read(path);
        assert_eq!(
            bytes.len(),
            CANCEL_PAYLOAD.len(),
            "{} is half written",
            path.display()
        );
        assert_eq!(bytes, CANCEL_PAYLOAD, "{} is corrupt", path.display());
    }
    // A cancelled copy is still a copy: the sources are all where they were.
    assert_eq!(walk_files(&payload).len(), CANCEL_FILES);
    assert!(s.is_running(), "the application survived the cancel");
}

/// Read `<done> / <total> files` out of a rendered progress dialog.
fn progress_files_done(screen: &str, total: &str) -> Option<u64> {
    let needle = format!(" / {total} files");
    let at = screen.find(&needle)?;
    let head = screen.get(..at)?;
    let digits: String = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// 7. `+` and `-` mark by mask, and the mask is remembered
// ---------------------------------------------------------------------------

#[test]
fn criterion_7_plus_and_minus_mark_by_mask_and_remember_it() {
    let t = Tree::new("c7");
    t.file("src/alpha.rs", b"rs one");
    t.file("src/beta.rs", b"rs two");
    t.file("src/notes.txt", b"not rs");
    t.file("src/readme.md", b"not rs either");
    t.dir("src/things");

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| {
        t.contains("readme") && t.contains("[things]")
    });

    // `+` opens the prompt on the default of `*`, caret at the
    // end. Backspace over it and type the mask.
    s.press(keys::PLUS, "the select-by-mask prompt", |t| {
        t.contains("Select by mask") && t.contains("Mark files matching:")
    });
    s.send(keys::BACKSPACE);
    s.type_text("*.rs");
    s.press(keys::ENTER, "the mask to be applied", |t| {
        t.contains("*.rs: 2 entries marked")
    });

    let marked = s.marked_left_entries();
    assert_eq!(
        marked.len(),
        2,
        "`*.rs` marked exactly two entries, got {marked:#?}\n{}",
        s.text()
    );
    assert!(
        marked.iter().any(|row| row.contains("alpha"))
            && marked.iter().any(|row| row.contains("beta")),
        "...and they are the two .rs files, got {marked:#?}"
    );
    assert!(
        !marked.iter().any(|row| row.contains("notes"))
            && !marked.iter().any(|row| row.contains("readme"))
            && !marked.iter().any(|row| row.contains("things")),
        "...and nothing else, got {marked:#?}"
    );
    // The status line is showing the mask's own verdict; the next key clears
    // it and the panel goes back to reporting the selection.
    s.send(keys::DOWN);
    s.wait_for("the selection count", |t| t.contains("2 of 5 selected"));

    // `-` opens the same prompt in the other direction, offering **the same
    // mask** - "One remembered mask, not one per direction."
    s.press(keys::MINUS, "the unselect-by-mask prompt", |t| {
        t.contains("Unselect by mask") && t.contains("Unmark files matching:")
    });
    assert!(
        s.text().contains("*.rs"),
        "the last mask is offered as the default:\n{}",
        s.text()
    );
    s.press(keys::ENTER, "the mask to be un-applied", |t| {
        t.contains("*.rs: 2 entries unmarked")
    });

    assert!(
        s.marked_left_entries().is_empty(),
        "`-` unmarked them:\n{}",
        s.text()
    );
    s.send(keys::DOWN);
    s.wait_for("the directory report", |t| t.contains("in 4 files, 1 dir"));
    assert!(
        !s.text().contains(" selected"),
        "and the status line went back to reporting the directory:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 8. Space sizes one directory; a marked one is a lower bound until Ctrl+L
// ---------------------------------------------------------------------------

/// The two trees criterion 8 measures, as `(name, file sizes)`.
///
/// Nested on purpose: the design says `Space` "walks it to full depth", and a
/// flat directory would be measured correctly by a walk that never recursed.
const SIZED: [(&str, &[usize]); 2] = [("treeone", &[5_000, 700, 30_000]), ("treetwo", &[9_000])];

fn build_sized(t: &Tree) -> [u64; 2] {
    let mut totals = [0u64; 2];
    for (i, (name, sizes)) in SIZED.iter().enumerate() {
        for (n, size) in sizes.iter().enumerate() {
            // One level down for the first file, two for the rest.
            let rel = if n == 0 {
                format!("src/{name}/file{n}.bin")
            } else {
                format!("src/{name}/deeper/file{n}.bin")
            };
            t.file(&rel, &vec![b'z'; *size]);
            totals[i] = totals[i].saturating_add(*size as u64);
        }
    }
    totals
}

#[test]
fn criterion_8_space_sizes_a_directory_and_ctrl_l_resolves_the_bound() {
    let t = Tree::new("c8");
    let totals = build_sized(&t);
    t.file("src/flat.txt", b"a file, always counted");

    // ---- Space sizes the directory under the cursor -----------------------
    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| {
        t.contains("[treeone]") && t.contains("[treetwo]")
    });
    s.focus_entry("treeone");
    let exact = format!("1 of 3 selected \u{b7} {}", status_k(totals[0]));
    s.press(keys::SPACE, "the sized selection", move |t| {
        t.contains(&exact)
    });
    assert!(
        !s.text().contains('\u{2265}'),
        "Space sized the directory, so the total is exact:\n{}",
        s.text()
    );

    // ---- Insert marks without sizing, so the total is a lower bound -------
    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("[treetwo]"));
    s.focus_entry("treeone");
    // `Insert` marks and steps down, so two presses mark both trees - and
    // the design is explicit that it "never sizes a directory".
    s.send(keys::INSERT);
    s.press(keys::INSERT, "both directories marked", |t| {
        t.contains("2 of 3 selected")
    });
    let bounded = s.text();
    assert!(
        bounded.contains("2 of 3 selected \u{b7} \u{2265} 0 k"),
        "an unsized marked directory makes the size a lower bound and says so \
:\n{bounded}"
    );

    // ---- Ctrl+L resolves it ----------------------------------------------
    let resolved = format!(
        "2 of 3 selected \u{b7} {}",
        status_k(totals[0].saturating_add(totals[1]))
    );
    let want = resolved.clone();
    s.press(keys::CTRL_L, "the resolved selection size", move |t| {
        t.contains(&want)
    });
    assert!(
        !s.text().contains('\u{2265}'),
        "Ctrl+L walked every marked directory, so the \u{2265} is gone \
:\n{}",
        s.text()
    );
    // The number rose from the bound to the truth, rather than the `≥` merely
    // being dropped off a figure that stayed at zero.
    assert!(
        totals[0].saturating_add(totals[1]) > 0 && !resolved.contains("\u{b7} 0 k"),
        "the resolved total is the real one: {resolved}"
    );
}

// ---------------------------------------------------------------------------
// 9. A destination that cannot be written is refused before anything is copied
// ---------------------------------------------------------------------------

#[test]
fn criterion_9_an_unwritable_destination_is_refused_up_front() {
    let t = Tree::new("c9");
    t.file("src/aa.txt", b"never copied");
    t.file("src/bb.txt", b"never copied either");

    if !permissions_bite(t.root()) {
        eprintln!(
            "criterion 9 NOT EXERCISED: this process can write into a mode-555 \
             directory (running as root?), so a read-only destination cannot be \
             set up here"
        );
        return;
    }

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("bb"));
    s.point_other_panel_at(&t.dst());
    // Read-only only once the panel has listed it, so the listing itself is not
    // what is being tested.
    fs::set_permissions(t.dst(), fs::Permissions::from_mode(0o555)).expect("chmod dst");

    s.focus_entry("aa");
    s.send(keys::INSERT);
    s.press(keys::INSERT, "both sources marked", |t| {
        t.contains("2 of 2 selected")
    });
    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 2 file(s)")
    });
    s.send(keys::ENTER);

    s.wait_for("the refusal", |t| t.contains("cannot be written"));
    let screen = s.text();
    assert!(
        screen.contains("copied 0 files"),
        "nothing was copied at all (refused up front):\n{screen}"
    );
    assert!(
        screen.contains("1 failed"),
        "one refusal naming the destination, not one failure per source:\n{screen}"
    );

    let _ = fs::set_permissions(t.dst(), fs::Permissions::from_mode(0o755));
    assert_eq!(
        listing(&t.dst()),
        Vec::<String>::new(),
        "and nothing was created in the destination"
    );
    assert_no_partials(&t.dst());
}

// ---------------------------------------------------------------------------
// 10. One file's failure does not abort the batch
// ---------------------------------------------------------------------------

#[test]
fn criterion_10_a_per_file_failure_does_not_abort_the_batch() {
    let t = Tree::new("c10");
    t.file("src/okone.txt", b"the first one that works");
    let locked = t.file("src/locked.txt", b"unreadable");
    t.file("src/oktwo.txt", b"the second one that works");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod locked");

    if !permissions_bite(t.root()) {
        eprintln!(
            "criterion 10 NOT EXERCISED: this process can read a mode-000 file \
             (running as root?), so a per-file failure cannot be set up here"
        );
        return;
    }

    let mut s = Session::start(Launch::new(t.src()));
    s.wait_for("the source listing", |t| t.contains("oktwo"));
    s.point_other_panel_at(&t.dst());

    // `locked.txt` sorts first, so the batch fails on its very first file -
    // which is the case a runner that gave up on error would get wrong.
    s.focus_entry("locked");
    s.send(keys::INSERT);
    s.send(keys::INSERT);
    s.press(keys::INSERT, "all three marked", |t| {
        t.contains("3 of 3 selected")
    });
    s.press(keys::F5, "the copy dialog", |t| {
        t.contains("Copy 3 file(s)")
    });
    s.send(keys::ENTER);

    s.wait_for("the failure summary", |t| t.contains("Retry failures"));
    let screen = s.text();
    assert!(
        screen.contains("copied 2 files") && screen.contains("1 failed"),
        "the batch carried on past the failure:\n{screen}"
    );
    assert!(
        screen.contains("locked.txt"),
        "the summary names what failed:\n{screen}"
    );
    assert!(
        screen.to_lowercase().contains("permission denied"),
        "...and why:\n{screen}"
    );
    assert!(
        screen.contains("Retry failures"),
        "...with the option to retry them:\n{screen}"
    );

    assert_eq!(
        listing(&t.dst()),
        vec!["okone.txt".to_string(), "oktwo.txt".to_string()],
        "the other two arrived and the unreadable one did not"
    );
    assert_eq!(
        read(&t.dst().join("okone.txt")),
        b"the first one that works"
    );
    assert_eq!(
        read(&t.dst().join("oktwo.txt")),
        b"the second one that works"
    );
    assert_no_partials(&t.dst());

    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o644));
}
