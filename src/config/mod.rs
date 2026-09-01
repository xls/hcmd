//! Configuration loading.
//!
//! Three files under `~/.config/holoscommander/`: `config.toml`, `keymap.toml`
//! and `themes/<name>.toml`.
//!
//! Two rules from the design shape everything here:
//!
//! * **Missing files are created on first run** with the `examples/` contents as
//!   commented defaults.
//! * **Loading never fails.** A malformed file degrades to the compiled-in
//!   defaults plus a warning naming the file, because a user must never be left
//!   without a file manager. An unknown key is a warning naming the file and,
//!   where it can be located, the line - never a silent ignore.

pub mod catalogue;
#[allow(
    clippy::module_inception,
    reason = "config.toml is one of three documents this module reads, and the \
              file that reads it is named after it"
)]
pub mod config;
pub mod emit;
pub mod keymap;
pub mod paths;
pub mod persist;
pub mod theme;
pub mod units;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub use config::{
    ArchiveConfig, AttrStyle, ColorDepthSetting, ColumnsConfig, Config, ConsoleConfig,
    DevicesConfig, DigitKeys, EditorConfig, Endian, ExecuteIn, ExecutePolicy, FiletypeRule,
    HexConfig, HexFormat, HexGroup, HighlightEngine, KeyboardProtocol, Matcher, ModeMatch,
    NameTruncate, OpenConfig, OpenHandler, OpsConfig, PanelConfig, QuickSearchCase,
    QuickSearchMode, RemoteConfig, SearchConfig, SearchEngine, SizeWalkStyle, SwitchOnRun, TabBar,
    TerminalConfig, UiConfig, ViewerConfig, ViewerEncodingConfig, ViewerHighlightConfig,
    ViewerMode, ViewerRenderConfig,
};
pub use keymap::{KeyContext, Keymap, Resolution};
pub use paths::{config_dir, start_dir, state_dir};
pub use theme::{ColorDepth, Named16, Rgb, Theme};
pub use units::{ByteSize, Timeout};

/// The `examples/` files, embedded so the binary is self-contained
/// (missing files are created on first run).
pub const EXAMPLE_CONFIG: &str = include_str!("../../examples/config.toml");
/// The shipped `keymap.toml`, which is also the compiled-in default layout.
pub const EXAMPLE_KEYMAP: &str = include_str!("../../examples/keymap.toml");
/// The shipped blue theme, which is also [`Theme::blue`].
pub const EXAMPLE_THEME_BLUE: &str = include_str!("../../themes/blue.toml");

/// The themes that ship with the program, compiled in.
///
/// `ui.theme = "dracula"` works on a machine with no theme files at all, which
/// is the point: a name in `config.toml` should not also require the user to go
/// and find a file. A `themes/<name>.toml` in the config directory still wins,
/// so any of these can be copied out and edited - the same rule applies to
/// settings, where the built-in shows through until something deliberately
/// overrides it.
///
/// Generated from one semantic palette (see the header of any of them), so a
/// slot missed in one would be missed in all of them; a test asserts every one
/// parses, covers every slot, and stays legible down to 16 colours.
pub const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("ayu-dark", include_str!("../../themes/ayu-dark.toml")),
    ("ayu-light", include_str!("../../themes/ayu-light.toml")),
    ("blue", include_str!("../../themes/blue.toml")),
    ("catppuccin", include_str!("../../themes/catppuccin.toml")),
    (
        "catppuccin-latte",
        include_str!("../../themes/catppuccin-latte.toml"),
    ),
    ("dracula", include_str!("../../themes/dracula.toml")),
    ("everforest", include_str!("../../themes/everforest.toml")),
    ("gruvbox", include_str!("../../themes/gruvbox.toml")),
    (
        "gruvbox-light",
        include_str!("../../themes/gruvbox-light.toml"),
    ),
    ("hackerman", include_str!("../../themes/hackerman.toml")),
    ("kanagawa", include_str!("../../themes/kanagawa.toml")),
    ("material", include_str!("../../themes/material.toml")),
    ("monokai", include_str!("../../themes/monokai.toml")),
    ("night-owl", include_str!("../../themes/night-owl.toml")),
    ("nord", include_str!("../../themes/nord.toml")),
    ("one-dark", include_str!("../../themes/one-dark.toml")),
    ("rose-pine", include_str!("../../themes/rose-pine.toml")),
    (
        "solarized-dark",
        include_str!("../../themes/solarized-dark.toml"),
    ),
    (
        "solarized-light",
        include_str!("../../themes/solarized-light.toml"),
    ),
    ("synthwave", include_str!("../../themes/synthwave.toml")),
    ("tokyo-night", include_str!("../../themes/tokyo-night.toml")),
];

/// A shipped theme by name.
pub fn builtin_theme(name: &str) -> Option<&'static str> {
    BUILTIN_THEMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| *text)
}

/// Every shipped theme's name, for `--check-config` and the F1 reference.
pub fn theme_names() -> Vec<&'static str> {
    BUILTIN_THEMES.iter().map(|(n, _)| *n).collect()
}

/// Every theme that can be chosen: the compiled-in set and whatever is in
/// `themes/` under the configuration directory, sorted, without duplicates.
///
/// A file of the same name as a built-in overrides it rather than appearing
/// twice, which is the rule the loader already follows. A hand-written theme
/// used to be invisible here: the picker offered the twenty-one built-ins and
/// nothing else, so the only way to reach your own was to name it in
/// `config.toml` and restart - and the picker is where people go to look.
///
/// A directory that cannot be read is not an error. There may not be one yet.
pub fn available_theme_names() -> Vec<String> {
    theme_names_in(paths::config_dir().ok().as_deref())
}

/// [`available_theme_names`] against a stated configuration directory.
///
/// Split out so the scan can be tested without reaching for the environment:
/// `set_var` is unsafe in edition 2024 and racy besides, and a test that
/// cannot run is how the last few defects stayed hidden.
pub fn theme_names_in(dir: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = theme_names().into_iter().map(str::to_string).collect();
    if let Some(dir) = dir
        && let Ok(entries) = std::fs::read_dir(dir.join("themes"))
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Everything loaded, plus every warning raised on the way.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// `config.toml`.
    pub config: Config,
    /// `keymap.toml`, over the built-in Total Commander layout.
    pub keymap: Keymap,
    /// `themes/<ui.theme>.toml`, over the built-in blue.
    pub theme: Theme,
    /// The directory everything was read from, when there was one.
    pub dir: Option<PathBuf>,
    /// Warnings, already phrased for a human and naming their file.
    pub warnings: Vec<String>,
}

