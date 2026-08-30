//! The `F1` help view.
//!
//! Every page is **generated from the active keymap**, never written by hand:
//! "A user who rebinds `Ctrl+D` sees their binding, not the shipped default.
//! Any hand-maintained key list is wrong the moment someone edits
//! `keymap.toml`; generating it is also less work."
//!
//! # One document, several sections
//!
//! the design lists "further pages" and, in the same breath, "the help view
//! uses the same viewer machinery, so quick find (`F7`, `/`) works in it.
//! Searching the keyboard reference for `rename` is the fastest path to
//! `Shift+F6` and `Ctrl+M`."
//!
//! A [`crate::viewer::Viewer`] is one buffer. A page-turning UI over several
//! buffers would mean either a find that searches only the page you are on -
//! which contradicts that sentence - or new viewer machinery the design does not
//! describe. So this is **one document with headed sections**, and
//! [`HelpTopic`] resolves to the line a section starts on.
//!
//!
//! # What is generated and could not have been written
//!
//! * The `Ctrl+<n>` block is built from `panel.columns.order`, so it reads
//!   `Ctrl+3  Sort by Size` only where size is the third column
//!   (invariant I14).
//! * Undeliverable bindings are marked and their fallback shown beside them
//!   (invariant I15). `Ctrl+H` in particular: on a
//!   legacy terminal the page shows it as unavailable and shows `Alt+.` in its
//!   place. All of that lives in [`crate::config::Keymap::describe`], so this
//!   module and the menu cannot spell a key two ways.
//! * Which group an action belongs in is read off the keymap's context tables
//!   rather than written down, so an action rebound from `[panel]` into
//!   `[global]` moves on the page by itself.

use crate::app::App;
use crate::config::{KeyContext, Keymap};
use crate::input::{Action, Binding, DialogId, KeyCode};
use crate::panel::{ColumnId, Side};

/// Which page `F1` was pressed for (the context sensitivity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    /// From a panel: the keyboard reference, which is the first and default
    /// page.
    Keyboard,
    /// From the viewer: the keys, which
    /// [`crate::ui::help::viewer_page`] already generates.
    Viewer,
    /// From a dialog: that dialog's section.
    Dialog(DialogId),
    /// From the console: what the design gives the shell and what it keeps.
    Console,
}

/// The heading the viewer section starts with.
///
/// It is [`crate::ui::help::viewer_page`]'s own first line, and it is a constant
/// here so [`page`] can find the line without searching for prose that might be
/// reworded.
const VIEWER_HEADING: &str = "Viewer keys";

/// The heading the console section starts with.
const CONSOLE_HEADING: &str = "Console";

/// The heading the dialogs section starts with.
const DIALOGS_HEADING: &str = "Dialogs";

/// How wide the key column of a generated table is, at most.
///
/// A binding marked `(unavailable)` on a legacy terminal is long; letting it
/// set the column width would indent every other row off the screen at 60
/// columns (the floor).
const KEY_COLUMN_MAX: usize = 34;

/// The dialogs the `Dialogs` section documents, in the order they appear.
///
/// A list beside the exhaustive `match` in [`dialog_help`] rather than instead
/// of it: the `match` is what makes a new [`DialogId`] a compile error, and
/// this is what puts the paragraphs in a sensible order rather than in
/// declaration order. Adding a variant means adding an arm there and an entry
/// here.
const DIALOG_ORDER: &[DialogId] = &[
    DialogId::Help,
    DialogId::Message,
    DialogId::ConfirmQuit,
    DialogId::GotoPath,
    DialogId::GotoOffset,
    DialogId::Mkdir,
    DialogId::MkdirForTarget,
    DialogId::Rename,
    DialogId::EditNew,
    DialogId::SelectMask,
    DialogId::UnselectMask,
    DialogId::CopyMove,
    DialogId::Conflict,
    DialogId::ConfirmDelete,
    DialogId::ConfirmRewrite,
    DialogId::Progress,
    DialogId::JobQueue,
    DialogId::JobSummary,
    DialogId::Pack,
    DialogId::Resize,
    DialogId::MultiRename,
    DialogId::RenameResult,
    DialogId::Find,
    DialogId::SaveSearch,
    DialogId::ConfirmRemoteSearch,
    DialogId::Drive(Side::Left),
    DialogId::Drive(Side::Right),
    DialogId::Hotlist,
    DialogId::HotlistAdd,
    DialogId::Menu,
    DialogId::ContextMenu,
    DialogId::Execute,
    DialogId::OpenWith,
    DialogId::History,
    DialogId::Theme,
    DialogId::Template,
    DialogId::FileSummary,
    DialogId::Connect,
    DialogId::HostForm,
    DialogId::RemoteSecret,
    DialogId::HostKey,
    DialogId::HostKeyChanged,
    DialogId::ConfirmDisconnect,
];

/// The whole help document, and the line the topic starts on.
///
/// **One document, not several viewers** - see the module documentation and
/// the design. `None` means "open at the top", which is what
/// [`HelpTopic::Keyboard`] asks for: the design makes the keyboard reference
/// "its first and default page", so there is nothing to seek to.
pub fn page(app: &App, topic: HelpTopic) -> (String, Option<u64>) {
    let mut lines: Vec<String> = Vec::new();
    extend(&mut lines, &keyboard_page(app));
    extend(&mut lines, &getting_started());

    let dialogs_at = lines.len();
    extend(&mut lines, &dialogs_page());
    extend(&mut lines, &console_page());
    extend(&mut lines, &remote_page());
    extend(&mut lines, &configuration_page(app));
    extend(&mut lines, &about_page());

    let body = lines.join("\n");
    let at = match topic {
        HelpTopic::Keyboard => None,
        HelpTopic::Viewer => line_starting_with(&lines, VIEWER_HEADING),
        HelpTopic::Console => line_starting_with(&lines, CONSOLE_HEADING),
        HelpTopic::Dialog(id) => dialog_offset(id)
            .map(|offset| offset.saturating_add(dialogs_at))
            .and_then(|line| u64::try_from(line).ok()),
    };
    (body, at)
}
/// The positional sort actions [`sort_block`] will name a column for.
///
/// It walks `panel.columns.order` and stops there, so a binding past the last
/// configured column is not in the block and must stay in the tables.
fn positional_sort_keys(app: &App) -> Vec<Action> {
    let covered = app
        .config
        .panel
        .columns
        .order
        .len()
        .min(SORT_BY_COLUMN.len());
    SORT_BY_COLUMN
        .iter()
        .take(covered)
        .chain(SORT_SECONDARY.iter().take(covered))
        .copied()
        .collect()
}

