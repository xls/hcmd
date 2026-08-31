//! The input model.
//!
//! the design calls normative and says it is the part most likely to be got
//! wrong by analogy with mc. Two behaviours are load-bearing:
//!
//! 1. **Typing navigates.** A printable key with a panel focused goes to the
//!    quick-search buffer, never to the command line - and the *first*
//!    character typed is already part of the buffer, so `tho` lands on
//!    `thorin` with the `t` included. There is no activation key and no
//!    timeout.
//! 2. **The command line is a focus target**, reached with `Left`/`Right` and
//!    left with `Up`/`Down`, with a caret that persists across the round trip.
//!    `Ctrl+Enter` inserts at that caret and never moves focus.
//!
//! [`dispatch`] is the single entry point. It must remain callable with **no
//! terminal attached**: it never touches stdout, never asks the terminal for its
//! size, and never reads the filesystem. A directory read is *requested* - see
//! [`crate::app::App::request_read`] - and serviced by the event loop, so a
//! headless test can push synthetic key events through and assert on state.
//!
//! # The keys that are ambiguous, and how they are resolved
//!
//! | Key | Resolution |
//! |---|---|
//! | `Backspace` | pops the quick-search buffer while one is running; goes to the parent directory otherwise |
//! | `Space` | extends a running quick-search buffer; toggles the mark otherwise |
//! | `Insert` | always marks and moves down, whatever the buffer holds; toggles overwrite on the command line |
//! | `0`-`9` | feed the buffer like any other character, unless `panel.digit_keys = "tabs"` |
//! | `Esc` | clears the buffer; clears the marks when the buffer is already empty |
//! | `Ctrl+W` | closes a tab with a panel focused, kills a word on the command line - by context, not by a special case |
//! | `Ctrl+H` | toggles hidden files under the enhanced keyboard protocol; is *Backspace* without it |
//!
//! # with a live shell, the command line is the shell's
//!
//! From v0.3 the row at the foot of the panel view **is the shell's own input
//! line**, so the twelve line-editing actions stop being implemented here and
//! start being forwarded to the PTY as bytes - see `console_key`, which is the
//! whole of the rule, and [`Action::belongs_to_the_shell`], which is the whole
//! of the list. Everything else is unchanged: `F5` still copies, `Ctrl+Enter`
//! still inserts a filename, and `Up`/`Down` still leave for the panel.
//!
//! [`CommandLine`] is not deleted by this. It is what the command line *is*
//! whenever no shell is running - a headless [`App`], a shell that would not
//! start, a shell that has died - and [`crate::app::App::console_owns_cmdline`]
//! is the single question that decides between the two. Every test in
//! `tests/input_model.rs` drives the headless side of it, unchanged.

pub mod action;
pub mod binding;
pub mod cmdline;
pub mod console;
pub mod dialogs;
pub mod files;
pub mod keyboard;
pub mod panel;
pub mod quicksearch;
pub mod search;
pub mod viewer;

pub use action::{Action, Milestone};
pub use binding::{Binding, KeyPress, parse_key};
pub use cmdline::{CommandLine, shell_quote};
pub use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
pub use dialogs::{dialog_accepted, dialog_answered};
pub use keyboard::Keyboard;
pub use quicksearch::{case_indicator, panel_status, status_label};
pub use viewer::{VIEWER_MOTIONS, viewer_extend, viewer_motion};

pub use crate::config::keymap::{KeyContext, Resolution};

use crate::app::App;
use crate::dialog::{ConfirmDialog, InputDialog};
use crate::error::Result;
use crate::ops::JobKind;
use crate::panel::{ColumnId, Side, SortKey};

/// Which modal dialog has focus.
///
/// The id is what [`dialog_accepted`] switches on, so a new dialog adds a
/// variant here and an arm there. Adding a variant is a source-compatible
/// change inside this crate as long as matches keep a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogId {
    /// `F1` help.
    Help,
    /// The `ui.confirm_exit` prompt.
    ConfirmQuit,
    /// `+` mark by wildcard.
    SelectMask,
    /// `-` unmark by wildcard.
    UnselectMask,
    /// The device picker for one side.
    Drive(Side),
    /// `Ctrl+D`, the hotlist alone, acting on the active panel.
    ///
    Hotlist,
    /// `Ctrl+Shift+D`, the label prompt for the directory being added to the
    /// hotlist.
    HotlistAdd,
    /// `F9`, the menu bar.
    Menu,
    /// `Shift+F10`, the context menu for the entry under the cursor.
    ///
    ContextMenu,
    /// A generic message the user has to acknowledge.
    Message,
    /// `F7` create directory (the `+ F7`).
    Mkdir,
    /// The copy/move dialog's `+ F7`: a directory **for the target**,
    /// which is not the same directory as `F7`'s.
    MkdirForTarget,
    /// How large each part of a `split_file` should be.
    Split,
    /// Naming a symbolic link about to be created.
    Symlink,
    /// Naming a hard link about to be created.
    Hardlink,
    /// The permission bits of the selection, as an octal number.
    Permissions,
    /// The warning before `F4` edits a file over the configured size.
    ConfirmEditLarge,
    /// Naming the checksum file `checksum_create` is about to write. The
    /// extension chooses the digest, so `.sha256` and `.sfv` are the same
    /// question answered two ways.
    Checksum,
    /// `F2` / `Shift+F6`, pure rename.
    Rename,
    /// `Ctrl+G` go to a typed path.
    GotoPath,
    /// `Ctrl+G` **inside the viewer**: a byte offset, accepting `0x` notation.
    /// A different question from [`DialogId::GotoPath`], asked
    /// by the same key in a different context.
    GotoOffset,
    /// `F5` / `F6`, the copy/move dialog.
    CopyMove,
    /// The progress dialog of a running job.
    Progress,
    /// "This file already exists".
    Conflict,
    /// `F8` / `Shift+F8`, the delete confirmation naming the count.
    ///
    ConfirmDelete,
    /// The end-of-batch failure summary, with the option to retry.
    ///
    JobSummary,
    /// The background queue view.
    JobQueue,
    /// `Shift+F4`: the name of the file to create and then edit.
    ///
    EditNew,
    /// `Alt+F5`, the pack dialog.
    Pack,
    /// the warning: a write into a compressed tar big enough that
    /// the rewrite is worth asking about, "with a cancel that is the default
    /// button".
    ConfirmRewrite,
    /// `Ctrl+M`, the Multi-Rename Tool.
    MultiRename,
    /// the result list, which the `Result list` button and the
    /// `rename_result` action both open.
    RenameResult,
    /// `Alt+F7`, the Find Files dialog.
    Find,
    /// The Load/Save tab's "Save as…" name prompt, pushed on top of the Find
    /// dialog the way `+ F7` is pushed on top of the copy dialog.
    SaveSearch,
    /// `Ctrl+F` on a local panel.
    Connect,
    /// The Add-host form, which `Add host` and `F4` both open.
    HostForm,
    /// A password or a passphrase.
    RemoteSecret,
    /// An unknown host key, with its fingerprint.
    HostKey,
    /// A **changed** host key. A message, not a question.
    HostKeyChanged,
    /// `Ctrl+F` on a connected panel.
    ConfirmDisconnect,
    /// the opt-in before a content search crosses a network.
    ConfirmRemoteSearch,
    /// the execute prompt: Execute / Open with... / View / Cancel.
    Execute,
    /// the "Open with..." chooser.
    OpenWith,
    /// `Alt+F8`, command history over the fallback command line's own
    /// list; the design).
    ///
    /// Not in the list of six, which names the dialog in its census without
    /// giving it an id. A `Dialog` has to answer `id()` with something, and
    /// every other candidate already means a different question.
    History,
    /// The theme picker, which previews as its cursor moves.
    Theme,
    /// The binary struct template picker, in the hex viewer.
    Template,
    /// `Shift+F9` in a panel and `F9` in the viewer: what this file is.
    FileSummary,
    /// `Shift+R`: resize and convert the marked images.
    Resize,
}

impl DialogId {
    /// A stable string id, for messages and tests.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::ConfirmQuit => "confirm_quit",
            Self::SelectMask => "select_mask",
            Self::UnselectMask => "unselect_mask",
            Self::Drive(Side::Left) => "drive_left",
            Self::Drive(Side::Right) => "drive_right",
            Self::Hotlist => "hotlist",
            Self::HotlistAdd => "hotlist_add",
            Self::Menu => "menu",
            Self::ContextMenu => "context_menu",
            Self::Message => "message",
            Self::Mkdir => "mkdir",
            Self::MkdirForTarget => "mkdir_for_target",
            Self::Checksum => "checksum",
            Self::ConfirmEditLarge => "confirm_edit_large",
            Self::Split => "split",
            Self::Symlink => "symlink",
            Self::Hardlink => "hardlink",
            Self::Permissions => "permissions",
            Self::Rename => "rename",
            Self::GotoPath => "goto_path",
            Self::GotoOffset => "goto_offset",
            Self::CopyMove => "copy_move",
            Self::Progress => "progress",
            Self::Conflict => "conflict",
            Self::ConfirmDelete => "confirm_delete",
            Self::JobSummary => "job_summary",
            Self::JobQueue => "job_queue",
            Self::EditNew => "edit_new",
            Self::Pack => "pack",
            Self::ConfirmRewrite => "confirm_rewrite",
            Self::MultiRename => "multi_rename",
            Self::RenameResult => "rename_result",
            Self::Find => "find",
            Self::SaveSearch => "save_search",
            Self::Connect => "connect",
            Self::HostForm => "host_form",
            Self::RemoteSecret => "remote_secret",
            Self::HostKey => "host_key",
            Self::HostKeyChanged => "host_key_changed",
            Self::ConfirmDisconnect => "confirm_disconnect",
            Self::ConfirmRemoteSearch => "confirm_remote_search",
            Self::Execute => "execute",
            Self::OpenWith => "open_with",
            Self::History => "history",
            Self::Theme => "theme",
            Self::Template => "template",
            Self::FileSummary => "file_summary",
            Self::Resize => "resize",
        }
    }
}

/// The focus state machine.
///
/// Only `Panel` and `CommandLine` participate in the `Left`/`Right`/`Up`/`Down`
/// fast switching; the rest consume all input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    /// One of the two panels. The default.
    Panel(Side),
    /// The command line.
    CommandLine,
    /// A modal dialog. Consumes all input.
    Dialog(DialogId),
    /// The `F3` viewer. Consumes all input.
    Viewer,
    /// The `Ctrl+O` console. The PTY consumes all input.
    Console,
}

impl Focus {
    /// Which keymap context this focus resolves keys against.
    pub const fn context(&self) -> KeyContext {
        match self {
            Self::Panel(_) => KeyContext::Panel,
            Self::CommandLine => KeyContext::CmdLine,
            Self::Dialog(_) => KeyContext::Dialog,
            Self::Viewer => KeyContext::Viewer,
            Self::Console => KeyContext::Console,
        }
    }

    /// True for the two states that fast-switch between each other.
    pub const fn is_fast_switchable(&self) -> bool {
        matches!(self, Self::Panel(_) | Self::CommandLine)
    }
}

impl Default for Focus {
    fn default() -> Self {
        Self::Panel(Side::Left)
    }
}

