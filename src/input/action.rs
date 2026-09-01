//! The [`Action`] enum.
//!
//! Every action carries a **stable string id**, so `keymap.toml` is writable by
//! hand and diffable. The ids here are exactly the ids used in
//! `examples/keymap.toml`.
//!
//! Actions whose feature belongs to a later milestone still exist
//! and are still bound. Dispatching one posts "not implemented until v0.4" in
//! the panel status line - never a panic, never a silent no-op. Every action
//! therefore carries the [`Milestone`] that brings it, and
//! [`Action::implemented`] is "has that milestone arrived yet".
//!
//! Naming the milestone rather than the one it is missing from is the whole
//! point: "not implemented in v0.1" told a user what was true a release ago,
//! and had to be edited in every message every time a milestone shipped.

/// Which release brings a feature.
///
/// Ordered, so "is it here yet" is a comparison against [`Milestone::CURRENT`]
/// rather than a table that has to be edited action by action every time a
/// milestone ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Milestone {
    /// The shell of it: panels, columns, tabs, the input model.
    V01,
    /// Operations: copy, move, delete, mkdir, progress, conflicts, masks.
    V02,
    /// The console, `Ctrl+O`, the PTY, and the `F4` editor round trip.
    V03,
    /// The viewer: `F3`, text and hex, highlighting, in-file search.
    V04,
    /// Archives.
    V05,
    /// Search and multi-rename.
    V06,
    /// Remote connections.
    V065,
    /// The rest of Total Commander: device pickers, hotlist, quick view.
    V07,
    /// SMB, filesystems inside disk images, binary templates in the viewer,
    /// the resize dialog, file information, and comparing two files.
    V09,
}

impl Milestone {
    /// The milestone being built right now. Everything at or below this is
    /// expected to work; everything above it says which release brings it.
    ///
    /// It moves **last** in a milestone, because moving it turns on every
    /// action of that milestone at once - including any that is still being
    /// written.
    pub const CURRENT: Self = Self::V09;

    /// Every milestone, in order.
    pub const ALL: &'static [Self] = &[
        Self::V01,
        Self::V02,
        Self::V03,
        Self::V04,
        Self::V05,
        Self::V06,
        Self::V065,
        Self::V07,
        Self::V09,
    ];

    /// How the design spells it: `v0.1`, `v0.65`, and so on.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::V01 => "v0.1",
            Self::V02 => "v0.2",
            Self::V03 => "v0.3",
            Self::V04 => "v0.4",
            Self::V05 => "v0.5",
            Self::V06 => "v0.6",
            Self::V065 => "v0.65",
            Self::V07 => "v0.7",
            Self::V09 => "v0.9",
        }
    }

    /// Has this milestone been reached?
    pub const fn is_current(&self) -> bool {
        (*self as u8) <= (Self::CURRENT as u8)
    }
}

impl std::fmt::Display for Milestone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Build the action enum, its ids, its descriptions, and the milestone that
/// brings each one, from a single table.
macro_rules! actions {
    ($( $variant:ident = $id:literal, $milestone:ident, $desc:literal ; )*) => {
        /// Everything a key can do.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Action {
            $(
                #[doc = $desc]
                $variant,
            )*
        }

        impl Action {
            /// Every action, in declaration order. The `F1` keyboard reference
            /// walks this.
            pub const ALL: &'static [Action] = &[ $( Action::$variant ),* ];

            /// The stable string id used in `keymap.toml`.
            pub const fn id(&self) -> &'static str {
                match self { $( Action::$variant => $id ),* }
            }

            /// Parse an id from `keymap.toml`.
            pub fn from_id(id: &str) -> Option<Action> {
                match id {
                    $( $id => Some(Action::$variant), )*
                    _ => None,
                }
            }

            /// A one-line description, shown beside the binding in the `F1`
            /// keyboard reference.
            pub const fn description(&self) -> &'static str {
                match self { $( Action::$variant => $desc ),* }
            }

            /// Which release brings this action.
            pub const fn milestone(&self) -> Milestone {
                match self { $( Action::$variant => Milestone::$milestone ),* }
            }

            /// False for an action whose feature lands in a later milestone.
            /// Dispatch turns that into a status-line message.
            pub const fn implemented(&self) -> bool {
                self.milestone().is_current()
            }
        }
    };
}