/// the keyboard reference, grouped the way the design groups its
/// keys: function keys, control keys, panel keys, command-line keys, viewer
/// keys.
///
/// Bindings the current terminal cannot deliver are marked and the working
/// fallback is shown next to them. The
/// `Ctrl+<n>` block is generated from `panel.columns.order`, so it names the
/// columns this configuration actually has.
pub fn keyboard_page(app: &App) -> String {
    let keymap = &app.keymap;
    let enhanced = app.keyboard.enhanced;
    let mut out = String::from("Keyboard reference\n\n");
    // First thing on the page, because this document is long and the fastest
    // route into it is the quick find the viewer already has. The keys are
    // read off the keymap like every other key on this page: a note that
    // named Ctrl+F on a machine where quick find has been rebound would be
    // the one false line in the document whose whole job is to be true.
    out.push_str(&format!(
        "This page is searchable: {} opens quick find, and the find bar's own\n\
         keys walk the matches from there.\n\n",
        bindings_text(keymap, Action::QuickFind, enhanced)
    ));
    out.push_str(
        "Generated from the keymap this session is running with, so what is\n\
         written here is what your keys do. Rebind anything in keymap.toml\n\
 and this page changes with it.\n\n",
    );
    if !enhanced {
        out.push_str(
            "This terminal does not have the Kitty keyboard protocol, so some\n\
 keys cannot reach the program at all. They are\n\
             marked below, and the binding that does work is printed beside\n\
             them.\n\n",
        );
    }

    let mut grouped: Vec<(Group, Vec<Action>)> = Group::ALL
        .iter()
        .map(|group| (*group, Vec::new()))
        .collect();
    for action in Action::ALL {
        // The positional sort keys are left out of the tables on purpose:
        // `sort_block` below prints the same bindings with the column each one
        // actually sorts by, read from `panel.columns.order`. Listing them
        // here as well gave the reader the same keys twice, once labelled
        // "Sort by column 3" and once "Sort by Size" - and the bare form is
        // the useless half, since the number means nothing without the order.
        //
        // **Only the ones that block will actually print.** It stops at the
        // configured column count, so with the default five columns `Ctrl+6`
        // to `Ctrl+9` are bound and named nowhere in it. Those keep their
        // generic row here rather than vanishing from the document.
        if positional_sort_keys(app).contains(action) {
            continue;
        }
        let group = group_of(keymap, *action);
        if let Some(slot) = grouped
            .iter_mut()
            .find(|(candidate, _)| *candidate == group)
        {
            slot.1.push(*action);
        }
    }

    for (group, actions) in &grouped {
        if actions.is_empty() {
            continue;
        }
        out.push_str(group.heading());
        out.push('\n');
        out.push('\n');
        out.push_str(&table(keymap, group.context(), actions, enhanced));
        out.push('\n');
        if *group == Group::Control {
            out.push_str(&sort_block(app));
            out.push('\n');
        }
    }

    // the fifth group. The viewer's own page is already generated
    // from the keymap, already carries the rules a table cannot state, and is
    // what `F1` inside the viewer opens - so it is included rather than
    // re-derived, which is what keeps one copy of it in the document.
    //
    out.push_str(&viewer_page(keymap, enhanced));
    out.push('\n');
    out
}

/// the "Getting started": the two-panel model, marking, the
/// command-line focus rules.
pub fn getting_started() -> String {
    let mut out = String::from("Getting started\n\n");
    out.push_str(
        "Two panels, one of them active. Tab moves between them. Almost every\n\
         operation reads the active panel as the source and the other as the\n\
         target: F5 copies from here to there, F6 moves, and swapping the two\n\
         with Ctrl+U reverses that without retyping anything.\n\n",
    );
    out.push_str(
        "Marking is not the cursor. Insert marks the entry under the cursor and\n\
         steps down, Space marks it and sizes a directory, and + - * mark,\n\
         unmark and invert by wildcard. An operation acts on the marks when\n\
         there are any and on the entry under the cursor when there are none,\n\
 so nothing is ever done to a selection you cannot see.\n\n",
    );
    out.push_str(
        "Typing in a panel is quick search, not a command: the cursor jumps to\n\
         the first entry that matches what you have typed, Backspace steps back\n\
         through the matches, and Esc clears the search before it clears the\n\
 marks.\n\n",
    );
    out.push_str(
        "The command line is below the panels and keeps its own caret. Typing a\n\
         character with the panel focused starts a quick search; typing where\n\
         the command line has focus types there. One focus rule is worth\n\
         knowing: Up and Down always belong to the panel, which is why history\n\
         is on Ctrl+Up and Ctrl+Down.\n\n",
    );
    out
}

