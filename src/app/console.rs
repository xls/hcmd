//! The console pane: this application's place for a shell.
//!
//! The pane holds **at most one live shell**. Everything addressed to it is
//! queued on the pane rather than written to the shell, so a keystroke sent
//! while the shell is dead, starting or restarting is not lost: the event loop
//! drains the queue when there is something to drain it into.
//!
//! The pane's directory agrees with the active panel's, or a `cd` is on its
//! way to make it agree. Which way the agreement travels depends on which end
//! moved, and a shell that is inside `ssh` is never sent a local `cd`, because
//! the path it would name is on the wrong machine.
//!
//! Coming back from the console re-reads **both** panels. A shell is the one
//! thing in this program that changes the filesystem without telling it, and
//! `cp` in there has two ends.

use crate::app::App;
use crate::console::sync::Cd;
use crate::console::{Console, ConsoleEvent, Origin};
use crate::input::Focus;
use crate::panel::Side;
use crate::vfs::VfsPath;

impl App {
    /// **The one question every console-aware code path asks**.
    ///
    /// True when a live shell owns the command line: the row at the foot of the
    /// panel view is the shell's own input line, its caret is the shell's, and
    /// the editing keys are forwarded rather than run. False in a headless
    /// `App`, before the shell has started, and after it has died - and then
    /// the v0.1 [`crate::input::CommandLine`] is the command line again,
    /// unchanged, which is
    /// what keeps the input model testable with no terminal attached.
    pub fn console_owns_cmdline(&self) -> bool {
        self.console.shell.as_ref().is_some_and(Console::is_alive)
    }

    /// Recompute the console's input block for this frame.
    ///
    /// The event loop calls this once, before drawing. See
    /// [`crate::console::Console::refresh_stable_block`] for why it is a
    /// per-frame `&mut` step rather than something the renderer works out.
    pub fn refresh_console_block(&mut self) {
        let max = self.config.console.cmdline_rows.max(1);
        if let Some(console) = self.console.shell.as_mut() {
            console.refresh_stable_block(max);
        }
    }

    /// When the console's repaint settle window expires
    /// ([`crate::console::SETTLE`]).
    ///
    /// The event loop uses it as a wake-up: the docked command line holds its
    /// previous rendering while the shell's output is still arriving, so
    /// something has to draw the settled prompt once the quiet comes, or it
    /// would not appear until the next keystroke.
    pub fn console_settle_deadline(&self) -> Option<std::time::Instant> {
        self.console
            .shell
            .as_ref()
            .and_then(Console::settle_deadline)
    }

    /// True while `Ctrl+O` has the panels hidden.
    pub const fn console_is_shown(&self) -> bool {
        matches!(self.focus, Focus::Console)
    }

    /// Install a freshly started shell, or record that there is none.
    ///
    ///
    /// **Assign through this, never to `App::console` directly.** A new shell
    /// knows nothing about where the old one was, and leaving the bookkeeping
    /// behind would have the first `cd` suppressed as the echo of a write to a
    /// PTY that no longer exists.
    pub fn set_console(&mut self, console: Option<Console>) {
        self.console = crate::console::Pane::holding(console);
    }

    /// Queue bytes for the shell.
    ///
    /// Dropped when no shell is running, so every caller can write
    /// unconditionally: there is nothing to send them to, and a queue that grew
    /// while nothing drained it would be delivered to whatever shell started
    /// next.
    pub fn to_shell(&mut self, bytes: &[u8]) {
        self.queue_for_shell(bytes, Origin::Typed);
    }

    /// Queue bytes this application generated rather than the user
    /// (the `cd`). See [`Origin`] for why the difference is drawn
    /// differently.
    pub fn to_shell_internal(&mut self, bytes: &[u8]) {
        self.queue_for_shell(bytes, Origin::Internal);
    }

    fn queue_for_shell(&mut self, bytes: &[u8], origin: Origin) {
        if !self.console_owns_cmdline() {
            return;
        }
        self.console.queue(bytes, origin);
    }

    /// Drain what the last keystroke queued for the shell, and who queued it.
    /// The event loop calls this once a frame, exactly as it drains the
    /// pending reads.
    pub fn take_pending_shell(&mut self) -> (Vec<u8>, Origin) {
        self.console.take_queued()
    }

    /// What the last keystroke queued, without draining it. For tests, which
    /// are the reason the queue exists.
    pub fn pending_shell(&self) -> &[u8] {
        self.console.queued()
    }

