//! `config.toml`.
//!
//! Every field carries `#[serde(default)]`, so a partial user file works and a
//! missing file is exactly the compiled-in defaults.
//!
//! # Where the design and `examples/config.toml` disagree, the design wins
//!
//! Recorded here and in
//!
//! * `panel.human_sizes` - the design says **false**; the example file says
//!   `true`. The reference screenshot shows `362,333`, not `362 K`. Default is
//!   `false`.
//! * `panel.thousands_separator` - the design says **true**; the example file
//!   omits it entirely. Default is `true`.
//! * `panel.attr_style` - the design defines it (`"unix"` default, `"dos"`
//!   alternative); the example file omits it. Default is `Unix`.
//! * `panel.dir_brackets` - the design defines it as `true`; the example file
//!   omits it. Default is `true`.
//! * `panel.date_format` - the design renders `2026-08-12 02:40` and gives the
//!   `date` column `min_chars = 16`, which only fits that form. The example
//!   file says `"%d-%m-%y %H:%M"`. Default is `"%Y-%m-%d %H:%M"`.
//! * `viewer.highlight.engine` - the example file says `"tree-sitter"`, but
//!   the design settle on `syntect`. Default is `Syntect`, and
//!   `"tree-sitter"` is accepted as a deprecated alias that raises a warning.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::panel::ColumnId;

use super::units::{ByteSize, Timeout};

/// The whole of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Chrome and layout.
    pub ui: UiConfig,
    /// Everything about how a panel renders and behaves.
    pub panel: PanelConfig,
    /// The `Ctrl+O` console (v0.3).
    pub console: ConsoleConfig,
    /// `Enter` policy and file associations.
    pub open: OpenConfig,
    /// The external editor `F4` shells out to (v0.3).
    pub editor: EditorConfig,
    /// The internal viewer (v0.4).
    pub viewer: ViewerConfig,
    /// File operations (v0.2).
    pub ops: OpsConfig,
    /// Archives (v0.5).
    pub archive: ArchiveConfig,
    /// Search (v0.6).
    pub search: SearchConfig,
    /// The device picker (v0.7).
    pub devices: DevicesConfig,
    /// Remote connections (v0.65).
    pub remote: RemoteConfig,
    /// Terminal capability overrides.
    pub terminal: TerminalConfig,
    /// File-type colouring rules. Kept out of the theme so a user
    /// keeps their rules across themes.
    ///
    /// Defaults to the worked example, the archive extensions. A file
    /// that writes any `[[filetypes]]` block **replaces** the list rather than
    /// adding to it, the same way `keymap.toml` replaces an action's bindings -
    /// which is why the shipped `config.toml` carries the default rule as a
    /// comment, so it is in front of anyone about to add their own.
    #[serde(default = "default_filetypes")]
    pub filetypes: Vec<FiletypeRule>,

    /// Warnings collected while loading: unknown keys, values that did not
    /// parse, files that could not be read. Never fatal; the UI
    /// shows them so nothing is silently ignored.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            panel: PanelConfig::default(),
            console: ConsoleConfig::default(),
            open: OpenConfig::default(),
            editor: EditorConfig::default(),
            viewer: ViewerConfig::default(),
            ops: OpsConfig::default(),
            archive: ArchiveConfig::default(),
            search: SearchConfig::default(),
            devices: DevicesConfig::default(),
            remote: RemoteConfig::default(),
            terminal: TerminalConfig::default(),
            filetypes: default_filetypes(),
            warnings: Vec::new(),
        }
    }
}

/// the worked example, shipped as the default rule set.
///
/// It lives here rather than as a constant in the renderer because the design
/// presents it as configuration - which means a user can drop it, extend it, or
/// point it at a different slot, and none of that should need a rebuild.
fn default_filetypes() -> Vec<FiletypeRule> {
    let exts = [
        "zip", "tar", "gz", "tgz", "bz2", "tbz", "xz", "txz", "zst", "7z", "rar", "lz4", "lzma",
        "cab", "iso", "jar", "war", "deb", "rpm",
    ];
    vec![FiletypeRule {
        matcher: Matcher {
            ext: exts.iter().map(|e| (*e).to_string()).collect(),
            mime: None,
            mode: None,
        },
        slot: "panel.archive_fg".to_string(),
    }]
}

// ---------------------------------------------------------------- ui --------

/// `[ui]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Theme name; loaded from `themes/<name>.toml`.
    pub theme: String,
    /// `+-|` instead of box drawing, and `...`/`^`/`v` instead of `…`/`▲`/`▼`.
    ///
    pub ascii_borders: bool,
    /// Keep the menu bar permanently visible. `F9` summons it either way.
    pub show_menubar: bool,
    /// Draw the key bar.
    pub show_keybar: bool,
    /// Mouse support is optional and additive.
    pub mouse: bool,
    /// The left panel's share of the width.
    pub split_ratio: f32,
    /// Prompt before quitting.
    pub confirm_exit: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "blue".to_string(),
            ascii_borders: false,
            show_menubar: false,
            show_keybar: true,
            mouse: false,
            split_ratio: 0.5,
            confirm_exit: true,
        }
    }
}