/// Handle one key event.
///
/// The whole of the design lands here. Resolution follows: the keymap
/// collapses steps 1-3 (context binding, global binding, built-in default) and
/// [`Resolution::Unbound`] is step 4 - the context's default text handling,
/// which is quick search for a panel and insert-at-the-caret for the command
/// line.
///
/// Returns `Err` only for a genuine failure. An unbound key, an action that is
/// not implemented until a later milestone, and a key that does nothing in the
/// current state are all `Ok(())` with a status-line message where one is warranted - never a
/// panic and never silence (the design scope note).
pub fn dispatch(app: &mut App, key: KeyEvent) -> Result<()> {
    // Under the enhanced protocol a key produces press, repeat and release
    // events. Only the first two act.
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }

    // The status message belongs to the key that produced it and is cleared by
    // the next one; an action below sets a fresh one if it has something to say.
    app.message = None;

    let press = console::resolve_ctrl_h(app, KeyPress::from(key));
    let ctx = app.focus.context();

    // a dialog "consumes all input". Every key goes to it - no
    // chord, no panel default, no fall-through - and the keymap is consulted
    // only for the `dialog` context, which is step 1 for the
    // context that has focus.
    // **Unless a viewer is on top of it.** the `F1` in a dialog
    // opens the reference *over* that dialog, exactly as the viewer's own
    // `Ctrl+G` opens a prompt over the viewer - and focus is the authority in
    // both directions: `push_viewer` sets `Focus::Viewer`, `push_dialog` sets
    // `Focus::Dialog`, and each restores what it displaced. Without this the
    // help page would be drawn and then answer no keys at all.
    if app.dialog_is_open() && app.focus != Focus::Viewer {
        return dialogs::dialog_key(app, press, ctx);
    }

    // the viewer "consumes all input", and the design gives it
    // the whole screen. There is no panel underneath to fall through to, so
    // every key resolves against the `viewer` context and stops there -
    // `F5` does not copy files at a viewer, and `F10` does not quit out from
    // under one, for the same reason neither does at the full-screen console.
    //
    // Below the dialog check, deliberately: the viewer's own `Ctrl+G` is a
    // dialog on top of it, and a dialog owns the keyboard while it is up.
    if matches!(app.focus, Focus::Viewer) {
        return viewer::viewer_key(app, press, ctx);
    }

    // the quick view "is still that panel", `Tab` moves focus
    // into it, "and the viewer's own keys apply". One branch, no new `Focus`
    // variant and no new `KeyContext` - the focus state stays
    // `Focus::Panel(side)`, which is what makes "it is still that panel"
    // literally true.
    if let Focus::Panel(side) = app.focus
        && app.quick_view_side() == Some(side)
    {
        return viewer::quick_view_key(app, press);
    }

    // with a live shell the command line **is** the shell's own
    // input line, and `Ctrl+O` gives the shell the whole screen. Both states
    // route the same way and differ only in how much is intercepted first.
    if app.console_owns_cmdline() && matches!(app.focus, Focus::CommandLine | Focus::Console) {
        return console::console_key(app, press, ctx);
    }

    // A chord in progress consumes the next press whatever it is.
    if let Some(first) = app.keyboard.pending_chord.take() {
        return match app.keymap.resolve_chord(ctx, first, press) {
            Some(action) => run_action(app, action, press),
            None => {
                app.message = Some(format!("{first} {press} is not bound"));
                Ok(())
            }
        };
    }

    match app.keymap.resolve(ctx, press) {
        Resolution::Action(action) => run_action(app, action, press),
        Resolution::ChordPending => {
            app.keyboard.pending_chord = Some(press);
            app.message = Some(format!("{press} …"));
            Ok(())
        }
        Resolution::Unbound => default_text_handling(app, press),
    }
}

/// step 4: nothing is bound, so the context's default text
/// handling gets the key.
fn default_text_handling(app: &mut App, press: KeyPress) -> Result<()> {
    // `as_text` is `None` whenever CONTROL or ALT is held, so a modified key
    // can never leak into the buffer or the line.
    let Some(c) = press.as_text() else {
        return Ok(());
    };
    match app.focus {
        Focus::Panel(_) => panel::panel_printable(app, c),
        Focus::CommandLine => app.cmdline.insert_char(c),
        // Dialogs, the viewer and the console are later milestones and consume
        // their own input.
        Focus::Dialog(_) | Focus::Viewer | Focus::Console => {}
    }
    Ok(())
}

/// `F10` / `Alt+Q` / `Alt+F4`.
///
/// > `ui.confirm_exit` gates a confirmation prompt. Quitting with a transfer in
/// > progress always prompts regardless of that setting, naming what is still
/// > running.
///
/// So there are two prompts and they default differently:
///
/// * **Nothing running**, `ui.confirm_exit` on: a guard against a mis-hit key,
///   and the affirmative is the default button. A stray `F10` on its own no
///   longer quits, which is the whole protection; `F10` then `Enter` still
///   does, which is the muscle memory. the rule about
///   an affirmative default applies where a keystroke the user was *already*
///   pressing would dismiss the prompt, and nothing here is mid-selection.
/// * **A job running**: unconditional, names each job and how far it has got,
///   and defaults to `Cancel`. This one is not routine - there is work to lose,
///   and quitting stops it - so it is worth a deliberate answer.
///
/// Quitting is still a flag ([`crate::app::App::should_quit`]); the event loop
/// is what leaves, and it drops the job channel on the way out, which is how a
/// worker learns to stop.
fn quit_requested(app: &mut App) {
    let running = crate::ops::running_job_lines(app.jobs.rows());
    if running.is_empty() {
        if app.config.ui.confirm_exit {
            let confirm = ConfirmDialog::new(
                DialogId::ConfirmQuit,
                "Quit",
                vec![format!("Quit {}?", crate::BIN_NAME)],
            )
            .with_buttons("Quit", "Cancel")
            .defaulting_to_yes();
            app.push_dialog(Box::new(confirm));
        } else {
            app.should_quit = true;
        }
        return;
    }
    let mut lines = vec!["Still running:".to_string()];
    lines.extend(running);
    lines.push("Quitting stops it where it is.".to_string());
    let confirm =
        ConfirmDialog::new(DialogId::ConfirmQuit, "Quit", lines).with_buttons("Quit", "Cancel");
    app.push_dialog(Box::new(confirm));
}

/// Act on a row of the context menu.
///
/// Every row but two is an action that already has a key, so the menu is a
/// second route to the same code rather than a second copy of it - which is
/// what the "every item is a key that already exists" asks for and
/// what invariant I11 tests.
pub(super) fn run_context_choice(app: &mut App, choice: crate::ui::dialog::ContextChoice) {
    use crate::ui::dialog::ContextChoice;

    match choice {
        ContextChoice::Action(action) => {
            if let Err(err) = run_action(app, action, KeyPress::plain(KeyCode::Null)) {
                app.message = Some(err.to_string());
            }
        }
        // The chooser reads the desktop entry directories, so it is queued for
        // the event loop like every other read.
        ContextChoice::OpenWith => {
            if let Some(path) = app.active_panel().active_tab().current_path() {
                app.request_open(crate::app::OpenRequest {
                    path,
                    never_execute: true,
                    intent: crate::app::OpenIntent::Chooser,
                });
            }
        }
        // A handler is `config.open.handlers[index]` with `{file}`
        // substituted - the same expansion `ops::open::resolve` performs for
        // `Enter`, so the menu row and the key cannot spell one differently.
        ContextChoice::Handler(index) => run_context_handler(app, index),
        ContextChoice::Properties => show_properties(app),
    }
}

/// Run the `index`-th `[[open.handlers]]` entry over the entry under the
/// cursor.
fn run_context_handler(app: &mut App, index: usize) {
    let Some(path) = app.active_panel().active_tab().current_path() else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let Some(local) = path.local_path() else {
        app.message = Some(crate::ops::open::NOT_LOCAL.to_string());
        return;
    };
    let Some(handler) = app.config.open.handlers.get(index) else {
        // The list changed under an open menu - a reload while it was up.
        app.message = Some("that handler is no longer configured".to_string());
        return;
    };
    let argv = crate::ops::open::expand_command(&handler.command, local);
    let Some((program, args)) = argv.split_first() else {
        app.message = Some("this handler has no command; nothing to run".to_string());
        return;
    };
    app.draft.sources.clear();
    app.handoff.external = Some(crate::app::ExternalCommand {
        program: program.clone(),
        args: args.to_vec(),
        cwd: path.parent(),
        follow: Some(app.active_side),
    });
}

/// the `properties`, which no other section of the design defines.
///
/// the design defines it from what the program already has:
/// the path, the size, the mtime and the mode in the panel's **own**
/// formatting, the owner and group, and - for a directory - the size walk's
/// figures, requesting the walk when it has not been done. No new dialog type,
/// no new theme slot and no new key.
/// Queue the file-information dialog for the entry under the cursor.
///
/// The facts are taken from the listing rather than from a fresh `stat`: the
/// panel has already formatted them, and asking again could answer differently
/// from what is on screen. Reading the file's head is the event loop's job -
/// see [`crate::app::fileinfo`] for why it cannot happen here.
fn open_file_info(app: &mut App) {
    let cfg = app.config.panel.clone();
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let entry = entry.clone();
    let Some(path) = tab.current_path() else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let request = crate::app::fileinfo::FileInfoRequest {
        path,
        name: entry.name.clone(),
        size: entry.size,
        attrs: crate::panel::format::attr_text(&entry, cfg.attr_style),
        modified: crate::panel::format::date_text(&entry, &cfg),
        is_dir: entry.is_dir(),
    };
    app.request_file_info(request);
}

/// The same dialog, for the file the viewer already has open.
///
/// The viewer knows its path and its length and nothing else about the entry,
/// so the attribute and date lines are left empty rather than invented. What
/// the dialog is opened for here is the contents.
pub(crate) fn open_viewer_file_info(app: &mut App) {
    let Some(viewer) = app.viewer() else {
        return;
    };
    let Some(path) = viewer.path().cloned() else {
        app.message = Some("this page is not a file".to_string());
        return;
    };
    let request = crate::app::fileinfo::FileInfoRequest {
        name: viewer.title().to_string(),
        size: viewer.len().unwrap_or(0),
        path,
        attrs: String::new(),
        modified: String::new(),
        is_dir: false,
    };
    app.request_file_info(request);
}

fn show_properties(app: &mut App) {
    let cfg = app.config.panel.clone();
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let entry = entry.clone();
    let Some(path) = tab.current_path() else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let mut lines = vec![
        path.to_string(),
        String::new(),
        format!("Size:  {}", crate::panel::format::size_text(&entry, &cfg)),
        format!("Date:  {}", crate::panel::format::date_text(&entry, &cfg)),
        // Both renderings, because `panel.attr_style` picks one for the column
        // and the other is the one the user may actually want to read here.
        format!(
            "Mode:  {}  {}  {}",
            crate::panel::format::attr_text(&entry, crate::config::AttrStyle::Unix),
            crate::panel::format::attr_text(&entry, crate::config::AttrStyle::Dos),
            crate::panel::format::perms_octal_text(&entry),
        ),
        format!(
            "Owner: {} ({})",
            crate::vfs::users::owner_name(entry.uid),
            crate::vfs::users::group_name(entry.gid),
        ),
    ];
    if entry.is_dir() {
        match app.jobs.sizes.get(&path) {
            Some(stats) => lines.push(format!(
                "Tree:  {} in {} files, {} directories",
                crate::ui::volume::human(stats.bytes),
                stats.files,
                stats.dirs,
            )),
            None => {
                // The same walk `Ctrl+L` and `Space` use, so a directory sized
                // here is then free for both.
                app.request_size(vec![path]);
                lines.push("Tree:  calculating...".to_string());
            }
        }
    }
    app.show_message("Properties", lines);
}