actions! {
    // ------------------------------------------------ global: function keys --
    Help              = "help",              V07, "Keyboard reference and general help";
    Reread            = "reread",            V01,  "Re-read the active panel";
    View              = "view",              V04, "View the file under the cursor";
    Edit              = "edit",              V03, "Edit the file under the cursor";
    Copy              = "copy",              V02, "Copy the selection to the other panel";
    Move              = "move",              V02, "Rename or move the selection";
    Mkdir             = "mkdir",             V02, "Create a directory";
    Delete            = "delete",            V02, "Delete the selection to the trash";
    Menu              = "menu",              V07, "Open the menu bar";
    MenuFiles         = "menu_files",        V07, "Open the Files menu";
    MenuMark          = "menu_mark",         V07, "Open the Mark menu";
    MenuCommands      = "menu_commands",     V07, "Open the Commands menu";
    MenuNet           = "menu_net",          V07, "Open the Net menu";
    MenuShow          = "menu_show",         V07, "Open the Show menu";
    MenuConfig        = "menu_config",       V07, "Open the Configuration menu";
    Quit              = "quit",              V01,  "Quit";
    ConsoleToggle     = "console_toggle",    V03, "Toggle console mode";
    ConsoleScrollUp   = "console_scroll_up",  V03, "Scroll the console back through its output";
    ConsoleScrollDown = "console_scroll_down",V03, "Scroll the console forward again";
    ViewScrollUp      = "view_scroll_up",    V04, "Scroll the view up one row, leaving the cursor";
    ViewScrollDown    = "view_scroll_down",  V04, "Scroll the view down one row, leaving the cursor";
    ViewScrollPageUp  = "view_scroll_page_up", V04, "Scroll the view up one page, leaving the cursor";
    ViewScrollPageDown = "view_scroll_page_down", V04, "Scroll the view down one page, leaving the cursor";
    ViewScrollLeft    = "view_scroll_left",  V04, "Scroll the view left, leaving the cursor";
    ViewScrollRight   = "view_scroll_right", V04, "Scroll the view right, leaving the cursor";
    SwapPanels        = "swap_panels",       V01,  "Swap the two panels";
    OtherPanel        = "other_panel",       V01,  "Move focus to the other panel";

    ViewSingle        = "view_single",       V04, "View the file under the cursor only";
    EditNew           = "edit_new",          V03, "Create a new text file and edit it";
    CopySameDir       = "copy_same_dir",     V02, "Copy within the same directory";
    RenameInPlace     = "rename_in_place",   V02, "Rename in place";
    DeletePermanent   = "delete_permanent",  V02, "Delete permanently, bypassing the trash";
    ContextMenu       = "context_menu",      V07, "Context menu for the entry under the cursor";
    CompareDirs       = "compare_dirs",      V07, "Compare the two file lists";
    CompareDirsContent = "compare_dirs_content", V09, "Compare the two file lists by content";
    CompareFiles      = "compare_files",     V09, "Compare the two files byte for byte";
    DiffFiles         = "diff_files",        V09, "Show the two files as a diff";
    ToggleDiff        = "toggle_diff",       V09, "Swap mode 3 between the document and the diff";
    GitHistory        = "git_history",       V09, "Browse the git history of the current directory";
    ChecksumCreate    = "checksum_create",   V09, "Write a checksum file for the selection";
    ChecksumVerify    = "checksum_verify",   V09, "Check the files a checksum file names";
    SplitFile         = "split_file",        V09, "Split the file into numbered parts";
    MergeFile         = "merge_file",        V09, "Merge a numbered set back together";
    CreateSymlink     = "create_symlink",    V09, "Create a symbolic link to the file under the cursor";
    CreateHardlink    = "create_hardlink",   V09, "Create a hard link to the file under the cursor";
    EditPermissions   = "edit_permissions",  V09, "Change the permissions of the selection";

    DriveLeft         = "drive_left",        V07, "Choose a device for the left panel";
    DriveRight        = "drive_right",       V07, "Choose a device for the right panel";
    ViewExternal      = "view_external",     V04, "View with the external viewer";
    Pack              = "pack",              V05, "Pack the selection into an archive";
    Unpack            = "unpack",            V05, "Unpack the archive under the cursor";
    Search            = "search",            V06, "Search for files";
    // the design gave the history to the shell - "nothing is pushed anywhere
    // here and there is no history file" - so there is no list of this
    // application's own to open a dialog on. With a shell alive `run_action`
    // says where the history actually is; the dialog over the fallback command
    // line's own list is a v0.7 dialog like the hotlist.
    HistoryDialog     = "history_dialog",    V07, "Command history";
    // A theme is judged against the program, so the picker previews as its
    // cursor moves and is narrow enough to leave the program visible.
    ThemePicker       = "theme_picker",      V07, "Choose a theme";
    JobQueue          = "job_queue",         V02, "Background job queue";

    // ------------------------------------------------- global: control keys --
    Hotlist           = "hotlist",           V07, "Directory hotlist";
    HotlistAdd        = "hotlist_add",       V07, "Add this directory to the hotlist";
    ShowHidden        = "show_hidden",       V01,  "Toggle showing hidden files";
    ConnectToggle     = "connect_toggle",    V065, "Connect or disconnect the active panel";
    DirSize           = "dir_size",          V02, "Calculate the space the selection occupies";
    BranchView        = "branch_view",       V06, "Flat recursive listing of the current tree";
    MultiRename       = "multi_rename",      V06, "Multi-rename tool";
    QuickView         = "quick_view",        V07, "Quick view in the other panel";
    PathToCmdline     = "path_to_cmdline",   V01,  "Copy the current path to the command line";
    SelectAll         = "select_all",        V01,  "Mark everything";
    ClipboardCopy     = "clipboard_copy",    V02, "Remember the entry under the cursor, to be copied";
    ClipboardCut      = "clipboard_cut",     V02, "Remember the entry under the cursor, to be moved";
    ClipboardPaste    = "clipboard_paste",   V02, "Put the clipboard down in this directory";
    ReloadConfig      = "reload_config",     V01,  "Reload the configuration files";
    EditConfig        = "edit_config",       V07, "Edit config.toml in the external editor";
    CheckUpdate       = "check_update",      V07, "Ask GitHub whether there is a newer release";

    TabNew            = "tab_new",           V01,  "New tab on the active panel";
    TabClose          = "tab_close",         V01,  "Close the active tab";
    TabNext           = "tab_next",          V01,  "Switch to the next tab in this panel";
    TabPrev           = "tab_prev",          V01,  "Switch to the previous tab in this panel";
    Tab1              = "tab_1",             V01,  "Switch to tab 1";
    Tab2              = "tab_2",             V01,  "Switch to tab 2";
    Tab3              = "tab_3",             V01,  "Switch to tab 3";
    Tab4              = "tab_4",             V01,  "Switch to tab 4";
    Tab5              = "tab_5",             V01,  "Switch to tab 5";
    Tab6              = "tab_6",             V01,  "Switch to tab 6";
    Tab7              = "tab_7",             V01,  "Switch to tab 7";
    Tab8              = "tab_8",             V01,  "Switch to tab 8";
    Tab9              = "tab_9",             V01,  "Switch to tab 9";

    SortByColumn1     = "sort_by_column_1",  V01,  "Sort by the 1st configured column";
    SortByColumn2     = "sort_by_column_2",  V01,  "Sort by the 2nd configured column";
    SortByColumn3     = "sort_by_column_3",  V01,  "Sort by the 3rd configured column";
    SortByColumn4     = "sort_by_column_4",  V01,  "Sort by the 4th configured column";
    SortByColumn5     = "sort_by_column_5",  V01,  "Sort by the 5th configured column";
    SortByColumn6     = "sort_by_column_6",  V01,  "Sort by the 6th configured column";
    SortByColumn7     = "sort_by_column_7",  V01,  "Sort by the 7th configured column";
    SortByColumn8     = "sort_by_column_8",  V01,  "Sort by the 8th configured column";
    SortByColumn9     = "sort_by_column_9",  V01,  "Sort by the 9th configured column";

    SortSecondary1   = "sort_secondary_1",  V01,  "Set the secondary sort to the 1st configured column";
    SortSecondary2   = "sort_secondary_2",  V01,  "Set the secondary sort to the 2nd configured column";
    SortSecondary3   = "sort_secondary_3",  V01,  "Set the secondary sort to the 3rd configured column";
    SortSecondary4   = "sort_secondary_4",  V01,  "Set the secondary sort to the 4th configured column";
    SortSecondary5   = "sort_secondary_5",  V01,  "Set the secondary sort to the 5th configured column";
    SortSecondary6   = "sort_secondary_6",  V01,  "Set the secondary sort to the 6th configured column";
    SortSecondary7   = "sort_secondary_7",  V01,  "Set the secondary sort to the 7th configured column";
    SortSecondary8   = "sort_secondary_8",  V01,  "Set the secondary sort to the 8th configured column";
    SortSecondary9   = "sort_secondary_9",  V01,  "Set the secondary sort to the 9th configured column";
    // V01 and not V07 on purpose: `SortState::clear_secondary` has existed
    // since v0.1 and every other sort action is V01. What was missing was a
    // binding, which is a keymap omission and not an unbuilt feature; a V07
    // here would make ctrl+shift+0 report "not implemented" on a tree where
    // it works.
    SortSecondaryClear = "sort_secondary_clear", V01, "Clear the secondary sort";

    SortByName        = "sort_by_name",      V01,  "Sort by name, whatever the column layout";
    SortByExt         = "sort_by_ext",       V01,  "Sort by extension, whatever the column layout";
    SortByDate        = "sort_by_date",      V01,  "Sort by date, whatever the column layout";
    SortBySize        = "sort_by_size",      V01,  "Sort by size, whatever the column layout";
    SortUnsorted      = "sort_unsorted",     V01,  "Leave the listing unsorted";
    SortDefault       = "sort_default",      V09,  "Sort by name ascending, the default order";

    // ------------------------------------------------------------- panel ----
    Open              = "open",              V01,  "Open the entry under the cursor";
    DialogNextControl = "dialog_next_control", V09, "Move to the next control in a dialog";
    DialogPrevControl = "dialog_prev_control", V09, "Move to the previous control in a dialog";
    OpenWith          = "open_with",         V07, "Open with the associated application, never execute";
    Parent            = "parent",            V01,  "Go to the parent directory";
    EnterAsDir        = "enter_as_dir",      V01,  "Enter the entry under the cursor as a directory";
    Root              = "root",              V01,  "Go to the root directory";
    Home              = "home",              V01,  "Go to the home directory";
    GotoPath          = "goto_path",         V01,  "Prompt for a path and go there";
    OtherPanelCd      = "other_panel_cd",    V01,  "Show the entry under the cursor in the other panel";
    OtherPanelSameDir = "other_panel_same_dir", V09, "Show this panel's own directory in the other panel";
    FocusCmdline      = "focus_cmdline",     V01,  "Move focus to the command line";
    PutSelected       = "put_selected",      V01,  "Insert the entry under the cursor at the command line caret";
    PutSelectedPath   = "put_selected_path", V01,  "Insert the full path of the entry under the cursor at the caret";
    ToggleSelect      = "toggle_select",     V01,  "Toggle the mark and move down";
    SelectUp          = "select_up",         V09,  "Toggle the mark and move up";
    SelectPageUp      = "select_page_up",    V09,  "Mark a page upward";
    SelectPageDown    = "select_page_down",  V09,  "Mark a page downward";
    SelectToStart     = "select_to_start",   V09,  "Mark from the cursor to the first row";
    SelectToEnd       = "select_to_end",     V09,  "Mark from the cursor to the last row";
    SelectAndSize     = "select_and_size",   V02,  "Toggle the mark, sizing a directory";
    SelectMask        = "select_mask",       V02, "Mark by wildcard";
    UnselectMask      = "unselect_mask",     V02, "Unmark by wildcard";
    InvertSelection   = "invert_selection",  V01,  "Invert the marks";
    ClearSearch       = "clear_search",      V01,  "Clear the quick-search buffer, then the marks";
    StartQuickSearch  = "start_quick_search",V01,  "Start a quick search explicitly";
    LeaveVirtual      = "leave_virtual",     V06, "Leave a virtual listing";
    SearchInPanel     = "search_in_panel",   V06, "Search within the current virtual listing";
    RenameResult      = "rename_result",     V06, "Show the last multi-rename's result list";

    CursorUp          = "cursor_up",         V01,  "Move the cursor up";
    CursorDown        = "cursor_down",       V01,  "Move the cursor down";
    CursorPageUp      = "cursor_page_up",    V01,  "Move the cursor up one page";
    CursorPageDown    = "cursor_page_down",  V01,  "Move the cursor down one page";
    CursorTop         = "cursor_top",        V01,  "Move the cursor to the first entry";
    CursorBottom      = "cursor_bottom",     V01,  "Move the cursor to the last entry";

    // ---------------------------------------------------------- cmdline -----
    LeaveToPanel      = "leave_to_panel",    V01,  "Return focus to the panel, keeping the text";
    HistoryPrev       = "history_prev",      V01,  "Previous command in history";
    HistoryNext       = "history_next",      V01,  "Next command in history";
    Run               = "run",               V03, "Run the command line";
    Clear             = "clear",             V01,  "Clear the command line; if empty, return to the panel";
    Complete          = "complete",          V03, "Path or command completion";
    KillWord          = "kill_word",         V01,  "Delete the word before the caret";
    KillLine          = "kill_line",         V01,  "Delete the whole line";
    KillToEnd         = "kill_to_end",       V01,  "Delete from the caret to the end of the line";
    LineStart         = "line_start",        V01,  "Move the caret to the start of the line";
    LineEnd           = "line_end",          V01,  "Move the caret to the end of the line";
    CaretLeft         = "caret_left",        V01,  "Move the caret left";
    CaretRight        = "caret_right",       V01,  "Move the caret right";
    CaretBackspace    = "caret_backspace",   V01,  "Delete the character before the caret";
    CaretDelete       = "caret_delete",      V01,  "Delete the character under the caret";
    ToggleOverwrite   = "toggle_overwrite",  V01,  "Toggle overwrite mode on the command line";

    // ----------------------------------------------------------- viewer -----
    Close             = "close",             V04, "Close the viewer";
    ModeText          = "mode_text",         V04, "Switch the viewer to text mode";
    ModeHex           = "mode_hex",          V04, "Switch the viewer to hex mode";
    QuickFind         = "quick_find",        V04, "Find within the viewer";
    FindNext          = "find_next",         V04, "Next match";
    FindPrev          = "find_prev",         V04, "Previous match";
    GotoOffset        = "goto_offset",       V04, "Go to a byte offset";
    ToggleWrap        = "toggle_wrap",       V04, "Toggle line wrapping";
    ViewerReload      = "viewer_reload",     V04, "Re-read the file from disk";
    HexGroup          = "hex_group",         V04, "Cycle the hex column width: 8, 16, 32, 64 bits";
    HexFormat         = "hex_format",        V04, "Switch the hex display base: hex or decimal";
    HexSign           = "hex_sign",          V04, "Switch the hex sign: unsigned or signed";
    HexEndian         = "hex_endian",        V04, "Flip the hex byte order";
    CycleEncoding     = "cycle_encoding",    V04, "Cycle the text encoding";
    // the cursor and selection. V04 because the v0.4 is the
    // milestone that brings the viewer and the design listed this
    // work as deliberately unwritten rather than as a later release's; an action
    // whose milestone is V04 reports as implemented, which is what these keys
    // must do.
    HexSide           = "hex_side",          V04, "Switch between the hex bytes and characters sides";
    CopyInterpretation = "copy_interpretation", V04, "Copy the selection's interpretation";
    SelectBlock       = "select_block",      V04, "Make the selection a column block, or linear again";
    Inspect           = "inspect",           V07, "Show every reading of the bytes at the cursor";
    FileInfo          = "file_info",         V07, "Show the file's size, attributes and what its contents are";
    Resize            = "resize",            V07, "Resize or convert the selected images into the other panel";
    CopyPath          = "copy_path",         V07, "Copy the full path of the selection to the clipboard";
    ViewerTemplate    = "viewer_template",   V07, "Apply a binary struct template to the hex dump";
    ModeRender        = "mode_render",       V07, "Switch the viewer to the rendered view";
    FoldToggle        = "fold_toggle",       V07, "Collapse or expand the region under the cursor";
    FoldAll           = "fold_all",          V07, "Collapse every region in the rendered view";
    UnfoldAll         = "unfold_all",        V07, "Expand every region in the rendered view";
}

