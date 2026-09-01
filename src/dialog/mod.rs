//! The modal dialog framework.
//!
//! v0.1 reserved two things and put nothing behind them: `Focus::Dialog(id)`,
//! which the design says "consumes all input", and `KeyContext::Dialog`,
//! which the design routes keys through. This module is what sits behind
//! both.
//!
//! # The shape of a dialog
//!
//! ```text
//!            ┌──────────── Create directory ───────────┐
//!            │ New directory name:                     │
//!            │ ┌─────────────────────────────────────┐ │
//!            │ │ photos/2026                         │ │
//!            │ └─────────────────────────────────────┘ │
//!            │            [ OK ]   [ Cancel ]          │
//!            └─────────────────────────────────────────┘
//! ```
//!
//! A [`Dialog`] owns its own state and answers keys; the framework owns the
//! stack, the focus bookkeeping, the border, the centring and the theme.
//!
//! # The rules the framework enforces so no dialog can get them wrong
//!
//! * **A dialog consumes all input**. [`crate::input::dispatch`]
//!   routes every key here while one is open, so nothing leaks to the panel.
//! * **`Esc` cancels, `Enter` accepts.** [`DialogKey::is_cancel`] and
//!   [`DialogKey::is_accept`] answer that once, from the resolved action *and*
//!   the raw key, so a rebound `Esc` still works and an unbound one still
//!   cancels.
//! * **`Tab` and `Shift+Tab` move between controls.** [`FocusRing`] is the
//!   whole of it, and it handles the wrap in both directions.
//! * **A dialog is never wider than the terminal**. [`centred`]
//!   clamps to the area it is given, so a dialog that would like 70 columns
//!   gets 60 at 60x15 rather than drawing off the edge - which is a bug, not a
//!   rounding error.
//! * **The theme's `dialog.*` slots are what colour it**, through
//!   [`DialogStyle`], which quantizes once per frame instead of once per span.
//!
//! # What a dialog does *not* get
//!
//! [`Dialog::handle_key`] is handed a key and nothing else: no `&mut App`, no
//! filesystem, no terminal size. A dialog is built with everything it needs and
//! answers with a [`DialogResult`], which [`crate::input::dialog_accepted`]
//! turns into state changes. That is the same constraint `dispatch` already
//! lives under and it is what keeps dialogs testable
//! without a terminal.

pub mod confirm;
pub mod input;
pub mod message;
pub mod mnemonic;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::config::{ColorDepth, Theme};
use crate::input::{Action, DialogId, KeyCode, KeyModifiers, KeyPress};
use crate::ops::{ConflictChoice, Decision, JobId, JobStatus};

pub use confirm::ConfirmDialog;
pub use input::InputDialog;
pub use message::MessageDialog;
pub use mnemonic::{Accel, Accelerated, control_for, duplicate_letter, letter_of};

/// The narrowest a dialog is ever drawn.
///
/// Below this there is no room for a border, a margin and a single character,
/// so the framework draws nothing rather than a broken box. It is well under
/// the 60-column floor, so this only ever bites on a terminal that is
/// already showing the "terminal too small" message.
pub const MIN_DIALOG_WIDTH: u16 = 12;

/// The shortest a dialog is ever drawn: a border, one row, a border.
pub const MIN_DIALOG_HEIGHT: u16 = 3;

/// How many columns the dialog leaves free on each side at the widest, so a
/// full-width dialog still reads as a dialog rather than as a repaint.
const SIDE_MARGIN: u16 = 2;

/// One key event, as a dialog sees it.
///
/// Carries both the raw [`KeyPress`] and the [`Action`] the keymap resolved it
/// to in [`crate::config::keymap::KeyContext::Dialog`], because a dialog needs
/// both: the action so a user's rebinding is honoured, and the raw key so text
/// still types step 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogKey {
    /// The key, already normalised.
    pub press: KeyPress,
    /// What the `dialog` keymap context resolved it to, if anything.
    pub action: Option<Action>,
}

impl DialogKey {
    /// A key with no binding behind it.
    pub const fn raw(press: KeyPress) -> Self {
        Self {
            press,
            action: None,
        }
    }

    /// `Esc`, or whatever the user bound `clear` to in the `dialog` context.
    pub fn is_cancel(&self) -> bool {
        self.press.code == KeyCode::Esc || self.action == Some(Action::Clear)
    }

    /// `Enter`, or whatever the user bound `open` to in the `dialog` context.
    pub fn is_accept(&self) -> bool {
        self.press.code == KeyCode::Enter || self.action == Some(Action::Open)
    }

    /// `Tab`, or whatever `dialog_next_control` is bound to in the `[dialog]`
    /// context. `Tab` stays a hardcoded fallback the way `Esc` does for cancel,
    /// so the key works even in a keymap that never names the action.
    pub fn is_next_control(&self) -> bool {
        (self.press.code == KeyCode::Tab && !self.press.mods.contains(KeyModifiers::SHIFT))
            || self.action == Some(Action::DialogNextControl)
    }

    /// `Shift+Tab`, or whatever `dialog_prev_control` is bound to. `BackTab` is
    /// normalised to `Shift+Tab` before it arrives; the hardcoded fallback is
    /// kept for the same reason [`Self::is_next_control`] keeps its `Tab`.
    pub fn is_prev_control(&self) -> bool {
        (self.press.code == KeyCode::Tab && self.press.mods.contains(KeyModifiers::SHIFT))
            || self.action == Some(Action::DialogPrevControl)
    }

    /// The character this key types, or `None` when it is not text.
    ///
    /// `None` whenever `Ctrl` or `Alt` is held, so a bound `Ctrl+K` can never
    /// leak into an input field - the same rule the quick-search buffer lives
    /// under.
    pub fn text(&self) -> Option<char> {
        self.press.as_text()
    }

    /// The `Alt` mnemonic this key names, folded to lower case.
    ///
    ///
    /// > **`Alt` with a letter jumps straight to a control.**
    ///
    /// A **letter**, specifically, and never a digit: the design reserves
    /// `Alt` with a digit for the tab strip, "so mnemonics are letters and the
    /// two never collide". `Ctrl` disqualifies the key as well - `Ctrl+Alt+X`
    /// is a different binding, not a sloppier spelling of this one - and
    /// `Shift` does not, because a terminal that reports `Alt+Shift+T` and one
    /// that reports `Alt+T` are describing the same keystroke.
    ///
    /// Folded to lower case so a dialog's table can be written one way. Only
    /// ASCII letters: a mnemonic has to be typeable on the keyboard that has
    /// `Alt` on it, and it has to be underlinable in an ASCII label.
    pub fn mnemonic(&self) -> Option<char> {
        if !self.press.mods.contains(KeyModifiers::ALT)
            || self.press.mods.contains(KeyModifiers::CONTROL)
        {
            return None;
        }
        match self.press.code {
            KeyCode::Char(c) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
            _ => None,
        }
    }
}

