//! The dialog stack, and where focus goes when it empties.
//!
//! Dialogs form a stack: a dialog opened from another sits over it, and
//! popping one uncovers the one beneath. The focus a pop restores is the one
//! that existed **before the stack was entered**, not before the frame being
//! popped, which is why every frame carries the bottom frame's restore target
//! rather than its own predecessor's.
//!
//! # Leaving without answering is a refusal
//!
//! the design ask their questions through a `oneshot` the connect
//! task is waiting on, and dropping the channel is how a refusal is expressed.
//! Every way out of such a dialog other than answering it therefore has to
//! drop the channel here. That is the whole difference between
//! [`App::pop_dialog`] and [`App::pop_answered_dialog`], and it is a
//! difference the stack has to make because the dialog cannot see it.

use crate::app::App;
use crate::dialog::{Dialog, DialogFrame};
use crate::input::{DialogId, Focus};

impl App {
    /// Open a dialog and give it focus.
    ///
    /// The focus to return to is remembered on the frame, so a dialog opened
    /// from the command line hands focus back to the command line - with its
    /// caret intact, because nothing touched it.
    pub fn push_dialog(&mut self, dialog: Box<dyn Dialog>) {
        // A dialog opened from another dialog inherits the bottom frame's
        // restore target, or the whole stack would unwind to a dialog that is
        // no longer there.
        let restore = self
            .dialogs
            .first()
            .map_or(self.focus, |frame| frame.restore);
        // A chord's first half belongs to the panel it was typed at. A dialog
        // that opens over it - a job finishing and raising its summary is the
        // case that needs no keystroke at all - would otherwise leave the chord
        // armed, and the first key pressed after the dialog closes would be
        // swallowed as its second half (a dialog consumes all
        // input, which includes the input that was half-typed).
        self.keyboard.pending_chord = None;
        let id = dialog.id();
        self.dialogs.push(DialogFrame { dialog, restore });
        self.set_focus(Focus::Dialog(id));
    }

    /// Close the top dialog, restoring focus.
    ///
    /// Focus goes to the dialog underneath when there is one, and otherwise to
    /// whatever had it when the stack was opened.
    pub fn pop_dialog(&mut self) -> Option<Box<dyn Dialog>> {
        self.pop_dialog_inner(true)
    }

    /// [`App::pop_dialog`] for a dialog that is leaving **because it was
    /// answered**, which is the one exit that must not drop the channel.
    ///
    /// the questions are answered through a `oneshot`
    /// the connect task waits on, and dropping it is a refusal. `pop_dialog`
    /// drops it, correctly, for every other way out. It cannot tell the two
    /// apart, so on the answering path it dropped the channel a moment before
    /// `answer_host_key` went to write to it: the answer went nowhere, the
    /// task saw a closed receiver, and accepting a host key reported "the host
    /// key was not accepted" no matter which button was pressed. Since the
    /// key was never accepted the connection never reached authentication
    /// either, so the password prompt appeared to be missing as well.
    pub fn pop_answered_dialog(&mut self) -> Option<Box<dyn Dialog>> {
        self.pop_dialog_inner(false)
    }

    fn pop_dialog_inner(&mut self, refuse_pending: bool) -> Option<Box<dyn Dialog>> {
        let frame = self.dialogs.pop()?;
        // the questions are answered through a `oneshot`
        // the connect task is waiting on, and **dropping it is a refusal**. A
        // dialog that leaves the stack any other way than by answering -
        // `Esc`, `close_dialogs`, a reload - therefore has to drop the channel
        // here, or the task would wait for an answer that is never coming
        // (the design, S6).
        match frame.dialog.id() {
            // These two carry a question, so they drop their channel only when
            // they leave WITHOUT answering it.
            DialogId::HostKey if refuse_pending => self.connector.refuse_host_key(),
            DialogId::RemoteSecret if refuse_pending => self.connector.refuse_secret(),
            // This one carries no question. the design gives a changed host
            // key no override, so there is nothing to accept and every way out
            // of it - including pressing Enter on the message - abandons the
            // attempt. Leaving it out of the answered path would strand a
            // connect task on a key the user was never offered.
            DialogId::HostKeyChanged => self.abandon_connect(),
            DialogId::HostKey | DialogId::RemoteSecret => {}
            _ => {}
        }
        match self.dialogs.last() {
            Some(next) => self.set_focus(Focus::Dialog(next.dialog.id())),
            None => self.set_focus(frame.restore),
        }
        Some(frame.dialog)
    }

    /// Close every dialog.
    pub fn close_dialogs(&mut self) {
        while self.pop_dialog().is_some() {}
    }