impl Action {
    /// Which of the six menus this action opens.
    ///
    /// The index into [`crate::ui::dialog::menu::MenuModel::menus`],
    /// zero-based, and therefore also the index into
    /// [`crate::ui::MENUBAR`] and [`crate::ui::dialog::menu::LETTERS`] -
    /// the three lists are the same six menus in the same order, which is
    /// what lets `Alt+<letter>` open one directly without a second table.
    /// `None` for every action that is not one of the six.
    pub const fn menu_index(&self) -> Option<usize> {
        match self {
            Self::MenuFiles => Some(0),
            Self::MenuMark => Some(1),
            Self::MenuCommands => Some(2),
            Self::MenuNet => Some(3),
            Self::MenuShow => Some(4),
            Self::MenuConfig => Some(5),
            _ => None,
        }
    }

    /// **the intercept list, from the other side.**
    ///
    /// > The keys this application binds - `Up`/`Down` to leave for the panel
    /// >, `Ctrl+Enter`, `Ctrl+O` - are intercepted before forwarding and
    /// > never reach the shell.
    ///
    /// True means the reverse: this action is the *shell's* line editor's, so
    /// with a live console the key is encoded and forwarded rather than run
    /// here. It is exactly the twelve the design calls "readline-style
    /// editing", which the design turns into "descriptions of what a default
    /// `bash` does rather than reimplementations".
    ///
    /// The filter is on the **action**, not on the key, which is what keeps a
    /// user's `keymap.toml` working: rebind `alt+k` to `job_queue` in
    /// `[cmdline]` and it still opens the queue, because `job_queue` is not one
    /// of these.
    ///
    /// **`reread` is in the list**, which is the one entry that is not a
    /// line-editing action. the design names `Ctrl+R` in as many words -
    /// "History, completion, `Ctrl+R`, vi or emacs bindings … all of it is
    /// whatever the user has configured, because the keys reach the shell" -
    /// and the design binds `Ctrl+R` to re-reading the panel. Both are
    /// satisfied because the filter is consulted *only* with the command line
    /// or the console focused: `Ctrl+R` on a panel still re-reads, and `Ctrl+R`
    /// on the shell's own input line is the shell's reverse search, which is
    /// what the design promises. `F2` is the other binding for a re-read and is
    /// not affected either way.
    ///
    /// Two neighbours are deliberately absent and worth naming:
    ///
    /// * `history_prev` / `history_next` are **translated**, not forwarded as
    ///   themselves. `Ctrl+Up` is not a key any shell binds; the action sends
    ///   the shell the plain `Up` it does bind, so the "history is
    ///   the shell's" holds on a key that does not collide with the
    ///   `Up`/`Down` leaving for the panel.
    /// * `run` is intercepted, because the design gives it three jobs - write
    ///   the line, push it onto *this* application's history, and switch to
    ///   console mode - of which only the first is the shell's.
    pub const fn belongs_to_the_shell(&self) -> bool {
        matches!(
            self,
            Self::Reread
                | Self::CaretLeft
                | Self::CaretRight
                | Self::LineStart
                | Self::LineEnd
                | Self::CaretBackspace
                | Self::CaretDelete
                | Self::KillWord
                | Self::KillLine
                | Self::KillToEnd
                | Self::ToggleOverwrite
                // `Clear` is deliberately NOT here. Esc on the command line
                // empties it, whoever owns the text: with a shell running that
                // means sending the shell a kill-line rather than forwarding
                // the Esc, because a user who presses Esc at a command line
                // wants the line gone. The cost is that a vi-mode shell cannot
                // be put into normal mode from a non-empty line - Esc on an
                // *empty* line is still forwarded, which is where a vi user
                // reaches for it least often, and `alt+esc` or a rebind in
                // [cmdline] gets it back for anyone who wants it.
                | Self::Complete
        )
    }

