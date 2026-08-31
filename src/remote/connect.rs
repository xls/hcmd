//! the connect dialog and its Add-host form.
//!
//! > The connect dialog offers three ways in, in this order of prominence:
//! > 1. **A quick-connect line**, focused on open [...]
//! > 2. **A list of saved hosts**, arrow-navigable, with quick search on the
//! >    same typing rules as a panel [...] `Enter` connects, `F4`
//! >    edits, `F8` deletes.
//! > 3. **An "Add host" form**: label, protocol, host, port, username,
//! >    authentication method, initial remote directory, and initial local
//! >    directory for the other panel.
//!
//! Laid out in that order, with the quick-connect line focused on open:
//!
//!
//! ```text
//!     +- Connect --------------------------------------------------+
//!     |Quick connect:                                              |
//!     |thorin@nas.local                                            |
//!     |Saved hosts:                             search: na [aa]    |
//!     |nas          sftp://thorin@nas.local:2222/srv/media         |
//!     |buildbox     sftp://thorin@buildbox:22                      |
//!     |mirror       ftp://anonymous@ftp.example.org:21             |
//!     |[ Add host ]  [ F4 Edit ]  [ F8 Delete ]                    |
//!     |[ Connect ]  [ Cancel ]                                     |
//!     +------------------------------------------------------------+
//! ```
//!
//! # It touches nothing
//!
//! The host book arrives already loaded and the answer travels back out as a
//! value: the dialog reads no file, writes no file and opens no socket, which
//! is the rule for everything `dispatch` can reach. The
//! one edge is [`crate::remote::auth::AuthPlan`], which is built here and which
//! consults `~/.ssh` for the key files that exist - noted in this module rather
//! than hidden, because it is the only I/O anywhere in this file.
//!
//! # A password on the quick-connect line
//!
//! the first example is `thorin:pass@192.168.1.10`, so this is the
//! one control in the program that can hold a credential as ordinary text. It
//! is used for that connection and nothing else:
//! it is never written to `hosts.toml`, never put in a history, and this type's
//! `Debug` runs the line through [`crate::remote::url::redact`] before printing
//! it, which is why that `Debug` is written by hand.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::config::{QuickSearchCase, QuickSearchMode};
use crate::dialog::{
    Accel, Accelerated, ConnectAnswer, Dialog, DialogKey, DialogOutcome, DialogResult, DialogStyle,
    FocusRing, Piece, draw_mnemonic, draw_mnemonic_buttons, draw_mnemonic_pieces, draw_text,
};
use crate::input::quicksearch::quick_match;
use crate::input::quicksearch::status_label;
use crate::input::{DialogId, KeyCode};
use crate::remote::auth::AuthPlan;
use crate::remote::hosts::{AuthMethod, SavedHost};
use crate::remote::url;
use crate::remote::{Protocol, Target};
use crate::ui::dialog::field::Field;
use crate::ui::dialog::{ellipsis, row};
use crate::ui::text;

/// A monotonic id per connect attempt.
///
/// An event from an attempt the user has already abandoned is dropped rather
/// than applied - the rule a directory read's generation follows, for the same
/// reason: the answer to a question nobody is asking any more must not land on
/// whatever is on screen now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectId(pub u64);

/// The attempt now running: which panel asked, where it was, and where it is
/// going.
#[derive(Debug, Clone)]
pub struct Attempt {
    /// Which attempt this is.
    pub id: ConnectId,
    /// Which panel is connecting.
    pub side: crate::panel::Side,
    /// Which of its tabs.
    pub tab: usize,
    /// Where that tab was, to go back to on disconnect.
    pub origin: crate::vfs::VfsPath,
    /// The name the cursor was on there.
    pub origin_cursor: Option<String>,
    /// Where it is connecting. Secret-free.
    pub target: crate::remote::Target,
}

/// At most one connect attempt is live.
///
/// Every question the attempt asks holds its own reply channel, and **dropping
/// a channel is a refusal**, so cancelling is the absence of an answer rather
/// than an extra code path: `Esc`, a closed dialog and an abandoned attempt
/// all mean "not accepted" through the same mechanism.
///
/// Every event an attempt produces carries the [`ConnectId`] it answers to,
/// and an event from an attempt that has been abandoned is dropped without
/// reaching the screen, so starting a second connection cannot be undone by
/// the first one finishing late.
#[derive(Debug, Default)]
pub struct Connector {
    /// `Ctrl+F` answered and the connection not started yet.
    ///
    /// One slot: the dialog closes when it answers, so a second connect cannot
    /// be asked for before this one has been handed to the event loop.
    queued: Option<Box<crate::app::ConnectRequest>>,
    /// The attempt now running, so an event from an abandoned one is dropped.
    live: Option<Box<Attempt>>,
    /// Monotonic source for [`ConnectId`].
    next: u64,
    /// The unknown-host prompt's reply channel while it is on screen.
    ///
    host_key: Option<tokio::sync::oneshot::Sender<bool>>,
    /// The password prompt's reply channel while it is on screen.
    ///
    secret: Option<tokio::sync::oneshot::Sender<Option<crate::dialog::SecretAnswer>>>,
}

impl Connector {
    /// Queue what the dialog answered, for the event loop to start.
    pub fn queue(&mut self, request: crate::app::ConnectRequest) {
        self.queued = Some(Box::new(request));
    }

    /// The next attempt id, which is never one that has been handed out
    /// before.
    pub fn next_id(&mut self) -> ConnectId {
        self.next = self.next.saturating_add(1);
        ConnectId(self.next)
    }

    /// Take the queued request and make it the live attempt.
    ///
    /// The two are one act: a request that has been handed to the event loop
    /// is exactly the attempt that is now running, and there is no moment when
    /// neither or both is true.
    pub fn start(&mut self) -> Option<Box<crate::app::ConnectRequest>> {
        let request = self.queued.take()?;
        self.live = Some(Box::new(Attempt {
            id: request.attempt,
            side: request.side,
            tab: request.tab,
            origin: request.origin.clone(),
            origin_cursor: request.origin_cursor.clone(),
            target: request.answer.target.clone(),
        }));
        Some(request)
    }

    /// Is `attempt` the one still being waited on?
    ///
    /// An event whose id is not this one belongs to an attempt that has been
    /// abandoned, and is dropped without reaching the screen.
    pub fn is_live(&self, attempt: ConnectId) -> bool {
        self.live.as_ref().is_some_and(|live| live.id == attempt)
    }

    /// Where the live attempt is going, for a message about it.
    pub fn target(&self) -> Option<&crate::remote::Target> {
        self.live.as_ref().map(|live| &live.target)
    }

    /// Finish the live attempt and hand back what it was.
    pub fn finish(&mut self) -> Option<Box<Attempt>> {
        self.live.take()
    }

    /// Hold the unknown-host prompt's reply channel while the prompt is up.
    pub fn hold_host_key(&mut self, reply: tokio::sync::oneshot::Sender<bool>) {
        self.host_key = Some(reply);
    }

    /// Hold the password prompt's reply channel while the prompt is up.
    pub fn hold_secret(
        &mut self,
        reply: tokio::sync::oneshot::Sender<Option<crate::dialog::SecretAnswer>>,
    ) {
        self.secret = Some(reply);
    }

    /// Answer the unknown-host prompt, or say there was nothing waiting.
    pub fn answer_host_key(&mut self, accepted: bool) -> bool {
        match self.host_key.take() {
            Some(reply) => reply.send(accepted).is_ok(),
            None => false,
        }
    }

    /// Answer the password prompt, or say there was nothing waiting.
    pub fn answer_secret(&mut self, answer: Option<crate::dialog::SecretAnswer>) -> bool {
        match self.secret.take() {
            Some(reply) => reply.send(answer).is_ok(),
            None => false,
        }
    }

    /// Is the unknown-host prompt still waiting on an answer?
    pub const fn awaiting_host_key(&self) -> bool {
        self.host_key.is_some()
    }

    /// Refuse the unknown-host prompt by dropping its channel.
    pub fn refuse_host_key(&mut self) {
        self.host_key = None;
    }

    /// Refuse the password prompt by dropping its channel.
    pub fn refuse_secret(&mut self) {
        self.secret = None;
    }

    /// Abandon the attempt and every question it had outstanding.
    ///
    /// Dropping the channels is what tells the task it was refused, so there
    /// is nothing else to send.
    pub fn abandon(&mut self) {
        self.live = None;
        self.host_key = None;
        self.secret = None;
    }
}

/// One focusable control of the connect dialog, in `Tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The quick-connect line. Focused on open.
    Quick,
    /// The saved-host list.
    Hosts,
    /// Open the Add-host form on a new host.
    Add,
    /// `F4`: open it on a copy of the selected host.
    Edit,
    /// `F8`: delete the selected host, after a confirmation naming it.
    Delete,
    /// Connect, to the line or to the selection.
    Connect,
    /// Give up.
    Cancel,
}

/// the letters.
///
/// `o` is the affirmative and `n` is `Cancel`, which is
/// the program-wide pair. `c` and `h` are spent on
/// nothing here: they are reserved for `Close` and `Help`, so `Host` is not
/// allowed to take the `h` it would otherwise want.
pub const MNEMONICS: &[(Control, char)] = &[
    (Control::Quick, 'q'),
    (Control::Hosts, 's'),
    (Control::Add, 'a'),
    (Control::Edit, 'e'),
    (Control::Delete, 'd'),
    (Control::Connect, 'o'),
    (Control::Cancel, 'n'),
];

/// Every control, in `Tab` order. The ring's length is this.
const CONTROLS: &[Control] = &[
    Control::Quick,
    Control::Hosts,
    Control::Add,
    Control::Edit,
    Control::Delete,
    Control::Connect,
    Control::Cancel,
];