impl Default for Loaded {
    /// The compiled-in defaults, with no directory consulted. This is what
    /// `App::headless` is built from in tests.
    fn default() -> Self {
        Self {
            config: Config::default(),
            keymap: Keymap::builtin(),
            theme: Theme::blue(),
            dir: None,
            warnings: Vec::new(),
        }
    }
}

/// Load from `~/.config/holoscommander/`, creating missing files.
///
/// Never fails: a directory that cannot be resolved or read yields the
/// compiled-in defaults and a warning.
pub fn load() -> Loaded {
    match config_dir() {
        Ok(dir) => {
            let mut loaded = load_from(&dir);
            loaded.warnings.splice(0..0, ensure_default_files(&dir));
            loaded
        }
        Err(err) => {
            let mut loaded = Loaded::default();
            loaded
                .warnings
                .push(format!("{err}; using the built-in defaults"));
            loaded
        }
    }
}

/// Load from an explicit directory. Does not create anything.
pub fn load_from(dir: &Path) -> Loaded {
    let mut warnings = Vec::new();

    // ------------------------------------------------------- config.toml ----
    let config_path = dir.join("config.toml");
    let config_text = read_optional(&config_path, &mut warnings);
    let mut config = match config_text.as_deref() {
        Some(text) => match toml::from_str::<Config>(text) {
            Ok(cfg) => {
                warnings.extend(unknown_keys(
                    text,
                    &config_path.display().to_string(),
                    &CONFIG_SCHEMA,
                ));
                warnings.extend(deprecated_values(text, &config_path.display().to_string()));
                cfg
            }
            Err(err) => {
                warnings.push(format!(
                    "{}: {err}; using the built-in defaults for this file",
                    config_path.display()
                ));
                Config::default()
            }
        },
        None => Config::default(),
    };

    // the design caps a panel at nine tabs, "which is what makes single-key
    // switching sufficient" - `Alt+1`-`Alt+9` is the entire keyspace. A larger
    // `panel.max_tabs` would open tabs no key can reach and that the state file
    // drops on the next start, so it is clamped, and clamping silently is
    // exactly the "never a silent ignore" the design rules out.
    let configured = config.panel.max_tabs;
    if !(1..=crate::panel::MAX_TABS).contains(&configured) {
        config.panel.max_tabs = configured.clamp(1, crate::panel::MAX_TABS);
        warnings.push(format!(
            "{}: panel.max_tabs = {configured} is outside 1..={}; using {}",
            config_path.display(),
            crate::panel::MAX_TABS,
            config.panel.max_tabs
        ));
    }

    // ------------------------------------------------------- keymap.toml ----
    let keymap_path = dir.join("keymap.toml");
    let keymap = match read_optional(&keymap_path, &mut warnings) {
        Some(text) => {
            if let Some(w) = stale_keymap_warning(&text) {
                warnings.push(w);
            }
            Keymap::load(&text, &keymap_path.display().to_string())
        }
        None => Keymap::builtin(),
    };
    warnings.extend(keymap.warnings.iter().cloned());

    // --------------------------------------------------- themes/<n>.toml ----
    let theme_path = dir.join("themes").join(format!("{}.toml", config.ui.theme));
    let theme = match read_optional(&theme_path, &mut warnings) {
        Some(text) => {
            let (theme, w) = Theme::parse(&text, &theme_path.display().to_string());
            warnings.extend(w);
            theme
        }
        // No file of that name, so try the compiled-in set before giving up.
        None => match builtin_theme(&config.ui.theme) {
            Some(text) => {
                let (theme, w) = Theme::parse(text, &config.ui.theme);
                warnings.extend(w);
                theme
            }
            None => {
                warnings.push(format!(
                    "{}: no such theme. Shipped themes are {}",
                    config.ui.theme,
                    theme_names().join(", ")
                ));
                Theme::blue()
            }
        },
    };

    // box drawing by default, detected from the locale, but the
    // config file wins where it says anything - over ssh the remote locale is
    // frequently wrong in both directions. Resolved here rather than in the
    // event loop because `App::perform_reload` (`Ctrl+Alt+R`) goes through this
    // function too, and doing it in the caller meant a reload silently threw
    // the locale detection away.
    config.ui.ascii_borders = crate::term::ascii_borders(&config.ui, config_text.as_deref());

    config.warnings = warnings.clone();
    Loaded {
        config,
        keymap,
        theme,
        dir: Some(dir.to_path_buf()),
        warnings,
    }
}

/// Read a file that is allowed not to exist. A read error that is *not*
/// `NotFound` is a warning.
fn read_optional(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warnings.push(format!("{}: {err}", path.display()));
            None
        }
    }
}

/// Create `config.toml`, `keymap.toml` and `themes/blue.toml` if they are
/// missing. Returns warnings; never fails.
///
/// `config.toml` is written **commented out**, which is what "commented
/// defaults" means and is also what keeps it honest: several values in
/// `examples/config.toml` disagree with the design (see
/// [`crate::config::config`]), and a fully commented file cannot disagree with
/// anything. `keymap.toml` and `themes/blue.toml` are written verbatim, because
/// they match the compiled-in defaults exactly and are files people edit.
pub fn ensure_default_files(dir: &Path) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Err(err) = fs::create_dir_all(dir.join("themes")) {
        warnings.push(format!("{}: {err}", dir.join("themes").display()));
        return warnings;
    }

    // settings files are written **fully commented out**, so a generated file
    // is a reference rather than an override. `keymap.toml` used to be written
    // live, and because a user file replaces the built-in bindings of every
    // action it mentions, that froze its owner at the defaults of the day they
    // first ran the program - a binding added later could never reach them, and
    // the failure was silent: the key simply did nothing. `config.toml` was
    // already written this way, which is why every change to it landed normally
    // over the same period.
    //
    // The theme is exempt: it is wholly the user's artifact and is read as
    // written, not layered under a built-in (the last line).
    let files: [(PathBuf, String); 3] = [
        (
            dir.join("config.toml"),
            comment_out(EXAMPLE_CONFIG, "settings"),
        ),
        (
            dir.join("keymap.toml"),
            comment_out(EXAMPLE_KEYMAP, "key bindings"),
        ),
        (
            dir.join("themes").join("blue.toml"),
            EXAMPLE_THEME_BLUE.to_string(),
        ),
    ];

    for (path, contents) in files {
        if path.exists() {
            continue;
        }
        if let Err(err) = fs::write(&path, contents) {
            warnings.push(format!("{}: could not create: {err}", path.display()));
        }
    }
    warnings
}

