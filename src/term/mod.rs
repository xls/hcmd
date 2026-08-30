//! Terminal setup and teardown.
//!
//! Rules this module exists to keep:
//!
//! * Alternate screen and raw mode on start; **restored on exit, on panic, and
//!   on `SIGTERM`**. A panic hook that leaves the user's terminal in raw mode is
//!   a bug. [`Term::restore`] is the one idempotent function that
//!   does it, and every exit path goes through it: the normal quit, an error
//!   returned from `main`, [`Term::install_panic_hook`], the signal guard in
//!   [`spawn_signal_guard`], and `Drop for Term`.
//! * Request the Kitty keyboard protocol, and fall back cleanly when the
//!   terminal does not have it. Without it `Ctrl+Enter`,
//!   `Ctrl+Up`/`Ctrl+Down`, `Shift+F1`-`F10` and `Alt+F1`-`F12` are physically
//!   undeliverable, and `Ctrl+H` cannot be told apart from `Backspace`.
//!   What we detect here is what `App::enhanced_keyboard`
//!   carries to the input layer and the `F1` help screen.
//! * Enable bracketed paste, so a pasted path is not read as navigation keys.

pub mod events;
// the clipboard sequence. Here rather than in the viewer because it
// is a fact about the terminal, and because it must be the only place in the
// crate that writes one.
pub mod osc52;
pub mod sequences;
pub mod unicode;

use std::io::{self, IsTerminal, Stdout, Write, stdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    is_raw_mode_enabled, supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::{KeyboardProtocol, TerminalConfig};
use crate::error::{Error, Result};

pub use events::{EVENT_CHANNEL_DEPTH, hold_input, paste_text, release_input, spawn_event_thread};
pub use sequences::{SequenceDecoder, SequenceMap};
pub use unicode::{ascii_borders, locale_is_utf8};

/// The ratatui terminal this application drives.
pub type Backend = CrosstermBackend<Stdout>;

/// The minimum usable size. Below this the UI renders a single
/// message instead of a broken layout.
pub const MIN_WIDTH: u16 = 60;
/// See [`MIN_WIDTH`].
pub const MIN_HEIGHT: u16 = 15;

/// The keyboard-protocol override the acceptance harness uses, because it
/// drives the binary through a bare pty that cannot answer a capability query.
/// Same three values as `terminal.keyboard_protocol`, and it wins over the
/// config file - the harness cannot edit the config.
pub const PROTOCOL_ENV: &str = "HCMD_KEYBOARD_PROTOCOL";

/// How long to wait for the terminal's answer to the keyboard-protocol query.
///
/// The query round-trips against the terminal, and a terminal that never
/// answers must not hang the application. crossterm bounds its own wait at two
/// seconds, which is far too long to sit in front of; this is the bound the
/// user actually experiences, and a terminal that does support the protocol
/// answers in well under a millisecond, local or over ssh.
///
/// One residue is worth knowing about: on a terminal that never answers, the
/// worker thread stays inside crossterm's own two-second wait, holding
/// crossterm's internal reader lock, so for about another second and a half
/// keystrokes are *queued* rather than acted on. Nothing is lost, and the UI is
/// already drawn by then. Anyone who cares avoids the query altogether with
/// `terminal.keyboard_protocol` or [`PROTOCOL_ENV`], both of which start
/// instantly.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Overrides [`QUERY_TIMEOUT`], in milliseconds, for a link slow enough to need
/// it.
pub const QUERY_TIMEOUT_ENV: &str = "HCMD_KEYBOARD_TIMEOUT_MS";

/// The hidden switch the acceptance harness uses to prove the rule
/// that "a panic hook that leaves the user's terminal in raw mode is a bug".
///
/// There is no other way to observe the panic path from outside the process:
/// the application has no reachable input that panics - that is the point of
/// it - so the only honest test is one the binary is asked to fail for.
pub const PANIC_TEST_ENV: &str = "HCMD_PANIC_TEST";

/// [`PANIC_TEST_ENV`]'s value for "panic as soon as the terminal is set up".
///
/// Also what any other truthy value means, so the original `HCMD_PANIC_TEST=1`
/// keeps working.
pub const PANIC_STAGE_START: &str = "start";

/// [`PANIC_TEST_ENV`]'s value for "panic on the first frame the viewer holds
/// the screen".
///
/// The viewer is a second state the restore rule has to survive: it takes the
/// whole screen, it is drawn before the too-small check, and it holds an open
/// file and a background scan. A panic there must still leave raw mode and the
/// alternate screen, and must still leave the console's shell running on its
/// own pty.
pub const PANIC_STAGE_VIEWER: &str = "viewer";

/// Every stage [`panic_test_hook`] knows by name.
const PANIC_STAGES: &[&str] = &[PANIC_STAGE_START, PANIC_STAGE_VIEWER];

/// Panic deliberately when [`PANIC_TEST_ENV`] names this `stage`, and do
/// nothing at all otherwise.
///
/// Called at [`PANIC_STAGE_START`] immediately after [`Term::init`], and at
/// [`PANIC_STAGE_VIEWER`] on the frame the viewer is drawn - both states in
/// which the terminal is in raw mode and on the alternate screen, which is the
/// only state the rule ("a panic hook that leaves the user's terminal
/// in raw mode is a bug") has anything to say about.
///
/// It is inert in normal use: an unset, empty or `0` value returns without
/// touching anything, and the variable is deliberately absent from `--help`.
#[expect(
    clippy::panic,
    reason = "panicking is what this function is for: it exists so the \
              acceptance harness can prove the terminal is restored on a \
              panic, and it is inert unless HCMD_PANIC_TEST names this stage"
)]
pub fn panic_test_hook(stage: &str) {
    if panic_test_requested(stage) {
        panic!("{PANIC_TEST_ENV}: deliberate panic at {stage}, for the acceptance harness");
    }
}