// ------------------------------------------------------------- panel --------

/// How the `attr` column renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttrStyle {
    /// `drwxr-xr-x`, ten characters. The meaningful equivalent on Linux.
    #[default]
    Unix,
    /// `-a--`, four characters, mapping hidden from the leading dot. There for
    /// Total Commander muscle memory.
    Dos,
}

/// What a quick search matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuickSearchMode {
    /// Leading characters.
    #[default]
    Prefix,
    /// Anywhere in the name.
    Substring,
    /// Subsequence.
    Fuzzy,
}

/// How case is handled while type-navigating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuickSearchCase {
    /// `tho` matches `Thorin` and `thorin`.
    Insensitive,
    /// `tho` matches `thorin` only.
    Sensitive,
    /// Insensitive until an uppercase character is typed. The ripgrep
    /// convention, and the default.
    #[default]
    Smart,
}

/// What bare digits do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigitKeys {
    /// Digits feed the quick-search buffer; tabs live on `Alt+<digit>`.
    #[default]
    QuickSearch,
    /// Digits switch tabs while the search buffer is empty; `Ctrl+S` starts a
    /// search explicitly.
    Tabs,
}

/// When to draw the tab bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabBar {
    /// Hidden with one tab.
    #[default]
    Auto,
    /// Always drawn.
    Always,
    /// Never drawn.
    Never,
}

/// How an over-long filename is cropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NameTruncate {
    /// End-crop when `ext` is a *rendered* column, middle-crop when it is not.
    #[default]
    Auto,
    /// Always keep the tail.
    Middle,
    /// Always keep the head.
    End,
}

/// `[panel]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelConfig {
    /// Show dotfiles (`Ctrl+H`).
    pub show_hidden: bool,
    /// Directories sort before files, on top of every sort order.
    ///
    pub directories_first: bool,
    /// `1.2M` instead of `362,333`.
    ///
    /// **the design wins over `examples/config.toml` here**: the default is
    /// `false`, because the reference screenshot shows exact byte counts.
    pub human_sizes: bool,
    /// Group the digits of an exact byte count.
    ///
    /// **the design wins over `examples/config.toml` here**: the example file
    /// omits the key; the design makes it `true`.
    pub thousands_separator: bool,
    /// Render directories as `[bin]`, so they are distinguishable without
    /// colour.
    pub dir_brackets: bool,
    /// How the `attr` column renders.
    ///
    /// **Added from the design**; `examples/config.toml` omits it.
    pub attr_style: AttrStyle,
    /// `chrono` format string for the `date` column.
    ///
    /// **the design wins over `examples/config.toml` here**: the default is
    /// `"%Y-%m-%d %H:%M"`, which is what renders `2026-08-12 02:40` and what
    /// the column's `min_chars = 16` is sized for.
    pub date_format: String,
    /// What a quick search matches.
    pub quick_search: QuickSearchMode,
    /// How case is handled.
    pub quick_search_case: QuickSearchCase,
    /// What bare digits do.
    pub digit_keys: DigitKeys,
    /// Nine, which is what makes single-key switching sufficient.
    ///
    pub max_tabs: usize,
    /// When to draw the tab bar.
    pub show_tab_bar: TabBar,
    /// Column layout.
    pub columns: ColumnsConfig,

    /// the design names this `panel.name_truncate`, while
    /// `examples/config.toml` puts it under `[panel.columns]`. Both are
    /// accepted; a value here wins.
    pub name_truncate: Option<NameTruncate>,
    /// the design names this `panel.name_min_width`; the example file puts it
    /// under `[panel.columns]`. Both are accepted; a value here wins.
    pub name_min_width: Option<u16>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            show_hidden: true,
            directories_first: true,
            // not examples/config.toml.
            human_sizes: false,
            thousands_separator: true,
            dir_brackets: true,
            attr_style: AttrStyle::Unix,
            date_format: "%Y-%m-%d %H:%M".to_string(),
            quick_search: QuickSearchMode::Prefix,
            quick_search_case: QuickSearchCase::Smart,
            digit_keys: DigitKeys::QuickSearch,
            max_tabs: 9,
            show_tab_bar: TabBar::Auto,
            columns: ColumnsConfig::default(),
            name_truncate: None,
            name_min_width: None,
        }
    }
}

impl PanelConfig {
    /// The effective name-cropping rule, resolving the two accepted locations.
    pub fn effective_name_truncate(&self) -> NameTruncate {
        self.name_truncate.unwrap_or(self.columns.name_truncate)
    }

    /// The effective minimum width for the flexible `name` column.
    pub fn effective_name_min_width(&self) -> u16 {
        self.name_min_width.unwrap_or(self.columns.name_min_width)
    }
}