    /// The actions still ours while `Ctrl+O` has the panels hidden.
    ///
    ///
    /// > It is not a split, not a pane, and not a shrunken terminal: it is the
    /// > same screen the shell would have had on its own. … a program that
    /// > wants a terminal gets a real one.
    ///
    /// So the rule inverts: in the full-screen console **everything** is
    /// forwarded except the key that gets you out and the two that walk the
    /// scrollback. `F5` does not copy files at a `vim` running in there, and
    /// `F10` does not quit the application out from under it.
    pub const fn survives_full_console(&self) -> bool {
        matches!(
            self,
            // `Help` is here because the design says so in as many words:
            // "F1 in the console explains the console". It is the one key that
            // has to reach this application from a screen the shell otherwise
            // owns entirely, and the page it opens says which keys the shell
            // keeps and which this program intercepts - which is exactly the
            // question someone looking at a full-screen shell has.
            Self::Help | Self::ConsoleToggle | Self::ConsoleScrollUp | Self::ConsoleScrollDown
        )
    }

    /// The message shown when an action that belongs to a later milestone is
    /// dispatched (the design scope; never a panic, never silence).
    ///
    /// It names the milestone that *brings* the feature, so the sentence stays
    /// true as milestones ship instead of ageing into a lie.
    pub fn not_implemented_message(&self) -> String {
        format!(
            "{}: not implemented until {}",
            self.description(),
            self.milestone()
        )
    }

