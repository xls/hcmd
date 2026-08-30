//! the contract, driven through the real routing.
//!
//! > The command line at the foot of the panel view is **the shell's own
//! > current input line**, rendered from the PTY … The keys this application
//! > binds - `Up`/`Down` to leave for the panel, `Ctrl+Enter`, `Ctrl+O` -
//! > are intercepted before forwarding and never reach the shell.
//!
//! Every test here starts a **real shell on a real PTY**, because the question
//! being asked - does this key reach the shell, or does it act here - is only
//! meaningful with a live one. What each test then asserts on is the byte queue
//! [`App::pending_shell`], which is where `dispatch` leaves what it decided to
//! forward: the decision is observable without waiting on a shell to echo
//! anything, so nothing here races.
//!
//! `/bin/sh` rather than `$SHELL`: it exists everywhere, it starts in
//! milliseconds, and none of these tests care what a prompt looks like.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "an integration test is its own crate, so it is not #[cfg(test)] \
              and clippy.toml's allow-*-in-tests keys do not reach it. \
              Panicking assertions are the point of a test."
)]

use std::path::Path;
use std::time::{Duration, Instant};

use holoscommander::app::App;
use holoscommander::config::{Config, Keymap, Theme};
use holoscommander::console::{Console, ConsoleEvent};
use holoscommander::input::{Focus, KeyCode, KeyEvent, KeyModifiers, dispatch};
use holoscommander::panel::Side;
use holoscommander::vfs::{Entry, VfsPath};
use tokio::sync::mpsc;

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

/// A headless app with a live `/bin/sh` on a PTY, plus the channel its reader
/// thread is writing to.
fn app_with_shell() -> (App, mpsc::Receiver<ConsoleEvent>) {
    let mut config = Config::default();
    config.console.shell = "/bin/sh".to_string();
    // The prompt hooks are bash and zsh only, and `/bin/sh` may be either - the
    // tests below assert on exact byte queues, so nothing extra goes down the
    // wire.
    config.console.inject_hooks = false;

    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    app.navigate(Side::Left, VfsPath::local("/home/thorin/src"));
    let _ = app.take_pending_reads();
    let tab = app.left.active_tab_mut();
    tab.entries = vec![Entry::file("alpha.rs"), Entry::file("My Report.pdf")];
    tab.loading = false;
    tab.cursor = 0;

    let (tx, rx) = mpsc::channel::<ConsoleEvent>(64);
    let console = Console::spawn(&app.config.console, Path::new("/"), (24, 80), tx)
        .expect("/bin/sh starts on a pty");
    app.set_console(Some(console));
    assert!(app.console_owns_cmdline(), "the shell is running");
    (app, rx)
}

fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    dispatch(app, KeyEvent::new(code, mods)).expect("dispatch never fails on a synthetic key");
}

/// Drain the byte queue `dispatch` left for the shell.
fn queued(app: &mut App) -> Vec<u8> {
    app.take_pending_shell().0
}

// ---------------------------------------------------------------------------
// what reaches the shell, and what does not
// ---------------------------------------------------------------------------

#[test]
fn typing_on_the_command_line_reaches_the_shell_and_not_this_application() {
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::CommandLine);

    press(&mut app, KeyCode::Char('l'), NONE);
    press(&mut app, KeyCode::Char('s'), NONE);
    press(&mut app, KeyCode::Char(' '), NONE);
    assert_eq!(queued(&mut app), b"ls ");
    assert_eq!(
        app.cmdline.text(),
        "",
        "the shell owns the text, so nothing is held here"
    );
}