/// `[panel.columns]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnsConfig {
    /// Left to right. This order also defines the sort keys: `Ctrl+1` sorts by
    /// the 1st column, and it keeps working for a hidden column.
    ///
    pub order: Vec<ColumnId>,
    /// Percentage of the panel's inner width. `name` is never listed: it is the
    /// flexible column and absorbs the leftover.
    pub width: HashMap<ColumnId, u16>,
    /// Characters below which a column is hidden rather than truncated.
    pub min_chars: HashMap<ColumnId, u16>,
    /// Hidden first when the panel narrows; first entry goes first.
    pub hide_priority: Vec<ColumnId>,
    /// How an over-long filename is cropped.
    pub name_truncate: NameTruncate,
    /// Below this, columns drop from the right instead.
    pub name_min_width: u16,
}

impl Default for ColumnsConfig {
    fn default() -> Self {
        Self {
            order: vec![
                ColumnId::Name,
                ColumnId::Ext,
                ColumnId::Size,
                ColumnId::Date,
                ColumnId::Attr,
            ],
            width: HashMap::from([
                (ColumnId::Ext, 6),
                (ColumnId::Size, 12),
                (ColumnId::Date, 20),
                (ColumnId::Attr, 8),
            ]),
            min_chars: HashMap::from([
                (ColumnId::Ext, 3),
                (ColumnId::Size, 7),
                (ColumnId::Date, 16),
                // Ten, not the four in the illustrative block: the
                // default `attr_style` is `"unix"`, whose text is
                // `drwxr-xr-x` - ten characters. At four the column was
                // allocated and then rendered as `drwx…`, which the design
                // rules out in as many words ("a column that cannot reach its
                // minimum is hidden rather than shown truncated"). the design
                // records these numbers as starting points to be tuned against
                // real widths. Set `attr = 4` alongside `attr_style = "dos"`.
                (ColumnId::Attr, 10),
            ]),
            hide_priority: vec![
                ColumnId::Attr,
                ColumnId::Ext,
                ColumnId::Size,
                ColumnId::Date,
            ],
            name_truncate: NameTruncate::Auto,
            name_min_width: 16,
        }
    }
}

// ----------------------------------------------------------- console --------

/// `[console] switch_on_run`.
///
/// > **Whether the screen switches to the console depends on whether the
/// > command still needs it.** Most commands typed at a file manager's command
/// > line finish before the eye moves - `mkdir`, `chmod`, `git add`, `touch`.
/// > Switching to the console for those shows a flash of a shell that is
/// > already back at a prompt and then has to be dismissed, which is two
/// > keystrokes and a jolt in exchange for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchOnRun {
    /// The panels stay. After [`ConsoleConfig::switch_delay`], the screen
    /// switches only if the shell has **not** returned to a prompt with an
    /// empty input line - the same test the design uses before writing a
    /// `cd`, so there is one definition of "the shell is idle" in the program
    /// rather than two that can disagree. The default.
    #[default]
    Auto,
    /// Always switch on `Enter`. The older behaviour, "for anyone who wants to
    /// watch everything".
    Always,
    /// Never switch; `Ctrl+O` is the only way in.
    Never,
}

/// `[console]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsoleConfig {
    /// Start a shell at all.
    ///
    /// the design makes the console unconditional and this defaults to `true`
    /// accordingly. It exists because the *absence* of a shell is a state the
    /// application must handle correctly whatever the reason for it - a
    /// headless [`crate::app::App`] has no PTY, a locked-down host may have no
    /// usable shell, and `console.shell` may simply be wrong - and the "until
    /// the PTY exists" command line is what takes over. Turning this off is
    /// the supported way to ask for that, rather than pointing `shell` at
    /// something that fails.
    pub enabled: bool,
    /// Empty means `$SHELL`.
    pub shell: String,
    /// Whether `Enter` on the command line puts the shell on screen.
    /// See [`SwitchOnRun`].
    pub switch_on_run: SwitchOnRun,
    /// How long [`SwitchOnRun::Auto`] waits before deciding.
    ///
    /// The command is given this long to finish. Whatever is still holding the
    /// terminal when it expires - a build, a pager, an editor, something asking
    /// a question - is worth looking at, and the screen switches to it.
    pub switch_delay: Timeout,
    /// Lines of scrollback kept.
    pub scrollback: usize,
    /// Command-history entries kept **by the fallback command line**.
    ///
    ///
    /// Not the shell's: with a live shell the history is the shell's own
    /// ("nothing is pushed anywhere here and there is no history file"), and
    /// nothing this application holds is consulted. What this caps is the
    /// list the pre-console command line keeps for the states that have no
    /// shell at all - `console.enabled = false`, a shell that would not
    /// start, a shell that has died.
    pub history_size: usize,
    /// How many rows the shell's own prompt and input line may take at the foot
    /// of the panel view.
    ///
    /// The command line **is** the shell's input line, prompt and all, and a
    /// two-line prompt needs two rows to be readable rather than nonsense. The
    /// bottom rows win when a prompt is taller than this, because that is the
    /// half being typed on. `1` restores the single row of v0.1 and v0.2.
    pub cmdline_rows: u16,
    /// Keep the shell's directory and the active panel's in step.
    ///
    pub sync_cwd: bool,
    /// Write the `OSC 7` / `OSC 133` prompt hooks into the shell at startup.
    ///
    ///
    /// Off means the panel stops following the shell unless the shell emits
    /// `OSC 7` on its own - fish does; bash and zsh do not - and the hooks can
    /// be installed by hand instead. See `crate::console::hooks` for the exact
    /// line and for what installing it costs.
    pub inject_hooks: bool,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shell: String::new(),
            switch_on_run: SwitchOnRun::Auto,
            // the own figure. Long enough that `mkdir`, `chmod`,
            // `git add` and `touch` are back at a prompt before it expires;
            // short enough that a build does not sit unseen.
            switch_delay: Timeout(Duration::from_millis(250)),
            scrollback: 10_000,
            history_size: 5_000,
            cmdline_rows: 2,
            sync_cwd: true,
            inject_hooks: true,
        }
    }
}

