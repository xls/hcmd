//! The console's place in the layout, occupied or not.

use crate::console::sync::CwdSync;
use crate::console::{Console, Origin};

/// The console's place in this application's layout, occupied or not.
///
/// It holds **at most one live shell**. Everything addressed to the shell is
/// queued on the pane rather than written to the shell, so a keystroke sent
/// while the shell is dead, starting or restarting is not lost: a PTY write can
/// block, [`crate::input::dispatch`] may not touch the outside world, and the
/// event loop drains the queue when there is something to drain it into. A
/// headless test can then assert on the exact bytes with no PTY anywhere.
///
/// The pane's directory agrees with the active panel's, or a `cd` is on its way
/// to make it agree.
///
/// [`Console`] itself is not changed by any of this. The fields here are the
/// difference between "a shell" and "a shell in this application", which is
/// what `Pane` names.
pub struct Pane {
    /// The shell, started at application start and living as
    /// long as the application does.
    ///
    /// `None` in two situations, and they behave identically: a headless
    /// application, which has no terminal to give a shell, and a session where
    /// the shell could not be started. Both fall back to the v0.1 command
    /// line.
    pub shell: Option<Console>,

    /// Bytes on their way to the shell, queued by
    /// [`crate::input::dispatch`] and flushed by the event loop.
    queued: Vec<u8>,
    /// Whether anything in `queued` was typed rather than generated.
    origin: Origin,

    /// the two-way directory sync, and all of the state it needs.
    ///
    /// Where the shell is or is heading, and how many `cd`s this application
    /// has written and not yet seen answered, which is what stops the two
    /// halves fighting. Every rule lives in [`crate::console::sync`].
    pub cwd_sync: CwdSync,

    /// `Ctrl+O` on a dead shell asked for a new one.
    ///
    /// Spawning is the event loop's, exactly as a directory read is.
    pub restart_requested: bool,

    /// the completion indicator: "a completion indicator in the key
    /// bar shows when a background command has produced output".
    ///
    /// Set while the panels are showing and a command is *running* in the
    /// shell, not merely while bytes arrive, which would light on the echo of
    /// every `cd` this application writes and on every character typed at the
    /// command line, and would then mean nothing. Cleared the moment the
    /// shell's own screen is on, because looking at it is what the indicator
    /// was asking for.
    pub activity: bool,

    /// Whether the command the shell is running is the `cd` this application
    /// wrote, rather than one the user started.
    ///
    /// Its whole job is to keep [`Pane::activity`] from lighting on ordinary
    /// navigation.
    pub cd_running: bool,

    /// the `auto`: when to look at whether the command is still
    /// holding the terminal.
    ///
    /// A deadline rather than a timer task, for the same reason a directory
    /// read is a request: `dispatch` records when the question should be
    /// asked and the event loop answers it, so the whole rule is drivable from
    /// a test with no clock and no terminal.
    pub switch_at: Option<std::time::Instant>,

    /// Did the screen come to the console on its own, rather than being asked
    /// for?
    ///
    /// The `auto` switch is "the command is still holding the terminal", and
    /// the honest end of that sentence is "and now it is not". A screen that
    /// arrived by itself goes back by itself once the shell is at a prompt
    /// again, so a `git clone` shows its progress and then hands the panels
    /// back. A console reached with `ctrl+o` was asked for and is never taken
    /// away.
    pub auto_shown: bool,

    /// Whether the shell was running a command at the last console event.
    ///
    /// Only an *edge* means anything here. A `cd` is echoed by the tty before
    /// the shell has read it, so the first batch after the write still reports
    /// "no command running"; clearing on that would throw the flag away before
    /// the command it describes had started. Falling from true to false is the
    /// command ending, and that is the only transition either flag reacts to.
    pub was_running: bool,
}

impl Pane {
    /// A pane holding this shell and nothing else: no queue, no `cd` in
    /// flight, no indicator.
    ///
    /// Everything about a previous shell goes with it. A queue addressed to a
    /// shell that is gone, a `cd` it never answered, and an indicator about a
    /// command it was running are all statements about something that no
    /// longer exists.
    pub fn holding(shell: Option<Console>) -> Self {
        Self {
            shell,
            ..Self::default()
        }
    }

    /// Queue bytes for the shell, remembering whether a person typed them.
    ///
    /// A batch holding even one typed byte is treated as typed throughout.
    /// The two cannot normally mix, since a `cd` is only written when the
    /// shell's input line is empty, and if they ever did, drawing the user's
    /// own keystroke without delay is the behaviour worth keeping.
    pub fn queue(&mut self, bytes: &[u8], origin: Origin) {
        if origin == Origin::Typed {
            self.origin = Origin::Typed;
        }
        self.queued.extend_from_slice(bytes);
    }

    /// What is waiting for the shell, without taking it.
    pub fn queued(&self) -> &[u8] {
        &self.queued
    }

    /// Take everything queued, and say whether any of it was typed.
    pub fn take_queued(&mut self) -> (Vec<u8>, Origin) {
        let origin = std::mem::replace(&mut self.origin, Origin::Internal);
        (std::mem::take(&mut self.queued), origin)
    }
}

impl Default for Pane {
    /// An empty pane: no shell, nothing queued, nothing running.
    fn default() -> Self {
        Self {
            shell: None,
            queued: Vec::new(),
            origin: Origin::Internal,
            cwd_sync: CwdSync::new(),
            restart_requested: false,
            activity: false,
            cd_running: false,
            switch_at: None,
            auto_shown: false,
            was_running: false,
        }
    }
}

impl std::fmt::Debug for Pane {
    /// Says whether a shell is there and how much is queued for it, and
    /// nothing about what was typed: [`Console`] is not [`std::fmt::Debug`],
    /// and bytes on their way to a shell are the user's business.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pane")
            .field("shell", &self.shell.is_some())
            .field("queued", &self.queued.len())
            .field("restart_requested", &self.restart_requested)
            .field("activity", &self.activity)
            .finish_non_exhaustive()
    }
}