/// What a dialog answers with when it is done.
///
/// New dialogs add variants. Everything that matches on this must keep a
/// catch-all arm, because the copy dialog (agent 2c), the conflict dialog and
/// the queue view all land here in this milestone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogResult {
    /// Nothing to carry: a message was acknowledged.
    None,
    /// One line of text: a mask, a directory name, a target path.
    Text(String),
    /// A yes/no answer.
    Confirm(bool),
    /// A conflict resolved. Goes straight to
    /// [`crate::app::App::answer_job`].
    Conflict(Box<Decision>),
    /// Everything the copy/move dialog collects.
    CopyMove(Box<CopyMoveAnswer>),
    /// Everything the `Alt+F5` pack dialog collects.
    Pack(Box<PackAnswer>),
    /// Everything the `Shift+R` resize dialog collects.
    ///
    /// The settings themselves rather than an answer struct of their own:
    /// there is nothing the dialog decides that the job does not read, so a
    /// second shape would only be a copy of the first with a different name.
    Resize(Box<crate::ops::resize::ResizeSettings>),
    /// the Multi-Rename Tool.
    MultiRename(Box<MultiRenameAnswer>),
    /// Everything the Find Files dialog collects.
    Find(Box<FindAnswer>),
    /// the connect dialog.
    Connect(Box<ConnectAnswer>),
    /// the password or passphrase. **Never** [`DialogResult::Text`]:
    /// a secret travelling in `Text` would be one `Debug` away from a log line,
    /// and this enum derives `Debug`.
    Secret(Box<SecretAnswer>),
    /// the host book was edited: the list as the dialog left it.
    Hosts(Vec<crate::remote::hosts::SavedHost>),
}

impl DialogResult {
    /// The text of a [`DialogResult::Text`], for the common case.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text.as_str()),
            _ => None,
        }
    }

    /// Whether a [`DialogResult::Confirm`] said yes.
    pub fn confirmed(&self) -> bool {
        matches!(self, Self::Confirm(true))
    }
}

/// What the connect dialog hands back.
///
/// Defined here beside [`CopyMoveAnswer`] and for the same reason: the dialog,
/// the event loop that starts the connection and the code that writes
/// `hosts.toml` all have to agree on the shape before any of them is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectAnswer {
    /// Where to connect. Secret-free, as every [`crate::remote::Target`] is.
    pub target: crate::remote::Target,
    /// What to try, in the order.
    pub plan: crate::remote::auth::AuthPlan,
    /// Only ever `Some` from a quick-connect line that carried one.
    /// Never from a saved host: a stored
    /// password lives in the keyring and is reached through
    /// [`crate::remote::auth::Method::Stored`].
    pub password: Option<crate::remote::secret::Secret>,
    /// the "initial local directory for the other panel".
    pub local_dir: Option<std::path::PathBuf>,
    /// The host book as the dialog left it, when `F4` or `F8` changed it.
    pub hosts: Option<Vec<crate::remote::hosts::SavedHost>>,
}

/// What the prompt hands back.
///
/// The one answer type in the program that carries a credential. It is boxed
/// into [`DialogResult::Secret`] and goes straight to the connect task that
/// asked for it; nothing else ever holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAnswer {
    /// What was typed. Its `Debug` redacts, which is what makes deriving
    /// `Debug` on this struct and on [`DialogResult`] safe (S1, S3).
    pub secret: crate::remote::secret::Secret,
    /// The "Save in the system keyring" checkbox, offered only where the host
    /// opted in and a keyring exists.
    pub remember: bool,
}

/// What the Find Files dialog hands back.
///
/// Defined here beside [`CopyMoveAnswer`] and for the same reason: the dialog,
/// the event loop that starts the search and the code that writes
/// `searches.toml` all have to agree on the list before any of them is
/// written.
///
/// The history lists the combo boxes are backed by are **not**
/// here: they are derived from `query` by the event loop, through
/// [`crate::search::saved::History::remember`], so a search started any other
/// way is remembered the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindAnswer {
    /// The search to run. Already validated: `Start search` refuses in the
    /// dialog rather than handing back a query that cannot compile.
    pub query: crate::search::query::Query,
    /// The saved-search list as the Load/Save tab left it, or `None` when it
    /// was not touched.
    ///
    /// `Save as…` and `Delete` change a list, not a file: the write reaches
    /// `searches.toml` through the event loop rather than from inside
    /// `handle_key` (the design).
    pub saved: Option<Vec<crate::search::saved::SavedSearch>>,
    /// Which tab was open when `Start search` was pressed, so the next opening
    /// is on the same one. Session state: it
    /// lands on [`crate::search::Session`] and is never written to disk.
    pub tab: usize,
}

/// What the Multi-Rename dialog hands back.
///
/// Four mutually exclusive things the action row can ask for, in one answer
/// rather than four `DialogResult` variants: three of them are buttons on one
/// row and all four carry the settings, which is what makes reopening `Ctrl+M`
/// offer what was last used.
///
/// Defined here beside [`CopyMoveAnswer`] and for the same reason: the dialog,
/// the event loop that queues the job and the undo store all have to agree on
/// the list before any of them is written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MultiRenameAnswer {
    /// `Start!` pressed: the pairs to rename, source order preserved. Empty
    /// for the other three buttons.
    pub pairs: Vec<(crate::vfs::VfsPath, crate::vfs::VfsPath)>,
    /// `Undo` pressed instead (the session undo).
    pub undo: bool,
    /// `Result list` pressed instead.
    pub show_result: bool,
    /// The settings as they stood, so reopening offers them again. Its own
    /// `Default` is `Settings::reset`, so a defaulted answer carries the masks
    /// the design opens on rather than empty ones.
    pub settings: crate::rename::Settings,
}

/// What the `Alt+F5` dialog hands back.
///
/// > `Alt+F5` packs the selection: a dialog for target name, format,
/// > compression level, and "move to archive" (pack then delete sources).
///
/// One field per clause of that sentence. Defined here beside
/// [`CopyMoveAnswer`] and for the same reason: the dialog, the event loop that
/// creates the archive and the job that fills it all have to agree on the list
/// before any of them is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackAnswer {
    /// The archive to create, as typed. Relative to the panel, exactly as the
    /// copy dialog's target is.
    pub target: String,
    /// Which format to write, which is also what the extension says.
    pub format: crate::vfs::archive::format::FormatId,
    /// `0..=9`, rescaled per format by
    /// [`crate::vfs::archive::format::CompressionLevel`].
    pub level: u8,
    /// "Move to archive": pack, then delete the sources - and only if the pack
    /// succeeded, which is `JobKind::Move`'s own promise.
    pub move_sources: bool,
}

/// What the copy/move dialog hands back.
///
/// Defined here rather than beside that dialog so the job engine, the
/// framework and agent 2c's implementation all agree on the field list before
/// any of them is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyMoveAnswer {
    /// The target field, path plus mask: `/srv/media/*.*`.
    pub target: String,
    /// "Only files of this type" - the second mask.
    pub file_mask: String,
    /// The Preserve attributes checkbox.
    pub preserve_attrs: bool,
    /// The Verify checkbox.
    pub verify: bool,
    /// `F2 Queue` was pressed rather than `OK`: append to the background queue
    /// instead of starting now.
    pub queue: bool,
    /// A conflict policy chosen up front, when the dialog offers one.
    pub conflict: Option<ConflictChoice>,
}

impl Default for CopyMoveAnswer {
    fn default() -> Self {
        Self {
            target: String::new(),
            file_mask: "*.*".to_string(),
            preserve_attrs: true,
            verify: false,
            queue: false,
            conflict: None,
        }
    }
}

/// What the framework does after a dialog has seen a key.
pub enum DialogOutcome {
    /// Handled; the dialog stays open.
    Consumed,
    /// Not handled - but still swallowed, because a modal dialog consumes all
    /// input.
    Ignored,
    /// Close and discard (`Esc`).
    Cancel,
    /// Close and act on the result (`Enter`, `OK`).
    Accept(DialogResult),
    /// Act on the result but **stay open**: the queue view's `Del`, which
    /// cancels or drops the selected job and then keeps showing the ones that
    /// remain.
    Act(DialogResult),
    /// Stay open and open another on top: the copy dialog's `+ F7`.
    Push(Box<dyn Dialog>),
    /// Close and open another in its place.
    Replace(Box<dyn Dialog>),
}

