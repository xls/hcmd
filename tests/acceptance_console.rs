//! The v0.3 acceptance criteria, driven through the real `hcmd`
//! binary.
//!
//! Same shape as `tests/acceptance.rs` and `tests/acceptance_ops.rs`: the
//! binary is spawned on a pseudo-terminal, key bytes are written into it, and
//! every assertion is made against cells parsed back out with `vt100`. Nothing
//! here inspects the library.
//!
//! What is different, and what to know before reading a test:
//!
//! * **There is a real shell inside our PTY inside the test's PTY.** The
//!   outer one is the terminal `hcmd` draws to; the inner one is the terminal
//!   `bash` draws to, and the design says the second is rendered into the
//!   first. So a criterion about the command line is a statement about three
//!   processes agreeing, and the harness pins every variable it can:
//!   - `SHELL=/bin/bash`, so `console.shell = ""` resolves to a shell whose
//!     prompt hooks this application actually injects;
//!   - `HOME` is a throwaway directory holding a `.bashrc` that sets
//!     `PS1` to [`PROMPT`]. It has to be an rc file rather than an environment
//!     variable: an interactive `bash` overwrites an inherited `PS1` with its
//!     own default before any rc file runs, which was verified rather than
//!     assumed.
//!   - `XDG_CONFIG_HOME` and `XDG_STATE_HOME` are throwaway too, so a run
//!     neither reads nor corrupts the developer's configuration, saved tabs or
//!     command history.
//! * **Nothing waits on a clock.** [`Session::wait`] polls the parsed screen
//!   until it has stopped changing *and* the expected thing is on it. Every
//!   test also starts with [`Session::ready`], which waits for the shell to
//!   have run the injected snippet and drawn its first *marked*
//!   prompt - the observable proof being the `clear` the snippet ends with.
//!   Without that, a fast test could type at a prompt whose `OSC 133` marks did
//!   not exist yet, and half would be racing.
//! * **`HCMD_KEYBOARD_PROTOCOL=enhanced`.** A bare pty answers no capability
//!   query, and criterion 6 needs `Ctrl+Enter`, which only the Kitty protocol
//!   can express.
//!
//! Every test skips itself where there is no `/bin/bash`: these criteria are
//! about a shell whose prompt this application knows how to mark, and a machine
//! without one is not a failing machine.

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

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

/// Keeps concurrently running tests off each other's temporary directories.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The `PS1` the throwaway `.bashrc` sets.
///
/// "Whatever the user's shell draws as a prompt is what appears
/// there". This string is nothing this application could compose - it is not a
/// path, it carries no `<cwd>` - so seeing it at the foot of the panel view is
/// proof that the row came from the shell rather than from a prompt built here.
const PROMPT: &str = "HCMD-PS1>";

/// Key encodings, verified against crossterm 0.29's
/// `src/event/sys/unix/parse.rs` exactly as `tests/acceptance.rs`'s are.
mod keys {
    pub const UP: &[u8] = b"\x1b[A";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const LEFT: &[u8] = b"\x1b[D";
    pub const RIGHT: &[u8] = b"\x1b[C";
    pub const ENTER: &[u8] = b"\r";
    /// `F3` - the viewer. `CSI 13 ~`: crossterm's `parse_csi_tilde`
    /// reads `11..=15` as `F(v - 10)`.
    pub const F3: &[u8] = b"\x1b[13~";
    pub const F4: &[u8] = b"\x1bOS";
    pub const F10: &[u8] = b"\x1b[21~";

    /// `Ctrl+T` - open a tab; `t` is codepoint 116.
    pub const CTRL_T: &[u8] = b"\x1b[116;5u";

    /// `Ctrl+O` - the "hide the panels" (`o` is codepoint 111).
    pub const CTRL_O: &[u8] = b"\x1b[111;5u";
    /// `Ctrl+Enter` - the "insert the filename at the caret".
    pub const CTRL_ENTER: &[u8] = b"\x1b[13;5u";
    /// `Ctrl+C`, in the Kitty encoding (`c` is codepoint 99).
    pub const CTRL_C: &[u8] = b"\x1b[99;5u";
    /// `Esc` - the "close the viewer".
    pub const ESC: &[u8] = b"\x1b";

    /// `Shift+PgUp` / `Shift+PgDn` - the console's scrollback.
    /// `CSI 5 ~` is PageUp and `CSI 6 ~` is
    /// PageDown; the `;2` is crossterm's modifier field with the Shift bit set.
    pub const SHIFT_PGUP: &[u8] = b"\x1b[5;2~";
    pub const SHIFT_PGDN: &[u8] = b"\x1b[6;2~";
}

/// The theme colours the assertions read back out of the rendered cells, from
/// `themes/blue.toml`. A cursor bar is a *background style* and does
/// not appear in the screen's plain text at all.
mod paint {
    /// `panel.cursor_bg` - the focused panel's cursor bar.
    pub const CURSOR_FOCUSED: (u8, u8, u8) = (0x00, 0xA8, 0xA8);
    /// `panel.cursor_bg_unfocused` - the active panel's bar while the command
    /// line has focus. Dimmer, never absent.
    pub const CURSOR_UNFOCUSED: (u8, u8, u8) = (0x00, 0x78, 0x78);
}

/// The bytes crossterm's `EnableMouseCapture` writes, first sequence first.
mod mouse {
    /// The first of the five; the others follow it in one write.
    pub const ON: &str = "\x1b[?1000h";
}

/// The bytes `Term::restore` writes, in the order it writes them.
mod restore {
    /// `EnterAlternateScreen`.
    pub const ALT_ON: &str = "\x1b[?1049h";
    /// `LeaveAlternateScreen`.
    pub const ALT_OFF: &str = "\x1b[?1049l";
    /// `Show`, which `Term::restore` emits *after* `disable_raw_mode`.
    pub const CURSOR_SHOWN: &str = "\x1b[?25h";
}

/// `bash`, or `None` - the shell the prompt snippet is injected into.
///
/// Newest first, and deliberately not `/bin/bash` first. macOS still ships
/// bash 3.2 there, which has no `PS0` - that arrived in 4.4 - and `PS0` is
/// where the command-start mark comes from. Against 3.2 the completion
/// indicator cannot light, so criteria 2 and 3 would be asserting a
/// limitation of a nineteen-year-old shell rather than anything about this
/// program. A capable bash lives beside it on every machine that has one:
/// `/opt/homebrew/bin` on Apple silicon, `/usr/local/bin` on Intel.
fn which_bash() -> Option<&'static str> {
    [
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
        "/usr/bin/bash",
        "/bin/bash",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

// ---------------------------------------------------------------------------
// The fixture directory
// ---------------------------------------------------------------------------

/// The files every criterion navigates over. `alpha.rs` is the one criterion 6
/// inserts into the shell's line, and it is the only entry beginning `alpha`.
const FILES: &[&str] = &["alpha.rs", "thunder", "zeta.txt", "gamma.md", "delta.log"];

/// A directory tree built fresh per test, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hcmd-con-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        // Canonical, because macOS resolves `/var` to `/private/var`: the
        // panel would hold the path as written and the shell would report it
        // as resolved, and a criterion comparing the two would be comparing
        // two spellings of one directory.
        let root = root.canonicalize().unwrap_or(root);
        for (i, name) in FILES.iter().enumerate() {
            std::fs::write(root.join(name), vec![b'x'; 3 + i]).expect("fixture file");
        }
        std::fs::create_dir_all(root.join("subdir")).expect("fixture subdir");
        std::fs::write(root.join("subdir/inner.txt"), b"inner").expect("fixture inner file");
        Self { root }
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

/// A running `hcmd`, the shell inside it, and the screen it has painted.
struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser,
    /// Every byte the child has written, for the assertions in criterion 10
    /// that are about escape sequences rather than about the screen.
    raw: Vec<u8>,
    home: PathBuf,
}