// -------------------------------------------------------------- open --------

/// `[open] execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutePolicy {
    /// Prompt: Execute / Open with… / View / Cancel. The default, because
    /// `Enter` is the key people press to navigate.
    #[default]
    Ask,
    /// Run it.
    Always,
    /// Treat it as data and open it with its association.
    Never,
}

/// `[open] execute_in`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecuteIn {
    /// The `Ctrl+O` console: output visible, stdin works, TUIs work.
    #[default]
    Console,
    /// Fork and detach. Suits GUI programs, discards output.
    Detached,
}

/// One `[[open.handlers]]` rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenHandler {
    /// What it matches.
    #[serde(rename = "match")]
    pub matcher: Matcher,
    /// The command, with `{file}` substituted.
    pub command: Vec<String>,
}

/// The `match = { … }` inline table shared by `[[open.handlers]]` and
/// `[[filetypes]]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Matcher {
    /// Extensions, without the dot.
    pub ext: Vec<String>,
    /// A MIME pattern such as `text/*`.
    pub mime: Option<String>,
    /// Mode bits, which the design makes rules matchable on alongside the
    /// extension. `None` means the rule says nothing about them.
    pub mode: Option<ModeMatch>,
}

/// What a `[[filetypes]]` rule can say about an entry's mode bits
/// ("rule-based on extension and mode bits").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModeMatch {
    /// The executable bit is set. Never the extension.
    Exec,
    /// A directory, or a symlink pointing at one.
    Dir,
    /// A symbolic link, wherever it points.
    Symlink,
}

/// `[open]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenConfig {
    /// What `Enter` does on a file with the executable bit set.
    pub execute: ExecutePolicy,
    /// Where it runs.
    pub execute_in: ExecuteIn,
    /// User associations, which win over the desktop's. First match is used.
    pub handlers: Vec<OpenHandler>,
}

impl Default for OpenConfig {
    fn default() -> Self {
        Self {
            execute: ExecutePolicy::Ask,
            execute_in: ExecuteIn::Console,
            handlers: Vec::new(),
        }
    }
}

// ------------------------------------------------------------ editor --------

/// `[editor]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// Empty means `$VISUAL`, then `$EDITOR`, then `nano`.
    ///
    /// **Empty is the default**, and that is what the design asks for:
    /// "Command from `editor.command`, default `nano`; `$VISUAL` then `$EDITOR`
    /// are consulted when the config value is empty." `nano` is named there as
    /// the end of that chain, not as a value written into the file - shipping
    /// `"nano"` here would make the other two unreachable for everyone who has
    /// not edited `config.toml`, which is the opposite of what consulting the
    /// environment is for.
    pub command: String,
    /// Argument template; `{file}` and `{line}` are substituted.
    pub args: Vec<String>,
    /// Confirm before `F4` opens a file larger than this.
    ///
    /// A big file in a line editor is slow to open and easy to open by
    /// accident - `F4` on the wrong row of a directory of disk images. The
    /// warning is a confirmation rather than a refusal: the file may genuinely
    /// need editing, and the reader is the one who knows. `0` turns it off.
    pub warn_above: ByteSize,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: vec!["{file}".to_string()],
            warn_above: ByteSize::mib(10),
        }
    }
}

// ------------------------------------------------------------ viewer --------

/// `[viewer] default_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewerMode {
    /// Decoded text.
    #[default]
    Text,
    /// Hex dump.
    Hex,
    /// The document rendered as what it is: a JSON tree, a page's text, a
    /// Markdown file with its headings made. See [`crate::viewer::render`].
    Render,
}