impl std::fmt::Debug for DialogOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Consumed => f.write_str("Consumed"),
            Self::Ignored => f.write_str("Ignored"),
            Self::Cancel => f.write_str("Cancel"),
            Self::Accept(result) => write!(f, "Accept({result:?})"),
            Self::Act(result) => write!(f, "Act({result:?})"),
            Self::Push(_) => f.write_str("Push(..)"),
            Self::Replace(_) => f.write_str("Replace(..)"),
        }
    }
}

/// One modal dialog.
///
/// `Send` so that [`crate::app::App`] stays `Send`; nothing sends a dialog
/// anywhere, but a non-`Send` field on `App` would infect the event loop's
/// future for no benefit.
pub trait Dialog: Send {
    /// Which dialog this is. Drives `Focus::Dialog(id)` and lets
    /// [`crate::input::dialog_accepted`] tell the answers apart.
    fn id(&self) -> DialogId;

    /// The title drawn in the border.
    fn title(&self) -> String;

    /// The size the dialog would like, borders included.
    ///
    /// It is a *hint*: [`centred`] clamps it to the area, so asking for 80
    /// columns at 60x15 gets 60. A dialog must therefore lay itself out against
    /// the rectangle it is actually given, never against what it asked for.
    fn size_hint(&self) -> (u16, u16);

    /// Handle one key.
    ///
    /// The framework has already dealt with nothing: `Esc`, `Enter`, `Tab` and
    /// `Shift+Tab` all arrive here, because a dialog with a multi-line text
    /// area or a "type a tab" field has to be able to claim them. Return
    /// [`DialogOutcome::Cancel`] and [`DialogOutcome::Accept`] for the two that
    /// close, and use [`DialogKey::is_cancel`] / [`DialogKey::is_accept`] so
    /// the user's own bindings work.
    fn handle_key(&mut self, key: &DialogKey) -> DialogOutcome;

    /// Lay this dialog out for the size it is about to be drawn at.
    ///
    /// `area` is the interior rectangle [`Dialog::render`] will be given, and
    /// this runs once per frame immediately before the frame is drawn, from
    /// [`crate::ui::sync_dialog_layout`]. It exists because [`Dialog::render`]
    /// takes `&self`: a dialog whose scroll offset genuinely persists between
    /// frames has to write it down somewhere, and the choice is a layout phase
    /// or interior mutability. The panels and the viewer already take the
    /// former, through [`crate::ui::sync_view_rows`] and
    /// [`crate::app::App::set_viewer_view`].
    ///
    /// The default does nothing, which is right for every dialog that fits in
    /// the box it is given. A dialog that does not lay out here still renders
    /// correctly; it only fails to remember where it had scrolled to.
    fn layout(&mut self, area: Rect) {
        let _ = area;
    }

    /// Draw the dialog's interior. `area` is inside the border already.
    fn render(&self, f: &mut Frame, area: Rect, style: &DialogStyle);

    /// Where the hardware cursor goes, in absolute screen coordinates
    /// (it is never hidden).
    ///
    /// `area` is the same interior rectangle [`Dialog::render`] was given.
    /// `None` parks it wherever the framework would otherwise put it.
    fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let _ = area;
        None
    }

    /// Which panel this dialog hangs under, when it is a dropdown rather than
    /// a centred box ("anchored under the target panel's
    /// header, so it is visually obvious which side it will act on").
    ///
    /// `None` - the default, and the answer for every dialog but one - means
    /// the framework centres it, which is what the design made
    /// the framework's job. See the design.
    fn anchor(&self) -> Option<crate::panel::Side> {
        None
    }

    /// Take the live job table, once per frame, before the dialog is drawn.
    ///
    /// The progress dialog and the queue view are **views of jobs**,
    /// so they have to see every [`crate::ops::JobUpdate`]
    /// that lands - otherwise a backgrounded job brought forward would show
    /// the bars it had when it was sent away. This is how the model reaches
    /// them: [`crate::app::App::sync_job_dialogs`] calls it on every frame of
    /// the stack.
    ///
    /// The default does nothing, which is right for every dialog that is not
    /// about a job. It is a push rather than a `&App` borrow because
    /// [`Dialog::handle_key`] deliberately has no access to `App`, and giving
    /// `render` one would undo that.
    fn job_update(&mut self, jobs: &[JobStatus]) {
        let _ = jobs;
    }

    /// This dialog as an [`Any`], when it has a reason to be downcast.
    ///
    /// Exactly one thing needs it and the default `None` is right for
    /// everything else: the `+ F7` opens a prompt **on top of** the
    /// copy dialog, and the answer has to reach back into the dialog
    /// underneath to point its target field at the directory that was just
    /// created. Everything else a dialog says travels out through
    /// [`DialogResult`], which is the direction the framework is built for.
    ///
    /// [`Any`]: std::any::Any
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// [`Dialog::as_any`], mutably.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// The job this dialog is a view of, when it is one.
    ///
    /// [`crate::app::App::sync_job_dialogs`] uses it to tell whether the
    /// progress dialog on the stack is still showing the job that belongs on
    /// screen, which is what makes backgrounding reversible
    /// without the framework having to downcast.
    fn job(&self) -> Option<JobId> {
        None
    }

    /// Every `Alt` mnemonic this dialog offers **right now**,
    /// in no particular order.
    ///
    /// Required, with no default body, deliberately: the design says a
    /// duplicate "is caught by a test over every dialog rather than by
    /// inspection", and a defaulted method would let a new dialog opt out of
    /// that test by saying nothing. A dialog with no mnemonics returns an
    /// empty `Vec`, in one line, and says why in its doc comment - which makes
    /// the empty answer a decision rather than an omission.
    ///
    ///
    /// A `Vec<char>` and not a `&'static [char]` because the answer is
    /// per-instance: the Find Files dialog answers for the open
    /// tab only, the summary dialog's answer depends on whether the job can be
    /// retried, and the conflict dialog's on which of the choices
    /// this conflict offers.
    ///
    /// A dialog that implements [`mnemonic::Accelerated`] writes it in one
    /// line:
    ///
    /// ```ignore
    /// fn mnemonic_letters(&self) -> Vec<char> {
    ///     self.mnemonics().iter().map(|(_, letter)| *letter).collect()
    /// }
    /// ```
    fn mnemonic_letters(&self) -> Vec<char>;
}

/// One entry on the dialog stack.
///
/// `restore` is the focus to go back to when the *whole* stack empties. It is
/// recorded per frame so a dialog that opens another does not lose the panel
/// the user came from.
pub struct DialogFrame {
    /// The dialog itself.
    pub dialog: Box<dyn Dialog>,
    /// Where focus returns when the stack empties.
    pub restore: crate::input::Focus,
}

