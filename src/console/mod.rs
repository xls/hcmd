//! The persistent shell on a PTY.
//!
//! > A **persistent** shell process on a PTY (`portable-pty`), started at
//! > application start, not on first `Ctrl+O`. Its scrollback survives
//! > toggling, so the output of a long build is still there when you come back.
//!
//! # Shape
//!
//! ```text
//!   reader thread  ──bytes──▶  tokio mpsc  ──▶  event loop  ──▶  Console::feed
//!   writer thread  ◀──bytes──  std mpsc    ◀──  event loop  ◀──  Console::write
//! ```
//!
//! Two threads, one channel each, and **the UI task never blocks on either**.
//! That is the same shape `crate::term::events` already uses for the keyboard,
//! and it is not decoration: a PTY read blocks until the shell says something,
//! and a PTY write blocks when the shell is not reading - a `cat` with a
//! stopped reader fills the kernel buffer and the writer waits. Neither may
//! ever be on the path that draws a frame.
//!
//! The `vt100` parser lives here, on the UI side, so the parsed screen has a
//! single owner and rendering needs no lock.
//!
//! # A shell that dies does not take the application with it
//!
//! The reader thread reports EOF, [`Console::is_alive`] goes false, and the
//! exit status is kept for the message. Nothing panics, nothing exits: the
//! panels are still a file manager. `Ctrl+O` on a dead console offers to start
//! a new one ([`crate::console::Pane::restart_requested`]).
//!
//! # What this module deliberately does not do
//!
//! It does not render. [`Console::screen`] hands out the parsed screen and
//! [`Console::input_block`] hands out the geometry of the command
//! line; painting cells is `crate::ui`'s. It does not dispatch keys either -
//! [`keys::encode`] turns a key into bytes and `crate::input` decides which
//! keys get that far.

pub mod hooks;
pub mod keys;
pub mod osc;
pub mod pane;
pub mod sync;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::config::ConsoleConfig;
use crate::error::{Error, Result};

pub use hooks::Shell;
pub use keys::TerminalMode;
pub use osc::{Osc7Reject, PromptMark};
pub use pane::Pane;

/// How many bytes the reader thread hands over at a time.
///
/// A build writing megabytes should reach the parser in few, large pieces; a
/// prompt should not wait for a buffer to fill. `read` returns what is there,
/// so this is a ceiling rather than a quantum.
pub const READ_CHUNK: usize = 16 * 1024;

/// The smallest grid the emulator is ever given, as `(rows, cols)`.
///
/// **`vt100` 0.16.2 panics below this.** `Screen::write_contents` computes
/// `size.cols - width` for a character of display width 2, and `Grid::col_wrap`
/// computes `prev_pos.row -= scrolled` when a wrap scrolls - so one wide
/// character in a one-column grid, or any wrap in a one-row grid, subtracts
/// with overflow inside the crate. Verified against 0.16.2 directly: 1x1, 1x2,
/// 2x1, 1x3 and 3x1 all panic on `日本`; 2x2 and above do not.
///
/// the design says a 1x1 terminal must not crash, and a `#![forbid(unsafe_code)]`
/// crate cannot catch someone else's arithmetic overflow, so the size is
/// clamped before it ever reaches the parser. Nothing is lost: the design draws
/// a "terminal too small" message below 60x15, and there is no console to look
/// at there anyway.
pub const MIN_GRID: (u16, u16) = (2, 2);

/// How many times [`Console::reap`] asks before giving up.
///
/// See that function: the PTY closing and the child becoming reapable are two
/// events, in that order, and a single `try_wait` loses the race often enough to
/// leave a zombie behind.
const REAP_ATTEMPTS: u32 = 20;

/// How long [`Console::reap`] waits between attempts - 20 x 5ms, so a tenth of a
/// second at the very worst, and nothing at all in the ordinary case where the
/// child is already reapable on the first ask.
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// The queue depth between the reader thread and the UI task.
///
/// Bounded, deliberately: an unbounded channel in front of a `yes` loop is a
/// memory leak with extra steps. When it fills, the reader thread blocks, the
/// PTY buffer fills behind it, and the shell is throttled - which is exactly
/// what a terminal that cannot keep up does.
pub const OUTPUT_CHANNEL_DEPTH: usize = 64;

/// What the reader thread has to say.
#[derive(Debug)]
pub enum ConsoleEvent {
    /// Bytes from the shell, to be fed to the parser.
    Output(Vec<u8>),
    /// The PTY closed: the shell has gone (the design - report it, do not
    /// die with it).
    Eof,
    /// Reading the PTY failed. Treated exactly as [`ConsoleEvent::Eof`] plus a
    /// reason to show.
    Failed(String),
}

/// How a shell ended, in the terms the design needs to report it.
///
/// `portable_pty::ExitStatus` is not `PartialEq` and carries no `Option`, so
/// this is the shape the UI holds: a code, a signal name, or neither when the
/// child was gone before it could be reaped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitReport {
    /// The exit code, when there is one.
    pub code: Option<u32>,
    /// The signal that killed it, already named.
    pub signal: Option<String>,
}

impl std::fmt::Display for ExitReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.signal, self.code) {
            (Some(signal), _) => write!(f, "killed by {signal}"),
            (None, Some(0)) => f.write_str("exited"),
            (None, Some(code)) => write!(f, "exited with status {code}"),
            (None, None) => f.write_str("exited"),
        }
    }
}

/// Where the command line is on the console's screen.
///
/// Rows are screen rows in the parsed [`vt100::Screen`], counted from the top
/// of the visible area. `first_row..=last_row` is the whole prompt block - as
/// many rows as a multi-line prompt needs - and the cursor is the shell's own
/// caret, which is what the persistent caret has become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBlock {
    /// The first row of the prompt.
    pub first_row: u16,
    /// The last row of the block; the cursor is on it or above it.
    pub last_row: u16,
    /// The shell's cursor row.
    pub cursor_row: u16,
    /// The shell's cursor column.
    pub cursor_col: u16,
    /// Where the *input* starts, when `OSC 133 ; B` said so. `None` means the
    /// prompt/input split is unknown and nothing may be inferred from it.
    pub input_start: Option<(u16, u16)>,
}

impl InputBlock {
    /// How many rows the block occupies.
    pub const fn rows(&self) -> u16 {
        self.last_row
            .saturating_sub(self.first_row)
            .saturating_add(1)
    }
}