/// `[viewer.highlight] engine`.
///
/// **the design wins over `examples/config.toml` here**: the example file says
/// `"tree-sitter"`, but the decision on record is `syntect` + `two-face`.
/// `"tree-sitter"` deserializes to [`HighlightEngine::Syntect`]
/// and raises a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighlightEngine {
    /// `syntect` with the `default-fancy` pure-Rust backend.
    #[default]
    #[serde(alias = "tree-sitter", alias = "tree_sitter")]
    Syntect,
    /// No highlighting.
    None,
}

/// `[viewer.encoding]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerEncodingConfig {
    /// `auto`, or an `encoding_rs` label.
    pub default: String,
    /// Sniff with `chardetng` when `default = "auto"`.
    pub detect: bool,
    /// Used when detection fails.
    pub fallback: String,
    /// Honour a BOM over both of the above.
    pub bom: bool,
    /// The `F8` ring ("`F8` cycles through a **configurable**
    /// shortlist").
    ///
    /// Labels, plus `"auto"` for whatever this file was detected as. An empty
    /// or wholly unrecognised list falls back to the own ring, so a typo
    /// makes `F8` ordinary rather than broken.
    pub shortlist: Vec<String>,
}

impl Default for ViewerEncodingConfig {
    fn default() -> Self {
        Self {
            default: "auto".to_string(),
            detect: true,
            fallback: "windows-1252".to_string(),
            bom: true,
            shortlist: crate::viewer::encoding::DEFAULT_SHORTLIST
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

/// `[viewer.render]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerRenderConfig {
    /// Above this, mode 3 refuses and says so.
    ///
    /// It exists because mode 3 is the one view that cannot stream. Every
    /// other thing the viewer does is bounded by the window - a 40 GB file
    /// opens as fast as a 4 kB one - and a rendered view breaks that: there is
    /// no way to show a JSON document's tree, or to know that an object has
    /// twelve keys, without having read to the closing brace.
    ///
    /// So the whole file is read, and a file too big to read is refused by
    /// name rather than half-rendered. Rendering the first megabyte and
    /// calling it the document would be a wrong answer given confidently,
    /// which is worse than no answer.
    pub max_size: ByteSize,
}

impl Default for ViewerRenderConfig {
    fn default() -> Self {
        Self {
            // Bigger than any document written to be read and small enough
            // that reading it is not felt: a 16 MB JSON file renders in well
            // under the time a keystroke is allowed to take.
            max_size: ByteSize::mib(16),
        }
    }
}

/// `[viewer.highlight]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerHighlightConfig {
    /// Which engine.
    pub engine: HighlightEngine,
    /// Above this, highlighting is off and the file still opens.
    pub max_size: ByteSize,
    /// Reserved: per-language language servers. Not used; the design rejects
    /// the approach, and the table is accepted only so an old file still loads.
    #[serde(default)]
    pub lsp: HashMap<String, String>,
}

impl Default for ViewerHighlightConfig {
    fn default() -> Self {
        Self {
            engine: HighlightEngine::Syntect,
            max_size: ByteSize::mib(8),
            lsp: HashMap::new(),
        }
    }
}

/// Bits per column in a hex dump.
///
/// A dump of single bytes is the right view for a string table and the wrong
/// one for an array of 32-bit integers, where the number you want is spread
/// across four columns and byte-swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum HexGroup {
    /// One byte per column: the familiar dump.
    #[default]
    Bits8,
    /// 16-bit words.
    Bits16,
    /// 32-bit words.
    Bits32,
    /// 64-bit words.
    Bits64,
}

impl HexGroup {
    /// Bytes in one column.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Bits8 => 1,
            Self::Bits16 => 2,
            Self::Bits32 => 4,
            Self::Bits64 => 8,
        }
    }

    /// The `g` key: 8 → 16 → 32 → 64 → 8.
    pub const fn next(self) -> Self {
        match self {
            Self::Bits8 => Self::Bits16,
            Self::Bits16 => Self::Bits32,
            Self::Bits32 => Self::Bits64,
            Self::Bits64 => Self::Bits8,
        }
    }

    /// Bits, for messages and for `config.toml`.
    pub const fn bits(self) -> u16 {
        (self.bytes() as u16) * 8
    }
}

impl TryFrom<u16> for HexGroup {
    type Error = String;

    fn try_from(bits: u16) -> Result<Self, String> {
        match bits {
            8 => Ok(Self::Bits8),
            16 => Ok(Self::Bits16),
            32 => Ok(Self::Bits32),
            64 => Ok(Self::Bits64),
            other => Err(format!(
                "hex group must be 8, 16, 32 or 64 bits, not {other}"
            )),
        }
    }
}

impl From<HexGroup> for u16 {
    fn from(g: HexGroup) -> Self {
        g.bits()
    }
}