    /// `Ctrl+<n>` addresses the n-th configured column.
    /// Returns `n`, one-based.
    pub const fn sort_column_index(&self) -> Option<usize> {
        match self {
            Self::SortByColumn1 => Some(1),
            Self::SortByColumn2 => Some(2),
            Self::SortByColumn3 => Some(3),
            Self::SortByColumn4 => Some(4),
            Self::SortByColumn5 => Some(5),
            Self::SortByColumn6 => Some(6),
            Self::SortByColumn7 => Some(7),
            Self::SortByColumn8 => Some(8),
            Self::SortByColumn9 => Some(9),
            _ => None,
        }
    }

    /// `Ctrl+Shift+<n>` addresses the same configured column as `Ctrl+<n>`, and
    /// sets the *secondary* key. Returns `n`, one-based.
    pub const fn sort_secondary_index(&self) -> Option<usize> {
        match self {
            Self::SortSecondary1 => Some(1),
            Self::SortSecondary2 => Some(2),
            Self::SortSecondary3 => Some(3),
            Self::SortSecondary4 => Some(4),
            Self::SortSecondary5 => Some(5),
            Self::SortSecondary6 => Some(6),
            Self::SortSecondary7 => Some(7),
            Self::SortSecondary8 => Some(8),
            Self::SortSecondary9 => Some(9),
            _ => None,
        }
    }