#[test]
fn the_line_editing_keys_are_the_shells() {
    // "Line editing is the shell's. History, completion, Ctrl+R,
    // vi or emacs bindings ... because the keys reach the shell."
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::CommandLine);
    app.cmdline.set_text("this must not be touched");

    for (code, mods, bytes) in [
        (KeyCode::Char('w'), CTRL, &[0x17_u8][..]), // kill word
        (KeyCode::Char('u'), CTRL, &[0x15][..]),    // kill line
        (KeyCode::Char('k'), CTRL, &[0x0b][..]),    // kill to end
        (KeyCode::Char('a'), CTRL, &[0x01][..]),    // line start
        (KeyCode::Char('e'), CTRL, &[0x05][..]),    // line end
        (KeyCode::Char('r'), CTRL, &[0x12][..]),    // reverse search
        (KeyCode::Left, NONE, b"\x1b[D"),           // caret left
        (KeyCode::Right, NONE, b"\x1b[C"),          // caret right
        (KeyCode::Backspace, NONE, &[0x7f][..]),    // rubout
        (KeyCode::Delete, NONE, b"\x1b[3~"),        // delete
        (KeyCode::Tab, NONE, b"\t"),                // completion
    ] {
        press(&mut app, code, mods);
        assert_eq!(queued(&mut app), bytes, "{code:?} did not reach the shell");
    }
    assert_eq!(
        app.cmdline.text(),
        "this must not be touched",
        "not one of those touched this application's own line"
    );

    // Esc is the exception, and it is a deliberate one: a person who presses
    // Esc at a command line wants the line gone, and with a shell running the
    // line is the shell's, so it is emptied with a kill-line rather than by
    // clearing a buffer here that the shell is not reading from. Forwarding
    // the Esc instead left the characters sitting in the shell and the next
    // Enter ran them.
    press(&mut app, KeyCode::Esc, NONE);
    assert_eq!(
        queued(&mut app),
        &[0x15][..],
        "Esc empties the shell's line rather than being forwarded to it"
    );
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "and none of them moved focus"
    );
}

#[test]
fn the_three_keys_spec_9_0_names_are_intercepted() {
    let (mut app, _rx) = app_with_shell();

    // Up and Down leave for the panel, and move its cursor a row.
    app.set_focus(Focus::CommandLine);
    app.left.active_tab_mut().cursor = 0;
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(app.left.active_tab().cursor, 1);
    assert!(queued(&mut app).is_empty(), "Down never reached the shell");

    // Ctrl+Enter inserts the filename at the shell's cursor and takes focus to
    // the command line.
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(app.focus, Focus::CommandLine);
    assert_eq!(
        queued(&mut app),
        b"'My Report.pdf' ",
        "quoted and with the separating space"
    );

    // Ctrl+O hides the panels; again brings them back.
    press(&mut app, KeyCode::Char('o'), CTRL);
    assert_eq!(app.focus, Focus::Console);
    assert!(
        queued(&mut app).is_empty(),
        "Ctrl+O never reached the shell"
    );
    press(&mut app, KeyCode::Char('o'), CTRL);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
}

#[test]
fn the_full_screen_console_gives_a_program_every_other_key() {
    // "It is not a split, not a pane, and not a shrunken terminal:
    // it is the same screen the shell would have had on its own ... a program
    // that wants a terminal gets a real one."
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::Console);

    for (code, mods, bytes) in [
        (KeyCode::F(5), NONE, &b"\x1b[15~"[..]),  // not "copy"
        (KeyCode::F(10), NONE, &b"\x1b[21~"[..]), // not "quit"
        (KeyCode::Char('c'), CTRL, &[0x03][..]),  // interrupt, not the clipboard
        (KeyCode::Up, NONE, b"\x1b[A"),           // history, not "leave to panel"
        (KeyCode::Tab, NONE, b"\t"),              // not "other panel"
        (KeyCode::Enter, NONE, b"\r"),
    ] {
        press(&mut app, code, mods);
        assert_eq!(queued(&mut app), bytes, "{code:?} was intercepted");
        assert_eq!(app.focus, Focus::Console, "{code:?} moved focus");
        assert!(!app.should_quit, "{code:?} quit the application");
        assert!(!app.dialog_is_open(), "{code:?} opened a dialog");
    }
}

#[test]
fn ctrl_c_keeps_its_two_meanings_by_context() {
    // "Ctrl+C/Ctrl+X/Ctrl+V is a path clipboard ... Ctrl+C keeps
    // its interrupt meaning in the console, resolved by context."
    let (mut app, _rx) = app_with_shell();

    app.set_focus(Focus::Panel(Side::Left));
    press(&mut app, KeyCode::Char('c'), CTRL);
    assert!(app.clipboard.is_some(), "on a panel it is the clipboard");
    assert!(queued(&mut app).is_empty());

    app.clipboard = None;
    app.set_focus(Focus::Console);
    press(&mut app, KeyCode::Char('c'), CTRL);
    assert_eq!(queued(&mut app), &[0x03], "in the console it interrupts");
    assert!(app.clipboard.is_none());
}

#[test]
fn history_is_translated_into_the_key_the_shell_binds() {
    // the design gives history to the shell, and the design has already spent
    // Up/Down on leaving for the panel. Ctrl+Up sends the shell the plain Up it
    // does bind.
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::CommandLine);

    press(&mut app, KeyCode::Up, CTRL);
    assert_eq!(queued(&mut app), b"\x1b[A");
    press(&mut app, KeyCode::Down, CTRL);
    assert_eq!(queued(&mut app), b"\x1b[B");
    assert_eq!(app.focus, Focus::CommandLine, "and neither leaves");
}