/// The `dialog.*` theme slots, quantized for this session once.
///
/// Quantizing per frame rather than per span matters: `Theme::quantize` walks
/// the `fallback_16` map on a 16-colour terminal, and a dialog draws dozens of
/// spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogStyle {
    /// `dialog.bg`.
    pub bg: Color,
    /// `dialog.fg`.
    pub fg: Color,
    /// `dialog.title`.
    pub title: Color,
    /// `dialog.border`.
    pub border: Color,
    /// `dialog.button`.
    pub button: Color,
    /// `dialog.button_focus`.
    pub button_focus: Color,
    /// `dialog.input_bg`.
    pub input_bg: Color,
    /// `dialog.input_fg`.
    pub input_fg: Color,
    /// `panel.cursor_bg` / `panel.cursor_fg`, for a **list** inside a dialog.
    ///
    /// A selected row in a list is the same idea as the cursor bar on a panel,
    /// so it is the same colour: a dialog that invented its own would teach a
    /// second visual language for the same thing. Before this, a selected row
    /// borrowed `dialog.button_focus`, which is the red a pressed button uses
    /// and read as an error rather than as a selection.
    pub cursor_bg: Color,
    /// See [`DialogStyle::cursor_bg`].
    pub cursor_fg: Color,
    /// `panel.inactive_cursor_bg` / `_fg`: the same row when its list does not
    /// have focus, exactly as an inactive panel dims its cursor bar.
    pub cursor_bg_unfocused: Color,
    /// See [`DialogStyle::cursor_bg_unfocused`].
    pub cursor_fg_unfocused: Color,
    /// `panel.border`: what marks the thing that has focus, which on a panel
    /// is its frame.
    pub focus: Color,
    /// `panel.marked_fg`: the colour a panel puts on a marked file.
    ///
    /// A label that is picked out is picked out the way a marked file is, by
    /// its foreground alone. Filling a label's background reads as a state the
    /// control is in rather than as a name for it, which is what the red one
    /// did.
    pub marked_fg: Color,
    /// `ui.ascii_borders`.
    pub ascii: bool,
}

impl DialogStyle {
    /// Resolve the slots against a theme and a colour depth.
    pub fn new(theme: &Theme, depth: ColorDepth, ascii: bool) -> Self {
        Self {
            bg: theme.quantize(theme.dialog.bg, depth),
            fg: theme.quantize(theme.dialog.fg, depth),
            title: theme.quantize(theme.dialog.title, depth),
            border: theme.quantize(theme.dialog.border, depth),
            button: theme.quantize(theme.dialog.button, depth),
            button_focus: theme.quantize(theme.dialog.button_focus, depth),
            input_bg: theme.quantize(theme.dialog.input_bg, depth),
            input_fg: theme.quantize(theme.dialog.input_fg, depth),
            cursor_bg: theme.quantize(theme.panel.cursor_bg, depth),
            cursor_fg: theme.quantize(theme.panel.cursor_fg, depth),
            cursor_bg_unfocused: theme.quantize(theme.panel.inactive_cursor_bg, depth),
            cursor_fg_unfocused: theme.quantize(theme.panel.inactive_cursor_fg, depth),
            focus: theme.quantize(theme.panel.border, depth),
            marked_fg: theme.quantize(theme.panel.marked_fg, depth),
            ascii,
        }
    }

    /// Body text on the dialog background.
    pub fn body(&self) -> Style {
        Style::new().fg(self.fg).bg(self.bg)
    }

    /// A text input field.
    pub fn input(&self) -> Style {
        Style::new().fg(self.input_fg).bg(self.input_bg)
    }

    /// A selected row in a list inside a dialog.
    ///
    /// The panel's cursor bar, dimmed when the list does not have focus, which
    /// is what an inactive panel does with its own.
    pub fn row_cursor(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(self.cursor_fg).bg(self.cursor_bg)
        } else {
            Style::new()
                .fg(self.cursor_fg_unfocused)
                .bg(self.cursor_bg_unfocused)
        }
    }

    /// The label of whatever has focus.
    ///
    /// Foreground only, in the colour a panel marks a file with. A label names
    /// a control; it is not the control, so it does not take a filled
    /// background the way the focused control itself does.
    pub fn focus_label(&self, focused: bool) -> Style {
        if focused {
            Style::new()
                .fg(self.marked_fg)
                .bg(self.bg)
                .add_modifier(Modifier::BOLD)
        } else {
            self.body()
        }
    }

    /// A button, focused or not.
    ///
    /// A focused button wears the panel's cursor bar, which is what every
    /// other "this is the one you are on" in the program looks like. It used
    /// to fill with `dialog.button_focus`, a red that reads as a warning about
    /// the button rather than as the cursor being on it.
    pub fn button(&self, focused: bool) -> Style {
        if focused {
            Style::new()
                .fg(self.cursor_fg)
                .bg(self.cursor_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(self.button).bg(self.bg)
        }
    }
}

/// Centre a rectangle of at most `want_w` x `want_h` inside `area`.
///
/// **Never wider or taller than `area`.** the design declares 60x15 usable, and
/// a dialog wider than the terminal is a bug rather than a rounding error, so
/// the clamp is unconditional and the margin is given up before the content is.
pub fn centred(area: Rect, want_w: u16, want_h: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    // The side margin is a nicety; it is the first thing surrendered.
    let widest = area.width.saturating_sub(SIDE_MARGIN.saturating_mul(2));
    let widest = if widest == 0 { area.width } else { widest };

    let w = want_w.max(MIN_DIALOG_WIDTH).min(widest).min(area.width);
    let h = want_h.max(MIN_DIALOG_HEIGHT).min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

/// Place a dialog inside the rectangle it was given, so the renderer and the
/// cursor agree.
///
/// [`centred`] for an ordinary dialog; **top-left within `area`** for one that
/// declares a [`Dialog::anchor`], because `crate::ui::dialog_area` has already
/// narrowed `area` to the rows under that panel's header and a centred box in
/// that rectangle would float in the middle of the panel rather than hang from
/// its top.
pub fn dialog_rect(area: Rect, dialog: &dyn Dialog) -> Rect {
    let (w, h) = dialog.size_hint();
    if dialog.anchor().is_none() {
        return centred(area, w, h);
    }
    // The same clamps [`centred`] applies, so an anchored dialog cannot be
    // narrower than a legible box or wider than the panel it hangs under.
    let widest = area.width.saturating_sub(SIDE_MARGIN.saturating_mul(2));
    let w = w.max(MIN_DIALOG_WIDTH).min(widest.max(1)).min(area.width);
    let h = h.max(MIN_DIALOG_HEIGHT).min(area.height);
    Rect::new(area.x, area.y, w, h)
}

/// The interior rectangle a dialog placed in `area` will be drawn into: the
/// box [`dialog_rect`] chooses, less its one-cell border.
///
/// Zero-sized when nothing legible fits, which is the case [`draw`] declines to
/// draw at all, so a caller that lays a dialog out ahead of the frame is told
/// the same size the frame will use.
pub fn dialog_interior(area: Rect, dialog: &dyn Dialog) -> Rect {
    let rect = dialog_rect(area, dialog);
    if rect.width < MIN_DIALOG_WIDTH || rect.height < MIN_DIALOG_HEIGHT {
        return Rect::new(rect.x, rect.y, 0, 0);
    }
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
}

/// Draw one dialog: the shadow-free box, its border, its title, its interior.
///
/// Returns the interior rectangle, which is also what
/// [`Dialog::render`] and [`Dialog::cursor`] are given.
pub fn draw(f: &mut Frame, dialog: &dyn Dialog, area: Rect, style: &DialogStyle) -> Rect {
    let rect = dialog_rect(area, dialog);
    if rect.width < MIN_DIALOG_WIDTH || rect.height < MIN_DIALOG_HEIGHT {
        // Nothing legible fits. Better an unobstructed panel than a box with
        // no content in it.
        return Rect::new(rect.x, rect.y, 0, 0);
    }

    // The panel underneath must not show through the dialog's own background.
    f.render_widget(Clear, rect);

    // `BorderType::Plain` is box drawing, not ASCII, so `ui.ascii_borders`
    // needs the same `+-|` set the panels use rather than a
    // different `BorderType`. There is one definition of that set, in
    // `ui::text::Glyphs`, and this is it.
    let title = truncate_title(&dialog.title(), rect.width);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(if style.ascii {
            crate::ui::text::Glyphs::new(true).border_set()
        } else {
            BorderType::Rounded.to_border_set()
        })
        .border_style(Style::new().fg(style.border).bg(style.bg))
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(style.title)
                .bg(style.bg)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::new().bg(style.bg));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if inner.width > 0 && inner.height > 0 {
        dialog.render(f, inner, style);
    }
    inner
}