/// the "Dialogs": `Tab`, `Esc`, `Enter`, and the `Alt`
/// mnemonics with the four reserved letters, then one paragraph per dialog.
pub fn dialogs_page() -> String {
    let mut out = String::from(DIALOGS_HEADING);
    out.push_str("\n\n");
    out.push_str(
        "Every dialog works the same way. Tab and Shift+Tab move between its\n\
         controls, Enter accepts, Esc cancels, and a dialog consumes all input\n\
         while it is open - no key leaks through to the panel behind it\n\
.\n\n",
    );
    out.push_str(
        "Alt with a letter jumps straight to a control, and the letter is drawn\n\
         underlined in that control's own label so it never has to be guessed.\n\
         An accelerator never turns anything off: Alt on a checkbox switches it\n\
         on and leaves it on, because a key that toggled would make a repeated\n\
 keystroke destructive.\n\n",
    );
    out.push_str(
        "Four letters mean the same thing everywhere they appear: n is Cancel\n\
         or No, c is Close, h is Help and o is OK. A dialog that spends one of\n\
         them on something else has no button of that kind at all, and every\n\
         such case is written down and tested.\n\n",
    );
    out.push_str(
        "Alt with a digit is never a mnemonic: it belongs to the tab strip, so\n\
 the two can never collide.\n\n",
    );
    for id in DIALOG_ORDER {
        let (title, body) = dialog_help(*id);
        out.push_str(title);
        out.push('\n');
        for line in body.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// the context-sensitive `F1` in the console.
///
/// Not one of the five "further pages", and here because the other sentence
/// requires it: "`F1` in the console explains the console." A topic with
/// nowhere to land would be a page that silently opened at the top.
pub fn console_page() -> String {
    let mut out = String::from(CONSOLE_HEADING);
    out.push_str("\n\n");
    out.push_str(
        "The command line is the shell's own. hcmd starts one shell on a pty at\n\
         startup and keeps it: what you type on the command line is what the\n\
         shell sees, prompt and all, and Ctrl+O gives that shell the whole\n\
 screen and takes it back.\n\n",
    );
    out.push_str(
        "So history, completion, Ctrl+R, and vi or emacs bindings are whatever\n\
         you have configured in your shell, not reimplementations of them.\n\
         There is no history file of this program's own and nothing is pushed\n\
         to one: one history that the shell already maintains beats two that\n\
 disagree. Alt+F8 says so rather than opening an empty\n\
         list, and opens a list only where there is no shell to ask.\n\n",
    );
    out.push_str(
        "A few keys are intercepted before the shell sees them, and only a few:\n\
 Up and Down, which belong to the panel, Ctrl+Enter,\n\
         which composes a command line from the entry under the cursor, and\n\
         Ctrl+O itself. In the full-screen console the rule inverts and\n\
         everything is forwarded except the key that gets you out and the two\n\
         that walk the scrollback, so F5 does not copy files at a vim running\n\
         in there.\n\n",
    );
    out.push_str(
        "The shell's directory and the panel's are kept in step, in both\n\
 directions, and a command run from the command line\n\
         switches the screen to the console only when the command is still\n\
 using it.\n\n",
    );
    out
}

/// the "Remote connections".
pub fn remote_page() -> String {
    let mut out = String::from("Remote connections\n\n");
    out.push_str(
        "Ctrl+F connects the active panel to a remote host and, on a panel that\n\
         is already connected, offers to disconnect it. SFTP over SSH, FTP and\n\
         FTPS are the protocols; SFTP is the default and the one to prefer.\n\n",
    );
    out.push_str(
        "The connect dialog takes either a quick-connect line - sftp://user@host\n\
         - or a saved host from the book. The book is hosts.toml beside the\n\
         other configuration files, and F4 and F8 in that dialog edit and\n\
         delete its entries.\n\n",
    );
    out.push_str(
        "Authentication is tried in order: the agent, then a key file, then a\n\
         password. A password is never written to a file. It goes to the system\n\
         keyring when you tick the box that offers it, and where no keyring is\n\
         available the dialog says so rather than silently forgetting\n\
.\n\n",
    );
    out.push_str(
        "An unknown host key is shown with its fingerprint and has to be\n\
         accepted before anything is sent. A key that has changed is a message\n\
         and not a question: nothing connects until known_hosts is corrected by\n\
 hand.\n\n",
    );
    out.push_str(
        "A connected panel is a panel. It sorts, filters, marks and copies like\n\
         any other, and the operations that cannot cross the network - running\n\
         a program, editing in place - refuse with the reason rather than\n\
 pretending.\n\n",
    );
    out
}

/// the "Configuration": the file locations of the design and the
/// main options, with the rule stated.
pub fn configuration_page(app: &App) -> String {
    let mut out = String::from("Configuration\n\n");
    let dir = crate::config::paths::config_dir().map_or_else(
        |_| "~/.config/holoscommander".to_string(),
        |d| d.display().to_string(),
    );
    out.push_str(&format!("Everything lives in {dir}:\n\n"));
    out.push_str(
        "  config.toml    every option below, and the [[open.handlers]] rules\n\
 \x20 keymap.toml every binding, in the tables resolves\n\
         \x20 hotlist.toml   the directory hotlist, in the order you built it\n\
         \x20 hosts.toml     saved remote hosts, never any password\n\
         \x20 searches.toml  saved searches\n\
         \x20 themes/        one file per theme; ui.theme names one\n\n",
    );
    out.push_str(
        "A generated file is a reference, not an override. config.toml and\n\
         keymap.toml are written fully commented out on first run, so the\n\
         defaults keep coming from the program and a file written the day you\n\
         installed it cannot freeze you at that day's defaults. Uncomment a\n\
 line to change it. hotlist.toml, hosts.toml and\n\
         searches.toml are the exception, and for the opposite reason: they\n\
         hold lists you built rather than defaults you might override, so the\n\
         program writes them in full and they do not exist until you add\n\
         something.\n\n",
    );
    out.push_str(
        "A bad value is never fatal. An unknown key, a value that does not\n\
         parse, or a file that cannot be read is collected as a warning and\n\
 shown, and the default is used.\n\n",
    );
    out.push_str("What this session is running with:\n\n");
    let cfg = &app.config;
    let rows: [(&str, String); 10] = [
        ("ui.theme", cfg.ui.theme.clone()),
        ("ui.ascii_borders", cfg.ui.ascii_borders.to_string()),
        ("panel.show_hidden", cfg.panel.show_hidden.to_string()),
        (
            "panel.quick_search",
            format!("{:?}", cfg.panel.quick_search).to_lowercase(),
        ),
        (
            "panel.quick_search_case",
            format!("{:?}", cfg.panel.quick_search_case).to_lowercase(),
        ),
        (
            "panel.columns.order",
            cfg.panel
                .columns
                .order
                .iter()
                .map(ColumnId::id)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "open.execute",
            format!("{:?}", cfg.open.execute).to_lowercase(),
        ),
        (
            "open.execute_in",
            format!("{:?}", cfg.open.execute_in).to_lowercase(),
        ),
        (
            "open.handlers",
            format!("{} rule(s)", cfg.open.handlers.len()),
        ),
        (
            "terminal keyboard protocol",
            if app.keyboard.enhanced {
                "enhanced".to_string()
            } else {
                "legacy: some keys cannot arrive".to_string()
            },
        ),
    ];
    let width = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    for (key, value) in &rows {
        out.push_str("  ");
        out.push_str(key);
        for _ in 0..width.saturating_sub(key.chars().count()) {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(value);
        out.push('\n');
    }
    out.push('\n');
    if !cfg.warnings.is_empty() || !app.keymap.warnings.is_empty() {
        out.push_str("Warnings from this session's configuration:\n\n");
        for warning in cfg.warnings.iter().chain(app.keymap.warnings.iter()) {
            out.push_str("  ");
            out.push_str(warning);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// the "About": version, build, and the crate versions this was
/// built against.
pub fn about_page() -> String {
    let mut out = String::from("About\n\n");
    out.push_str(&format!(
        "  {} {}\n  {} build\n\n",
        crate::BIN_NAME,
        crate::VERSION,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    ));
    out.push_str("A Total Commander alternative for the terminal. The default keys are\n");
    out.push_str("mapped identical to Total Commander, so your fingers already know them.\n\n");
    out.push_str("Built against:\n\n");
    for (name, version, what) in CRATE_VERSIONS {
        out.push_str(&format!("  {name:<26}{version:<10}{what}\n"));
    }
    out.push('\n');
    out.push_str(
        "Nothing here shells out for functionality: archives, search, device\n\
         enumeration and file associations are all in-process crates\n\
. The one process this program starts on purpose is the\n\
         one you asked it to start.\n\n",
    );
    // The one page in the program that names a version is where the key that
    // asks about a newer one belongs.
    out.push_str(
        "Checking for a newer release asks GitHub for the latest tag and says\n\
         so once per version, with the command that installs it. It downloads\n\
         nothing and never replaces this binary - the install command is\n\
         yours to run. Which version you have been told about is remembered\n\
         in update.toml; delete that file to hear it again.\n\n",
    );
    out
}

/// The crates the design pins, for [`about_page`].
///
/// Hand-maintained beside `Cargo.toml`, because a dependency's version is not
/// available to the program at runtime without a build script and the design
/// rule 5 would not spend one on this. It lists the crates that decide
/// behaviour a user would report a bug against, not the whole tree.
const CRATE_VERSIONS: &[(&str, &str, &str)] = &[
    ("ratatui", "0.30.2", "rendering"),
    ("crossterm", "0.29.0", "the terminal and its keys"),
    ("tokio", "1.53.1", "the event loop"),
    ("portable-pty / vt100", "0.9.0 / 0.16.2", "the console"),
    ("syntect / two-face", "5.3.0 / 0.5.2", "viewer highlighting"),
    (
        "encoding_rs / chardetng",
        "0.8.35 / 1.0.0",
        "viewer encodings",
    ),
    ("zip / tar / sevenz-rust2", "8.6 / 0.4 / 0.22", "archives"),
    ("ignore / grep-searcher", "0.4.33 / 0.1.17", "search"),
    ("russh / russh-sftp", "0.63.1 / 2.4.0", "SFTP"),
    ("suppaftp", "10.0.2", "FTP and FTPS"),
    ("keyring", "4.1.6", "stored passwords"),
    ("sysinfo", "0.39.6", "the device picker"),
    ("open", "5.4.2", "the desktop's default application"),
    ("mime_guess / infer", "2.0.5 / 0.22.0", "file associations"),
    (
        "freedesktop-desktop-entry",
        "0.8.2",
        "the Open with... chooser",
    ),
];

// ----------------------------------------------------------- internals ------

/// Which of the groups an action's keys belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// a global action whose binding is a function key.
    Function,
    /// every other global action.
    Control,
    /// bound in the `[panel]` table.
    Panel,
    /// bound in the `[cmdline]` table.
    CmdLine,
    /// bound in the `[viewer]` table. Rendered by
    /// [`crate::ui::help::viewer_page`] rather than as a table here, so this group
    /// is collected and then dropped - see [`keyboard_page`].
    Viewer,
    /// Bound nowhere in this keymap.
    Unbound,
}

impl Group {
    /// The groups, in the order the design introduces them.
    ///
    /// [`Group::Viewer`] is absent: those actions are printed by
    /// [`crate::ui::help::viewer_page`], which [`keyboard_page`] appends whole, and
    /// listing them twice in one searchable document would put two hits behind
    /// every viewer key.
    const ALL: &'static [Self] = &[
        Self::Function,
        Self::Control,
        Self::Panel,
        Self::CmdLine,
        Self::Unbound,
    ];

    /// The heading this group is printed under.
    const fn heading(self) -> &'static str {
        match self {
            Self::Function => "Function keys",
            Self::Control => "Control keys",
            Self::Panel => "Panel keys",
            Self::CmdLine => "Command-line keys",
            Self::Viewer => "Viewer keys",
            Self::Unbound => "Not bound in this keymap",
        }
    }

    /// The context this group's bindings are described in.
    const fn context(self) -> KeyContext {
        match self {
            Self::Panel | Self::Function | Self::Control | Self::Unbound => KeyContext::Panel,
            Self::CmdLine => KeyContext::CmdLine,
            Self::Viewer => KeyContext::Viewer,
        }
    }
}

/// Which group an action belongs in, read off the keymap rather than written
/// down.
///
/// A binding that appears in **every** context's list is a `[global]` one; a
/// binding that appears in only some of them is that context's. So an action
/// with any context-specific binding belongs to that context's group, and one
/// with only global bindings is split by the question - is its key a
/// function key?
///
/// An action bound in two contexts lands in the first of [`KeyContext::ALL`],
/// which is the order `keymap.toml`'s tables are written in. Nothing in the
/// shipped keymap is in that position, and the alternative - printing it twice -
/// would put two hits behind one key in a document the design quick find has to
/// work in.
fn group_of(keymap: &Keymap, action: Action) -> Group {
    let per: Vec<(KeyContext, Vec<Binding>)> = KeyContext::ALL
        .iter()
        .map(|ctx| (*ctx, keymap.bindings_in(*ctx, action)))
        .collect();
    let is_global = |binding: &Binding| per.iter().all(|(_, list)| list.contains(binding));
    for (ctx, list) in &per {
        if !list.iter().any(|binding| !is_global(binding)) {
            continue;
        }
        return match ctx {
            KeyContext::Panel => Group::Panel,
            KeyContext::CmdLine => Group::CmdLine,
            KeyContext::Viewer => Group::Viewer,
            // A `[dialog]` or `[console]` binding is not a group of its own in
            // `Esc` in a dialog is documented in the Dialogs
            // section, and the console's keys in the Console one.
            KeyContext::Dialog | KeyContext::Console => Group::Control,
        };
    }
    let global = keymap.bindings_for(action);
    if global.is_empty() {
        return Group::Unbound;
    }
    if global
        .iter()
        .any(|binding| matches!(binding.first().code, KeyCode::F(_)))
    {
        Group::Function
    } else {
        Group::Control
    }
}

/// One group's table: the binding, padded, then the action's description.
fn table(keymap: &Keymap, ctx: KeyContext, actions: &[Action], enhanced: bool) -> String {
    let rendered: Vec<(String, &'static str)> = actions
        .iter()
        .map(|action| {
            (
                keymap.describe(ctx, *action, enhanced),
                action.description(),
            )
        })
        .collect();
    let width = rendered
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0)
        .min(KEY_COLUMN_MAX);
    let mut out = String::new();
    for (keys, description) in &rendered {
        out.push_str("  ");
        out.push_str(keys);
        for _ in 0..width.saturating_sub(keys.chars().count()) {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(description);
        out.push('\n');
    }
    out
}

/// `Ctrl+<n>` addresses the n-th **configured** column.
///
/// > The `F1` keyboard reference prints the live mapping, generated from the
/// > active column order, so it is correct for whatever layout is configured.
///
/// This is invariant I14, and it is the block a hand-written page could not
/// have contained: change `panel.columns.order` and the words after each key
/// change with nothing rebound.
fn sort_block(app: &App) -> String {
    let mut out = String::from("\nSorting by column\n\n");
    let order = &app.config.panel.columns.order;
    let rows: Vec<(String, String)> = order
        .iter()
        .take(SORT_BY_COLUMN.len())
        .enumerate()
        .flat_map(|(index, column)| {
            let primary = SORT_BY_COLUMN.get(index).copied();
            let secondary = SORT_SECONDARY.get(index).copied();
            [
                primary.map(|action| {
                    (
                        app.keymap
                            .describe(KeyContext::Panel, action, app.keyboard.enhanced),
                        format!("Sort by {}", column.header()),
                    )
                }),
                secondary.map(|action| {
                    (
                        app.keymap
                            .describe(KeyContext::Panel, action, app.keyboard.enhanced),
                        format!("Secondary sort by {}", column.header()),
                    )
                }),
            ]
        })
        .flatten()
        .collect();
    let width = rows
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0)
        .min(KEY_COLUMN_MAX);
    for (keys, description) in &rows {
        out.push_str("  ");
        out.push_str(keys);
        for _ in 0..width.saturating_sub(keys.chars().count()) {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(description);
        out.push('\n');
    }
    out.push_str(
        "\n  The same key again reverses that sort. A column that is hidden\n\
         \x20 because the panel is narrow still has its number: the mapping is\n\
         \x20 the configured order, not what happens to be on the screen\n\
 \x20.\n",
    );
    out
}

/// `Ctrl+1`..`Ctrl+9`, in order, so a column index can find its action.
///
/// [`Action::sort_column_index`] answers the other direction and cannot be
/// inverted without a table; this is that table, and its order is the whole of
/// its meaning.
const SORT_BY_COLUMN: [Action; 9] = [
    Action::SortByColumn1,
    Action::SortByColumn2,
    Action::SortByColumn3,
    Action::SortByColumn4,
    Action::SortByColumn5,
    Action::SortByColumn6,
    Action::SortByColumn7,
    Action::SortByColumn8,
    Action::SortByColumn9,
];

/// `Ctrl+Shift+1`..`Ctrl+Shift+9`, the secondary sort.
const SORT_SECONDARY: [Action; 9] = [
    Action::SortSecondary1,
    Action::SortSecondary2,
    Action::SortSecondary3,
    Action::SortSecondary4,
    Action::SortSecondary5,
    Action::SortSecondary6,
    Action::SortSecondary7,
    Action::SortSecondary8,
    Action::SortSecondary9,
];

/// One dialog's paragraph: its heading and its body.
///
/// An exhaustive `match`, which is what makes a new [`DialogId`] a compile
/// error here rather than a dialog with no help. A new variant needs an arm
/// here **and** an entry in [`DIALOG_ORDER`].
fn dialog_help(id: DialogId) -> (&'static str, &'static str) {
    match id {
        DialogId::Help => (
            "Help",
            "This page. F1 opens it from anywhere and lands on the section for\n\
             wherever you pressed it; F7 and / search the whole document, not\n\
 just the section you are looking at.",
        ),
        DialogId::Message => (
            "Message",
            "Something you have to acknowledge. Enter and Esc both close it.",
        ),
        DialogId::ConfirmQuit => (
            "Quit?",
            "Shown when ui.confirm_exit is on. Quitting with a transfer still\n\
             running always asks, whatever that setting says, and names what is\n\
 still running.",
        ),
        DialogId::GotoPath => (
            "Go to path",
            "Ctrl+G. The field starts empty because it is for going somewhere\n\
             else; Enter on an empty field goes home. ~ and $VAR expand, and a\n\
             relative path resolves against the panel. A path that does not\n\
             exist is refused here, leaving what you typed to be corrected\n\
.",
        ),
        DialogId::GotoOffset => (
            "Go to offset",
            "Ctrl+G inside the viewer, which is a different question from the\n\
             one above and is asked by the same key in a different context\n\
. Takes 0x1f00, 1f00h or 7936 for a byte offset, 50%\n\
             for a percentage, and :500 or L500 for a line.",
        ),
        DialogId::Mkdir => (
            "Create directory",
            "F7. Intermediate directories are created with it.",
        ),
        DialogId::MkdirForTarget => (
            "Create directory (for the target)",
            "+ F7 inside the copy dialog: a directory for the target side, not\n\
             for the panel. The name lands in the target field when it is made\n\
.",
        ),
        DialogId::Rename => (
            "Rename",
            "F2 and Shift+F6. The stem is preselected, so typing replaces the\n\
             name and leaves the extension. A name that already exists is\n\
 refused before anything happens.",
        ),
        DialogId::EditNew => (
            "New file",
            "Shift+F4: the name of the file to create, which is then opened in\n\
 the external editor.",
        ),
        DialogId::SelectMask => (
            "Mark by wildcard",
            "The + key. *.txt marks every text file, and * and ? mean what \
             they do in a shell.",
        ),
        DialogId::UnselectMask => ("Unmark by wildcard", "The - key, and the same syntax."),
        DialogId::CopyMove => (
            "Copy / Move",
            "F5 and F6, one dialog for both. The target takes a path and a\n\
             mask together - /srv/media/*.* - and the options row carries\n\
             preserve attributes, verify, and a conflict policy chosen up\n\
 front. F2 queues the job instead of starting it.",
        ),
        DialogId::Conflict => (
            "This file already exists",
            "One question per conflict, with an all variant of each answer so a\n\
 long copy is not answered file by file.",
        ),
        DialogId::ConfirmDelete => (
            "Delete",
            "F8 and Delete go to the trash; Shift+F8 and Shift+Delete bypass it\n\
 and say so. The count is named in the question.",
        ),
        DialogId::ConfirmRewrite => (
            "Rewrite this archive?",
            "Writing into a compressed tar rewrites the whole file. Above a\n\
             size worth asking about, this asks, with cancel as the default\n\
 button.",
        ),
        DialogId::Progress => (
            "Progress",
            "Two bars, the current file and the batch, with a rate measured\n\
             above the backend. Esc cancels the job; F2 sends it to the\n\
 background queue and it keeps running.",
        ),
        DialogId::JobQueue => (
            "Background jobs",
            "Everything queued, running or finished. Enter brings a job back to\n\
 the foreground exactly as it was.",
        ),
        DialogId::JobSummary => (
            "Job summary",
            "What failed at the end of a batch, with the option to retry just\n\
 the failures.",
        ),
        DialogId::Pack => (
            "Pack",
            "Alt+F5: target name, format, compression level, and move to\n\
             archive, which packs and then deletes the sources - and only if\n\
 the pack succeeded.",
        ),
        DialogId::Resize => (
            "Resize images",
            "Shift+R. Keep the aspect ratio or set both edges, in percent or in\n\
             pixels; best fit or exact; the output format with its quality or\n\
             compression; and a prefix and postfix for the new names. The\n\
             images are written into the other panel's directory, and a name\n\
             already there goes through the usual conflict dialog.",
        ),
        DialogId::MultiRename => (
            "Multi-rename",
            "Ctrl+M. A name mask and an extension mask with [N], [C] and [E]\n\
             placeholders, a search and replace over the result, and a preview\n\
             table that updates as you type. Start! renames; Undo puts the last\n\
 run back.",
        ),
        DialogId::RenameResult => (
            "Rename results",
            "What happened per file, failures included.",
        ),
        DialogId::Find => (
            "Find files",
            "Alt+F7. General takes the name mask, the text to find and where to\n\
             start; Advanced takes size, date and attribute filters; Load/Save\n\
             keeps searches by name in searches.toml. Results open as a virtual\n\
 listing you can act on like any other.",
        ),
        DialogId::SaveSearch => (
            "Save search as",
            "The name a search is remembered under, on top of the Find dialog.",
        ),
        DialogId::ConfirmRemoteSearch => (
            "Search across the network?",
            "A content search on a connected panel reads every candidate file\n\
             over the link. It is opt-in per search rather than a setting,\n\
 because the cost is per search.",
        ),
        DialogId::Drive(Side::Left) => (
            "Devices (left panel)",
            "Alt+F1. Mount point, label, filesystem and free of total, with the\n\
             hotlist under a separator. Arrows move, Enter chooses, Esc\n\
             cancels, and typing quick-searches the list - typing us jumps to\n\
             /usr. Alt+F1 always acts on the left panel, whichever one has\n\
 focus.",
        ),
        DialogId::Drive(Side::Right) => (
            "Devices (right panel)",
            "Alt+F2, and the same list. It always acts on the right panel,\n\
             whichever one has focus: the pair is spatial, not relative\n\
.",
        ),
        DialogId::Hotlist => (
            "Directory hotlist",
            "Ctrl+D. The same list Alt+F1 shows under its separator, on its\n\
             own, acting on whichever panel has focus. Ctrl+Shift+D adds the\n\
             directory you are in. An entry whose path has gone is shown greyed\n\
             with the reason rather than dropped, and Enter on it refuses\n\
 instead of navigating.",
        ),
        DialogId::HotlistAdd => (
            "Add to the hotlist",
            "Ctrl+Shift+D. The label starts as the last component of the path\n\
             and is yours to change. Adding a directory that is already in the\n\
             list replaces that entry's label where it stands rather than\n\
             adding a second row, and the order is the order you put them in -\n\
 hotlist.toml is never sorted.",
        ),
        DialogId::Menu => (
            "Menu bar",
            "F9. Six menus - Files, Mark, Commands, Net, Show and\n\
             Configuration - each opened directly by Alt and its underlined\n\
             letter. Left and Right walk between menus, Up and Down within one,\n\
             Enter runs the item and Esc gives the panel back. Every item shows\n\
             the key that runs it, which is the point of the bar on a terminal\n\
 that cannot deliver half of them.",
        ),
        DialogId::ContextMenu => (
            "Context menu",
            "Shift+F10, or Alt+K where the terminal cannot deliver it. What it\n\
             offers depends on the entry under the cursor: the handlers\n\
             config.toml associates with its type, Open with..., the clipboard\n\
             operations and the file operations. On a remote panel or inside an\n\
             archive the entries that need a real local path are absent rather\n\
 than greyed.",
        ),
        DialogId::Execute => (
            "Execute?",
            "What Enter on an executable file asks, because Enter is the key\n\
             people press to navigate and the cost of an accidental execution\n\
             is unbounded. It names the file, its size and what the file\n\
             actually is - read from the content, never guessed from the name -\n\
             and offers Execute, Open with..., View (F3) and Cancel. It opens\n\
             on Cancel. Set open.execute to always or never in config.toml to\n\
 stop being asked.",
        ),
        DialogId::OpenWith => (
            "Open with...",
            "The applications the desktop advertises for this file's type, read\n\
             from the desktop entry files themselves. Typing quick-searches the\n\
             names. Reachable from the execute prompt and from the context menu\n\
.",
        ),
        DialogId::History => (
            "Command history",
            "Alt+F8. The chosen command is put on the command line, not run:\n\
             Enter on the command line is what runs one. With a shell alive the\n\
             history is the shell's own and Ctrl+R in the console is where it\n\
             lives, so this list is only ever the fallback command line's\n\
.",
        ),
        DialogId::Theme => (
            "Theme",
            "A narrow list of every theme. The theme changes as the cursor\n\
             moves, so what you are choosing between is the program itself and\n\
             not a swatch: the panels, the cursor bar and the key bar stay\n\
             visible beside it. Typing quick-searches the names. Enter keeps\n\
             what is on screen; Esc puts back the one you started with.\n\
             The choice lasts the session - put it in config.toml to keep it.",
        ),
        DialogId::Template => (
            "Template",
            "A narrow list of the binary struct templates the program knows,\n\
             for the hex viewer. A template names the fields of a known format\n\
             - the width of a PNG, the entry point of an ELF - so the dump can\n\
             say what a run of bytes is instead of only what it reads as.\n\
             Typing quick-searches the names; Enter applies one, Esc leaves\n\
             the dump as it was.",
        ),
        DialogId::FileSummary => (
            "File information",
            "Shift+F9 or Shift+Space in a panel, F9 in the viewer. The name,\n\
             the size and the attributes of the file, and then what its\n\
             contents turned out to be: a PNG reads as its dimensions and its\n\
             colour type, an ELF as its machine and entry point. A file no\n\
             template recognises still shows its name, size and attributes and\n\
             says the contents were not recognised.",
        ),
        DialogId::Connect => (
            "Connect",
            "Ctrl+F: a quick-connect line or a saved host. See the Remote\n\
 connections section.",
        ),
        DialogId::HostForm => (
            "Add host",
            "The saved-host form, opened by Add host and by F4 in the connect\n\
 dialog. It never holds a password.",
        ),
        DialogId::RemoteSecret => (
            "Password",
            "A password or a passphrase. It is never written to a file, and the\n\
             box that offers to keep it puts it in the system keyring\n\
.",
        ),
        DialogId::HostKey => (
            "Unknown host key",
            "The fingerprint, to be accepted before anything is sent\n\
.",
        ),
        DialogId::HostKeyChanged => (
            "Host key changed",
            "A message, not a question. Nothing connects until known_hosts is\n\
 corrected by hand.",
        ),
        DialogId::ConfirmDisconnect => (
            "Disconnect?",
            "Ctrl+F on a panel that is already connected.",
        ),
    }
}

/// How far into [`dialogs_page`] a dialog's paragraph starts.
///
/// Computed from the same list and the same renderer the page is built from,
/// so the two cannot drift apart.
fn dialog_offset(id: DialogId) -> Option<usize> {
    let page = dialogs_page();
    let mut wanted = String::new();
    for candidate in DIALOG_ORDER {
        let (title, _) = dialog_help(*candidate);
        if *candidate == id {
            wanted = title.to_string();
            break;
        }
    }
    if wanted.is_empty() {
        return None;
    }
    page.lines().position(|line| line == wanted)
}

/// The first line that starts with `heading`.
fn line_starting_with(lines: &[String], heading: &str) -> Option<u64> {
    let index = lines.iter().position(|line| line.starts_with(heading))?;
    u64::try_from(index).ok()
}

/// Append a block's lines, keeping one blank line between sections.
fn extend(lines: &mut Vec<String>, block: &str) {
    for line in block.lines() {
        lines.push(line.to_string());
    }
    if lines.last().is_some_and(|line| !line.is_empty()) {
        lines.push(String::new());
    }
}

/// Build the `F1` page for the viewer.
///
/// > **Context-sensitive**: `F1` inside a dialog opens that dialog's page; `F1`
/// > in the viewer opens the viewer keys.
///
/// Generated from the **active keymap**, never hand-written, so a user who
/// rebinds a key sees their binding. Returned as text because the help view is
/// another viewer over it.
///
/// `enhanced` is whether the Kitty keyboard protocol is active
/// ([`crate::input::Keyboard::enhanced`]). It is a parameter rather than an
/// assumption because the design asks this page to mark the bindings this
/// terminal cannot deliver, and the viewer's own `Ctrl+Shift+Home` and
/// `Ctrl+Shift+End` are two of them. Passed through to
/// [`crate::config::Keymap::describe`], which is where the marking lives.
pub fn viewer_page(keymap: &crate::config::Keymap, enhanced: bool) -> String {
    use crate::input::Action;
    let mut out = String::from("Viewer keys\n\n");
    let actions = [
        Action::Close,
        Action::ModeText,
        Action::ModeHex,
        Action::ModeRender,
        Action::FoldToggle,
        Action::FoldAll,
        Action::UnfoldAll,
        Action::Edit,
        Action::ToggleWrap,
        Action::QuickFind,
        Action::FindNext,
        Action::FindPrev,
        Action::GotoOffset,
        Action::CycleEncoding,
        Action::CursorUp,
        Action::CursorDown,
        Action::CaretLeft,
        Action::CaretRight,
        Action::CursorPageUp,
        Action::CursorPageDown,
        // the design gives the bare pair to the *row's* edges and the
        // `Ctrl`ed pair to the file's; a page that listed only one of them
        // would leave the other looking unbound.
        Action::LineStart,
        Action::LineEnd,
        Action::CursorTop,
        Action::CursorBottom,
        // the same movements with `Ctrl` move the view instead,
        // which is a different job and belongs beside them rather than after
        // the selection keys.
        Action::ViewScrollUp,
        Action::ViewScrollDown,
        Action::ViewScrollPageUp,
        Action::ViewScrollPageDown,
        Action::ViewScrollLeft,
        Action::ViewScrollRight,
        // the cursor and selection. Listed after the movement keys
        // because that is the order the section introduces them in: the arrows
        // move a cursor, `Shift` with them selects, and the rest is what can be
        // done with a selection.
        Action::SelectAll,
        Action::SelectBlock,
        // the four. They were missing from this page entirely,
        // which made them undiscoverable: the whole point of putting the
        // grouping on a key is trying one and then another against an
        // unfamiliar file, and a key nobody knows about is not a key.
        Action::HexGroup,
        Action::HexFormat,
        Action::HexSign,
        Action::HexEndian,
        Action::HexSide,
        Action::ClipboardCopy,
        Action::CopyInterpretation,
        Action::Inspect,
        Action::ViewerTemplate,
        Action::FileInfo,
        Action::Help,
    ];
    let width = actions
        .iter()
        .map(|a| bindings_text(keymap, *a, enhanced).chars().count())
        .max()
        .unwrap_or(0)
        .max(4);
    for action in actions {
        let keys = bindings_text(keymap, action, enhanced);
        let pad = width.saturating_sub(keys.chars().count());
        out.push_str("  ");
        out.push_str(&keys);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push_str("  ");
        out.push_str(viewer_description(action));
        out.push('\n');
    }
    // The two that have no binding of their own, because they are a *modifier*
    // on the movement keys above rather than keys in their own right.
    // A generated page cannot find them in the keymap, and a
    // page that left them out would leave selecting undiscoverable.
    out.push_str("\n  Shift + any movement       Extend the selection\n");
    out.push_str("  Ctrl+Shift + a movement    Extend it as a column block\n");
    // Two things the generated table above cannot say, and both bite in
    // practice. The first is a real collision between
    // `Ctrl+Home` and `Ctrl+End` are movements in their own right, so adding
    // `Shift` to them reads as "extend to the file's end" and never as a block.
    // The second is the reason the `Alt` keys exist at all.
    out.push_str(
        "\n  Ctrl+Shift+Home and Ctrl+Shift+End extend to the file's ends and\n\
         stay linear: Ctrl+Home and Ctrl+End are movements themselves, so\n\
         Shift extends them rather than squaring them off. Alt+B makes the\n\
         selection a block instead, and Alt+C copies the interpretation,\n\
         because many terminals cannot send Ctrl+Shift with an arrow at all.\n",
    );
    out.push_str("\nEsc clears the selection first and closes the viewer only when\n");
    out.push_str("there is none. A copy goes to the terminal's own clipboard\n");
    out.push_str("(OSC 52) and to the internal one, and is refused rather than\n");
    out.push_str("truncated above viewer.copy_max.\n");
    out.push_str("\nGo to takes 0x1f00, 1f00h or 7936 for a byte offset, 50% for a\n");
    out.push_str("percentage of the file, and :500 or L500 for a line number.\n");
    out.push_str("\nThe viewer streams: no file is ever read whole.\n");
    out.push_str("While the index is still building, End and percentage seeks are\n");
    out.push_str("marked approximate in the status line rather than blocked.\n");
    out
}

/// One row's key column.
///
/// [`crate::config::Keymap::describe`] and nothing else: this used to be its
/// own renderer, which meant the viewer page and the whole-program page
/// could spell the same key two ways.
///
fn bindings_text(
    keymap: &crate::config::Keymap,
    action: crate::input::Action,
    enhanced: bool,
) -> String {
    keymap.describe(crate::config::KeyContext::Viewer, action, enhanced)
}

/// What an action does **in the viewer**, where that is not what its own
/// description says.
///
/// the design makes one action mean different things in different contexts,
/// and three of the keys are exactly that: `F4` is `edit` on a
/// panel and the mode toggle here, and `Left` / `Right` are the command line's
/// caret keys and the sideways move here. The action's own description belongs
/// to the action; this is the page's, and the page is the viewer's.
const fn viewer_description(action: crate::input::Action) -> &'static str {
    use crate::input::Action;
    match action {
        Action::Edit => "Toggle between text and hex",
        Action::CaretLeft => "Move the cursor left one character or column",
        Action::CaretRight => "Move the cursor right one character or column",
        // Nothing in a file is an "entry". These four are the panel's cursor
        // keys elsewhere and the navigation keys here, which is
        // the one action meaning different things in different
        // contexts - and this page is the viewer's context.
        Action::CursorUp => "Move the cursor up one row",
        Action::CursorDown => "Move the cursor down one row",
        Action::CursorPageUp => "Up one page",
        Action::CursorPageDown => "Down one page",
        Action::CursorTop => "Go to the first byte of the file",
        Action::CursorBottom => "Go to the last byte of the file",
        Action::GotoOffset => "Go to an offset, a percentage or a line",
        // the design again: on a panel these three remember files, mark
        // every entry and switch panels, and none of those meanings follows
        // into a viewer.
        Action::LineStart => "Go to the start of the row",
        Action::LineEnd => "Go to the end of the row",
        // The cursor and the selection stay put for all six, which is the
        // whole point of them and the only thing worth saying here.
        Action::ViewScrollUp => "Scroll the view up a row, keeping the selection",
        Action::ViewScrollDown => "Scroll the view down a row, keeping the selection",
        Action::ViewScrollPageUp => "Scroll the view up a page, keeping the selection",
        Action::ViewScrollPageDown => "Scroll the view down a page, keeping the selection",
        Action::ViewScrollLeft => "Scroll the view left (text mode, wrap off)",
        Action::ViewScrollRight => "Scroll the view right (text mode, wrap off)",
        Action::SelectAll => "Select the whole file",
        Action::ClipboardCopy => "Copy the selection",
        Action::HexGroup => "Hex column size: 8, 16, 32, 64 bits",
        Action::HexFormat => "Hex display base: hex or decimal",
        Action::HexSign => "Hex sign: unsigned or signed (decimal only)",
        Action::HexEndian => "Hex byte order: little or big",
        Action::HexSide => "Switch between the hex bytes and characters sides",
        Action::ModeRender => {
            "Show the document rendered: a JSON tree, a page's text, Markdown made"
        }
        Action::FoldToggle => "Collapse or expand this line's region (mode 3)",
        Action::FoldAll => "Collapse every region (mode 3)",
        Action::UnfoldAll => "Expand every region (mode 3)",
        Action::ViewerTemplate => "Read the bytes at the cursor as a known format, and colour them",
        other => other.description(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app() -> App {
        App::headless(
            Config::default(),
            crate::config::Keymap::builtin(),
            crate::config::Theme::blue(),
        )
    }

    #[test]
    fn the_page_is_generated_from_the_active_keymap() {
        // "A user who rebinds `Ctrl+D` sees their binding, not
        // the shipped default."
        let mut app = app();
        let before = keyboard_page(&app);
        assert!(before.contains("Ctrl+D"), "{before}");
        app.keymap
            .overlay("[global]\nhotlist = [\"alt+k\"]\n", "test.toml");
        let after = keyboard_page(&app);
        assert!(after.contains("Alt+K"), "{after}");
        assert!(
            !after.contains("Ctrl+D  "),
            "the old binding is gone: {after}"
        );
    }

    #[test]
    fn the_top_of_the_page_says_it_can_be_searched() {
        // The help is shown in the viewer, so quick find works in it - and
        // nothing said so. The note is at the top because a reader who has to
        // scroll to learn how to search has already lost the search.
        let mut app = app();
        let page = keyboard_page(&app);
        let note = page
            .lines()
            .take(4)
            .find(|l| l.contains("searchable"))
            .unwrap_or_default()
            .to_string();
        assert!(!note.is_empty(), "no note near the top of:\n{page}");
        // The shipped keymap's own bindings, not a hard-coded pair.
        assert!(note.contains("Ctrl+F"), "{note}");
        assert!(note.contains("/"), "{note}");

        // Rebound, and the note follows the keymap like every other row.
        app.keymap
            .overlay("[viewer]\nquick_find = [\"ctrl+j\"]\n", "test.toml");
        let after = keyboard_page(&app);
        let moved = after
            .lines()
            .take(4)
            .find(|l| l.contains("searchable"))
            .unwrap_or_default()
            .to_string();
        assert!(moved.contains("Ctrl+J"), "{moved}");
        assert!(!moved.contains("Ctrl+F"), "{moved}");
    }

    #[test]
    fn the_sort_block_names_the_configured_columns() {
        // Invariant I14, "The `F1` keyboard reference prints
        // the live mapping, generated from the active column order, so it is
        // correct for whatever layout is configured."
        let mut app = app();
        let page = keyboard_page(&app);
        let name_line = page
            .lines()
            .find(|l| l.contains("Sort by Name"))
            .unwrap_or_default()
            .to_string();
        assert!(name_line.contains("Ctrl+1"), "{page}");

        app.config.panel.columns.order = vec![
            ColumnId::Size,
            ColumnId::Name,
            ColumnId::Date,
            ColumnId::Ext,
            ColumnId::Attr,
        ];
        let page = keyboard_page(&app);
        let size_line = page
            .lines()
            .find(|l| l.contains("Sort by Size"))
            .unwrap_or_default()
            .to_string();
        assert!(
            size_line.contains("Ctrl+1"),
            "size is the first column now:\n{page}"
        );
        let name_line = page
            .lines()
            .find(|l| l.contains("Sort by Name"))
            .unwrap_or_default()
            .to_string();
        assert!(name_line.contains("Ctrl+2"), "{page}");
        assert!(page.contains("Secondary sort by Size"), "{page}");
    }

    #[test]
    fn a_legacy_terminal_is_told_which_keys_cannot_arrive() {
        // Invariant I15, "The help screen marks `Ctrl+H` as unavailable
        // and shows `Alt+.` in its place when running on a legacy
        // terminal, so this never has to be debugged."
        let mut app = app();
        app.keyboard.enhanced = false;
        let page = keyboard_page(&app);
        let hidden = page
            .lines()
            .find(|l| l.contains("Toggle showing hidden files"))
            .unwrap_or_default()
            .to_string();
        assert!(hidden.contains("Ctrl+H"), "{page}");
        assert!(hidden.contains("(unavailable)"), "{hidden}");
        assert!(hidden.contains("Alt+."), "{hidden}");
        // Alt+F1 is in the set too, and its documented fallback is
        // beside it.
        let drive = page
            .lines()
            .find(|l| l.contains("Choose a device for the left panel"))
            .unwrap_or_default()
            .to_string();
        assert!(drive.contains("(unavailable)"), "{drive}");

        // And with the protocol active nothing is marked.
        app.keyboard.enhanced = true;
        let page = keyboard_page(&app);
        assert!(!page.contains("(unavailable)"), "{page}");
    }

    #[test]
    fn every_action_appears_exactly_once() {
        // The page is the reference: an action missing from it is a key nobody
        // can find out about.
        let app = app();
        let page = keyboard_page(&app);
        for action in Action::ALL {
            // The viewer's own page words its rows differently from
            // `Action::description`, so a viewer action is looked up by its
            // binding instead.
            if group_of(&app.keymap, *action) == Group::Viewer {
                continue;
            }
            // The positional sort keys are documented by `sort_block`, which
            // names the column each one actually sorts - "Sort by Size" rather
            // than the generic "sort by the third column" their description
            // carries. They are looked up by their binding instead, which
            // still proves the reader can find the key: what must not happen
            // is an action reachable by a key that appears nowhere.
            if positional_sort_keys(&app).contains(action) {
                let keys = bindings_text(&app.keymap, *action, app.keyboard.enhanced);
                assert!(
                    !keys.is_empty() && page.contains(&keys),
                    "{} is bound to {keys} and that key is not on the F1 page:\n{page}",
                    action.id()
                );
                continue;
            }
            assert!(
                page.contains(action.description()),
                "{} is not on the F1 page:\n{page}",
                action.id()
            );
        }
    }

    #[test]
    fn the_document_is_one_buffer_with_a_line_per_topic() {
        // one document, and a topic is a heading
        // and a line number rather than a separate buffer.
        let app = app();
        let (body, at) = page(&app, HelpTopic::Keyboard);
        assert_eq!(at, None, "the keyboard page is the top of the document");
        for heading in [
            "Keyboard reference",
            "Getting started",
            DIALOGS_HEADING,
            CONSOLE_HEADING,
            "Remote connections",
            "Configuration",
            "About",
            VIEWER_HEADING,
        ] {
            assert!(
                body.contains(heading),
                "{heading} missing from the document"
            );
        }

        let lines: Vec<&str> = body.lines().collect();
        for (topic, expected) in [
            (HelpTopic::Viewer, VIEWER_HEADING),
            (HelpTopic::Console, CONSOLE_HEADING),
        ] {
            let (_, at) = page(&app, topic);
            let line = at.expect("a topic resolves to a line");
            let index = usize::try_from(line).expect("a line fits");
            assert!(
                lines
                    .get(index)
                    .is_some_and(|text| text.starts_with(expected)),
                "{topic:?} landed on {:?}",
                lines.get(index)
            );
        }
    }

    #[test]
    fn f1_in_a_dialog_lands_on_that_dialogs_paragraph() {
        // "`F1` inside a dialog opens that dialog's page."
        let app = app();
        for id in DIALOG_ORDER {
            let (body, at) = page(&app, HelpTopic::Dialog(*id));
            let line = at.unwrap_or_else(|| panic!("{} has no line", id.id()));
            let index = usize::try_from(line).expect("a line fits");
            let (title, _) = dialog_help(*id);
            assert_eq!(
                body.lines().nth(index),
                Some(title),
                "{} landed somewhere else",
                id.id()
            );
        }
    }

    #[test]
    fn every_dialog_paragraph_is_listed_once() {
        let mut seen: Vec<DialogId> = Vec::new();
        for id in DIALOG_ORDER {
            assert!(!seen.contains(id), "{} is listed twice", id.id());
            seen.push(*id);
        }
    }

    #[test]
    fn the_configuration_page_names_the_files_and_this_sessions_values() {
        // the "Configuration (file locations and the main options)",
        // and the rule stated where a reader will meet it.
        let app = app();
        let page = configuration_page(&app);
        for name in ["config.toml", "keymap.toml", "hotlist.toml", "themes/"] {
            assert!(page.contains(name), "{name} missing:\n{page}");
        }
        assert!(page.contains("commented out"), "{page}");
        assert!(page.contains("ui.theme"), "{page}");
        assert!(page.contains("open.execute"), "{page}");
    }

    #[test]
    fn the_about_page_names_the_version_and_the_crates() {
        // "About (version, build, the crate versions it was built
        // against)."
        let page = about_page();
        assert!(page.contains(crate::VERSION), "{page}");
        assert!(page.contains(crate::BIN_NAME), "{page}");
        for name in ["ratatui", "russh", "mime_guess / infer"] {
            assert!(page.contains(name), "{name} missing:\n{page}");
        }
    }

    #[test]
    fn searching_the_reference_for_rename_finds_the_keys_spec_8_3_promises() {
        // the own example: "Searching the keyboard reference for
        // `rename` is the fastest path to `Shift+F6` and `Ctrl+M`."
        let app = app();
        let (body, _) = page(&app, HelpTopic::Keyboard);
        let hits: Vec<&str> = body
            .lines()
            .filter(|line| line.to_lowercase().contains("rename"))
            .collect();
        let joined = hits.join("\n");
        assert!(joined.contains("Shift+F6"), "{joined}");
        assert!(joined.contains("Ctrl+M"), "{joined}");
    }
}