/// Draw [`QUICK_HINT`] after the label, where the row is wide enough.
fn draw_quick_hint(f: &mut Frame, row: Rect, style: &DialogStyle) {
    let label = QUICK_LABEL.chars().count();
    // Two spaces after the label, and the hint whole or not at all.
    let start = label.saturating_add(2);
    let need = start.saturating_add(QUICK_HINT.chars().count());
    if usize::from(row.width) < need {
        return;
    }
    let Ok(x) = u16::try_from(start) else { return };
    let Ok(width) = u16::try_from(QUICK_HINT.chars().count()) else {
        return;
    };
    draw_text(
        f,
        Rect::new(row.x.saturating_add(x), row.y, width, 1),
        QUICK_HINT,
        style.body(),
        style.ascii,
    );
}

/// The quick-connect line's label.
const QUICK_LABEL: &str = "Quick connect:";
/// What may be typed on that line, beside the label.
///
/// One note rather than a help screen, because there is one shape: every
/// protocol this program speaks takes the same line, and the parts are all
/// optional except the host. A per-protocol popup would be four copies of
/// this sentence. It is dropped rather than truncated on a dialog too narrow
/// to hold it, since half a syntax is worse than none.
const QUICK_HINT: &str = "[scheme://][user[:pass]@]host[:port][/path]";
/// The saved-host list's label.
const HOSTS_LABEL: &str = "Saved hosts:";
/// How wide the label column of the list is.
const LABEL_COLUMN: usize = 12;
/// How many host rows the dialog asks for, and how far `PageUp` and `PageDown`
/// move.
///
/// A constant rather than the height the list was given last frame, because
/// remembering that would mean a render-time cell on a dialog that otherwise
/// has none, and a page that is occasionally one row out is not worth one.
const LIST_ROWS: u16 = 6;
/// Rows that are not the list: two labels, the field, two button rows and the
/// error row.
const CHROME_ROWS: u16 = 6;

/// the connect dialog.
pub struct ConnectDialog {
    field: Field,
    hosts: Vec<SavedHost>,
    cursor: usize,
    /// The panel-style quick-search buffer over the host labels (the
    /// design.
    quick: String,
    mode: QuickSearchMode,
    case: QuickSearchCase,
    default: Protocol,
    user: String,
    keyring: bool,
    home: PathBuf,
    /// True once `F4` or `F8` has changed the list, which is what puts it in
    /// the answer for the event loop to write.
    dirty: bool,
    /// The host `F8` is asking about, if any.
    confirm_delete: Option<usize>,
    /// Which button the delete confirmation has focus on. `false` is `Cancel`,
    /// which is the safe default a confirmation opens on
    /// (`src/dialog/confirm.rs`).
    confirm_yes: bool,
    error: Option<String>,
    ring: FocusRing,
}

impl ConnectDialog {
    /// Built with the host book already loaded, because a dialog may not touch
    /// the filesystem.
    ///
    /// `default` is `remote.default_protocol`, `user` is what a quick-connect
    /// line with no `user@` means for the SSH family, and `keyring` says
    /// whether a store is available at all.
    pub fn new(hosts: Vec<SavedHost>, default: Protocol, user: String, keyring: bool) -> Self {
        Self {
            field: Field::new(),
            hosts,
            cursor: 0,
            quick: String::new(),
            mode: QuickSearchMode::default(),
            case: QuickSearchCase::default(),
            default,
            user,
            keyring,
            // `$HOME`, an environment variable and not a directory read.
            home: crate::config::paths::home_dir().unwrap_or_else(|_| PathBuf::from("/")),
            dirty: false,
            confirm_delete: None,
            confirm_yes: false,
            error: None,
            ring: FocusRing::new(CONTROLS.len()),
        }
    }

    /// Match the list's quick search to the panel's own configured rules
    /// ("the same typing rules as a panel").
    ///
    /// A builder rather than two more arguments to [`ConnectDialog::new`],
    /// whose shape the design fixes.
    #[must_use]
    pub const fn with_quick_search(mut self, mode: QuickSearchMode, case: QuickSearchCase) -> Self {
        self.mode = mode;
        self.case = case;
        self
    }

    /// Open with the quick-connect line already filled in, which is what
    /// reconnecting a dropped connection offers.
    ///
    /// The text is whatever the caller has; it is never a password, because
    /// the only line this program remembers anywhere is
    /// [`crate::remote::url::redact`]ed first.
    #[must_use]
    pub fn with_line(mut self, line: impl Into<String>) -> Self {
        self.field.set_text(line);
        self
    }

    /// The host book as the dialog holds it now.
    pub fn hosts(&self) -> &[SavedHost] {
        &self.hosts
    }

    /// Replace the host book: what the Add-host form's answer does to the
    /// dialog underneath it, exactly as `Save as...` does to the Find dialog.
    ///
    pub fn set_hosts(&mut self, hosts: Vec<SavedHost>) {
        self.hosts = hosts;
        self.clamp_cursor();
        self.dirty = true;
        self.error = None;
    }

    /// Whether the host book has been edited and needs writing.
    pub const fn dirty(&self) -> bool {
        self.dirty
    }

    /// The quick-connect line as typed.
    ///
    /// **This can contain a password** (the first example), so
    /// nothing that stores or logs may take it without
    /// [`crate::remote::url::redact`] first.
    pub fn line(&self) -> &str {
        self.field.text()
    }

    /// Which saved host is selected.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// the "if no keyring is available, say so in the dialog".
    ///
    /// Shown for a selected host whose `auth` is `keyring` when this session
    /// has no store: that host will be prompted for every time, and the place
    /// to learn it is here rather than at the fourth prompt.
    pub fn note(&self) -> Option<String> {
        let host = self.selected()?;
        (host.auth == AuthMethod::Keyring && !self.keyring)
            .then(crate::remote::keyring::unavailable_message)
    }

    /// The label `F8` is asking about, when it is asking.
    pub fn confirming(&self) -> Option<&str> {
        let index = self.confirm_delete?;
        self.hosts.get(index).map(|h| h.label.as_str())
    }

    /// Which control has focus.
    pub fn focused(&self) -> Control {
        CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(Control::Quick)
    }

    /// The selected host, if there is one.
    pub fn selected(&self) -> Option<&SavedHost> {
        self.hosts.get(self.cursor)
    }

    fn clamp_cursor(&mut self) {
        self.cursor = if self.hosts.is_empty() {
            0
        } else {
            self.cursor.min(self.hosts.len().saturating_sub(1))
        };
    }

    /// Move the cursor by one row, or to an end.
    fn move_cursor(&mut self, to: usize) {
        if self.hosts.is_empty() {
            return;
        }
        self.cursor = to.min(self.hosts.len().saturating_sub(1));
        self.quick.clear();
    }

    /// One keystroke of panel quick search over the labels.
    ///
    /// A character that matches nothing is **refused** rather than typed, so
    /// the buffer always names a host that is on the screen - which is what
    /// makes `Backspace` step back through matches rather than through
    /// characters that never matched.
    fn quick_search(&mut self, ch: char) -> bool {
        let mut candidate = self.quick.clone();
        candidate.push(ch);
        let Some(found) = self
            .hosts
            .iter()
            .position(|h| quick_match(&h.label, &candidate, self.mode, self.case))
        else {
            return false;
        };
        self.quick = candidate;
        self.cursor = found;
        true
    }

    /// The quick-search buffer, for the status fragment and for tests.
    pub fn quick_buffer(&self) -> &str {
        &self.quick
    }

    /// Delete the confirmed host and close the confirmation.
    fn delete_confirmed(&mut self) {
        let Some(index) = self.confirm_delete.take() else {
            return;
        };
        if index < self.hosts.len() {
            self.hosts.remove(index);
            self.dirty = true;
        }
        self.clamp_cursor();
        self.quick.clear();
        self.confirm_yes = false;
    }

    /// The host book to hand back, or `None` when nothing changed.
    fn edited(&self) -> Option<Vec<SavedHost>> {
        self.dirty.then(|| self.hosts.clone())
    }

    /// `Enter`'s rule: "to the quick-connect line when it has focus and is not
    /// empty, to the selected host otherwise".
    fn accept(&mut self) -> DialogOutcome {
        if self.focused() == Control::Quick && !self.field.is_empty() {
            return self.connect_line();
        }
        if self.focused() == Control::Hosts && !self.hosts.is_empty() {
            return self.connect_selected();
        }
        self.connect_best()
    }

    /// The `Connect` button's rule, which is also `Alt+O`'s.
    ///
    /// A typed line wins, because pressing a button moves the focus to that
    /// button and a line that was typed and then
    /// silently ignored is the worse of the two surprises.
    fn connect_best(&mut self) -> DialogOutcome {
        if !self.field.is_empty() {
            return self.connect_line();
        }
        if self.hosts.is_empty() {
            self.error = Some("type a host, or add one to the list".to_string());
            return DialogOutcome::Consumed;
        }
        self.connect_selected()
    }

    /// the quick-connect line.
    fn connect_line(&mut self) -> DialogOutcome {
        let parsed = match url::parse(self.field.text(), self.default, &self.user) {
            Ok(parsed) => parsed,
            Err(why) => {
                // The message never quotes the line back, so a password typed
                // on it cannot reach the error row (the design, S3).
                self.error = Some(why.to_string());
                return DialogOutcome::Consumed;
            }
        };
        let plan = if parsed.target.protocol.verifies_host_key()
            || parsed.target.protocol == Protocol::Smb
        {
            // SMB joins the SSH family here for one reason: the line carries
            // the user, and a guest or anonymous SMB login has no method at
            // all. `for_line` is what knows that; `for_password_login(None)`
            // would put a password prompt in front of a share that needs none.
            AuthPlan::for_line(&parsed, &self.home)
        } else {
            // FTP has no agent and no keys, and a typed line is not a host, so
            // there is nothing to opt in with either.
            AuthPlan::for_password_login(None)
        };
        DialogOutcome::Accept(DialogResult::Connect(Box::new(ConnectAnswer {
            target: parsed.target,
            plan,
            password: parsed.password,
            local_dir: None,
            hosts: self.edited(),
        })))
    }