    /// Apply one message from the console reader thread.
    ///
    /// Output is fed to the emulator; an `OSC 7` in it moves the active panel;
    /// EOF marks the shell dead and says so, without taking the application
    /// with it.
    pub fn apply_console_event(&mut self, event: ConsoleEvent) {
        let cwd = match event {
            ConsoleEvent::Output(bytes) => {
                let shown = self.console_is_shown();
                let (report, running) = match self.console.shell.as_mut() {
                    Some(console) => (console.feed(&bytes), console.command_running()),
                    None => (crate::console::FeedReport::default(), false),
                };
                // "a completion indicator in the key bar shows
                // when a background command has produced output" - *background*
                // being the whole point, so only while the panels are what is
                // on screen.
                //
                // **Read off the batch, not off the state it left behind.**
                // A PTY read returns whatever has arrived, and `echo hi` - its
                // `OSC 133 ; C`, its output and the next prompt's `; A` -
                // routinely arrives as a single chunk: at a 60 fps draw budget
                // that is roughly one command in three. Sampling
                // `command_running()` after such a batch answers "nothing is
                // running", which is true and useless, and the indicator would
                // stay dark for exactly the case the design says it exists
                // for - "a command finished with output while the panels were
                // showing, which is the ordinary case under `auto`". So
                // `FeedReport` reports what happened *inside* the batch.
                //
                // And only for a command the **user** started. the design's
                // panel → shell half writes a `cd` on every panel move, and to
                // the shell's prompt marks that is a command like any other:
                // an indicator that lit on it would light on ordinary
                // navigation and stop meaning anything.
                //
                // `console_cd_running` and not `CwdSync::unanswered`, which is
                // tempting and is off by one batch: the `OSC 7` answers the
                // `cd` while the shell is still mid-command, and the readline
                // bytes that follow it (`ESC[?2004h`) then arrive with the
                // counter already back at zero.
                let was_running = self.console.was_running;
                self.console.was_running = running;
                let started = report.command_started || (running && !was_running);
                if (started || running) && !shown && !self.console.cd_running {
                    self.console.activity = true;
                }
                // The command ended. Whatever it was, the next one is the
                // user's until this application says otherwise - and the end
                // has to be taken from the batch too, or a `cd` whose whole
                // cycle arrived in one chunk would leave the flag set forever
                // and every later command would be silent.
                if report.command_ended || (was_running && !running) {
                    self.console.cd_running = false;
                }
                // the authority check, applied to the *panel →
                // shell* half as well: an `OSC 7` naming another machine means
                // the shell on the far end of the PTY is inside `ssh`, and a
                // local `cd` written into it moves a shell somewhere the panel
                // is not. `CwdSync` declines until a local one arrives.
                if report.foreign_host {
                    self.console.cwd_sync.shell_is_foreign();
                }
                report.cwd
            }
            ConsoleEvent::Eof | ConsoleEvent::Failed(_) => {
                let failure = match event {
                    ConsoleEvent::Failed(why) => Some(why),
                    ConsoleEvent::Output(_) | ConsoleEvent::Eof => None,
                };
                if let Some(console) = self.console.shell.as_mut() {
                    console.closed(failure);
                }
                self.report_dead_shell();
                None
            }
        };

        let Some(cwd) = cwd.filter(|_| self.config.console.sync_cwd) else {
            return;
        };
        self.follow_shell_cwd(cwd);
    }

    /// A command was just written to the shell; decide what the screen does.
    ///
    ///
    /// `always` switches now, `never` never does, and `auto` - the default -
    /// arms the deadline below instead of switching, so a `mkdir` that is back
    /// at a prompt before it expires costs no flash at all.
    pub fn command_was_run(&mut self) {
        match self.config.console.switch_on_run {
            crate::config::SwitchOnRun::Never => {}
            crate::config::SwitchOnRun::Always => self.set_focus(Focus::Console),
            crate::config::SwitchOnRun::Auto => {
                let delay = self.config.console.switch_delay.duration();
                // `checked_add`: a delay so large that the clock cannot express
                // the deadline is a delay that never expires, which is what
                // `never` is for and is the harmless way to read it.
                self.console.switch_at = std::time::Instant::now().checked_add(delay);
            }
        }
    }

