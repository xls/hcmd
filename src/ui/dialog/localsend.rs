//! `Alt+X`: the device picker.
//!
//! A list of the devices discovery has heard, filling in as they answer; an
//! address field for a device that will not announce, or one on another
//! subnet; a PIN field for a device that asks for one; Send and Cancel.
//! The list is the thing most people use, so it has focus first and a
//! letter jumps to the next device whose name starts with it, the way the
//! drives popup does. The box is a fixed size and never resizes as devices
//! arrive.
//!
//! The answer goes back as [`DialogResult::Text`], the device and the PIN
//! encoded by [`encode_choice`], because the dialog result has no variant
//! for a device and the drives popup's rule - no new variant to get wrong -
//! holds here too. [`decode_choice`] is the other half, used by the
//! application.

use ratatui::Frame;
use ratatui::layout::Rect;

use super::field::Field;
use crate::dialog::{
    Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle, FocusRing, draw_mnemonic_buttons,
    draw_text,
};
use crate::input::{DialogId, KeyCode};
use crate::localsend::{DeviceType, Peer, Protocol};
use crate::ui::text::Glyphs;

/// The controls, in ring order.
const LIST: usize = 0;
const ADDRESS: usize = 1;
const PIN: usize = 2;
const SEND: usize = 3;
const CANCEL: usize = 4;

/// The box's inside width and how many device rows it shows.
const WIDTH: u16 = 72;
const LIST_ROWS: u16 = 8;

/// The picker.
pub struct SendDeviceDialog {
    /// How many selected entries are going.
    /// What is going: how many folders and files were selected.
    selection: Selection,
    /// What that comes to once counted: every file and every byte.
    summary: Option<Summary>,
    /// Devices heard, by name.
    peers: Vec<Peer>,
    /// The device the cursor is on.
    cursor: usize,
    /// A typed address, `host` or `host:port`.
    address: Field,
    /// A typed PIN.
    pin: Field,
    /// Which control has focus.
    ring: FocusRing,
    /// Why the last Enter did nothing.
    refusal: Option<String>,
    /// Why nobody will be heard: discovery could not start.
    not_listening: Option<String>,
}