    /// the saved-host list.
    fn connect_selected(&mut self) -> DialogOutcome {
        let Some(host) = self.hosts.get(self.cursor).cloned() else {
            self.error = Some("there are no saved hosts yet".to_string());
            return DialogOutcome::Consumed;
        };
        if let Some(why) = host.problem() {
            self.error = Some(format!("{}: {why}", host.label));
            return DialogOutcome::Consumed;
        }
        let plan = if host.protocol.verifies_host_key() {
            AuthPlan::for_host(&host, &self.home)
        } else {
            AuthPlan::for_password_login(Some(&host))
        };
        let target: Target = host.target();
        DialogOutcome::Accept(DialogResult::Connect(Box::new(ConnectAnswer {
            target,
            plan,
            // Never from a saved host: the design keeps a stored password in
            // the keyring, reached through `Method::Stored`, and never here.
            password: None,
            local_dir: host.local_path(&self.home),
            hosts: self.edited(),
        })))
    }

    /// Open the Add-host form, on a copy of the selected host for `F4`.
    fn open_form(&mut self, editing: Option<usize>) -> DialogOutcome {
        if editing.is_some() && self.hosts.is_empty() {
            self.error = Some("there are no saved hosts yet".to_string());
            return DialogOutcome::Consumed;
        }
        self.error = None;
        DialogOutcome::Push(Box::new(HostFormDialog::new(self.hosts.clone(), editing)))
    }

    /// `F8`: ask, naming the label.
    fn ask_delete(&mut self) -> DialogOutcome {
        if self.hosts.is_empty() {
            self.error = Some("there are no saved hosts yet".to_string());
            return DialogOutcome::Consumed;
        }
        self.error = None;
        self.confirm_delete = Some(self.cursor);
        // The safe button, exactly as `ConfirmDialog` opens.
        self.confirm_yes = false;
        DialogOutcome::Consumed
    }

    /// The whole of the confirmation's key handling, which replaces the
    /// dialog's own while it is on the screen.
    ///
    /// `Alt+D` and `Alt+N` keep the meanings [`MNEMONICS`] gives them -
    /// `Delete` and `Cancel` - which is why the confirmation needs no letters
    /// of its own and is not a second screen for the census.
    fn handle_confirm(&mut self, key: &DialogKey) -> DialogOutcome {
        if let Some(letter) = key.mnemonic() {
            match letter {
                'd' => self.delete_confirmed(),
                'n' => self.confirm_delete = None,
                _ => {}
            }
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            self.confirm_delete = None;
            return DialogOutcome::Consumed;
        }
        if key.is_accept() {
            if self.confirm_yes {
                self.delete_confirmed();
            } else {
                self.confirm_delete = None;
            }
            return DialogOutcome::Consumed;
        }
        match key.press.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.confirm_yes = !self.confirm_yes;
            }
            KeyCode::Char('y' | 'Y') => self.delete_confirmed(),
            KeyCode::Char('n' | 'N') => self.confirm_delete = None,
            _ => {}
        }
        // a modal question consumes everything until it is
        // answered.
        DialogOutcome::Consumed
    }

    /// The rows the host list gets in an interior of this height.
    fn list_height(area: Rect) -> u16 {
        area.height.saturating_sub(CHROME_ROWS).max(1)
    }

    /// The window of hosts visible in `height` rows: the page the cursor is
    /// on, so moving within a page does not scroll.
    fn window(&self, height: usize) -> std::ops::Range<usize> {
        if height == 0 || self.hosts.is_empty() {
            return 0..0;
        }
        let start = self
            .cursor
            .saturating_div(height)
            .saturating_mul(height)
            .min(self.hosts.len().saturating_sub(1));
        start..start.saturating_add(height).min(self.hosts.len())
    }

    /// One row of the list: `nas          sftp://thorin@nas.local:2222/srv`.
    fn list_row(host: &SavedHost, width: usize, ascii: bool) -> String {
        let label = text::fit_left(&host.label, LABEL_COLUMN, text::Crop::End, ellipsis(ascii));
        let summary = host.summary();
        text::truncate(
            &format!("{label} {summary}"),
            width,
            text::Crop::End,
            ellipsis(ascii),
        )
    }
}

impl Accelerated for ConnectDialog {
    type Control = Control;

