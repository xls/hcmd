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

use crate::dialog::{Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, draw_text};
use crate::input::DialogId;
use crate::serve::Served;
use crate::serve::http::human_size;

/// How many requests the log shows: the newest, and enough to see a browser
/// pull a page and its assets.
pub const LOG_ROWS: usize = 5;

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
        }
    }

    /// Log one answered request.
    pub fn push(&mut self, served: &Served) {
        let line = format!(
            "{:<8} {:<3} {:>9}  {}  {}",
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
        let widest = self
            .urls
            .iter()
            .chain(self.log.iter())
            .map(|l| crate::ui::text::width(l))
            .max()
            .unwrap_or(0)
            .max(44);
        let w = u16::try_from(widest.saturating_add(6)).unwrap_or(u16::MAX);
        // The addresses, a blank, the log heading, the log rows, a blank, the
        // hint, and the border.
        let rows = self
            .urls
            .len()
            .saturating_add(3)
            .saturating_add(LOG_ROWS)
            .saturating_add(usize::from(self.failed.is_some()))
            .saturating_add(3);
        (w, u16::try_from(rows).unwrap_or(u16::MAX))
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // Either way out stops the share: there is no "keep serving in the
        // background", by design.
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
        line(f, &mut y, "");
        line(f, &mut y, "Requests:");
        if self.log.is_empty() {
            line(f, &mut y, "  none yet");
        }
        for entry in &self.log {
            line(f, &mut y, &format!("  {entry}"));
        }
        if let Some(why) = &self.failed {
            line(f, &mut y, &format!("stopped: {why}"));
        }
        line(f, &mut y, "");
        line(f, &mut y, "Esc stops serving");
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
