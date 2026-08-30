//! The viewer stack.
//!
//! Viewers form a stack with ids that only ever go up. Closing one uncovers
//! the one beneath it; closing the last returns focus to what held it **before
//! the first was opened**, which is not the same as before the last was
//! opened, and is the reason the restore target is recorded once on the way in
//! rather than per frame.
//!
//! A viewer is opened by a request the event loop services, because opening
//! one reads a file. The geometry goes the other way: the renderer pushes the
//! size in before the frame is drawn, so the keys waiting behind a viewer that
//! has just been opened are applied to a viewer that knows how big it is.

use crate::app::{App, InputRoute, ViewRequest};
use crate::input::Focus;
use crate::viewer::index::IndexBatch;
use crate::viewer::{Viewer, ViewerId, copy::CopyRequest};

impl App {
    /// **The one question every viewer-aware code path asks**.
    ///
    /// True while `F3` has the screen. The viewer consumes all input and hides
    /// the panels, exactly as `Ctrl+O` does for the console - see
    /// [`App::console_is_shown`], which this is the twin of.
    pub const fn viewer_is_shown(&self) -> bool {
        matches!(self.focus, Focus::Viewer)
    }

    /// The viewer with focus.
    pub fn viewer(&self) -> Option<&Viewer> {
        self.viewers.top()
    }

