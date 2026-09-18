//! `Ctrl+N`: the share, for as long as this is on screen.
//!
//! The dialog is the server's lifetime: it opens when the listener starts and
//! closing it stops the listener. What it shows is what somebody sharing a
//! few files wants to read off the screen and nothing more - the addresses to
//! give out, and the last few requests, so they can see the other side
//! actually fetching.

use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::dialog::{
    Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_buttons, draw_text,
};
use crate::input::DialogId;
use crate::serve::Served;
use crate::serve::http::human_size;

/// How many requests the log keeps: the newest, and enough to watch a
/// browser pull a page and its assets or a client walk a folder. Fewer are
/// drawn on a terminal too short for all of them.
pub const LOG_ROWS: usize = 10;

/// The one button. `Esc` and `Enter` press it too.
const STOP: &str = "Stop serving";

/// The box's inside width. Fixed: a dialog that grew with its longest log
/// line would jump about as requests came in. A longer line is cut with an
/// ellipsis by [`draw_text`]; the frame clamps this to the terminal.
const WIDTH: u16 = 76;

/// The share dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeDialog {
    /// The ways in: one per address worth giving out.
    urls: Vec<String>,
    /// How many selected entries are shared.
    count: usize,
    /// The last [`LOG_ROWS`] requests, oldest first.
    log: VecDeque<String>,
    /// Why the listener stopped on its own, if it did.
    failed: Option<String>,
    /// Something about how it started worth reading: the port is not the
    /// configured one.
    note: Option<String>,
    /// The host firewall's state and what opens the port, from
    /// [`crate::serve::firewall`].
    firewall: Vec<String>,
}

impl ServeDialog {
    /// A dialog for a share reachable at `urls`, holding `count` entries.
    #[must_use]
    pub fn new(urls: Vec<String>, count: usize) -> Self {
        Self {
            urls,
            count,
            log: VecDeque::with_capacity(LOG_ROWS),
            failed: None,
            note: None,
            firewall: Vec::new(),
        }
    }

    /// Set the firewall lines shown under the addresses.
    pub fn firewall(&mut self, lines: Vec<String>) {
        self.firewall = lines;
    }

    /// Add a line under the addresses about how the share started.
    pub fn note(&mut self, text: String) {
        self.note = Some(text);
    }

    /// Log one answered request, stamped with the wall-clock time it was
    /// answered: a share left up for an afternoon should say when the other
    /// side fetched, not only that it did.
    pub fn push(&mut self, served: &Served) {
        let line = format!(
            "{} {:<8} {:<3} {:>9}  {}  {}",
            chrono::Local::now().format("%H:%M:%S"),
            served.method,
            served.status,
            human_size(served.bytes),
            served.peer,
            served.path
        );
        self.log.push_back(line);
        while self.log.len() > LOG_ROWS {
            self.log.pop_front();
        }
    }

    /// Note that the listener has stopped on its own.
    pub fn failed(&mut self, why: String) {
        self.failed = Some(why);
    }

    /// The addresses shown.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// The log, oldest first.
    #[must_use]
    pub fn log(&self) -> Vec<&str> {
        self.log.iter().map(String::as_str).collect()
    }
}

impl Dialog for ServeDialog {
    fn id(&self) -> DialogId {
        DialogId::Serve
    }

    fn title(&self) -> String {
        let plural = if self.count == 1 { "" } else { "s" };
        format!("Serving {} item{plural}", self.count)
    }

    fn size_hint(&self) -> (u16, u16) {
        // Nothing here changes while the dialog is up: the addresses, the
        // note and the firewall lines are set before it is shown, the log
        // has a fixed number of rows, and the row a failure would use is
        // reserved from the start. So the box never resizes. The frame
        // clamps it to the terminal; `render` then draws fewer log rows
        // rather than losing the button.
        let rows = self
            .urls
            .len()
            .saturating_add(usize::from(self.note.is_some()))
            .saturating_add(self.firewall.len())
            // The blank, the log heading, the log, the failure row, the
            // button row, and the border.
            .saturating_add(2)
            .saturating_add(LOG_ROWS)
            .saturating_add(2)
            .saturating_add(2);
        (
            WIDTH.saturating_add(2),
            u16::try_from(rows).unwrap_or(u16::MAX),
        )
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // The one button, and either key that presses a button: there is no
        // "keep serving in the background", by design.
        if key.is_cancel() || key.is_accept() {
            return DialogOutcome::Accept(DialogResult::None);
        }
        DialogOutcome::Ignored
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let body = style.body();
        let ascii = style.ascii;
        let mut y = area.y;
        let line = |f: &mut Frame, y: &mut u16, text: &str| {
            if *y < area.bottom() {
                draw_text(f, Rect::new(area.x, *y, area.width, 1), text, body, ascii);
            }
            *y = y.saturating_add(1);
        };
        for url in &self.urls {
            line(f, &mut y, url);
        }
        if let Some(note) = &self.note {
            line(f, &mut y, note);
        }
        for text in &self.firewall {
            line(f, &mut y, text);
        }
        line(f, &mut y, "");
        line(f, &mut y, "Requests:");
        // The button keeps its row whatever the height, and the failure
        // row above it is kept whether or not there is a failure to show;
        // the log gets what is left between here and them, oldest rows
        // first to go.
        let button_row = area.bottom().saturating_sub(1);
        let failed_row = button_row.saturating_sub(1);
        let room = usize::from(failed_row.saturating_sub(y));
        if self.log.is_empty() {
            line(f, &mut y, "  none yet");
        }
        let skip = self.log.len().saturating_sub(room);
        for entry in self.log.iter().skip(skip) {
            line(f, &mut y, &format!("  {entry}"));
        }
        if let Some(why) = &self.failed
            && failed_row > area.y
        {
            let row = Rect::new(area.x, failed_row, area.width, 1);
            draw_text(f, row, &format!("stopped: {why}"), body, ascii);
        }
        if button_row > area.y {
            let row = Rect::new(area.x, button_row, area.width, 1);
            draw_buttons(f, row, &[STOP], 0, style);
        }
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