/// How a hex column is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HexFormat {
    /// Hex digits.
    #[default]
    Hex,
    /// Decimal, unsigned.
    Unsigned,
    /// Decimal, two's complement.
    Signed,
}

impl HexFormat {
    /// True when the column is a number rather than hex digits, which is the
    /// only case in which a sign means anything.
    pub const fn is_decimal(self) -> bool {
        matches!(self, Self::Unsigned | Self::Signed)
    }

    /// `d`: hex to decimal and back.
    ///
    /// `remembered` is the decimal reading last in use, so `d` twice is a round
    /// trip rather than a reset - going to decimal and coming back must not
    /// silently change what the user had chosen about sign.
    pub const fn toggle_base(self, remembered: Self) -> Self {
        if self.is_decimal() {
            Self::Hex
        } else if remembered.is_decimal() {
            remembered
        } else {
            Self::Unsigned
        }
    }

    /// `s`: unsigned to signed and back, in decimal only.
    ///
    /// `None` in hex, where the question does not arise: hex digits have no
    /// sign, and answering a question about sign with a change of base would
    /// be answering a different one.
    pub const fn toggle_sign(self) -> Option<Self> {
        match self {
            Self::Hex => None,
            Self::Unsigned => Some(Self::Signed),
            Self::Signed => Some(Self::Unsigned),
        }
    }

    /// The `d` key: hex → unsigned → signed → hex.
    pub const fn next(self) -> Self {
        match self {
            Self::Hex => Self::Unsigned,
            Self::Unsigned => Self::Signed,
            Self::Signed => Self::Hex,
        }
    }

    /// For the status line.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hex => "hex",
            Self::Unsigned => "unsigned",
            Self::Signed => "signed",
        }
    }
}

/// Byte order for columns wider than one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    /// Least significant byte first.
    #[default]
    Little,
    /// Most significant byte first.
    Big,
}

impl Endian {
    /// The `e` key.
    pub const fn flip(self) -> Self {
        match self {
            Self::Little => Self::Big,
            Self::Big => Self::Little,
        }
    }

    /// For the status line, which must always name it when it matters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Little => "LE",
            Self::Big => "BE",
        }
    }
}

/// `[viewer.hex]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HexConfig {
    /// Bits per column: 8, 16, 32 or 64.
    pub group: HexGroup,
    /// Hex, unsigned or signed.
    pub format: HexFormat,
    /// Byte order, for columns wider than a byte.
    pub endian: Endian,
}

/// `[viewer]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerConfig {
    /// Wrap long lines.
    pub wrap: bool,
    /// Show line numbers.
    pub line_numbers: bool,
    /// Give the viewer a cursor the arrow keys move.
    ///
    /// > `viewer.cursor = false` restores pure scrolling for anyone who wants
    /// > it.
    ///
    /// With it off the arrows scroll the page as they always did and there is
    /// nothing to select from, which is `less`'s behaviour and a reasonable
    /// thing to prefer.
    pub cursor: bool,
    /// Tab stop.
    pub tab_width: u16,
    /// What a file opens as when nothing more specific applies.
    ///
    /// The **floor**, not the last word: a binary opens in hex over the top of
    /// it, and a recognised format opens as a document over the top of it when
    /// [`ViewerConfig::open_as_document`] is on. It is what is left when
    /// neither of those applies, which is every ordinary text file.
    pub default_mode: ViewerMode,
    /// Whether a file whose format is recognised opens as what it is.
    ///
    /// On by default: a JSON file opens as a tree, a PNG as its dimensions and
    /// colour type. Off, both open at the floor above - which is what someone
    /// editing JSON wants, because the thing they are about to change is the
    /// source and not the shape it describes.
    pub open_as_document: bool,
    /// Whether a tracked file that differs from `HEAD` opens as its diff.
    ///
    /// On by default, and it is the same idea as `open_as_document` one step
    /// further: for a file you have edited since committing it, the document
    /// the file *is* right now is the change you made. `1` still gives the
    /// text and `2` the bytes, and an unmodified or untracked file is
    /// unaffected because there is no diff to show.
    pub diff_against_git: bool,
    /// Bytes per row in hex mode.
    pub hex_width: u16,
    /// How the bytes are grouped and read in hex mode.
    pub hex: HexConfig,
    /// Granularity of the background line index.
    pub index_chunk: ByteSize,
    /// Encoding handling.
    pub encoding: ViewerEncodingConfig,
    /// Syntax highlighting.
    pub highlight: ViewerHighlightConfig,
    /// Mode 3, the rendered view.
    pub render: ViewerRenderConfig,
    /// The largest selection `Ctrl+C` will copy.
    ///
    /// > Large selections are refused rather than truncated: `OSC 52` payloads
    /// > are base64 through the terminal's input path and some terminals cap
    /// > them.
    pub copy_max: ByteSize,
    /// How long the cursor must rest on a file before quick view opens it.
    /// 150 ms.
    ///
    /// > Held `Down` through a directory must not open a file per keystroke:
    /// > the pending file is replaced rather than queued, and only the one the
    /// > cursor rests on for `viewer.quick_view_delay` is opened.
    pub quick_view_delay: Timeout,
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            wrap: false,
            line_numbers: true,
            cursor: true,
            tab_width: 4,
            default_mode: ViewerMode::Text,
            open_as_document: true,
            diff_against_git: true,
            hex_width: 16,
            hex: HexConfig::default(),
            index_chunk: ByteSize::mib(1),
            encoding: ViewerEncodingConfig::default(),
            highlight: ViewerHighlightConfig::default(),
            render: ViewerRenderConfig::default(),
            copy_max: ByteSize::mib(1),
            quick_view_delay: Timeout(std::time::Duration::from_millis(150)),
        }
    }
}