    /// When the `auto` next needs to be asked, if it is waiting.
    ///
    /// The event loop sleeps until then rather than until its ordinary tick, so
    /// a shell that says nothing at all - the `sleep 30` case - still gets the
    /// screen at `switch_delay` rather than whenever the next byte arrives.
    pub const fn console_switch_deadline(&self) -> Option<std::time::Instant> {
        self.console.switch_at
    }

    /// Answer the `auto` once its deadline has passed.
    ///
    /// > After `switch_delay`, if the shell has **not** returned to a prompt
    /// > with an empty input line, the command is still holding the terminal …
    /// > and the screen switches to the console.
    ///
    /// The idle test is [`Console::input_is_empty`] - "the check is the same
    /// 'at a prompt with an empty input line' test uses before writing a `cd`,
    /// so there is one definition of 'the shell is idle' in the program rather
    /// than two that can disagree". Its `None` means "cannot tell", and a shell
    /// that cannot say has not said it is back, so the screen switches: under
    /// `auto` the cost of being wrong that way is one `Ctrl+O`, and the cost of
    /// the other way is a program waiting for input nobody can see.
    pub fn service_console_switch(&mut self, now: std::time::Instant) {
        let Some(deadline) = self.console.switch_at else {
            return;
        };
        if now < deadline {
            return;
        }
        self.console.switch_at = None;
        if !self.console_owns_cmdline() || self.console_is_shown() {
            return;
        }
        // Not while something else owns the screen. `F3` on a file after
        // pressing `Enter` - well within `console.switch_delay`, which the user
        // may have set to seconds - would otherwise have the file vanish
        // mid-sentence and the keystrokes go to the shell, leaving a viewer on
        // the stack that is never drawn and never popped. A dialog over the
        // viewer is the same state (`DialogFrame::restore == Focus::Viewer`),
        // and a dialog anywhere is a question waiting for an answer.
        //
        // The switch is **dropped**, not re-armed: the automatic
        // switch is "the command is still holding the terminal *now*", and
        // `Ctrl+O` is one keystroke for a user who wants to look.
        if self.viewer_is_shown() || self.dialog_is_open() {
            return;
        }
        let idle = self
            .console
            .shell
            .as_ref()
            .and_then(Console::input_is_empty);
        if idle != Some(true) {
            self.set_focus(Focus::Console);
        }
    }

    /// Ask the child whether it has ended, and report it if it has.
    ///
    ///
    /// The event loop calls this once a frame, exactly as it drains the pending
    /// reads. EOF on the PTY master is the *other* signal that a shell has gone
    /// and it is the weaker one - `sleep 300 &` then `exit` leaves the slave
    /// open, so no EOF ever arrives - see [`Console::poll_exit`].
    pub fn service_console_exit(&mut self) {
        if self.console.shell.as_mut().is_some_and(Console::poll_exit) {
            self.report_dead_shell();
        }
    }

    /// Say that the shell has gone, and make sure the user is not left looking
    /// at it.
    fn report_dead_shell(&mut self) {
        self.message = self.console.shell.as_ref().map(Console::death_notice);
        // The panels are still a file manager. Leaving focus
        // inside a console that no longer exists would be a state with no key
        // out of it.
        if self.console_is_shown() {
            self.set_focus(Focus::Panel(self.active_side));
        }
        // Nothing is going to run, so nothing is still holding the terminal.
        self.console.switch_at = None;
    }

    /// Shell → panel.
    ///
    /// The **active** panel and only when it is somewhere else: a panel
    /// already showing that directory would otherwise re-read on every prompt,
    /// and the `cd` this application writes in the other direction would
    /// bounce back and forth forever.
    fn follow_shell_cwd(&mut self, cwd: std::path::PathBuf) {
        // `None` where the panel is not a local listing: an archive or a remote
        // host has its own idea of where it is, and the
        // local shell does not get to move it.
        let panel_at = self
            .active_panel()
            .active_tab()
            .path
            .local_path()
            .map(std::path::Path::to_path_buf);
        let follow = self
            .console
            .cwd_sync
            .shell_reported(cwd, panel_at.as_deref());
        // A directory that cannot be listed is said out loud rather than
        // navigated into: `navigate` empties the tab before the read starts, so
        // following the shell into a hole shows nothing and then an error.
        if let Some(message) = follow.message() {
            self.message = Some(message);
        }
        if let Some(target) = follow.navigate_to() {
            let side = self.active_side;
            self.navigate(side, VfsPath::local(target));
        }
    }