/// Whether [`PANIC_TEST_ENV`] asks for a panic at `stage`.
///
/// A value that names a stage asks for **that** stage only; any other truthy
/// value keeps the switch's original meaning, which is "as early as possible".
///
/// Read **once**: [`PANIC_STAGE_VIEWER`] is asked on every frame the viewer is
/// on screen, and `std::env::var` allocates a `String` each time it is called.
/// A per-frame allocation for a switch that is unset in every real run is
/// exactly the sort of thing that has no business on the draw path.
fn panic_test_requested(stage: &str) -> bool {
    static SETTING: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let value = SETTING.get_or_init(|| {
        std::env::var(PANIC_TEST_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "0")
    });
    let Some(value) = value.as_deref() else {
        return false;
    };
    if PANIC_STAGES.contains(&value) {
        return value == stage;
    }
    stage == PANIC_STAGE_START
}

// One flag per thing that has to be undone, rather than one flag for "we
// started". `--keytest` puts the terminal in raw mode with bracketed paste but
// never enters the alternate screen, and sending the leave-alternate-screen
// escape to a terminal that is not on it moves the cursor around for no
// reason. Each of these is set only once the escape it stands for has actually
// gone out, and `restore` swaps each back, so it is idempotent by
// construction.
/// Whether we pushed keyboard enhancement flags that have to be popped.
static PUSHED_FLAGS: AtomicBool = AtomicBool::new(false);
/// Whether the terminal is in raw mode.
static RAW: AtomicBool = AtomicBool::new(false);
/// Whether we are on the alternate screen.
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);
/// Whether bracketed paste is on.
static PASTE: AtomicBool = AtomicBool::new(false);
/// Whether mouse capture is on and has to be turned off again.
static MOUSE: AtomicBool = AtomicBool::new(false);
/// Whether the panic hook is already chained, so installing twice is harmless.
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// How many [`ExpectedPanics`] guards this thread is inside.
    ///
    /// A count and not a flag, so that a contained call inside a contained
    /// call does not un-arm the outer one when the inner one ends.
    /// An atomic rather than a `Cell` because interior mutability by `Cell`
    /// is banned here, and because this is read from a panic hook where a
    /// borrow that could fail would be the wrong kind of risk.
    static EXPECTING_PANIC: AtomicUsize = const { AtomicUsize::new(0) };
}

/// A receipt saying that a panic on this thread is expected and handled.
///
/// Some of the libraries this program reads untrusted bytes with panic where
/// this crate would return an error: `squashfs_reader` answers
/// `unimplemented!()` for the inode types it does not model, and a parser fed
/// a crafted header can overflow arithmetic that a debug build checks. Those
/// panics are caught by the caller (`vfs::image::format::contained`) and
/// turned into ordinary refusals.
///
/// The panic hook does not know that, and its job - restore the terminal,
/// print the message - is exactly wrong for a panic somebody is catching: it
/// would drop the user out of the alternate screen and print a backtrace over
/// a UI that is still running. Holding this guard tells it to stand aside.
///
/// It covers **one thread**, the one that made it, so a real panic anywhere
/// else is still reported and still restores the terminal.
pub struct ExpectedPanics(());

impl ExpectedPanics {
    /// Arm the guard for as long as the value lives.
    pub fn new() -> Self {
        let _ = EXPECTING_PANIC.try_with(|depth| depth.fetch_add(1, Ordering::SeqCst));
        Self(())
    }
}

impl Default for ExpectedPanics {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ExpectedPanics {
    fn drop(&mut self) {
        let _ = EXPECTING_PANIC.try_with(|depth| {
            let _ = depth.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |depth| {
                Some(depth.saturating_sub(1))
            });
        });
    }
}

/// Whether the panicking thread said its panic was expected.
///
/// `try_with` because this is called from a panic hook, which can run while a
/// thread's local storage is already being torn down; an answer of "not
/// expected" is the safe one there.
pub fn panic_is_expected() -> bool {
    EXPECTING_PANIC
        .try_with(|depth| depth.load(Ordering::SeqCst) > 0)
        .unwrap_or(false)
}

