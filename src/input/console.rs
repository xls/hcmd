//! Keys that belong to the shell.
//!
//! A running shell owns the command line, so once [`super::dispatch`] has
//! decided that the console is the consumer, what is left is deciding whether
//! this particular key is the shell's or this application's, and turning the
//! shell's into bytes.
//!
//! # `Ctrl+H` is Backspace on a legacy terminal
//!
//! It is a *bindable* key under the enhanced protocol and it is the byte
//! `Backspace` sends without one, so the resolution has to happen before the
//! keymap is consulted rather than inside it: a terminal that cannot tell the
//! two apart has not sent two different keys for the keymap to distinguish.
//!
//! # Bytes, not keystrokes
//!
//! Everything addressed to the shell is queued on
//! [`crate::console::Pane`] rather than written, because a PTY write can block
//! and `dispatch` may not touch the outside world.

use crate::app::App;
use crate::config::keymap::{KeyContext, Resolution};
use crate::error::Result;
use crate::input::{KeyCode, KeyModifiers, KeyPress, run_action};

/// Route one key while a live shell owns the keyboard.
///
/// The keymap is resolved exactly as it always is - the order does
/// not change because a shell is running - and the *action* it produced decides
/// what happens to the key:
///
/// | State | Kept here | Sent to the shell |
/// |---|---|---|
/// | command line focused | everything else, so `F5` still copies and `Ctrl+Enter` still inserts a filename | [`Action::belongs_to_the_shell`]: the twelve line-editing actions of the design |
/// | `Ctrl+O`, panels hidden | [`Action::survives_full_console`]: the toggle and the two scrollback keys | everything else, because a program that wants a terminal gets a real one |
///
/// A key that resolves to nothing is sent, which is the whole point: the shell
/// is a full application and most of what it reads is bound to nothing here.
///
/// A **chord prefix is sent rather than armed**. Nothing shipped is a chord
/// (the design gives `Ctrl+X` back to cut), and arming one inside the console
/// would swallow the shell's next key waiting for a second half it will never
/// get.
pub(super) fn console_key(app: &mut App, press: KeyPress, ctx: KeyContext) -> Result<()> {
    let full_screen = app.console_is_shown();
    // the command line **is** the shell's own input line, so the
    // keys that edit a line of text are the shell's whatever a global binding
    // says. Asked as a question about the key rather than about the action it
    // resolves to, because that is where the collision is: `Delete` is bound
    // globally to the file operation `F8` also runs, so pressing it to rub out
    // a character in the console raised "Delete the folder?" over the panel -
    // one stray Enter from deleting a directory the user was not looking at.
    if !full_screen && shell_owns_key(press) {
        forward_to_shell(app, press);
        return Ok(());
    }
    match app.keymap.resolve(ctx, press) {
        Resolution::Action(action) => {
            let ours = if full_screen {
                action.survives_full_console()
            } else {
                !action.belongs_to_the_shell()
            };
            if ours {
                return run_action(app, action, press);
            }
        }
        Resolution::ChordPending | Resolution::Unbound => {}
    }
    forward_to_shell(app, press);
    Ok(())
}

/// The keys that edit a line of text, which the shell owns while it owns the
/// command line.
///
/// **Bare only.** `Shift+Delete` is the permanent delete and is
/// not a line-editing key; the `Ctrl` combinations that readline uses
/// (`Ctrl+W`, `Ctrl+U`, `Ctrl+K`, `Ctrl+A`, `Ctrl+E`) already resolve to
/// actions [`Action::belongs_to_the_shell`] recognises, so they need nothing
/// here. This list is exactly the keys a global binding can steal out from
/// under a shell that is waiting for them.
pub(super) const fn shell_owns_key(press: KeyPress) -> bool {
    if !press.mods.is_empty() {
        return false;
    }
    matches!(
        press.code,
        KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
    )
}

/// Encode a key and queue it for the shell.
///
/// A key with no terminal encoding at all - a bare modifier press, a media key -
/// queues nothing rather than something else.
pub(super) fn forward_to_shell(app: &mut App, press: KeyPress) {
    let mode = app
        .console
        .shell
        .as_ref()
        .map(crate::console::Console::mode)
        .unwrap_or_default();
    if let Some(bytes) = crate::console::keys::encode(press, mode) {
        app.to_shell(&bytes);
    }
}

/// `Ctrl+H` **is** ASCII 0x08, the byte a legacy terminal also sends for
/// `Backspace`.
///
/// With the enhanced keyboard protocol the two are distinguishable and both
/// work. Without it, `Backspace` wins - navigation is used constantly and the
/// hidden-files toggle is not - so a `Ctrl+H` press is *delivered as*
/// `Backspace` and `Ctrl+H` is simply unavailable. `Alt+.` is the documented
/// fallback for `show_hidden` and is bound in the shipped keymap.
pub(super) fn resolve_ctrl_h(app: &App, press: KeyPress) -> KeyPress {
    let ctrl_h = KeyPress::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
    if !app.keyboard.enhanced && press == ctrl_h {
        return KeyPress::plain(KeyCode::Backspace);
    }
    press
}