/// Turn a settings file into documentation: every line that is not already a
/// comment or blank is prefixed with `# `.
fn comment_out(text: &str, what: &str) -> String {
    let mut out = String::with_capacity(text.len() + 512);
    out.push_str(&format!(
        "# Holos Commander {what}, written by version {}.\n\
         #\n\
         # Every line below is commented out, and every value shown is the\n\
         # compiled-in default. Uncomment only what you want to change.\n\
         #\n\
         # A line you uncomment is yours from then on, and does NOT move when\n\
         # the default does - that is what an override means. Anything left\n\
         # commented keeps tracking the built-in, including changes made in\n\
         # later versions. `hcmd --check-config` reports where the two now\n\
         # differ.\n\
         #\n",
        env!("CARGO_PKG_VERSION")
    ));
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Warn when `keymap.toml` was generated by an older hcmd than this one.
///
/// The trap this closes cost an afternoon: a `keymap.toml` doubles as the
/// reference for what every key does, and it is written **once**, on first run,
/// and never rewritten. So a file from an old version silently lacks every
/// binding added since - which is how `Alt+V` looked unbindable when it was
/// bound all along - and any line it leaves active pins that version's default
/// over a newer one. The file stamps the version that wrote it into its own
/// header, so this is a comparison, not a guess. `None` for a hand-written file
/// with no stamp, and for one already current.
fn stale_keymap_warning(text: &str) -> Option<String> {
    let stamped = stamped_version(text)?;
    let current = env!("CARGO_PKG_VERSION");
    if !version_is_older(&stamped, current) {
        return None;
    }
    Some(format!(
        "keymap.toml was written by hcmd {stamped} and this is {current}; bindings added since \
         are not listed in it, and any line it leaves active pins that version's default. \
         Delete keymap.toml to regenerate an up-to-date reference - your own changes are its \
         uncommented lines, so copy those first."
    ))
}

/// The version a generated settings file stamps into its header, if it carries
/// one. [`comment_out`] writes `written by version X.` at the top.
fn stamped_version(text: &str) -> Option<String> {
    let marker = "written by version ";
    let start = text.find(marker)? + marker.len();
    let version: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let version = version.trim_end_matches('.').to_string();
    (!version.is_empty()).then_some(version)
}

/// True when `have` is an older version than `want`, comparing dotted numeric
/// components left to right. A component that will not parse counts as zero, so
/// a malformed stamp never triggers the warning by accident.
fn version_is_older(have: &str, want: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(have), parts(want));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

/// Bump the `written by version X` stamp in a generated file's header to the
/// current version, so it stops reading as stale after an update.
///
/// `keymap.toml` still uses this: it has no config struct to generate from, so
/// an update leaves the user's file alone and only moves its stamp forward.
fn bump_stamp(text: &str, current: &str) -> String {
    match stamped_version(text) {
        Some(old) => text.replacen(
            &format!("written by version {old}"),
            &format!("written by version {current}"),
            1,
        ),
        None => text.to_string(),
    }
}

/// The `(section, key)` pairs a `config.toml` sets **live** (uncommented), so a
/// regeneration can keep exactly those and comment the rest at their default.
///
/// An array-of-tables such as `[[filetypes]]` records `(name, "")`: the array
/// is set as a whole rather than key by key, which is the empty-key form the
/// generator asks about in [`emit::generate_preserving`].
fn live_keys(text: &str) -> std::collections::HashSet<(String, String)> {
    let mut set = std::collections::HashSet::new();
    let mut section = String::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("[[") {
            if let Some(name) = rest.split("]]").next() {
                let name = name.trim().to_string();
                set.insert((name.clone(), String::new()));
                section = name;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix('[') {
            if let Some(name) = rest.split(']').next() {
                section = name.trim().to_string();
            }
            continue;
        }
        let key: String = t
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !key.is_empty()
            && t.get(key.len()..)
                .map(str::trim_start)
                .is_some_and(|rest| rest.starts_with('='))
        {
            set.insert((section.clone(), key));
        }
    }
    set
}

/// Move an existing file aside to a dated backup before it is regenerated, so
/// the old file is "renamed to a backup with a date". Returns the backup path,
/// or `None` when there was nothing to move.
///
/// A rename, modelled on the atomic rename in [`persist`]: the old file is the
/// user's and is being replaced whole rather than edited, so moving it aside is
/// the honest thing, and a rename is what leaves no half-written file behind.
/// Today's date names it; a second run on the same day takes a numeric suffix
/// rather than overwriting the first backup.
fn backup_aside(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let date = chrono::Local::now().format("%Y-%m-%d");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut candidate = dir.join(format!("{name}.{date}.bak"));
    let mut n: u32 = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{name}.{date}-{n}.bak"));
        n = n.saturating_add(1);
    }
    fs::rename(path, &candidate)?;
    Ok(Some(candidate))
}

/// `--update-config`: regenerate `config.toml` from the config schema, keeping
/// the values the user set, and move the old file aside with today's date.
///
/// The old model appended commented examples of new options, which scattered
/// duplicate `# [section]` blocks and eventually corrupted the file. This
/// replaces it: the file is generated from the structs, so an update is a fresh
/// render that carries the user's set values forward and cannot double a
/// section. Parsing reuses the loader's tolerance (a value that does not parse
/// is dropped with a warning and its default written), and the old file is
/// backed up before the new one lands.
///
/// `keymap.toml` has no struct to generate from, so it is left as the user's,
/// its version stamp bumped when an older one would otherwise read as stale.
pub fn update_config() -> i32 {
    let dir = match config_dir() {
        Ok(dir) => dir,
        Err(err) => {
            println!("configuration directory: unavailable ({err})");
            return 1;
        }
    };
    println!("configuration directory: {}", dir.display());
    let current = env!("CARGO_PKG_VERSION");

    let config_path = dir.join("config.toml");
    match fs::read_to_string(&config_path) {
        Ok(user_text) => {
            let cfg = match toml::from_str::<Config>(&user_text) {
                Ok(cfg) => cfg,
                Err(err) => {
                    println!("  config.toml: {err}; unparsable values fall back to defaults");
                    Config::default()
                }
            };
            let live = live_keys(&user_text);
            let is_live =
                |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
            let regenerated = emit::generate_preserving(&cfg, &is_live, current);
            if regenerated == user_text {
                println!("  config.toml: already up to date");
            } else {
                match backup_aside(&config_path) {
                    Ok(backup) => match fs::write(&config_path, &regenerated) {
                        Ok(()) => match backup {
                            Some(b) => println!(
                                "  config.toml: regenerated (old file kept at {})",
                                b.display()
                            ),
                            None => println!("  config.toml: written"),
                        },
                        Err(err) => println!("  config.toml: could not write: {err}"),
                    },
                    Err(err) => println!("  config.toml: could not back up the old file: {err}"),
                }
            }
        }
        Err(_) => println!("  config.toml: not present, nothing to update"),
    }

    let keymap_path = dir.join("keymap.toml");
    match fs::read_to_string(&keymap_path) {
        Ok(user_text) => {
            let stale =
                stamped_version(&user_text).is_some_and(|old| version_is_older(&old, current));
            if stale {
                match fs::write(&keymap_path, bump_stamp(&user_text, current)) {
                    Ok(()) => {
                        println!(
                            "  keymap.toml: version stamp brought up to date for hcmd {current}"
                        );
                    }
                    Err(err) => println!("  keymap.toml: could not write: {err}"),
                }
            } else {
                println!("  keymap.toml: already up to date");
            }
        }
        Err(_) => println!("  keymap.toml: not present, nothing to update"),
    }
    0
}

/// Whether the user's `config.toml` text is what the current schema would
/// regenerate from it: current when a regenerate-preserving-its-values changes
/// nothing. This is the same computation [`update_config`] acts on, so the two
/// commands never disagree about whether a file is current.
fn config_is_current(user_text: &str, current: &str) -> bool {
    let cfg = toml::from_str::<Config>(user_text).unwrap_or_default();
    let live = live_keys(user_text);
    let is_live = |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
    emit::generate_preserving(&cfg, &is_live, current) == user_text
}

/// Deprecated values that still load, reported so they can be fixed.
fn deprecated_values(text: &str, file_label: &str) -> Vec<String> {
    let mut out = Vec::new();
    // The *value* on the `engine` line, not "the file mentions tree-sitter
    // anywhere" - the shipped `config.toml` explains the deprecation in a
    // comment, and matching that told everyone their file was deprecated.
    if let Some(line) = value_line(text, "engine", "tree-sitter") {
        out.push(format!(
            "{file_label}:{line}: viewer.highlight.engine = \"tree-sitter\" is deprecated; \
 settles on syntect. Reading it as \"syntect\"."
        ));
    }
    out
}

/// The 1-based line number where an uncommented `key = …` assigns a value
/// containing `needle`.
fn value_line(text: &str, key: &str, needle: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            let t = line.trim_start();
            if t.starts_with('#') {
                return false;
            }
            t.strip_prefix(key)
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .is_some_and(|value| value.contains(needle))
        })
        .map(|i| i.saturating_add(1))
}