// --------------------------------------------------------------- ops --------

/// `[ops]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpsConfig {
    /// Follow symlinks when copying.
    pub follow_symlinks: bool,
    /// Preserve mode and times.
    pub preserve_attrs: bool,
    /// `F8` trashes; `Shift+F8` always unlinks.
    pub trash_on_delete: bool,
    /// Prompt before overwriting.
    pub confirm_overwrite: bool,
    /// Run operations in the background queue.
    pub background_queue: bool,
    /// Below this, the progress dialog's per-file bar is omitted because it
    /// would only ever flash.
    pub file_bar_min_size: ByteSize,
    /// The window the **displayed** transfer rate is averaged over.
    /// Short, so a stall shows as one; the ETA uses the
    /// cumulative average instead, which must not jump.
    pub rate_window: Timeout,
    /// Below this many progress samples the rate renders as `-` and the ETA is
    /// omitted rather than guessed.
    pub rate_min_samples: usize,
    /// How far two mtimes may differ and still count as the same.
    ///
    ///
    /// Two seconds by default, "because FAT stores mtime at two-second
    /// resolution and a copy to a USB stick would otherwise report every file
    /// as changed".
    pub compare_mtime_slack: Timeout,
    /// Read the bytes as well. Off by default:
    ///
    /// > A byte-for-byte comparison of two trees is a different operation with
    /// > a different cost, and doing it behind a key that looks instant would
    /// > be the surprise the design avoids elsewhere.
    ///
    /// On, the comparison becomes a job with a progress dialog
    /// like any other long read.
    pub compare_contents: bool,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            preserve_attrs: true,
            trash_on_delete: true,
            confirm_overwrite: true,
            background_queue: true,
            file_bar_min_size: ByteSize::mib(1),
            rate_window: Timeout(std::time::Duration::from_secs(3)),
            rate_min_samples: 4,
            compare_mtime_slack: Timeout(std::time::Duration::from_secs(2)),
            compare_contents: false,
        }
    }
}

// ----------------------------------------------------------- archive --------

/// `[archive]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchiveConfig {
    /// `Enter` on an archive browses it.
    pub enter_on_click: bool,
    /// Empty means `$TMPDIR`.
    pub temp_dir: String,
    /// Warn above this when rewriting a compressed tar.
    pub rewrite_warn_size: ByteSize,
    /// Refuse above this.
    pub rewrite_max_size: ByteSize,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            enter_on_click: true,
            temp_dir: String::new(),
            rewrite_warn_size: ByteSize::mib(256),
            rewrite_max_size: ByteSize::mib(500),
        }
    }
}

// ------------------------------------------------------------ search --------

/// `[search] engine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    /// In-process, on ripgrep's own libraries. Nothing shells out.
    #[default]
    Internal,
    /// Reserved, and refused with a reason.
    ///
    /// the design offers this "for users who want to substitute their own
    /// tools" by spawning `fd`/`rg`, and the design settles that nothing in
    /// this program shells out. The value keeps deserializing so an existing
    /// config file still loads, and [`SearchConfig::engine_refusal`] is what
    /// says so: removing a key a user may have written is a worse outcome than
    /// a clear message.
    External,
}

/// What `search.engine = "external"` is told.
///
/// Both sections are named on purpose: one offers the key and the other rules
/// out the mechanism it would need, and a user who set it deserves to know
/// which rule won.
pub const EXTERNAL_ENGINE_REFUSAL: &str = "search.engine = \"external\" would run rg as a subprocess,; the internal engine is the supported path";

/// `[search]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Which engine.
    pub engine: SearchEngine,
    /// A file manager should show what is there.
    pub respect_gitignore: bool,
}

// `name_tool` and `content_tool` were here, holding "fd" and "rg". Nothing
// read them: the two tools they named would have been *spawned*, and this
// program does not spawn anything - the search runs in process on the
// `ignore` and `grep-*` libraries, which is what makes `engine = "external"`
// a refusal rather than a mode. A key that is declared, validated and
// documented but never read is a promise the program does not keep, so the
// keys are gone and `config.toml` now says one thing about the engine instead
// of two. An existing file that still has them gets the ordinary unknown-key
// warning, which is how the user finds out.