/// What has to happen before the process ends on a signal, beyond the
/// terminal.
///
/// `std::process::exit` runs no destructors. Everything this program cleans up
/// in a `Drop` - `ArchiveSession`'s 0700 temp tree of extracted members, the
/// router's cancellation of the walks still reading the filesystem - is
/// therefore skipped on `SIGINT`, `SIGHUP` and `SIGTERM`, which are exactly
/// the ways a session ends when a terminal window is closed. Restoring the
/// terminal before exiting was already right; this is the same idea for the
/// things the process owns outside its own memory, and it is a registry
/// because `term` must not know what an archive is.
///
/// Jobs run once, in the order they were registered, on whichever thread
/// reaches [`Term::run_exit_cleanup`] first.
static EXIT_CLEANUP: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(Vec::new());

/// An initialised terminal.
///
/// Dropping it restores the terminal, so every early return is covered without
/// a `defer`.
pub struct Term {
    terminal: Terminal<Backend>,
    enhanced: bool,
    decoder: SequenceDecoder,
    warnings: Vec<String>,
    /// Whether `ui.mouse` asked for mouse reporting.
    ///
    /// Held here rather than read back from the terminal - nothing can be read
    /// back from a terminal - so [`Term::resume`] can restore what
    /// [`Term::restore`] turned off.
    mouse: bool,
}

impl Term {
    /// Enter raw mode and the alternate screen, and negotiate the keyboard
    /// protocol.
    pub fn init(cfg: &TerminalConfig) -> Result<Self> {
        // Raw mode goes on first, deliberately. The capability query below is
        // run on a worker thread with a bounded wait, and crossterm's
        // non-raw path toggles raw mode itself - a thread doing that after we
        // had given up on it would take raw mode away behind our back. With
        // raw mode already on it takes the path that leaves the mode alone.
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);

        let enhanced = detect_enhanced_keyboard(cfg);

        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        ALT_SCREEN.store(true, Ordering::SeqCst);
        // Bracketed paste, so a pasted path reaches the command line as text
        // rather than as a burst of navigation keys.
        execute!(out, EnableBracketedPaste)?;
        PASTE.store(true, Ordering::SeqCst);

        if enhanced {
            // DISAMBIGUATE_ESCAPE_CODES is what makes Ctrl+Enter, Ctrl+Up and
            // Shift+F<n> distinguishable at all.
            //
            // REPORT_EVENT_TYPES and REPORT_ALL_KEYS_AS_ESCAPE_CODES are what
            // make a *bare* `Shift` press and release arrive as events, which
            // is the only way to know whether Shift is being held - the design
            // swaps the key bar for the `Shift+F<n>` labels "where the terminal
            // reports modifier state", and this is that. They also produce key
            // *release* events; `input::dispatch` ignores those by contract.
            let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
            if execute!(out, PushKeyboardEnhancementFlags(flags)).is_ok() {
                PUSHED_FLAGS.store(true, Ordering::SeqCst);
            }
        }