// ------------------------------------------------------- unknown keys -------

/// A description of the keys a TOML file may contain, used to warn about the
/// ones it should not.
///
/// Deliberately shallow: it knows table names and the leaf keys inside them,
/// which is enough to catch a typo, and it does not try to re-implement the
/// serde types.
#[derive(Debug, Default)]
pub struct Schema {
    /// `key -> allowed child keys`. An empty child set means "any child is
    /// fine" (an open table such as `[terminal.sequences]`).
    tables: BTreeMap<String, Option<Vec<&'static str>>>,
    scalars: Vec<&'static str>,
}

impl Schema {
    /// Declare a top-level scalar key.
    pub fn scalar(&mut self, name: &'static str) {
        self.scalars.push(name);
    }

    /// Declare a table and the leaf keys it accepts.
    pub fn table(&mut self, name: &str, keys: &[&'static str]) {
        self.tables.insert(name.to_string(), Some(keys.to_vec()));
    }

    /// Declare a table whose keys are user-chosen.
    pub fn open_table(&mut self, name: &str) {
        self.tables.insert(name.to_string(), None);
    }

    fn knows_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    fn is_open(&self, name: &str) -> bool {
        matches!(self.tables.get(name), Some(None))
    }

    /// Is `key` allowed inside `table`? An empty `table` means the top level.
    fn allows(&self, table: &str, key: &str) -> bool {
        if table.is_empty() {
            return self.scalars.contains(&key) || self.tables.contains_key(key);
        }
        match self.tables.get(table) {
            Some(None) => true,
            Some(Some(keys)) => keys.contains(&key),
            None => false,
        }
    }
}

/// Walk a parsed TOML document against a [`Schema`] and report every key the
/// schema does not know, with a line number where one can be found.
///
/// The line is located by scanning the source for the key, which is exact for
/// the ordinary `key = value` and `[table]` forms this project's files use.
/// Where it cannot be located the warning names the file and the key alone,
/// which is still better than silence.
pub fn unknown_keys(text: &str, file_label: &str, schema: &Schema) -> Vec<String> {
    let Ok(doc) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_table(text, file_label, schema, "", &doc, &mut out);
    out.sort();
    out.dedup();
    out
}

/// One level of [`unknown_keys`]. `path` is the dotted table path, empty at the
/// top level.
fn walk_table(
    text: &str,
    file_label: &str,
    schema: &Schema,
    path: &str,
    table: &toml::Table,
    out: &mut Vec<String>,
) {
    let join = |key: &str| -> String {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    let where_ = || -> String {
        if path.is_empty() {
            "the top level".to_string()
        } else {
            format!("[{path}]")
        }
    };

    for (key, value) in table {
        let full = join(key);
        match value {
            toml::Value::Table(inner) => {
                if !schema.knows_table(&full) {
                    out.push(warn_at(
                        text,
                        file_label,
                        key,
                        &format!("unknown section [{full}]"),
                    ));
                    continue;
                }
                if schema.is_open(&full) {
                    continue;
                }
                walk_table(text, file_label, schema, &full, inner, out);
            }
            toml::Value::Array(items) => {
                let is_table_array = items.iter().all(toml::Value::is_table) && !items.is_empty();
                if is_table_array && schema.knows_table(&full) {
                    if schema.is_open(&full) {
                        continue;
                    }
                    for item in items {
                        if let Some(t) = item.as_table() {
                            walk_table(text, file_label, schema, &full, t, out);
                        }
                    }
                    continue;
                }
                if !schema.allows(path, key) {
                    out.push(warn_at(
                        text,
                        file_label,
                        key,
                        &format!("unknown key {key:?} in {}", where_()),
                    ));
                }
            }
            _ => {
                if !schema.allows(path, key) {
                    out.push(warn_at(
                        text,
                        file_label,
                        key,
                        &format!("unknown key {key:?} in {}", where_()),
                    ));
                }
            }
        }
    }
}

/// Find the 1-based line a key is written on.
fn find_line(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            let t = line.trim_start();
            if t.starts_with('#') {
                return false;
            }
            t.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
                || t.starts_with(&format!("[{key}]"))
                || t.starts_with(&format!("[[{key}]]"))
        })
        .map(|i| i + 1)
}

fn warn_at(text: &str, file_label: &str, key: &str, message: &str) -> String {
    match find_line(text, key) {
        Some(line) => format!("{file_label}:{line}: {message}"),
        None => format!("{file_label}: {message}"),
    }
}