/// A title that always fits between the corners.
fn truncate_title(title: &str, width: u16) -> String {
    // Two corners, two spaces around the title, and at least one border cell
    // either side of it.
    let room = usize::from(width).saturating_sub(6);
    if room == 0 {
        return String::new();
    }
    crate::ui::text::truncate(title, room, crate::ui::text::Crop::End, "\u{2026}")
}

/// Draw one line of body text, cropped to the width it is given.
pub fn draw_text(f: &mut Frame, area: Rect, text: &str, style: Style, ascii: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ellipsis = if ascii { "..." } else { "\u{2026}" };
    // Before the crop, not after: an ASCII spelling can be wider than the
    // character it replaces (`\u{2192}` becomes `->`), so cropping first would
    // overrun the width by exactly as much as it expanded.
    let text = if ascii {
        crate::ui::text::ascii_spelling(text)
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let body = crate::ui::text::truncate(
        &text,
        usize::from(area.width),
        crate::ui::text::Crop::End,
        ellipsis,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(body, style))).style(style),
        area,
    );
}

/// Split `text` at the letter an `Alt` mnemonic underlines.
///
/// Returns `(head, letter, tail)`. `None` when the letter is not in `text` at
/// all, which is what a crop can do to a long label: the label is still drawn,
/// just without an underline, because a mnemonic that is off the right-hand
/// edge of the box is a mnemonic the user cannot read anyway.
///
/// **The first occurrence at a word start, and only failing that the first
/// occurrence anywhere.** A word start is a character at index 0 or preceded
/// by a character that is not ASCII alphanumeric.
///
/// The word-start half exists because the labels this program underlines are
/// sentences and not words: `Alt+M` on
/// `Rename mask: file name` belongs on `mask`, not on the `m` inside
/// `Rename`, and `Alt+I` on `Pack 3 file(s) into` belongs on `into`, not on
/// the `i` inside `file(s)`. The fall-through half is what keeps the letters
/// the Find Files dialog already ships: seven of its twenty-one
/// have no word-start occurrence at all - the `e` of `RegEx`, the `y` of
/// `Only` - and land on the same character they always have.
///
/// Both halves ignore ASCII case, because a table is written in lower case and
/// a label is not.
///
/// **A control's mark is not part of its label.** A ticked checkbox is drawn
/// `[x] Hex` (the design draws its boxes that way) and that `x` is at a
/// word start, so searched as one string it would take the underline off
/// every label whose mnemonic is `x` - `Hex` here, `1x` in the design's
/// multi-rename - and put it on the tick mark, for exactly as long as the box
/// stays ticked. The label is therefore searched first and the whole string
/// only if that fails, so a mark can never win a letter its own label also
/// has.
pub fn split_mnemonic(text: &str, mnemonic: char) -> Option<(&str, &str, &str)> {
    let mark = mark_len(text);
    if mark > 0
        && let Some(found) = split_mnemonic_from(text, mnemonic, mark)
    {
        return Some(found);
    }
    split_mnemonic_from(text, mnemonic, 0)
}

/// The length in bytes of the mark a control draws in front of its label, or
/// `0` when there is none.
///
/// A mark is a bracket, one character, the matching bracket and a space:
/// `[x] `, `[ ] `, `[-] `, `(x) `, `( ) `. That is every mark the tree draws -
/// [`crate::ui::dialog::checkbox`] and the Find dialog's radio and tri-state -
/// and nothing else: a button's `[ Cancel ]` has a letter where the closing
/// bracket would be, and a label that merely opens with a bracket, like
/// multi-rename's `[E]`, has no space after it.
fn mark_len(text: &str) -> usize {
    let mut chars = text.char_indices();
    let Some((_, open)) = chars.next() else {
        return 0;
    };
    let close = match open {
        '[' => ']',
        '(' => ')',
        _ => return 0,
    };
    if chars.next().is_none() {
        return 0;
    }
    if chars.next().map(|(_, ch)| ch) != Some(close) {
        return 0;
    }
    match chars.next() {
        Some((at, ' ')) => at.saturating_add(1),
        _ => 0,
    }
}

/// [`split_mnemonic`] over `text[from..]`, with the offsets it returns still
/// measured from the start of `text`.
///
/// The character at `from` counts as a word start, because it is one: the mark
/// in front of it is not a word.
fn split_mnemonic_from(text: &str, mnemonic: char, from: usize) -> Option<(&str, &str, &str)> {
    let want = mnemonic.to_ascii_lowercase();
    let mut anywhere: Option<(usize, char)> = None;
    let mut previous: Option<char> = None;
    for (offset, ch) in text.get(from..)?.char_indices() {
        let at = offset.saturating_add(from);
        if ch.to_ascii_lowercase() == want {
            if previous.is_none_or(|before| !before.is_ascii_alphanumeric()) {
                return slice_around(text, at, ch);
            }
            if anywhere.is_none() {
                anywhere = Some((at, ch));
            }
        }
        previous = Some(ch);
    }
    let (at, ch) = anywhere?;
    slice_around(text, at, ch)
}

/// `text` split into what is before `ch`, `ch` itself, and what is after it.
///
/// `get` rather than indexing throughout: `at` comes from `char_indices` and
/// is therefore always on a boundary, but a slice that panics on a label is
/// not a trade this program makes.
fn slice_around(text: &str, at: usize, ch: char) -> Option<(&str, &str, &str)> {
    let end = at.saturating_add(ch.len_utf8());
    Some((text.get(..at)?, text.get(at..end)?, text.get(end..)?))
}