        let (map, warnings) = SequenceMap::parse(&cfg.sequences);
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        Ok(Self {
            terminal,
            enhanced: enhanced && PUSHED_FLAGS.load(Ordering::SeqCst),
            decoder: SequenceDecoder::new(map),
            warnings,
            mouse: false,
        })
    }

    /// The ratatui terminal.
    pub fn terminal(&mut self) -> &mut Terminal<Backend> {
        &mut self.terminal
    }

    /// Whether the enhanced keyboard protocol is actually in effect.
    ///
    /// This is what `App::enhanced_keyboard` is set from: the design makes
    /// `Ctrl+H` versus `Backspace` depend on it, and the design marks the
    /// bindings the terminal cannot deliver in the help screen.
    pub const fn enhanced_keyboard(&self) -> bool {
        self.enhanced
    }

    /// Warnings from parsing `[terminal.sequences]`, for `App::warnings`.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Turn mouse reporting on or off (`ui.mouse`, optional and
    /// additive). [`Term::restore`] turns it back off.
    ///
    /// The request is remembered, so [`Term::resume`] can put it back: the `F4`
    /// hand-over is a sequence of steps and **every step taken has to be put
    /// back**. `restore` disables mouse reporting on the way out to the editor,
    /// and without this the session would come back with `ui.mouse` silently off
    /// until the process was restarted.
    pub fn set_mouse(&mut self, enabled: bool) -> Result<()> {
        self.mouse = enabled;
        let mut out = stdout();
        if enabled {
            execute!(out, EnableMouseCapture)?;
            MOUSE.store(true, Ordering::SeqCst);
        } else if MOUSE.swap(false, Ordering::SeqCst) {
            execute!(out, DisableMouseCapture)?;
        }
        Ok(())
    }

    /// Apply `[terminal.sequences]` to one decoded key event (the design
    /// item 4), returning the events the application should act on.
    ///
    /// With no sequences configured - the normal case - this is the identity
    /// and nothing is ever buffered.
    pub fn decode(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        self.decoder.feed(key)
    }

    /// Release a partly-matched sequence. Call it when the select loop times
    /// out, so a lone `Esc` is never held waiting for a character that is not
    /// coming.
    pub fn flush_sequence(&mut self) -> Vec<KeyEvent> {
        self.decoder.flush()
    }

    /// Whether a partial escape sequence is being held.
    pub fn sequence_pending(&self) -> bool {
        self.decoder.is_pending()
    }

    /// Undo everything [`Term::init`] did. Safe to call twice, and safe to call
    /// when init never ran - which is what lets the panic hook call it blindly.
    ///
    /// The order matters: pop the keyboard flags while the terminal is still in
    /// the mode they were pushed in, stop bracketed paste and mouse reporting
    /// before leaving the screen they were enabled on, then leave the alternate
    /// screen, leave raw mode, and only then show the cursor - so it is the
    /// user's own shell prompt the cursor comes back on.
    pub fn restore() -> io::Result<()> {
        let mut out = stdout();
        let mut did_something = false;

        if PUSHED_FLAGS.swap(false, Ordering::SeqCst) {
            let _ = queue!(out, PopKeyboardEnhancementFlags);
            did_something = true;
        }
        if PASTE.swap(false, Ordering::SeqCst) {
            let _ = queue!(out, DisableBracketedPaste);
            did_something = true;
        }
        if MOUSE.swap(false, Ordering::SeqCst) {
            let _ = queue!(out, DisableMouseCapture);
            did_something = true;
        }
        if ALT_SCREEN.swap(false, Ordering::SeqCst) {
            let _ = queue!(out, LeaveAlternateScreen);
            did_something = true;
        }
        let _ = out.flush();

        let raw = if RAW.swap(false, Ordering::SeqCst) {
            did_something = true;
            disable_raw_mode()
        } else {
            Ok(())
        };

        // Last, and only when there was something to undo: ratatui hides the
        // cursor for any frame that does not place one, and a second call to
        // `restore` must not write anything at all.
        if did_something {
            let _ = execute!(out, Show);
        }
        raw
    }

    /// Hand the terminal back to a program that is about to take it over
    /// (the `F4`).
    ///
    /// > Sequence: leave alternate screen → restore cooked mode → pop keyboard
    /// > enhancement flags → spawn with inherited stdio → wait → re-enter
    /// > alternate screen → raw mode → push flags → force full redraw → reread
    /// > the panel.
    ///
    /// This is the first three steps, and [`Term::resume`] is the three that
    /// put them back. `restore` already does all three in the order that
    /// works - the flags are popped while the terminal is still in the mode they
    /// were pushed in - so this is `restore` under the name the caller means,
    /// and the flag statics make it exact: only what was actually on is undone.
    ///
    /// **Both halves belong to the event loop**, which is the only place
    /// allowed to touch the terminal; `crate::input::dispatch` queues the
    /// request as [`crate::app::ExternalCommand`] instead.
    ///
    pub fn suspend(&mut self) -> Result<()> {
        // Before the terminal is handed over, not after: from here until
        // `resume` the external program is the only reader of stdin. Without
        // this, our reader thread and `nvim` race for every keystroke.
        //
        events::hold_input();
        Self::restore()?;
        Ok(())
    }

    /// Take the terminal back after an external program has exited.
    ///
    ///
    /// Re-enters the alternate screen, raw mode, bracketed paste and the
    /// keyboard flags, then **clears ratatui's idea of what is on screen** so
    /// the next draw repaints every cell - the editor wrote over all of them
    /// and a diffing renderer would otherwise leave its last frame behind.
    /// That is the "force full redraw", and it is the step most
    /// easily forgotten because it is invisible until it is wrong.
    ///
    /// The keyboard protocol is *not* re-negotiated: whether this terminal has
    /// it was settled at startup and cannot have changed, and re-querying would
    /// spend the timeout again on a terminal that never answers.
    pub fn resume(&mut self) -> Result<()> {
        let restored = self.restore_terminal_state();
        // Whatever happened above, the reader thread starts again - an
        // application that cannot receive a keystroke cannot even be quit - and
        // only *after* raw mode is back, so no key is read in cooked mode.
        // Idempotent, so this is also correct on the path where `suspend` was
        // never reached.
        events::release_input();
        restored
    }

    /// Everything [`Term::resume`] does to the terminal itself.
    fn restore_terminal_state(&mut self) -> Result<()> {
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);

        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        ALT_SCREEN.store(true, Ordering::SeqCst);
        execute!(out, EnableBracketedPaste)?;
        PASTE.store(true, Ordering::SeqCst);
        // `restore` disabled it on the way out, and the sequence
        // puts back every step it took. Only when it was asked for: a session
        // with `ui.mouse` off must not gain mouse reporting by pressing `F4`.
        if self.mouse {
            execute!(out, EnableMouseCapture)?;
            MOUSE.store(true, Ordering::SeqCst);
        }

        if self.enhanced {
            let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
            if execute!(out, PushKeyboardEnhancementFlags(flags)).is_ok() {
                PUSHED_FLAGS.store(true, Ordering::SeqCst);
            }
        }

        // the "force full redraw": the editor wrote over every
        // cell, and ratatui's diffing renderer would otherwise leave its last
        // frame behind.
        //
        // `Terminal::resize` rather than `Terminal::clear`, which looks like
        // the obvious call and is a trap. `clear` snapshots the cursor first -
        // `get_cursor_position`, which writes `ESC[6n` and *waits for the
        // terminal to answer on stdin*. A terminal that does not answer makes
        // crossterm give up after two seconds with an error, and with a `?` on
        // it that error ends the session: pressing `F4` would quit the
        // application. `resize` re-establishes the viewport, clears it and
        // resets the back buffer with no round trip at all, which is every part
        // of that call this needs. The size comes from an ioctl, not from the
        // terminal's cooperation.
        let size = self.terminal.size()?;
        self.terminal
            .resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
        Ok(())
    }

    /// Install a panic hook that restores the terminal before the default hook
    /// prints anything.
    ///
    /// The previous hook is chained rather than replaced, and it runs *after*
    /// the restore, so the panic message is printed on a terminal that is out
    /// of raw mode and back on the main screen - otherwise it is either
    /// invisible or staircased.
    ///
    /// Call this **before** [`Term::init`], so a failure inside init is covered
    /// too. Calling it twice is a no-op rather than a second link in the chain.
    pub fn install_panic_hook() {
        if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A panic the caller is already containing is not the end of the
            // session and must not be treated as one: leaving the alternate
            // screen and printing a backtrace over a running UI would turn a
            // handled refusal into a wrecked terminal. See [`ExpectedPanics`].
            if panic_is_expected() {
                return;
            }
            let _ = Self::restore();
            previous(info);
        }));
    }

    /// Register work to run before the process ends on a signal.
    ///
    /// For anything a `Drop` would have done and `std::process::exit` will
    /// not: removing a temp tree, cancelling a walk. Keep the job short and
    /// make it safe to run from a signal task - it is the last thing that
    /// happens before the process is gone, and nothing waits for it.
    pub fn on_exit(job: impl FnOnce() + Send + 'static) {
        Self::cleanup_jobs().push(Box::new(job));
    }

    /// Run everything [`Term::on_exit`] registered, once.
    ///
    /// The list is taken rather than iterated, so a second call - a signal
    /// arriving while the first one is still running, a caller that is not
    /// sure whether it has already been through here - does nothing rather
    /// than deleting the same tree twice.
    pub fn run_exit_cleanup() {
        let jobs = std::mem::take(&mut *Self::cleanup_jobs());
        for job in jobs {
            job();
        }
    }

    /// The registry, with a poisoned lock recovered rather than escalated: a
    /// panic in one job must not leave the rest of the cleanup unreachable.
    fn cleanup_jobs() -> std::sync::MutexGuard<'static, Vec<Box<dyn FnOnce() + Send>>> {
        EXIT_CLEANUP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How long the guard gives an event loop to end a session gracefully on
    /// `SIGHUP` before ending it itself.
    ///
    /// the design persists the tabs on exit, and `std::process::exit` runs no
    /// destructors and reaches no `panel::state::save`. Closing the terminal
    /// window - which is how a session with the console open actually ends -
    /// therefore lost the session's tabs while a `kill -TERM` kept them. The
    /// event loop is selected on the same signal and needs a few milliseconds
    /// to break, restore and write the file; this is the backstop for the case
    /// where it is busy elsewhere and never gets there.
    const HANGUP_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

    /// Restore the terminal on `SIGINT` and `SIGHUP`, and - when `terminate` is
    /// set - on `SIGTERM` as well.
    ///
    /// These are the cases where there is no graceful exit to have: `SIGHUP`
    /// means the terminal is already gone, and `SIGINT` arrives from outside.
    /// Note that `Ctrl+C` is *not* one of them: raw mode clears `ISIG`, so the
    /// byte reaches the application as an ordinary key event, and nothing binds
    /// it - it does nothing, and `F10` / `Alt+Q` quit. What this
    /// guard is for is a `kill -INT`, a `Ctrl+C` sent before raw mode is on, and
    /// a `SIGHUP`.
    ///
    /// The event loop handles `SIGTERM` itself, breaking out of the loop and
    /// restoring on the way through `main`, because a graceful exit is
    /// preferable where one is available - so it passes `terminate = false`.
    /// `--keytest` has no such loop, so it passes `true` rather than dying with
    /// raw mode on and the keyboard flags still pushed.
    ///
    /// Must be called from inside the tokio runtime, and *before* raw mode goes
    /// on: a signal delivered between `enable_raw_mode` and this call would hit
    /// the default disposition and kill the process with the terminal
    /// unrestored. tokio registers the handler when the stream is created, so
    /// calling this first leaves no window at all.
    pub fn spawn_signal_guard(terminate: bool) -> Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut term = match terminate {
            true => Some(signal(SignalKind::terminate())?),
            false => None,
        };
        tokio::spawn(async move {
            let code = tokio::select! {
                _ = interrupt.recv() => 130,
                _ = hangup.recv() => 129,
                _ = async {
                    match term.as_mut() {
                        Some(term) => {
                            term.recv().await;
                        }
                        // Never completes, so the arm is simply not in play.
                        None => std::future::pending().await,
                    }
                } => 143,
            };
            // `SIGHUP` with an event loop running has a graceful path: `main`
            // selects on the same signal, breaks, restores and saves the tab
            // state. Give it that, then end the process anyway -
            // the terminal is gone and nothing may hang on to it. `SIGINT` has
            // no such path and is not delayed.
            if code == 129 && !terminate {
                tokio::time::sleep(Self::HANGUP_GRACE).await;
            }
            let _ = Self::restore();
            // The terminal is back; now everything the exit will not run a
            // destructor for. The temp tree an archive session extracted into
            // is the one that matters: a `Ctrl+C` used to leave it behind for
            // the next startup's sweep to find, which is a directory of the
            // user's files sitting in `$TMPDIR` until then.
            Self::run_exit_cleanup();
            std::process::exit(code);
        });
        Ok(())
    }

    /// `hcmd --keytest`: print the decoded key event for whatever is pressed,
    /// so a user on an unusual terminal can see what it sends (the design
    /// item 3).
    ///
    /// Restores the terminal on every exit path, like everything else here -
    /// including a signal. `--keytest` spends its whole life in raw mode with
    /// bracketed paste and the keyboard flags pushed, and it has no event loop
    /// to break out of, so it runs inside a small tokio runtime purely to have
    /// [`Term::spawn_signal_guard`] armed for `SIGTERM`/`SIGINT`/`SIGHUP`.
    /// A `pkill hcmd` from another terminal must not leave this
    /// one needing `stty sane`.
    pub fn keytest() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            // Armed before raw mode goes on, so there is no window in which the
            // default disposition applies.
            Self::spawn_signal_guard(true)?;
            let result = tokio::task::spawn_blocking(Self::keytest_raw).await;
            match result {
                Ok(result) => result,
                // The blocking task panicked; the panic hook has already
                // restored the terminal.
                Err(err) => Err(crate::Error::msg(format!("--keytest: {err}"))),
            }
        })
    }

    /// The body of [`Term::keytest`], on a blocking thread with the signal
    /// guard already armed.
    fn keytest_raw() -> Result<()> {
        let cfg = TerminalConfig::default();
        let enhanced = detect_enhanced_keyboard(&cfg);

        println!("{} --keytest", crate::BIN_NAME);
        println!(
            "enhanced keyboard protocol: {}",
            if enhanced {
                "active (Ctrl+Enter, Ctrl+Up/Down, Shift+F1..F10 and Ctrl+H all work)"
            } else {
                "NOT active - legacy terminal"
            }
        );
        println!("TERM={}", std::env::var("TERM").unwrap_or_default());
        println!("override with {PROTOCOL_ENV}=auto|enhanced|legacy or terminal.keyboard_protocol");
        println!(
            "\npress keys to see how this terminal encodes them; Ctrl+Q or Esc Esc to leave\n"
        );

        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);

        // No alternate screen here on purpose: the point of --keytest is to
        // leave a transcript in the scrollback that a user can paste into a
        // bug report.
        let mut out = stdout();
        // Bracketed paste, so a paste shows up as one event here exactly as it
        // does in the application.
        if execute!(out, EnableBracketedPaste).is_ok() {
            PASTE.store(true, Ordering::SeqCst);
        }
        if enhanced {
            // The same set the application pushes, so `--keytest` shows what
            // the application will actually see.
            let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
            if execute!(out, PushKeyboardEnhancementFlags(flags)).is_ok() {
                PUSHED_FLAGS.store(true, Ordering::SeqCst);
            }
        }

        let result = keytest_loop();
        let _ = Self::restore();
        result
    }
}