/// A colour from the parsed screen, in the three forms a terminal can express
/// one.
///
/// `vt100::Color` is not re-exported directly so that the renderer has exactly
/// one conversion to write and `crate::ui` needs no opinion about `vt100`.
/// [`CellColor::Default`] is the terminal's own default - the design wants "the
/// same screen the shell would have had on its own", so it maps to
/// `ratatui::style::Color::Reset` rather than to a theme slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// The terminal's default foreground or background.
    Default,
    /// One of the 256 indexed colours.
    Indexed(u8),
    /// A direct colour, to be quantized for the session's depth.
    ///
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for CellColor {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(i) => Self::Indexed(i),
            vt100::Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

/// The OSC sink the parser reports through.
///
/// It is the `vt100` callbacks object, so it is called *during* parsing, with
/// the screen in exactly the state the escape sequence left it in - which is
/// what makes `OSC 133`'s positions exact rather than a guess after the fact.
#[derive(Debug, Default)]
struct OscSink {
    /// This machine's name, for the authority check.
    host: String,
    /// The most recent accepted `OSC 7`. Only the newest matters.
    cwd: Option<PathBuf>,
    /// Where `OSC 133 ; A` put the start of the prompt.
    prompt_start: Option<(u16, u16)>,
    /// Where `OSC 133 ; B` put the start of the input.
    input_start: Option<(u16, u16)>,
    /// Whether a command is running: `OSC 133 ; C` seen without its `; D`.
    ///
    /// the completion indicator is about "a background command",
    /// and this is the only thing in the stream that can tell one from the
    /// shell redrawing its own prompt or echoing what is being typed.
    running: bool,
    /// A command **started** somewhere in the batch being parsed.
    ///
    /// [`Console::feed`] takes this, so it survives a batch that both starts
    /// and finishes a command. A whole `echo hi` - its `; C`, its output and
    /// the next prompt's `; A` - routinely arrives in a single PTY read, and
    /// sampling [`OscSink::running`] after the batch reports `false` for it:
    /// the command that lit the completion indicator would then be
    /// exactly the one the indicator never mentions.
    started: bool,
    /// A command **ended** somewhere in the batch being parsed, having been
    /// running when it did. Taken by [`Console::feed`], for the same reason.
    ended: bool,
    /// An `OSC 7` in this batch named a host that is not this machine.
    ///
    /// the design ignores it for the shell → panel half, and this is what
    /// lets the *panel → shell* half decline as well: inside `ssh` the shell on
    /// the other end of the PTY is on another machine, and a local `cd` written
    /// into it is a directory change nobody asked for.
    foreign: bool,
}

impl vt100::Callbacks for OscSink {
    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        match osc::decode_osc7(params, &self.host) {
            Ok(path) => {
                self.cwd = Some(path);
                self.foreign = false;
                return;
            }
            // the design ignores an `OSC 7` naming another machine - but
            // *which* machine it named is news in itself: the shell on the
            // other end is somewhere this application cannot reach, so the
            // panel → shell `cd` has to stop until a local one arrives. See
            // `crate::console::sync::CwdSync`.
            Err(Osc7Reject::ForeignHost(_)) => {
                self.foreign = true;
                return;
            }
            Err(_) => {}
        }
        // Positions are read *now*: `vte` dispatches in stream order, so the
        // cursor is standing on the marked cell.
        match osc::decode_osc133(params) {
            Some(PromptMark::PromptStart) => {
                self.prompt_start = Some(screen.cursor_position());
                self.input_start = None;
                // A shell that emits `; A` but never `; D` - the guarantee is
                // only that a prompt follows the command, so this is the mark
                // that is certain to arrive.
                if self.running {
                    self.ended = true;
                }
                self.running = false;
            }
            Some(PromptMark::InputStart) => self.input_start = Some(screen.cursor_position()),
            Some(PromptMark::OutputStart) => {
                // The command was submitted; the marks belong to the prompt
                // that is now history, and everything printed from here until
                // the next prompt is the command's own output.
                self.prompt_start = None;
                self.input_start = None;
                self.running = true;
                self.started = true;
            }
            Some(PromptMark::CommandEnd(_)) => {
                if self.running {
                    self.ended = true;
                }
                self.running = false;
            }
            None => {}
        }
    }
}

/// What one batch of PTY output said, beyond the cells it drew.
///
/// Returned by [`Console::feed`], and every field is about the **batch** rather
/// than about the state left at the end of it. That distinction is the whole
/// point: a `read` on a PTY returns whatever has arrived, and an `echo hi`
/// routinely arrives as one chunk holding its `OSC 133 ; C`, its output and the
/// next prompt's `; A`. Sampling the state afterwards says "no command is
/// running", which is true and useless - the command that the design's
/// completion indicator exists for would be exactly the one it never lit for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedReport {
    /// The directory the shell reported through `OSC 7`, when it reported one
    /// in this batch and it named this machine. A batch with
    /// several - a `for d in …; do cd $d; done` - yields the last, which is
    /// where the shell now is.
    pub cwd: Option<PathBuf>,
    /// An `OSC 7` in this batch named another host: the shell is not on this
    /// machine. `ssh` in the console is the ordinary way.
    pub foreign_host: bool,
    /// A command started in this batch (`OSC 133 ; C`).
    pub command_started: bool,
    /// A command that was running ended in this batch (`OSC 133 ; D`, or the
    /// `; A` of the prompt that follows it).
    pub command_ended: bool,
}

/// The running shell.
pub struct Console {
    /// The master side. Kept for `resize`.
    master: Box<dyn MasterPty + Send>,
    /// The child, for `try_wait` and for killing it on the way out.
    child: Box<dyn Child + Send + Sync>,
    /// Bytes on their way to the shell. A bounded queue would drop keystrokes;
    /// this one is drained by a thread whose only job is to block on the write.
    to_shell: Option<std_mpsc::Sender<Vec<u8>>>,
    /// The emulator. Owns the screen and the OSC sink.
    parser: vt100::Parser<OscSink>,
    /// The shell that was started, for messages and for the snippet.
    shell: Shell,
    /// The program that was actually executed.
    program: String,
    /// False once the PTY has closed.
    alive: bool,
    /// How it ended, once it has.
    exit: Option<ExitReport>,
    /// Why reading stopped, when it stopped badly.
    failure: Option<String>,
    /// The current size, so a resize that changes nothing costs nothing.
    size: (u16, u16),
    /// The last input block whose rows were not all blank.
    ///
    /// A shell redraws its prompt as carriage-return, erase-line, then the text -
    /// three sequences, which do not have to arrive in one PTY read. Drawing
    /// between the erase and the text renders an empty row, so the command line
    /// blinks black on every prompt redraw, which is every `cd` and every
    /// command. The rendered content is held here and drawn
    /// instead whenever the live rows are momentarily empty: a prompt that is
    /// about to be rewritten looks the same as the one being replaced far more
    /// often than it looks like nothing.
    last_block: Option<StableBlock>,
    /// How many rows the prompt took the last time the shell was **idle**.
    ///
    /// The docked command line's height comes from the prompt, and from nothing
    /// else. Taking it from the live block instead makes it jump every time the
    /// shell echoes something: writing a `cd` for the panel→shell sync puts the
    /// echoed command on one row and the new prompt on the next, so the region
    /// is briefly two rows and the panels are briefly one row shorter - a
    /// visible jolt on every tab switch and every directory change, for a
    /// command the user did not type and cannot see.
    ///
    /// So it is measured only at a prompt with nothing typed, and held through
    /// everything else. Content stays live - what is typed appears as it is
    /// typed - and a block taller than the held height is shown from the
    /// bottom, which is the end that has the cursor on it.
    idle_rows: u16,
    /// When output last arrived from the shell, and when the user last typed.
    ///
    /// A shell repaints its prompt with several escape sequences that need not
    /// arrive together, so the docked command line can be drawn half-written -
    /// not blank, just wrong for a frame. Holding the rendering until the
    /// output has been quiet for [`SETTLE`] removes that, because a repaint is
    /// a burst and the quiet marks its end.
    ///
    /// The debounce applies to **shell-initiated** repaints only. Bytes the
    /// user just typed are echoed back within milliseconds, and delaying those
    /// would be laggy typing in exchange for nothing: nobody is bothered by
    /// their own keystroke appearing.
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    /// Whether the command the shell is running is one this application wrote.
    ///
    /// A running command's blank input row is normally real and is shown as-is
    /// (see [`Console::refresh_stable_block`]). The cwd sync's `cd` is the
    /// exception: it is a command with no output and no user behind it, so its
    /// blank row is not news, it is the flicker this whole mechanism exists to
    /// stop. `App` keeps its own flag for the *completion indicator*, which is
    /// a separate question about a different part of the screen.
    internal_running: bool,
}

/// How long the shell's output must be quiet before a repaint is drawn.
pub const SETTLE: Duration = Duration::from_millis(50);

/// How long after the user types that echo is drawn without waiting.
pub const ECHO_WINDOW: Duration = Duration::from_millis(250);

/// Who asked for bytes to go to the shell.
///
/// This application writes to the shell for two quite different reasons, and
/// the docked command line has to tell them apart. A keystroke is echoed back
/// within milliseconds and the user is waiting to see it, so it is drawn at
/// once. The `cd` written for the cwd sync is a command the user
/// did not type and cannot see: drawing its echo, its blank line and its
/// reprinted prompt is three frames of noise for something that was supposed to
/// be invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The user typed it. Echo is drawn without waiting.
    Typed,
    /// This application generated it. Its repaint is debounced like any other
    /// shell-initiated one, and the prompt from before it is held meanwhile.
    Internal,
}