impl Session {
    fn start(bash: &str, cols: u16, rows: u16, cwd: &Path) -> Self {
        Self::start_with_config(bash, cols, rows, cwd, None)
    }

    /// [`Session::start`], with a `config.toml` written into the throwaway
    /// `XDG_CONFIG_HOME` first.
    ///
    /// the design makes the file optional and every key defaulted, so the two
    /// criteria that need one write only the lines they are about.
    fn start_with_config(
        bash: &str,
        cols: u16,
        rows: u16,
        cwd: &Path,
        config: Option<&str>,
    ) -> Self {
        let home = std::env::temp_dir().join(format!(
            "hcmd-con-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("throwaway home");
        // An interactive bash replaces an inherited `PS1` with its own default
        // before any rc file runs - verified, not assumed - so the prompt this
        // harness recognises has to be set from a `.bashrc`. `~/.bashrc` is read
        // after `/etc/bash.bashrc`, so it wins on every distribution that ships
        // one.
        std::fs::write(
            home.join(".bashrc"),
            format!("PS1='{PROMPT} '\nunset HISTFILE\n"),
        )
        .expect("write .bashrc");

        if let Some(text) = config {
            let dir = home.join("holoscommander");
            std::fs::create_dir_all(&dir).expect("config dir");
            std::fs::write(dir.join("config.toml"), text).expect("write config.toml");
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
        // machine: the assertions read `│` and `▲`.
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("LC_ALL", "en_US.UTF-8");
        // a bare pty answers no capability query, and criterion 6
        // needs `Ctrl+Enter`, which only the Kitty protocol can express.
        cmd.env("HCMD_KEYBOARD_PROTOCOL", "enhanced");
        // No filesystem watch under the harness: these tests run a real shell
        // in the panel's own directory, so a command that touches a file would
        // trigger a rescan and move the cursor out from under an assertion.
        // What the watch does is covered by unit tests.
        cmd.env("HCMD_NO_FS_WATCH", "1");
        cmd.env("XDG_CONFIG_HOME", &home);
        cmd.env("XDG_STATE_HOME", &home);
        // "Shell from `$SHELL`". Pinned, so the criteria are
        // about a shell whose prompt this application marks
        // rather than about whatever the developer happens to use.
        cmd.env("SHELL", bash);
        cmd.env("HOME", &home);
        cmd.cwd(cwd);

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
        // The master is dropped with the pair; the reader and writer keep their
        // own handles on it, which is all this harness needs.
        drop(pair.master);

        Self {
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

    /// The screen's rows, trailing blanks trimmed.
    fn lines(&self) -> Vec<String> {
        self.text()
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    /// How many rows read exactly `needle`.
    ///
    /// Exact rather than `contains`, because criterion 9 has to tell the tty's
    /// echo of a typed line from `cat` writing the same line back out, and
    /// criterion 8 has to tell `hcmd-line-1` from `hcmd-line-10`.
    fn line_count(&self, needle: &str) -> usize {
        self.lines().iter().filter(|line| *line == needle).count()
    }

    fn has_line(&self, needle: &str) -> bool {
        self.line_count(needle) > 0
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
        let deadline = Instant::now() + Duration::from_secs(25);
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

    /// The panels are drawn and the fixture listing has arrived.
    fn wait_for_listing(&mut self) {
        self.wait_for("the fixture listing", |t| {
            t.contains("thunder") && t.contains("[subdir]")
        });
    }

    /// The application is up **and the shell is past the snippet**.
    ///
    /// The snippet is written to the PTY as the shell's first input line and
    /// ends with `clear`, so a console screen holding nothing but a prompt
    /// on its first row is the observable proof that it ran - and the prompt
    /// drawn after it is the first one carrying the `OSC 133` marks that the
    /// command line, the `cd` and the `switch_delay` decision are all read
    /// from.
    ///
    /// Every test starts here, so none of them can race the snippet.
    fn ready(&mut self) {
        self.wait_for_listing();
        self.send(keys::CTRL_O);
        self.wait("the shell's first prompt on a cleared screen", |s| {
            let lines = s.lines();
            let Some(first) = lines.first() else {
                return false;
            };
            first.trim_start().starts_with(PROMPT)
                && lines.iter().skip(1).all(|line| line.trim().is_empty())
        });
        self.send(keys::CTRL_O);
        self.wait_for_listing();
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

    /// Type a line into whichever line editor has focus, and wait for it to be
    /// echoed at the foot of the panel view.
    fn type_at_cmdline(&mut self, text: &'static str) {
        self.send(text.as_bytes());
        self.wait(&format!("the shell to echo {text:?}"), move |s| {
            s.cmdline_text().contains(text)
        });
    }

    // -- reading the painted cells ----------------------------------------

    /// The row index of the command line: the row above the key bar.
    ///
    /// Found structurally rather than by looking for a prompt, because
    /// the design makes that prompt the shell's and it could be anything.
    /// The key bar is the last row and the command line is the rows above it;
    /// the shell's caret is on the last of those.
    fn cmdline_row(&self) -> u16 {
        let (rows, _) = self.parser.screen().size();
        let last = rows.saturating_sub(1);
        let text = self.text();
        let has_keybar = text
            .lines()
            .nth(usize::from(last))
            .is_some_and(|line| line.contains("F10"));
        if has_keybar {
            last.saturating_sub(1)
        } else {
            last
        }
    }

    /// What the command line reads - the "the shell's own current
    /// input line", as it was actually painted.
    fn cmdline_text(&self) -> String {
        self.lines()
            .get(usize::from(self.cmdline_row()))
            .cloned()
            .unwrap_or_default()
    }

    /// The command line with the painted caret taken off the end.
    ///
    /// While the *panel* has focus the command-line caret is drawn as a solid
    /// cell rather than as the hardware cursor, so the same shell line reads
    /// one glyph longer than it does with focus here. Criterion 5 compares the
    /// line across exactly that focus change, and the block is this
    /// application's drawing rather than anything the shell holds.
    fn shell_line(&self) -> String {
        self.cmdline_text()
            .trim_end_matches('\u{2588}')
            .trim_end()
            .to_string()
    }

    /// the completion indicator: the key bar's last cell.
    ///
    /// > A completion indicator in the key bar shows when a background command
    /// > has produced output.
    ///
    /// A single styled cell, so it is invisible in the plain text.
    fn keybar_indicator(&self) -> bool {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        screen
            .cell(rows.saturating_sub(1), cols.saturating_sub(1))
            .is_some_and(|cell| cell.contents() == "\u{25cf}")
    }

    /// Whether a prompt has been drawn *below* the last row reading `needle`.
    ///
    /// "The shell is back at a prompt" cannot be asked of the whole screen: the
    /// prompt that started the program is still up there in the scrollback.
    /// Position is the question, so position is what is compared.
    fn prompt_below(&self, needle: &str) -> bool {
        let lines = self.lines();
        let last = lines.iter().rposition(|line| line == needle);
        let prompt = lines.iter().rposition(|line| line.contains(PROMPT));
        matches!((last, prompt), (Some(last), Some(prompt)) if prompt > last)
    }

    /// The screen row the active panel's cursor bar is on, whichever of the two
    /// the design styles it is wearing.
    ///
    /// The bar changes style when the command line takes focus and must never
    /// disappear, so both are accepted here; criterion 5 is about the row it is
    /// on, not about which style it wears.
    fn cursor_screen_row(&self) -> Option<u16> {
        self.row_with_bg(paint::CURSOR_FOCUSED)
            .or_else(|| self.row_with_bg(paint::CURSOR_UNFOCUSED))
            .map(|(row, _)| row)
    }

    /// The entry the active panel's cursor bar is sitting on.
    fn cursor_entry(&self) -> String {
        self.row_with_bg(paint::CURSOR_FOCUSED)
            .or_else(|| self.row_with_bg(paint::CURSOR_UNFOCUSED))
            .map(|(_, text)| text)
            .unwrap_or_default()
    }

    /// The first row painted in `bg`, as `(row, text)`.
    ///
    /// Only the rows above the command line are searched: the blue theme paints
    /// `keybar.label_bg` in the same `#00A8A8` as `panel.cursor_bg`.
    fn row_with_bg(&self, bg: (u8, u8, u8)) -> Option<(u16, String)> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let want = vt100::Color::Rgb(bg.0, bg.1, bg.2);
        for row in 0..self.cmdline_row() {
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
                return Some((row, text.trim().to_string()));
            }
        }
        None
    }

    /// Where the terminal's real cursor is, as `(row, col)`.
    fn hardware_cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Whether the terminal's real cursor is drawn at all. the design hides
    /// it while a panel has focus, so this is how "focus is on the command
    /// line" is observed from outside.
    fn hardware_cursor_hidden(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    /// True while the command line - and therefore the shell's input line - has
    /// focus.
    fn cmdline_has_focus(&self) -> bool {
        !self.hardware_cursor_hidden() && self.hardware_cursor().0 == self.cmdline_row()
    }

    /// The whole byte stream the child has written.
    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    /// What the tab state file holds, or `None` if there is none.
    ///
    /// `XDG_STATE_HOME` is the throwaway home, so this is the file *this*
    /// session wrote and nothing else.
    fn saved_tabs(&self) -> Option<String> {
        std::fs::read_to_string(self.home.join("holoscommander/tabs.toml")).ok()
    }

    /// The `hcmd` process's own pid.
    fn pid(&mut self) -> u32 {
        self.child.process_id().expect("hcmd has a pid")
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
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

// ---------------------------------------------------------------------------
// Looking at processes, for criterion 10
// ---------------------------------------------------------------------------

/// The pids whose parent is `parent`, from `/proc`.
///
/// the design starts the shell as a child of this application, so the shell
/// is exactly what this finds - no marker file, no `$$` echoed into the
/// fixture, nothing that could name a process that had already been replaced.
#[cfg(target_os = "macos")]
fn child_pids(parent: u32) -> Vec<u32> {
    ps(&["-Ao", "pid=,ppid="])
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse::<u32>().ok()?;
            let ppid = it.next()?.parse::<u32>().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn child_pids(parent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let ppid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<u32>().ok());
        if ppid == Some(parent) {
            out.push(pid);
        }
    }
    out
}

/// Ask `ps` one question. macOS has no `/proc`, and this is a test: starting
/// a process to look at processes is a cost the program itself never pays.
#[cfg(target_os = "macos")]
fn ps(args: &[&str]) -> String {
    std::process::Command::new("/bin/ps")
        .args(args)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The executable name of a live process, or `None` if it is not there.
#[cfg(target_os = "macos")]
fn process_name(pid: u32) -> Option<String> {
    let out = ps(&["-o", "comm=", "-p", &pid.to_string()]);
    let name = out.trim();
    if name.is_empty() {
        return None;
    }
    // `comm` is a full path here, unlike Linux's bare name.
    Some(
        Path::new(name)
            .file_name()
            .map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned()),
    )
}

/// The executable name of a live process, or `None` if it is not there.
#[cfg(not(target_os = "macos"))]
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
}

/// Whether `pid` is still a running process rather than gone or a zombie.
///
/// `/proc/<pid>/stat` is `pid (comm) state …` and `comm` may itself contain
/// spaces and parentheses, so the state is read after the *last* `)`.
#[cfg(target_os = "macos")]
fn process_is_running(pid: u32) -> bool {
    let state = ps(&["-o", "state=", "-p", &pid.to_string()]);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

#[cfg(not(target_os = "macos"))]
fn process_is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.split_whitespace().next(), None | Some("Z") | Some("X"))
}

/// Poll until `pid` is gone, or fail loudly. "A file manager that leaves
/// orphaned shells behind is a bug".
fn wait_for_process_to_go(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !process_is_running(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "{what}: pid {pid} ({}) is still running after hcmd exited",
        process_name(pid).unwrap_or_else(|| "?".to_string())
    );
}

/// The one shell `hcmd` started, asserted to be exactly one.
fn the_shell_of(hcmd: u32) -> u32 {
    let children = child_pids(hcmd);
    assert_eq!(
        children.len(),
        1,
        "the design starts one persistent shell; hcmd's children are {:?}",
        children
            .iter()
            .map(|pid| (*pid, process_name(*pid)))
            .collect::<Vec<_>>()
    );
    let pid = children.first().copied().unwrap_or_default();
    assert_eq!(
        process_name(pid).as_deref(),
        Some("bash"),
        "the child hcmd started is the shell"
    );
    pid
}

/// Start a session in a fresh fixture, or `None` where there is no `bash`.
fn session(tag: &str) -> Option<(Fixture, Session)> {
    let bash = which_bash()?;
    let fix = Fixture::new(tag);
    let mut s = Session::start(bash, 120, 30, fix.path());
    s.ready();
    Some((fix, s))
}

/// The `eprintln!` every test shares for a machine with no `bash`.
fn no_bash(what: &str) {
    eprintln!("no bash on this machine; skipping {what}");
}

/// Can this `bash` report that a command has started?
///
/// The mark comes from `PS0`, which bash gained in 4.4. macOS still ships
/// 3.2.57 as `/bin/bash` and the CI image carries nothing newer, so on a
/// stock Mac the completion indicator has nothing to light from. A criterion
/// asserting it there would be asserting the age of the shell.
///
/// Returns the version alongside the answer so a skip can say which bash it
/// was looking at, rather than leaving that to be guessed from the machine.
fn bash_marks_commands(bash: &str) -> (bool, String) {
    let out = std::process::Command::new(bash)
        .arg("-c")
        .arg("printf %s.%s \"${BASH_VERSINFO[0]}\" \"${BASH_VERSINFO[1]}\"")
        .output();
    let Ok(out) = out else {
        return (false, "unknown".to_string());
    };
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    ((major, minor) >= (4, 4), version)
}

/// The `eprintln!` for a bash too old to mark where a command starts.
fn no_ps0(bash: &str, version: &str, what: &str) {
    eprintln!("{bash} is bash {version}, which has no PS0; skipping {what}");
}

// ---------------------------------------------------------------------------
// 1. Ctrl+O hides the panels and shows the shell
// ---------------------------------------------------------------------------

/// > `Ctrl+O` **hides the panels**, so the screen is the shell and nothing
/// > else … `Ctrl+O` again brings the panels back, with them refreshed. It is
/// > not a split, not a pane, and not a shrunken terminal.
#[test]
fn criterion_1_ctrl_o_hides_the_panels_and_shows_the_shell() {
    let Some((_fix, mut s)) = session("c1") else {
        return no_bash("the Ctrl+O criterion");
    };

    // The panel view: two boxed panels, the shell's prompt at the foot, the key
    // bar under it.
    assert!(
        s.text().contains("thunder") && s.text().contains('│'),
        "the panels are drawn to start with:\n{}",
        s.text()
    );
    assert!(
        s.cmdline_text().contains(PROMPT),
        "and the shell's prompt is at the foot of them:\n{}",
        s.cmdline_text()
    );

    s.press(keys::CTRL_O, "the shell to take the screen", |t| {
        !t.contains("thunder")
    });
    let lines = s.lines();
    assert!(
        lines
            .first()
            .is_some_and(|line| line.trim_start().starts_with(PROMPT)),
        "the shell's own screen, from its first row:\n{}",
        s.text()
    );
    assert!(
        !s.text().contains("[subdir]") && !s.text().contains('│'),
        "no panels and no panel borders - it is not a split:\n{}",
        s.text()
    );
    assert!(
        !s.text().contains("F10"),
        "and no key bar over the shell's screen:\n{}",
        s.text()
    );

    // "`Ctrl+O` again brings the panels back, **with them
    // refreshed**." A file made by the shell while the panels were hidden is
    // the observable form of that: the listing on screen was read before it
    // existed, so it can only be right if coming back re-reads.
    s.send(b"touch hcmd-made-here\r");
    s.wait("the shell to have made the file", |s| {
        s.lines()
            .iter()
            .any(|line| line.contains(PROMPT) && line.contains("touch hcmd-made-here"))
    });

    s.press(keys::CTRL_O, "the panels to come back", |t| {
        t.contains("thunder")
    });
    assert!(
        s.text().contains("[subdir]") && s.text().contains("F10"),
        "the whole panel view is back:\n{}",
        s.text()
    );
    assert!(
        s.text().contains("hcmd-made-here"),
        "and refreshed, so what the shell did while they were hidden is in \
         the listing:\n{}",
        s.text()
    );
    assert!(
        s.cmdline_text().contains(PROMPT),
        "with the shell's prompt back at the foot of it:\n{}",
        s.cmdline_text()
    );
}

// ---------------------------------------------------------------------------
// 2. A command typed at the command line runs
// ---------------------------------------------------------------------------

/// "`Enter` on a non-empty command line writes it to the PTY …
/// **`auto`** (default): the panels stay. After `switch_delay`, if the shell
/// has **not** returned to a prompt with an empty input line, the command is
/// still holding the terminal … and the screen switches to the console so it
/// can be interacted with. If the shell *has* returned, nothing happens at all:
/// no switch, no flash."
///
/// Both halves of that, in one session, because they are one rule: the same
/// `Enter`, the same default, and the only difference is whether the command
/// was still there at the deadline. `echo` is the first - back at a prompt in
/// microseconds - and `cat` with no arguments is the second, the smallest
/// program that holds a terminal until it is interrupted.
#[test]
fn criterion_2_a_command_typed_at_the_command_line_runs_in_the_console() {
    let Some((_fix, mut s)) = session("c2") else {
        return no_bash("the run-a-command criterion");
    };

    // `Right` from the panel focuses the command line.
    s.send(keys::RIGHT);
    s.wait(
        "focus to reach the command line",
        Session::cmdline_has_focus,
    );

    // The characters are the shell's: they are echoed by *its* line editor, on
    // the row this application is rendering.
    s.type_at_cmdline("echo hcmd-ran-this");
    assert!(
        s.cmdline_text()
            .contains(&format!("{PROMPT} echo hcmd-ran-this")),
        "the typed line appears after the shell's own prompt:\n{}",
        s.cmdline_text()
    );

    // A command that finishes before the eye moves. `settle` waits far longer
    // than `switch_delay`, so a switch that was going to happen has happened by
    // the time this returns.
    s.press(keys::ENTER, "the shell to be back at a prompt", |t| {
        t.contains(PROMPT)
    });
    assert!(
        s.text().contains("[subdir]") && s.text().contains("thunder"),
        "`echo` was back at a prompt before `switch_delay`, so the panels \
         stayed: no switch, no flash:\n{}",
        s.text()
    );
    // The indicator is made of the command-start mark, so it cannot light on a
    // bash that cannot emit one. Everything above this holds either way.
    match which_bash().map(|b| (b, bash_marks_commands(b))) {
        Some((bash, (false, version))) => {
            no_ps0(bash, &version, "the completion indicator");
        }
        _ => assert!(
            s.keybar_indicator(),
            "and the key bar's completion indicator is what says the command \
             finished with output while the panels were showing - the ordinary \
             case under `auto`:\n{}",
            s.text()
        ),
    }

    // The output is in the buffer "for whenever it is wanted".
    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("[subdir]")
    });
    assert!(
        s.has_line("hcmd-ran-this"),
        "`echo` printed on a row of its own, so the command really ran:\n{}",
        s.text()
    );
    assert!(
        s.lines()
            .iter()
            .any(|line| line.contains(PROMPT) && line.contains("echo hcmd-ran-this")),
        "and the line it ran is above it in the console's scrollback:\n{}",
        s.text()
    );
    s.press(keys::CTRL_O, "the panels back", |t| t.contains("[subdir]"));

    // Now one that is *still holding the terminal* at the deadline. Nothing
    // here waits on a clock: `switch_delay` is 250ms and `settle` needs a
    // quarter of a second of an unchanging screen before it even looks.
    s.send(keys::RIGHT);
    s.wait(
        "focus to reach the command line again",
        Session::cmdline_has_focus,
    );
    s.type_at_cmdline("cat");
    s.press(
        keys::ENTER,
        "the panels to get out of the way of a command that kept the terminal",
        |t| !t.contains("[subdir]"),
    );

    // And it is a real terminal: `cat` echoes what is typed at it.
    s.send(b"hcmd-still-running\r");
    s.wait("cat to echo the line back", |s| {
        s.line_count("hcmd-still-running") == 2
    });

    // Switching in is automatic, and so is switching back out. The screen was
    // taken because the command was still holding the terminal; when that stops
    // being true the reason is gone, and a screen nobody asked for should not
    // have to be dismissed. This reverses the earlier rule, which left a
    // finished `git clone` sitting at a prompt until `Ctrl+O` was pressed.
    //
    // A console reached with `Ctrl+O` is a decision and is never taken away -
    // `a_console_that_was_asked_for_is_never_taken_away` covers that half.
    s.press(keys::CTRL_C, "the panels back once cat has gone", |t| {
        t.contains("[subdir]")
    });
}

// ---------------------------------------------------------------------------
// 3. Scrollback survives toggling
// ---------------------------------------------------------------------------

/// "A **persistent** shell process on a PTY … Its scrollback
/// survives toggling, so the output of a long build is still there when you
/// come back."
#[test]
fn criterion_3_scrollback_survives_toggling() {
    let Some((fix, mut s)) = session("c3") else {
        return no_bash("the persistent-scrollback criterion");
    };

    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });
    s.send(b"echo hcmd-marker-4711\r");
    s.wait("the marker to be printed", |s| {
        s.has_line("hcmd-marker-4711")
    });

    s.press(keys::CTRL_O, "the panels to come back", |t| {
        t.contains("thunder")
    });
    assert!(
        !s.text().contains("hcmd-marker-4711"),
        "the panels really are what is on screen now:\n{}",
        s.text()
    );

    // The shell kept running behind them, and its screen is untouched.
    s.press(keys::CTRL_O, "the shell's screen again", |t| {
        !t.contains("thunder")
    });
    assert!(
        s.has_line("hcmd-marker-4711"),
        "the output of a command run before the toggle is still there \
:\n{}",
        s.text()
    );
    assert!(
        s.lines()
            .iter()
            .any(|line| line.contains(PROMPT) && line.contains("echo hcmd-marker-4711")),
        "and so is the command that produced it:\n{}",
        s.text()
    );

    // Still the same shell, not a fresh one: it answers on the same screen.
    s.send(b"echo hcmd-marker-4712\r");
    s.wait("a second command in the same session", |s| {
        s.has_line("hcmd-marker-4712")
    });
    assert!(
        s.has_line("hcmd-marker-4711"),
        "and the first marker is still above it - one persistent shell:\n{}",
        s.text()
    );

    // The stronger form of the same criterion, and the rest of
    // "While in panel mode the PTY keeps running; output is buffered. A
    // completion indicator in the key bar shows when a background command has
    // produced output."
    //
    // The indicator only fires for output produced *behind the panels*, so the
    // command has to still be running when the toggle lands and must produce
    // its output afterwards. This used to be spelled `sleep 6`, and it was a
    // race the harness lost about one run in six: on a loaded machine the
    // sleep elapsed before the toggle landed, the output went to the shell's
    // own screen where it raises no indicator, and the wait then sat for its
    // full deadline. Waiting longer would not have fixed it, because the
    // failure is the ordering, not the duration.
    //
    // So the timing is taken out of it. `cat` on a fifo blocks until this test
    // writes to it, which makes "the command is still running when the toggle
    // lands" true by construction rather than by luck, and makes the output
    // happen at a moment the test chooses.
    let fifo = fix.path().join("late-output");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    if !made {
        eprintln!("no mkfifo on this machine; skipping the indicator half");
        return;
    }

    s.send(b"cat late-output\r");
    s.send(keys::CTRL_O);
    s.wait_for("the panels, with the command still running", |t| {
        t.contains("thunder")
    });
    // `cat` cannot have finished: nothing has been written to the fifo yet.
    // Knowing it is *running* is the command-start mark again, so this is
    // asked only of a bash that can send one.
    let marks = match which_bash() {
        Some(bash) => {
            let (marks, version) = bash_marks_commands(bash);
            if !marks {
                no_ps0(bash, &version, "the mid-command check");
            }
            marks
        }
        None => true,
    };
    assert!(
        !marks || !s.shell_line().contains(PROMPT),
        "the shell is mid-command, so there is no prompt on its line:\n{}",
        s.cmdline_text()
    );

    // Opening the writing end unblocks `cat`, which prints behind the panels.
    std::fs::write(&fifo, b"hcmd-late-output\n").expect("write the fifo");

    s.wait(
        "the key bar's completion indicator",
        Session::keybar_indicator,
    );

    // The command finishes and the shell draws its next prompt, all with the
    // panels on screen. That the *prompt* comes back is what says the whole
    // command ran while they were up.
    s.wait(
        "the shell to reach its next prompt behind the panels",
        |s| s.shell_line().trim_start().starts_with(PROMPT),
    );

    s.press(keys::CTRL_O, "the shell's screen a third time", |t| {
        !t.contains("thunder")
    });
    assert!(
        s.has_line("hcmd-late-output"),
        "what printed while the panels were up was buffered and is there \
:\n{}",
        s.text()
    );
    assert!(
        s.has_line("hcmd-marker-4711") && s.has_line("hcmd-marker-4712"),
        "and nothing before it was lost:\n{}",
        s.text()
    );
}

/// the design against the viewer takes the whole screen the way
/// `Ctrl+O` does, and "while in panel mode the PTY keeps running; output is
/// buffered" has to hold behind it too.
///
/// This is the third backdrop the shell has to survive - panels, console,
/// viewer - and the only one that also holds an open file and a background
/// index scan. A viewer that stopped draining the pty, or that took the loop
/// away from it, would show up here and nowhere else.
#[test]
fn the_shell_keeps_running_behind_the_viewer() {
    let Some((_fix, mut s)) = session("viewer-console") else {
        return no_bash("the shell-behind-the-viewer criterion");
    };

    // Start something slow enough that the viewer is certain to be up before
    // it prints, and go back to the panels.
    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });
    s.send(b"sleep 2; echo hcmd-behind-the-viewer\r");
    s.press(keys::CTRL_O, "the panels", |t| t.contains("thunder"));