/// The schema for `config.toml`.
static CONFIG_SCHEMA: std::sync::LazyLock<Schema> = std::sync::LazyLock::new(|| {
    let mut s = Schema::default();
    s.table(
        "ui",
        &[
            "theme",
            "ascii_borders",
            "show_menubar",
            "show_keybar",
            "mouse",
            "split_ratio",
            "confirm_exit",
        ],
    );
    s.table(
        "panel",
        &[
            "show_hidden",
            "git_status",
            "directories_first",
            "human_sizes",
            "thousands_separator",
            "dir_brackets",
            "attr_style",
            "date_format",
            "quick_search",
            "quick_search_case",
            "quick_search_filter",
            "size_walk_style",
            "digit_keys",
            "max_tabs",
            "show_tab_bar",
            "columns",
            "name_truncate",
            "name_min_width",
        ],
    );
    s.table(
        "panel.columns",
        &[
            "order",
            "width",
            "min_chars",
            "hide_priority",
            "name_truncate",
            "name_min_width",
        ],
    );
    s.open_table("panel.columns.width");
    s.open_table("panel.columns.min_chars");
    s.table(
        "console",
        &[
            "enabled",
            "shell",
            "switch_on_run",
            // how long `auto` waits before deciding.
            "switch_delay",
            "scrollback",
            "history_size",
            // the command line is the shell's own input line, and
            // a multi-line prompt needs the rows to say so.
            "cmdline_rows",
            "sync_cwd",
            "inject_hooks",
        ],
    );
    s.table("open", &["execute", "execute_in", "handlers"]);
    s.table("open.handlers", &["match", "command"]);
    s.open_table("open.handlers.match");
    s.table("editor", &["command", "args", "warn_above"]);
    s.table(
        "viewer",
        &[
            "wrap",
            "line_numbers",
            // the two keys. They have been on `ViewerConfig` since
            // v0.4 and were missing here, so writing either one was reported as
            // an unknown key - the design.
            "cursor",
            "copy_max",
            "tab_width",
            "default_mode",
            "open_as_document",
            "diff_against_git",
            "render",
            "hex_width",
            "hex",
            "index_chunk",
            "encoding",
            "highlight",
            // the quick view is the viewer, so its debounce is a
            // `[viewer]` key.
            "quick_view_delay",
        ],
    );
    // the grouping table, likewise missing entirely. Note that
    // 10.2.1's own example writes `width` here while the implementation spells
    // it `viewer.hex_width`; the schema accepts the implementation's spelling
    // only, because harmonising the two is a config change with a migration.
    s.table("viewer.hex", &["group", "format", "endian"]);
    s.table(
        "viewer.encoding",
        &["default", "detect", "fallback", "bom", "shortlist"],
    );
    s.table("viewer.highlight", &["engine", "max_size", "lsp"]);
    s.table("viewer.render", &["max_size"]);
    s.open_table("viewer.highlight.lsp");
    s.table(
        "ops",
        &[
            "follow_symlinks",
            "preserve_attrs",
            "trash_on_delete",
            "confirm_overwrite",
            "background_queue",
            // the progress dialog's per-file bar, and the two
            // transfer-rate numbers.
            "file_bar_min_size",
            "rate_window",
            "rate_min_samples",
            // the Shift+F2: how far two mtimes may drift and still
            // count as the same, and whether the bytes are read at all.
            "compare_mtime_slack",
            "compare_contents",
        ],
    );
    s.table(
        "archive",
        &[
            "enter_on_click",
            "temp_dir",
            "rewrite_warn_size",
            "rewrite_max_size",
        ],
    );
    s.table("search", &["engine", "respect_gitignore"]);
    s.table("devices", &["show_all"]);
    s.table(
        "remote",
        &[
            "default_protocol",
            "connect_timeout",
            "keepalive",
            "view_max_size",
            "pool_size",
            "strict_host_keys",
            "s3_credentials_from_env",
            "listing_ttl",
            "pipeline",
        ],
    );
    s.table("terminal", &["keyboard_protocol", "colors", "sequences"]);
    s.open_table("terminal.sequences");
    s.table("filetypes", &["match", "slot"]);
    s.open_table("filetypes.match");
    s
});