/// A rendered copy of the shell's input block, kept so a prompt mid-redraw is
/// never drawn as an empty row. See [`Console::last_block`].
#[derive(Debug, Clone)]
pub struct StableBlock {
    /// Where it was, so the caret lands in the same place.
    pub block: InputBlock,
    /// Its cells, top row first, exactly as they were on the shell's screen.
    pub rows: Vec<Vec<Option<vt100::Cell>>>,
}

impl Console {
    /// Start the shell.
    ///
    /// `cwd` is where it starts - the active panel's directory, so the shell
    /// and the panel agree from the first prompt rather than after the first
    /// `cd`. `tx` is the reader thread's channel, owned by the caller for the
    /// same reason [`crate::term::spawn_event_thread`]'s is: the event loop
    /// selects on it.
    ///
    /// Fails only when the PTY cannot be opened or the shell cannot be
    /// executed. Everything after that is reported through `tx`.
    pub fn spawn(
        cfg: &ConsoleConfig,
        cwd: &Path,
        size: (u16, u16),
        tx: mpsc::Sender<ConsoleEvent>,
    ) -> Result<Self> {
        let program = shell_program(cfg);
        let shell = Shell::detect(&program);
        let (rows, cols) = clamp_grid(size.0, size.1);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| Error::msg(format!("could not open a pty: {err}")))?;