#[test]
fn function_keys_still_manage_files_from_the_command_line() {
    // the design hands the shell the *line editing*, not the file manager.
    // With the panels on screen, F5 is still copy.
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::CommandLine);
    press(&mut app, KeyCode::F(5), NONE);
    assert!(app.dialog_is_open(), "F5 opened the copy dialog");
    assert!(queued(&mut app).is_empty(), "and sent the shell nothing");
}

// ---------------------------------------------------------------------------
// the shell's directory
// ---------------------------------------------------------------------------

#[test]
fn an_osc_7_from_the_shell_moves_the_active_panel() {
    let (mut app, _rx) = app_with_shell();
    assert_eq!(app.active_side, Side::Left);

    // A directory that really is there: the shell -> panel half does
    // not follow the shell into one that is not (`console::sync::readable`), so
    // the name that exercises the percent-decoding has to exist to be followed.
    let dir = std::env::temp_dir().join(format!(" somewhere {}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the directory the shell names");
    let encoded = dir.display().to_string().replace(' ', "%20");

    app.apply_console_event(ConsoleEvent::Output(
        format!("\x1b]7;file://{encoded}\x07").into_bytes(),
    ));
    let landed = app.left.active_tab().path.clone();
    let asked = !app.take_pending_reads().is_empty();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        landed,
        VfsPath::local(&dir),
        "percent-decoded, and the active panel followed"
    );
    assert!(asked, "and the listing was asked for");
}

/// the shell -> panel half, when there is nothing to move to.
#[test]
fn an_osc_7_naming_a_directory_that_is_not_there_leaves_the_panel_alone() {
    let (mut app, _rx) = app_with_shell();
    let before = app.left.active_tab().path.clone();

    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]7;file:///tmp/hcmd-never-existed-93b2\x07".to_vec(),
    ));
    assert_eq!(
        app.left.active_tab().path,
        before,
        "the panel does not follow the shell into a hole"
    );
    assert!(
        app.take_pending_reads().is_empty(),
        "and does not ask for a listing of one"
    );
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("does not exist"),
        "and says why rather than doing nothing: {message}"
    );
}

#[test]
fn an_osc_7_naming_another_host_is_ignored() {
    // An ssh session inside the console is somewhere else entirely.
    let (mut app, _rx) = app_with_shell();
    let before = app.left.active_tab().path.clone();
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]7;file://elsewhere-entirely/var/tmp\x07".to_vec(),
    ));
    assert_eq!(app.left.active_tab().path, before);
}

#[test]
fn the_panel_does_not_write_a_cd_over_a_line_it_cannot_see() {
    // the design allows the `cd` "only when the shell is at a prompt and its
    // input line is empty". A `/bin/sh` that does not mark its prompt cannot
    // say so, and "cannot tell" is not "yes".
    let (mut app, _rx) = app_with_shell();
    let _ = queued(&mut app);
    app.navigate(Side::Left, VfsPath::local("/usr/share"));
    assert!(
        queued(&mut app).is_empty(),
        "nothing may be typed at a line this application cannot read"
    );
}

// ---------------------------------------------------------------------------
// the walkthrough, re-observed for
// ---------------------------------------------------------------------------