    // Quick search onto a file, then `F3`. `alpha.rs` is the only entry
    // beginning `alpha`, and it is a file rather than a directory.
    s.send(b"alpha");
    s.press(keys::F3, "the viewer over the whole screen", |t| {
        t.contains("alpha.rs") && !t.contains("thunder")
    });

    // Hold the viewer past the `sleep`, so the command runs and prints while
    // the viewer - not the panels and not the console - is what is on screen.
    // Wall clock rather than the shell's prompt, because `shell_line` reads the
    // *panel's* command-line row and the viewer is covering it, which is the
    // whole point.
    let held = Instant::now() + Duration::from_secs(4);
    while Instant::now() < held {
        s.settle();
        assert!(
            !s.text().contains("hcmd-behind-the-viewer"),
            "the viewer is what is on screen, not the shell's output:\n{}",
            s.text()
        );
    }
    assert!(
        s.text().contains("alpha.rs") && !s.text().contains("thunder"),
        "and the viewer still has the screen:\n{}",
        s.text()
    );

    // `Esc` closes the viewer and `Ctrl+O` shows what was
    // buffered while it held the screen.
    s.press(keys::ESC, "the panels back", |t| t.contains("thunder"));
    s.press(keys::CTRL_O, "the shell's screen again", |t| {
        !t.contains("thunder")
    });
    assert!(
        s.has_line("hcmd-behind-the-viewer"),
        "what printed while the viewer was up was buffered and is there \
:\n{}",
        s.text()
    );