impl std::fmt::Debug for SendDeviceDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendDeviceDialog")
            .field("selection", &self.selection)
            .field("peers", &self.peers.len())
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl SendDeviceDialog {
    /// A picker for `selection`, with nobody heard yet and nothing counted.
    #[must_use]
    pub fn new(selection: Selection) -> Self {
        Self {
            selection,
            summary: None,
            peers: Vec::new(),
            cursor: 0,
            address: Field::new(),
            pin: Field::new(),
            ring: FocusRing::new(5),
            refusal: None,
            not_listening: None,
        }
    }

    /// Say that discovery is not running, and why; the address field is
    /// then the way to a device.
    pub fn not_listening(&mut self, why: String) {
        self.not_listening = Some(why);
    }

    /// What the selection comes to, once the folders have been walked.
    pub fn set_summary(&mut self, summary: Summary) {
        self.summary = Some(summary);
    }

    /// The count, once it has arrived.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        self.summary
    }

    /// The line under the list: how many files and bytes are going, once
    /// counted - the fact to read before pressing Send - and whether that is
    /// enough to be drawn as a warning.
    fn sending_line(&self) -> (String, bool) {
        let Some(summary) = self.summary else {
            return ("Counting the files...".to_string(), false);
        };
        let size = crate::serve::http::human_size(summary.bytes);
        let plural = if summary.files == 1 { "" } else { "s" };
        if summary.files > MANY_FILES {
            (
                format!(
                    "Warning: {} files, {size} - more than {MANY_FILES} files are about to go",
                    summary.files
                ),
                true,
            )
        } else {
            (
                format!("{} file{plural}, {size} in all", summary.files),
                false,
            )
        }
    }

    /// Replace the list with what discovery has now, keeping the cursor on
    /// the same device when it is still there.
    pub fn set_peers(&mut self, peers: Vec<Peer>) {
        let on = self.peers.get(self.cursor).cloned();
        self.peers = peers;
        self.cursor = on
            .and_then(|was| {
                self.peers.iter().position(|p| {
                    (p.fingerprint.is_some() && p.fingerprint == was.fingerprint)
                        || (p.host == was.host && p.port == was.port)
                })
            })
            .unwrap_or(0)
            .min(self.peers.len().saturating_sub(1));
    }

    /// The devices listed.
    #[must_use]
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// What Enter would send to: the typed address when there is one, else
    /// the device under the cursor.
    #[must_use]
    pub fn choice(&self) -> Option<(Peer, String)> {
        let pin = self.pin.text().trim().to_string();
        if !self.address.is_empty() {
            return Some((Peer::typed(self.address.text()), pin));
        }
        self.peers.get(self.cursor).cloned().map(|p| (p, pin))
    }

    fn accept(&mut self) -> DialogOutcome {
        match self.choice() {
            Some((peer, pin)) => {
                DialogOutcome::Accept(DialogResult::Text(encode_choice(&peer, &pin)))
            }
            None => {
                self.refusal = Some("no device yet - wait, or type an address".to_string());
                DialogOutcome::Consumed
            }
        }
    }

    /// A letter on the list: the next device after the cursor whose name
    /// starts with it, wrapping.
    fn jump(&mut self, ch: char) {
        let n = self.peers.len();
        if n == 0 {
            return;
        }
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        for step in 1..=n {
            let at = (self.cursor + step) % n;
            if self
                .peers
                .get(at)
                .and_then(|p| p.alias.chars().next())
                .is_some_and(|first| first.to_lowercase().next() == Some(lower))
            {
                self.cursor = at;
                return;
            }
        }
    }

    /// One device row as drawn.
    fn row_text(peer: &Peer, width: usize) -> String {
        let kind = match peer.device_type {
            Some(DeviceType::Mobile) => "mobile",
            Some(DeviceType::Desktop) => "desktop",
            Some(DeviceType::Web) => "web",
            Some(DeviceType::Headless) => "headless",
            Some(DeviceType::Server) => "server",
            Some(DeviceType::Unknown) | None => "",
        };
        let model = peer.model.as_deref().unwrap_or("");
        // An unencrypted device is shown as what it is: an http:// address.
        let scheme = match peer.protocol {
            Protocol::Https => "",
            Protocol::Http => "http://",
        };
        let line = format!(
            "{:<20} {:<8} {:<12} {scheme}{}:{}",
            crate::ui::text::fit_left(&peer.alias, 20, crate::ui::text::Crop::End, "…"),
            kind,
            crate::ui::text::fit_left(model, 12, crate::ui::text::Crop::End, "…"),
            peer.host,
            peer.port
        );
        crate::ui::text::fit_left(&line, width, crate::ui::text::Crop::End, "…")
    }

    /// The rows of the box: the list, then the fields, then the buttons.
    /// The rows of the box, laid out like the other dialogs: no blank rows,
    /// a rule between the list and the fields, the buttons on the last row.
    fn rects(area: Rect) -> Rects {
        let row = |n: u16| Rect::new(area.x, area.y.saturating_add(n), area.width, 1);
        let list = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            LIST_ROWS.min(area.height),
        );
        let rule = row(1 + LIST_ROWS);
        let sending = row(2 + LIST_ROWS);
        let label_w = 9_u16;
        let field = |n: u16, w: u16| {
            Rect::new(
                area.x.saturating_add(label_w),
                area.y.saturating_add(n),
                w.min(area.width.saturating_sub(label_w)),
                1,
            )
        };
        let address = field(3 + LIST_ROWS, area.width);
        let pin = field(4 + LIST_ROWS, 12);
        let refusal = row(5 + LIST_ROWS);
        let buttons = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        Rects {
            list,
            rule,
            sending,
            address,
            pin,
            refusal,
            buttons,
        }
    }
}

/// What was selected to send: folders and files, at the top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    /// Folders among the selected entries.
    pub folders: usize,
    /// Files among them.
    pub files: usize,
}

/// Past this many files the sending line is a warning: a folder that looked
/// like one thing is about to become a long list on the other side's screen.
pub const MANY_FILES: u64 = 50;