    fn mnemonics(&self) -> &'static [(Control, char)] {
        MNEMONICS
    }

    fn accel(&self, control: Control) -> Accel<Control> {
        match control {
            Control::Quick | Control::Hosts => Accel::Focus,
            // Always a button, even with an empty host book: an `Accel::Absent`
            // here would take `e` and `d` off the dialog's letters for as long
            // as the list is empty, which is a second screen for the design's
            // census in exchange for nothing the error row does not already
            // say.
            Control::Add | Control::Edit | Control::Delete | Control::Connect | Control::Cancel => {
                Accel::Press
            }
        }
    }

    fn focus_control(&mut self, control: Control) {
        if let Some(at) = CONTROLS.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// Nothing here is a checkbox, so the "never turns anything
    /// off" has nothing to do.
    fn switch_on(&mut self, control: Control) {
        let _ = control;
    }

    fn press(&mut self, control: Control) -> DialogOutcome {
        match control {
            Control::Add => self.open_form(None),
            Control::Edit => self.open_form(Some(self.cursor)),
            Control::Delete => self.ask_delete(),
            Control::Connect => self.connect_best(),
            Control::Cancel => match self.edited() {
                // The edits still have to reach `hosts.toml`: `F4` and `F8`
                // changed a list, and giving up on *connecting* is not giving
                // up on that.
                Some(hosts) => DialogOutcome::Accept(DialogResult::Hosts(hosts)),
                None => DialogOutcome::Cancel,
            },
            Control::Quick | Control::Hosts => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for ConnectDialog {
    fn id(&self) -> DialogId {
        DialogId::Connect
    }

    fn title(&self) -> String {
        "Connect".to_string()
    }

    fn size_hint(&self) -> (u16, u16) {
        (68, LIST_ROWS.saturating_add(CHROME_ROWS).saturating_add(2))
    }

    /// All seven.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics()
            .iter()
            .filter(|(control, _)| match self.accel(*control) {
                Accel::Absent => false,
                Accel::Focus | Accel::Check | Accel::Gate(_) | Accel::Press => true,
            })
            .map(|(_, letter)| *letter)
            .collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // The `F8` confirmation owns every key while it is up, mnemonics
        // included.
        if self.confirm_delete.is_some() {
            return self.handle_confirm(key);
        }
        // before anything that reads `key.action` and before
        // either the field or the quick search can see the key.
        //
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        // the design puts `F4` and `F8` on the list, and the button labels
        // say so, so they work from anywhere in the dialog.
        match key.press.code {
            KeyCode::F(4) => return self.open_form(Some(self.cursor)),
            KeyCode::F(8) => return self.ask_delete(),
            _ => {}
        }
        if key.is_cancel() {
            // A running quick search takes the first `Esc`, exactly as it does
            // in a panel; the second closes the dialog.
            if self.focused() == Control::Hosts && !self.quick.is_empty() {
                self.quick.clear();
                return DialogOutcome::Consumed;
            }
            return self.press(Control::Cancel);
        }
        if key.is_accept() {
            return match self.focused() {
                Control::Cancel => self.press(Control::Cancel),
                Control::Add => self.open_form(None),
                Control::Edit => self.open_form(Some(self.cursor)),
                Control::Delete => self.ask_delete(),
                Control::Quick | Control::Hosts | Control::Connect => self.accept(),
            };
        }
        if self.focused() == Control::Quick {
            let before = self.field.text().to_string();
            if self.field.handle(key) {
                if before != self.field.text() {
                    self.error = None;
                }
                return DialogOutcome::Consumed;
            }
        }
        if self.focused() == Control::Hosts {
            match key.press.code {
                // At the edges the list gives the key back to the focus ring,
                // which is the only way out of it: `Up` on the top row used to
                // saturate at zero and consume the key, so once the cursor was
                // in the saved hosts there was no route back to the quick
                // connect field short of `Tab` all the way round or `Alt+Q`.
                // A list that traps the cursor is a list nobody can leave.
                KeyCode::Up => {
                    if self.cursor == 0 {
                        self.ring.prev();
                    } else {
                        self.move_cursor(self.cursor.saturating_sub(1));
                    }
                    return DialogOutcome::Consumed;
                }
                KeyCode::Down => {
                    if self.cursor.saturating_add(1) >= self.hosts.len() {
                        self.ring.next();
                    } else {
                        self.move_cursor(self.cursor.saturating_add(1));
                    }
                    return DialogOutcome::Consumed;
                }
                KeyCode::PageUp => {
                    self.move_cursor(self.cursor.saturating_sub(usize::from(LIST_ROWS)));
                    return DialogOutcome::Consumed;
                }
                KeyCode::PageDown => {
                    self.move_cursor(self.cursor.saturating_add(usize::from(LIST_ROWS)));
                    return DialogOutcome::Consumed;
                }
                KeyCode::Home => {
                    self.move_cursor(0);
                    return DialogOutcome::Consumed;
                }
                KeyCode::End => {
                    self.move_cursor(usize::MAX);
                    return DialogOutcome::Consumed;
                }
                KeyCode::Backspace => {
                    self.quick.pop();
                    return DialogOutcome::Consumed;
                }
                _ => {}
            }
            if let Some(ch) = key.text() {
                // A character that matches nothing is swallowed rather than
                // typed, which is what the quick search does.
                self.quick_search(ch);
                return DialogOutcome::Consumed;
            }
        }
        match key.press.code {
            KeyCode::Up | KeyCode::Left => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            KeyCode::Down | KeyCode::Right => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focused();
        if let Some(rect) = row(area, 0) {
            draw_mnemonic(
                f,
                rect,
                QUICK_LABEL,
                'q',
                style.button(focused == Control::Quick),
                style.ascii,
            );
            draw_quick_hint(f, rect, style);
        }
        if let Some(rect) = row(area, 1) {
            self.field.render(f, rect, style);
        }
        if let Some(rect) = row(area, 2) {
            self.draw_hosts_label(f, rect, style, focused == Control::Hosts);
        }

        let height = Self::list_height(area);
        match self.confirm_delete {
            Some(_) => self.draw_confirm(f, area, height, style),
            None => self.draw_list(f, area, height, style, focused == Control::Hosts),
        }

        let after_list = 3u16.saturating_add(height);
        if let Some(rect) = row(area, after_list) {
            let index = match focused {
                Control::Add => 0,
                Control::Edit => 1,
                Control::Delete => 2,
                Control::Quick | Control::Hosts | Control::Connect | Control::Cancel => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                rect,
                &[
                    ("Add host", Some('a')),
                    ("F4 Edit", Some('e')),
                    ("F8 Delete", Some('d')),
                ],
                index,
                style,
            );
        }
        if let Some(rect) = row(area, after_list.saturating_add(1)) {
            // The refusal wins the row: a note about the keyring is worth
            // saying, and a refusal is worth saying first.
            match (self.error.as_deref(), self.note()) {
                (Some(error), _) => draw_text(f, rect, error, style.button(true), style.ascii),
                (None, Some(note)) => draw_text(f, rect, &note, style.body(), style.ascii),
                (None, None) => {}
            }
        }
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last.max(after_list.saturating_add(2))) {
            let index = match focused {
                Control::Connect => 0,
                Control::Cancel => 1,
                Control::Quick
                | Control::Hosts
                | Control::Add
                | Control::Edit
                | Control::Delete => usize::MAX,
            };
            draw_mnemonic_buttons(
                f,
                rect,
                &[("Connect", Some('o')), ("Cancel", Some('n'))],
                index,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        if self.focused() != Control::Quick || self.confirm_delete.is_some() {
            return None;
        }
        self.field.cursor(row(area, 1)?)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl ConnectDialog {
    /// The list's label, with the quick-search fragment right-aligned beside
    /// it - the same string a panel puts in its status line.
    fn draw_hosts_label(&self, f: &mut Frame, rect: Rect, style: &DialogStyle, focused: bool) {
        let search = status_label(&self.quick, self.case);
        let width = usize::from(rect.width);
        let label_width = match search.as_deref() {
            Some(fragment) if text::width(fragment).saturating_add(2) < width => {
                width.saturating_sub(text::width(fragment))
            }
            _ => width,
        };
        let label_rect = Rect::new(
            rect.x,
            rect.y,
            u16::try_from(label_width)
                .unwrap_or(rect.width)
                .min(rect.width),
            1,
        );
        draw_mnemonic(
            f,
            label_rect,
            HOSTS_LABEL,
            's',
            style.button(focused),
            style.ascii,
        );
        if let Some(fragment) = search
            && label_width < width
        {
            let x = rect.x.saturating_add(label_rect.width);
            let rest = rect.right().saturating_sub(x);
            if rest > 0 {
                draw_text(
                    f,
                    Rect::new(x, rect.y, rest, 1),
                    &fragment,
                    style.body(),
                    style.ascii,
                );
            }
        }
    }

    /// The saved-host list, or the sentence that stands in for an empty one.
    fn draw_list(
        &self,
        f: &mut Frame,
        area: Rect,
        height: u16,
        style: &DialogStyle,
        focused: bool,
    ) {
        if self.hosts.is_empty() {
            if let Some(rect) = row(area, 3) {
                draw_text(
                    f,
                    rect,
                    "no saved hosts yet - Add host, or type a line above",
                    style.body(),
                    style.ascii,
                );
            }
            return;
        }
        let window = self.window(usize::from(height));
        for (offset, index) in window.clone().enumerate() {
            let Some(rect) = row(
                area,
                3u16.saturating_add(u16::try_from(offset).unwrap_or(0)),
            ) else {
                break;
            };
            let Some(host) = self.hosts.get(index) else {
                break;
            };
            let text = Self::list_row(host, usize::from(rect.width), style.ascii);
            // The panel's cursor bar, not a button: this is a selected row in
            // a list and it should look like every other selected row in the
            // program.
            let line_style = if index == self.cursor {
                style.row_cursor(focused)
            } else {
                style.body()
            };
            draw_text(f, rect, &text, line_style, style.ascii);
        }
    }

    /// `F8`'s confirmation, drawn over the list and naming the label.
    ///
    fn draw_confirm(&self, f: &mut Frame, area: Rect, height: u16, style: &DialogStyle) {
        let label = self.confirming().unwrap_or("");
        if let Some(rect) = row(area, 3) {
            draw_text(
                f,
                rect,
                &format!("Delete the saved host '{label}'?"),
                style.body(),
                style.ascii,
            );
        }
        if height > 1
            && let Some(rect) = row(area, 4)
        {
            draw_mnemonic_buttons(
                f,
                rect,
                &[("Delete", Some('d')), ("Cancel", Some('n'))],
                usize::from(!self.confirm_yes),
                style,
            );
        }
    }
}

/// Manual, and it **redacts the quick-connect line**
/// (the design, S3).
///
/// [`Field`] derives `Debug` and prints what it holds, and what this one holds
/// can be `thorin:hunter2@nas.local`. Deriving here would put that in every
/// `Debug` of the dialog stack.
impl std::fmt::Debug for ConnectDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectDialog")
            .field("line", &url::redact(self.field.text()))
            .field("hosts", &self.hosts.len())
            .field("cursor", &self.cursor)
            .field("quick", &self.quick)
            .field("dirty", &self.dirty)
            .field("keyring", &self.keyring)
            .finish()
    }
}

// --------------------------------------------------------- the host form ----

/// One focusable control of the Add-host form, in `Tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormControl {
    /// What the list calls this host.
    Label,
    /// The protocol stepper.
    Protocol,
    /// The address.
    Host,
    /// The port. Empty means the protocol's own default.
    Port,
    /// The login name.
    User,
    /// The `auth` stepper: the four values.
    Auth,
    /// The key file, for `auth = key`.
    KeyFile,
    /// A password to put in the keyring, for `auth = keyring`.
    Password,
    /// The initial remote directory.
    RemoteDir,
    /// the initial local directory for the other panel.
    LocalDir,
    /// Save it into the host book.
    Save,
    /// Give up.
    Cancel,
}

/// the letters for the form.
///
/// `Host` takes the `t` of "Host" and `Port` the `r` of "Port" rather than
/// reassigning `h`, which the design reserves for `Help`.
pub const FORM_MNEMONICS: &[(FormControl, char)] = &[
    (FormControl::Label, 'l'),
    (FormControl::Protocol, 'p'),
    (FormControl::Host, 't'),
    (FormControl::Port, 'r'),
    (FormControl::User, 'u'),
    (FormControl::Auth, 'a'),
    (FormControl::KeyFile, 'k'),
    (FormControl::Password, 'w'),
    (FormControl::RemoteDir, 'd'),
    (FormControl::LocalDir, 'i'),
    (FormControl::Save, 'o'),
    (FormControl::Cancel, 'n'),
];

/// Every control, in `Tab` order.
const FORM_CONTROLS: &[FormControl] = &[
    FormControl::Label,
    FormControl::Host,
    FormControl::Port,
    FormControl::User,
    FormControl::Protocol,
    FormControl::Auth,
    FormControl::KeyFile,
    FormControl::Password,
    FormControl::RemoteDir,
    FormControl::LocalDir,
    FormControl::Save,
    FormControl::Cancel,
];

/// The protocols the form steps through, in the order.
pub const PROTOCOLS: &[Protocol] = &[
    Protocol::Sftp,
    Protocol::Ftp,
    Protocol::Ftps,
    Protocol::FtpsImplicit,
    Protocol::Smb,
    Protocol::Dav,
    Protocol::Davs,
    Protocol::S3,
];

/// How wide the label column is. `Initial local directory:` is the longest.
const FORM_LABEL_COLUMN: u16 = 25;

/// The seven text fields' labels, in the order they are drawn, each with the
/// letter the design underlines in it.
///
/// `i` has no word-start occurrence in `Local directory:`, so the label is
/// `Initial local directory:` - which is the own phrase for it
/// ("initial local directory for the other panel") and puts the letter where
/// a reader looks for it.
const FORM_ROWS: &[(FormControl, &str, char)] = &[
    (FormControl::Label, "Label:", 'l'),
    (FormControl::Host, "Host:", 't'),
    (FormControl::Port, "Port:", 'r'),
    (FormControl::User, "Username:", 'u'),
    (FormControl::KeyFile, "Key file:", 'k'),
    (FormControl::Password, "Password:", 'w'),
    (FormControl::RemoteDir, "Remote directory:", 'd'),
    (FormControl::LocalDir, "Initial local directory:", 'i'),
];

/// The row the two steppers share, counted from the top of the interior.
const STEPPER_ROW: u16 = 4;

/// the third way in: the Add-host form.
///
/// `Add host` opens it on a new host and `F4` on a copy of the selected one.
/// It answers with the **whole host book** as it would be after the edit
/// ([`DialogResult::Hosts`]), which is what the connect dialog underneath
/// takes back through [`ConnectDialog::set_hosts`]: one list travels, so the
/// two cannot disagree about what is in the book.
///
/// `Debug` is **written by hand**, because one field here can now hold a
/// secret. Everything else on the form is one of the design's non-secret
/// values; the password is redacted rather than the whole type being
/// unprintable, so a trace of a connect attempt is still readable.
pub struct HostFormDialog {
    hosts: Vec<SavedHost>,
    editing: Option<usize>,
    label: Field,
    host: Field,
    port: Field,
    user: Field,
    key_file: Field,
    /// The password, when one is being put into the keyring.
    ///
    /// A `Field` like the others, and **kept out of `Debug`** by the manual
    /// implementation below: this is the one field on this form that can hold
    /// a secret, and the type's derived `Debug` was the thing that made the
    /// rest of it safe to print.
    password: Field,
    remote_dir: Field,
    local_dir: Field,
    protocol: usize,
    auth: usize,
    error: Option<String>,
    ring: FocusRing,
}

impl std::fmt::Debug for HostFormDialog {
    /// Everything but the password, which is redacted rather than the whole
    /// type being unprintable: a trace of a connect attempt is worth reading,
    /// and the one field that could carry a secret is the one field left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFormDialog")
            .field("hosts", &self.hosts.len())
            .field("editing", &self.editing)
            .field("label", &self.label)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("key_file", &self.key_file)
            .field("password", &"<redacted>")
            .field("remote_dir", &self.remote_dir)
            .field("local_dir", &self.local_dir)
            .field("protocol", &self.protocol)
            .field("auth", &self.auth)
            .field("error", &self.error)
            .finish()
    }
}

impl HostFormDialog {
    /// The password typed into the form, when one was.
    ///
    /// Read once, by the handler that stores it in the keyring, and never
    /// written anywhere else. `hosts.toml` does not carry it: that file is
    /// the design's non-secret half and this is the reason it stays that way
    /// even though the form can now ask.
    #[must_use]
    pub fn typed_password(&self) -> Option<String> {
        let text = self.password.text();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// A form over `hosts`, editing the entry at `editing` or adding a new one.
    ///
    /// An `editing` index that is not in the list is treated as an add, which
    /// is what a list edited underneath the form would otherwise turn into a
    /// panic.
    pub fn new(hosts: Vec<SavedHost>, editing: Option<usize>) -> Self {
        let editing = editing.filter(|i| *i < hosts.len());
        let host = editing
            .and_then(|i| hosts.get(i))
            .cloned()
            .unwrap_or_default();
        let protocol = PROTOCOLS
            .iter()
            .position(|p| *p == host.protocol)
            .unwrap_or(0);
        let auth = AuthMethod::ALL
            .iter()
            .position(|a| *a == host.auth)
            .unwrap_or(0);
        // A new host offers the protocol's default port rather than an empty
        // field, so the commonest edit is no edit.
        let port = if host.port == 0 {
            String::new()
        } else {
            host.port.to_string()
        };
        Self {
            hosts,
            editing,
            label: Field::with_text(host.label),
            password: Field::new(),
            host: Field::with_text(host.host),
            port: Field::with_text(port),
            user: Field::with_text(host.username),
            key_file: Field::with_text(host.key_file),
            remote_dir: Field::with_text(host.remote_dir),
            local_dir: Field::with_text(host.local_dir),
            protocol,
            auth,
            error: None,
            ring: FocusRing::new(FORM_CONTROLS.len()),
        }
    }

    /// Which control has focus.
    pub fn focused(&self) -> FormControl {
        FORM_CONTROLS
            .get(self.ring.index())
            .copied()
            .unwrap_or(FormControl::Label)
    }

    /// The protocol currently chosen.
    pub fn protocol(&self) -> Protocol {
        PROTOCOLS
            .get(self.protocol)
            .copied()
            .unwrap_or(Protocol::Sftp)
    }

    /// The `auth` value currently chosen.
    pub fn auth(&self) -> AuthMethod {
        AuthMethod::ALL
            .get(self.auth)
            .copied()
            .unwrap_or(AuthMethod::Agent)
    }

    /// Whether this form is editing an existing entry rather than adding one.
    pub const fn editing(&self) -> Option<usize> {
        self.editing
    }

    /// The refusal currently shown, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The field a control edits, when it is a field.
    const fn field(&self, control: FormControl) -> Option<&Field> {
        match control {
            FormControl::Label => Some(&self.label),
            FormControl::Host => Some(&self.host),
            FormControl::Port => Some(&self.port),
            FormControl::User => Some(&self.user),
            FormControl::KeyFile => Some(&self.key_file),
            FormControl::Password => Some(&self.password),
            FormControl::RemoteDir => Some(&self.remote_dir),
            FormControl::LocalDir => Some(&self.local_dir),
            FormControl::Protocol | FormControl::Auth | FormControl::Save | FormControl::Cancel => {
                None
            }
        }
    }

    /// The same, mutably.
    const fn field_mut(&mut self, control: FormControl) -> Option<&mut Field> {
        match control {
            FormControl::Label => Some(&mut self.label),
            FormControl::Host => Some(&mut self.host),
            FormControl::Port => Some(&mut self.port),
            FormControl::User => Some(&mut self.user),
            FormControl::KeyFile => Some(&mut self.key_file),
            FormControl::Password => Some(&mut self.password),
            FormControl::RemoteDir => Some(&mut self.remote_dir),
            FormControl::LocalDir => Some(&mut self.local_dir),
            FormControl::Protocol | FormControl::Auth | FormControl::Save | FormControl::Cancel => {
                None
            }
        }
    }

    /// The protocol stepper's own label, which is the string its `p` is
    /// underlined in.
    fn protocol_label(&self) -> String {
        format!("Protocol: < {} >", self.protocol().id())
    }

    /// The `auth` stepper's own label.
    fn auth_label(&self) -> String {
        format!("Auth: < {} >", self.auth().id())
    }

    /// Step a stepper, wrapping: both are short rings of equal choices rather
    /// than a scale with two ends.
    fn step(index: usize, len: usize, forward: bool) -> usize {
        if len == 0 {
            return 0;
        }
        if forward {
            index.saturating_add(1).rem_euclid(len)
        } else {
            index.saturating_add(len).saturating_sub(1).rem_euclid(len)
        }
    }

    /// The host this form describes, or the reason it describes none.
    ///
    /// Validated here rather than at `Save` so a test can ask the question
    /// without pressing a button.
    pub fn to_host(&self) -> std::result::Result<SavedHost, String> {
        let port = self.port.text().trim();
        let port = if port.is_empty() {
            0
        } else {
            port.parse::<u16>()
                .map_err(|_| format!("{port}: not a port number"))?
        };
        let host = SavedHost {
            label: self.label.text().trim().to_string(),
            protocol: self.protocol(),
            host: self.host.text().trim().to_string(),
            port,
            username: self.user.text().trim().to_string(),
            auth: self.auth(),
            key_file: self.key_file.text().trim().to_string(),
            remote_dir: self.remote_dir.text().trim().to_string(),
            local_dir: self.local_dir.text().trim().to_string(),
        };
        if let Some(why) = host.problem() {
            return Err(why);
        }
        // A duplicate label makes the list's quick search ambiguous and the
        // delete confirmation misleading, so it is refused here rather than
        // discovered later.
        if self
            .hosts
            .iter()
            .enumerate()
            .any(|(i, other)| Some(i) != self.editing && other.label == host.label)
        {
            return Err(format!("there is already a host called {}", host.label));
        }
        Ok(host)
    }

    /// The host book as it would be after this edit.
    fn accept(&mut self) -> DialogOutcome {
        let host = match self.to_host() {
            Ok(host) => host,
            Err(why) => {
                self.error = Some(why);
                return DialogOutcome::Consumed;
            }
        };
        let mut hosts = self.hosts.clone();
        match self.editing {
            Some(index) => match hosts.get_mut(index) {
                Some(slot) => *slot = host,
                // The list changed underneath the form; adding is the answer
                // that loses nothing.
                None => hosts.push(host),
            },
            None => hosts.push(host),
        }
        DialogOutcome::Accept(DialogResult::Hosts(hosts))
    }
}

impl Accelerated for HostFormDialog {
    type Control = FormControl;

    fn mnemonics(&self) -> &'static [(FormControl, char)] {
        FORM_MNEMONICS
    }

    fn accel(&self, control: FormControl) -> Accel<FormControl> {
        match control {
            FormControl::Label
            | FormControl::Host
            | FormControl::Port
            | FormControl::User
            | FormControl::KeyFile
            | FormControl::RemoteDir
            | FormControl::LocalDir
            // The two steppers are focus-only: the design gives no meaning
            // to "accelerate a multi-way control" and both candidate meanings
            // turn something off.
            | FormControl::Protocol
            | FormControl::Password
            | FormControl::Auth => Accel::Focus,
            FormControl::Save | FormControl::Cancel => Accel::Press,
        }
    }

    fn focus_control(&mut self, control: FormControl) {
        if let Some(at) = FORM_CONTROLS.iter().position(|c| *c == control) {
            self.ring.set(at);
        }
    }

    /// Nothing here is a checkbox.
    fn switch_on(&mut self, control: FormControl) {
        let _ = control;
    }

    fn press(&mut self, control: FormControl) -> DialogOutcome {
        match control {
            FormControl::Save => self.accept(),
            FormControl::Cancel => DialogOutcome::Cancel,
            FormControl::Password
            | FormControl::Label
            | FormControl::Protocol
            | FormControl::Host
            | FormControl::Port
            | FormControl::User
            | FormControl::Auth
            | FormControl::KeyFile
            | FormControl::RemoteDir
            | FormControl::LocalDir => DialogOutcome::Consumed,
        }
    }
}

impl Dialog for HostFormDialog {
    fn id(&self) -> DialogId {
        DialogId::HostForm
    }

    fn title(&self) -> String {
        match self.editing {
            Some(_) => "Edit host".to_string(),
            None => "Add host".to_string(),
        }
    }

    fn size_hint(&self) -> (u16, u16) {
        // The field rows, the stepper row they are split around, the error
        // row, the button row, two borders. Counted from `FORM_ROWS` rather
        // than written down, because a row added to that table and not here
        // pushes the buttons off the bottom of the box - which is what
        // happened when the password row arrived.
        let fields = u16::try_from(FORM_ROWS.len()).unwrap_or(8);
        (72, fields.saturating_add(5))
    }

    /// All eleven.
    fn mnemonic_letters(&self) -> Vec<char> {
        self.mnemonics()
            .iter()
            .filter(|(control, _)| match self.accel(*control) {
                Accel::Absent => false,
                Accel::Focus | Accel::Check | Accel::Gate(_) | Accel::Press => true,
            })
            .map(|(_, letter)| *letter)
            .collect()
    }

    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome {
        // before anything that reads `key.action` and before a
        // field can see the key.
        if let Some(outcome) = self.mnemonic_key(key) {
            return outcome;
        }
        if self.ring.handle(key) {
            return DialogOutcome::Consumed;
        }
        if key.is_cancel() {
            return DialogOutcome::Cancel;
        }
        if key.is_accept() {
            return match self.focused() {
                FormControl::Cancel => DialogOutcome::Cancel,
                FormControl::Label
                | FormControl::Protocol
                | FormControl::Host
                | FormControl::Port
                | FormControl::User
                | FormControl::Auth
                | FormControl::KeyFile
                | FormControl::Password
                | FormControl::RemoteDir
                | FormControl::LocalDir
                | FormControl::Save => self.accept(),
            };
        }
        let focused = self.focused();
        if let Some(field) = self.field_mut(focused) {
            let before = field.text().to_string();
            if field.handle(key) {
                if before != self.field(focused).map_or("", Field::text) {
                    self.error = None;
                }
                return DialogOutcome::Consumed;
            }
        }
        match (key.press.code, focused) {
            (KeyCode::Left, FormControl::Protocol) => {
                self.protocol = Self::step(self.protocol, PROTOCOLS.len(), false);
                self.error = None;
                DialogOutcome::Consumed
            }
            (KeyCode::Right, FormControl::Protocol) => {
                self.protocol = Self::step(self.protocol, PROTOCOLS.len(), true);
                self.error = None;
                DialogOutcome::Consumed
            }
            (KeyCode::Left, FormControl::Auth) => {
                self.auth = Self::step(self.auth, AuthMethod::ALL.len(), false);
                self.error = None;
                DialogOutcome::Consumed
            }
            (KeyCode::Right, FormControl::Auth) => {
                self.auth = Self::step(self.auth, AuthMethod::ALL.len(), true);
                self.error = None;
                DialogOutcome::Consumed
            }
            (KeyCode::Up | KeyCode::Left, _) => {
                self.ring.prev();
                DialogOutcome::Consumed
            }
            (KeyCode::Down | KeyCode::Right, _) => {
                self.ring.next();
                DialogOutcome::Consumed
            }
            _ => DialogOutcome::Ignored,
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focused();
        for (index, (control, label, letter)) in FORM_ROWS.iter().enumerate() {
            let index = u16::try_from(index).unwrap_or(0);
            // The steppers sit between `Username` and `Key file`.
            let y = if index < STEPPER_ROW {
                index
            } else {
                index.saturating_add(1)
            };
            let Some(rect) = row(area, y) else { continue };
            let label_width = FORM_LABEL_COLUMN.min(rect.width);
            draw_mnemonic(
                f,
                Rect::new(rect.x, rect.y, label_width, 1),
                label,
                *letter,
                style.button(focused == *control),
                style.ascii,
            );
            let x = rect.x.saturating_add(label_width);
            let width = rect.right().saturating_sub(x);
            if width > 0
                && let Some(field) = self.field(*control)
            {
                let cell = Rect::new(x, rect.y, width, 1);
                if *control == FormControl::Password {
                    // Drawn as marks, never as itself. The field still holds
                    // the text - it has to, to be edited and then stored - and
                    // this is the only place it would otherwise appear.
                    let marks = "*".repeat(field.text().chars().count().min(usize::from(width)));
                    draw_text(f, cell, &marks, style.input(), style.ascii);
                } else {
                    field.render(f, cell, style);
                }
            }
        }
        if let Some(rect) = row(area, STEPPER_ROW) {
            // Two pieces and not one string: a mnemonic is scoped to its own
            // control's label, and as one string
            // `Auth`'s `a` would be found inside `Protocol` first.
            let pieces = [
                Piece::new(
                    self.protocol_label(),
                    Some('p'),
                    style.button(focused == FormControl::Protocol),
                    focused == FormControl::Protocol,
                ),
                Piece::new(
                    self.auth_label(),
                    Some('a'),
                    style.button(focused == FormControl::Auth),
                    focused == FormControl::Auth,
                ),
            ];
            draw_mnemonic_pieces(f, rect, &pieces, style.body());
        }
        let rows = u16::try_from(FORM_ROWS.len())
            .unwrap_or(0)
            .saturating_add(1);
        if let Some(rect) = row(area, rows)
            && let Some(error) = self.error.as_deref()
        {
            draw_text(f, rect, error, style.button(true), style.ascii);
        }
        let last = area.height.saturating_sub(1);
        if let Some(rect) = row(area, last.max(rows.saturating_add(1))) {
            let index = match focused {
                FormControl::Save => 0,
                FormControl::Cancel => 1,
                FormControl::Label
                | FormControl::Protocol
                | FormControl::Host
                | FormControl::Port
                | FormControl::User
                | FormControl::Auth
                | FormControl::KeyFile
                | FormControl::Password
                | FormControl::RemoteDir
                | FormControl::LocalDir => usize::MAX,
            };
            // `OK` and not `Save`: the design underlines the letter in the
            // label.3 gives this button `o`, and
            // the word `Save` has no `o` in it to underline.
            draw_mnemonic_buttons(
                f,
                rect,
                &[("OK", Some('o')), ("Cancel", Some('n'))],
                index,
                style,
            );
        }
    }

    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let focused = self.focused();
        let field = self.field(focused)?;
        let index = FORM_ROWS.iter().position(|(c, _, _)| *c == focused)?;
        let index = u16::try_from(index).unwrap_or(0);
        let y = if index < STEPPER_ROW {
            index
        } else {
            index.saturating_add(1)
        };
        let rect = row(area, y)?;
        let label_width = FORM_LABEL_COLUMN.min(rect.width);
        let x = rect.x.saturating_add(label_width);
        let width = rect.right().saturating_sub(x);
        if width == 0 {
            return None;
        }
        field.cursor(Rect::new(x, rect.y, width, 1))
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorDepth, Theme};
    use crate::input::{KeyModifiers, KeyPress};
    use ratatui::style::Modifier;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    fn alt(c: char) -> DialogKey {
        DialogKey::raw(KeyPress::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    fn typed(d: &mut dyn Dialog, text: &str) {
        for ch in text.chars() {
            d.handle_key(&key(KeyCode::Char(ch)));
        }
    }

    /// Every character drawn with [`Modifier::UNDERLINED`], in screen order.
    fn underlined(d: &dyn Dialog, w: u16, h: u16) -> Vec<char> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    // Folded, because a table is written in lower case and a
                    // label is not: `Alt+Q` underlines the `Q` of `Quick`.
                    out.extend(cell.symbol().chars().map(|c| c.to_ascii_lowercase()));
                }
            }
        }
        out
    }

    /// The whole screen as text, for "is this on the screen" assertions.
    fn screen(d: &dyn Dialog, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let style = DialogStyle::new(&Theme::blue(), ColorDepth::TrueColor, false);
        terminal
            .draw(|f| {
                crate::dialog::draw(f, d, f.area(), &style);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                if let Some(cell) = buffer.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn book() -> Vec<SavedHost> {
        vec![
            SavedHost {
                label: "nas".to_string(),
                protocol: Protocol::Sftp,
                host: "nas.local".to_string(),
                port: 2222,
                username: "thorin".to_string(),
                auth: AuthMethod::Agent,
                remote_dir: "/srv/media".to_string(),
                local_dir: "~/Downloads".to_string(),
                ..SavedHost::default()
            },
            SavedHost {
                label: "buildbox".to_string(),
                protocol: Protocol::Sftp,
                host: "buildbox".to_string(),
                port: 22,
                username: "thorin".to_string(),
                ..SavedHost::default()
            },
            SavedHost {
                label: "mirror".to_string(),
                protocol: Protocol::Ftp,
                host: "ftp.example.org".to_string(),
                port: 21,
                username: "anonymous".to_string(),
                auth: AuthMethod::Password,
                ..SavedHost::default()
            },
        ]
    }

    fn dialog() -> ConnectDialog {
        ConnectDialog::new(book(), Protocol::Sftp, "thorin".to_string(), true)
    }

    /// The answer, or a failure naming what came back instead.
    fn answer(outcome: DialogOutcome) -> DialogResult {
        match outcome {
            DialogOutcome::Accept(result) => result,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn the_quick_connect_line_has_focus_when_the_dialog_opens() {
        // "A quick-connect line, focused on open".
        let d = dialog();
        assert_eq!(d.focused(), Control::Quick);
        assert_eq!(d.line(), "");
        assert_eq!(d.cursor(), 0);
    }

    #[test]
    fn enter_on_the_line_connects_to_what_the_line_names() {
        let mut d = dialog();
        typed(&mut d, "thorin@buildbox");
        let DialogResult::Connect(answer) = answer(d.handle_key(&key(KeyCode::Enter))) else {
            panic!("the line should have connected");
        };
        assert_eq!(answer.target.host, "buildbox");
        assert_eq!(answer.target.user, "thorin");
        assert_eq!(answer.target.port, 22, "the protocol's default");
        assert_eq!(
            answer.local_dir, None,
            "a typed line has no local directory"
        );
        assert_eq!(answer.hosts, None, "the book was not edited");
    }

    #[test]
    fn enter_with_an_empty_line_connects_to_the_selected_host() {
        // the line wins only when it has focus
        // and is not empty.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::Tab));
        assert_eq!(d.focused(), Control::Hosts);
        d.handle_key(&key(KeyCode::Down));
        let DialogResult::Connect(answer) = answer(d.handle_key(&key(KeyCode::Enter))) else {
            panic!("the selection should have connected");
        };
        assert_eq!(answer.target.host, "buildbox");
        assert_eq!(answer.password, None, "a saved host carries no password");
    }

    #[test]
    fn a_saved_host_brings_its_initial_local_directory() {
        // the "initial local directory for the other panel",
        // expanded against `$HOME` at use.
        let mut d = dialog();
        // `Alt+O` is the `Connect` button, and it takes the focus with it
        // before it acts.
        let DialogResult::Connect(answer) = answer(d.handle_key(&alt('o'))) else {
            panic!("expected a connection");
        };
        assert_eq!(answer.target.host, "nas.local");
        let local = answer.local_dir.expect("nas has a local directory");
        assert!(local.ends_with("Downloads"), "{local:?}");
        assert!(!local.starts_with("~"), "the tilde was expanded: {local:?}");
    }

    #[test]
    fn typing_in_the_list_is_panel_quick_search_on_the_label() {
        // "quick search on the same typing rules as a panel - typing
        // `na` jumps to `nas.local`."
        let mut d = dialog();
        d.focus_control(Control::Hosts);
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor(), 1);
        typed(&mut d, "mi");
        assert_eq!(d.quick_buffer(), "mi");
        assert_eq!(d.selected().map(|h| h.label.as_str()), Some("mirror"));

        // A character that matches nothing is swallowed, not typed.
        typed(&mut d, "z");
        assert_eq!(d.quick_buffer(), "mi");
        assert_eq!(d.selected().map(|h| h.label.as_str()), Some("mirror"));

        // And the fragment is on the screen, in the panel's own spelling.
        let drawn = screen(&d, 80, 20);
        assert!(drawn.contains("search: mi"), "{drawn}");
    }

    #[test]
    fn esc_ends_a_running_quick_search_before_it_closes_the_dialog() {
        // The panel rule, which is what a user who has just
        // typed into the list expects the first `Esc` to do.
        let mut d = dialog();
        d.focus_control(Control::Hosts);
        typed(&mut d, "mi");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Consumed
        ));
        assert_eq!(d.quick_buffer(), "");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn f8_asks_before_it_deletes_and_the_question_names_the_label() {
        // "F8 deletes it after a confirmation
        // naming the label."
        let mut d = dialog();
        d.focus_control(Control::Hosts);
        d.handle_key(&key(KeyCode::F(8)));
        assert_eq!(d.confirming(), Some("nas"));
        assert_eq!(d.hosts().len(), 3, "nothing has gone yet");
        let drawn = screen(&d, 80, 20);
        assert!(drawn.contains("Delete the saved host 'nas'?"), "{drawn}");

        // Enter takes the focused button, which is the safe one.
        d.handle_key(&key(KeyCode::Enter));
        assert_eq!(d.confirming(), None);
        assert_eq!(d.hosts().len(), 3, "Cancel is the default button");
        assert!(!d.dirty());

        d.handle_key(&key(KeyCode::F(8)));
        d.handle_key(&key(KeyCode::Right));
        d.handle_key(&key(KeyCode::Enter));
        assert_eq!(d.hosts().len(), 2);
        assert_eq!(
            d.hosts().first().map(|h| h.label.as_str()),
            Some("buildbox")
        );
        assert!(d.dirty());
    }

    #[test]
    fn the_delete_confirmation_owns_every_key_until_it_is_answered() {
        // `Alt+D` and `Alt+N` keep the meanings the table gives
        // them, and nothing else does anything.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::F(8)));
        d.handle_key(&alt('a'));
        assert_eq!(d.confirming(), Some("nas"), "Alt+A did not open the form");
        d.handle_key(&alt('n'));
        assert_eq!(d.confirming(), None);
        assert_eq!(d.hosts().len(), 3);

        d.handle_key(&key(KeyCode::F(8)));
        d.handle_key(&alt('d'));
        assert_eq!(d.hosts().len(), 2, "Alt+D confirmed it");
    }

    #[test]
    fn f4_opens_the_form_on_a_copy_of_the_selection() {
        // the third way in, reached from the second.
        let mut d = dialog();
        d.focus_control(Control::Hosts);
        d.handle_key(&key(KeyCode::Down));
        let DialogOutcome::Push(form) = d.handle_key(&key(KeyCode::F(4))) else {
            panic!("F4 pushes the form");
        };
        assert_eq!(form.id(), DialogId::HostForm);
        assert_eq!(form.title(), "Edit host");
        let form = form
            .as_any()
            .and_then(|a| a.downcast_ref::<HostFormDialog>())
            .expect("the form");
        assert_eq!(form.editing(), Some(1));
        assert_eq!(
            form.to_host().expect("valid").host,
            "buildbox",
            "a copy of the selected host"
        );
    }

    #[test]
    fn an_edited_host_book_reaches_the_answer_even_when_the_dialog_is_cancelled() {
        // `F4` and `F8` change a *list*, and
        // giving up on connecting is not giving up on that.
        let mut d = dialog();
        d.handle_key(&key(KeyCode::F(8)));
        d.handle_key(&alt('d'));
        let DialogResult::Hosts(hosts) = answer(d.handle_key(&key(KeyCode::Esc))) else {
            panic!("cancelling a dirty dialog still hands the book back");
        };
        assert_eq!(hosts.len(), 2);

        // And a clean one just cancels.
        let mut clean = dialog();
        assert!(matches!(
            clean.handle_key(&key(KeyCode::Esc)),
            DialogOutcome::Cancel
        ));
    }

    #[test]
    fn a_book_edited_through_the_form_reaches_the_dialog_underneath() {
        let mut d = dialog();
        let mut edited = book();
        edited.push(SavedHost {
            label: "vault".to_string(),
            host: "vault.local".to_string(),
            ..SavedHost::default()
        });
        d.set_hosts(edited);
        assert_eq!(d.hosts().len(), 4);
        assert!(d.dirty());
        let DialogResult::Connect(answer) = answer(d.press(Control::Connect)) else {
            panic!("expected a connection");
        };
        assert_eq!(
            answer.hosts.map(|h| h.len()),
            Some(4),
            "the edit rides along with the connection"
        );
    }

    #[test]
    fn the_connect_button_takes_a_typed_line_over_the_selection() {
        // `Alt+O` focuses the button before it presses it, so the focus rule
        // `Enter` follows cannot be the button's rule as well.
        let mut d = dialog();
        typed(&mut d, "thorin@elsewhere");
        let DialogResult::Connect(typed_line) = answer(d.handle_key(&alt('o'))) else {
            panic!("expected a connection");
        };
        assert_eq!(typed_line.target.host, "elsewhere");

        // And with nothing typed it is the selection, wherever the focus is.
        let mut d = dialog();
        d.focus_control(Control::Connect);
        let DialogResult::Connect(selected) = answer(d.handle_key(&key(KeyCode::Enter))) else {
            panic!("expected a connection");
        };
        assert_eq!(selected.target.host, "nas.local");
    }

    #[test]
    fn a_password_on_the_quick_connect_line_never_reaches_the_debug_output() {
        // the design and S3: the own example is
        // `thorin:pass@192.168.1.10`, and this is the only control in the
        // program that can hold a credential as ordinary text.
        let mut d = dialog();
        typed(&mut d, "thorin:hunter2@192.168.1.10");
        let printed = format!("{d:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("thorin@192.168.1.10"), "{printed}");

        // It does reach the connection, once, and nowhere else.
        let DialogResult::Connect(answer) = answer(d.handle_key(&key(KeyCode::Enter))) else {
            panic!("expected a connection");
        };
        assert!(answer.password.is_some());
        assert!(!format!("{:?}", answer.target).contains("hunter2"));
        assert!(!answer.target.url("/").contains("hunter2"));
    }

    #[test]
    fn a_line_that_will_not_parse_is_refused_in_the_dialog() {
        // the "refused up front", and the message does not quote the
        // line back - which is what keeps a password out of the error row.
        let mut d = dialog();
        typed(&mut d, "telnet://thorin:hunter2@nas.local");
        assert!(matches!(
            d.handle_key(&key(KeyCode::Enter)),
            DialogOutcome::Consumed
        ));
        let why = d.error().expect("a refusal");
        assert!(!why.contains("hunter2"), "{why}");
        assert!(why.contains("telnet"), "{why}");

        // Typing again clears it.
        typed(&mut d, "x");
        assert_eq!(d.error(), None);
    }

    #[test]
    fn a_host_that_wants_a_keyring_this_session_has_not_got_says_so() {
        // "If no keyring is available, say so in the dialog and
        // fall back to prompting every time."
        let mut hosts = book();
        if let Some(host) = hosts.first_mut() {
            host.auth = AuthMethod::Keyring;
        }
        let with = ConnectDialog::new(hosts.clone(), Protocol::Sftp, "thorin".to_string(), true);
        assert_eq!(with.note(), None, "there is a keyring, so nothing to say");

        let without = ConnectDialog::new(hosts, Protocol::Sftp, "thorin".to_string(), false);
        let note = without.note().expect("a note");
        assert!(note.contains("keyring"), "{note}");
        let drawn = screen(&without, 80, 20);
        assert!(drawn.contains("keyring"), "{drawn}");
    }

    #[test]
    fn an_empty_host_book_is_a_sentence_and_not_a_panic() {
        let mut d = ConnectDialog::new(Vec::new(), Protocol::Sftp, "thorin".to_string(), true);
        assert_eq!(d.selected(), None);
        assert_eq!(d.note(), None);
        let drawn = screen(&d, 80, 20);
        assert!(drawn.contains("no saved hosts yet"), "{drawn}");

        // Every button still answers, and none of them moves a cursor that is
        // not there.
        d.focus_control(Control::Hosts);
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::End, KeyCode::Home] {
            d.handle_key(&key(code));
        }
        assert_eq!(d.cursor(), 0);
        assert!(matches!(d.press(Control::Edit), DialogOutcome::Consumed));
        assert_eq!(d.error(), Some("there are no saved hosts yet"));
        assert!(matches!(d.press(Control::Delete), DialogOutcome::Consumed));
        assert_eq!(d.confirming(), None);
        assert!(matches!(d.press(Control::Connect), DialogOutcome::Consumed));
    }

    #[test]
    fn every_letter_the_table_names_is_underlined_on_the_screen() {
        // the underline, read off the buffer rather than trusted.
        //
        let d = dialog();
        let mut drawn = underlined(&d, 90, 24);
        drawn.sort_unstable();
        let mut want: Vec<char> = MNEMONICS.iter().map(|(_, l)| *l).collect();
        want.sort_unstable();
        assert_eq!(drawn, want);
    }

    #[test]
    fn every_letter_of_the_form_is_underlined_on_the_screen() {
        let form = HostFormDialog::new(book(), Some(0));
        let mut drawn = underlined(&form, 90, 24);
        drawn.sort_unstable();
        let mut want: Vec<char> = FORM_MNEMONICS.iter().map(|(_, l)| *l).collect();
        want.sort_unstable();
        assert_eq!(drawn, want);
    }

    #[test]
    fn both_dialogs_lay_themselves_out_against_the_rectangle_they_are_given() {
        // the floor is 60x15, and a dialog is never wider than the
        // terminal.
        let form: HostFormDialog = HostFormDialog::new(book(), None);
        let mut connect = dialog();
        connect.focus_control(Control::Hosts);
        connect.handle_key(&key(KeyCode::F(8)));
        let dialogs: [&dyn Dialog; 3] = [&dialog(), &form, &connect];
        for d in dialogs {
            for (w, h) in [(200u16, 50u16), (80, 24), (60, 15), (30, 6), (4, 3)] {
                let drawn = screen(d, w, h);
                for line in drawn.lines() {
                    assert_eq!(
                        crate::ui::text::width(line),
                        usize::from(w),
                        "{}: {w}x{h}",
                        d.title()
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------- the form ----

    #[test]
    fn the_form_answers_with_the_whole_host_book() {
        // One list travels, so the form and the dialog underneath it cannot
        // disagree about what is in the book.
        let mut form = HostFormDialog::new(book(), None);
        typed(&mut form, "vault");
        form.focus_control(FormControl::Host);
        typed(&mut form, "vault.local");
        let DialogResult::Hosts(hosts) = answer(form.press(FormControl::Save)) else {
            panic!("the form answers with the book");
        };
        assert_eq!(hosts.len(), 4);
        assert_eq!(hosts.last().map(|h| h.label.as_str()), Some("vault"));
        assert_eq!(hosts.last().map(|h| h.port), Some(0), "the default port");
    }

    #[test]
    fn editing_replaces_the_entry_rather_than_appending_one() {
        let mut form = HostFormDialog::new(book(), Some(1));
        assert_eq!(form.title(), "Edit host");
        form.focus_control(FormControl::RemoteDir);
        typed(&mut form, "/srv/build");
        let DialogResult::Hosts(hosts) = answer(form.press(FormControl::Save)) else {
            panic!("the form answers with the book");
        };
        assert_eq!(hosts.len(), 3, "replaced, not appended");
        assert_eq!(
            hosts.get(1).map(|h| h.remote_dir.as_str()),
            Some("/srv/build")
        );
    }

    #[test]
    fn the_form_refuses_a_host_it_could_not_connect_to() {
        let mut form = HostFormDialog::new(Vec::new(), None);
        typed(&mut form, "nowhere");
        assert!(matches!(
            form.press(FormControl::Save),
            DialogOutcome::Consumed
        ));
        assert_eq!(form.error(), Some("a host needs an address"));

        // A port that is not a port.
        form.focus_control(FormControl::Host);
        typed(&mut form, "nas.local");
        form.focus_control(FormControl::Port);
        typed(&mut form, "http");
        assert!(matches!(
            form.press(FormControl::Save),
            DialogOutcome::Consumed
        ));
        assert_eq!(form.error(), Some("http: not a port number"));
    }

    #[test]
    fn a_duplicate_label_is_refused_because_quick_search_could_not_tell_them_apart() {
        let mut form = HostFormDialog::new(book(), None);
        typed(&mut form, "nas");
        form.focus_control(FormControl::Host);
        typed(&mut form, "other.local");
        assert!(matches!(
            form.press(FormControl::Save),
            DialogOutcome::Consumed
        ));
        assert_eq!(form.error(), Some("there is already a host called nas"));

        // Editing an entry is allowed to keep its own label.
        let edit = HostFormDialog::new(book(), Some(0));
        assert!(edit.to_host().is_ok());
    }

    #[test]
    fn the_steppers_walk_spec_16_1s_protocols_and_spec_16_3s_four_methods() {
        let mut form = HostFormDialog::new(Vec::new(), None);
        form.focus_control(FormControl::Protocol);
        let mut seen = vec![form.protocol()];
        for _ in 1..PROTOCOLS.len() {
            form.handle_key(&key(KeyCode::Right));
            seen.push(form.protocol());
        }
        assert_eq!(seen, PROTOCOLS.to_vec());
        form.handle_key(&key(KeyCode::Right));
        assert_eq!(form.protocol(), Protocol::Sftp, "it wraps");

        form.focus_control(FormControl::Auth);
        let mut seen = vec![form.auth()];
        for _ in 1..AuthMethod::ALL.len() {
            form.handle_key(&key(KeyCode::Right));
            seen.push(form.auth());
        }
        assert_eq!(seen, AuthMethod::ALL.to_vec());
        form.handle_key(&key(KeyCode::Left));
        assert_eq!(form.auth(), AuthMethod::Password);
    }

    #[test]
    fn an_out_of_range_edit_index_adds_rather_than_panicking() {
        let form = HostFormDialog::new(book(), Some(99));
        assert_eq!(form.editing(), None);
        assert_eq!(form.title(), "Add host");
    }

    #[test]
    fn a_mnemonic_moves_the_caret_and_types_nothing() {
        // the design, over the form's eleven controls.
        let mut form = HostFormDialog::new(book(), Some(0));
        let before = form.to_host().expect("valid");
        form.handle_key(&alt('k'));
        assert_eq!(form.focused(), FormControl::KeyFile);
        form.handle_key(&alt('t'));
        assert_eq!(form.focused(), FormControl::Host);
        form.handle_key(&alt('p'));
        assert_eq!(form.focused(), FormControl::Protocol);
        assert_eq!(form.to_host().expect("valid"), before, "nothing changed");
    }
    #[test]
    fn up_on_the_first_host_goes_back_to_the_quick_connect_field() {
        // The list used to consume every Up and Down, so `Up` on the top row
        // saturated at zero and there was no way back to the field above it.
        let mut d = dialog();
        assert!(d.hosts.len() >= 2, "the fixture has rows to walk");
        while d.focused() != Control::Hosts {
            d.ring.next();
        }

        // Down through the list, then back up and out of the top.
        d.handle_key(&key(KeyCode::Down));
        assert_eq!(d.cursor, 1);
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(d.cursor, 0, "still in the list");
        d.handle_key(&key(KeyCode::Up));
        assert_eq!(
            d.focused(),
            Control::Quick,
            "the top row hands Up back to the ring"
        );
    }

    #[test]
    fn down_past_the_last_host_leaves_the_list_forwards() {
        let mut d = dialog();
        while d.focused() != Control::Hosts {
            d.ring.next();
        }
        let last = d.hosts.len().saturating_sub(1);
        for _ in 0..last {
            d.handle_key(&key(KeyCode::Down));
        }
        assert_eq!(d.cursor, last, "the last row");
        d.handle_key(&key(KeyCode::Down));
        assert_ne!(d.focused(), Control::Hosts, "and out the bottom");
    }

    #[test]
    fn a_selected_host_wears_the_panels_cursor() {
        // A selected row in a list is the same idea as the cursor bar on a
        // panel, so it takes the same colour. So does a focused button: "the
        // one you are on" is one idea and the program spells it one way. Both
        // used to fill with `dialog.button_focus`, a red that read as a
        // warning about the row rather than as the cursor being on it, and
        // this test used to assert the two were different for that reason.
        let theme = Theme::blue();
        let style = DialogStyle::new(&theme, ColorDepth::TrueColor, false);
        assert_eq!(style.row_cursor(true).bg, Some(style.cursor_bg));
        assert_eq!(
            style.row_cursor(true).bg,
            style.button(true).bg,
            "a selected row and a focused button are the same cursor"
        );
        // And nothing wears the old red as a background any more.
        assert_ne!(style.button(true).bg, Some(style.button_focus));
        // A label is picked out by its foreground alone.
        assert_eq!(style.focus_label(true).fg, Some(style.marked_fg));
        assert_eq!(style.focus_label(true).bg, Some(style.bg));
        assert_eq!(style.row_cursor(false).bg, Some(style.cursor_bg_unfocused));
    }
}