/// the six-step walkthrough, driven against a **live shell**.
///
/// `tests/input_model.rs` pins the same six steps against the fallback command
/// line of the design, where this application holds the text and the caret.
/// That state is still reachable and still normative - `console.enabled =
/// false`, a shell that will not start, a shell that has died - but it is no
/// longer the common one, and the design is explicit that it was
/// "a placeholder for this, not a design".
///
/// So the *behaviour* the design requires is asserted here a second time, on
/// what actually happens now, and the two suites together say that the
/// walkthrough works in both states. What changes is only how it is observed:
///
/// > **The caret is the shell's**, and the requirement that it survive focus
/// > leaving and returning is satisfied by the shell's own line buffer rather
/// > than by state held here.
///
/// Which is why step 3 asserts that **nothing was written** across the focus
/// round trip. That is the strongest available form of "the text and the caret
/// are both kept": a line buffer that received no bytes cannot have been
/// disturbed, where the fallback could only promise not to have mutated its own
/// copy.
#[test]
fn the_spec_7_4_walkthrough_works_with_the_shell_holding_the_line() {
    let (mut app, _rx) = app_with_shell();
    app.left.active_tab_mut().cursor = 0;
    let _ = queued(&mut app);

    // 1. `Right` from the panel focuses the command line. It is a focus key
    //    here and is not forwarded; once focus is there it becomes the shell's
    //    own caret movement, which `the_line_editing_keys_are_the_shells` pins.
    press(&mut app, KeyCode::Right, NONE);
    assert_eq!(app.focus, Focus::CommandLine);
    assert!(
        queued(&mut app).is_empty(),
        "the key that *enters* the command line is not typed into it"
    );

    // 2. Type `cp `. The characters are the shell's; the caret is the shell's.
    for c in ['c', 'p', ' '] {
        press(&mut app, KeyCode::Char(c), NONE);
    }
    assert_eq!(queued(&mut app), b"cp ");
    assert_eq!(
        app.cmdline.text(),
        "",
        "no copy of the line is held here to be lost"
    );

    // 3. `Up` returns focus to the panel. The text and the caret are both kept
    //    - by the shell, because not one byte was sent to disturb them.
    press(&mut app, KeyCode::Up, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert!(
        queued(&mut app).is_empty(),
        "leaving the command line writes nothing to the shell"
    );

    // 4. `Ctrl+Enter` inserts the entry under the cursor at the shell's cursor,
    //    shell-quoted and with its separating space, and takes focus with it.
    assert_eq!(
        app.left.active_tab().current().map(|e| e.name.clone()),
        Some("alpha.rs".to_string())
    );
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(queued(&mut app), b"alpha.rs ");
    assert_eq!(
        app.focus,
        Focus::CommandLine,
        "Ctrl+Enter takes focus with it"
    );

    // 5. `Down` returns to the panel **and** steps onto the next entry, which
    //    is what makes consecutive filenames two keys each.
    press(&mut app, KeyCode::Down, NONE);
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(app.left.active_tab().cursor, 1);
    assert!(queued(&mut app).is_empty());
    press(&mut app, KeyCode::Enter, CTRL);
    assert_eq!(
        queued(&mut app),
        b"'My Report.pdf' ",
        "quoted, because the shell runs this line"
    );

    // 6. `Enter` runs it - and the shell is what runs it, so the key is
    //    forwarded rather than acted on here.
    assert_eq!(app.focus, Focus::CommandLine);
    press(&mut app, KeyCode::Enter, NONE);
    assert_eq!(queued(&mut app), b"\r");
    assert!(
        app.cmdline.history.is_empty(),
        "the design pushes what the *shell* was holding, and a /bin/sh that \
         marks no input line cannot be read"
    );
}

// ---------------------------------------------------------------------------
// the completion indicator
// ---------------------------------------------------------------------------

/// > While in panel mode the PTY keeps running; output is buffered. A
/// > completion indicator in the key bar shows when a background command has
/// > produced output.
///
/// "Background" and "a command" are both load-bearing, and both are read off
/// `OSC 133`: output arriving between `; C` and `; D` is a command's, and
/// output arriving while the console has the screen is not in the background.
#[test]
fn output_from_a_command_behind_the_panels_lights_the_indicator() {
    let (mut app, _rx) = app_with_shell();
    assert!(!app.console.activity);

    // A prompt drawing itself is not a command producing output. This is the
    // case that matters: the design writes a `cd` on every panel move, and
    // an indicator that lit on its prompt would light constantly and mean
    // nothing.
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;A\x07thorin@host:~$ \x1b]133;B\x07".to_vec(),
    ));
    assert!(
        !app.console.activity,
        "a prompt is not a background command"
    );

    // The command starts, and prints.
    app.apply_console_event(ConsoleEvent::Output(b"\x1b]133;C\x07".to_vec()));
    app.apply_console_event(ConsoleEvent::Output(b"compiling...\r\n".to_vec()));
    assert!(app.console.activity, "a build behind the panels says so");

    // Looking at the shell is what the indicator was asking for.
    app.set_focus(Focus::Console);
    assert!(!app.console.activity);

    // And output arriving while the shell's own screen is up needs no
    // indicator: it is already on screen.
    app.apply_console_event(ConsoleEvent::Output(b"still compiling...\r\n".to_vec()));
    assert!(!app.console.activity);

    // Back to the panels, and the command ends.
    app.set_focus(Focus::Panel(Side::Left));
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;D;0\x07\x1b]133;A\x07thorin@host:~$ ".to_vec(),
    ));
    assert!(
        !app.console.activity,
        "the prompt that follows a command is not itself output"
    );
}