impl Selection {
    /// "folder", "3 files", "2 folders", "2 folders and 1 file".
    #[must_use]
    pub fn describe(self) -> String {
        let folders = match self.folders {
            0 => None,
            1 => Some("folder".to_string()),
            n => Some(format!("{n} folders")),
        };
        let files = match self.files {
            0 => None,
            n => Some(format!("{n} file{}", if n == 1 { "" } else { "s" })),
        };
        match (folders, files) {
            (Some(f), Some(g)) => format!("{f} and {g}"),
            (Some(f), None) => f,
            (None, Some(g)) => g,
            (None, None) => "nothing".to_string(),
        }
    }
}

/// What the selection comes to once every folder has been walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// Every file that would be sent.
    pub files: u64,
    /// Every byte.
    pub bytes: u64,
}

/// Where the parts of the box land.
struct Rects {
    list: Rect,
    rule: Rect,
    sending: Rect,
    address: Rect,
    pin: Rect,
    refusal: Rect,
    buttons: Rect,
}

/// The device and PIN as one line: tab-separated fields, none of which can
/// hold a tab - a host, a port, a scheme, a fingerprint, a name and a PIN.
#[must_use]
pub fn encode_choice(peer: &Peer, pin: &str) -> String {
    let protocol = match peer.protocol {
        Protocol::Http => "http",
        Protocol::Https => "https",
    };
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        peer.host,
        peer.port,
        protocol,
        peer.fingerprint.as_deref().unwrap_or(""),
        peer.alias.replace('\t', " "),
        pin.replace('\t', "")
    )
}

/// [`encode_choice`] undone, or `None` for anything else.
#[must_use]
pub fn decode_choice(text: &str) -> Option<(Peer, String)> {
    let mut parts = text.split('\t');
    let host = parts.next()?.to_string();
    let port: u16 = parts.next()?.parse().ok()?;
    let protocol = match parts.next()? {
        "http" => Protocol::Http,
        "https" => Protocol::Https,
        _ => return None,
    };
    let fingerprint = parts.next().filter(|f| !f.is_empty()).map(str::to_string);
    let alias = parts.next()?.to_string();
    let pin = parts.next().unwrap_or("").to_string();
    if host.is_empty() || alias.is_empty() {
        return None;
    }
    Some((
        Peer {
            alias,
            host,
            port,
            protocol,
            fingerprint,
            device_type: None,
            model: None,
        },
        pin,
    ))
}

impl Dialog for SendDeviceDialog {
    fn id(&self) -> DialogId {
        DialogId::SendDevice
    }

