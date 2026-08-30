//! The command line's editable state - **for the states
//! that have no shell**.
//!
//! # This is the fallback now, not the design
//!
//! the design gave the command line to the shell: the row at the foot of the
//! panel view *is* the shell's own input line, prompt and all, rendered from
//! the PTY by [`crate::ui::cmdline`]. Everything in this file is what the
//! command line is when there is no shell to be - and that is three real
//! states, not a legacy corner:
//!
//! 1. [`crate::app::App::headless`] - no terminal, therefore no PTY. The
//!    the design input-model tests drive it, and the design makes
//!    that constructible-and-drivable property normative.
//! 2. A shell that would not start: `console.enabled = false`, a bad
//!    `console.shell`, a host with no usable shell.
//! 3. A shell that has died and has not been restarted yet.
//!
//! [`crate::app::App::console_owns_cmdline`] is the one question that tells
//! them apart, and it is asked in exactly one sentence per call path. While it
//! is true, [`CommandLine::text`], [`CommandLine::caret`] and
//! [`CommandLine::overwrite`] are **neither read nor written**: they keep their
//! last values, the renderer does not draw them, and the editing keys never
//! reach them because they are forwarded to the shell.
//!
//! # Where the persistent caret went
//!
//! > **The caret is the shell's**, and the requirement that it survive focus
//! > leaving and returning is satisfied by the shell's own line buffer rather
//! > than by state held here.
//!
//! [`CommandLine::caret`] is the state that requirement used to be held in, so
//! this is the place to say what replaced it: **nothing**. With a live shell,
//! moving focus to a panel and back sends the shell not one byte, so its line
//! buffer and its cursor are not merely restored afterwards - they were never
//! disturbed. That is the strongest form of the guarantee rather than a weaker
//! one, and the walkthrough still runs step for step: `Ctrl+Enter` writes the
//! shell-quoted name to the PTY at the shell's own cursor
//! ([`crate::app::App::insert_argument`]), the shell's line editor inserts it
//! mid-line, and `Enter` runs it.
//!
//! # the table is now a description, not a specification
//!
//! "the readline bindings the design lists become descriptions of what a
//! default `bash` does rather than reimplementations". With a shell running,
//! the editing methods below are **not** what happens when those keys are
//! pressed - the keys go to the shell and its own line editor answers them, in
//! whatever mode, with whatever bindings and whatever completion the user has
//! configured. Read the table this way:
//!
//! | key | With a live shell |
//! |---|---|
//! | printable char | the byte reaches the shell, which echoes it |
//! | `Left` / `Right` | the shell moves its own cursor |
//! | `Up` / `Down` | **intercepted** - the leave-for-the-panel, which also moves the panel cursor one row. Never sent |
//! | `Ctrl+Up` / `Ctrl+Down` | translated to a bare `Up` / `Down`, which is the key every shell actually binds to its history. The history walked is the shell's |
//! | `Esc` | the shell's - a vi-mode shell needs it more than this application needs a second way back to the panel. `Ctrl+U` still clears the line, in the shell |
//! | `Enter` | **intercepted**: `\r` to the PTY, and `console.switch_on_run` decides whether the screen follows the command. Nothing is pushed to any history here - it is the shell's |
//! | `Ctrl+Enter` | **intercepted**: the quoted name is written to the PTY, and focus comes here |
//! | `Tab` | the shell's completion, which knows about its own aliases, functions and `$PATH` |
//! | `Ctrl+W` / `Ctrl+U` / `Ctrl+K` / `Ctrl+A` / `Ctrl+E` | the shell's readline, which is better at them than these methods were |
//! | `Ctrl+R` | the shell's reverse search (the design names it). `Ctrl+R` on a *panel* still re-reads the panel, resolved by context |
//!
//! `crate::input::Action::belongs_to_the_shell` is that filter, and it filters
//! on the **action**, so a user who rebinds one of them keeps their binding.
//! The methods below stay because in the three states above they are still what
//! those keys do.
//!
//! # The caret is a character index, never a byte index
//!
//! [`CommandLine::caret`] counts **characters**, not bytes. Slicing `text` by it
//! directly is a bug: a path with an accented letter in it would panic on a
//! byte boundary. Use [`CommandLine::byte_offset`] to convert, or the editing
//! methods here, which all maintain the invariant `caret <= char_count()`.
//!
//! # The caret is persistent state
//!
//! It survives focus leaving and returning, and it is where `Ctrl+Enter`
//! inserts. It resets to 0 only when the line is cleared - by
//! `Esc` on an empty line, or by running the command.