/// The `cd` of the design is a command as far as `OSC 133` is concerned, and
/// it is **not** one the user is waiting on.
///
/// Found by driving the real binary: without this, walking the panels lit the
/// indicator on every single directory change, which is the one thing that
/// would make it worthless. The `cd`'s echo arrives *before* its own `; C`, so
/// the flag can only be cleared when the command ends - asserted here by the
/// second, genuine command still lighting it afterwards.
#[test]
fn the_cd_this_application_writes_does_not_light_the_indicator() {
    let (mut app, _rx) = app_with_shell();
    // A shell that marks its input, so the "only when … its input
    // line is empty" can be answered and the `cd` is actually written.
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
    ));
    let _ = queued(&mut app);

    app.navigate(Side::Left, VfsPath::local("/usr/share"));
    assert!(
        !queued(&mut app).is_empty(),
        "the `cd` really was written, or this test proves nothing"
    );

    // The tty echoes the line back before the shell has read it: no command is
    // running yet, and this batch must not throw away what was just recorded.
    app.apply_console_event(ConsoleEvent::Output(b"cd /usr/share\r\n".to_vec()));
    // Now it runs, and prints its new prompt.
    app.apply_console_event(ConsoleEvent::Output(b"\x1b]133;C\x07".to_vec()));
    app.apply_console_event(ConsoleEvent::Output(b"\x1b[?2004h".to_vec()));
    assert!(
        !app.console.activity,
        "walking the panels is not a background command"
    );

    // The prompt ends it, and the next command is the user's again.
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
    ));
    app.apply_console_event(ConsoleEvent::Output(b"\x1b]133;C\x07".to_vec()));
    app.apply_console_event(ConsoleEvent::Output(
        b"warning: 3 files changed\r\n".to_vec(),
    ));
    assert!(
        app.console.activity,
        "and a real command behind the panels still says so"
    );
}

// ---------------------------------------------------------------------------
// a shell that dies
// ---------------------------------------------------------------------------

#[test]
fn a_dead_shell_hands_the_command_line_back_and_offers_a_new_one() {
    let (mut app, _rx) = app_with_shell();
    app.set_focus(Focus::Console);

    app.apply_console_event(ConsoleEvent::Eof);
    assert!(!app.console_owns_cmdline(), "the shell has gone");
    assert_eq!(
        app.focus,
        Focus::Panel(Side::Left),
        "focus does not stay inside a console that no longer exists"
    );
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("Ctrl+O"),
        "it says how to get another: {message}"
    );

    // The v0.1 command line is the command line again - the "until
    // the PTY exists", which is also "once it has gone".
    app.set_focus(Focus::CommandLine);
    press(&mut app, KeyCode::Char('l'), NONE);
    press(&mut app, KeyCode::Char('s'), NONE);
    assert_eq!(app.cmdline.text(), "ls");
    assert!(queued(&mut app).is_empty(), "there is nowhere to send it");

    // And Ctrl+O asks for a new shell rather than doing nothing.
    press(&mut app, KeyCode::Char('o'), CTRL);
    assert!(app.console.restart_requested);
}