/// One key, printed the way `--keytest` prints it.
///
/// Split out from the loop so the formatting can be exercised without a
/// terminal.
fn keytest_line(key: KeyEvent) -> String {
    use crate::input::KeyPress;

    let press = KeyPress::from(key);
    format!(
        "  code={:?}  modifiers={:?}  kind={:?}  state={:?}\r\n  \
         bytes={}\r\n  keymap.toml binding: \"{press}\"\r\n\r\n",
        key.code,
        key.modifiers,
        key.kind,
        key.state,
        raw_bytes(key.code),
    )
}

/// The bytes behind a key, where they can be recovered.
///
/// crossterm decodes the terminal's input and does not hand back what it read,
/// so this is honest rather than complete: a character key's own encoding is
/// exact, and everything else says so instead of guessing.
fn raw_bytes(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            let hex: Vec<String> = encoded.bytes().map(|b| format!("{b:02x}")).collect();
            hex.join(" ")
        }
        _ => "(consumed by the decoder)".to_string(),
    }
}

fn keytest_loop() -> Result<()> {
    let mut last_was_escape = false;

    loop {
        let event = crossterm::event::read()?;
        match event {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    // Shown, because on the enhanced protocol their presence is
                    // itself the answer to "is this terminal reporting
                    // releases".
                    print!("{}", keytest_line(key));
                    let _ = stdout().flush();
                    continue;
                }

                print!("{}", keytest_line(key));
                let _ = stdout().flush();

                if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }
                if key.code == KeyCode::Esc {
                    if last_was_escape {
                        return Ok(());
                    }
                    last_was_escape = true;
                } else {
                    last_was_escape = false;
                }
            }
            Event::Paste(text) => {
                print!("  paste {:?}\r\n\r\n", paste_text(&text));
                let _ = stdout().flush();
                last_was_escape = false;
            }
            Event::Resize(w, h) => {
                print!("  resize {w}x{h}\r\n\r\n");
                let _ = stdout().flush();
            }
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {}
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = Self::restore();
    }
}