/// `--check-config`: validate and report.
///
/// Returns the process exit code: `0` when nothing was wrong, `1` when there
/// were warnings. Never a hard failure, because the point of the flag is to
/// tell you what is wrong, not to refuse.
pub fn check_config() -> i32 {
    let dir = config_dir();
    match &dir {
        Ok(dir) => println!("configuration directory: {}", dir.display()),
        Err(err) => println!("configuration directory: unavailable ({err})"),
    }

    let loaded = match &dir {
        Ok(dir) => load_from(dir),
        Err(_) => Loaded::default(),
    };

    for name in ["config.toml", "keymap.toml"] {
        match &dir {
            Ok(d) if d.join(name).exists() => println!("  {name}: present"),
            Ok(_) => println!("  {name}: absent, using built-in defaults"),
            Err(_) => println!("  {name}: not checked"),
        }
    }
    println!("  theme: {}", loaded.theme.name);
    println!(
        "  columns: {}",
        loaded
            .config
            .panel
            .columns
            .order
            .iter()
            .map(|c| c.id())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Whether `config.toml` matches what the schema would generate now. Same
    // computation `--update-config` acts on, so the two commands agree: a file
    // this flags is a file that command would rewrite.
    let mut problems = loaded.warnings.clone();
    if let Ok(d) = &dir {
        let path = d.join("config.toml");
        if let Ok(text) = fs::read_to_string(&path)
            && !config_is_current(&text, env!("CARGO_PKG_VERSION"))
        {
            problems.push(format!(
                "{}: not current for hcmd {}; run `hcmd --update-config` to regenerate it \
                 (your set values are kept, the old file is backed up)",
                path.display(),
                env!("CARGO_PKG_VERSION"),
            ));
        }
    }

    if problems.is_empty() {
        println!("\nno problems found");
        0
    } else {
        println!("\n{} problem(s):", problems.len());
        for w in &problems {
            println!("  {w}");
        }
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_older_compares_components_numerically() {
        assert!(version_is_older("0.1.0", "0.9.6"));
        assert!(version_is_older("0.9.5", "0.9.6"));
        assert!(!version_is_older("0.9.6", "0.9.6"), "equal is not older");
        assert!(!version_is_older("1.0.0", "0.9.6"), "newer is not older");
        assert!(
            version_is_older("0.9.6", "0.10.0"),
            "10 is a bigger minor than 9, not a smaller string"
        );
    }

    #[test]
    fn a_keymap_from_an_older_version_is_flagged_stale() {
        let text =
            "# Holos Commander key bindings, written by version 0.1.0.\n#\ncopy = [\"f5\"]\n";
        let warning = stale_keymap_warning(text).expect("a stale-keymap warning");
        assert!(
            warning.contains("0.1.0"),
            "names the old version: {warning}"
        );
        assert!(
            warning.contains("Delete keymap.toml"),
            "says how to fix it: {warning}"
        );
    }

    #[test]
    fn a_keymap_written_by_this_version_is_quiet() {
        let current = env!("CARGO_PKG_VERSION");
        let text = format!("# Holos Commander key bindings, written by version {current}.\n");
        assert!(
            stale_keymap_warning(&text).is_none(),
            "a current file must not warn"
        );
    }

    #[test]
    fn a_regenerated_file_keeps_set_values_live_and_comments_the_rest() {
        // The heart of the new model: a user file that sets a handful of values
        // is regenerated so those stay live and every other option is written
        // commented at its default, with no section written twice.
        let current = env!("CARGO_PKG_VERSION");
        let user = "[panel]\ngit_status = false\nmax_tabs = 3\n";
        let cfg: Config = toml::from_str(user).expect("a partial file");
        let live = live_keys(user);
        let is_live =
            |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
        let out = emit::generate_preserving(&cfg, &is_live, current);

        assert!(
            out.contains("\ngit_status = false\n"),
            "a set value stays live: {out}"
        );
        assert!(
            out.contains("\nmax_tabs = 3\n"),
            "and so does the other one"
        );
        assert!(
            out.contains("# show_hidden = true"),
            "an unset option is written commented at its default: {out}"
        );
        // No duplicate section headers anywhere.
        for header in ["[panel]", "[ui]", "[viewer]", "[panel.columns]"] {
            let n = out.matches(&format!("\n{header}\n")).count();
            assert_eq!(n, 1, "{header} appears {n} times, not once");
        }
        // And it round-trips.
        let back: Config = toml::from_str(&out).expect("the regenerated file parses");
        assert!(!back.panel.git_status);
        assert_eq!(back.panel.max_tabs, 3);
    }

    #[test]
    fn a_regeneration_is_idempotent_and_check_agrees() {
        // Regenerating a file the schema already produced changes nothing, and
        // `config_is_current` (what --check-config uses) agrees with that, so
        // "update, then check" comes out clean.
        let current = env!("CARGO_PKG_VERSION");
        let user = "[panel]\ngit_status = false\n";
        let cfg: Config = toml::from_str(user).expect("a partial file");
        let live = live_keys(user);
        let is_live =
            |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
        let once = emit::generate_preserving(&cfg, &is_live, current);
        assert!(
            config_is_current(&once, current),
            "the freshly generated file reads as current"
        );

        let cfg2: Config = toml::from_str(&once).expect("the generated file parses");
        let live2 = live_keys(&once);
        let is_live2 =
            |section: &str, key: &str| live2.contains(&(section.to_string(), key.to_string()));
        let twice = emit::generate_preserving(&cfg2, &is_live2, current);
        assert_eq!(once, twice, "a second regeneration is a no-op");
    }

    #[test]
    fn a_nested_value_and_a_filetype_entry_round_trip_through_regeneration() {
        let current = env!("CARGO_PKG_VERSION");
        let user = "[panel.columns]\nname_min_width = 24\n\n\
             [[filetypes]]\nmatch = { ext = [\"log\"] }\nslot = \"panel.exec_fg\"\n";
        let cfg: Config = toml::from_str(user).expect("nested + array file");
        let live = live_keys(user);
        let is_live =
            |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
        let out = emit::generate_preserving(&cfg, &is_live, current);

        assert!(
            out.contains("\nname_min_width = 24\n"),
            "the nested [panel.columns] value is live: {out}"
        );
        assert!(
            out.contains("[[filetypes]]\nmatch = { ext = [\"log\"] }"),
            "the user's filetype rule is written live, not the default one: {out}"
        );
        let back: Config = toml::from_str(&out).expect("regenerated nested + array parses");
        assert_eq!(back.panel.columns.name_min_width, 24);
        assert_eq!(back.filetypes.len(), 1);
        assert_eq!(
            back.filetypes.first().map(|r| r.slot.as_str()),
            Some("panel.exec_fg")
        );
    }

    #[test]
    fn an_unparsable_value_falls_back_to_the_default_when_regenerated() {
        // The loader already drops a value that will not parse and keeps the
        // default; the regeneration is built on the parsed config, so the bad
        // value is simply gone and its default is what gets written.
        let current = env!("CARGO_PKG_VERSION");
        let user = "[panel]\nmax_tabs = \"three\"\ngit_status = false\n";
        // The whole file fails to parse (one bad scalar), so the loader hands
        // back the default, exactly as `update_config` does.
        assert!(toml::from_str::<Config>(user).is_err());
        let cfg = Config::default();
        let live = live_keys(user);
        let is_live =
            |section: &str, key: &str| live.contains(&(section.to_string(), key.to_string()));
        let out = emit::generate_preserving(&cfg, &is_live, current);
        // `git_status` was live in the file, so it stays live, but at the
        // default value the parsed config carries.
        assert!(out.contains("\ngit_status = true\n"), "{out}");
        let back: Config = toml::from_str(&out).expect("the regenerated file parses");
        assert_eq!(
            back.panel.max_tabs, 9,
            "the bad value is gone, default written"
        );
    }

    #[test]
    fn the_old_file_is_moved_aside_to_a_dated_backup() {
        // The other half of the update: before the regenerated file is written,
        // the old one is renamed to a dated backup, so nothing the user had is
        // lost. A second backup on the same day does not clobber the first.
        let dir = std::env::temp_dir().join(format!("hcmd-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        fs::write(&path, "old contents").expect("write");

        let first = backup_aside(&path)
            .expect("backup succeeds")
            .expect("something was moved");
        assert!(!path.exists(), "the original was moved, not copied");
        assert!(first.exists(), "the backup is there");
        assert!(
            first
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("config.toml.") && n.ends_with(".bak")),
            "the backup is dated: {first:?}"
        );
        assert_eq!(
            fs::read_to_string(&first).expect("read backup"),
            "old contents"
        );

        // A second same-day backup lands beside the first rather than over it.
        fs::write(&path, "newer contents").expect("write again");
        let second = backup_aside(&path)
            .expect("second backup succeeds")
            .expect("moved again");
        assert_ne!(first, second, "the first backup is not overwritten");
        assert_eq!(
            fs::read_to_string(&first).expect("read backup"),
            "old contents"
        );

        // Nothing to move is not an error.
        let absent = dir.join("keymap.toml");
        assert!(backup_aside(&absent).expect("no error").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_multi_line_doc_comment_becomes_multiple_hash_lines() {
        // A field whose doc spans several lines writes several `#` lines above
        // its option. `s3_credentials_from_env` has a multi-paragraph doc.
        let out = emit::generate(&Config::default());
        let hashes = out
            .lines()
            .skip_while(|l| !l.contains("AWS_ACCESS_KEY_ID"))
            .take_while(|l| !l.trim_start().starts_with("s3_credentials_from_env"))
            .filter(|l| l.trim_start().starts_with('#'))
            .count();
        assert!(
            hashes >= 3,
            "expected several comment lines above the option, got {hashes}"
        );
    }

    #[test]
    fn a_keymap_with_no_version_stamp_is_quiet() {
        // A hand-written file carries no header, so there is nothing to be
        // stale against and nothing to warn about.
        assert!(stale_keymap_warning("copy = [\"ctrl+c\"]\n").is_none());
        assert_eq!(stamped_version("no header here\n"), None);
    }

    /// A throwaway configuration directory holding `themes/<name>.toml`.
    fn with_themes(tag: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hcmd-themescan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("themes")).expect("themes dir");
        for name in names {
            std::fs::write(dir.join("themes").join(name), b"").expect("theme file");
        }
        dir
    }

    #[test]
    fn a_hand_written_theme_is_offered_beside_the_shipped_ones() {
        let dir = with_themes("mine", &["mine.toml"]);
        let names = theme_names_in(Some(&dir));
        assert!(names.iter().any(|n| n == "mine"), "{names:?}");
        assert!(
            names.iter().any(|n| n == "dracula"),
            "the built-ins are still there: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_named_after_a_built_in_appears_once() {
        let dir = with_themes("override", &["nord.toml"]);
        let names = theme_names_in(Some(&dir));
        assert_eq!(
            names.iter().filter(|n| *n == "nord").count(),
            1,
            "overriding a built-in must not list it twice: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_toml_files_count() {
        let dir = with_themes("junk", &["notes.txt", "README.md", "real.toml"]);
        let names = theme_names_in(Some(&dir));
        assert!(names.iter().any(|n| n == "real"), "{names:?}");
        for unwanted in ["notes", "README"] {
            assert!(
                !names.iter().any(|n| n == unwanted),
                "{unwanted} listed: {names:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_directory_is_the_shipped_set_and_no_complaint() {
        assert_eq!(theme_names_in(None).len(), theme_names().len());
        let missing = std::env::temp_dir().join("hcmd-themescan-does-not-exist");
        assert_eq!(theme_names_in(Some(&missing)).len(), theme_names().len());
    }

    use super::*;
    use crate::panel::ColumnId;

    #[test]
    fn the_shipped_example_config_deserializes_completely() {
        let cfg: Config = toml::from_str(EXAMPLE_CONFIG).expect("examples/config.toml parses");
        assert_eq!(cfg.ui.theme, "blue");
        assert_eq!(cfg.panel.max_tabs, 9);
        assert_eq!(cfg.panel.columns.order.first(), Some(&ColumnId::Name));
        assert_eq!(cfg.viewer.index_chunk, ByteSize::mib(1));
        assert_eq!(cfg.archive.rewrite_max_size, ByteSize::mib(500));
        assert_eq!(cfg.remote.connect_timeout.duration().as_secs(), 10);
        // The example file's deprecated engine value still loads.
        assert_eq!(cfg.viewer.highlight.engine, HighlightEngine::Syntect);
    }

    #[test]
    fn the_shipped_example_config_raises_no_unknown_key_warnings() {
        let warnings = unknown_keys(EXAMPLE_CONFIG, "examples/config.toml", &CONFIG_SCHEMA);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn the_search_section_round_trips_and_names_every_key_it_has() {
        // the design names exactly two keys and the design
        // adds none: everything else the Find dialog collects is per-search
        // state. the design makes the generated reference file a promise
        // about the defaults, so the schema, the struct and the shipped file
        // have to agree - and this is what notices when they stop.
        let text = "[search]\nengine = \"external\"\nrespect_gitignore = true\n";
        let cfg: Config = toml::from_str(text).expect("a [search] section");
        assert_eq!(cfg.search.engine, SearchEngine::External);
        assert!(cfg.search.respect_gitignore);
        assert!(unknown_keys(text, "config.toml", &CONFIG_SCHEMA).is_empty());

        let written = toml::to_string(&cfg.search).expect("round trip");
        let back: SearchConfig = toml::from_str(&written).expect("round trip");
        assert_eq!(back.engine, cfg.search.engine);
        assert_eq!(back.respect_gitignore, cfg.search.respect_gitignore);
        assert!(
            unknown_keys(
                &format!("[search]\n{written}"),
                "config.toml",
                &CONFIG_SCHEMA
            )
            .is_empty(),
            "every key `SearchConfig` writes is a key the schema knows: {written}"
        );

        // The defaults are the reference file's, and a file manager finds what
        // is on disk.
        let shipped: Config = toml::from_str(EXAMPLE_CONFIG).expect("examples/config.toml");
        assert_eq!(shipped.search.engine, SearchEngine::Internal);
        assert!(!shipped.search.respect_gitignore);

        // `name_tool` and `content_tool` are gone from the struct, the schema
        // and the reference file together. Nothing ever read them, and a file
        // that still carries one is told so rather than being quietly obeyed
        // in a way it never was.
        let warnings = unknown_keys(
            "[search]\nname_tool = \"fd\"\ncontent_tool = \"rg\"\n",
            "config.toml",
            &CONFIG_SCHEMA,
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("name_tool")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_external_search_engine_is_refused_with_the_reason() {
        // the design offers the key, the design rules out the subprocess it
        // would need, and the design resolves it: the search
        // says which rule won and then runs the internal engine. Silently
        // running internal while the config says external is the one outcome
        // worse than either.
        let mut cfg = SearchConfig::default();
        assert_eq!(cfg.engine_refusal(), None);
        cfg.engine = SearchEngine::External;
        let refusal = cfg.engine_refusal().expect("a reason");
        assert!(refusal.contains("subprocess"), "{refusal}");
        assert!(refusal.contains("subprocess"), "{refusal}");
        assert!(refusal.contains("internal engine"), "{refusal}");
        // And the shipped file explains it where a user would look for it.
        assert!(
            EXAMPLE_CONFIG.contains("the internal engine is the supported path"),
            "in config.toml"
        );
    }

    #[test]
    fn spec_9_3s_own_console_snippet_loads_exactly_as_written() {
        // the design prints this block. A user who copies it out of the spec
        // has to get what it says - `switch_on_run` was a `bool`, so
        // `"auto"` was a type error and `switch_delay` an unknown key.
        let text = "[console]\nswitch_on_run = \"auto\"\nswitch_delay  = \"250ms\"\n";
        let cfg: Config = toml::from_str(text).expect("the own snippet");
        assert_eq!(cfg.console.switch_on_run, SwitchOnRun::Auto);
        assert_eq!(cfg.console.switch_delay.duration().as_millis(), 250);
        assert!(
            unknown_keys(text, "config.toml", &CONFIG_SCHEMA).is_empty(),
            "and neither key is a warning"
        );

        for (written, expected) in [
            ("always", SwitchOnRun::Always),
            ("never", SwitchOnRun::Never),
            ("auto", SwitchOnRun::Auto),
        ] {
            let cfg: Config =
                toml::from_str(&format!("[console]\nswitch_on_run = \"{written}\"\n"))
                    .expect("every value the design names");
            assert_eq!(cfg.console.switch_on_run, expected);
        }

        // And the default is `auto`, which is the whole point of the decision
        // recorded in.
        assert_eq!(ConsoleConfig::default().switch_on_run, SwitchOnRun::Auto);
        assert_eq!(
            ConsoleConfig::default().switch_delay.duration().as_millis(),
            250
        );
    }

    #[test]
    fn spec_defaults_win_over_the_example_file() {
        // the design against examples/config.toml. See config::config.
        let d = PanelConfig::default();
        assert!(!d.human_sizes, "exact byte counts by default");
        assert!(d.thousands_separator);
        assert!(d.dir_brackets);
        assert_eq!(d.attr_style, AttrStyle::Unix);
        assert_eq!(d.date_format, "%Y-%m-%d %H:%M");
        assert_eq!(d.quick_search, QuickSearchMode::Prefix);
        assert_eq!(d.quick_search_case, QuickSearchCase::Smart);
        assert_eq!(d.digit_keys, DigitKeys::QuickSearch);
    }

    #[test]
    fn a_partial_file_keeps_every_other_default() {
        let cfg: Config = toml::from_str("[panel]\nmax_tabs = 3\n").expect("partial config");
        assert_eq!(cfg.panel.max_tabs, 3);
        assert_eq!(cfg.ui.theme, "blue");
        assert!(cfg.ui.show_keybar);
        assert_eq!(cfg.panel.columns.order.len(), 6);
    }

    #[test]
    fn an_unknown_key_is_a_warning_with_a_line_number() {
        let text = "[ui]\ntheme = \"blue\"\nwibble = 3\n\n[nope]\nx = 1\n";
        let warnings = unknown_keys(text, "config.toml", &CONFIG_SCHEMA);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w == "config.toml:3: unknown key \"wibble\" in [ui]"),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w == "config.toml:5: unknown section [nope]"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_broken_config_file_degrades_to_defaults() {
        let dir = std::env::temp_dir().join(format!("hcmd-cfg-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        fs::write(dir.join("config.toml"), "this is not = = toml").expect("write");
        let loaded = load_from(&dir);
        assert_eq!(loaded.config.ui.theme, "blue");
        assert!(!loaded.warnings.is_empty());
        assert!(loaded.warnings.iter().any(|w| w.contains("config.toml")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_are_created_and_then_load_clean() {
        let dir = std::env::temp_dir().join(format!("hcmd-create-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let warnings = ensure_default_files(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(dir.join("config.toml").exists());
        assert!(dir.join("keymap.toml").exists());
        assert!(dir.join("themes").join("blue.toml").exists());

        let loaded = load_from(&dir);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        // The created config.toml is entirely commented, so the compiled-in
        // the design defaults apply.
        assert!(!loaded.config.panel.human_sizes);
        assert_eq!(loaded.theme, Theme::blue());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn effective_name_settings_accept_both_locations() {
        let a: Config = toml::from_str("[panel.columns]\nname_min_width = 20\n").expect("a");
        assert_eq!(a.panel.effective_name_min_width(), 20);
        let b: Config =
            toml::from_str("[panel]\nname_min_width = 24\n[panel.columns]\nname_min_width = 20\n")
                .expect("b");
        assert_eq!(b.panel.effective_name_min_width(), 24);
    }

    #[test]
    fn the_shipped_example_config_raises_no_warning_of_any_kind() {
        // The file `ensure_default_files` hands a new user must not tell them
        // their configuration is wrong. This caught the deprecation scanner
        // matching "tree-sitter" inside the comment that *explains* the
        // deprecation.
        let warnings = deprecated_values(EXAMPLE_CONFIG, "examples/config.toml");
        assert!(warnings.is_empty(), "{warnings:#?}");
        let warnings = unknown_keys(EXAMPLE_CONFIG, "examples/config.toml", &CONFIG_SCHEMA);
        assert!(warnings.is_empty(), "{warnings:#?}");
    }

    #[test]
    fn the_deprecated_engine_alias_still_warns_for_a_real_old_file() {
        let warnings =
            deprecated_values("[viewer.highlight]\nengine = \"tree-sitter\"\n", "old.toml");
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(
            warnings
                .first()
                .is_some_and(|w| w.starts_with("old.toml:2:")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn the_first_run_config_file_is_entirely_inert() {
        // the keymap gets the same treatment, and for a sharper reason - a user
        // file *replaces* the built-in bindings of every action it mentions, so
        // a live generated file freezes its owner at the day they installed and
        // a binding added later can never reach them.
        let keymap = comment_out(EXAMPLE_KEYMAP, "key bindings");
        for line in keymap.lines() {
            let t = line.trim_start();
            assert!(
                t.is_empty() || t.starts_with('#'),
                "uncommented binding in the first-run keymap: {line:?}"
            );
        }
        assert!(
            Keymap::load(&keymap, "keymap.toml").warnings.is_empty(),
            "the generated keymap must load cleanly"
        );

        // Every line commented, so the compiled-in defaults are what runs and
        // the file cannot disagree with them - including the [[filetypes]]
        // block, which would otherwise *replace* the default rule set with an
        // empty matcher.
        let generated = comment_out(EXAMPLE_CONFIG, "settings");
        for line in generated.lines() {
            let t = line.trim_start();
            assert!(
                t.is_empty() || t.starts_with('#'),
                "uncommented line in the first-run file: {line:?}"
            );
        }
        let cfg: Config = toml::from_str(&generated).expect("the commented file parses");
        assert_eq!(
            cfg.filetypes.len(),
            Config::default().filetypes.len(),
            "the default filetype rules survive"
        );
    }
}