    /// The viewer with focus, mutably - which is what a key event needs.
    pub fn viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.viewers.top_mut()
    }

    /// The viewer a key event should act on, whichever kind it is.
    ///
    /// the design puts a viewer **inside a panel**: `Tab` moves focus into
    /// it and "the viewer's own keys then apply". That viewer is not on the
    /// stack - it is the panel's, and the panel still has
    /// [`Focus::Panel`] - so the key handlers ask for this rather than for
    /// [`App::viewer`] and one set of handlers serves both.
    ///
    ///
    /// A full-screen viewer wins whenever there is one, because
    /// [`Focus::Viewer`] means the whole screen is that viewer and the panels
    /// behind it are a backdrop.
    pub fn focused_viewer(&self) -> Option<&Viewer> {
        if self.focus != Focus::Viewer && self.quick_view_side() == Some(self.active_side) {
            return self.quick_viewer();
        }
        self.viewer()
    }

    /// [`App::focused_viewer`], mutably - which is what a key event needs.
    pub fn focused_viewer_mut(&mut self) -> Option<&mut Viewer> {
        if self.focus != Focus::Viewer && self.quick_view_side() == Some(self.active_side) {
            return self.quick_viewer_mut();
        }
        self.viewer_mut()
    }

    /// How deep the viewer stack is. Two means the help page is over a file.
    pub fn viewer_depth(&self) -> usize {
        self.viewers.depth()
    }

    /// A fresh [`ViewerId`].
    pub fn next_viewer_id(&mut self) -> ViewerId {
        self.viewers.next_id()
    }

    /// Put a viewer on top and give it focus.
    pub fn push_viewer(&mut self, viewer: Viewer) {
        // A chord's first half belongs to the panel it was typed at; the viewer
        // consumes all input and would otherwise swallow the first key after it
        // closes as the chord's second half. Same rule as `push_dialog`.
        self.keyboard.pending_chord = None;
        // And the focus it displaced is remembered on the way in, exactly as
        // `push_dialog` remembers it: `F3` is reachable from the command line,
        // and returning to the panel from there would leave a
        // half-written command on a line that no longer has the keyboard.
        self.viewers.push(viewer, self.focus);
        self.set_focus(Focus::Viewer);
        // Laid out at once, at the size the last frame was drawn at, so that a
        // key held while the file was opening is applied to a viewer that knows
        // how big the screen is (see `viewer_view`). The event loop lays it out
        // again before it draws; a layout is a function of the window, so the
        // cost is one window read on open.
        let (rows, cols) = self.viewers.view();
        if rows > 0 && cols > 0 {
            self.service_viewer(rows, cols);
        }
    }

    /// Close the top viewer, restoring focus.
    ///
    /// Focus goes to the viewer underneath when there is one - closing the
    /// `F1` help page returns to the file it was opened over - and otherwise to
    /// the active panel.
    pub fn pop_viewer(&mut self) -> Option<Viewer> {
        let top = self.viewers.pop()?;
        if !self.viewers.is_open() {
            // Whatever had the keyboard when the stack opened - the panel
            // ordinarily, the command line when `F3` was pressed from it.
            let restore = match self.viewers.restore() {
                // Never back into a viewer that is no longer there, and never
                // into the console, which the user left by opening this.
                Focus::Viewer => Focus::Panel(self.active_side),
                other => other,
            };
            self.set_focus(restore);
        }
        Some(top)
    }

    /// Close every viewer, which is what quitting has to do - a `Viewer`'s
    /// `Drop` cancels its background scan.
    pub fn close_viewers(&mut self) {
        while self.pop_viewer().is_some() {}
    }

    /// Ask the event loop to open a viewer.
    ///
    /// Queued rather than performed, because opening reads from the filesystem
    /// and `dispatch` may not.
    pub fn request_view(&mut self, request: ViewRequest) {
        self.viewers.request(request);
    }

    /// Where keyboard input is currently routed.
    ///
    /// The event loop applies every key already waiting before it draws, so a
    /// held key cannot back up. That is only safe while consecutive keys mean
    /// the same thing, and some keys change what the next one means by queuing
    /// work the loop performs *between* frames: `F3` does not open the viewer,
    /// it queues one, because `dispatch` may not touch the filesystem.
    /// Draining past that `F3` would hand the `2`
    /// after it to the panel's quick search instead of to the viewer that is
    /// about to exist.
    ///
    /// So the loop compares this before and after each key and stops draining
    /// the moment it changes. Cheap, and it fails in the safe direction: an
    /// unnecessary stop costs one frame, a missed one misroutes a keystroke.
    pub fn input_route(&self) -> InputRoute {
        InputRoute {
            focus: self.focus,
            viewer: self.viewer().is_some(),
            pending_view: self.viewers.pending().is_some(),
            viewer_copy: self.viewer().is_some_and(Viewer::copy_pending),
            search: self.search.pending.is_some(),
            dialog: self.dialog_is_open(),
            console_restart: self.console.restart_requested,
            quit: self.should_quit,
            quick: self.quick.is_some(),
            drives: self.drives.asked().is_some(),
            open: self.handoff.open.is_some(),
        }
    }

    /// Drain the queued viewer. The event loop calls this once a frame.
    pub fn take_pending_view(&mut self) -> Option<ViewRequest> {
        self.viewers.take_pending()
    }

    /// Take the queued copy off the top viewer, if any.
    ///
    /// Queued for the same reason a view is: copying reads the file and
    /// `dispatch` may not. `main::service_viewer_copy`
    /// is what performs it.
    pub fn take_viewer_copy(&mut self) -> Option<CopyRequest> {
        self.viewer_mut()?.take_copy_request()
    }

    /// The message a finished copy leaves in the status line.
    ///
    /// The once-per-session notice is folded into the first copy's message
    /// rather than being a message of its own, because a second line the user
    /// has to dismiss is exactly what "told about once, not on every copy"
    /// exists to avoid. `delivered` is whether there was a terminal on stdout
    /// at all: a piped run has no clipboard to write to, and saying so is
    /// honest where naming `OSC 52` would not be.
    ///
    pub fn copy_report(&mut self, bytes: u64, note: Option<String>, delivered: bool) -> String {
        let unit = if bytes == 1 { "byte" } else { "bytes" };
        let mut out = format!("copied {bytes} {unit}");
        // Why it is not what the user might have expected - the
        // half-covered-word fallback of the design, above all.
        if let Some(note) = note {
            out.push_str(" - ");
            out.push_str(&note);
        }
        if !self.osc52_notice_shown {
            self.osc52_notice_shown = true;
            out.push_str(if delivered {
                " - if your terminal ignores OSC 52, the text is also on the \
                 internal clipboard (Ctrl+V at the command line)"
            } else {
                " - stdout is not a terminal, so the text is on the internal \
                 clipboard only (Ctrl+V at the command line)"
            });
        }
        out
    }

    /// `Ctrl+V` at the **command line**: paste what a viewer copy left.
    ///
    ///
    /// True when it pasted, so the caller falls through to the file
    /// paste when there is no text to put down. Only at the command line: a
    /// viewer's text never lands in a directory, and pasting into a *panel* is
    /// always files.
    pub fn paste_text_clipboard(&mut self) -> bool {
        if !matches!(self.focus, Focus::CommandLine) {
            return false;
        }
        let Some(text) = self.text_clipboard.clone() else {
            return false;
        };
        self.paste_into_cmdline(&text);
        true
    }

    /// Whether a viewer is waiting to be opened. For tests, which are the
    /// reason the queue exists.
    pub const fn pending_view(&self) -> Option<&ViewRequest> {
        self.viewers.pending()
    }

    /// Apply one batch from a background line-index scan.
    ///
    /// Offered to every viewer on the stack rather than only the top one: the
    /// help page sits over a file whose scan is still running, and that scan's
    /// batches must keep landing so the file underneath is still indexed when
    /// the help page closes. A batch whose id matches nothing is dropped, which
    /// is what makes a scan winding down after its viewer closed harmless.
    pub fn apply_index_batch(&mut self, batch: &IndexBatch) -> bool {
        // And to the quick viewer, which is not on the stack but is a viewer
        // with a scan of its own: without this it would have
        // no line numbers and no percentage, which is half of what its status
        // line says.
        let quick = self
            .quick_viewer_mut()
            .is_some_and(|v| v.apply_index(batch));
        self.viewers.apply_index(batch) || quick
    }

    /// Apply one batch from a background match counter.
    ///
    /// Offered to every viewer for the same reason a line-index batch is, and
    /// dropped just as quietly when it belongs to a closed viewer or to a
    /// search two keystrokes ago.
    pub fn apply_find_batch(&mut self, batch: &crate::viewer::find::FindBatch) -> bool {
        self.viewers.apply_find(batch)
    }

    /// The background match counters the event loop owes the open viewers.
    ///
    /// Drained the same way [`App::take_pending_jobs`] is, and for the same
    /// reason: `dispatch` queues work and never spawns it.
    ///
    pub fn take_find_jobs(&mut self) -> Vec<crate::viewer::find::FindJob> {
        self.viewers.take_find_jobs()
    }

    /// Tell the application how big a viewer's body would be on the screen
    /// just drawn (`crate::ui::viewer`'s `body_rows` / `body_cols`).
    ///
    /// The event loop calls it every frame, with or without a viewer up, so
    /// that a viewer pushed before the next frame - one whose open was slow
    /// enough for the keys behind it to be held - is laid out at the size it is
    /// about to be drawn at rather than at nothing. See `viewer_view`.
    pub const fn set_viewer_view(&mut self, rows: u16, cols: u16) {
        self.viewers.set_view(rows, cols);
    }

    /// Lay out the viewer for a screen this size, before it is drawn.
    ///
    /// Called by the event loop for the same reason
    /// [`crate::ui::sync_view_rows`] is: reading the window is the model's job
    /// and `crate::ui::draw` only has `&App`.
    pub fn service_viewer(&mut self, rows: u16, cols: u16) {
        self.viewers.set_view(rows, cols);
        let ascii = self.config.ui.ascii_borders;
        let Some(viewer) = self.viewers.top_mut() else {
            return;
        };
        // the glyphs the viewer *chooses* - the control pictures -
        // degrade with every other glyph this program picks. The renderer knows
        // the terminal and the layout is the model's, so the flag crosses here.
        viewer.set_ascii(ascii);
        if let Err(err) = viewer.layout(rows, cols) {
            self.message = Some(err.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};

    #[test]
    fn the_clipboard_notice_is_shown_once_a_session() {
        // a terminal without `OSC 52` is "told about once, not
        // on every copy". Support cannot be detected in band, so what is kept
        // is the promise the user can observe.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let first = app.copy_report(5, None, true);
        assert_eq!(
            first,
            "copied 5 bytes - if your terminal ignores OSC 52, the text is also on the \
             internal clipboard (Ctrl+V at the command line)"
        );
        assert_eq!(app.copy_report(5, None, true), "copied 5 bytes");
        assert_eq!(app.copy_report(1, None, true), "copied 1 byte");
    }

    #[test]
    fn a_copy_report_carries_the_note_that_says_why_it_is_not_what_was_expected() {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.osc52_notice_shown = true;
        let got = app.copy_report(
            4,
            Some("not aligned to the 32-bit columns - copied as hex digits".to_string()),
            true,
        );
        assert_eq!(
            got,
            "copied 4 bytes - not aligned to the 32-bit columns - copied as hex digits"
        );
    }

    #[test]
    fn a_piped_run_is_told_the_text_went_to_the_internal_clipboard_only() {
        // There is no terminal to write `OSC 52` to, and naming it would be
        // describing something that did not happen.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let got = app.copy_report(5, None, false);
        assert!(
            got.starts_with("copied 5 bytes - stdout is not a terminal"),
            "{got}"
        );
    }

    #[test]
    fn the_text_clipboard_is_pasted_at_the_command_line_and_nowhere_else() {
        // the clipboard holds paths and
        // cannot hold a string, so this is the sibling that can - and a
        // viewer's text never lands in a directory.
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.text_clipboard = Some("quick".to_string());
        assert!(!app.paste_text_clipboard(), "a panel paste is always files");
        app.set_focus(Focus::CommandLine);
        assert!(app.paste_text_clipboard());
        assert_eq!(app.cmdline.text(), "quick");
        // Nothing held: the caller falls through to the file clipboard.
        app.text_clipboard = None;
        assert!(!app.paste_text_clipboard());
    }
}
