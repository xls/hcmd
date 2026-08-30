//! Getting terminal events into the tokio select loop.
//!
//! `crossterm`'s async `EventStream` lives behind its `event-stream` feature,
//! which pulls in `futures-core` - not on the table, and the design records
//! the decision not to add it. A blocking reader on its own thread costs
//! nothing and lets key events, resize events and directory-scan results all
//! land in the same `select!`.

use std::io;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use crossterm::event::Event;
use tokio::sync::mpsc;

/// How long the reader thread blocks before checking whether the UI has gone
/// away. Only the shutdown latency of a thread nobody is waiting for.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The queue depth between the reader thread and the UI task. Keystrokes
/// arriving faster than this are the terminal replaying a paste, and bracketed
/// paste turns that into one event anyway.
pub const EVENT_CHANNEL_DEPTH: usize = 64;

/// Whether the reader thread may take keystrokes off the real stdin.
///
/// **This is what makes the `F4` usable at all.** While `nvim` runs
/// with inherited stdio, this thread and the editor are two readers on one
/// terminal, and every keystroke is a coin flip between them. Closing the gate
/// parks the thread; opening it lets it go again.
///
/// A static rather than a handle threaded through the call graph, for the same
/// reason [`super::Term`]'s raw-mode and alternate-screen flags are: the
/// terminal is one global object, [`super::Term::restore`] is already driven
/// from a panic hook that has no `self`, and a second reader thread is not a
/// thing this application can have.
struct Gate {
    /// Whether reading is allowed, and whether the thread has acknowledged.
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

static GATE: Gate = Gate {
    state: Mutex::new((true, false)),
    changed: Condvar::new(),
};

/// How long [`hold_input`] waits for the reader thread to acknowledge.
///
/// It parks at the top of its loop, so the wait is one [`POLL_INTERVAL`] plus
/// scheduling. Bounded and then ignored rather than waited on forever: an
/// acknowledgement that never comes must delay `F4` by a fraction of a second,
/// not hang the application in front of an editor that never starts.
const ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// Stop reading stdin, and wait for the reader thread to say it has stopped.
///
///
/// Call before handing the terminal to an external program. Idempotent.
pub fn hold_input() {
    let mut guard = GATE.state.lock().unwrap_or_else(PoisonError::into_inner);
    guard.0 = false;
    GATE.changed.notify_all();
    // `wait_timeout_while` returns on the timeout with the predicate still
    // true; that is the "gave up" case and is deliberately not an error.
    let (_guard, _timed_out) = GATE
        .changed
        .wait_timeout_while(guard, ACK_TIMEOUT, |state| !state.1)
        .unwrap_or_else(PoisonError::into_inner);
}

/// Read stdin again, after the external program has exited.
///
/// Idempotent, and safe to call without a matching [`hold_input`] - which is
/// what makes it correct on the failure path, where the spawn never happened.
pub fn release_input() {
    let mut guard = GATE.state.lock().unwrap_or_else(PoisonError::into_inner);
    guard.0 = true;
    guard.1 = false;
    GATE.changed.notify_all();
}

/// Park until the gate is open, and say so while parked.
fn wait_for_gate() {
    let mut guard = GATE.state.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.0 {
        return;
    }
    guard.1 = true;
    GATE.changed.notify_all();
    while !guard.0 {
        guard = GATE
            .changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner);
    }
    guard.1 = false;
}

/// Whether a byte sitting in the input buffer is ours to read.
fn gate_is_open() -> bool {
    GATE.state.lock().unwrap_or_else(PoisonError::into_inner).0
}

/// Read terminal events on a dedicated thread and forward them.
///
/// The thread is detached: it ends when the receiver is dropped, and in any
/// case when the process exits. Errors are forwarded rather than swallowed, so
/// a broken stdin surfaces as a clean exit instead of a spin.
pub fn spawn_event_thread(tx: mpsc::Sender<io::Result<Event>>) {
    let fallback = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("hcmd-input".to_string())
        .spawn(move || event_thread(&tx));
    if let Err(err) = spawned {
        // No reader thread means no input will ever arrive. Report it on the
        // channel rather than leaving the UI waiting forever. `try_send`
        // rather than `blocking_send`: this runs inside the tokio runtime.
        let _ = fallback.try_send(Err(err));
    }
}

fn event_thread(tx: &mpsc::Sender<io::Result<Event>>) {
    loop {
        wait_for_gate();
        match crossterm::event::poll(POLL_INTERVAL) {
            Ok(true) => {
                // `poll` does not consume, so a keystroke that arrived while
                // the gate was closing is still in the buffer for whoever owns
                // the terminal now. Checked here and not only at the top of the
                // loop, because the wait above can be a whole `POLL_INTERVAL`
                // in the past by the time this line is reached.
                if !gate_is_open() {
                    continue;
                }
                let event = crossterm::event::read();
                let failed = event.is_err();
                if tx.blocking_send(event).is_err() || failed {
                    return;
                }
            }
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(err) => {
                let _ = tx.blocking_send(Err(err));
                return;
            }
        }
    }
}

/// What a bracketed paste should insert into the command line.
///
/// the design wants a pasted path to reach the command line as text rather
/// than as a sequence of navigation keys, which is what enabling bracketed
/// paste achieves. What is left is deciding what "the text" is: newlines are
/// stripped, because a multi-line paste into a single-line command field would
/// otherwise submit or corrupt it, and the remaining C0 control characters go
/// with them - a paste is allowed to contain them and none of them are
/// meaningful in a path. Everything else is inserted verbatim, tabs included as
/// a single space so a column-pasted path does not trigger completion.
pub fn paste_text(raw: &str) -> String {
    raw.chars()
        .filter_map(|c| match c {
            '\n' | '\r' => None,
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_are_stripped_and_the_rest_is_verbatim() {
        assert_eq!(
            paste_text("/home/thorin/My Report (final).pdf\n"),
            "/home/thorin/My Report (final).pdf"
        );
        assert_eq!(paste_text("one\r\ntwo"), "onetwo");
    }

    #[test]
    fn an_escape_inside_a_paste_cannot_reach_the_terminal() {
        assert_eq!(paste_text("a\u{1b}[31mb"), "a[31mb");
    }

    #[test]
    fn a_tab_becomes_a_space_rather_than_completion() {
        assert_eq!(paste_text("a\tb"), "a b");
    }

    #[test]
    fn unicode_survives() {
        assert_eq!(paste_text("/tmp/naïve - ünïcode"), "/tmp/naïve - ünïcode");
    }
}