impl SearchConfig {
    /// The message a search must show before it runs, if there is one.
    ///
    /// `Some` only for `engine = "external"`. The search then runs on the
    /// internal engine anyway: silently running internal while the config says
    /// external is the one outcome worse than either, and refusing to search at
    /// all would leave a user whose config file says `external` with no way to
    /// search until they edit it.
    pub const fn engine_refusal(&self) -> Option<&'static str> {
        match self.engine {
            SearchEngine::Internal => None,
            SearchEngine::External => Some(EXTERNAL_ENGINE_REFUSAL),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            engine: SearchEngine::Internal,
            respect_gitignore: false,
        }
    }
}

// ----------------------------------------------------------- devices --------

/// `[devices]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicesConfig {
    /// Include proc, sysfs, cgroup and snap loopbacks.
    pub show_all: bool,
}

// ------------------------------------------------------------ remote --------

/// `[remote]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// `sftp` or `ftp`.
    pub default_protocol: String,
    /// Connect timeout.
    pub connect_timeout: Timeout,
    /// Keepalive interval.
    pub keepalive: Timeout,
    /// Largest file `F3` will fetch from a remote.
    pub view_max_size: ByteSize,
    /// Pooled connections per host.
    pub pool_size: usize,
    /// A changed host key is always fatal.
    pub strict_host_keys: bool,
    /// Whether an S3 connection may take credentials from the environment.
    ///
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and
    /// `AWS_REGION`: the names every AWS tool uses, so a shell already set up
    /// for `aws` or `rclone` needs nothing typed. What was typed always wins;
    /// the environment only fills in what was left blank.
    ///
    /// A setting rather than a rule because ambient credentials are a
    /// surprising thing to apply silently: a key left over in a shell would
    /// otherwise connect somebody to an account they did not name, and the
    /// panel would look exactly the same either way.
    pub s3_credentials_from_env: bool,
    /// How long a cached directory listing is served for (
    /// "Directory listings are cached with a short TTL").
    ///
    /// Short, because a remote directory changes under a panel exactly as a
    /// local one does and `Ctrl+R` is what a user reaches for when it has.
    ///
    pub listing_ttl: Timeout,
    /// How many chunks of a transfer are in flight at once (the design's
    /// pipelined reads).
    ///
    /// The one number that turns a round trip per chunk into a stream. Four is
    /// what a 100 ms link needs to keep a window full without the progress
    /// dialog getting ahead of what the server has acknowledged by more than
    /// this many chunks (the design I11).
    pub pipeline: usize,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            default_protocol: "sftp".to_string(),
            connect_timeout: Timeout(Duration::from_secs(10)),
            keepalive: Timeout(Duration::from_secs(30)),
            view_max_size: ByteSize::mib(32),
            pool_size: 4,
            strict_host_keys: true,
            s3_credentials_from_env: true,
            listing_ttl: Timeout(Duration::from_secs(2)),
            pipeline: 4,
        }
    }
}

// ---------------------------------------------------------- terminal --------

/// `[terminal] keyboard_protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyboardProtocol {
    /// Ask the terminal, and use the enhanced protocol if it says yes.
    #[default]
    Auto,
    /// Force the Kitty keyboard protocol on.
    Enhanced,
    /// Force it off.
    Legacy,
}

/// `[terminal] colors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorDepthSetting {
    /// Detect from `COLORTERM` and `TERM`.
    #[default]
    Auto,
    /// 24-bit colour.
    Truecolor,
    /// The 256-colour indexed palette.
    #[serde(rename = "256")]
    Indexed256,
    /// The 16 ANSI colours, which is when a theme's `fallback_16` matters.
    ///
    #[serde(rename = "16")]
    Ansi16,
}

/// `[terminal]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Force the keyboard protocol on or off.
    pub keyboard_protocol: KeyboardProtocol,
    /// Force the colour depth, or `Auto` to detect it.
    ///
    /// Detection reads `COLORTERM` and `TERM`, which are frequently wrong over
    /// ssh and inside multiplexers - in both directions. Same reasoning as
    /// `ui.ascii_borders`: detect, but let the file win.
    pub colors: ColorDepthSetting,
    /// Raw escape sequences bound to logical keys, for terminals that need it.
    ///
    ///
    /// Parsed and carried in v0.1; wiring it into the decoder is a later
    /// milestone. `"shift+f5" = "[15;2~"`.
    pub sequences: HashMap<String, String>,
}

// --------------------------------------------------------- filetypes --------

/// One `[[filetypes]]` rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FiletypeRule {
    /// What it matches.
    #[serde(rename = "match")]
    pub matcher: Matcher,
    /// A theme slot path such as `panel.archive_fg`, resolved with
    /// [`crate::config::Theme::slot`].
    pub slot: String,
}