        let mut cmd = CommandBuilder::new(&program);
        cmd.cwd(cwd);
        // We are the terminal, and `vt100` is what we are. Announcing the
        // *outer* terminal would invite sequences this emulator does not parse
        // - a kitty-graphics image or a Sixel would land on the screen as
        // rubbish - so the child is told what is actually on the other end.
        cmd.env("TERM", "xterm-256color");
        // A shell rc file that wants to know is entitled to (the design
        // documents the snippet for shells we do not inject into).
        cmd.env("HCMD_CONSOLE", "1");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| Error::msg(format!("{program}: could not start the shell: {err}")))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| Error::msg(format!("could not read the pty: {err}")))?;
        spawn_reader(reader, tx);

        let writer = pair
            .master
            .take_writer()
            .map_err(|err| Error::msg(format!("could not write to the pty: {err}")))?;
        let to_shell = spawn_writer(writer);

        let sink = OscSink {
            host: osc::hostname(),
            ..OscSink::default()
        };
        let parser = vt100::Parser::new_with_callbacks(rows, cols, cfg.scrollback, sink);

        let mut console = Self {
            master: pair.master,
            child,
            to_shell: Some(to_shell),
            parser,
            shell,
            program,
            alive: true,
            exit: None,
            failure: None,
            size: (rows, cols),
            last_block: None,
            idle_rows: 1,
            last_output: None,
            last_input: None,
            internal_running: false,
        };

        // the snippet, installed after the shell has read its own
        // configuration rather than instead of it. See `hooks`.
        if cfg.inject_hooks
            && let Some(snippet) = hooks::snippet(console.shell)
        {
            let mut line = snippet.as_bytes().to_vec();
            line.push(b'\r');
            console.write(&line, Origin::Typed);
        }

        Ok(console)
    }

    /// Which shell is running.
    pub const fn shell(&self) -> Shell {
        self.shell
    }

    /// The program that was executed, for a message that names it.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The parsed screen - the rendering source of truth.
    ///
    /// Cells are read straight out of it, so nothing is copied to draw a frame.
    /// [`CellColor`] is the one conversion the renderer needs.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// The modes that change what a key encodes to.
    pub fn mode(&self) -> TerminalMode {
        TerminalMode::from_screen(self.screen())
    }

    /// Whether a program has taken the console's **alternate screen**.
    ///
    /// `vim`, `less`, `fzf` - anything that asks for `ESC[?1049h`. While it is
    /// true there is no shell input line on screen at all: the grid in view
    /// belongs to that program, and the command line has nothing to
    /// render, so the renderer says so instead of copying somebody's text out
    /// of the alternate grid and presenting it as a prompt.
    pub fn alternate_screen(&self) -> bool {
        self.screen().alternate_screen()
    }

    /// Whether the shell is still running.
    pub const fn is_alive(&self) -> bool {
        self.alive
    }

    /// How the shell ended, once it has.
    pub const fn exit(&self) -> Option<&ExitReport> {
        self.exit.as_ref()
    }

    /// Why reading the PTY stopped, when it stopped badly rather than cleanly.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// One line describing a dead shell, for the status line.
    pub fn death_notice(&self) -> String {
        let how = match (&self.failure, &self.exit) {
            (Some(why), _) => why.clone(),
            (None, Some(exit)) => exit.to_string(),
            (None, None) => "exited".to_string(),
        };
        format!("{}: {how} - Ctrl+O starts a new one", self.program)
    }

    /// Resize the PTY and the emulator together ("the PTY is
    /// resized with the terminal").
    ///
    /// A resize that changes nothing does nothing: `SIGWINCH` on every frame
    /// would interrupt the shell's read for no reason.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = clamp_grid(rows, cols);
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        self.parser.screen_mut().set_size(rows, cols);
        // A failure here is not fatal: the emulator has already been resized,
        // so the picture is right even if the shell was not told.
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// The size the PTY currently believes it has, as `(rows, cols)`.
    pub const fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Send bytes to the shell: keystrokes and commands.
    ///
    ///
    /// Never blocks - the writer thread owns the blocking write. Any input
    /// also returns the view to the live screen, which is what a terminal does
    /// when you type while scrolled back.
    ///
    /// `origin` decides how the resulting repaint is drawn - see [`Origin`].
    pub fn write(&mut self, bytes: &[u8], origin: Origin) {
        if bytes.is_empty() {
            return;
        }
        match origin {
            Origin::Typed => self.last_input = Some(Instant::now()),
            // A command line that repaints itself because of something this
            // application wrote is a shell-initiated repaint as far as the
            // renderer is concerned, whatever set it off.
            Origin::Internal => self.internal_running = true,
        }
        self.scroll_to_live();
        let Some(tx) = self.to_shell.as_ref() else {
            return;
        };
        if tx.send(bytes.to_vec()).is_err() {
            // The writer thread has gone, which means the PTY has closed.
            self.to_shell = None;
            self.alive = false;
        }
    }

    /// Feed output from the reader thread into the emulator.
    ///
    /// Returns what the batch said about the shell - see [`FeedReport`], and
    /// note that every field is taken, so a batch that both starts and ends a
    /// command reports both rather than neither.
    pub fn feed(&mut self, bytes: &[u8]) -> FeedReport {
        self.last_output = Some(Instant::now());
        self.parser.process(bytes);
        let sink = self.parser.callbacks_mut();
        let report = FeedReport {
            cwd: sink.cwd.take(),
            foreign_host: std::mem::take(&mut sink.foreign),
            command_started: std::mem::take(&mut sink.started),
            command_ended: std::mem::take(&mut sink.ended),
        };
        if report.command_ended {
            self.internal_running = false;
        }
        report
    }

    /// Ask the **child** whether it has ended, without waiting for the PTY to
    /// say so.
    ///
    /// EOF on the master is one signal of two, and it is the weaker one: it
    /// means the last *descriptor* on the slave has closed, which a background
    /// job outlives. `sleep 300 &` followed by `exit` leaves the shell gone and
    /// the slave still open, so the reader thread stays blocked in `read`,
    /// [`ConsoleEvent::Eof`] never arrives, and without this the console reports
    /// a live shell for five minutes - drawing a dead prompt at the foot of the
    /// panel view, queueing keystrokes into a PTY nobody reads, and holding the
    /// exited shell as an unreaped zombie the whole time.
    ///
    /// So the child is asked directly, once a frame. Non-blocking: `try_wait`
    /// answers immediately, and a child that has not ended costs one syscall.
    /// Returns true on the transition, so the caller reports it exactly once.
    pub fn poll_exit(&mut self) -> bool {
        if !self.alive {
            return false;
        }
        let Ok(Some(status)) = self.child.try_wait() else {
            return false;
        };
        self.exit = Some(ExitReport {
            code: Some(status.exit_code()),
            signal: status.signal().map(|s| s.to_string()),
        });
        self.alive = false;
        self.to_shell = None;
        // A shell that has gone has no prompt, so the held copy of one must not
        // outlive it - see `stable_input_block`.
        self.last_block = None;
        true
    }

    /// Note that the PTY has closed, and reap the child.
    pub fn closed(&mut self, failure: Option<String>) {
        self.alive = false;
        self.failure = failure;
        self.to_shell = None;
        if self.exit.is_none() {
            self.exit = Some(self.reap());
        }
    }

    /// Ask the child how it ended, **reaping it**, without blocking the UI on a
    /// process that is entitled to ignore us.
    ///
    /// The PTY closing and the child being reapable are two different events,
    /// and they arrive in that order: `EOF` on the master means the last slave
    /// descriptor is gone, which the kernel reports as soon as the child's
    /// descriptors are closed - before `wait` can return its status. A single
    /// `try_wait` therefore loses the race often enough to matter, and losing it
    /// is not cosmetic: `closed` records the answer once, so a `None` there
    /// leaves a **zombie for the lifetime of the application** and the design's
    /// report says "exited" with no status to show for it.
    ///
    /// So it is polled rather than waited on. [`REAP_ATTEMPTS`] tries over
    /// [`REAP_INTERVAL`] each - a few milliseconds in total, and only ever spent
    /// on a shell that has not been reaped yet - and then it gives up rather
    /// than hang. A shell that outlives that is left to `init`, which is the
    /// same fate a terminal emulator gives it; what must not happen is the event
    /// loop blocking on it, because a shell is allowed to take its time and the
    /// panels are still a file manager.
    fn reap(&mut self) -> ExitReport {
        for attempt in 0..REAP_ATTEMPTS {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return ExitReport {
                        code: Some(status.exit_code()),
                        signal: status.signal().map(ToString::to_string),
                    };
                }
                // Not reapable yet. Give it a moment - the descriptors are
                // already closed, so this is the gap between that and the
                // process actually leaving, which is short.
                Ok(None) => {
                    if attempt + 1 < REAP_ATTEMPTS {
                        std::thread::sleep(REAP_INTERVAL);
                    }
                }
                // Already reaped by someone else, or no such child. Either way
                // there is nothing left to wait for.
                Err(_) => break,
            }
        }
        ExitReport::default()
    }

    // ------------------------------------------------------- scrollback -----

    /// Scroll the view by whole rows: positive is back into history
    /// (the "the full scrollback").
    pub fn scroll_by(&mut self, rows: isize) {
        let current = self.screen().scrollback();
        let next = if rows >= 0 {
            current.saturating_add(rows.unsigned_abs())
        } else {
            current.saturating_sub(rows.unsigned_abs())
        };
        // `vt100` clamps to what the scrollback actually holds.
        self.parser.screen_mut().set_scrollback(next);
    }

    /// Back to the live screen.
    pub fn scroll_to_live(&mut self) {
        if self.screen().scrollback() != 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
    }

    /// How far back the view currently is.
    pub fn scroll_offset(&self) -> usize {
        self.screen().scrollback()
    }

    // ------------------------------------------- the command line -

    /// Where the shell's prompt and input line are on the screen.
    ///
    ///
    /// `max_rows` caps how much of the panel view the command line may take;
    /// a taller prompt keeps its **bottom** rows, because that is the half with
    /// the input on it.
    ///
    /// The block's first row comes from `OSC 133 ; A` when the shell marks its
    /// prompt, which is exact for a prompt of any height. Without the mark it
    /// is found by walking up from the cursor over non-blank rows - which is
    /// right for the two-line prompts that make this feature worth having,
    /// because every one of them (starship's `add_newline`, oh-my-posh's, a
    /// hand-rolled `PS1` with a `\n`) leaves a blank row above itself.
    pub fn input_block(&self, max_rows: u16) -> InputBlock {
        let sink = self.parser.callbacks();
        locate_block(self.screen(), sink.prompt_start, sink.input_start, max_rows)
    }

    /// Recompute the block that the frame will be laid out and painted from.
    ///
    /// Called **once per frame, before drawing**, by the event loop - which is
    /// the only place a `&mut Console` exists at the right moment. Three things
    /// consume the result (the layout's row reservation, the painter, and the
    /// caret) and they must agree; storing one value they all read makes that
    /// structural rather than a rule to remember. Measuring the layout from one
    /// block and painting from another is exactly how a fixed flicker came
    /// back.
    ///
    /// Two things are settled here, both explained on the fields:
    ///
    /// * The **height** is the prompt's, taken only at a prompt with nothing
    ///   typed, and held through everything else - so the `cd` the cwd sync
    ///   writes cannot widen the region for a frame.
    /// * The **content** is held while the shell's output is still arriving, so
    ///   a prompt repainted across several PTY reads is never drawn
    ///   half-written.
    pub fn refresh_stable_block(&mut self, max_rows: u16) {
        let mut block = self.input_block(max_rows);
        let scroll = self.scroll_offset();

        if scroll == 0 && self.input_is_empty() == Some(true) {
            self.idle_rows = block.rows().clamp(1, max_rows);
        }
        let want = self.idle_rows.clamp(1, max_rows);
        block.first_row = block.last_row.saturating_sub(want.saturating_sub(1));

        let rows = block_rows(self.screen(), block, scroll);
        let fresh = StableBlock { block, rows };

        // Scrolling back is the one blankness that is *meant*: the input line
        // has moved off the top of the view, and showing the held copy would be
        // putting somebody else's output where the prompt belongs.
        if scroll != 0 {
            self.last_block = Some(fresh);
            return;
        }

        let live_is_blank = fresh.rows.iter().all(|row| {
            row.iter()
                .all(|cell| cell.as_ref().is_none_or(|c| c.contents().trim().is_empty()))
        });

        // Output still arriving: a repaint is a burst of escape sequences and
        // drawing between them shows the prompt half-written. Bytes the user
        // just typed are exempt - nobody is bothered by their own keystroke
        // appearing, and delaying it would be laggy typing for nothing.
        let settling = self.last_output.is_some_and(|at| at.elapsed() < SETTLE)
            && !self.last_input.is_some_and(|at| at.elapsed() < ECHO_WINDOW);

        // A *running* command's blank row is not a race: the shell has echoed
        // the command and its cursor sits below it with no prompt drawn, and
        // the design wants that shown rather than papered over with the
        // prompt from before the command started - unless this application is
        // what started it, which is the `internal_running` case.
        // A *running* command's blank row is shown, but only when the user
        // started it: the cwd sync's `cd` is a command with nothing to show.
        let user_command_running = self.command_running() && !self.internal_running;
        let hold = (settling || (live_is_blank && !user_command_running)) && self.alive;
        if hold && self.last_block.is_some() {
            return;
        }
        self.last_block = Some(fresh);
    }

    /// The block the current frame is laid out and painted from.
    ///
    /// [`Console::refresh_stable_block`] is what fills it; this is the read the
    /// renderer and the layout share.
    pub fn stable_block(&self, max_rows: u16) -> StableBlock {
        self.last_block.clone().unwrap_or_else(|| {
            let block = self.input_block(max_rows);
            let rows = block_rows(self.screen(), block, self.scroll_offset());
            StableBlock { block, rows }
        })
    }

    /// When the settle window (see [`SETTLE`]) will expire, if it is running.
    ///
    /// The event loop uses it as a wake-up: the held rendering is replaced by
    /// the settled one when the output goes quiet, and without a timer that
    /// would not be drawn until the next key arrived.
    pub fn settle_deadline(&self) -> Option<Instant> {
        let at = self.last_output?;
        let due = at.checked_add(SETTLE)?;
        (due > Instant::now()).then_some(due)
    }

    /// The text of the shell's current input line, prompt excluded.
    ///
    ///
    /// `None` when the shell does not mark where its input starts - see
    /// [`osc`] - because the alternative is storing a history entry with
    /// somebody's prompt glued to the front of it. Trailing spaces are dropped:
    /// the screen is a grid and the rest of the row is blank cells, not part of
    /// the command.
    pub fn input_text(&self) -> Option<String> {
        read_input_text(self.screen(), self.parser.callbacks().input_start)
    }

    /// Whether a command is running in the shell.
    ///
    /// True between `OSC 133 ; C` and the `; D` or next `; A` that answers it.
    /// A shell emitting no prompt marks is always `false`: without them the
    /// echo of a keystroke and the output of a build are the same bytes, and
    /// an indicator that cannot tell them apart is worse than none - the same
    /// silent one-way degradation the `cd` sync makes.
    pub fn command_running(&self) -> bool {
        self.parser.callbacks().running
    }

    /// Whether the shell is sitting at a prompt with nothing typed
    /// (the condition for writing a `cd`).
    ///
    /// `None` means "cannot tell", which is not the same as `false` and must
    /// not be treated as one: a `cd` written over a half-typed command line
    /// corrupts it, so the caller acts on `Some(true)` and on nothing else.
    pub fn input_is_empty(&self) -> Option<bool> {
        let text = self.input_text()?;
        Some(text.is_empty())
    }
}