    /// "Send 3 files", "Send folder - 12 files", "Send 2 folders - 40 files":
    /// what was picked, and for folders what that comes to.
    fn title(&self) -> String {
        let what = self.selection.describe();
        if self.selection.folders == 0 {
            return format!("Send {what}");
        }
        match self.summary {
            Some(summary) => format!(
                "Send {what} - {} file{}",
                summary.files,
                if summary.files == 1 { "" } else { "s" }
            ),
            None => format!("Send {what} - counting..."),
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        // The heading, the list, the rule, the sending line, two fields, the
        // refusal row, the buttons, and the border. Fixed: devices arriving
        // must not move the box.
        (
            WIDTH.saturating_add(2),
            1 + LIST_ROWS + 1 + 1 + 2 + 1 + 1 + 2,
        )
    }

    fn mnemonic_letters(&self) -> Vec<char> {
        vec!['s', 'n']
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // A cancel is answered, not merely closed: `DialogOutcome::Cancel`
        // pops the dialog and tells nobody, and the application has
        // discovery and the queued paths to put away. The same route the
        // share dialog takes, for the same reason.
        // `Alt+S` and `Alt+N` are the buttons, from anywhere in the box.
        match key.mnemonic() {
            Some('s') => return self.accept(),
            Some('n') => return DialogOutcome::Accept(DialogResult::None),
            _ => {}
        }
        if key.is_cancel() {
            return DialogOutcome::Accept(DialogResult::None);
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        match self.ring.index() {
            LIST => match key.press.code {
                KeyCode::Up => {
                    self.cursor = self.cursor.saturating_sub(1);
                    DialogOutcome::Consumed
                }
                KeyCode::Down => {
                    self.cursor = self
                        .cursor
                        .saturating_add(1)
                        .min(self.peers.len().saturating_sub(1));
                    DialogOutcome::Consumed
                }
                KeyCode::Home => {
                    self.cursor = 0;
                    DialogOutcome::Consumed
                }
                KeyCode::End => {
                    self.cursor = self.peers.len().saturating_sub(1);
                    DialogOutcome::Consumed
                }
                KeyCode::Enter => self.accept(),
                _ => {
                    if let Some(ch) = key.text().filter(|c| c.is_alphanumeric()) {
                        self.jump(ch);
                        return DialogOutcome::Consumed;
                    }
                    DialogOutcome::Ignored
                }
            },
            ADDRESS | PIN => {
                if key.is_accept() {
                    return self.accept();
                }
                let field = if self.ring.is(ADDRESS) {
                    &mut self.address
                } else {
                    &mut self.pin
                };
                if field.handle(key) {
                    DialogOutcome::Consumed
                } else {
                    DialogOutcome::Ignored
                }
            }
            SEND => {
                if key.is_accept() || key.press.code == KeyCode::Char(' ') {
                    self.accept()
                } else {
                    DialogOutcome::Ignored
                }
            }
            _ => {
                if key.is_accept() || key.press.code == KeyCode::Char(' ') {
                    DialogOutcome::Accept(DialogResult::None)
                } else {
                    DialogOutcome::Ignored
                }
            }
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        let body = style.body();
        let ascii = style.ascii;
        let rects = Self::rects(area);
        let heading = if let Some(why) = &self.not_listening {
            format!("{why} - type an address below")
        } else if self.peers.is_empty() {
            "Listening for devices - open LocalSend on the other device".to_string()
        } else {
            let plural = if self.peers.len() == 1 { "" } else { "s" };
            format!("{} device{plural} heard:", self.peers.len())
        };
        draw_text(
            f,
            Rect::new(area.x, area.y, area.width, 1),
            &heading,
            body,
            ascii,
        );
        // A rule under the list, as the drives popup draws one, then what
        // is going.
        draw_text(
            f,
            rects.rule,
            &Glyphs::new(ascii)
                .horizontal()
                .repeat(usize::from(rects.rule.width)),
            body,
            ascii,
        );
        let (sending, warn) = self.sending_line();
        // The warning takes the colour the dialogs use for a refusal.
        let sending_style = if warn { style.button(true) } else { body };
        draw_text(f, rects.sending, &sending, sending_style, ascii);
        let list_focused = self.ring.is(LIST);
        for row in 0..rects.list.height {
            let Some(peer) = self.peers.get(usize::from(row)) else {
                break;
            };
            let rect = Rect::new(
                rects.list.x,
                rects.list.y.saturating_add(row),
                rects.list.width,
                1,
            );
            let text = format!(
                "  {}",
                Self::row_text(peer, usize::from(rect.width).saturating_sub(2))
            );
            let on = usize::from(row) == self.cursor;
            let painted = if on {
                style.row_cursor(list_focused)
            } else {
                body
            };
            draw_text(f, rect, &text, painted, ascii);
        }
        let label_at = |y: u16| Rect::new(area.x, y, 9, 1);
        draw_text(
            f,
            label_at(rects.address.y),
            "Address:",
            style.focus_label(self.ring.is(ADDRESS)),
            ascii,
        );
        draw_text(
            f,
            label_at(rects.pin.y),
            "PIN:",
            style.focus_label(self.ring.is(PIN)),
            ascii,
        );
        self.address.render(f, rects.address, style);
        self.pin.render(f, rects.pin, style);
        if let Some(why) = &self.refusal {
            draw_text(f, rects.refusal, why, style.button(true), ascii);
        }
        let focused = match self.ring.index() {
            SEND => 0,
            CANCEL => 1,
            _ => usize::MAX,
        };
        draw_mnemonic_buttons(
            f,
            rects.buttons,
            &[("Send", Some('s')), ("Cancel", Some('n'))],
            focused,
            style,
        );
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let rects = Self::rects(area);
        if self.ring.is(ADDRESS) {
            self.address.cursor(rects.address)
        } else if self.ring.is(PIN) {
            self.pin.cursor(rects.pin)
        } else {
            None
        }
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
#[path = "localsend_tests.rs"]
mod tests;