/// Send the shell a bare cursor key (the history translation).
pub(super) fn shell_arrow(app: &mut App, code: KeyCode) {
    forward_to_shell(app, KeyPress::plain(code));
}

/// `Ctrl+O`.
///
/// > `Ctrl+O` **hides the panels** … `Ctrl+O` again brings the panels back,
/// > with them refreshed.
///
/// [`App::toggle_console`] is that, and is where the rule lives. The one case
/// it cannot see from `App` alone is the shell that **died while its own screen
/// was on**: the design says a dead shell is reported and a new one offered,
/// and offering it here - rather than dropping the user back to the panels to
/// press the same key a second time - is what makes the notice
/// [`crate::ui::console`] draws over that screen true.
///
/// The guard is `App::console` still being `Some`: *a shell that died*, not a
/// start that failed. After a failed start there is nothing to restart, and
/// `Ctrl+O` has to stay the way out, or the console would be a screen with no
/// key that leaves it.
pub(super) fn console_toggle(app: &mut App) {
    // the design asks for `Ctrl+O` on a connected panel to open "an
    // interactive SSH session to that host rather than the local shell". That
    // is a second console subsystem - a russh shell channel with a pty
    // request, window-size propagation and its own `vt100::Parser` - and the
    // v0.65 line does not mention it, so it is not in this milestone. What
    // the design already prescribes for FTP is what happens for every
    // protocol here: keep the local shell, **and say so**.
    let note = if app.console_is_shown() {
        None
    } else {
        app.active_panel().active_tab().remote_view().map(|view| {
            // **No version in this message**.
            // It used to name v0.7, which has now shipped without the SSH
            // console, and the design has no line after v0.7 to name
            // instead - so it says what is true and why, and names nothing.
            // A message that promises a release which has already gone by is
            // the exact lie `Action::not_implemented_message` exists to
            // prevent.
            format!(
                "{}: Ctrl+O is the local shell; an SSH console to a connected \
 host is not built",
                view.authority,
            )
        })
    };
    if app.console_is_shown() && app.console.shell.is_some() && !app.console_owns_cmdline() {
        app.console.restart_requested = true;
        app.message = Some(match app.console.shell.as_ref() {
            Some(console) => format!("{}: starting a new shell", console.program()),
            None => "starting a shell".to_string(),
        });
        return;
    }
    app.toggle_console();
    // After the toggle, because the toggle has a status line of its own and
    // this is the sentence the user needs to read.
    if let Some(note) = note {
        app.message = Some(note);
    }
}

/// `Shift+PgUp` / `Shift+PgDn`: walk the console's scrollback (the design's
/// "the full scrollback, exactly as if the file manager were not running").
///
/// A screenful at a time, less one row of overlap, which is what every terminal
/// does and what makes a long build log readable rather than a flicker book.
/// Plain `PgUp` is deliberately *not* this: it belongs to the `less` that may
/// be running in there.
pub(super) fn scroll_console(app: &mut App, back: bool) {
    let Some(console) = app.console.shell.as_mut() else {
        app.message = Some("there is no console to scroll".to_string());
        return;
    };
    let (rows, _cols) = console.screen().size();
    let page = isize::try_from(rows.saturating_sub(1).max(1)).unwrap_or(1);
    console.scroll_by(if back { page } else { page.saturating_neg() });
}

/// `Enter` on the command line.
///
/// > `Enter` on a non-empty command line writes it to the PTY. History is the
/// > shell's own, so nothing is pushed anywhere here and there is no
/// > history file - one history that the shell already maintains beats two that
/// > disagree.
///
/// Two jobs, and only the first is the shell's, which is why this action is
/// intercepted rather than forwarded. The second is the screen:
/// [`App::command_was_run`] applies `console.switch_on_run`, whose default
/// `auto` leaves the panels where they are and looks again after
/// `console.switch_delay` - a `mkdir` is back at a prompt long before then and
/// costs no flash at all.
///
/// With no shell running the v0.1 behaviour stands: the line is taken, it goes
/// to that command line's own history, the caret resets to 0,
/// and the status line says why nothing ran. That list is the fallback's, for
/// the states the design describes as having no PTY; nothing is pushed to it
/// while a shell is alive, because nothing could ever read it back.
pub(super) fn run_command_line(app: &mut App) {
    if app.console_owns_cmdline() {
        // The newline goes in whatever the line holds - a bare `Enter` at a
        // shell prompt is a blank line, and swallowing it would make the key
        // feel dead.
        app.to_shell(b"\r");
        app.command_was_run();
        return;
    }

    // An empty line runs nothing, the way a shell prompt does; `Esc` is the key
    // that gives focus back.
    if app.cmdline.is_empty() {
        return;
    }
    let command = app.cmdline.text().to_string();
    // `console.history_size` caps this list, which is the only history this
    // application keeps: with a shell alive the history is the shell's own.
    //
    app.cmdline
        .push_history_capped(command.clone(), app.config.console.history_size);
    app.cmdline.clear();
    app.message = Some(match app.console.shell.as_ref() {
        Some(console) => format!(
            "{command}: kept in the history - {}",
            console.death_notice()
        ),
        None => format!("{command}: kept in the history - no shell is running"),
    });
}