impl Drop for Console {
    /// Close the PTY and stop the shell.
    ///
    /// Dropping the writer sends EOF, which is what a shell reads as the user
    /// hanging up; `kill` is the backstop for one that ignores it. Neither can
    /// fail in a way worth reporting from a destructor.
    fn drop(&mut self) {
        self.to_shell = None;
        // **Only a child we still own.** `portable-pty`'s `ChildKiller` for a
        // `std::process::Child` is `libc::kill(self.id(), SIGHUP)` with no
        // already-waited check - where `std::process::Child::kill` refuses once
        // a status has been stored, precisely so a reaped pid that the kernel
        // has since handed to somebody else is not signalled. `exit` being set
        // means `closed`/`poll_exit` has already reaped this one, so the pid is
        // no longer ours to hang up.
        if self.exit.is_none() {
            let _ = self.child.kill();
        }
        // **And reap it.** `portable-pty`'s `kill` is a `SIGHUP` on unix - not a
        // `SIGKILL`, which is worth knowing - and it does not wait, so without
        // this the child is left unwaited. At process exit `init` would collect
        // it, but this destructor also runs when a dead console is replaced by a
        // new one (`App::set_console`), and there the zombie would outlive every
        // restart. `reap` is bounded and never blocks for long.
        if self.exit.is_none() {
            self.exit = Some(self.reap());
        }
    }
}

/// [`Console::input_block`], as a free function over the parsed screen, so the
/// rule can be tested without a PTY.
/// The cells of an input block, top row first.
pub(crate) fn block_rows(
    screen: &vt100::Screen,
    block: InputBlock,
    scroll: usize,
) -> Vec<Vec<Option<vt100::Cell>>> {
    let (screen_rows, screen_cols) = screen.size();
    (block.first_row..=block.last_row)
        .map(|row| {
            // Scrolled back, this row of the live grid has moved down and may
            // have moved off the bottom entirely; an empty row is then correct.
            let visible = usize::from(row)
                .checked_add(scroll)
                .and_then(|r| u16::try_from(r).ok())
                .filter(|r| *r < screen_rows);
            visible.map_or_else(
                || vec![None; usize::from(screen_cols)],
                |r| {
                    (0..screen_cols)
                        .map(|c| screen.cell(r, c).cloned())
                        .collect()
                },
            )
        })
        .collect()
}