/// The `HCMD_KEYBOARD_PROTOCOL` override, when it is set to something we
/// understand.
pub fn protocol_from_env() -> Option<KeyboardProtocol> {
    let raw = std::env::var(PROTOCOL_ENV).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(KeyboardProtocol::Auto),
        "enhanced" => Some(KeyboardProtocol::Enhanced),
        "legacy" => Some(KeyboardProtocol::Legacy),
        _ => None,
    }
}

/// Decide whether to use the Kitty keyboard protocol.
///
/// `auto` asks the terminal; `enhanced` and `legacy` force it. Forcing
/// `enhanced` on a terminal that does not have it is harmless - the escape is
/// ignored - and is the escape hatch for a terminal whose reply we cannot read.
/// The `HCMD_KEYBOARD_PROTOCOL` environment variable overrides the config file,
/// because the acceptance harness has a pty but no config.
pub fn detect_enhanced_keyboard(cfg: &TerminalConfig) -> bool {
    let protocol = protocol_from_env().unwrap_or(cfg.keyboard_protocol);
    match protocol {
        KeyboardProtocol::Enhanced => true,
        KeyboardProtocol::Legacy => false,
        KeyboardProtocol::Auto => {
            if !stdout().is_terminal() {
                return false;
            }
            query_enhanced_keyboard(query_timeout())
        }
    }
}