    // And it is still the same shell, not a replacement.
    s.send(b"echo hcmd-still-the-same-shell\r");
    s.wait("the same shell to answer again", |s| {
        s.has_line("hcmd-still-the-same-shell")
    });
    assert!(
        s.has_line("hcmd-behind-the-viewer"),
        "with everything above it intact:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 4. The command line is the shell's own prompt
// ---------------------------------------------------------------------------

///
///
/// > The command line at the foot of the panel view is **the shell's own
/// > current input line**, rendered from the PTY … holoscommander does not
/// > compose a prompt of its own.
///
/// `PS1` is set to something no prompt composed here could produce, so the row
/// can only have come from the shell.
#[test]
fn criterion_4_the_command_line_is_the_shells_own_prompt() {
    let Some((fix, mut s)) = session("c4") else {
        return no_bash("the shell's-prompt criterion");
    };

    let row = s.cmdline_text();
    assert!(
        row.trim_start().starts_with(PROMPT),
        "the shell's own PS1 is what is drawn at the foot of the panel view \
, got {row:?}:\n{}",
        s.text()
    );
    // The v0.1 prompt is `<cwd>> ` (`ui::cmdline::prompt`). the design calls
    // that "a placeholder for this, not a design", and with a live shell it
    // must not be what is drawn.
    let cwd = fix.path().display().to_string();
    assert!(
        !row.contains(&format!("{cwd}> ")),
        "no prompt is composed here while a shell is running, \
         got {row:?}"
    );

    // It is the *current input line*, not a snapshot: what is typed appears on
    // it, because the shell echoed it there.
    s.send(keys::RIGHT);
    s.wait(
        "focus to reach the command line",
        Session::cmdline_has_focus,
    );
    s.type_at_cmdline("hcmd-typed-here");
    let row = s.cmdline_text();
    assert!(
        row.contains(PROMPT) && row.contains("hcmd-typed-here"),
        "the prompt and the shell's line buffer are one row, \
         got {row:?}:\n{}",
        s.text()
    );
    // The caret is the shell's, and it is at the end of what was typed.
    let end = row.find("hcmd-typed-here").unwrap_or_default() + "hcmd-typed-here".len();
    assert_eq!(
        usize::from(s.hardware_cursor().1),
        end,
        "the caret is the shell's own, standing after the last character \
:\n{row:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Up and Down leave the command line and never reach the shell
// ---------------------------------------------------------------------------

///
///
/// > `Up` / `Down` - **return focus to the panel** *and* move its cursor one
/// > row in that direction, preserving the text.
/// > The keys this application binds - `Up`/`Down` to leave for the panel
/// > … are intercepted before forwarding and never reach the shell.
///
/// "Never reach the shell" is asserted the strongest way available from
/// outside: the shell's own line is rendered on screen, and `Up` at a bash
/// prompt recalls a history entry - which would replace it visibly. It does
/// not change, so nothing arrived.
#[test]
fn criterion_5_up_and_down_leave_the_command_line_without_the_shell_seeing_them() {
    let Some((_fix, mut s)) = session("c5") else {
        return no_bash("the Up/Down criterion");
    };

    s.send(keys::RIGHT);
    s.wait(
        "focus to reach the command line",
        Session::cmdline_has_focus,
    );
    s.type_at_cmdline("cp X");
    let line = s.shell_line();
    let start_row = s
        .cursor_screen_row()
        .unwrap_or_else(|| panic!("no panel cursor bar:\n{}", s.text()));

    // Down: focus back to the panel, and the panel cursor steps one row.
    s.send(keys::DOWN);
    s.wait("Down to return focus to the panel", |s| {
        s.hardware_cursor_hidden()
    });
    assert_eq!(
        s.cursor_screen_row(),
        Some(start_row + 1),
        "Down moves the panel cursor a row as well:\n{}",
        s.text()
    );
    assert_eq!(
        s.shell_line(),
        line,
        "and the shell's line is untouched - `Up`/`Down` never reach it \
"
    );

    // Back to the command line: the shell's line buffer and caret were never
    // disturbed, so there is nothing to restore.
    s.send(keys::RIGHT);
    s.wait("focus to return to the command line", |s| {
        s.cmdline_has_focus()
    });
    assert_eq!(s.shell_line(), line, "the text survives the round trip");

    // Up: the same, in the other direction.
    s.send(keys::UP);
    s.wait("Up to return focus to the panel", |s| {
        s.hardware_cursor_hidden()
    });
    assert_eq!(
        s.cursor_screen_row(),
        Some(start_row),
        "Up moves the panel cursor back a row:\n{}",
        s.text()
    );
    assert_eq!(
        s.shell_line(),
        line,
        "and the shell still holds exactly `cp X` - no history was recalled"
    );
}

// ---------------------------------------------------------------------------
// 6. Ctrl+Enter inserts the filename at the shell's cursor, mid-line
// ---------------------------------------------------------------------------

/// "`Ctrl+Enter` writes the filename to the PTY at the shell's cursor.
/// Insertion mid-line still works, because that is what the shell's own
/// line editor does with the characters." And "`Ctrl+Enter` moves focus to
/// the command line."
#[test]
fn criterion_6_ctrl_enter_inserts_the_filename_mid_line() {
    let Some((_fix, mut s)) = session("c6") else {
        return no_bash("the Ctrl+Enter criterion");
    };

    // Put the panel cursor on `alpha.rs` (the quick search).
    s.press(b"alpha", "the quick-search buffer", |t| {
        t.contains("search: alpha")
    });
    // The panel draws the name and the extension in separate columns,
    // so the row reads `alpha` … `rs` rather than `alpha.rs`.
    let on = s.cursor_entry();
    assert!(
        on.starts_with("alpha") && on.contains("rs"),
        "the panel cursor is on alpha.rs, got {on:?}:\n{}",
        s.text()
    );

    s.send(keys::RIGHT);
    s.wait(
        "focus to reach the command line",
        Session::cmdline_has_focus,
    );
    s.type_at_cmdline("cp X");

    // One `Left` puts the shell's caret *on* the `X`, so an insertion that
    // appended and an insertion that landed at the caret have different
    // answers - which is the whole point of the criterion.
    let end = s.hardware_cursor().1;
    s.send(keys::LEFT);
    s.wait("the shell's caret to step left", move |s| {
        s.hardware_cursor().1 == end - 1
    });

    s.send(keys::CTRL_ENTER);
    s.wait("the filename to be inserted mid-line", |s| {
        s.cmdline_text().contains("cp alpha.rs X")
    });
    let row = s.cmdline_text();
    assert!(
        row.contains(&format!("{PROMPT} cp alpha.rs X")),
        "inserted at the shell's cursor with the separating space, \
         not appended, got {row:?}"
    );
    assert!(
        s.cmdline_has_focus(),
        "and focus stays on the command line:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 7. The shell's cwd and the panel's directory track each other
// ---------------------------------------------------------------------------

/// both halves:
///
/// > **Panel → shell**: when the active panel changes directory, write
/// > `cd <path>` to the PTY … **Shell → panel**: rely on **OSC 7** … Parse
/// > OSC 7 out of the PTY stream and update the active panel.
#[test]
fn criterion_7_cd_moves_the_panel_and_the_panel_moves_the_shell() {
    let Some((fix, mut s)) = session("c7") else {
        return no_bash("the cwd-sync criterion");
    };
    let root = fix.path().display().to_string();
    let subdir = fix.path().join("subdir").display().to_string();

    // -- panel → shell -----------------------------------------------------
    s.press(b"sub", "the cursor on [subdir]", |t| {
        t.contains("search: sub")
    });
    // `inner` rather than `inner.txt`: the extension is its own column.
    s.press(keys::ENTER, "the subdir listing", |t| t.contains("inner"));

    // The shell was told to follow, and `pwd` is what proves it did.
    s.press(keys::CTRL_O, "the shell's screen", |t| !t.contains("inner"));
    s.send(b"pwd\r");
    let expected = subdir.clone();
    s.wait("the shell to report the panel's directory", move |s| {
        s.has_line(&expected)
    });

    // -- shell → panel -----------------------------------------------------
    s.send(b"cd ..\r");
    s.press(keys::CTRL_O, "the panels to come back", |t| {
        t.contains("thunder")
    });
    assert!(
        s.text().contains("[subdir]") && !s.text().contains("inner"),
        "a `cd` in the console moved the active panel:\n{}",
        s.text()
    );
    // The border elides a long path, so what is checked is the component that
    // identifies it rather than the whole string.
    let leaf = fix
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.clone());
    assert!(
        s.text().contains(&leaf),
        "and the panel's border names the directory the shell moved to:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 8. Scrolling the console, and snapping back
// ---------------------------------------------------------------------------

/// The console's scrollback keys.
///
/// **`Shift+PgUp`/`Shift+PgDn`, not plain `PgUp`/`PgDn`** - the decision
/// recorded in the design, and the reason is the own: "a program that wants a
/// terminal gets a real one", so plain `PgUp` belongs to the `less` that may
/// be running in there. It is also what every terminal emulator does, and the
/// design itself binds `PgUp`/`PgDn` only on a *panel*. Both halves are
/// asserted: the shifted keys scroll the console, and the unshifted one does
/// not move the view at all - it is forwarded, so it is still there for the
/// program that wants it.
#[test]
fn criterion_8_shift_page_keys_scroll_the_console_and_a_keystroke_snaps_back() {
    let Some((_fix, mut s)) = session("c8") else {
        return no_bash("the console-scrollback criterion");
    };

    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });
    // More output than the 30-row screen can hold, so there is a history to
    // scroll back into. Brace expansion, so nothing outside bash is involved.
    s.send(b"for i in {1..40}; do echo hcmd-line-$i; done\r");
    s.wait("the last line of the output", |s| {
        s.has_line("hcmd-line-40")
    });
    assert!(
        !s.has_line("hcmd-line-1"),
        "the first line has scrolled off, or there is nothing to scroll to:\n{}",
        s.text()
    );

    // Shift+PgUp scrolls the console back, and the row that had gone off the
    // top is on screen again.
    s.send(keys::SHIFT_PGUP);
    s.wait("hcmd-line-1 to be back on screen", |s| {
        s.has_line("hcmd-line-1")
    });
    assert!(
        s.text().contains("Shift+PgDn"),
        "and says so, rather than leaving the user wondering why the screen \
         stopped following:\n{}",
        s.text()
    );

    // Shift+PgDn returns to the live screen.
    s.press(keys::SHIFT_PGDN, "the live screen again", |t| {
        !t.contains("Shift+PgDn")
    });
    assert!(
        s.has_line("hcmd-line-40"),
        "Shift+PgDn came forward again:\n{}",
        s.text()
    );

    // And any keystroke snaps back, as it does in a terminal emulator.
    s.press(keys::SHIFT_PGUP, "the scrolled-back screen again", |t| {
        t.contains("Shift+PgDn")
    });
    s.press(b"q", "the live screen, snapped back by a keystroke", |t| {
        !t.contains("Shift+PgDn")
    });
    assert!(
        s.has_line("hcmd-line-40"),
        "typing returned the view to the live screen:\n{}",
        s.text()
    );
    let prompt_row = s
        .lines()
        .into_iter()
        .rev()
        .find(|line| line.contains(PROMPT))
        .unwrap_or_default();
    assert!(
        prompt_row.trim_end().ends_with('q'),
        "and the keystroke itself went to the shell, not to the scrollback, \
         got {prompt_row:?}:\n{}",
        s.text()
    );

    // Plain PgUp is not a scrollback key. It is forwarded like every other
    // key, which is what leaves it available to the pager the design promises
    // a real terminal to - so the view does not move.
    s.send(b"\x1b[5~");
    s.settle();
    assert!(
        !s.text().contains("Shift+PgDn"),
        "plain PgUp belongs to the program in the console, not to the \
         scrollback:\n{}",
        s.text()
    );
    assert!(
        s.has_line("hcmd-line-40"),
        "so the live screen is still what is showing:\n{}",
        s.text()
    );
}

// ---------------------------------------------------------------------------
// 9. An interactive program holds the terminal
// ---------------------------------------------------------------------------

/// "its output visible and its pager interactive … Nothing about a
/// running command is hidden or summarised - a program that wants a terminal
/// gets a real one. `Ctrl+C` kills it and returns."
///
/// `cat` with no arguments is the smallest program that *holds* a terminal: it
/// reads until it is interrupted, and every line typed at it comes back. Two
/// identical rows are the proof - one is the tty's echo of what was typed, the
/// other is `cat` writing it out again.
///
/// **What "and returns" means here.** It is the shell's prompt that comes back,
/// and `Ctrl+O` that brings the panels back. the "returns to the
/// panels immediately" is in the paragraph about *a command started from the
/// panels* - the `execute_in = "console"`, which is a one-shot
/// execution and is not in v0.3. A console session
/// the user opened with `Ctrl+O` stays open until `Ctrl+O` closes it, which is
/// what the `switch_on_run` and every prior file manager do; a
/// console that threw the panels back over the screen after every `echo` would
/// be unusable for the interactive work this criterion is about.
#[test]
fn criterion_9_an_interactive_program_holds_the_terminal_until_ctrl_c() {
    let Some((_fix, mut s)) = session("c9") else {
        return no_bash("the interactive-program criterion");
    };

    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });
    s.send(b"cat\r");
    s.send(b"hcmd-echo-me\r");
    s.wait("cat to echo the line back", |s| {
        s.line_count("hcmd-echo-me") == 2
    });
    assert!(
        s.text().contains("cat"),
        "the program that is holding the terminal is on screen:\n{}",
        s.text()
    );
    // It is still holding the terminal: there is no new prompt under the echo.
    let after_echo = s
        .lines()
        .iter()
        .skip_while(|line| *line != "hcmd-echo-me")
        .filter(|line| line.contains(PROMPT))
        .count();
    assert_eq!(
        after_echo,
        0,
        "cat still has the terminal, so the shell has drawn no prompt \
:\n{}",
        s.text()
    );

    // Ctrl+C kills it. In the console the key keeps its interrupt meaning,
    // so it reaches the program rather than the clipboard.
    s.send(keys::CTRL_C);
    s.wait("the shell's prompt to come back", |s| {
        s.prompt_below("hcmd-echo-me")
    });
    assert!(
        s.line_count("hcmd-echo-me") == 2,
        "what cat printed is still on the screen:\n{}",
        s.text()
    );

    // The shell is usable again, which is what "killed it" has to mean.
    s.send(b"echo hcmd-after-cat\r");
    s.wait("a command after the interrupt", |s| {
        s.has_line("hcmd-after-cat")
    });

    // And Ctrl+O brings the panels back, unharmed.
    s.press(keys::CTRL_O, "the panels to come back", |t| {
        t.contains("thunder")
    });
    assert!(
        s.cmdline_text().contains(PROMPT),
        "with the shell's prompt at the foot of them:\n{}",
        s.cmdline_text()
    );
}

// ---------------------------------------------------------------------------
// 10. Quitting with a live shell
// ---------------------------------------------------------------------------

/// the "a panic hook that leaves the user's terminal in raw mode is a
/// bug", applied to the exit path that has a second terminal behind it, plus
/// "A file manager that leaves orphaned shells
/// behind is a bug."
///
/// Both routes out are checked, because they are different code:
///
/// * `F10` from the panels, with a live shell holding scrollback behind them;
/// * `SIGTERM` while the shell has the whole screen. `F10` cannot be used for
///   that half - in the full-screen console the function keys are the running
///   program's - and a terminal being closed is
///   exactly how a session with the console open ends in practice.
#[test]
fn criterion_10_quitting_restores_the_terminal_and_leaves_no_orphan_shell() {
    let Some((_fix, mut s)) = session("c10") else {
        return no_bash("the quit criterion");
    };

    // -- F10, with a live shell behind the panels --------------------------
    let hcmd = s.pid();
    let shell = the_shell_of(hcmd);

    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });
    s.send(b"echo hcmd-still-here\r");
    s.wait("the shell to be doing something", |s| {
        s.has_line("hcmd-still-here")
    });
    s.press(keys::CTRL_O, "the panels to come back", |t| {
        t.contains("thunder")
    });

    // `ui.confirm_exit` is on by default, so `F10` asks first.
    s.press(keys::F10, "the quit prompt", |t| {
        t.contains("Quit Holos Commander?")
    });
    s.send(b"y");
    let ok = s.wait_exit(Duration::from_secs(15));
    assert_eq!(ok, Some(true), "F10 quits cleanly:\n{}", s.text());
    assert_terminal_restored(&s, "F10 with a live shell");
    wait_for_process_to_go(shell, "F10 with a live shell");

    // -- SIGTERM, with the shell holding the screen ------------------------
    let Some((_fix, mut s)) = session("c10b") else {
        return;
    };
    let hcmd = s.pid();
    let shell = the_shell_of(hcmd);
    s.press(keys::CTRL_O, "the shell's screen", |t| {
        !t.contains("thunder")
    });

    let killed = std::process::Command::new("kill")
        .args(["-TERM", &hcmd.to_string()])
        .status()
        .expect("send SIGTERM to hcmd");
    assert!(killed.success(), "kill -TERM {hcmd} failed");

    let ok = s.wait_exit(Duration::from_secs(15));
    assert_eq!(
        ok,
        Some(true),
        "SIGTERM with the console open is a graceful quit:\n{}",
        s.text()
    );
    assert_terminal_restored(&s, "SIGTERM with the console open");
    wait_for_process_to_go(shell, "SIGTERM with the console open");
}

/// the alternate screen is left and the cursor is shown *after*
/// raw mode has been turned off. Raw mode is a termios flag and emits no
/// sequence of its own, so the `Show` arriving last is the observable proof.
fn assert_terminal_restored(s: &Session, what: &str) {
    assert!(
        !s.parser.screen().alternate_screen(),
        "{what}: the alternate screen is left on exit"
    );
    let raw = s.raw_text();
    let entered = raw.find(restore::ALT_ON);
    let left = raw.rfind(restore::ALT_OFF);
    let shown = raw.rfind(restore::CURSOR_SHOWN);
    assert!(
        entered.is_some() && left > entered,
        "{what}: the alternate screen was entered and then left"
    );
    assert!(
        shown > left,
        "{what}: the cursor is shown last, after disable_raw_mode"
    );
}

// ---------------------------------------------------------------------------
// 11. F4 hands the terminal over and takes it back whole
// ---------------------------------------------------------------------------

/// "leave alternate screen → restore cooked mode → pop keyboard
/// enhancement flags → spawn with inherited stdio → wait → re-enter alternate
/// screen → raw mode → push flags → force full redraw → reread the panel."
///
/// **Every step taken has to be put back**, and the one that was not is mouse
/// reporting: `Term::restore` disables it on the way out and nothing turned it
/// back on, so a session started with `ui.mouse = true` came back from a single
/// `F4` with the mouse silently off for the rest of its life. The editor here
/// is a `printf` on the real terminal, which is also how "inherited stdio"
/// becomes observable: the marker lands in the byte stream between leaving the
/// alternate screen and re-entering it, where nothing this application draws
/// ever goes.
#[test]
fn criterion_11_f4_hands_the_terminal_over_and_puts_every_step_back() {
    let Some(bash) = which_bash() else {
        return no_bash("the F4 round-trip criterion");
    };
    let fix = Fixture::new("c11");
    let mut s = Session::start_with_config(
        bash,
        120,
        30,
        fix.path(),
        Some(concat!(
            "[ui]\n",
            "mouse = true\n",
            "[editor]\n",
            "command = \"/bin/sh\"\n",
            "args = [\"-c\", \"printf HCMD-EDITOR-RAN\"]\n",
        )),
    );
    s.ready();

    assert!(
        s.raw_text().contains(mouse::ON),
        "`ui.mouse = true` turned mouse reporting on at startup"
    );

    // Onto a file: `..` is row 0 and `[subdir]` is row 1.
    s.press(keys::DOWN, "the cursor on [subdir]", |_| true);
    s.press(keys::DOWN, "the cursor on a file", |_| true);
    // The name and the extension are separate columns, so the
    // row reads `alpha              rs`.
    assert!(
        s.cursor_entry().contains("alpha") && !s.cursor_entry().contains("subdir"),
        "the cursor is on a file, not a directory: {:?}",
        s.cursor_entry()
    );

    let before = s.raw_text().len();
    s.send(keys::F4);
    s.wait("the editor to have run and the panels to come back", |s| {
        s.raw_text()[before..].contains("HCMD-EDITOR-RAN") && s.text().contains("thunder")
    });

    let after_editor = s.raw_text();
    let tail = after_editor
        .split_once("HCMD-EDITOR-RAN")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_default();
    assert!(
        tail.contains(restore::ALT_ON),
        "the alternate screen is re-entered after the editor"
    );
    assert!(
        tail.contains(mouse::ON),
        "and mouse reporting is put back with it: `ui.mouse` must not be \
         silently turned off by pressing F4"
    );
}

// ---------------------------------------------------------------------------
// 12. Closing the terminal window keeps the tabs
// ---------------------------------------------------------------------------

/// "Tabs are persisted per panel to
/// `~/.local/state/holoscommander/` and restored on start" - with no exception
/// for how the session ended.
///
/// `SIGHUP` is how a session actually ends: the terminal window closes. It used
/// to reach a signal guard that restored the terminal and called
/// `std::process::exit`, which runs no destructors and never reaches the save -
/// so a `kill -TERM` kept the session's tabs and closing the window lost them.
#[test]
fn criterion_12_closing_the_terminal_keeps_the_tabs() {
    let Some((fix, mut s)) = session("c12") else {
        return no_bash("the tab-persistence criterion");
    };

    // A second tab, moved into `subdir` so the saved state is distinguishable
    // from the directory the session started in.
    s.press(keys::CTRL_T, "a second tab", |t| t.contains("thunder"));
    s.press(keys::DOWN, "the cursor on [subdir]", |_| true);
    // The name and the extension are separate columns, so the
    // listing is recognised by the path on the panel's border.
    s.press(keys::ENTER, "the subdirectory's listing", |t| {
        t.contains("/subdir") && t.contains("inner")
    });

    let pid = s.pid();
    assert!(
        std::process::Command::new("kill")
            .args(["-HUP", &pid.to_string()])
            .status()
            .is_ok_and(|st| st.success()),
        "sent SIGHUP to hcmd"
    );
    assert_eq!(
        s.wait_exit(Duration::from_secs(10)),
        Some(true),
        "hcmd ended cleanly rather than being killed where it stood"
    );

    let saved = s.saved_tabs().unwrap_or_default();
    assert!(
        saved.contains("subdir"),
        "the tab open when the terminal closed is in the state file \
:\n{saved}"
    );
    assert!(
        saved.contains(&fix.path().display().to_string()),
        "and so is the panel it belonged to:\n{saved}"
    );
}