fn locate_block(
    screen: &vt100::Screen,
    prompt_start: Option<(u16, u16)>,
    input_start: Option<(u16, u16)>,
    max_rows: u16,
) -> InputBlock {
    let (rows, _cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_row = cursor_row.min(rows.saturating_sub(1));
    let max_rows = max_rows.max(1);
    let ceiling = cursor_row.saturating_sub(max_rows.saturating_sub(1));

    let marked = prompt_start
        .filter(|(row, _)| *row <= cursor_row)
        .map(|(row, _)| row);

    // Without a prompt mark the block is **the cursor's row and nothing else**.
    //
    // Walking upward while the row above is non-blank looks reasonable and is
    // not: the row above a prompt is usually the *previous command's echo or
    // output*, so the guess swallows it and reports a two-row prompt for a
    // one-row `PS1`. Worse, how much it swallows changes as output scrolls, so
    // the command line's height oscillates and the panels resize under the user
    // - a visible flicker on every `cd`.
    //
    // A shell that marks its prompt (`OSC 133; A`, injected by `hooks`) gets a
    // genuinely multi-row prompt drawn correctly. A shell that does not - one
    // whose `PS1` was replaced after our hook ran, which is what starship and
    // any hand-rolled prompt do - gets one row, which is right far more often
    // than a guess is and is stable either way. This is the same one-way silent
    // degradation the design makes when OSC 7 never arrives.
    let first = marked.unwrap_or(cursor_row);

    // Keep the bottom `max_rows`: the input is at the end of the block.
    let mut first = first.max(ceiling);

    // Drop blank rows from the top of the block.
    //
    // Starship's `add_newline` - on by default - prints an empty line before
    // the prompt, and it is inside the marked prompt region, so the block comes
    // out one row taller than the prompt anyone would say they have. That blank
    // is breathing room in a scrolling terminal and dead space in a docked
    // command line, where it costs the panels a row. It also moves: whether the
    // row reads as blank changes as output scrolls past, so the region's height
    // oscillates and the panels resize under the user.
    //
    // Only *leading* blanks go. A blank row in the middle of a prompt is the
    // author's spacing between two lines they meant to draw.
    while first < cursor_row && row_is_blank(screen, first) {
        first = first.saturating_add(1);
    }

    InputBlock {
        first_row: first,
        last_row: cursor_row,
        cursor_row,
        cursor_col,
        input_start: input_start.filter(|(row, _)| *row <= cursor_row && *row >= first),
    }
}

/// [`Console::input_text`], as a free function over the parsed screen.
///
/// # Every answer here is a promise about a grid that may have moved
///
/// `input_start` is a position in the **live grid**, recorded by `OSC 133; B`
/// at the instant the prompt was drawn. Everything that reads cells back -
/// `Screen::cell`, `Screen::contents_between`, `Screen::row_wrapped` - is
/// addressed in *visible* rows, and `vt100` 0.16.2 resolves those through
/// `Grid::visible_rows`, which skips the scrollback offset. The two coordinate
/// spaces coincide only while the view is live, the primary screen is in front,
/// and nothing has scrolled since the mark was taken. Where they do not, the
/// row read is somebody else's output wearing the input line's position - and
/// the hook then writes `cd '<path>'` on top of a half-typed command, which
/// the shell runs as one line.
///
/// So the mark is **checked against the grid as it is now**, and every check
/// answers `None` rather than a plausible wrong answer:
/// [`Console::input_is_empty`] documents that "cannot tell" is not `false`, and
/// the design acts on `Some(true)` alone. The next prompt re-marks and the
/// reading is exact again.
fn read_input_text(screen: &vt100::Screen, input_start: Option<(u16, u16)>) -> Option<String> {
    let (row, col) = input_start?;
    let (rows, cols) = screen.size();
    // **Both coordinates are checked against the grid as it is now.**
    // `input_start` was recorded by `OSC 133 ; B` when the prompt was drawn, and
    // nothing updates it when the terminal is resized - so after a narrowing it
    // can name a column the grid no longer has. That is not merely a wrong
    // answer: `vt100` 0.16.2's `Screen::contents_between` computes
    // `cols - start_col` unguarded when the block spans more than one row
    // (`screen.rs`, the `Ordering::Less` arm), which underflows and panics.
    // Verified against 0.16.2 directly: a row wrapped at 80 columns, the grid
    // set to 30, the row wrapped again, then `contents_between(0, 40, 1, 30)`
    // panics with "attempt to subtract with overflow".
    //
    // `None` rather than a clamp, because a clamped column would be a
    // *plausible* wrong answer: `Console::input_is_empty` documents that "cannot
    // tell" is not the same as `false`, and the design writes a `cd` only on
    // `Some(true)`. Returning a truncated line here could report an empty input
    // line that is not empty and drop a `cd` on top of what the user was
    // typing. The next prompt re-marks and the reading is exact again.
    if row >= rows || col >= cols {
        return None;
    }
    // **A program owns the alternate screen.** bash's own `Ctrl+X Ctrl+E`
    // launches `$EDITOR` without emitting `OSC 133 ; C`, and so does every
    // readline widget that takes the screen - fzf's `Ctrl+R`, any `bind -x`.
    // The marks still name the primary screen's prompt row while `cell` reads
    // the alternate grid, which is blank there, so the answer would be "the
    // input line is empty" and a `cd '<path>'\r` would be typed into vim.
    if screen.alternate_screen() {
        return None;
    }
    // **The view is scrolled back.** `Shift+PgUp` in the console leaves an
    // offset that nothing resets until the next write, and at any offset the
    // marked row reads as whatever history has moved into its place.
    if screen.scrollback() != 0 {
        return None;
    }
    // **The grid scrolled under the mark.** Output arriving while the user is
    // typing - a background job's `[1]+ Done`, a program printing a line -
    // moves the prompt up and leaves the mark behind, pointing at a row that
    // now holds that output. `vt100` reports no scroll and nothing updates the
    // mark, so the only defence is to ask whether the mark and the grid still
    // describe the same thing. Three questions do it, and each of them is a
    // property of a prompt that is genuinely where the mark says:
    //
    // 1. The mark is at or above the cursor's row - the cursor is *in* the
    //    input, so it can never be above where the input starts.
    let (cursor_row, cursor_col) = screen.cursor_position();
    if row > cursor_row {
        return None;
    }
    // 2. The rows between the two are one wrapped logical line. A cursor two
    //    rows below the mark on a line that never wrapped is not on the input.
    let mut probe = row;
    while probe < cursor_row {
        if !screen.row_wrapped(probe) {
            return None;
        }
        probe = probe.saturating_add(1);
    }
    // 3. On the mark's own row the cursor is at or after the mark, and the
    //    prompt that pushed the input to that column has left something in
    //    those columns. This is the one that catches the common case, where the
    //    prompt was on the **bottom** row: the grid scrolls, the prompt and its
    //    text move up, and the cursor and the mark are both on the bottom row
    //    again - but that row is now the blank one that scrolled in, with
    //    nothing before the mark and the cursor back at column 0.
    if cursor_row == row {
        if cursor_col < col {
            return None;
        }
        let prompt_drew_something =
            (0..col).any(|c| screen.cell(row, c).is_some_and(vt100::Cell::has_contents));
        if col > 0 && !prompt_drew_something {
            return None;
        }
    }
    // A long command wraps onto the rows below; `vt100` records which rows are
    // continuations, so the whole logical line comes back.
    let mut last = row;
    while last.saturating_add(1) < rows && screen.row_wrapped(last) {
        last = last.saturating_add(1);
    }
    let text = screen.contents_between(row, col, last, cols);
    Some(text.trim_end().to_string())
}

/// Hold a grid size at or above [`MIN_GRID`]. See that constant for why this is
/// not optional.
const fn clamp_grid(rows: u16, cols: u16) -> (u16, u16) {
    (
        if rows < MIN_GRID.0 { MIN_GRID.0 } else { rows },
        if cols < MIN_GRID.1 { MIN_GRID.1 } else { cols },
    )
}

/// Whether every cell on a row is empty.
fn row_is_blank(screen: &vt100::Screen, row: u16) -> bool {
    let (_rows, cols) = screen.size();
    (0..cols).all(|col| {
        screen
            .cell(row, col)
            .is_none_or(|cell| !cell.has_contents())
    })
}

/// The shell to start: `console.shell`, then `$SHELL`, then `/bin/sh`.
///
fn shell_program(cfg: &ConsoleConfig) -> String {
    let configured = cfg.shell.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    std::env::var("SHELL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Read the PTY on its own thread and forward what it says.
///
/// Detached, like the key reader: it ends when the receiver is dropped, and in
/// any case when the process exits.
fn spawn_reader(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<ConsoleEvent>) {
    let fallback = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hcmd-console-read".to_string())
        .spawn(move || {
            let mut buf = vec![0_u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.blocking_send(ConsoleEvent::Eof);
                        return;
                    }
                    Ok(n) => {
                        let chunk = buf.get(..n).unwrap_or_default().to_vec();
                        if tx.blocking_send(ConsoleEvent::Output(chunk)).is_err() {
                            return;
                        }
                    }
                    // EIO is what a Linux pty master reports when the last
                    // slave closes: an ordinary exit, not a failure.
                    Err(err) if err.raw_os_error() == Some(5) => {
                        let _ = tx.blocking_send(ConsoleEvent::Eof);
                        return;
                    }
                    Err(err) => {
                        let _ = tx.blocking_send(ConsoleEvent::Failed(err.to_string()));
                        return;
                    }
                }
            }
        });
    if spawned.is_err() {
        // No reader means the console can never show anything. Say so on the
        // channel rather than leaving a window that never updates.
        let _ = fallback.try_send(ConsoleEvent::Failed(
            "could not start the console reader thread".to_string(),
        ));
    }
}

/// Write to the PTY on its own thread, so a blocked write never reaches the UI.
fn spawn_writer(mut writer: Box<dyn Write + Send>) -> std_mpsc::Sender<Vec<u8>> {
    let (tx, rx) = std_mpsc::channel::<Vec<u8>>();
    let spawned = std::thread::Builder::new()
        .name("hcmd-console-write".to_string())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    return;
                }
            }
            // The sender was dropped: closing the writer sends EOF to the
            // shell, which is the polite way to end the session.
        });
    // A thread that failed to start dropped the closure holding the receiver,
    // so the channel is already closed: `Console::write` notices on the first
    // keystroke and marks the console dead rather than swallowing keys forever.
    drop(spawned);
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console cannot be built without a PTY, so the pieces that are pure -
    /// which is every decision in this file that could be wrong - are tested
    /// through a parser built the same way `Console::spawn` builds one.
    fn parser(rows: u16, cols: u16) -> vt100::Parser<OscSink> {
        let sink = OscSink {
            host: "workstation".to_string(),
            ..OscSink::default()
        };
        vt100::Parser::new_with_callbacks(rows, cols, 100, sink)
    }

    #[test]
    fn osc_7_reaches_the_sink_with_either_terminator() {
        // the design names the BEL form; the ST form is the one zsh's own
        // snippets use, and `vte` dispatches both identically.
        let mut p = parser(4, 40);
        p.process(b"\x1b]7;file:///home/thorin\x07");
        assert_eq!(
            p.callbacks_mut().cwd.take(),
            Some(PathBuf::from("/home/thorin"))
        );
        p.process(b"\x1b]7;file://workstation/srv/media\x1b\\");
        assert_eq!(
            p.callbacks_mut().cwd.take(),
            Some(PathBuf::from("/srv/media"))
        );
    }

    #[test]
    fn a_foreign_host_never_moves_the_panel() {
        let mut p = parser(4, 40);
        p.process(b"\x1b]7;file://build-farm/var/tmp\x07");
        assert_eq!(p.callbacks_mut().cwd.take(), None);
    }

    #[test]
    fn the_prompt_marks_land_where_the_cursor_was() {
        // The whole point of using the callbacks rather than scanning the
        // stream afterwards: `vte` dispatches in order, so the cursor is
        // standing on the marked cell.
        let mut p = parser(4, 40);
        p.process(b"\x1b]133;A\x1b\\thorin@box $ \x1b]133;B\x1b\\ls -la");
        let sink = p.callbacks();
        assert_eq!(sink.prompt_start, Some((0, 0)));
        assert_eq!(sink.input_start, Some((0, 13)));
    }

    #[test]
    fn a_command_being_run_clears_the_marks() {
        let mut p = parser(4, 40);
        p.process(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x1b\\");
        assert_eq!(p.callbacks().input_start, None);
        assert_eq!(p.callbacks().prompt_start, None);
    }

    #[test]
    fn the_input_line_is_read_back_without_the_prompt() {
        // the "at a prompt with an empty input line", which is also
        // the one definition of "the shell is idle". The prompt is on
        // the same row and is not part of the answer.
        let mut p = parser(4, 40);
        p.process(b"\x1b]133;A\x1b\\thorin@box $ \x1b]133;B\x1b\\cargo test");
        let sink_input = p.callbacks().input_start;
        assert_eq!(
            read_input_text(p.screen(), sink_input).as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn a_narrowed_terminal_does_not_take_vt100_below_zero() {
        // A regression test for a real panic, not a hypothetical one.
        //
        // `input_start` is recorded by `OSC 133 ; B` at the width the prompt was
        // drawn at, and a resize does not update it. `vt100` 0.16.2's
        // `contents_between` computes `cols - start_col` unguarded on a block
        // that spans more than one row, so a stale column past the new width
        // panics with "attempt to subtract with overflow" - inside a crate this
        // one cannot catch, `#![forbid(unsafe_code)]` or not.
        //
        // The sequence below is the one that reaches it: a prompt whose input
        // starts at column 40, the terminal narrowed to 30, and the row wrapped
        // again by whatever the shell drew next. `set_size` clears the wrap
        // flags, which is why the row has to be re-wrapped for the bad arm to be
        // taken at all.
        let mut p = parser(5, 80);
        p.process(b"\x1b]133;A\x1b\\");
        p.process(&[b'#'; 40]);
        p.process(b"\x1b]133;B\x1b\\");
        p.process(&[b'x'; 60]);
        let stale = p.callbacks().input_start;
        assert_eq!(
            stale,
            Some((0, 40)),
            "the mark is recorded at the old width"
        );

        p.screen_mut().set_size(5, 30);
        p.process(b"\x1b[H");
        p.process(&[b'y'; 40]);
        assert!(
            p.screen().row_wrapped(0),
            "the row has to be wrapped for the panicking arm to be taken"
        );

        // Must not panic, and must not invent an answer either: the design
        // acts on `Some(true)` alone, so "cannot tell" has to stay `None`.
        assert_eq!(read_input_text(p.screen(), stale), None);
    }

    #[test]
    fn a_scrolled_back_view_cannot_read_the_input_line() {
        // the design writes a `cd` only on `Some(true)`, and `Shift+PgUp` in
        // the console leaves an offset that nothing resets until the next
        // write. `vt100`'s `contents_between` is addressed in *visible* rows
        // and `input_start` is a live-grid row, so at any offset the mark reads
        // whatever history has moved into its place - which is blank far more
        // often than not. This used to answer `Some("")`, and a panel move then
        // typed `cd '<path>'` on top of a half-typed `rm -rf …`.
        let mut p = parser(5, 40);
        // Enough output to have pushed rows off the top: `set_scrollback`
        // clamps to what the scrollback actually holds, so a screen that has
        // never scrolled cannot be scrolled back.
        for i in 0..20 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        p.process(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\rm -rf important");
        let mark = p.callbacks().input_start;
        assert_eq!(
            read_input_text(p.screen(), mark).as_deref(),
            Some("rm -rf important"),
            "live, the reading is exact"
        );

        for offset in 1..4 {
            p.screen_mut().set_scrollback(offset);
            assert_eq!(
                read_input_text(p.screen(), mark),
                None,
                "at offset {offset} the answer is 'cannot tell', never 'empty'"
            );
        }
        p.screen_mut().set_scrollback(0);
        assert_eq!(
            read_input_text(p.screen(), mark).as_deref(),
            Some("rm -rf important"),
            "and it is exact again the moment the view is live"
        );
    }

    #[test]
    fn a_program_on_the_alternate_screen_has_no_input_line_to_read() {
        // bash's own `Ctrl+X Ctrl+E` launches $EDITOR **without** emitting
        // `OSC 133 ; C` - verified against bash, not assumed - and so does
        // every readline widget that takes the screen (fzf's `Ctrl+R`, any
        // `bind -x`). The prompt marks therefore still point at the primary
        // screen's prompt row while every read goes to the alternate grid,
        // which is blank there. Answering `Some("")` had the design type
        // `cd '<path>'\r` into the program the user was editing in.
        let mut p = parser(6, 40);
        p.process(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\vi notes.txt");
        let mark = p.callbacks().input_start;
        assert_eq!(
            read_input_text(p.screen(), mark).as_deref(),
            Some("vi notes.txt")
        );

        // The program takes the screen without a preexec mark.
        p.process(b"\x1b[?1049h\x1b[H");
        assert!(p.screen().alternate_screen());
        assert_eq!(
            p.callbacks().input_start,
            mark,
            "the marks are untouched, which is exactly the trap"
        );
        assert_eq!(
            read_input_text(p.screen(), mark),
            None,
            "there is no shell input line on screen, so nothing may be inferred"
        );

        // Back on the primary screen the reading is exact again.
        p.process(b"\x1b[?1049l");
        assert_eq!(
            read_input_text(p.screen(), mark).as_deref(),
            Some("vi notes.txt")
        );
    }

    #[test]
    fn output_that_scrolls_the_grid_leaves_the_mark_behind() {
        // A background job's `[1]+ Done` printing while the user types moves
        // the prompt up a row; nothing tells the marks. The row `input_start`
        // names then holds that output - blank at the marked column often
        // enough - and `input_is_empty` answered `Some(true)` for a line that
        // still read `rm -rf build`.
        let mut p = parser(4, 40);
        p.process(b"\x1b[4;1H");
        p.process(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\rm -rf build");
        let mark = p.callbacks().input_start;
        assert_eq!(mark, Some((3, 2)), "the prompt is on the bottom row");
        assert_eq!(
            read_input_text(p.screen(), mark).as_deref(),
            Some("rm -rf build")
        );

        // One line of background output. The grid scrolls; the mark does not.
        p.process(b"\r\n[1]+  Done   sleep 3\r\n");
        assert_eq!(
            read_input_text(p.screen(), mark),
            None,
            "the mark no longer belongs to the line the cursor is on"
        );
    }

    #[test]
    fn an_input_line_that_wrapped_comes_back_whole() {
        let mut p = parser(4, 10);
        p.process(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\abcdefghijkl");
        let sink_input = p.callbacks().input_start;
        assert_eq!(
            read_input_text(p.screen(), sink_input).as_deref(),
            Some("abcdefghijkl")
        );
    }

    #[test]
    fn without_the_input_mark_the_text_is_unknown_rather_than_guessed() {
        // A prompt with the command glued to the front of it is worse than no
        // history entry at all, and the `cd` must not be written
        // over a half-typed line.
        let mut p = parser(4, 40);
        p.process(b"thorin@box $ cargo test");
        assert_eq!(read_input_text(p.screen(), None), None);
    }

    #[test]
    fn the_marked_prompt_gives_a_multi_line_block_exactly() {
        // "A multi-line prompt is drawn as many rows as it
        // needs", which a two-line starship prompt is.
        let mut p = parser(6, 40);
        p.process(b"build output\r\n\x1b]133;A\x1b\\~/src on main\r\n> \x1b]133;B\x1b\\");
        let sink = p.callbacks();
        let block = locate_block(p.screen(), sink.prompt_start, sink.input_start, 4);
        assert_eq!(block.first_row, 1);
        assert_eq!(block.last_row, 2);
        assert_eq!(block.rows(), 2);
        assert_eq!(block.input_start, Some((2, 2)));
    }

    #[test]
    fn a_block_taller_than_the_cap_keeps_its_bottom_rows() {
        // The input is at the *end* of the block, so a prompt too tall for the
        // space loses its top rather than the row being typed on.
        let mut p = parser(6, 40);
        p.process(b"\x1b]133;A\x1b\\one\r\ntwo\r\nthree\r\n> \x1b]133;B\x1b\\");
        let sink = p.callbacks();
        let block = locate_block(p.screen(), sink.prompt_start, sink.input_start, 2);
        assert_eq!(block.first_row, 2);
        assert_eq!(block.last_row, 3);
    }

    /// The height rule, exercised through `locate_block` plus the idle pin the
    /// way `stable_input_block` composes them - no PTY needed.
    fn pinned(idle_rows: u16, block: InputBlock, max_rows: u16) -> u16 {
        let want = idle_rows.clamp(1, max_rows);
        let first = block.last_row.saturating_sub(want.saturating_sub(1));
        InputBlock {
            first_row: first,
            ..block
        }
        .rows()
    }

    #[test]
    fn an_echoed_command_does_not_widen_the_docked_command_line() {
        // Writing a `cd` for the panel→shell sync puts the echoed command on
        // one row and the new prompt on the next, so a height taken from the
        // live block is briefly two and the panels are briefly one row shorter
        // - a jolt on every tab switch, panel switch and directory change, for
        // a command the user did not type and cannot see.
        let mut p = parser(6, 40);
        p.process(b"$ ");
        let idle = locate_block(p.screen(), None, None, 4);
        assert_eq!(idle.rows(), 1, "a one-line prompt, measured while idle");

        // Now the echo of a synthetic cd, and the prompt after it.
        let mut p = parser(6, 40);
        p.process(b"cd /somewhere\r\n$ ");
        let during = locate_block(p.screen(), Some((0, 0)), None, 4);
        assert!(during.rows() >= 2, "the live block really is taller here");
        assert_eq!(
            pinned(idle.rows(), during, 4),
            1,
            "but the docked height is the idle prompt's, so nothing moves"
        );
    }

    #[test]
    fn without_a_prompt_mark_the_block_is_one_row() {
        // Walking upward while the row above is non-blank looks reasonable and
        // is not: the row above a prompt is usually the previous command's echo
        // or output, so the guess swallows it and reports two rows for a
        // one-row PS1 - and how much it swallows changes as output scrolls, so
        // the panels resize under the user on every `cd`.
        let mut p = parser(6, 40);
        p.process(b"build output\r\n\r\n~/src on main\r\n> ");
        let block = locate_block(p.screen(), None, None, 4);
        assert_eq!(block.rows(), 1, "the cursor's row and nothing else");
        assert_eq!(block.first_row, 3);
        assert_eq!(block.last_row, 3);
    }

    #[test]
    fn the_height_does_not_move_as_output_scrolls_past() {
        // The flicker, pinned: same prompt, different things above it.
        for above in [
            &b""[..],
            &b"one line\r\n"[..],
            &b"a\r\nb\r\nc\r\n"[..],
            &b"\r\n\r\n"[..],
        ] {
            let mut p = parser(8, 40);
            p.process(above);
            p.process(b"> ");
            let block = locate_block(p.screen(), None, None, 4);
            assert_eq!(block.rows(), 1, "above = {above:?}");
        }
    }

    #[test]
    fn a_marked_prompt_loses_its_leading_blank_but_keeps_the_rest() {
        // Starship's `add_newline` prints an empty line before the prompt and it
        // is inside the marked region: breathing room in a scrolling terminal,
        // dead space in a docked command line, and it costs the panels a row.
        let mut p = parser(8, 40);
        p.process(b"output\r\n\r\n~/src on main\r\n> ");
        // The mark points at the blank row, as starship's does.
        let block = locate_block(p.screen(), Some((1, 0)), None, 4);
        assert_eq!(block.first_row, 2, "the leading blank is dropped");
        assert_eq!(block.rows(), 2, "and the two real prompt rows are kept");
    }

    #[test]
    fn a_blank_inside_a_marked_prompt_is_the_authors_spacing() {
        // Only *leading* blanks go. One between two lines somebody meant to
        // draw is deliberate.
        let mut p = parser(8, 40);
        p.process(b"top line\r\n\r\nbottom line\r\n> ");
        let block = locate_block(p.screen(), Some((0, 0)), None, 6);
        assert_eq!(block.first_row, 0);
        assert_eq!(block.rows(), 4, "nothing in the middle is trimmed");
    }

    #[test]
    fn the_grid_never_goes_below_the_size_vt100_can_survive() {
        // a 1x1 terminal renders a message rather than crashing.
        // `vt100` 0.16.2 subtracts with overflow below 2x2 - see MIN_GRID - and
        // this is the clamp that keeps that arithmetic out of reach. The
        // assertion below is the proof, not a formality: it is the exact input
        // that panics one size down.
        assert_eq!(clamp_grid(0, 0), MIN_GRID);
        assert_eq!(clamp_grid(1, 1), MIN_GRID);
        assert_eq!(clamp_grid(1, 200), (2, 200));
        assert_eq!(clamp_grid(50, 1), (50, 2));
        assert_eq!(clamp_grid(30, 120), (30, 120));

        let (rows, cols) = clamp_grid(1, 1);
        let mut p = vt100::Parser::new(rows, cols, 10);
        p.process("日本語abc\r\n\x1b[31mxy\x1b[0m".as_bytes());
        assert_eq!(p.screen().size(), MIN_GRID);
    }

    #[test]
    fn a_blank_row_is_a_row_with_nothing_on_it() {
        let mut p = parser(3, 10);
        p.process(b"one\r\n\r\nthree");
        assert!(!row_is_blank(p.screen(), 0));
        assert!(row_is_blank(p.screen(), 1));
        assert!(!row_is_blank(p.screen(), 2));
    }

    #[test]
    fn the_shell_comes_from_config_then_the_environment() {
        let cfg = ConsoleConfig {
            shell: "  /usr/bin/fish  ".to_string(),
            ..ConsoleConfig::default()
        };
        assert_eq!(shell_program(&cfg), "/usr/bin/fish");
        // An empty setting falls through to $SHELL, which is set in every
        // session this will ever run in - and to /bin/sh when it is not.
        let cfg = ConsoleConfig::default();
        let expected = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        assert_eq!(shell_program(&cfg), expected.trim());
    }

    #[test]
    fn an_exit_report_says_what_happened() {
        assert_eq!(ExitReport::default().to_string(), "exited");
        assert_eq!(
            ExitReport {
                code: Some(130),
                signal: None
            }
            .to_string(),
            "exited with status 130"
        );
        assert_eq!(
            ExitReport {
                code: Some(1),
                signal: Some("Hangup".to_string())
            }
            .to_string(),
            "killed by Hangup"
        );
    }

    #[test]
    fn colours_convert_in_all_three_forms() {
        assert_eq!(CellColor::from(vt100::Color::Default), CellColor::Default);
        assert_eq!(CellColor::from(vt100::Color::Idx(4)), CellColor::Indexed(4));
        assert_eq!(
            CellColor::from(vt100::Color::Rgb(1, 2, 3)),
            CellColor::Rgb(1, 2, 3)
        );
    }
}