/// One line of body text with its `Alt` mnemonic underlined.
///
/// > The letter is shown underlined in that control's label, so the whole set
/// > is readable off the screen rather than memorised.
///
/// The same crop as [`draw_text`], and the underline applied to whatever
/// survives it, so a label narrowed by a small terminal still lines up with
/// every other label on the row.
pub fn draw_mnemonic(
    f: &mut Frame,
    area: Rect,
    text: &str,
    mnemonic: char,
    style: Style,
    ascii: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ellipsis = if ascii { "..." } else { "\u{2026}" };
    let text = if ascii {
        crate::ui::text::ascii_spelling(text)
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let body = crate::ui::text::truncate(
        &text,
        usize::from(area.width),
        crate::ui::text::Crop::End,
        ellipsis,
    );
    let spans = match split_mnemonic(&body, mnemonic) {
        Some((head, letter, tail)) => vec![
            Span::styled(head.to_string(), style),
            Span::styled(letter.to_string(), style.add_modifier(Modifier::UNDERLINED)),
            Span::styled(tail.to_string(), style),
        ],
        None => vec![Span::styled(body, style)],
    };
    f.render_widget(Paragraph::new(Line::from(spans)).style(style), area);
}

/// How many columns [`draw_mnemonic_buttons`] needs to draw all of `labels` on
/// one row.
///
/// The same `[ label ]` decoration and the same two-column separator the
/// renderer uses, in one place, so a dialog that asks "will these fit" and the
/// function that draws them cannot disagree. the four buttons are
/// two columns too wide for a 60-column terminal, and
/// `crate::ui::dialog::ExecuteDialog` puts them on two rows rather than letting
/// `Cancel` fall off the edge.
pub fn mnemonic_buttons_width(labels: &[(&str, Option<char>)]) -> usize {
    let mut width = 0usize;
    for (index, (label, _)) in labels.iter().enumerate() {
        if index > 0 {
            width = width.saturating_add(2);
        }
        width = width.saturating_add(crate::ui::text::width(label).saturating_add(4));
    }
    width
}

/// Draw a row of buttons, centred, with the focused one highlighted.
///
/// Buttons that do not fit are dropped from the right rather than truncated: a
/// half-drawn `[ Canc` is worse than a missing button, and every dialog binds
/// `Esc` to the same thing its Cancel button does.
pub fn draw_buttons(
    f: &mut Frame,
    area: Rect,
    labels: &[&str],
    focused: usize,
    style: &DialogStyle,
) {
    let plain: Vec<(&str, Option<char>)> = labels.iter().map(|label| (*label, None)).collect();
    draw_mnemonic_buttons(f, area, &plain, focused, style);
}

/// [`draw_buttons`], with each button's `Alt` mnemonic underlined in its label.
///
///
/// One implementation and not two: a dialog that grows mnemonics must not also
/// grow a second button row that lays out a cell differently from every other
/// dialog's.
pub fn draw_mnemonic_buttons(
    f: &mut Frame,
    area: Rect,
    labels: &[(&str, Option<char>)],
    focused: usize,
    style: &DialogStyle,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (i, (label, mnemonic)) in labels.iter().enumerate() {
        let text = format!("[ {label} ]");
        let w = crate::ui::text::width(&text);
        if used.saturating_add(w).saturating_add(1) > usize::from(area.width) {
            break;
        }
        if i > 0 {
            spans.push(Span::styled("  ", style.body()));
            used = used.saturating_add(2);
        }
        used = used.saturating_add(w);
        let chosen = style.button(i == focused);
        match mnemonic.and_then(|ch| {
            split_mnemonic(&text, ch).map(|(h, l, t)| (h.to_string(), l.to_string(), t.to_string()))
        }) {
            Some((head, letter, tail)) => {
                spans.push(Span::styled(head, chosen));
                spans.push(Span::styled(
                    letter,
                    chosen.add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(tail, chosen));
            }
            None => spans.push(Span::styled(text, chosen)),
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .centered()
            .style(style.body()),
        area,
    );
}

/// One labelled control on a row it shares with others, for
/// [`draw_mnemonic_pieces`].
///
/// A struct and not a tuple because of [`Piece::focused`]: a row that does not
/// fit has to know which of its controls the user is on, and a fourth
/// anonymous field would be read wrong the first time somebody added a piece.
#[derive(Debug, Clone)]
pub struct Piece {
    /// What is drawn, mark and all: `[x] Hex`, `Counter:`, `[ Devices ]`.
    pub label: String,
    /// The `Alt` letter underlined in it, or `None` for a piece
    /// that is not a control - a group heading - or whose label has no letter
    /// to underline.
    pub mnemonic: Option<char>,
    /// The style it is drawn in, focus highlight and greying included.
    pub style: Style,
    /// True when this piece is the control the dialog's focus is on.
    ///
    /// It is not inferred from [`Piece::style`], which cannot be compared for
    /// "is this the highlighted one" once greying is in play, and it is not
    /// optional: see [`draw_mnemonic_pieces`], where it is the difference
    /// between a keystroke the user can see the effect of and one they cannot.
    pub focused: bool,
}

impl Piece {
    /// One piece of a shared row.
    pub fn new(
        label: impl Into<String>,
        mnemonic: Option<char>,
        style: Style,
        focused: bool,
    ) -> Self {
        Self {
            label: label.into(),
            mnemonic,
            style,
            focused,
        }
    }
}

/// Draw several labelled controls on one row, each in its own style, each with
/// its own `Alt` mnemonic underlined inside its own label.
///
/// A piece that does not fit is dropped whole rather than truncated, the same
/// rule [`draw_buttons`] keeps: half a checkbox label reads as a different
/// option.
///
/// **The focused piece is never the one dropped.** the design promises 60x15
/// is usable, and at 60 columns the Find Files dialog has 54 to
/// put four checkboxes in and needs 60 for the widest of those rows. Dropping
/// from the right alone left `Hex` and `CP437 (DOS)` off the screen while
/// `Alt+X` and `Alt+P` still ticked them, so the search ran in hex, or over a
/// charset nobody chose, with nothing drawn anywhere to say so. The row is
/// therefore a window that slides just far enough right to keep the focused
/// piece in it, and every accelerator focuses its control before it ticks it
/// ([`mnemonic::Accelerated::mnemonic_key`]), so whatever a keystroke changes
/// is on the screen by the next frame.
///
/// This exists rather than an "underline the nth occurrence" parameter on
/// [`draw_mnemonic`] because **a mnemonic is scoped to its own label piece**.
/// `Case: Unchanged   Counter: start 10` is one
/// row and four controls; searched as one string, the `t` of `start 10` would
/// land in `Counter`. Each piece is searched on its own, so it cannot.
pub fn draw_mnemonic_pieces(f: &mut Frame, area: Rect, pieces: &[Piece], body: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = usize::from(area.width);
    let first = first_piece(pieces, width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (i, piece) in pieces.iter().enumerate().skip(first) {
        let gap = usize::from(i > first) * 2;
        let piece_w = crate::ui::text::width(&piece.label);
        if used.saturating_add(gap).saturating_add(piece_w) > width {
            break;
        }
        if gap > 0 {
            spans.push(Span::styled("  ", body));
            used = used.saturating_add(gap);
        }
        // the mnemonic is underlined in the control's own label,
        // and a checkbox's box is a mark rather than part of that label - see
        // `split_mnemonic`, which is where the two are told apart.
        match piece.mnemonic.and_then(|ch| {
            split_mnemonic(&piece.label, ch)
                .map(|(h, l, t)| (h.to_string(), l.to_string(), t.to_string()))
        }) {
            Some((head, letter, tail)) => {
                spans.push(Span::styled(head, piece.style));
                spans.push(Span::styled(
                    letter,
                    piece.style.add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(tail, piece.style));
            }
            None => spans.push(Span::styled(piece.label.clone(), piece.style)),
        }
        used = used.saturating_add(piece_w);
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(body), area);
}

/// The leftmost piece to draw so that the focused one still fits on the row.
///
/// Zero when nothing on the row has the focus, which is the common case and
/// the layout every dialog was written against: the window only slides when it
/// has to, so a row that fits is drawn exactly where it always was.
fn first_piece(pieces: &[Piece], width: usize) -> usize {
    let Some(focused) = pieces.iter().position(|piece| piece.focused) else {
        return 0;
    };
    let mut first = 0;
    while first < focused
        && pieces
            .get(first..=focused)
            .is_some_and(|window| pieces_width(window) > width)
    {
        first = first.saturating_add(1);
    }
    first
}

/// What a run of pieces costs, the two columns between each pair included.
fn pieces_width(pieces: &[Piece]) -> usize {
    pieces.iter().enumerate().fold(0, |total, (i, piece)| {
        total
            .saturating_add(usize::from(i > 0) * 2)
            .saturating_add(crate::ui::text::width(&piece.label))
    })
}

/// Which control has focus, and the `Tab` / `Shift+Tab` that moves it.
///
/// Wraps in both directions, which is what makes `Shift+Tab` from the first
/// control land on the last rather than doing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusRing {
    count: usize,
    index: usize,
}

impl FocusRing {
    /// A ring over `count` controls, focused on the first.
    ///
    /// A count of zero is allowed and makes every method a no-op, so a dialog
    /// that has not decided on its controls yet still compiles and draws.
    pub const fn new(count: usize) -> Self {
        Self { count, index: 0 }
    }

    /// The focused control.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// How many controls there are.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// True when control `i` has focus.
    pub const fn is(&self, i: usize) -> bool {
        self.count > 0 && self.index == i
    }

    /// Focus a control directly, clamping.
    pub const fn set(&mut self, i: usize) {
        if self.count > 0 {
            self.index = if i >= self.count { self.count - 1 } else { i };
        }
    }

    /// `Tab`.
    pub const fn next(&mut self) {
        if self.count > 0 {
            self.index = (self.index + 1) % self.count;
        }
    }

    /// `Shift+Tab`.
    pub const fn prev(&mut self) {
        if self.count > 0 {
            self.index = (self.index + self.count - 1) % self.count;
        }
    }

    /// Consume `Tab` / `Shift+Tab` if that is what this key is.
    ///
    /// Returns true when the key was consumed, so a dialog's `handle_key`
    /// opens with `if self.ring.handle(key) { return DialogOutcome::Consumed }`.
    pub fn handle(&mut self, key: &DialogKey) -> bool {
        if key.is_next_control() {
            self.next();
            true
        } else if key.is_prev_control() {
            self.prev();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::KeyPress;

    fn key(code: KeyCode) -> DialogKey {
        DialogKey::raw(KeyPress::plain(code))
    }

    #[test]
    fn a_dialog_is_never_wider_than_the_terminal() {
        // the floor. A dialog that wants 100 columns gets 60 at most,
        // and a margin when there is room for one.
        let screen = Rect::new(0, 0, 60, 15);
        let r = centred(screen, 100, 40);
        assert!(r.width <= screen.width, "{r:?}");
        assert!(r.height <= screen.height, "{r:?}");
        assert!(r.x >= screen.x && r.right() <= screen.right(), "{r:?}");
        assert!(r.y >= screen.y && r.bottom() <= screen.bottom(), "{r:?}");

        // And it is centred.
        let r = centred(screen, 20, 5);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 5);
        assert_eq!(r.x, 20);
        assert_eq!(r.y, 5);
    }

    #[test]
    fn centring_survives_a_terminal_smaller_than_the_minimum() {
        for (w, h) in [(0, 0), (1, 1), (4, 2), (11, 3)] {
            let screen = Rect::new(0, 0, w, h);
            let r = centred(screen, 40, 10);
            assert!(r.width <= w, "{w}x{h} gave {r:?}");
            assert!(r.height <= h, "{w}x{h} gave {r:?}");
        }
    }

    #[test]
    fn the_focus_ring_wraps_both_ways() {
        let mut ring = FocusRing::new(3);
        assert!(ring.is(0));
        ring.next();
        assert!(ring.is(1));
        ring.next();
        ring.next();
        assert!(ring.is(0), "Tab wraps forwards");
        ring.prev();
        assert!(ring.is(2), "Shift+Tab wraps backwards");
        ring.set(99);
        assert!(ring.is(2), "set clamps");
    }

    #[test]
    fn an_empty_ring_is_inert_rather_than_a_panic() {
        let mut ring = FocusRing::new(0);
        ring.next();
        ring.prev();
        ring.set(4);
        assert_eq!(ring.index(), 0);
        assert!(!ring.is(0));
    }

    #[test]
    fn tab_and_shift_tab_are_consumed_by_the_ring() {
        let mut ring = FocusRing::new(2);
        assert!(ring.handle(&key(KeyCode::Tab)));
        assert!(ring.is(1));
        let shift_tab = DialogKey::raw(KeyPress::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert!(ring.handle(&shift_tab));
        assert!(ring.is(0));
        assert!(!ring.handle(&key(KeyCode::Char('a'))));
    }

    #[test]
    fn a_rebound_focus_key_moves_focus_and_tab_still_works() {
        // The `[dialog]` context can put `dialog_next_control` on another key,
        // and the ring follows the resolved action - while `Tab` stays a live
        // fallback the way `Esc` does for cancel.
        let mut ring = FocusRing::new(3);
        let next = DialogKey {
            press: KeyPress::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            action: Some(Action::DialogNextControl),
        };
        assert!(ring.handle(&next));
        assert!(ring.is(1));
        let prev = DialogKey {
            press: KeyPress::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            action: Some(Action::DialogPrevControl),
        };
        assert!(ring.handle(&prev));
        assert!(ring.is(0));
        // Tab is untouched by the rebinding.
        assert!(ring.handle(&key(KeyCode::Tab)));
        assert!(ring.is(1));
    }

    #[test]
    fn esc_cancels_and_enter_accepts_however_they_are_bound() {
        assert!(key(KeyCode::Esc).is_cancel());
        assert!(key(KeyCode::Enter).is_accept());
        // A user who rebound the `dialog` context still gets both.
        let rebound = DialogKey {
            press: KeyPress::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            action: Some(Action::Clear),
        };
        assert!(rebound.is_cancel());
        assert!(rebound.text().is_none(), "and it does not also type");
    }

    #[test]
    fn a_title_always_fits_between_the_corners() {
        let title = truncate_title("Rename/Move 3 file(s) to", 20);
        assert!(crate::ui::text::width(&title) <= 14, "{title:?}");
        assert_eq!(truncate_title("anything", 4), "");
    }

    #[test]
    fn a_mnemonic_underlines_the_letter_at_a_word_start() {
        // the table, which is the whole reason the
        // rule is not "the first occurrence anywhere": these are the labels
        // this milestone's tables put a letter in, and the first occurrence is
        // the wrong character in every one of them.
        assert_eq!(
            split_mnemonic("Rename mask: file name", 'm'),
            Some(("Rename ", "m", "ask: file name")),
            "the `m` of `mask`, not the one inside `Rename`"
        );
        assert_eq!(
            split_mnemonic("Pack 3 file(s) into", 'i'),
            Some(("Pack 3 file(s) ", "i", "nto")),
            "the `i` of `into`, not the one inside `file(s)`"
        );
        assert_eq!(
            split_mnemonic("Copy 3 file(s) to", 't'),
            Some(("Copy 3 file(s) ", "t", "o")),
            "the trailing `to` of the title line"
        );
        assert_eq!(
            split_mnemonic("start 10", 't'),
            Some(("s", "t", "art 10")),
            "one piece of a row is searched on its own, so `Counter` is not in it"
        );
    }

    #[test]
    fn a_word_start_is_anything_after_a_non_alphanumeric() {
        // A checkbox's mark carries no letters, so `[x] Whole words only`
        // starts its first word at index 4 and not at index 0.
        assert_eq!(
            split_mnemonic("[x] Whole words only", 'w'),
            Some(("[x] ", "W", "hole words only"))
        );
        // A digit is alphanumeric, so the `x` of `10x` does not start a word
        // and the one after the space does.
        assert_eq!(
            split_mnemonic("10x, x-ray", 'x'),
            Some(("10x, ", "x", "-ray"))
        );
    }

    #[test]
    fn a_letter_with_no_word_start_falls_back_to_the_first_occurrence() {
        // This half is what keeps the twenty-one letters the Find
        // Files dialog already ships: seven of them have no word-start
        // occurrence and must land where they always have.
        //
        assert_eq!(
            split_mnemonic("[ ] RegEx", 'e'),
            Some(("[ ] R", "e", "gEx")),
            "and case-insensitively, so it is not the `E` of `Ex`"
        );
        assert_eq!(
            split_mnemonic("[ ] Only search in selected directories/files", 'y'),
            Some(("[ ] Onl", "y", " search in selected directories/files"))
        );
        assert_eq!(
            split_mnemonic("[ ] Search archives", 'v'),
            Some(("[ ] Search archi", "v", "es"))
        );
        assert_eq!(
            split_mnemonic("Subdirs", 'b'),
            Some(("Su", "b", "dirs")),
            "`Subdirs` has one word and it does not start with `b`"
        );
    }

    #[test]
    fn a_ticked_box_never_takes_the_underline_off_its_own_label() {
        // The regression: a tick mark is the literal character `x`
        // (the design draws its boxes `[x]`), it sits at a word start
        // because `[` is not alphanumeric, and the two labels in the tree
        // whose mnemonic is `x` - the `Hex` and the design's
        // `1x` - both spell theirs mid-word. Searched as one string the
        // underline therefore moved onto the mark the moment the box was
        // ticked and back off it when it was unticked, which is the opposite
        // of "the letter is shown underlined in that control's label".
        //
        assert_eq!(
            split_mnemonic("[x] Hex", 'x'),
            Some(("[x] He", "x", "")),
            "the `x` of `Hex`, not the tick in front of it"
        );
        assert_eq!(split_mnemonic("[ ] Hex", 'x'), Some(("[ ] He", "x", "")));
        assert_eq!(
            split_mnemonic("[x] 1x", 'x'),
            Some(("[x] 1", "x", "")),
            "and the same underline, ticked or not"
        );
        assert_eq!(split_mnemonic("[ ] 1x", 'x'), Some(("[ ] 1", "x", "")));
        // A radio and a tri-state carry marks too, in round brackets and with
        // a `-` for "ignore".
        assert_eq!(
            split_mnemonic("(x) Exact", 'x'),
            Some(("(x) E", "x", "act"))
        );
        assert_eq!(
            split_mnemonic("[-] Executable", 'x'),
            Some(("[-] E", "x", "ecutable"))
        );
        // A mark is a bracket, one character, its closing bracket and a
        // space, and nothing else is one: a button keeps its whole label, and
        // so does a label that merely opens with a bracket.
        assert_eq!(
            split_mnemonic("[ Cancel ]", 'c'),
            Some(("[ ", "C", "ancel ]"))
        );
        assert_eq!(
            split_mnemonic("[E] and [X]", 'x'),
            Some(("[E] and [", "X", "]"))
        );
        // The fall-through: a letter that is only in the mark is still found,
        // because half an underline is better than none.
        assert_eq!(split_mnemonic("[x] 1", 'x'), Some(("[", "x", "] 1")));
    }

    #[test]
    fn a_letter_that_is_not_in_the_label_has_no_underline() {
        // the design ties the underline to the label, so a crop that ate the
        // letter leaves a label with no underline rather than an underline in
        // the wrong place. The key still works: it is read from the keystroke,
        // never from the paint.
        assert_eq!(split_mnemonic("Only files of this ty", 'p'), None);
        assert_eq!(split_mnemonic("", 'a'), None);
        // And a multi-byte label is sliced on a character boundary rather than
        // panicking: `at` comes from `char_indices`.
        assert_eq!(
            split_mnemonic("Gr\u{f6}\u{df}e: mask", 'm'),
            Some(("Gr\u{f6}\u{df}e: ", "m", "ask"))
        );
    }

    /// Every underlined cell of a one-row render of `pieces`, as `(x, char)`.
    fn underlined_pieces(pieces: &[Piece], width: u16) -> Vec<(u16, char)> {
        let backend = ratatui::backend::TestBackend::new(width, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let body = Style::new();
        terminal
            .draw(|f| draw_mnemonic_pieces(f, Rect::new(0, 0, width, 1), pieces, body))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        for x in 0..width {
            let Some(cell) = buffer.cell((x, 0)) else {
                continue;
            };
            if cell.modifier.contains(Modifier::UNDERLINED) {
                out.extend(cell.symbol().chars().map(|ch| (x, ch)));
            }
        }
        out
    }

    #[test]
    fn each_piece_of_a_row_is_searched_on_its_own() {
        // the counter row is four
        // controls on one line. As one string, `Alt+T` for `start 10` would
        // underline the `t` of `Counter`; as four pieces it cannot, because
        // the `t` is looked for in `start 10` and nowhere else.
        let pieces = vec![
            Piece::new("Case: Unchanged", None, Style::new(), false),
            Piece::new("Counter:", None, Style::new(), false),
            Piece::new("start 10", Some('t'), Style::new(), false),
            Piece::new("step 5", Some('p'), Style::new(), false),
        ];
        let painted = underlined_pieces(&pieces, 60);
        // `Case: Unchanged` is 15 wide, then two spaces, `Counter:` is 8, then
        // two more: `start 10` begins at 27 and its `t` is the second cell.
        assert_eq!(painted, vec![(28, 't'), (40, 'p')]);
    }

    #[test]
    fn a_piece_that_does_not_fit_is_dropped_whole() {
        // The same rule `draw_buttons` keeps: half a checkbox label reads as a
        // different option.
        let pieces = vec![
            Piece::new("[ ] RegEx", Some('g'), Style::new(), false),
            Piece::new("[ ] Hex", Some('x'), Style::new(), false),
        ];
        assert_eq!(underlined_pieces(&pieces, 40), vec![(6, 'g'), (17, 'x')]);
        // Two columns short of the second piece, and it is not drawn at all.
        assert_eq!(underlined_pieces(&pieces, 17), vec![(6, 'g')]);
        // A control with no letter draws as plain text (the design:
        // ten of them, each with a reason).
        let plain = vec![Piece::new("[ ] [E]", None, Style::new(), false)];
        assert!(underlined_pieces(&plain, 40).is_empty());
    }

    #[test]
    fn the_focused_piece_is_never_the_one_dropped() {
        // The regression: the design promises 60x15 is usable, and at that
        // width the Find Files dialog has 54 columns for a row
        // that wants 60. Dropping from the right alone left `Hex` off the
        // screen while `Alt+X` still ticked it, so the search silently ran in
        // hex. Every accelerator focuses its control before it changes it
        // (`mnemonic::Accelerated::mnemonic_key`), so keeping the focused
        // piece on the row is what puts the change back on the screen.
        let row = |focused: usize| -> Vec<Piece> {
            [
                ("[ ] Whole words only", 'w'),
                ("[ ] Case sensitive", 'c'),
                ("[ ] RegEx", 'g'),
                ("[x] Hex", 'x'),
            ]
            .iter()
            .enumerate()
            .map(|(i, (label, letter))| {
                Piece::new(*label, Some(*letter), Style::new(), i == focused)
            })
            .collect()
        };
        // 20 + 2 + 18 + 2 + 9 + 2 + 7 is 60, six more than there is room for.
        // With the focus on the first box the row is drawn exactly where it
        // always was and the last box is the one dropped.
        let painted = underlined_pieces(&row(0), 54);
        assert_eq!(painted, vec![(4, 'W'), (26, 'C'), (48, 'g')]);
        // With the focus on `Hex` the window slides right by one piece, and
        // the underline is on the `x` of the label rather than on the tick.
        let painted = underlined_pieces(&row(3), 54);
        assert_eq!(painted, vec![(4, 'C'), (26, 'g'), (37, 'x')]);
    }
}