    /// The whole stack, bottom first. The renderer draws them in this order so
    /// a nested dialog sits on top of its parent.
    pub fn dialogs(&self) -> &[DialogFrame] {
        &self.dialogs
    }

    /// The whole stack, bottom first, mutably, which is what the pre-draw
    /// layout pass needs: every frame is laid out, not only the top one, because
    /// every frame is drawn.
    pub fn dialogs_mut(&mut self) -> &mut [DialogFrame] {
        &mut self.dialogs
    }

    /// The dialog with focus.
    pub fn top_dialog(&self) -> Option<&dyn Dialog> {
        self.dialogs.last().map(|f| f.dialog.as_ref())
    }

    /// The dialog with focus, mutably - which is what a key event needs.
    pub fn top_dialog_mut(&mut self) -> Option<&mut Box<dyn Dialog>> {
        self.dialogs.last_mut().map(|f| &mut f.dialog)
    }

    /// Is a dialog open?
    pub fn dialog_is_open(&self) -> bool {
        !self.dialogs.is_empty()
    }

    /// Open a modal message box. The commonest thing a completed job wants.
    pub fn show_message(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.push_dialog(Box::new(crate::dialog::MessageDialog::new(title, lines)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};
    use crate::panel::Side;

    #[test]
    fn a_dialog_takes_focus_and_gives_it_back_to_where_it_came_from() {
        use crate::dialog::MessageDialog;

        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.set_focus(Focus::Panel(Side::Right));
        assert!(!app.dialog_is_open());

        app.push_dialog(Box::new(MessageDialog::line("Done", "ok")));
        assert_eq!(app.focus, Focus::Dialog(DialogId::Message));
        assert!(app.dialog_is_open());
        // The active side is untouched: a dialog is not a panel switch.
        assert_eq!(app.active_side, Side::Right);

        app.pop_dialog();
        assert_eq!(app.focus, Focus::Panel(Side::Right));
        assert!(!app.dialog_is_open());
    }

    #[test]
    fn a_nested_dialog_unwinds_to_the_original_focus_not_to_its_parent() {
        use crate::dialog::MessageDialog;
        use crate::input::DialogId;

        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        app.set_focus(Focus::CommandLine);
        app.push_dialog(Box::new(MessageDialog::line("Outer", "a")));
        app.push_dialog(Box::new(MessageDialog::line("Inner", "b")));
        assert_eq!(app.dialogs().len(), 2);

        app.pop_dialog();
        assert_eq!(
            app.focus,
            Focus::Dialog(DialogId::Message),
            "focus falls to the dialog underneath"
        );
        app.pop_dialog();
        assert_eq!(
            app.focus,
            Focus::CommandLine,
            "and only then back to the command line"
        );
    }

    #[test]
    fn answering_the_host_key_prompt_reaches_the_task_waiting_on_it() {
        // The bug: `pop_dialog` drops the pending oneshot, which is right for
        // every exit except the one that answers. It ran first on the
        // answering path too, so `answer_host_key(true)` wrote to a channel
        // that was already gone, the task saw a closed receiver and read it as
        // a refusal, and accepting a host key reported "the host key was not
        // accepted" whichever button was pressed.
        use crate::dialog::ConfirmDialog;
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let (reply, answer) = tokio::sync::oneshot::channel();
        app.connector.hold_host_key(reply);
        app.push_dialog(Box::new(ConfirmDialog::new(
            DialogId::HostKey,
            "Unknown host key",
            vec!["SHA256:whatever".to_string()],
        )));

        // The answering path, exactly as `input::dispatch` walks it.
        let closed = app.pop_answered_dialog().map(|d| d.id());
        assert_eq!(closed, Some(DialogId::HostKey));
        app.answer_host_key(true);
        assert_eq!(answer.blocking_recv(), Ok(true), "the task heard yes");
    }

    #[test]
    fn a_host_key_prompt_dismissed_any_other_way_still_refuses() {
        // The invariant the drop exists for, which the fix must not lose:
        // Esc, close_dialogs, a reload - anything that is not an answer -
        // drops the channel, and a dropped channel is a refusal.
        use crate::dialog::ConfirmDialog;
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let (reply, answer) = tokio::sync::oneshot::channel();
        app.connector.hold_host_key(reply);
        app.push_dialog(Box::new(ConfirmDialog::new(
            DialogId::HostKey,
            "Unknown host key",
            vec!["SHA256:whatever".to_string()],
        )));
        app.pop_dialog();
        assert!(!app.connector.awaiting_host_key());
        assert!(answer.blocking_recv().is_err(), "a dropped channel refuses");
    }
}