use unicode_width::UnicodeWidthStr;

/// The command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandLine {
    text: String,
    /// Character index into `text`, in `0..=char_count()`.
    caret: usize,
    /// Command history, oldest first - **the fallback's, and only its**.
    ///
    /// the design gives history to the shell - "History, completion,
    /// `Ctrl+R`, vi or emacs bindings … all of it is whatever the user has
    /// configured" - and `Ctrl+Up`/`Ctrl+Down` are translated into the bare
    /// `Up`/`Down` the shell binds, so with a live shell this list is neither
    /// walked nor written: the design says in as many words that "nothing is
    /// pushed anywhere here and there is no history file - one history that the
    /// shell already maintains beats two that disagree".
    ///
    /// What is left is the history of the pre-console command line,
    /// for the shell-less states in the module documentation above, capped at
    /// `console.history_size` by [`CommandLine::push_history_capped`].
    pub history: Vec<String>,
    /// Where we are in the history while browsing it. `None` means "editing the
    /// live line".
    pub hist_pos: Option<usize>,
    /// `Insert` toggles this while the command line has focus.
    pub overwrite: bool,
}

impl CommandLine {
    /// An empty command line.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the text, clamping the caret.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.clamp();
    }

    /// The caret, as a **character** index.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// How many characters the text holds.
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// True when there is nothing on the line.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The caret as a byte offset into [`CommandLine::text`]. Always on a
    /// character boundary, so slicing with it is safe.
    pub fn byte_offset(&self) -> usize {
        self.byte_offset_of(self.caret)
    }

    /// The byte offset of a character index, clamped to the end of the text.
    pub fn byte_offset_of(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// The display width of the text before the caret, for placing the terminal
    /// cursor. Counts double-width characters as two.
    pub fn display_width_to_caret(&self) -> usize {
        let end = self.byte_offset();
        self.text.get(..end).map_or(0, UnicodeWidthStr::width)
    }

    /// Move the caret, clamping into range.
    pub fn set_caret(&mut self, caret: usize) {
        self.caret = caret.min(self.char_count());
    }

    fn clamp(&mut self) {
        let n = self.char_count();
        if self.caret > n {
            self.caret = n;
        }
    }

    /// One character left.
    pub fn move_left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    /// One character right.
    pub fn move_right(&mut self) {
        self.caret = (self.caret.saturating_add(1)).min(self.char_count());
    }

    /// To the start of the line (`Ctrl+A`).
    pub fn move_home(&mut self) {
        self.caret = 0;
    }

    /// To the end of the line (`Ctrl+E`).
    pub fn move_end(&mut self) {
        self.caret = self.char_count();
    }

    /// Insert a character at the caret, honouring overwrite mode.
    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_offset();
        if self.overwrite && self.caret < self.char_count() {
            let next = self.byte_offset_of(self.caret.saturating_add(1));
            self.text.replace_range(at..next, &c.to_string());
        } else {
            self.text.insert(at, c);
        }
        self.caret = self.caret.saturating_add(1);
    }

    /// Insert a string at the caret; the caret advances to just past it.
    ///
    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_offset();
        self.text.insert_str(at, s);
        self.caret = self.caret.saturating_add(s.chars().count());
    }

    /// Insert a filename at the caret, adding a separating space after it
    /// unless the following character is already one, so `cp foo.txt bar.txt`
    /// composes without manual spacing.
    ///
    /// The separator is *added*, never overwritten in: both it and the name go
    /// in through [`CommandLine::insert_str`], so overwrite mode does not eat
    /// the character that followed the caret. `cp /dest` with the caret at 3
    /// becomes `cp name /dest`, not `cp name dest`.
    pub fn insert_argument(&mut self, arg: &str) {
        self.insert_str(arg);
        let following = self
            .text
            .get(self.byte_offset()..)
            .and_then(|s| s.chars().next());
        if following != Some(' ') {
            self.insert_str(" ");
        }
    }

    /// Delete the character before the caret. Returns whether anything went.
    pub fn backspace(&mut self) -> bool {
        if self.caret == 0 {
            return false;
        }
        let start = self.byte_offset_of(self.caret.saturating_sub(1));
        let end = self.byte_offset();
        self.text.replace_range(start..end, "");
        self.caret = self.caret.saturating_sub(1);
        true
    }

    /// Delete the character under the caret.
    pub fn delete(&mut self) -> bool {
        if self.caret >= self.char_count() {
            return false;
        }
        let start = self.byte_offset();
        let end = self.byte_offset_of(self.caret.saturating_add(1));
        self.text.replace_range(start..end, "");
        true
    }

    /// `Ctrl+W`: delete the word before the caret.
    pub fn kill_word(&mut self) {
        let mut chars: Vec<char> = self.text.chars().collect();
        let mut i = self.caret.min(chars.len());
        while i > 0
            && chars
                .get(i.saturating_sub(1))
                .is_some_and(|c| c.is_whitespace())
        {
            i = i.saturating_sub(1);
        }
        while i > 0
            && chars
                .get(i.saturating_sub(1))
                .is_some_and(|c| !c.is_whitespace())
        {
            i = i.saturating_sub(1);
        }
        let removed = self.caret.saturating_sub(i);
        chars.drain(i..self.caret.min(chars.len()));
        self.text = chars.into_iter().collect();
        self.caret = self.caret.saturating_sub(removed);
    }

    /// `Ctrl+U`: delete the whole line, leaving the caret at 0.
    pub fn kill_line(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// `Ctrl+K`: delete from the caret to the end of the line.
    pub fn kill_to_end(&mut self) {
        let at = self.byte_offset();
        self.text.truncate(at);
    }

    /// Clear the line and reset the caret - the only thing that resets it.
    ///
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.hist_pos = None;
    }

    /// [`CommandLine::push_history`], with `console.history_size` applied.
    ///
    /// The cap is on the *fallback's* list - the only history this application
    /// keeps - and it drops the oldest entries, which is what
    /// every shell does with `HISTSIZE`. A cap of zero keeps nothing.
    pub fn push_history_capped(&mut self, command: impl Into<String>, cap: usize) {
        self.push_history(command);
        let len = self.history.len();
        if len > cap {
            self.history.drain(..len.saturating_sub(cap));
        }
    }

    /// Remember a command that was run. Consecutive duplicates are collapsed.
    pub fn push_history(&mut self, command: impl Into<String>) {
        let command = command.into();
        if command.trim().is_empty() {
            return;
        }
        if self.history.last() == Some(&command) {
            self.hist_pos = None;
            return;
        }
        self.history.push(command);
        self.hist_pos = None;
    }

    /// `Ctrl+Up` / `Alt+P`: older entry - **only where no shell is running**.
    ///
    /// With one, `crate::input::run_action` sends a bare `Up` instead and the
    /// history walked is the shell's own.
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next = match self.hist_pos {
            None => self.history.len().saturating_sub(1),
            Some(0) => return false,
            Some(i) => i.saturating_sub(1),
        };
        self.hist_pos = Some(next);
        if let Some(entry) = self.history.get(next) {
            self.text = entry.clone();
            self.move_end();
        }
        true
    }

    /// `Ctrl+Down` / `Alt+N`: newer entry, then back to an empty line - again
    /// only where no shell is running. See [`CommandLine::history_prev`].
    pub fn history_next(&mut self) -> bool {
        let Some(i) = self.hist_pos else {
            return false;
        };
        let next = i.saturating_add(1);
        if next >= self.history.len() {
            self.hist_pos = None;
            self.text.clear();
            self.caret = 0;
            return true;
        }
        self.hist_pos = Some(next);
        if let Some(entry) = self.history.get(next) {
            self.text = entry.clone();
            self.move_end();
        }
        true
    }
}