/// The bound on the capability query, from the environment or [`QUERY_TIMEOUT`].
fn query_timeout() -> Duration {
    let configured = std::env::var(QUERY_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    match configured {
        Some(ms) => Duration::from_millis(ms.min(10_000)),
        None => QUERY_TIMEOUT,
    }
}

/// Ask the terminal, with a bound on how long we are willing to wait.
///
/// crossterm's `supports_keyboard_enhancement` blocks on a reply, so it runs on
/// a worker thread. Raw mode is switched on first and left on for the duration:
/// that puts crossterm on the code path that does not touch the mode itself, so
/// a thread that outlives our patience cannot change it underneath us. Such a
/// thread is harmless otherwise - it reads through crossterm's filtered
/// internal queue, which puts back every event that is not the reply, so no
/// keystroke is lost.
fn query_enhanced_keyboard(timeout: Duration) -> bool {
    let was_raw = is_raw_mode_enabled().unwrap_or(false);
    if !was_raw && enable_raw_mode().is_err() {
        return false;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hcmd-kbd-query".to_string())
        .spawn(move || {
            let _ = tx.send(supports_keyboard_enhancement().unwrap_or(false));
        });

    let answer = match spawned {
        Ok(_handle) => rx.recv_timeout(timeout).unwrap_or(false),
        Err(_) => false,
    };

    if !was_raw {
        let _ = disable_raw_mode();
    }
    answer
}

/// True when the terminal is too small to lay out.
pub const fn too_small(width: u16, height: u16) -> bool {
    width < MIN_WIDTH || height < MIN_HEIGHT
}

/// Turn a terminal-size query failure into our error type, so `main` has one
/// error path.
pub fn size() -> Result<(u16, u16)> {
    crossterm::terminal::size().map_err(Error::Bare)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyboardProtocol;

    #[test]
    fn the_protocol_can_be_forced_both_ways_without_a_terminal() {
        // The environment variable would override the config, and the test
        // process may be run under a harness that sets it.
        if std::env::var(PROTOCOL_ENV).is_ok() {
            return;
        }
        let cfg = TerminalConfig {
            keyboard_protocol: KeyboardProtocol::Enhanced,
            ..Default::default()
        };
        assert!(detect_enhanced_keyboard(&cfg));
        let cfg = TerminalConfig {
            keyboard_protocol: KeyboardProtocol::Legacy,
            ..Default::default()
        };
        assert!(!detect_enhanced_keyboard(&cfg));
    }

    #[test]
    fn the_minimum_size_is_the_one_the_spec_names() {
        assert!(too_small(59, 40));
        assert!(too_small(200, 14));
        assert!(!too_small(60, 15));
        // A 1x1 terminal is a size, not a crash.
        assert!(too_small(1, 1));
    }

    /// The teardown flags are process-wide, so the tests that read or write
    /// them take turns. Everything else here is pure and runs in parallel.
    static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn restore_is_safe_to_call_when_nothing_was_entered() {
        let _guard = STATE.lock();
        assert!(Term::restore().is_ok());
    }

    #[test]
    fn restore_is_idempotent() {
        let _guard = STATE.lock();
        // Whatever state the flags were left in, two calls in a row must both
        // succeed and the second must be a no-op.
        assert!(Term::restore().is_ok());
        assert!(Term::restore().is_ok());
        assert!(!RAW.load(Ordering::SeqCst));
        assert!(!ALT_SCREEN.load(Ordering::SeqCst));
        assert!(!PASTE.load(Ordering::SeqCst));
        assert!(!PUSHED_FLAGS.load(Ordering::SeqCst));
        assert!(!MOUSE.load(Ordering::SeqCst));
    }

    #[test]
    fn exit_cleanup_runs_every_job_once_and_then_has_nothing_left() {
        // What the signal path calls between `restore` and `process::exit`,
        // because `process::exit` runs no destructor: the archive session's
        // temp tree would otherwise outlive the process that made it.
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let counter = Arc::clone(&runs);
            Term::on_exit(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
        Term::run_exit_cleanup();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "both jobs ran");
        Term::run_exit_cleanup();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "and neither ran twice");
    }

    #[test]
    fn installing_the_panic_hook_twice_does_not_chain_it_twice() {
        Term::install_panic_hook();
        assert!(HOOK_INSTALLED.load(Ordering::SeqCst));
        Term::install_panic_hook();
        assert!(HOOK_INSTALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn a_panic_restores_the_terminal() {
        // "A panic hook that leaves the user's terminal in raw mode
        // is a bug." Raw mode itself cannot be faked in a test process without
        // a tty, so this stands in the bracketed-paste flag - the same
        // teardown, through the same function, from inside the hook.
        let _guard = STATE.lock();
        Term::install_panic_hook();
        PASTE.store(true, Ordering::SeqCst);

        let panicked = std::panic::catch_unwind(|| panic!("deliberate"));

        assert!(panicked.is_err());
        assert!(
            !PASTE.load(Ordering::SeqCst),
            "the panic hook must have restored the terminal"
        );
    }

    /// A panic somebody is catching must leave the terminal alone.
    ///
    /// The other half of the hook: a library that answers `unimplemented!()`
    /// for an inode type is contained by `vfs::image::format::contained`, and
    /// if the hook still tore the session down for it the containment would be
    /// worse than the panic.
    #[test]
    fn a_contained_panic_leaves_the_terminal_alone() {
        let _guard = STATE.lock();
        Term::install_panic_hook();
        PASTE.store(true, Ordering::SeqCst);

        let panicked = {
            let _quiet = ExpectedPanics::new();
            std::panic::catch_unwind(|| panic!("expected"))
        };

        assert!(panicked.is_err(), "the panic still happened");
        assert!(
            PASTE.load(Ordering::SeqCst),
            "a contained panic must not restore the terminal"
        );
        assert!(
            !panic_is_expected(),
            "and the guard is disarmed when it is dropped"
        );
        PASTE.store(false, Ordering::SeqCst);
    }

    #[test]
    fn the_query_timeout_is_bounded_and_overridable() {
        // The default is what an unset or unparseable variable gives.
        assert_eq!(QUERY_TIMEOUT, Duration::from_millis(500));
        assert!(query_timeout() <= Duration::from_secs(10));
    }

    #[test]
    fn keytest_prints_the_pieces_a_user_needs() {
        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        let line = keytest_line(key);
        assert!(line.contains("code=Char('h')"), "{line}");
        assert!(line.contains("modifiers="), "{line}");
        assert!(line.contains("kind="), "{line}");
        assert!(line.contains("bytes=68"), "{line}");
        assert!(line.contains("ctrl+h"), "{line}");
    }

    #[test]
    fn raw_bytes_are_exact_for_characters_and_honest_otherwise() {
        assert_eq!(raw_bytes(KeyCode::Char('a')), "61");
        assert_eq!(raw_bytes(KeyCode::Char('é')), "c3 a9");
        assert!(raw_bytes(KeyCode::F(5)).contains("decoder"));
    }
}