#[test]
fn the_shell_really_is_running_and_really_gets_the_bytes() {
    // The queue is what every test above asserts on; this one proves the queue
    // is connected to something. `dispatch` queues, the event loop flushes, and
    // the shell echoes.
    let (mut app, mut rx) = app_with_shell();
    app.set_focus(Focus::CommandLine);
    for c in "echo hcmd-was-here".chars() {
        press(&mut app, KeyCode::Char(c), NONE);
    }
    press(&mut app, KeyCode::Enter, NONE);

    let bytes = queued(&mut app);
    if let Some(console) = app.console.shell.as_mut() {
        console.write(&bytes, holoscommander::console::Origin::Typed);
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "the shell never echoed:\n{}",
            app.console
                .shell
                .as_ref()
                .map(|c| c.screen().contents())
                .unwrap_or_default()
        );
        if let Ok(event) = rx.try_recv() {
            app.apply_console_event(event);
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
        let screen = app
            .console
            .shell
            .as_ref()
            .map(|c| c.screen().contents())
            .unwrap_or_default();
        if screen.contains("hcmd-was-here") {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// the design: the prompt hooks, end to end
// ---------------------------------------------------------------------------

/// The whole mechanism through a real `bash`: the snippet goes in, the
/// prompt comes back marked, and the input line can be read off the screen
/// without the prompt.
///
/// Skipped where there is no `bash` - this is the one test whose subject is a
/// specific shell, and a machine without one is not a failing machine.
#[test]
fn bash_gets_the_hooks_and_the_input_line_is_readable() {
    let Some(bash) = which_bash() else {
        eprintln!("no bash on this machine; skipping the prompt-hook test");
        return;
    };

    let mut config = Config::default();
    config.console.shell = bash;
    config.console.inject_hooks = true;

    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    let (tx, mut rx) = mpsc::channel::<ConsoleEvent>(64);
    let console = Console::spawn(&app.config.console, Path::new("/tmp"), (24, 80), tx)
        .expect("bash starts on a pty");
    app.set_console(Some(console));
    app.set_focus(Focus::CommandLine);

    // Wait for the snippet to have been read, run and the first marked prompt
    // drawn. `input_text` answering at all is the proof: it is None until
    // OSC 133;B has said where the input starts.
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_text)
            .is_some()
    });

    assert_eq!(
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_text)
            .as_deref(),
        Some(""),
        "a fresh prompt has an empty input line"
    );
    assert_eq!(
        app.console.shell.as_ref().and_then(Console::input_is_empty),
        Some(true),
        "which is what the design needs before it may write a `cd`"
    );

    // Type something. The prompt is not part of it.
    for c in "echo one".chars() {
        press(&mut app, KeyCode::Char(c), NONE);
    }
    let bytes = queued(&mut app);
    if let Some(console) = app.console.shell.as_mut() {
        console.write(&bytes, holoscommander::console::Origin::Typed);
    }
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_text)
            .is_some_and(|t| t == "echo one")
    });
    assert_eq!(
        app.console.shell.as_ref().and_then(Console::input_is_empty),
        Some(false),
        "and now a `cd` would be refused, because the line is not empty"
    );

    // shell -> panel: `cd` in the console moves the active panel.
    let bytes = b"\x15cd /usr/share\r".to_vec(); // Ctrl+U first, in case of noise
    if let Some(console) = app.console.shell.as_mut() {
        console.write(&bytes, holoscommander::console::Origin::Typed);
    }
    pump_until(&mut app, &mut rx, |app| {
        app.left.active_tab().path == VfsPath::local("/usr/share")
    });
}

/// `bash` on `$PATH`, or `None`.
fn which_bash() -> Option<String> {
    ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .map(str::to_string)
}

/// Feed console output into the app until `done` holds, or fail loudly.
fn pump_until(app: &mut App, rx: &mut mpsc::Receiver<ConsoleEvent>, done: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(app) {
        assert!(
            Instant::now() < deadline,
            "timed out; the console shows:\n{}",
            app.console
                .shell
                .as_ref()
                .map(|c| c.screen().contents())
                .unwrap_or_default()
        );
        match rx.try_recv() {
            Ok(event) => app.apply_console_event(event),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// the two halves must not fight.
///
/// Every `cd` this application writes produces a prompt, and every prompt
/// produces an `OSC 7` - which arrives *later*. Walking into a directory and
/// straight back out again used to drag the panel back in as the first `cd`'s
/// echo landed; `App::set_console`'s bookkeeping is what stops it.
#[test]
fn walking_in_and_straight_back_out_does_not_bounce() {
    let Some(bash) = which_bash() else {
        eprintln!("no bash on this machine; skipping the cwd-sync test");
        return;
    };
    let mut config = Config::default();
    config.console.shell = bash;
    config.console.inject_hooks = true;

    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    let (tx, mut rx) = mpsc::channel::<ConsoleEvent>(64);
    let console = Console::spawn(&app.config.console, Path::new("/usr"), (24, 80), tx)
        .expect("bash starts on a pty");
    app.set_console(Some(console));
    app.navigate(Side::Left, VfsPath::local("/usr"));
    let _ = app.take_pending_reads();
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_is_empty)
            .is_some()
    });

    // In, then straight back out, faster than either prompt can answer.
    for target in ["/usr/share", "/usr"] {
        app.navigate(Side::Left, VfsPath::local(target));
        let _ = app.take_pending_reads();
        let bytes = queued(&mut app);
        if let Some(console) = app.console.shell.as_mut() {
            console.write(&bytes, holoscommander::console::Origin::Typed);
        }
    }
    assert_eq!(app.left.active_tab().path, VfsPath::local("/usr"));

    // Let both echoes land. The panel must still be where the user left it.
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(event) => app.apply_console_event(event),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
        assert_eq!(
            app.left.active_tab().path,
            VfsPath::local("/usr"),
            "an echoed OSC 7 dragged the panel somewhere the user had left"
        );
    }
}