/// Quote a filename for a POSIX shell, because the command line runs through
/// one: an unquoted `My Report (final).pdf` is a bug.
///
/// Names that need no quoting are returned unchanged, so the common case stays
/// readable.
pub fn shell_quote(name: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '-' | '/' | '@' | '+' | ',' | '=' | ':' | '%')
    };
    if !name.is_empty() && name.chars().all(safe) && !name.starts_with('-') {
        return name.to_string();
    }
    // Single quotes protect everything except a single quote itself.
    let mut out = String::with_capacity(name.len() + 2);
    out.push('\'');
    for c in name.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_7_4_walkthrough() {
        // the design steps 2-5, without any focus involved: the caret is
        // state that belongs to the command line.
        let mut cl = CommandLine::new();
        cl.insert_str("cp ");
        assert_eq!(cl.caret(), 3);
        assert_eq!(cl.text(), "cp ");

        cl.insert_argument("foo.txt");
        assert_eq!(cl.text(), "cp foo.txt ");
        assert_eq!(cl.caret(), 11);

        cl.insert_argument("bar.txt");
        assert_eq!(cl.text(), "cp foo.txt bar.txt ");
    }

    #[test]
    fn insertion_is_at_the_caret_not_at_the_end() {
        let mut cl = CommandLine::new();
        cl.set_text("cp  /dest");
        cl.set_caret(3);
        cl.insert_argument("a.txt");
        assert_eq!(cl.text(), "cp a.txt /dest");
        // The caret sits just past what was inserted; the separating space was
        // already there, so none was added.
        assert_eq!(cl.caret(), 8);
    }

    #[test]
    fn the_caret_is_a_character_index_not_a_byte_index() {
        let mut cl = CommandLine::new();
        cl.set_text("héllo");
        cl.set_caret(2);
        assert_eq!(cl.char_count(), 5);
        assert_eq!(cl.byte_offset(), 3, "h + 2-byte e-acute");
        cl.insert_char('X');
        assert_eq!(cl.text(), "héXllo");
    }

    #[test]
    fn display_width_counts_wide_characters() {
        let mut cl = CommandLine::new();
        cl.set_text("日本x");
        cl.set_caret(2);
        assert_eq!(cl.display_width_to_caret(), 4);
    }

    #[test]
    fn overwrite_replaces_rather_than_inserts() {
        let mut cl = CommandLine::new();
        cl.set_text("abc");
        cl.set_caret(1);
        cl.overwrite = true;
        cl.insert_char('X');
        assert_eq!(cl.text(), "aXc");
        // At the end, overwrite still appends.
        cl.move_end();
        cl.insert_char('Z');
        assert_eq!(cl.text(), "aXcZ");
    }

    #[test]
    fn the_separating_space_is_added_not_overwritten_in_overwrite_mode() {
        // the space is *added* after the inserted name. In
        // overwrite mode it must not eat the character that followed the
        // caret - `/dest` becoming `dest` silently changes an absolute
        // destination into a relative one.
        let mut cl = CommandLine::new();
        cl.set_text("cp /dest");
        cl.set_caret(3);
        cl.overwrite = true;
        cl.insert_argument("notes.txt");
        assert_eq!(cl.text(), "cp notes.txt /dest");
        assert_eq!(cl.caret(), 13);
    }

    #[test]
    fn readline_editing() {
        let mut cl = CommandLine::new();
        cl.set_text("git commit --amend");
        cl.move_end();
        cl.kill_word();
        assert_eq!(cl.text(), "git commit ");
        cl.kill_to_end();
        assert_eq!(cl.text(), "git commit ");
        cl.set_caret(4);
        cl.kill_to_end();
        assert_eq!(cl.text(), "git ");
        cl.kill_line();
        assert_eq!(cl.text(), "");
        assert_eq!(cl.caret(), 0);
    }

    #[test]
    fn history_walks_and_returns_to_a_blank_line() {
        let mut cl = CommandLine::new();
        cl.push_history("one");
        cl.push_history("two");
        cl.push_history("two");
        assert_eq!(cl.history.len(), 2);
        assert!(cl.history_prev());
        assert_eq!(cl.text(), "two");
        assert!(cl.history_prev());
        assert_eq!(cl.text(), "one");
        assert!(!cl.history_prev());
        assert!(cl.history_next());
        assert_eq!(cl.text(), "two");
        assert!(cl.history_next());
        assert_eq!(cl.text(), "");
        assert!(!cl.history_next());
    }

    #[test]
    fn names_are_quoted_only_when_they_need_it() {
        assert_eq!(shell_quote("foo.txt"), "foo.txt");
        assert_eq!(shell_quote("a/b-c_d.tar.gz"), "a/b-c_d.tar.gz");
        assert_eq!(shell_quote("My Report.pdf"), "'My Report.pdf'");
        assert_eq!(shell_quote("*.rs"), "'*.rs'");
        assert_eq!(shell_quote("--weird"), "'--weird'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote(""), "''");
    }
}