    /// Panel → shell.
    ///
    /// > when the active panel changes directory, write `cd <path>` to the PTY,
    /// > quoted, only when the shell is at a prompt and its input line is empty.
    ///
    /// "Only when" is load-bearing and is not a guess:
    /// [`Console::input_is_empty`] answers `None` when the shell does not mark
    /// where its input begins, and `None` is not `false` - a `cd` written over
    /// a half-typed command corrupts it, so nothing is written unless the shell
    /// has said, in as many words, that there is nothing on the line.
    pub(super) fn sync_shell_cwd(&mut self, side: Side) {
        if side != self.active_side || !self.config.console.sync_cwd {
            return;
        }
        if !self.console_owns_cmdline() {
            return;
        }
        let Some(path) = self
            .panel(side)
            .active_tab()
            .path
            .local_path()
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        let empty = self
            .console
            .shell
            .as_ref()
            .and_then(Console::input_is_empty);
        if let Cd::Write(line) = self.console.cwd_sync.panel_moved(&path, empty) {
            // The shell is about to run a command that the user did not type,
            // so the completion indicator must not treat what it
            // prints as news. See `console_cd_running`.
            self.console.cd_running = true;
            self.to_shell_internal(&line);
        }
    }

    /// `Ctrl+O`.
    ///
    /// > `Ctrl+O` again brings the panels back, **with them refreshed**.
    ///
    /// Toggles between the panels and the full-screen console. A shell that has
    /// died is not a dead end: the key asks for a new one, and the event loop
    /// starts it.
    ///
    /// **Coming back re-reads both panels**, which is what "refreshed" is for:
    /// a shell is the one thing in this application that changes the filesystem
    /// without telling it, and the listing on screen was read before whatever
    /// was just done in there. `reread` keeps the cursor, the marks and the
    /// quick-search buffer and replaces the rows only when the
    /// new ones arrive, so this costs a directory read and disturbs nothing the
    /// user was in the middle of. Both panels, not just the active one: `cp` in
    /// the console has two ends.
    pub fn toggle_console(&mut self) {
        if self.console_is_shown() {
            self.set_focus(Focus::Panel(self.active_side));
            self.reread_both();
            return;
        }
        if self.console_owns_cmdline() {
            self.set_focus(Focus::Console);
            return;
        }
        self.console.restart_requested = true;
        self.message = Some(match self.console.shell.as_ref() {
            Some(console) => format!("{}: starting a new shell", console.program()),
            None => "starting a shell".to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{app_with, deliver};
    use crate::config::{Config, Keymap, Theme};
    use crate::vfs::Entry;

    /// A shell sitting at a marked, empty prompt - the one condition
    /// for writing a `cd`, and the definition of "the shell is idle".
    const AT_A_PROMPT: &[u8] = b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\";

    /// A headless app with a **live shell**, or `None` where there is no
    /// `/bin/sh`.
    ///
    /// The same shape `crate::ui::console`'s tests use, and for the same
    /// reason: the questions below - does a `cd` go down the wire, does the
    /// screen switch - are only meaningful with a console that is alive. No
    /// hooks are injected and nothing waits on the shell: every prompt mark in
    /// these tests is written into the emulator by the test itself, so the
    /// screen holds exactly what was fed to it and there is no race.
    fn app_with_shell() -> Option<(App, tokio::sync::mpsc::Receiver<ConsoleEvent>)> {
        if !std::path::Path::new("/bin/sh").exists() {
            return None;
        }
        let mut config = Config::default();
        config.console.shell = "/bin/sh".to_string();
        config.console.inject_hooks = false;
        let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
        let (tx, rx) = tokio::sync::mpsc::channel::<ConsoleEvent>(64);
        let console =
            Console::spawn(&app.config.console, std::path::Path::new("/"), (24, 80), tx).ok()?;
        app.set_console(Some(console));
        assert!(app.console_owns_cmdline(), "the shell is running");
        Some((app, rx))
    }

    fn feed(app: &mut App, bytes: &[u8]) {
        app.apply_console_event(ConsoleEvent::Output(bytes.to_vec()));
    }

    #[test]
    fn switch_on_run_always_switches_and_never_never_does() {
        // "**`always`** is the older behaviour, for anyone who
        // wants to watch everything … **`never`** never switches; `Ctrl+O` is
        // the only way in."
        let mut app = app_with(&["alpha"]);
        app.config.console.switch_on_run = crate::config::SwitchOnRun::Always;
        app.command_was_run();
        assert_eq!(app.focus, Focus::Console);
        assert_eq!(app.console_switch_deadline(), None, "no waiting involved");

        let mut app = app_with(&["alpha"]);
        app.config.console.switch_on_run = crate::config::SwitchOnRun::Never;
        app.command_was_run();
        assert_eq!(app.focus, Focus::Panel(Side::Left));
        assert_eq!(app.console_switch_deadline(), None);
        app.service_console_switch(std::time::Instant::now());
        assert_eq!(
            app.focus,
            Focus::Panel(Side::Left),
            "and never later either"
        );
    }

    #[test]
    fn switch_on_run_auto_leaves_the_panels_alone_for_a_command_that_finished() {
        // the default: "the panels stay. After `switch_delay`, if
        // the shell has **not** returned to a prompt with an empty input line
        // … the screen switches … If the shell *has* returned, nothing happens
        // at all: no switch, no flash."
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        assert_eq!(
            app.config.console.switch_on_run,
            crate::config::SwitchOnRun::Auto,
            "which is the shipped default"
        );
        feed(&mut app, AT_A_PROMPT);

        app.command_was_run();
        assert_eq!(app.focus, Focus::Panel(Side::Left), "no immediate switch");
        let deadline = app
            .console_switch_deadline()
            .expect("auto armed the deadline instead of switching");

        // Before it: nothing, whatever the shell is doing.
        app.service_console_switch(deadline - std::time::Duration::from_millis(1));
        assert_eq!(app.focus, Focus::Panel(Side::Left));

        // At it, with the shell back at an empty prompt: still nothing.
        app.service_console_switch(deadline);
        assert_eq!(
            app.focus,
            Focus::Panel(Side::Left),
            "the shell is idle, so there is nothing to switch to"
        );
        assert_eq!(
            app.console_switch_deadline(),
            None,
            "and it is answered once"
        );
    }

    #[test]
    fn switch_on_run_auto_shows_a_command_that_is_still_holding_the_terminal() {
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        feed(&mut app, AT_A_PROMPT);
        app.command_was_run();
        // `OSC 133 ; C`: the command was submitted and its output starts here,
        // which is what a `cat`, a build or a pager looks like at the deadline.
        feed(&mut app, b"cat\r\n\x1b]133;C\x1b\\");
        assert_eq!(
            app.console.shell.as_ref().and_then(Console::input_is_empty),
            None,
            "there is no prompt to read, which is not the same as an empty one"
        );

        let deadline = app.console_switch_deadline().expect("armed");
        app.service_console_switch(deadline);
        assert_eq!(
            app.focus,
            Focus::Console,
            "the command still needs the terminal, so the screen is given to it"
        );
    }

    #[test]
    fn a_tab_switch_takes_the_shell_with_it() {
        // "the shell's cwd and the active panel's directory must
        // track each other" - unconditionally, and `Tab` changes which
        // directory is active without ever reading one.
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        feed(&mut app, AT_A_PROMPT);
        app.left.active_tab_mut().path = VfsPath::local("/tmp");
        app.right.active_tab_mut().path = VfsPath::local("/var");
        let _ = app.take_pending_shell();

        app.set_focus(Focus::Panel(Side::Right));
        let queued = String::from_utf8_lossy(app.pending_shell()).into_owned();
        assert!(
            queued.contains("cd /var"),
            "the other panel's directory went to the shell: {queued:?}"
        );

        // And it is idempotent: the same side again is not a change.
        let _ = app.take_pending_shell();
        app.set_focus(Focus::Panel(Side::Right));
        assert!(app.pending_shell().is_empty());
    }

    #[test]
    fn a_filename_reaches_the_shell_as_text_rather_than_as_keystrokes() {
        // "an unquoted `My Report (final).pdf` is a bug, not a
        // nuisance" - and quoting is only half of it. A TAB inside a filename
        // written raw into a pty is readline's completion key, which silently
        // substitutes a different name; a newline is `accept-line`, which runs
        // whatever is on the line. Bracketed paste is what makes the bytes
        // text.
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        feed(&mut app, AT_A_PROMPT);
        // The shell asks for bracketed paste, as every readline shell does.
        feed(&mut app, b"\x1b[?2004h");
        app.left.active_tab_mut().entries = vec![Entry::file("no\ttab.txt")];
        app.left.active_tab_mut().cursor = 0;
        let _ = app.take_pending_shell();

        app.put_selected(false);
        let (queued, _) = app.take_pending_shell();
        let text = String::from_utf8_lossy(&queued).into_owned();
        assert!(
            text.starts_with("\x1b[200~") && text.ends_with("\x1b[201~"),
            "wrapped as a paste, so readline inserts it instead of obeying it: \
             {text:?}"
        );
        assert!(
            text.contains("'no\ttab.txt' "),
            "quoted, with the separating space: {text:?}"
        );
    }

    #[test]
    fn a_command_that_starts_and_finishes_in_one_batch_still_lights_the_indicator() {
        // "a completion indicator in the key bar shows when a
        // background command has produced output" - and the design leans on
        // it: "the key bar's completion indicator is what says a command
        // finished with output while the panels were showing, which is the
        // ordinary case under `auto`".
        //
        // A pty read returns whatever has arrived, and `echo hi` - its `; C`,
        // its output and the next prompt's `; A` - routinely arrives as one
        // chunk. Sampling `command_running()` after such a batch reports "no
        // command is running", and the indicator stayed dark for exactly the
        // case it exists for.
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        assert!(!app.console_is_shown(), "the panels are what is on screen");
        feed(
            &mut app,
            b"echo hi\r\n\x1b]133;C\x1b\\hi\r\n\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\",
        );
        assert!(
            app.console.activity,
            "one batch holding the whole C..A cycle still says a command ran"
        );

        // And looking at the shell answers it.
        app.set_focus(Focus::Console);
        assert!(!app.console.activity);
    }

    #[test]
    fn a_shell_inside_ssh_is_never_sent_a_local_cd() {
        // an `OSC 7` naming "a hostname that is not this machine"
        // is not this shell's directory - and it is not somewhere a local `cd`
        // may be written to either. The remote shell marks its own prompt, so
        // "the input line is empty" answers `Some(true)` about a prompt on
        // another machine.
        let Some((mut app, _rx)) = app_with_shell() else {
            return;
        };
        app.left.active_tab_mut().path = VfsPath::local("/tmp");
        app.right.active_tab_mut().path = VfsPath::local("/var");
        feed(
            &mut app,
            b"\x1b]7;file://build-farm/home/dev\x07\x1b]133;A\x1b\\dev@build-farm$ \x1b]133;B\x1b\\",
        );
        assert_eq!(
            app.console.shell.as_ref().and_then(Console::input_is_empty),
            Some(true),
            "the remote prompt reads as an empty input line, which is the trap"
        );
        let _ = app.take_pending_shell();

        app.navigate(Side::Left, VfsPath::local("/etc"));
        let _ = app.take_pending_reads();
        assert!(
            app.pending_shell().is_empty(),
            "nothing is typed into the ssh session: {:?}",
            String::from_utf8_lossy(app.pending_shell())
        );

        // Back on this machine, the sync resumes on the next panel move.
        feed(&mut app, b"\x1b]7;file:///home/thorin\x07");
        app.navigate(Side::Left, VfsPath::local("/srv"));
        let _ = app.take_pending_reads();
        let queued = String::from_utf8_lossy(app.pending_shell()).into_owned();
        assert!(queued.contains("cd /srv"), "{queued:?}");
    }

    #[test]
    fn coming_back_from_the_console_re_reads_both_panels() {
        // "Ctrl+O again brings the panels back, **with them
        // refreshed**." A shell is the one thing here that changes the
        // filesystem without saying so, and the rows on screen were read
        // before whatever was just done in there.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.navigate(Side::Left, VfsPath::local("/x"));
        app.navigate(Side::Right, VfsPath::local("/y"));
        deliver(&mut app, Side::Left, &["a", "b"]);
        let _ = app.take_pending_reads();

        // The focus is the whole question `toggle_console` asks, so it is set
        // directly: no PTY is needed to state the rule, and
        // `tests/acceptance_console.rs` drives it against a real one.
        app.set_focus(Focus::Console);
        app.toggle_console();

        assert_eq!(app.focus, Focus::Panel(Side::Left));
        let asked: Vec<Side> = app.take_pending_reads().iter().map(|r| r.side).collect();
        assert_eq!(
            asked,
            vec![Side::Left, Side::Right],
            "both panels, not just the active one: a `cp` in the console has \
             two ends"
        );
        assert_eq!(
            app.left.active_tab().entries.len(),
            2,
            "and the rows stay on screen until the replacement arrives"
        );

        // Going the *other* way asks for nothing: there is nothing new to see
        // on a screen that is about to be hidden.
        app.toggle_console();
        assert!(app.take_pending_reads().is_empty());
    }
}