/// a `cd` is written "only when the shell is at a prompt and its
/// input line is empty" - and **a scrolled-back console cannot say**.
///
/// `Shift+PgUp` leaves the console's view in the scrollback, and nothing resets
/// it until the next write. `vt100` addresses `contents_between` in *visible*
/// rows while `OSC 133 ; B` recorded a row of the live grid, so at any offset
/// the marked row reads whatever history has moved into its place - blank far
/// more often than not. This used to report an empty input line for a shell
/// with `rm -rf important` half-typed on it, and the next panel move appended
/// `cd '<path>'\r` to that line, which the shell then ran as one command.
#[test]
fn a_scrolled_console_never_reports_a_half_typed_line_as_empty() {
    let Some(bash) = which_bash() else {
        eprintln!("no bash on this machine; skipping the scrolled-console test");
        return;
    };
    let mut config = Config::default();
    config.console.shell = bash;
    config.console.inject_hooks = true;

    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    let (tx, mut rx) = mpsc::channel::<ConsoleEvent>(64);
    let console = Console::spawn(&app.config.console, Path::new("/usr"), (24, 80), tx)
        .expect("bash starts on a pty");
    app.set_console(Some(console));
    app.navigate(Side::Left, VfsPath::local("/usr"));
    let _ = app.take_pending_reads();
    let _ = queued(&mut app);
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_is_empty)
            .is_some()
    });

    // Fill the scrollback, so there is somewhere to scroll back to.
    write_to_shell(&mut app, b"seq 1 60\r");
    pump_until(&mut app, &mut rx, |app| {
        app.console.shell.as_ref().and_then(Console::input_is_empty) == Some(true)
    });

    // A half-typed command, not submitted.
    write_to_shell(&mut app, b"rm -rf important");
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_text)
            .is_some_and(|t| t == "rm -rf important")
    });
    assert_eq!(
        app.console.shell.as_ref().and_then(Console::input_is_empty),
        Some(false),
        "live, the line is read exactly and a `cd` is refused as Busy"
    );

    // `Ctrl+O` to the console, `Shift+PgUp` to walk back, `Ctrl+O` to the
    // panels - the real key path, and the one that leaves the offset behind:
    // nothing resets it until the next write to the shell.
    press(&mut app, KeyCode::Char('o'), CTRL);
    assert_eq!(app.focus, Focus::Console, "Ctrl+O shows the shell");
    press(&mut app, KeyCode::PageUp, KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('o'), CTRL);
    let _ = app.take_pending_reads();
    assert_eq!(app.focus, Focus::Panel(Side::Left), "and Ctrl+O comes back");
    assert!(
        app.console.shell.as_ref().map_or(0, Console::scroll_offset) > 0,
        "the view is in the scrollback"
    );
    assert_eq!(
        app.console.shell.as_ref().and_then(Console::input_is_empty),
        None,
        "scrolled back, the honest answer is 'cannot tell'"
    );

    // And that is what stops the `cd`: nothing at all is queued for the shell.
    app.navigate(Side::Left, VfsPath::local("/usr/share"));
    let _ = app.take_pending_reads();
    assert!(
        queued(&mut app).is_empty(),
        "a panel move while the shell has something typed writes nothing"
    );
}