/// Which section of the reference `F1` lands on.
///
/// The topic is the context the key was pressed in, which is what makes `F1`
/// "context-sensitive: `F1` in a dialog explains that dialog, `F1` in the
/// viewer explains the viewer, `F1` in the console explains the console".
/// There is still only one document behind it, so quick find works across the
/// whole reference.
fn help_topic(app: &App) -> crate::ui::help::HelpTopic {
    use crate::ui::help::HelpTopic;
    match app.focus {
        Focus::Dialog(id) => HelpTopic::Dialog(id),
        Focus::Viewer => HelpTopic::Viewer,
        Focus::Console => HelpTopic::Console,
        Focus::Panel(_) | Focus::CommandLine => HelpTopic::Keyboard,
    }
}

/// the `Configuration`: open `config.toml` in the editor
/// and reload when it exits.
///
/// the design specifies no control for any of `config.toml`'s keys and
/// the design promises a menu that "opens the settings", so this is the
/// resolution the design records: the file itself, in the
/// editor the user already configured. The reload is armed here so it happens
/// whether the editor saved anything or not - a reload of an unchanged file
/// costs one read and cannot be wrong.
fn open_config_editor(app: &mut App) {
    let dir = match crate::config::paths::config_dir() {
        Ok(dir) => dir,
        Err(err) => {
            app.message = Some(err.to_string());
            return;
        }
    };
    let path = crate::vfs::VfsPath::local(dir.join("config.toml"));
    let caps = path.backend().capabilities();
    match crate::ops::editor::plan(&app.config.editor, &path, caps, None, app.active_side) {
        Ok(command) => {
            app.draft.sources.clear();
            app.handoff.external = Some(command);
            app.reload_config();
        }
        Err(why) => app.message = Some(why.to_string()),
    }
}

/// `Ctrl+Shift+D`: the label prompt for the directory the active panel is in.
///
///
/// Seeded from the last path component and editable, which is what the design
/// asks for. An [`InputDialog`], not a dialog of its own: it asks for a line
/// of text and the census already covers its two letters.
///
fn open_hotlist_add(app: &mut App) {
    let path = app.active_panel().active_tab().path.clone();
    let Some(local) = path.local_path() else {
        app.message =
            Some("the hotlist holds local directories; this panel is not showing one".to_string());
        return;
    };
    let label = crate::devices::hotlist::suggest_label(local);
    // The *path* is not carried in the dialog: it is the active panel's, the
    // dialog is modal, and nothing can move the panel while it is on screen -
    // so `dialog_accepted` reads it back from where it still is.
    app.push_dialog(Box::new(InputDialog::new(
        DialogId::HotlistAdd,
        "Add to the hotlist",
        "Label:",
        &label,
    )));
}

/// `Shift+F10`: the context menu for the entry under the cursor.
///
///
/// Built from `config.open.handlers`, the entry's mode and its **name** - no
/// I/O, because `dispatch` may not read. The MIME is
/// therefore `mime_guess`'s answer from the extension alone; the content-first
/// correction of the design belongs to the event loop's `Enter`, which has
/// read the file's head.
fn open_context_menu(app: &mut App) {
    let tab = app.active_panel().active_tab();
    let Some(entry) = tab.current().filter(|e| !e.is_parent) else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let name = entry.name.clone();
    let mode = entry.mode;
    let marked = tab.marks.len();
    let Some(path) = tab.current_path() else {
        app.message = Some("nothing under the cursor".to_string());
        return;
    };
    let local = path.local_path().is_some();
    let mime = crate::ops::open::mime_of(&name, &[]);
    let items = crate::ui::dialog::ContextMenuDialog::items_for(
        &app.config.open,
        &name,
        &mime,
        mode,
        local,
    );
    let dialog = crate::ui::dialog::ContextMenuDialog::new(name, marked, items)
        .with_keys(&app.keymap, app.keyboard.enhanced);
    app.push_dialog(Box::new(dialog));
}
/// The column a positional sort key means: `Ctrl+<n>` and its secondary twin.
///
/// The n-th configured column, and **past the end of the order it wraps back
/// to the start**: with the default five columns `Ctrl+6` is `Ctrl+1` again,
/// `Ctrl+7` is `Ctrl+2`, `Ctrl+8` is `Ctrl+3` and `Ctrl+9` is `Ctrl+4`.
///
/// Wrapping rather than refusing, because the high keys were reachable and
/// good for nothing: there are nine positional actions and at most eight
/// columns to point them at, a default layout has five, and so `Ctrl+6` to
/// `Ctrl+9` answered "there is no column 6" and did nothing else.
///
/// What they are now is the fallback for the low keys, which is the half of
/// this that matters. `Ctrl+1` to `Ctrl+3` are exactly the ones a terminal is
/// most likely not to deliver: without the Kitty keyboard protocol `Ctrl+1`
/// encodes to nothing at all and `Ctrl+3` encodes to `Escape`, and a terminal
/// emulator that uses `Ctrl+<n>` for its own tabs takes them before this
/// program is asked. The fifth column has no fallback and cannot have one,
/// there being four keys left over rather than five.
fn sort_column(order: &[crate::panel::ColumnId], n: usize) -> Option<crate::panel::ColumnId> {
    if order.is_empty() {
        return None;
    }
    order.get(n.saturating_sub(1) % order.len()).copied()
}