    /// `Alt+<n>` switches tabs. Returns `n`, one-based.
    pub const fn tab_index(&self) -> Option<usize> {
        match self {
            Self::Tab1 => Some(1),
            Self::Tab2 => Some(2),
            Self::Tab3 => Some(3),
            Self::Tab4 => Some(4),
            Self::Tab5 => Some(5),
            Self::Tab6 => Some(6),
            Self::Tab7 => Some(7),
            Self::Tab8 => Some(8),
            Self::Tab9 => Some(9),
            _ => None,
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_milestone_has_a_label_and_they_are_all_distinct() {
        // This used to read the design with `include_str!` and check each label
        // appeared in it, which tied the crate's ability to compile to a
        // document outside it. What is worth holding is the property the
        // labels themselves have to have.
        let mut seen = std::collections::HashSet::new();
        for m in Milestone::ALL {
            let label = m.label();
            assert!(!label.is_empty(), "{m:?} has no label");
            assert!(seen.insert(label), "two milestones share the label {label}");
        }
    }

    #[test]
    fn the_not_implemented_message_names_the_milestone_that_brings_it() {
        // v0.7 is the last line, so there is no action left above
        // `CURRENT` to demonstrate the message with. The format is still
        // covered - against a synthetic milestone rather than an action -
        // because a message that names a release which has already shipped is
        // exactly the lie `not_implemented_message` was written to prevent.
        // Updated rather than deleted, which
        // is the whole point of the test.
        assert!(Milestone::V07.is_current());
        assert_eq!(Milestone::V07.label(), "v0.7");
        assert_eq!(
            format!("{}: not implemented until {}", "Something", Milestone::V07),
            "Something: not implemented until v0.7"
        );
        // Every action v0.7 brings now reports as available.
        for action in [
            Action::Hotlist,
            Action::HotlistAdd,
            Action::DriveLeft,
            Action::DriveRight,
            Action::QuickView,
            Action::CompareDirs,
            Action::CompareDirsContent,
            Action::ContextMenu,
            Action::Menu,
            Action::MenuFiles,
            Action::MenuMark,
            Action::MenuCommands,
            Action::MenuNet,
            Action::MenuShow,
            Action::MenuConfig,
            Action::Help,
            Action::OpenWith,
            Action::HistoryDialog,
            Action::EditConfig,
            Action::CheckUpdate,
        ] {
            assert!(action.implemented(), "{action} is a v0.7 feature");
        }
        // Everything the earlier milestones are for still reports as
        // available.
        for action in [
            Action::ConnectToggle,
            Action::Search,
            Action::BranchView,
            Action::MultiRename,
            Action::LeaveVirtual,
            Action::SearchInPanel,
            Action::RenameResult,
            Action::Pack,
            Action::Unpack,
            Action::Copy,
            Action::Move,
            Action::Mkdir,
            Action::Delete,
            Action::DeletePermanent,
            Action::DirSize,
            Action::SelectMask,
            Action::UnselectMask,
            Action::SelectAndSize,
        ] {
            assert!(action.implemented(), "{action} is a v0.2 feature");
        }
    }

    #[test]
    fn ids_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for a in Action::ALL {
            assert!(seen.insert(a.id()), "duplicate action id {}", a.id());
            assert_eq!(Action::from_id(a.id()), Some(*a));
        }
    }

    #[test]
    fn every_action_id_in_the_shipped_keymap_exists() {
        let text = include_str!("../../examples/keymap.toml");
        let doc: toml::Table = toml::from_str(text).expect("examples/keymap.toml parses");
        for (table_name, table) in &doc {
            let table = table.as_table().expect("keymap sections are tables");
            for id in table.keys() {
                assert!(
                    Action::from_id(id).is_some(),
                    "[{table_name}] {id} has no Action variant"
                );
            }
        }
    }
}