/// "A shell that dies does not take the application with it" -
/// and it has to be *noticed* for that to mean anything.
///
/// EOF on the pty master says the last slave descriptor has closed, which a
/// background job outlives: `sleep 5 &` then `exit` leaves the shell gone and
/// the slave held open, so no `ConsoleEvent::Eof` ever arrives. Without asking
/// the child directly the console reported a live shell for as long as the job
/// ran - drawing a dead prompt at the foot of the panel view, queueing keys
/// into a pty nobody reads, and holding the exited shell as a zombie.
#[test]
fn a_shell_that_exits_behind_a_background_job_is_still_noticed() {
    let Some(bash) = which_bash() else {
        eprintln!("no bash on this machine; skipping the orphaned-shell test");
        return;
    };
    let mut config = Config::default();
    config.console.shell = bash;
    config.console.inject_hooks = true;

    let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
    let (tx, mut rx) = mpsc::channel::<ConsoleEvent>(64);
    let console = Console::spawn(&app.config.console, Path::new("/tmp"), (24, 80), tx)
        .expect("bash starts on a pty");
    app.set_console(Some(console));
    pump_until(&mut app, &mut rx, |app| {
        app.console
            .shell
            .as_ref()
            .and_then(Console::input_is_empty)
            .is_some()
    });

    // The job keeps the slave open long past the shell that started it.
    write_to_shell(&mut app, b"sleep 5 </dev/null >/dev/null 2>&1 &\r");
    write_to_shell(&mut app, b"exit\r");

    // Well inside the job's lifetime: the point is that this does not wait for
    // it. Nothing here polls the pty for an EOF that is not coming.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && app.console_owns_cmdline() {
        while let Ok(event) = rx.try_recv() {
            app.apply_console_event(event);
        }
        app.service_console_exit();
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        !app.console_owns_cmdline(),
        "the shell exited, so the command line is the fallback again \
"
    );
    assert!(
        app.console.shell.as_ref().and_then(Console::exit).is_some(),
        "and it was reaped, with a status to report rather than a zombie"
    );
    assert!(
        app.message
            .as_deref()
            .is_some_and(|m| m.contains("Ctrl+O starts a new one")),
        "and said so, offering a new one: {:?}",
        app.message
    );
}

/// Write bytes to the shell the way the event loop does: through the queue.
fn write_to_shell(app: &mut App, bytes: &[u8]) {
    app.to_shell(bytes);
    let (queued, _) = app.take_pending_shell();
    if let Some(console) = app.console.shell.as_mut() {
        console.write(&queued, holoscommander::console::Origin::Typed);
    }
}

// ---------------------------------------------------------------------------
// the design: the automatic switch does not take a screen that is
// already spoken for
// ---------------------------------------------------------------------------

/// `Enter` on the command line, then `F3` before the switch delay expires.
///
/// the design switches to the console when the command is still holding the
/// terminal after `console.switch_delay` - 250 ms by default and configurable
/// up to whatever the user likes. `F3` is reachable from the command line,
/// so the window is real: without this guard the file the user
/// had just opened vanished mid-screen, the keystrokes went to the shell, and
/// the viewer was left on the stack where nothing would ever draw or pop it -
/// the next `Esc` dropped the user into a file they had stopped reading
/// instead of back into the panels.
#[test]
fn the_automatic_console_switch_never_takes_the_screen_from_the_viewer() {
    use holoscommander::viewer::Viewer;

    let (mut app, _rx) = app_with_shell();
    // A prompt, then a command that is still holding the terminal.
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
    ));
    app.command_was_run();
    assert!(
        app.console_switch_deadline().is_some(),
        "the switch is armed (the `auto`)"
    );
    app.apply_console_event(ConsoleEvent::Output(b"cat\r\n\x1b]133;C\x07".to_vec()));

    // `F3` before the deadline.
    let id = app.next_viewer_id();
    let cfg = app.config.viewer.clone();
    let v = Viewer::open_memory(id, "notes.txt", "alpha\nbeta\n".to_string(), &cfg).expect("open");
    app.push_viewer(v);
    assert!(app.viewer_is_shown());

    // The deadline passes.
    app.service_console_switch(Instant::now() + Duration::from_secs(60));
    assert!(
        app.viewer_is_shown(),
        "the viewer still has the screen, got {:?}",
        app.focus
    );
    assert_eq!(app.viewer_depth(), 1, "and it is still the only one");
    assert!(
        app.console_switch_deadline().is_none(),
        "the switch is dropped rather than left armed to fire later"
    );

    // Closing the viewer hands the screen back to the panels, not to a console
    // the user never asked to see.
    app.pop_viewer();
    assert_eq!(app.focus, Focus::Panel(Side::Left));
    assert_eq!(app.viewer_depth(), 0);
}

/// The same guard for a dialog, which is a question waiting for an answer.
#[test]
fn the_automatic_console_switch_never_takes_the_screen_from_a_dialog() {
    use holoscommander::dialog::MessageDialog;

    let (mut app, _rx) = app_with_shell();
    app.apply_console_event(ConsoleEvent::Output(
        b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
    ));
    app.command_was_run();
    app.apply_console_event(ConsoleEvent::Output(b"cat\r\n\x1b]133;C\x07".to_vec()));
    app.push_dialog(Box::new(MessageDialog::new(
        "Careful",
        vec!["about to do something".to_string()],
    )));
    let asked = app.focus;
    app.service_console_switch(Instant::now() + Duration::from_secs(60));
    assert_eq!(app.focus, asked, "the question is still on screen");
}