/// Run a resolved action.
///
/// `press` is the key that produced the action - the second key for a chord.
/// Two rules in the design are stated about a *key* rather than about an
/// action and need it: `Backspace` pops the quick-search buffer (while
/// `Ctrl+PgUp`, bound to the same `parent` action, does not), and `Esc` has a
/// meaning of its own on a panel.
pub(crate) fn run_action(app: &mut App, action: Action, press: KeyPress) -> Result<()> {
    use Action as A;

    match action {
        A::Quit => quit_requested(app),

        // ------------------------------------------------------- focus -----
        A::OtherPanel => {
            let next = app.active_side.other();
            app.set_focus(Focus::Panel(next));
        }
        // Left/Right from a panel enter the command line at its remembered
        // caret - the caret is state on the command line, so there is nothing
        // to restore, only something not to disturb.
        A::FocusCmdline => app.set_focus(Focus::CommandLine),
        // Up/Down leave it again, keeping the text and the caret.
        A::LeaveToPanel => {
            // leaving the command line also moves the panel
            // cursor one row, so the key keeps the meaning it has in the panel
            // rather than being spent purely on focus. `Ctrl+Enter` then `Down`
            // is what makes picking consecutive filenames two keys each.
            app.set_focus(Focus::Panel(app.active_side));
            match press.code {
                KeyCode::Up => app.move_cursor(-1),
                KeyCode::Down => app.move_cursor(1),
                // Bound to something else in keymap.toml: hand focus back
                // without inventing a direction for it.
                _ => {}
            }
        }

        // ------------------------------------------------- panel cursor -----
        A::CursorUp => app.move_cursor(-1),
        A::CursorDown => app.move_cursor(1),
        A::CursorPageUp => {
            let rows = app.active_panel().view_rows.max(1);
            app.move_cursor(isize::try_from(rows).unwrap_or(1).saturating_neg());
        }
        A::CursorPageDown => {
            let rows = app.active_panel().view_rows.max(1);
            app.move_cursor(isize::try_from(rows).unwrap_or(1));
        }
        A::CursorTop => app.move_cursor_to(0),
        A::CursorBottom => {
            let last = app
                .active_panel()
                .active_tab()
                .entries
                .len()
                .saturating_sub(1);
            app.move_cursor_to(last);
        }

        // ------------------------------------------------ quick search ------
        A::ClearSearch => panel::clear_search_then_marks(app),
        A::StartQuickSearch => {
            // The buffer starts on any printable key, so this is only ever
            // *needed* under `panel.digit_keys = "tabs"`, where a leading digit
            // would otherwise switch tabs and `2026-budget.xlsx` would be
            // unreachable by its first character. Arming makes
            // the next digit a search rather than a tab; it is harmless - and
            // still shows in the status line - under the default setting.
            app.active_panel_mut().quick.arm();
        }

        // -------------------------------------------------- navigation ------
        A::Parent => {
            // `Backspace` pops the buffer when one is running and only then
            // goes up. The rule is about that key: `Ctrl+PgUp` is bound to the
            // same action and always goes up.
            if press.code == KeyCode::Backspace && app.active_panel_mut().quick.pop() {
                panel::rematch(app);
            } else {
                let here = app.active_panel().active_tab().path.clone();
                if let Some(parent) = here.parent() {
                    // Land on the directory we are leaving rather than at the
                    // top of the parent, so walking back up a tree keeps its
                    // place at every level. Leaving an *archive* is the same
                    // move and lands on the archive file, which is why the
                    // name comes from `leaving_name`: the root of an archive
                    // segment has no file name of its own.
                    let select = crate::app::leaving_name(&here);
                    app.navigate_selecting(app.active_side, parent, select);
                }
            }
        }
        A::Root => {
            let root = app.active_panel().active_tab().path.segment_root();
            app.navigate(app.active_side, root);
        }
        // the field starts empty, and `Enter` on an empty field means home
        // - which is what keeps the feature usable on a terminal that
        // cannot deliver `Ctrl+Shift+G`.
        A::GotoPath => app.push_dialog(Box::new(
            crate::dialog::InputDialog::new(
                DialogId::GotoPath,
                "Go to",
                "Path (empty for home):",
                "",
            )
            // an empty field is not a mistake here, it means home - and it is
            // what keeps the feature reachable on a terminal that cannot
            // deliver `Ctrl+Shift+G`.
            .allowing_empty(),
        )),
        A::Home => match crate::config::paths::home_dir() {
            Ok(home) => app.navigate(app.active_side, crate::vfs::VfsPath::local(home)),
            Err(err) => app.message = Some(err.to_string()),
        },
        A::Open => app.open_under_cursor(),
        // "`Ctrl+PgDn` remains \"enter as a directory\", forcing
        // archive entry for archives with odd extensions."
        A::EnterAsDir => app.enter_as_dir(),
        A::Reread => panel::reread(app),
        A::OtherPanelCd => {
            // the other panel follows this one. On a **file** -
            // and a search result is the natural case, since "where does this
            // live" is the question a hit raises - the answer is the directory
            // it lives in, with the cursor on it. Asking the other panel to
            // list a file has always been wrong here; it was merely harmless
            // while the key was only ever pressed on a directory.
            let tab = app.active_panel().active_tab();
            let Some(path) = tab.current_path() else {
                return Ok(());
            };
            let is_dir = tab.current().is_some_and(|e| e.is_dir() || e.is_parent);
            let other = app.active_side.other();
            if is_dir {
                app.navigate(other, path);
            } else {
                let select = path.file_name();
                match path.parent() {
                    Some(parent) => app.navigate_selecting(other, parent, select),
                    None => app.navigate(other, path),
                }
            }
        }
        A::LeaveVirtual => {
            // the design makes this a second, *state-dependent* meaning of
            // `Ctrl+R` and `Esc` rather than a binding of its own - "one key,
            // resolved by panel state" - so the shipped keymap does not bind
            // it: `A::Reread` and `A::ClearSearch` resolve it themselves, and
            // a context binding here would shadow both. A user who binds it by
            // hand gets exactly the leave, and on a real directory the two
            // keys the spec names keep their normal meaning.
            if panel::leave_virtual(app) {
                return Ok(());
            }
            match press.code {
                KeyCode::Esc => panel::clear_search_then_marks(app),
                KeyCode::Char('r') if press.mods.contains(KeyModifiers::CONTROL) => {
                    panel::reread(app)
                }
                _ => {
                    app.message =
                        Some("this panel is not a virtual listing; nothing to leave".to_string());
                }
            }
        }

        // "`Ctrl+B` branch view is this same mechanism with an
        // empty pattern: a flat recursive listing of the current tree, in the
        // active panel." One engine, one cancellation, one virtual listing;
        // only the header and the status line say `branch` rather than
        // `search`. No dialog, which is also why it never searches archives -
        // opening every archive in a tree is not what one keystroke should be
        // able to ask for.
        A::BranchView => {
            let tab = app.active_panel().active_tab();
            // A branch view *of* a virtual listing is a branch view of the
            // tree that listing came from: a `list:` path has no tree under it.
            //
            let root = tab
                .virtual_view()
                .map_or_else(|| tab.path.clone(), |view| view.origin.clone());
            app.request_search(
                crate::search::Query::branch(root),
                crate::panel::VirtualKind::Branch,
            );
        }

        // ------------------------------------------------------ marking -----
        A::ToggleSelect => {
            // `Insert` is never ambiguous: it marks whatever the quick-search
            // buffer holds, and moving down clears the buffer the way any other
            // cursor movement does.
            app.toggle_mark_under_cursor();
            app.move_cursor(1);
        }
        A::SelectAndSize => {
            // `Space` extends a running search - filenames contain spaces - and
            // marks only when the buffer is empty.
            if app.active_panel().quick.is_empty() {
                app.toggle_mark_under_cursor();
                panel::size_under_cursor(app);
            } else {
                panel::type_into_quick_search(app, ' ');
            }
        }
        A::FileInfo => open_file_info(app),
        A::Resize => files::open_resize(app),
        A::CopyPath => files::copy_paths(app),
        A::DirSize => panel::size_selection(app),
        // `+` and `-` open the same prompt in opposite directions, offering
        // the last mask of the session.
        A::SelectMask => panel::open_mask_prompt(app, DialogId::SelectMask),
        A::UnselectMask => panel::open_mask_prompt(app, DialogId::UnselectMask),
        A::InvertSelection => app.invert_marks(),
        A::SelectAll => app.mark_all(),
        // `Ctrl+C` keeps its interrupt meaning inside the console;
        // these are the panel context, resolved by the keymap.
        A::ClipboardCopy => app.clipboard_take(false),
        A::ClipboardCut => app.clipboard_take(true),
        // the fallback, pasted: at the **command line** `Ctrl+V` prefers what a
        // viewer copy left, and everywhere else it is the files as before.
        A::ClipboardPaste => {
            if !app.paste_text_clipboard() {
                app.clipboard_paste();
            }
        }

        // --------------------------------------------------------- tabs -----
        A::TabNew => {
            let path = app.active_panel().active_tab().path.clone();
            let max = app.config.panel.max_tabs;
            if app.active_panel_mut().open_tab(path.clone(), max) {
                // A new tab is "opened at the current directory", which means
                // it has to be *read*: `open_tab` only pushes an empty `Tab`,
                // and nothing else would ever fill it, so without this the new
                // tab renders as a blank panel. `dispatch` never touches the
                // filesystem itself - it queues the read and the event loop
                // services it.
                //
                let side = app.active_side;
                let index = app.active_panel().active_index();
                app.request_read(side, index, path);
            } else {
                app.message = Some(format!("already at the maximum of {max} tabs"));
            }
        }
        // every one of these makes a different directory the
        // active one without reading it, so the shell is told explicitly.
        A::TabNext => {
            app.active_panel_mut().cycle_tab(true);
            app.sync_active_cwd();
        }
        A::TabPrev => {
            app.active_panel_mut().cycle_tab(false);
            app.sync_active_cwd();
        }
        A::TabClose => {
            if app.close_tab(app.active_side) {
                app.sync_active_cwd();
            } else {
                app.message = Some("the last tab cannot be closed".to_string());
            }
        }
        a if a.tab_index().is_some() => panel::select_tab(app, a.tab_index().unwrap_or(1)),

        // -------------------------------------------------------- sorts -----
        // Positional over `panel.columns.order`, and it keeps working on a
        // column that is currently hidden.
        a if a.sort_column_index().is_some() => {
            let n = a.sort_column_index().unwrap_or(1);
            match sort_column(&app.config.panel.columns.order, n) {
                Some(column) => app.sort_active(SortKey::Column(column)),
                None => app.message = Some("no columns are configured".to_string()),
            }
        }
        // the same positional mapping, setting the tiebreak.
        a if a.sort_secondary_index().is_some() => {
            let n = a.sort_secondary_index().unwrap_or(1);
            match sort_column(&app.config.panel.columns.order, n) {
                Some(column) => app.sort_secondary(column),
                None => app.message = Some("no columns are configured".to_string()),
            }
        }
        A::SortByName => app.sort_active(SortKey::Column(ColumnId::Name)),
        A::SortByExt => app.sort_active(SortKey::Column(ColumnId::Ext)),
        A::SortByDate => app.sort_active(SortKey::Column(ColumnId::Date)),
        A::SortBySize => app.sort_active(SortKey::Column(ColumnId::Size)),
        A::SortUnsorted => app.sort_active(SortKey::Unsorted),
        // `Ctrl+F7`. Total Commander puts "unsorted" here and it is the one
        // sort nobody wants twice: a listing in whatever order the filesystem
        // handed it over is not an order, and the way back from it was to
        // remember which column had been sorted by. This is the way back.
        A::SortDefault => app.sort_active(SortKey::default()),

        // ------------------------------------------------------ toggles -----
        A::ShowHidden => {
            app.config.panel.show_hidden = !app.config.panel.show_hidden;
            panel::reread(app);
        }
        A::SwapPanels => app.swap_panels(),

        // ------------------------------------------- the command line -------
        // Both of these insert at the *remembered* caret and **take focus to
        // the command line** - the design record that as a deliberate
        // reversal of an earlier draft, because one filename followed by typing
        // is the common case. Only the source of the text differs.
        A::PutSelected => app.put_selected(false),
        A::PutSelectedPath => app.put_selected(true),
        A::PathToCmdline => {
            let path = app.active_panel().active_tab().path.to_string();
            app.insert_argument(&shell_quote(&path));
        }
        A::CaretLeft => app.cmdline.move_left(),
        A::CaretRight => app.cmdline.move_right(),
        A::LineStart => app.cmdline.move_home(),
        A::LineEnd => app.cmdline.move_end(),
        A::CaretBackspace => {
            app.cmdline.backspace();
        }
        A::CaretDelete => {
            app.cmdline.delete();
        }
        A::KillWord => app.cmdline.kill_word(),
        A::KillLine => app.cmdline.kill_line(),
        A::KillToEnd => app.cmdline.kill_to_end(),
        A::ToggleOverwrite => app.cmdline.overwrite = !app.cmdline.overwrite,
        A::Clear => {
            // Esc clears the line; if it is already empty, focus returns to
            // the panel. Clearing is also the only thing that resets the
            // caret.
            //
            // `clear_command_line` is the same function the panel's own Esc
            // uses, so one key means one thing in both places and the shell
            // case is handled once: with a live shell the text belongs to the
            // shell and is emptied with a kill-line, not by clearing a buffer
            // here that the shell is not reading from.
            if !panel::clear_command_line(app) {
                app.set_focus(Focus::Panel(app.active_side));
            }
        }
        // the design makes history the shell's: "History, completion,
        // `Ctrl+R`, vi or emacs bindings … all of it is whatever the user has
        // configured". `Ctrl+Up` is not a key any shell binds, so it is
        // *translated* into the plain `Up` every shell does - which the
        // design has spent on leaving for the panel and cannot also spend
        // here.
        A::HistoryPrev => {
            if app.console_owns_cmdline() {
                console::shell_arrow(app, KeyCode::Up);
            } else {
                app.cmdline.history_prev();
            }
        }
        A::HistoryNext => {
            if app.console_owns_cmdline() {
                console::shell_arrow(app, KeyCode::Down);
            } else {
                app.cmdline.history_next();
            }
        }
        // `F3` / `Shift+F3`. Queued, not opened: opening reads
        // from the filesystem and starts a background scan, and neither is
        // `dispatch`'s to do.
        //
        // `view` and `view_single` are the same thing in v0.4 and the split is
        // kept because the design binds both: Total Commander's `F3` walks a
        // multi-file selection and `Shift+F3` refuses to, and there is no
        // walk to refuse until the viewer can be handed a list.
        A::View | A::ViewSingle => viewer::open_viewer(app),

        // `F4` / `Shift+F4`.
        A::Edit => files::open_editor(app),
        A::EditNew => files::open_edit_new(app),

        // the design makes the history the shell's own, so there is no list
        // here to open a dialog on to. Saying where it actually lives beats
        // both a dialog showing nothing and a milestone message that promises
        // one ("History, completion, `Ctrl+R`, vi or emacs
        // bindings - all of it is whatever the user has configured").
        A::HistoryDialog if app.console_owns_cmdline() => {
            app.message =
                Some("history is the shell's own - press Ctrl+R in the console".to_string());
        }
        // With no shell alive the fallback command line keeps a list of its
        // own, and `Alt+F8` is a dialog over that (the design;
        // the design). The chosen line is **put on** the
        // command line, not run: `Enter` there is what runs one.
        A::HistoryDialog => {
            let history = app.cmdline.history.clone();
            app.push_dialog(Box::new(crate::ui::dialog::HistoryDialog::new(&history)));
        }

        A::ThemePicker => app.open_theme_picker(),

        // Queued, not asked: the request goes out from the event loop, so a
        // slow or unreachable GitHub cannot hold a keystroke.
        A::CheckUpdate => app.request_update_check(),

        // `Ctrl+O`.
        A::ConsoleToggle => console::console_toggle(app),
        A::ConsoleScrollUp => console::scroll_console(app, true),
        A::ConsoleScrollDown => console::scroll_console(app, false),
        A::Run => console::run_command_line(app),

        A::ReloadConfig => app.reload_config(),
        // the `Configuration` menu, resolved as the design resolves it: the
        // design specifies no control for any of its keys, so the settings are
        // opened in the editor and reloaded when it exits. A settings dialog
        // for ninety keys is a feature the design does not describe.
        A::EditConfig => open_config_editor(app),

        // ------------------------------------------- the design help -----
        // The whole-program reference, on the section this key was pressed in.
        // Generated by the event loop from the live keymap, so `dispatch` only
        // says which topic.
        A::Help => app.request_view(crate::app::ViewRequest::Help {
            topic: help_topic(app),
        }),

        // -------------------------------------------- the design menus --
        // `F9` opens the bar on its first menu; `Alt`+letter opens the one
        // that letter names. One path, because they differ only in which menu
        // is dropped.
        A::Menu
        | A::MenuFiles
        | A::MenuMark
        | A::MenuCommands
        | A::MenuNet
        | A::MenuShow
        | A::MenuConfig => {
            let model = crate::ui::dialog::menu::model(app);
            let open = action.menu_index().unwrap_or(0);
            app.push_dialog(Box::new(crate::ui::dialog::MenuDialog::new(model, open)));
        }

        // ------------------------------------------ the design menu ---
        A::ContextMenu => open_context_menu(app),

        // ------------------------------------------- the design devices --
        // **Spatial**: `Alt+F1` is the left panel and `Alt+F2` the right,
        // whichever one has focus, and the focus follows the choice
        // (invariant I1).
        A::DriveLeft => app.request_drives(crate::app::DrivesRequest::Devices(Side::Left)),
        A::DriveRight => app.request_drives(crate::app::DrivesRequest::Devices(Side::Right)),
        A::Hotlist => app.request_drives(crate::app::DrivesRequest::Hotlist),
        A::HotlistAdd => open_hotlist_add(app),

        // -------------------------------------------- the design compare -
        A::CompareDirs => app.compare_lists(),
        A::GitHistory => app.open_git_history(),
        A::CompareFiles => app.compare_files(),
        A::DiffFiles => app.diff_files(),
        A::ChecksumCreate => files::open_checksum(app),
        A::ChecksumVerify => files::verify_checksum(app),
        A::SplitFile => files::open_split(app),
        A::MergeFile => files::merge_parts(app),
        A::CreateSymlink => files::open_link(app, true),
        A::CreateHardlink => files::open_link(app, false),
        A::EditPermissions => files::open_permissions(app),

        // ------------------------------------------ the design quick view
        A::QuickView => app.quick_view_toggle(),

        // ----------------------------------------- the design open with --
        // `Shift+Enter` "**always** opens with the associated application,
        // never executes", which is the one flag that separates it from
        // `Enter`.
        A::OpenWith => app.open_with_association(),

        // the `Ctrl+Shift+0`: no secondary key at all, which is a
        // state the nine `Ctrl+Shift+<n>` keys cannot reach on their own.
        A::SortSecondaryClear => app.sort_secondary_clear(),

        // ------------------------------------------- the design operations --
        A::RenameInPlace => files::open_rename(app),
        A::Copy => files::open_copy_move(app, JobKind::Copy, false),
        A::CopySameDir => files::open_copy_move(app, JobKind::Copy, true),
        A::Move => files::open_copy_move(app, JobKind::Move, false),
        A::Mkdir => files::open_mkdir(app),
        A::Delete => files::open_delete_confirm(app, true),
        A::DeletePermanent => files::open_delete_confirm(app, false),
        A::JobQueue => files::open_job_queue(app),

        // ------------------------------------------- the design archives --
        // the two halves: "`Alt+F5` packs the selection: a dialog
        // for target name, format, compression level, and 'move to archive'",
        // and "`Alt+F6` unpacks the archive under the cursor to the other
        // panel's directory".
        A::Pack => files::open_pack(app),
        A::Unpack => files::open_unpack(app),

        // ------------------------------- the design search rename ----
        // ------------------------------- the design remote connections ---
        // One key, resolved by panel state: the connect dialog on a local
        // panel, a disconnect prompt on a connected one.
        A::ConnectToggle => app.connect_toggle(),

        A::Search => search::open_find(app),
        A::SearchInPanel => search::open_find_in_panel(app),
        A::MultiRename => search::open_multi_rename(app),
        A::RenameResult => search::open_rename_result(app),

        // Everything else belongs to a later milestone. It resolves, and it
        // says so: never a panic, never a silent no-op.
        other => {
            app.message = Some(if other.implemented() {
                format!("{}: nothing bound to do yet", other.description())
            } else {
                other.not_implemented_message()
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Keymap, Theme};
    use crate::dialog::DialogResult;
    use crate::dialog::{InputDialog, MessageDialog};
    use crate::panel::Side;
    use crate::ui::dialog::CopyMoveDialog;
    use crate::ui::dialog::{FindDialog, MultiRenameDialog};
    use crate::vfs::Entry;
    use crate::vfs::VfsPath;
    use crate::viewer::select::Extend;

    fn app_with(entries: Vec<Entry>) -> App {
        let mut app = App::headless(Config::default(), Keymap::builtin(), Theme::blue());
        let tab = app.left.active_tab_mut();
        tab.path = VfsPath::local("/root");
        tab.entries = entries;
        app
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        let key = KeyEvent::new(code, mods);
        dispatch(app, key).expect("dispatch never fails on a bound key");
    }

    // ------------------------------------------ the rendered view, 3 ----

    /// `3` in the viewer, through the shipped keymap.
    #[test]
    fn three_opens_the_rendered_view_and_the_fold_keys_are_bound_with_it() {
        let app = app_with(vec![Entry::file("a.json")]);
        for (key, want) in [
            (KeyCode::Char('1'), Action::ModeText),
            (KeyCode::Char('2'), Action::ModeHex),
            (KeyCode::Char('3'), Action::ModeRender),
            (KeyCode::Enter, Action::FoldToggle),
            (KeyCode::Char(' '), Action::FoldToggle),
            (KeyCode::Char('-'), Action::FoldAll),
            (KeyCode::Char('+'), Action::UnfoldAll),
        ] {
            assert_eq!(
                app.keymap
                    .resolve(KeyContext::Viewer, KeyPress::new(key, KeyModifiers::NONE)),
                Resolution::Action(want),
                "{key:?}"
            );
        }
    }

    // ------------------------------------- the hex viewer's templates ----

    /// A viewer over some bytes, in hex, with the focus on it.
    fn app_viewing(body: &str) -> App {
        let mut app = app_with(vec![Entry::file("a.bin")]);
        let viewer = crate::viewer::Viewer::open_memory(
            crate::viewer::ViewerId(1),
            "a.bin",
            body.to_string(),
            &app.config.viewer,
        )
        .expect("a viewer");
        app.push_viewer(viewer);
        if let Some(viewer) = app.viewer_mut() {
            viewer
                .set_mode(crate::config::ViewerMode::Hex)
                .expect("hex");
        }
        app
    }

    /// `t` in the viewer, through the shipped keymap rather than by calling
    /// the handler, so the binding is what is asserted.
    #[test]
    fn t_opens_the_template_picker_in_the_viewer_and_nowhere_else() {
        let mut app = app_viewing("0123456789abcdef");
        assert_eq!(
            app.keymap.resolve(
                KeyContext::Viewer,
                KeyPress::new(KeyCode::Char('t'), KeyModifiers::NONE)
            ),
            Resolution::Action(Action::ViewerTemplate)
        );
        // And nothing takes a bare `t` away from it in a panel, where it is
        // quick search and must stay quick search.
        assert_eq!(
            app.keymap.resolve(
                KeyContext::Panel,
                KeyPress::new(KeyCode::Char('t'), KeyModifiers::NONE)
            ),
            Resolution::Unbound
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::Template)
        );
    }

    /// The picker's first row is the one that removes a template.
    #[test]
    fn the_picker_offers_none_first_and_then_the_built_in_templates() {
        let mut app = app_viewing("0123456789abcdef");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        let picker = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<crate::ui::dialog::template::TemplateDialog>())
            .expect("the picker, downcast");
        assert_eq!(
            picker.selected(),
            Some(crate::ui::dialog::template::NONE),
            "nothing is applied, so the cursor is on the row that applies nothing"
        );
    }

    #[test]
    fn accepting_a_name_applies_it_and_none_takes_it_away_again() {
        let mut app = app_viewing("0123456789abcdef");
        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text("PNG".to_string()),
        );
        assert_eq!(
            app.viewer().and_then(crate::viewer::Viewer::template_name),
            Some("PNG")
        );
        // The message says where it landed, not only which one it is.
        let said = app.message.clone().unwrap_or_default();
        assert!(said.contains("PNG"), "{said}");
        assert!(said.contains("0x0"), "{said}");

        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text(crate::ui::dialog::template::NONE.to_string()),
        );
        assert_eq!(
            app.viewer().and_then(crate::viewer::Viewer::template_name),
            None
        );
    }

    #[test]
    fn a_name_that_is_not_a_template_says_so_and_changes_nothing() {
        let mut app = app_viewing("0123456789abcdef");
        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text("PNG".to_string()),
        );
        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text("not a format".to_string()),
        );
        assert_eq!(
            app.viewer().and_then(crate::viewer::Viewer::template_name),
            Some("PNG"),
            "the applied template is left alone"
        );
        assert!(
            app.message
                .as_deref()
                .is_some_and(|m| m.contains("no such template")),
            "{:?}",
            app.message
        );
    }

    /// `Esc` out of the picker leaves whatever was applied alone.
    #[test]
    fn cancelling_the_picker_keeps_the_applied_template() {
        let mut app = app_viewing("0123456789abcdef");
        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text("PNG".to_string()),
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.top_dialog().is_none(), "the picker closed");
        assert_eq!(
            app.viewer().and_then(crate::viewer::Viewer::template_name),
            Some("PNG")
        );
    }

    /// Reopening the picker starts on what is applied, so stepping away from
    /// it and back is a comparison.
    #[test]
    fn the_picker_opens_on_the_applied_template() {
        let mut app = app_viewing("0123456789abcdef");
        dialog_answered(
            &mut app,
            DialogId::Template,
            None,
            DialogResult::Text("GIF header".to_string()),
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        let picker = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<crate::ui::dialog::template::TemplateDialog>())
            .expect("the picker, downcast");
        assert_eq!(picker.selected(), Some("GIF header"));
    }

    // ------------------------------ the design remote connections --------

    /// the key bar: "`Ctrl+F` | connect the active panel to a remote
    /// host; on an already-connected panel, prompt to disconnect".
    ///
    /// Through `dispatch` and the shipped keymap, so the binding is asserted
    /// and not only the handler.
    #[test]
    fn ctrl_f_reaches_the_connect_dialog_through_the_shipped_keymap() {
        let mut app = app_with(vec![Entry::file("a")]);
        assert_eq!(
            app.keymap.resolve(
                KeyContext::Panel,
                KeyPress::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            ),
            Resolution::Action(Action::ConnectToggle)
        );
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(
            app.top_dialog().map(crate::dialog::Dialog::id),
            Some(DialogId::Connect),
            "Ctrl+F on a local panel opens the connect dialog"
        );
        assert_eq!(
            app.message, None,
            "and it is no longer reported as unimplemented"
        );
    }

    /// the v0.65 line has arrived, so the action reports as
    /// available rather than naming a release.
    #[test]
    fn connect_toggle_is_implemented_in_this_milestone() {
        assert!(Action::ConnectToggle.implemented());
        assert_eq!(
            Action::ConnectToggle.milestone(),
            crate::input::Milestone::V065
        );
    }

    /// the `Ctrl+O` on a connected panel, and
    /// the resolution of it: the SSH console is not
    /// in this milestone, so the local shell stays **and says so**.
    #[test]
    fn ctrl_o_on_a_connected_panel_keeps_the_local_shell_and_says_so() {
        let mut app = app_with(vec![Entry::file("a")]);
        let id = crate::remote::RemoteId(2);
        {
            let tab = app.left.active_tab_mut();
            tab.path = id.path("/srv");
            tab.remote_view = Some(Box::new(crate::panel::RemoteView {
                id,
                authority: "sftp://thorin@nas.local:2222".to_string(),
                origin: VfsPath::local("/home/thorin"),
                origin_cursor: None,
                disconnected: false,
            }));
        }
        console::console_toggle(&mut app);
        let message = app.message.clone().unwrap_or_default();
        assert!(
            message.contains("sftp://thorin@nas.local:2222"),
            "{message}"
        );
        assert!(
            message.contains("local shell") && message.contains("not built"),
            "it says the console stays local and why"
        );
        assert!(
            !message.contains("v0."),
            "and names no release: v0.7 has shipped without it and the design \
             has nothing after it: {message}"
        );
        assert!(
            !message.contains("hunter2"),
            "and it carries no secret, because a Target has none"
        );
    }

    /// "`F3` on a remote file streams into the viewer with a
    /// size cap (`remote.view_max_size`, default 32 MB) before it offers to
    /// download instead."
    ///
    /// Decided before anything is read, which is the whole point of a cap.
    #[test]
    fn f3_on_an_oversized_remote_file_refuses_before_it_transfers_anything() {
        let mut app = app_with(vec![Entry {
            size: app_cap() + 1,
            ..Entry::file("huge.iso")
        }]);
        let id = crate::remote::RemoteId(4);
        app.left.active_tab_mut().path = id.path("/srv");
        viewer::open_viewer(&mut app);
        assert!(app.take_pending_view().is_none(), "nothing was queued");
        let message = app.message.clone().unwrap_or_default();
        assert!(message.contains("remote.view_max_size"), "{message}");

        // A file under the cap opens as any other does.
        let mut app = app_with(vec![Entry {
            size: 10,
            ..Entry::file("small.txt")
        }]);
        app.left.active_tab_mut().path = id.path("/srv");
        viewer::open_viewer(&mut app);
        assert!(app.take_pending_view().is_some());
    }

    /// The shipped `remote.view_max_size`, for the test above.
    fn app_cap() -> u64 {
        crate::config::Config::default()
            .remote
            .view_max_size
            .bytes()
    }

    // ------------------------------------ the design search rename --

    /// the retry, and the one job kind it cannot be built for.
    ///
    /// A multi-rename's sources are paired positionally with `JobSpec::targets`,
    /// so a retry over the subset that failed would have to
    /// carry exactly their targets. Rebuilding the spec with `targets` emptied
    /// instead queued a `JobKind::Rename` that renamed nothing and reported a
    /// clean run: the status line said `retrying 1 failed item` and then
    /// `renamed 0 files, 0 dirs`, and the disk was untouched.
    ///
    /// The summary dialog no longer offers the button (`SummaryDialog`), and
    /// this is the other half: the queue view opens a summary for any finished
    /// job with failures, so the refusal has to be here too.
    #[test]
    fn a_failed_multi_rename_is_refused_a_retry_rather_than_queued_empty() {
        let mut app = app_with(vec![Entry::file("alpha.txt")]);
        let source = VfsPath::local("/root/alpha.txt");
        let target = VfsPath::local("/root/alpha-v2.txt");
        let id = app.request_job(crate::ops::JobSpec::rename(vec![(source.clone(), target)]));
        let queued = app.take_pending_jobs().len();
        assert_eq!(queued, 1, "the batch itself is a job like any other");
        if let Some(status) = app.jobs.status_mut(id) {
            status.finished = Some(Box::new(crate::ops::JobSummary {
                kind: crate::ops::JobKind::Rename,
                files_done: 0,
                dirs_done: 0,
                bytes_done: 0,
                skipped: 0,
                failures: vec![crate::ops::JobFailure {
                    path: source,
                    error: "alpha-v2.txt already exists".to_string(),
                }],
                cancelled: false,
                elapsed: std::time::Duration::ZERO,
                sized: Vec::new(),
                differing: Vec::new(),
                first_difference: None,
            }));
        }

        let line = files::retry_failures(&mut app, id);
        assert_eq!(
            line,
            "a multi-rename is not retried; use Undo, or run it again"
        );
        assert!(
            app.take_pending_jobs().is_empty(),
            "and above all nothing was queued"
        );
    }

    /// `Alt+F7` opens the Find Files dialog rather than
    /// reporting a milestone.
    #[test]
    fn alt_f7_opens_the_find_dialog() {
        let mut app = app_with(vec![Entry::file("alpha.rs")]);
        press(&mut app, KeyCode::F(7), KeyModifiers::ALT);
        assert_eq!(app.focus, Focus::Dialog(DialogId::Find));
        assert_eq!(app.message, None, "it acts rather than explaining itself");
    }

    /// `Ctrl+B` queues a branch view of the panel's own tree,
    /// with no dialog in front of it.
    #[test]
    fn ctrl_b_queues_a_branch_view_of_the_panels_tree() {
        let mut app = app_with(vec![Entry::file("alpha.rs")]);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        let request = app
            .take_pending_search()
            .expect("ctrl+b queues a search rather than running one");
        assert_eq!(request.kind, crate::panel::VirtualKind::Branch);
        assert!(request.query.is_branch());
        assert_eq!(request.query.roots, vec![VfsPath::local("/root")]);
    }

    /// the two switches that are configuration
    /// rather than dialog controls are stamped onto every query, whichever key
    /// started it. Neither the dialog nor `Query::branch` is handed a `Config`.
    #[test]
    fn a_queued_search_carries_the_configured_gitignore_and_symlink_rules() {
        let mut config = Config::default();
        config.search.respect_gitignore = true;
        config.ops.follow_symlinks = true;
        let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
        app.left.active_tab_mut().path = VfsPath::local("/root");
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        let request = app.take_pending_search().expect("a queued search");
        assert!(request.query.respect_gitignore);
        assert!(request.query.follow_symlinks);
    }

    /// the `Start search`, answered: the search is queued and the
    /// combo boxes remember what was typed.
    #[test]
    fn the_find_dialogs_answer_queues_the_search_and_feeds_the_history() {
        let mut app = app_with(vec![Entry::file("alpha.rs")]);
        let mut query = crate::search::Query::new(VfsPath::local("/root"));
        query.name = "*.rs".to_string();
        dialog_accepted(
            &mut app,
            DialogId::Find,
            DialogResult::Find(Box::new(crate::dialog::FindAnswer {
                query: query.clone(),
                saved: None,
                tab: 1,
            })),
        );
        let request = app.take_pending_search().expect("a queued search");
        assert_eq!(request.query.name, "*.rs");
        assert_eq!(request.kind, crate::panel::VirtualKind::Search);
        assert_eq!(app.search.history.names, vec!["*.rs".to_string()]);
        assert_eq!(app.search.history.roots, vec!["/root".to_string()]);
        assert_eq!(app.search.last, Some(query));
        assert_eq!(app.search.tab, 1, "it reopens on the tab it was left on");
        assert!(app.search.history_dirty, "the write is the event loop's");
        assert!(
            !app.search.saved_dirty,
            "the Load/Save tab was not touched, so nothing needs writing"
        );
    }

    /// `Ctrl+M` opens the multi-rename tool over the whole
    /// directory when nothing is marked.
    #[test]
    fn ctrl_m_opens_the_multi_rename_tool() {
        let mut app = app_with(vec![Entry::file("alpha.txt"), Entry::file("beta.txt")]);
        press(&mut app, KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, Focus::Dialog(DialogId::MultiRename));
        let plan = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<MultiRenameDialog>())
            .map(|dialog| dialog.plan().items().len());
        assert_eq!(plan, Some(2), "both rows, because nothing is marked");
    }

    /// `Start!` queues a rename, and the undo is armed from the
    /// pairs the job was given rather than re-derived later.
    #[test]
    fn start_queues_a_rename_and_arms_the_undo() {
        let mut app = app_with(vec![Entry::file("alpha.txt")]);
        let pairs = vec![(
            VfsPath::local("/root/alpha.txt"),
            VfsPath::local("/root/beta.txt"),
        )];
        dialog_accepted(
            &mut app,
            DialogId::MultiRename,
            DialogResult::MultiRename(Box::new(crate::dialog::MultiRenameAnswer {
                pairs: pairs.clone(),
                undo: false,
                show_result: false,
                settings: crate::rename::Settings::reset(),
            })),
        );
        let request = app.take_pending_rename().expect("a queued rename");
        assert_eq!(request.pairs, pairs);
        assert!(!request.undoing);
        app.start_rename(*request);
        let undo = app.rename.undo.as_ref().expect("the undo is armed");
        assert_eq!(
            undo.pairs,
            vec![(
                VfsPath::local("/root/beta.txt"),
                VfsPath::local("/root/alpha.txt"),
            )],
            "the pairs, reversed"
        );
    }

    /// the `Undo`, with nothing to undo: said out loud, never a
    /// silent no-op.
    #[test]
    fn undo_with_nothing_to_undo_says_so() {
        let mut app = app_with(vec![Entry::file("alpha.txt")]);
        dialog_accepted(
            &mut app,
            DialogId::MultiRename,
            DialogResult::MultiRename(Box::new(crate::dialog::MultiRenameAnswer {
                undo: true,
                ..crate::dialog::MultiRenameAnswer::default()
            })),
        );
        assert!(app.take_pending_rename().is_none());
        assert_eq!(app.message.as_deref(), Some("there is nothing to undo"));
    }

    /// `Alt+Shift+F7` narrows: the rows the user marked become the search
    /// roots.
    #[test]
    fn search_in_panel_makes_the_marked_rows_the_roots() {
        let mut app = app_with(vec![Entry::dir("src"), Entry::dir("docs")]);
        {
            let tab = app.left.active_tab_mut();
            tab.marks.insert("src".to_string());
        }
        press(
            &mut app,
            KeyCode::F(7),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(app.focus, Focus::Dialog(DialogId::Find));
        let roots = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<FindDialog>())
            .map(|dialog| dialog.query().roots);
        assert_eq!(roots, Some(vec![VfsPath::local("/root/src")]));
    }

    /// `Alt+Shift+M` with no rename behind it reports rather than opening an
    /// empty list - and the key really is bound to it.
    #[test]
    fn the_result_list_says_when_there_is_nothing_to_show() {
        let mut app = app_with(vec![Entry::file("alpha.txt")]);
        assert_eq!(
            app.keymap.resolve(
                KeyContext::Panel,
                KeyPress::new(KeyCode::Char('M'), KeyModifiers::ALT | KeyModifiers::SHIFT)
            ),
            Resolution::Action(Action::RenameResult),
            "examples/keymap.toml binds alt+shift+m"
        );
        press(
            &mut app,
            KeyCode::Char('M'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert!(app.top_dialog().is_none());
        assert_eq!(
            app.message.as_deref(),
            Some("no multi-rename has run yet this session")
        );
    }

    /// `search.engine = "external"` is refused
    /// with the reason, and the internal engine runs anyway.
    #[test]
    fn an_external_search_engine_is_reported_and_the_search_still_runs() {
        let mut config = Config::default();
        config.search.engine = crate::config::SearchEngine::External;
        let mut app = App::headless(config, Keymap::builtin(), Theme::blue());
        app.left.active_tab_mut().path = VfsPath::local("/root");
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        let request = app.take_pending_search().expect("a queued search");
        // `start_search` spawns a walk, which needs a runtime; the refusal is
        // decided before that and is what this asserts on.
        assert_eq!(
            app.config.search.engine_refusal(),
            Some(crate::config::config::EXTERNAL_ENGINE_REFUSAL)
        );
        assert!(request.query.is_branch());
    }

    /// the design, worked through for every shifted key the
    /// default map can produce - the table is the specification and each row of
    /// it is one line here.
    #[test]
    fn shift_and_ctrl_shift_resolve_to_the_movement_and_how_it_extends() {
        let km = Keymap::builtin();
        let ask = |code: KeyCode, mods: KeyModifiers| {
            viewer_extend(&km, KeyContext::Viewer, KeyPress::new(code, mods))
        };
        const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
        let ctrl_shift = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);

        // 1. A key that resolves as itself runs as itself and extends nothing.
        assert_eq!(
            ask(KeyCode::Down, KeyModifiers::NONE),
            Some((Action::CursorDown, Extend::None))
        );
        // 2. `Shift` + a movement: linear.
        for (code, action) in [
            (KeyCode::Up, Action::CursorUp),
            (KeyCode::Down, Action::CursorDown),
            (KeyCode::Left, Action::CaretLeft),
            (KeyCode::Right, Action::CaretRight),
            (KeyCode::PageUp, Action::CursorPageUp),
            (KeyCode::PageDown, Action::CursorPageDown),
            (KeyCode::Home, Action::LineStart),
            (KeyCode::End, Action::LineEnd),
        ] {
            assert_eq!(
                ask(code, SHIFT),
                Some((action, Extend::Linear)),
                "Shift+{code:?}"
            );
        }
        // 3. `Ctrl+Shift` + a movement whose `Ctrl` form is unbound in the
        //    viewer: rectangular.
        for (code, action) in [
            (KeyCode::Up, Action::CursorUp),
            (KeyCode::Down, Action::CursorDown),
            (KeyCode::Left, Action::CaretLeft),
            (KeyCode::Right, Action::CaretRight),
            (KeyCode::PageUp, Action::CursorPageUp),
            (KeyCode::PageDown, Action::CursorPageDown),
        ] {
            assert_eq!(
                ask(code, ctrl_shift),
                Some((action, Extend::Rectangular)),
                "Ctrl+Shift+{code:?}"
            );
        }
        // `Ctrl+Shift+Home` / `End` come out **linear**, because step 2 already
        // finds `ctrl+home` bound to `CursorTop` - and a rectangle over the
        // whole file is the whole file.
        assert_eq!(
            ask(KeyCode::Home, ctrl_shift),
            Some((Action::CursorTop, Extend::Linear))
        );
        assert_eq!(
            ask(KeyCode::End, ctrl_shift),
            Some((Action::CursorBottom, Extend::Linear))
        );
        // A user binding always wins: `shift+n` is `[viewer] find_prev`, so it
        // never reaches step 2.
        assert_eq!(
            ask(KeyCode::Char('N'), SHIFT),
            Some((Action::FindPrev, Extend::None))
        );
        // `Shift+Tab` resolves to nothing and `Tab` is not a movement, so it is
        // swallowed like any other unbound viewer key.
        assert_eq!(ask(KeyCode::BackTab, SHIFT), None);
        assert_eq!(ask(KeyCode::Tab, SHIFT), None);
    }

    #[test]
    fn every_viewer_motion_is_an_action_the_viewer_answers() {
        // The two tables of the design are one table: an
        // action that extends a selection but does not move the cursor would
        // make `Shift+Home` extend something `Home` does not move.
        assert_eq!(VIEWER_MOTIONS.len(), 10);
        for (action, motion) in VIEWER_MOTIONS {
            assert_eq!(viewer_motion(*action), Some(*motion));
            assert!(action.implemented(), "{action} is bound and works");
        }
        // And nothing else is one.
        assert_eq!(viewer_motion(Action::Close), None);
        assert_eq!(viewer_motion(Action::SelectAll), None);
    }

    #[test]
    fn tab_ctrl_a_and_the_copies_are_bound_in_the_viewer_rather_than_swallowed() {
        // `[viewer] select_all = ["ctrl+a"]` and
        // `hex_side = ["tab"]` are bound, and until this milestone both reached
        // `viewer_action`'s catch-all and reported "not available in the
        // viewer".
        let km = Keymap::builtin();
        let ask = |code: KeyCode, mods: KeyModifiers| {
            km.resolve(KeyContext::Viewer, KeyPress::new(code, mods))
        };
        assert_eq!(
            ask(KeyCode::Tab, KeyModifiers::NONE),
            Resolution::Action(Action::HexSide),
            "the design gives Tab to the hex side, not to other_panel"
        );
        assert_eq!(
            ask(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Resolution::Action(Action::SelectAll)
        );
        assert_eq!(
            ask(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Resolution::Action(Action::ClipboardCopy)
        );
        assert_eq!(
            ask(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT)
            ),
            Resolution::Action(Action::CopyInterpretation)
        );
        // item 2's documented alternates, for the terminals that
        // deliver neither `ctrl+shift+c` nor `ctrl+shift+arrow`.
        assert_eq!(
            ask(KeyCode::Char('c'), KeyModifiers::ALT),
            Resolution::Action(Action::CopyInterpretation)
        );
        assert_eq!(
            ask(KeyCode::Char('b'), KeyModifiers::ALT),
            Resolution::Action(Action::SelectBlock)
        );
    }

    #[test]
    fn alt_f5_opens_the_pack_dialog_with_every_answer_spec_13_2_asks_for() {
        // "a dialog for target name, format, compression level,
        // and 'move to archive'". The dialog is what collects them; this is
        // that it opens at all, on the other panel's directory, with a name
        // taken from what is being packed.
        let mut app = app_with(vec![Entry::file("notes.txt")]);
        app.right.active_tab_mut().path = VfsPath::local("/srv/media");
        press(&mut app, KeyCode::F(5), KeyModifiers::ALT);

        let dialog = app.top_dialog().expect("a dialog opened");
        assert_eq!(dialog.id(), DialogId::Pack);
        let pack = dialog
            .as_any()
            .and_then(|any| any.downcast_ref::<crate::ui::dialog::pack::PackDialog>())
            .expect("the pack dialog");
        assert_eq!(pack.target(), "/srv/media/notes.zip");
        assert!(!pack.moves());
    }

    #[test]
    fn alt_f5_answered_leaves_a_request_rather_than_creating_anything() {
        // `dispatch` may not touch the filesystem,
        // and creating a container is touching it. So the answer becomes a
        // request the event loop performs - the same shape as `F3`'s viewer.
        let mut app = app_with(vec![Entry::file("notes.txt")]);
        app.right.active_tab_mut().path = VfsPath::local("/srv/media");
        press(&mut app, KeyCode::F(5), KeyModifiers::ALT);
        dialog_accepted(
            &mut app,
            DialogId::Pack,
            DialogResult::Pack(Box::new(crate::dialog::PackAnswer {
                target: "/srv/media/notes.tar.gz".to_string(),
                format: crate::vfs::archive::format::FormatId::TarGz,
                level: 9,
                move_sources: true,
            })),
        );
        let request = app.take_pending_pack().expect("a request was queued");
        assert_eq!(request.container.to_string(), "/srv/media/notes.tar.gz");
        assert_eq!(request.level, 9);
        assert!(request.move_sources);
        assert_eq!(request.sources.len(), 1);
        assert!(!request.container.to_string().contains('#'));
    }

    #[test]
    fn an_unedited_copy_target_keeps_its_segments() {
        // A `VfsPath` is a stack of segments and the dialog's target is one
        // line of text: `…/bundle.zip#/` read back as text is a local file
        // whose name ends in `#`. `F5` into an archive has to add a member, not
        // create that file.
        let mut app = app_with(vec![Entry::file("outside.txt")]);
        app.right.active_tab_mut().path =
            VfsPath::local("/srv/bundle.zip").with_segment(crate::vfs::BackendKind::Archive, "/");
        press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        let target = app
            .top_dialog()
            .and_then(crate::dialog::Dialog::as_any)
            .and_then(|any| any.downcast_ref::<CopyMoveDialog>())
            .map(|d| d.target().to_string())
            .expect("the copy dialog");
        assert_eq!(target, "/srv/bundle.zip#/*.*");

        dialog_accepted(
            &mut app,
            DialogId::CopyMove,
            DialogResult::CopyMove(Box::new(crate::dialog::CopyMoveAnswer {
                target,
                ..crate::dialog::CopyMoveAnswer::default()
            })),
        );
        let request = app.take_pending_jobs().pop().expect("a job was queued");
        let dest = request.spec.dest.expect("a destination");
        assert_eq!(dest.segments().len(), 2, "still two segments: {dest}");
        assert_eq!(dest.backend(), crate::vfs::BackendKind::Archive);
    }

    #[test]
    fn quitting_with_confirm_exit_off_is_still_a_bare_flag() {
        // `ui.confirm_exit` gates the prompt, so with it off the
        // key quits outright and nothing opens.
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.config.ui.confirm_exit = false;
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert!(app.should_quit);
        assert!(!app.dialog_is_open());
    }

    #[test]
    fn confirm_exit_prompts_and_both_answers_are_honoured() {
        for (answer, quits) in [(KeyCode::Char('n'), false), (KeyCode::Char('y'), true)] {
            let mut app = app_with(vec![Entry::file("alpha")]);
            app.config.ui.confirm_exit = true;
            press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
            assert!(!app.should_quit, "the prompt comes first");
            assert_eq!(app.focus, Focus::Dialog(DialogId::ConfirmQuit));
            press(&mut app, answer, KeyModifiers::NONE);
            assert_eq!(app.should_quit, quits, "{answer:?}");
            assert!(!app.dialog_is_open());
        }
    }

    #[test]
    fn the_plain_quit_prompt_defaults_to_quitting() {
        // A stray `F10` alone no longer quits, which is the protection; `F10`
        // then `Enter` still does, which is the muscle memory.
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.config.ui.confirm_exit = true;
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.should_quit);
    }

    #[test]
    fn a_running_job_prompts_whatever_confirm_exit_says_and_names_it() {
        // "Quitting with a transfer in progress always prompts
        // regardless of that setting, naming what is still running."
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.config.ui.confirm_exit = false;
        let id = app.request_job(crate::ops::JobSpec::new(
            JobKind::Copy,
            vec![VfsPath::local("/root/alpha")],
            Some(VfsPath::local("/dest")),
        ));
        if let Some(status) = app.jobs.status_mut(id) {
            status.files_total = 200;
            status.files_done = 10;
        }

        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert!(!app.should_quit, "the transfer prompt is unconditional");
        assert_eq!(app.focus, Focus::Dialog(DialogId::ConfirmQuit));
        let lines = crate::ops::running_job_lines(app.jobs.rows());
        assert!(
            lines.iter().any(|l| l.contains("Copying")),
            "the prompt names what is running: {lines:?}"
        );

        // And this one defaults to `Cancel`: `Enter` on it does not quit.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.should_quit);
    }

    #[test]
    fn a_finished_job_is_not_something_to_be_warned_about() {
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.config.ui.confirm_exit = false;
        let id = app.request_job(crate::ops::JobSpec::new(
            JobKind::Copy,
            vec![VfsPath::local("/root/alpha")],
            Some(VfsPath::local("/dest")),
        ));
        let summary = Box::new(crate::ops::JobSummary {
            kind: JobKind::Copy,
            files_done: 1,
            dirs_done: 0,
            bytes_done: 0,
            skipped: 0,
            failures: Vec::new(),
            cancelled: false,
            elapsed: std::time::Duration::ZERO,
            sized: Vec::new(),
            differing: Vec::new(),
            first_difference: None,
        });
        if let Some(status) = app.jobs.status_mut(id) {
            status.finished = Some(summary);
        }
        press(&mut app, KeyCode::F(10), KeyModifiers::NONE);
        assert!(app.should_quit, "nothing is still running");
    }

    #[test]
    fn a_dialog_consumes_every_key_the_panel_would_have_taken() {
        // a modal dialog "consumes all input".
        let mut app = app_with(vec![Entry::file("alpha"), Entry::file("beta")]);
        app.push_dialog(Box::new(MessageDialog::line("Done", "ok")));

        // `j` would type into the quick-search buffer on a panel, and `Tab`
        // would switch panels. Neither reaches the panel.
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.left.quick.is_empty(), "nothing typed into the panel");
        assert_eq!(app.active_side, Side::Left, "and no panel switch");
        assert!(app.dialog_is_open());

        // `Enter` closes a message box.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.dialog_is_open());
        assert_eq!(app.focus, Focus::Panel(Side::Left));
    }

    #[test]
    fn esc_in_a_dialog_cancels_it_rather_than_clearing_the_panel_marks() {
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.mark_all();
        assert_eq!(app.left.active_tab().marks.len(), 1);

        app.push_dialog(Box::new(InputDialog::new(
            DialogId::Mkdir,
            "Create directory",
            "Name:",
            "",
        )));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.dialog_is_open());
        assert_eq!(
            app.left.active_tab().marks.len(),
            1,
            "Esc belonged to the dialog"
        );
    }

    #[test]
    fn accepting_the_mkdir_prompt_queues_a_job_under_the_panels_directory() {
        let mut app = app_with(vec![Entry::file("alpha")]);
        app.push_dialog(Box::new(InputDialog::new(
            DialogId::Mkdir,
            "Create directory",
            "Name:",
            "",
        )));
        for c in "photos".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(!app.dialog_is_open());
        let queued = app.take_pending_jobs();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].spec.kind, JobKind::Mkdir);
        assert_eq!(
            queued[0].spec.dest.as_ref().map(ToString::to_string),
            Some("/root/photos".to_string())
        );
    }

    #[test]
    fn ctrl_l_sizes_every_marked_directory_and_nothing_else() {
        // `Ctrl+L` walks every marked directory.
        let mut app = app_with(vec![
            Entry::parent_entry(),
            Entry::dir("one"),
            Entry::dir("two"),
            Entry::file("a.txt"),
        ]);
        {
            let tab = app.left.active_tab_mut();
            tab.marks.insert("one".to_string());
            tab.marks.insert("two".to_string());
            tab.marks.insert("a.txt".to_string());
        }
        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);

        let queued = app.take_pending_jobs();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].spec.kind, JobKind::Size);
        let mut sources: Vec<String> = queued[0]
            .spec
            .sources
            .iter()
            .map(ToString::to_string)
            .collect();
        sources.sort();
        assert_eq!(
            sources,
            vec!["/root/one", "/root/two"],
            "files are not walked"
        );
    }

    #[test]
    fn ctrl_l_says_so_rather_than_doing_nothing_when_a_tree_is_already_sized() {
        let mut app = app_with(vec![Entry::parent_entry(), Entry::dir("one")]);
        app.left.active_tab_mut().marks.insert("one".to_string());
        app.jobs
            .sizes
            .insert(VfsPath::local("/root/one"), crate::ops::TreeStats::ZERO);

        press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.take_pending_jobs().is_empty(), "the cache answered");
        assert!(
            app.message
                .as_deref()
                .unwrap_or_default()
                .contains("already sized"),
            "{:?}",
            app.message
        );
    }

    #[test]
    fn space_sizes_the_directory_under_the_cursor_and_insert_never_does() {
        // "Only `Space` sizes a directory."
        let mut app = app_with(vec![Entry::dir("tree"), Entry::file("a.txt")]);
        press(&mut app, KeyCode::Insert, KeyModifiers::NONE);
        assert!(
            app.take_pending_jobs().is_empty(),
            "Insert marks without walking"
        );

        app.move_cursor_to(0);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        let queued = app.take_pending_jobs();
        assert_eq!(queued.len(), 1, "Space walks it");
        assert_eq!(queued[0].spec.kind, JobKind::Size);
        assert_eq!(
            queued[0].spec.sources.first().map(ToString::to_string),
            Some("/root/tree".to_string())
        );

        // And a file under the cursor is not a walk.
        app.move_cursor_to(1);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.take_pending_jobs().is_empty());
    }

    #[test]
    fn space_still_extends_a_running_quick_search_rather_than_sizing() {
        // filenames contain spaces.
        let mut app = app_with(vec![Entry::dir("my tree")]);
        press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(app.left.quick.buffer, "my ");
        assert!(app.take_pending_jobs().is_empty(), "nothing was sized");
        assert!(app.left.active_tab().marks.is_empty(), "and nothing marked");
    }

    #[test]
    fn plus_opens_the_mask_prompt_and_the_answer_marks_by_wildcard() {
        // `+` "opens a mask prompt, default `*`".
        let mut app = app_with(vec![
            Entry::parent_entry(),
            Entry::dir("src"),
            Entry::file("main.rs"),
            Entry::file("lib.rs"),
            Entry::file("Cargo.toml"),
        ]);
        press(&mut app, KeyCode::Char('+'), KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Dialog(DialogId::SelectMask));
        assert_eq!(
            app.top_dialog().map(|d| d.title()),
            Some("Select by mask".to_string())
        );

        // The default `*` is offered; type over it.
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        for c in "*.rs".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(!app.dialog_is_open());
        let mut marked: Vec<String> = app.left.active_tab().marks.iter().cloned().collect();
        marked.sort();
        assert_eq!(marked, vec!["lib.rs", "main.rs"]);
        // Marking by mask never walks a tree.
        assert!(app.take_pending_jobs().is_empty());
    }

    #[test]
    fn the_last_mask_is_remembered_and_offered_to_the_next_prompt() {
        // "remembered per session and offered as the default on
        // the next open" - across `+` and `-` alike.
        let mut app = app_with(vec![Entry::file("main.rs"), Entry::file("notes.bak")]);
        press(&mut app, KeyCode::Char('+'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        for c in "*.rs".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.masks.last, "*.rs");

        press(&mut app, KeyCode::Char('-'), KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Dialog(DialogId::UnselectMask));
        // `Enter` straight away: the remembered mask, unmarking this time.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.left.active_tab().marks.is_empty());
        assert!(
            app.message
                .as_deref()
                .unwrap_or_default()
                .contains("unmarked"),
            "{:?}",
            app.message
        );
    }

    #[test]
    fn cancelling_the_mask_prompt_changes_nothing() {
        let mut app = app_with(vec![Entry::file("main.rs")]);
        press(&mut app, KeyCode::Char('+'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.dialog_is_open());
        assert!(app.left.active_tab().marks.is_empty());
        assert_eq!(app.masks.last, "*", "and the default is still the default");
    }

    // ------------------------------------------------ the design, `F4` ---

    #[test]
    fn f4_queues_the_editor_and_touches_nothing_else() {
        // The keystroke plans; the event loop performs. Nothing
        // here may reach the terminal or the filesystem,
        // so what a test can see is the queued
        // command - and that is exactly what `ops::editor::service` consumes.
        let mut app = app_with(vec![Entry::file("notes.txt")]);
        // Named explicitly, because the default is **empty** and the design
        // then consults `$VISUAL` and `$EDITOR` - so a bare default would make
        // this assertion say what the developer's shell profile says. Which
        // program the chain picks is `ops::editor`'s to pin; what is asserted
        // here is that `F4` routes to it and touches nothing else.
        app.config.editor.command = "nano".to_string();
        press(&mut app, KeyCode::F(4), KeyModifiers::NONE);

        let queued = app.handoff.external.clone().expect("F4 queues the editor");
        assert_eq!(queued.program, "nano");
        assert_eq!(queued.args, vec!["/root/notes.txt".to_string()]);
        assert_eq!(queued.follow, Some(Side::Left));
        assert_eq!(queued.cwd, Some(VfsPath::local("/root")));
        assert!(
            app.draft.sources.is_empty(),
            "F4 creates nothing: only Shift+F4 does"
        );
        assert_eq!(
            app.left.active_tab().pending_select.as_deref(),
            Some("notes.txt"),
            "the cursor stays on the edited file across the re-read"
        );
        assert!(!app.dialog_is_open());
    }

    #[test]
    fn f4_on_a_directory_and_on_an_empty_panel_says_so() {
        let mut app = app_with(vec![Entry::dir("photos")]);
        press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        assert!(app.handoff.external.is_none());
        assert!(
            app.message
                .as_deref()
                .unwrap_or_default()
                .contains("photos"),
            "{:?}",
            app.message
        );

        let mut app = app_with(Vec::new());
        press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        assert!(app.handoff.external.is_none());
        assert_eq!(app.message.as_deref(), Some("nothing to edit"));
    }

    #[test]
    fn f4_inside_something_that_is_not_the_local_filesystem_says_what_to_do_instead() {
        // the temp-file round trip - download, edit, upload on
        // change, with the mtime check - serves archives and remotes with one
        // piece of code and has not been written. Refused before an editor is
        // started, not after twenty minutes of editing, and the message names
        // **no release**: v0.7 has shipped without it and the design has no
        // line after v0.7.
        let mut app = app_with(vec![Entry::file("member.txt")]);
        app.left.active_tab_mut().path = VfsPath::new(crate::vfs::BackendKind::List, "/results");
        press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        assert!(app.handoff.external.is_none());
        let message = app.message.as_deref().unwrap_or_default();
        assert!(message.contains("F5"), "{message}");
        assert!(!message.contains("v0."), "{message}");
    }

    #[test]
    fn shift_f4_prompts_and_then_queues_both_the_creation_and_the_editor() {
        // "prompts for a name, creates the file, then opens the
        // editor". The creation is the event loop's, so what the prompt leaves
        // behind is the file to create beside the command to run.
        let mut app = app_with(vec![Entry::file("notes.txt")]);
        app.config.editor.command = "nano".to_string();
        press(&mut app, KeyCode::F(4), KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Dialog(DialogId::EditNew));
        assert!(app.handoff.external.is_none(), "the prompt comes first");

        for c in "draft.md".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert!(!app.dialog_is_open());
        let queued = app.handoff.external.clone().expect("the editor is queued");
        assert_eq!(queued.args, vec!["/root/draft.md".to_string()]);
        assert_eq!(
            app.draft.sources,
            vec![VfsPath::local("/root/draft.md")],
            "the file to create before the editor runs"
        );
        assert_eq!(
            app.left.active_tab().pending_select.as_deref(),
            Some("draft.md")
        );
    }

    #[test]
    fn shift_f4_on_a_backend_that_cannot_be_written_refuses_before_the_prompt() {
        let mut app = app_with(vec![Entry::file("member.txt")]);
        app.left.active_tab_mut().path = VfsPath::new(crate::vfs::BackendKind::List, "/results");
        press(&mut app, KeyCode::F(4), KeyModifiers::SHIFT);
        assert!(!app.dialog_is_open(), "asking for a name first is the bug");
        assert!(app.handoff.external.is_none());
        let message = app.message.as_deref().unwrap_or_default();
        assert!(message.contains("F5"), "{message}");
        assert!(
            !message.contains("v0."),
            "no release is named: v0.7 has shipped without the \
             round trip: {message}"
        );
    }
}
