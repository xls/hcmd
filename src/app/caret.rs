//! Text put into the command line.
//!
//! Whatever arrives here lands **at the caret**, not appended: the command
//! line's caret is persistent state, and `cp <caret> /dest` is the reason it
//! is. An insert moves focus to the command line if it was not already there,
//! which the design both record as a deliberate reversal of an
//! earlier draft, because one filename followed by typing is the common case.
//!
//! # Quoted once, and sent as text rather than as keys
//!
//! A name is quoted for the shell exactly once, wherever it is going. With a
//! live shell that is not enough on its own: a filename on Linux may hold any
//! byte but `/` and `NUL`, and bytes written raw into a PTY are keys. A TAB in
//! a name would run readline's completion and substitute a different one, and
//! a newline is `accept-line`, which submits a command nobody pressed `Enter`
//! on. Everything therefore goes through [`crate::console::keys::paste`],
//! which wraps it in bracketed paste markers where the shell asked for them,
//! so readline inserts the bytes instead of obeying them.

use crate::app::App;
use crate::console::Console;
use crate::input::{Focus, shell_quote};

impl App {
    /// `Ctrl+Enter`: insert the entry under the active panel's cursor at the
    /// command line's remembered caret, **and move focus to the command line**.
    ///
    ///
    /// The focus move is the rule, not a side effect: the design both record it
    /// as "a deliberate reversal of an earlier draft" - one filename followed
    /// by typing is the common case, and `Down` gets back to the panel *and*
    /// onto the next entry when it is not.
    pub fn put_selected(&mut self, full_path: bool) {
        let tab = self.active_panel().active_tab();
        let Some(entry) = tab.current() else { return };
        let text = if full_path {
            tab.current_path()
                .map(|p| p.to_string())
                .unwrap_or_else(|| entry.name.clone())
        } else {
            entry.name.clone()
        };
        self.insert_argument(&shell_quote(&text));
        // the insert takes focus with it. One filename followed by typing or
        // running is the common case, and `Down` gets back to the panel *and*
        // onto the next entry when it is not.
        self.set_focus(Focus::CommandLine);
    }

    /// Put an argument on the command line, wherever the command line is.
    ///
    ///
    /// With a live shell this writes to the PTY **at the shell's cursor**, and
    /// insertion mid-line still works because that is what the shell's own line
    /// editor does with the characters. Without one it is the
    /// v0.1 [`CommandLine`], unchanged.
    ///
    /// the separating space is added in both modes, and in both for
    /// the same reason - `cp foo.txt bar.txt` composes without manual spacing -
    /// under the same condition: not when the character the caret is standing
    /// on is already a space. With a shell that character is read off the
    /// parsed screen, which is where the shell's caret is.
    ///
    /// # It goes to the shell as *text*, not as keystrokes
    ///
    /// "an unquoted `My Report (final).pdf` is a bug, not a
    /// nuisance" - and quoting is only half of it. A filename on Linux may hold
    /// any byte but `/` and `NUL`, control characters included, and bytes
    /// written raw into a PTY are *keys*: a TAB in a filename runs readline's
    /// completion and silently substitutes a different name, and a newline is
    /// `accept-line`, which submits a command the user never pressed `Enter`
    /// on. So the argument goes through [`crate::console::keys::paste`], which
    /// wraps it in `ESC[200~`/`ESC[201~` where the shell asked for bracketed
    /// paste - the same route [`App::paste_into_cmdline`] already takes, and
    /// the one that makes readline insert the bytes instead of obeying them.
    pub fn insert_argument(&mut self, arg: &str) {
        if !self.console_owns_cmdline() {
            self.cmdline.insert_argument(arg);
            return;
        }
        let following_is_space = self.console.shell.as_ref().is_some_and(|console| {
            let screen = console.screen();
            let (row, col) = screen.cursor_position();
            screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == " ")
        });
        let mut text = arg.to_string();
        if !following_is_space {
            text.push(' ');
        }
        let mode = self
            .console
            .shell
            .as_ref()
            .map(Console::mode)
            .unwrap_or_default();
        let bytes = crate::console::keys::paste(&text, mode);
        self.to_shell(&bytes);
    }

    /// Insert literal text at the command line's caret - a bracketed paste,
    /// which is the one thing that arrives as text rather than as
    /// a key.
    ///
    /// With a live shell the markers are passed on when the shell asked for
    /// them, so it can tell a paste from typing; without one this is the v0.1
    /// insert at the caret.
    pub fn paste_into_cmdline(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.console_owns_cmdline() {
            self.cmdline.insert_str(text);
            return;
        }
        let mode = self
            .console
            .shell
            .as_ref()
            .map(Console::mode)
            .unwrap_or_default();
        let bytes = crate::console::keys::paste(text, mode);
        self.to_shell(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app_with;

    #[test]
    fn put_selected_inserts_at_the_caret_and_takes_focus_there() {
        let mut app = app_with(&["My Report.pdf"]);
        app.cmdline.set_text("cp  /dest");
        app.cmdline.set_caret(3);
        app.put_selected(false);
        // Mid-line, not appended: ` /dest` still trails it.
        assert_eq!(app.cmdline.text(), "cp 'My Report.pdf' /dest");
        assert_eq!(
            app.focus,
            Focus::CommandLine,
            "the insert takes focus with it"
        );
    }
}
