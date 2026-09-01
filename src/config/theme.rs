//! Themes.
//!
//! A theme is a TOML file declaring RGB colours against semantic slots. The
//! `blue` theme - Total Commander / Norton Commander - is **compiled in**, so
//! the application works with no config directory at all, and a missing slot in
//! a user theme falls back to the blue value rather than being an error.

use std::collections::HashMap;
use std::fmt;

use ratatui::style::Color;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 24-bit colour, parsed from `"#RRGGBB"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Rgb {
    /// From components.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `"#RRGGBB"`. The `#` is required, because a bare `0000A8` in TOML
    /// is ambiguous with a number.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix('#')
            .ok_or_else(|| format!("{text:?} is not a colour; expected \"#RRGGBB\""))?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{text:?} is not six hex digits after the '#'"));
        }
        let pair = |i: usize| -> Result<u8, String> {
            let s = hex
                .get(i..i + 2)
                .ok_or_else(|| format!("{text:?} is short"))?;
            u8::from_str_radix(s, 16).map_err(|e| format!("{text:?}: {e}"))
        };
        Ok(Self::new(pair(0)?, pair(2)?, pair(4)?))
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Rgb;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a colour such as \"#0000A8\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Rgb, E> {
                Rgb::parse(v).map_err(E::custom)
            }
        }
        d.deserialize_str(V)
    }
}

/// One of the sixteen ANSI colours, for the `[fallback_16]` table
/// ("Themes must declare a `fallback_16` mapping so a 16-color
/// session is not a guess").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Named16 {
    /// ANSI 0.
    Black,
    /// ANSI 1.
    Red,
    /// ANSI 2.
    Green,
    /// ANSI 3.
    Yellow,
    /// ANSI 4.
    Blue,
    /// ANSI 5.
    Magenta,
    /// ANSI 6.
    Cyan,
    /// ANSI 7.
    White,
    /// ANSI 8.
    BrightBlack,
    /// ANSI 9.
    BrightRed,
    /// ANSI 10.
    BrightGreen,
    /// ANSI 11.
    BrightYellow,
    /// ANSI 12.
    BrightBlue,
    /// ANSI 13.
    BrightMagenta,
    /// ANSI 14.
    BrightCyan,
    /// ANSI 15.
    BrightWhite,
}

impl Named16 {
    /// Every name, in ANSI order.
    pub const ALL: &'static [Self] = &[
        Self::Black,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
        Self::White,
        Self::BrightBlack,
        Self::BrightRed,
        Self::BrightGreen,
        Self::BrightYellow,
        Self::BrightBlue,
        Self::BrightMagenta,
        Self::BrightCyan,
        Self::BrightWhite,
    ];

    /// The name as written in a theme file.
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Blue => "blue",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
            Self::White => "white",
            Self::BrightBlack => "bright_black",
            Self::BrightRed => "bright_red",
            Self::BrightGreen => "bright_green",
            Self::BrightYellow => "bright_yellow",
            Self::BrightBlue => "bright_blue",
            Self::BrightMagenta => "bright_magenta",
            Self::BrightCyan => "bright_cyan",
            Self::BrightWhite => "bright_white",
        }
    }

    /// Parse a theme-file name.
    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|n| n.id() == s)
    }

    /// The ratatui colour.
    pub const fn to_ratatui(self) -> Color {
        match self {
            Self::Black => Color::Black,
            Self::Red => Color::Red,
            Self::Green => Color::Green,
            Self::Yellow => Color::Yellow,
            Self::Blue => Color::Blue,
            Self::Magenta => Color::Magenta,
            Self::Cyan => Color::Cyan,
            // ratatui calls ANSI 7 `Gray` and ANSI 15 `White`.
            Self::White => Color::Gray,
            Self::BrightBlack => Color::DarkGray,
            Self::BrightRed => Color::LightRed,
            Self::BrightGreen => Color::LightGreen,
            Self::BrightYellow => Color::LightYellow,
            Self::BrightBlue => Color::LightBlue,
            Self::BrightMagenta => Color::LightMagenta,
            Self::BrightCyan => Color::LightCyan,
            Self::BrightWhite => Color::White,
        }
    }

    /// A representative RGB value, used when quantizing without a
    /// `[fallback_16]` entry.
    pub const fn rgb(self) -> Rgb {
        match self {
            Self::Black => Rgb::new(0x00, 0x00, 0x00),
            Self::Red => Rgb::new(0xA8, 0x00, 0x00),
            Self::Green => Rgb::new(0x00, 0xA8, 0x00),
            Self::Yellow => Rgb::new(0xA8, 0x54, 0x00),
            Self::Blue => Rgb::new(0x00, 0x00, 0xA8),
            Self::Magenta => Rgb::new(0xA8, 0x00, 0xA8),
            Self::Cyan => Rgb::new(0x00, 0xA8, 0xA8),
            Self::White => Rgb::new(0xC0, 0xC0, 0xC0),
            Self::BrightBlack => Rgb::new(0x54, 0x54, 0x54),
            Self::BrightRed => Rgb::new(0xFF, 0x54, 0x54),
            Self::BrightGreen => Rgb::new(0x54, 0xFF, 0x54),
            Self::BrightYellow => Rgb::new(0xFF, 0xFF, 0x54),
            Self::BrightBlue => Rgb::new(0x54, 0x54, 0xFF),
            Self::BrightMagenta => Rgb::new(0xFF, 0x54, 0xFF),
            Self::BrightCyan => Rgb::new(0x54, 0xFF, 0xFF),
            Self::BrightWhite => Rgb::new(0xFF, 0xFF, 0xFF),
        }
    }
}

impl fmt::Display for Named16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl Serialize for Named16 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for Named16 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Named16;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 16-colour name such as \"bright_cyan\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Named16, E> {
                Named16::from_id(v).ok_or_else(|| E::custom(format!("unknown colour name {v:?}")))
            }
        }
        d.deserialize_str(V)
    }
}

/// How many colours the session can actually show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum ColorDepth {
    /// `COLORTERM` says `truecolor` or `24bit`.
    #[default]
    TrueColor,
    /// `TERM` contains `256color`.
    Indexed256,
    /// Anything else.
    Ansi16,
}

impl ColorDepth {
    /// Read the environment (truecolor, else 256, else 16).
    pub fn detect() -> Self {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        if colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit") {
            return Self::TrueColor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") {
            return Self::Indexed256;
        }
        Self::Ansi16
    }

    /// [`ColorDepth::detect`], with `[terminal] colors` overriding it.
    ///
    ///
    /// Detection reads `COLORTERM` and `TERM`, which are frequently wrong over
    /// ssh and inside multiplexers, so the file wins where it says anything -
    /// the same rule `ui.ascii_borders` follows.
    pub fn resolve(setting: crate::config::ColorDepthSetting) -> Self {
        use crate::config::ColorDepthSetting as S;
        match setting {
            S::Auto => Self::detect(),
            S::Truecolor => Self::TrueColor,
            S::Indexed256 => Self::Indexed256,
            S::Ansi16 => Self::Ansi16,
        }
    }
}

// -------------------------------------------------------- slot groups -------

/// Build a theme group: a struct of [`Rgb`] slots, plus an all-`Option` "raw"
/// twin that a partial user theme deserializes into, plus the merge that lays
/// the raw over the built-in blue.
macro_rules! theme_group {
    ($name:ident, $raw:ident, $( $slot:ident = $key:literal ),+ $(,)?) => {
        /// A resolved theme group. Every slot is populated, falling back to the
        /// built-in blue theme where the user's file was silent.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        pub struct $name {
            $(
                #[doc = concat!("The `", $key, "` slot.")]
                #[serde(rename = $key)]
                pub $slot: Rgb,
            )+
        }

        #[derive(Debug, Clone, Copy, Default, Deserialize)]
        #[serde(default)]
        struct $raw {
            $(
                #[serde(rename = $key)]
                $slot: Option<Rgb>,
            )+
        }

        impl $name {
            /// Every slot name in this group, as spelled in a theme file.
            pub const SLOTS: &'static [&'static str] = &[ $( $key ),+ ];

            fn merge(self, raw: $raw) -> Self {
                Self { $( $slot: raw.$slot.unwrap_or(self.$slot), )+ }
            }

            /// Look a slot up by its name within the group.
            pub fn slot(&self, name: &str) -> Option<Rgb> {
                match name {
                    $( $key => Some(self.$slot), )+
                    _ => None,
                }
            }
        }
    };
}

theme_group!(
    PanelTheme,
    RawPanelTheme,
    bg = "bg",
    fg = "fg",
    dir_fg = "dir_fg",
    exec_fg = "exec_fg",
    link_fg = "link_fg",
    archive_fg = "archive_fg",
    marked_fg = "marked_fg",
    cursor_bg = "cursor_bg",
    cursor_fg = "cursor_fg",
    cursor_bg_unfocused = "cursor_bg_unfocused",
    cursor_fg_unfocused = "cursor_fg_unfocused",
    inactive_cursor_bg = "inactive_cursor_bg",
    inactive_cursor_fg = "inactive_cursor_fg",
    border = "border",
    inactive_border = "inactive_border",
    header_bg = "header_bg",
    header_fg = "header_fg",
    status_bg = "status_bg",
    status_fg = "status_fg",
);

theme_group!(
    CmdlineTheme,
    RawCmdlineTheme,
    bg = "bg",
    fg = "fg",
    prompt_fg = "prompt_fg",
    caret = "caret",
    caret_unfocused = "caret_unfocused",
);

theme_group!(
    KeybarTheme,
    RawKeybarTheme,
    number_fg = "number_fg",
    label_bg = "label_bg",
    label_fg = "label_fg",
);

theme_group!(
    DialogTheme,
    RawDialogTheme,
    bg = "bg",
    fg = "fg",
    title = "title",
    border = "border",
    button = "button",
    button_focus = "button_focus",
    input_bg = "input_bg",
    input_fg = "input_fg",
);

theme_group!(
    ViewerTheme,
    RawViewerTheme,
    bg = "bg",
    fg = "fg",
    line_numbers = "line_numbers",
    match_ = "match",
    current_match = "current_match",
    hex_offset = "hex_offset",
    hex_ascii = "hex_ascii",
    selection_bg = "selection_bg",
    selection_fg = "selection_fg",
);

theme_group!(
    DiffTheme,
    RawDiffTheme,
    added = "added",
    removed = "removed",
    header = "header",
    marker = "marker",
);

theme_group!(
    SynTheme,
    RawSynTheme,
    keyword = "keyword",
    string = "string",
    comment = "comment",
    type_ = "type",
    function = "function",
    number = "number",
    constant = "constant",
    operator = "operator",
    punctuation = "punctuation",
    variable = "variable",
);

/// A complete theme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Theme {
    /// The theme's own name, as declared in the file.
    pub name: String,
    /// `panel.*`.
    pub panel: PanelTheme,
    /// `cmdline.*`.
    pub cmdline: CmdlineTheme,
    /// `keybar.*`.
    pub keybar: KeybarTheme,
    /// `dialog.*`.
    pub dialog: DialogTheme,
    /// `viewer.*`.
    pub viewer: ViewerTheme,
    /// `syn.*`.
    pub syn: SynTheme,
    /// `diff.*`: the colours of a rendered diff. Their own group rather than
    /// four more `syn` slots, because a diff is not syntax: the same file
    /// highlighted as Rust and shown as a diff wants both palettes at once.
    pub diff: DiffTheme,
    /// `[fallback_16]`: the mapping used when the terminal has only 16 colours.
    pub fallback_16: HashMap<Rgb, Named16>,
}

/// The on-disk shape. Everything optional, so a partial theme file works.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawTheme {
    name: Option<String>,
    panel: RawPanelTheme,
    cmdline: RawCmdlineTheme,
    keybar: RawKeybarTheme,
    dialog: RawDialogTheme,
    viewer: RawViewerTheme,
    syn: RawSynTheme,
    diff: RawDiffTheme,
    fallback_16: HashMap<Rgb, Named16>,
}

impl Theme {
    /// The compiled-in Total Commander blue, byte-for-byte
    /// `themes/blue.toml`.
    pub fn blue() -> Self {
        let c = Rgb::parse;
        // These literals are the theme file. `expect` is safe here in the sense
        // the hard rules allow: the values are literals in this function, so a
        // failure is a compile-time typo, not a runtime condition. Even so we
        // fall back rather than panic.
        let hex = |s: &str, fallback: Rgb| c(s).unwrap_or(fallback);
        let k = Rgb::new(0, 0, 0);
        Self {
            name: "blue".to_string(),
            panel: PanelTheme {
                bg: hex("#0000A8", k),
                fg: hex("#C0C0C0", k),
                dir_fg: hex("#FFFFFF", k),
                exec_fg: hex("#54FF54", k),
                link_fg: hex("#54FFFF", k),
                archive_fg: hex("#FF54FF", k),
                marked_fg: hex("#FFFF54", k),
                cursor_bg: hex("#00A8A8", k),
                cursor_fg: hex("#000000", k),
                cursor_bg_unfocused: hex("#007878", k),
                cursor_fg_unfocused: hex("#000000", k),
                inactive_cursor_bg: hex("#2020B0", k),
                inactive_cursor_fg: hex("#C0C0C0", k),
                border: hex("#54FFFF", k),
                inactive_border: hex("#8080C0", k),
                header_bg: hex("#0000A8", k),
                header_fg: hex("#FFFF54", k),
                status_bg: hex("#0000A8", k),
                status_fg: hex("#54FFFF", k),
            },
            cmdline: CmdlineTheme {
                bg: hex("#000000", k),
                fg: hex("#C0C0C0", k),
                prompt_fg: hex("#54FF54", k),
                caret: hex("#FFFFFF", k),
                caret_unfocused: hex("#A0A0A0", k),
            },
            keybar: KeybarTheme {
                number_fg: hex("#C0C0C0", k),
                label_bg: hex("#00A8A8", k),
                label_fg: hex("#000000", k),
            },
            dialog: DialogTheme {
                bg: hex("#C0C0C0", k),
                fg: hex("#000000", k),
                title: hex("#000080", k),
                border: hex("#000000", k),
                button: hex("#000080", k),
                button_focus: hex("#A80000", k),
                input_bg: hex("#000080", k),
                input_fg: hex("#FFFFFF", k),
            },
            viewer: ViewerTheme {
                bg: hex("#000000", k),
                fg: hex("#C0C0C0", k),
                line_numbers: hex("#606060", k),
                match_: hex("#FFFF54", k),
                current_match: hex("#FF5454", k),
                hex_offset: hex("#54FFFF", k),
                hex_ascii: hex("#54FF54", k),
                selection_bg: hex("#FF54FF", k),
                selection_fg: hex("#000000", k),
            },
            syn: SynTheme {
                keyword: hex("#FF54FF", k),
                string: hex("#54FF54", k),
                comment: hex("#808080", k),
                type_: hex("#54FFFF", k),
                function: hex("#FFFF54", k),
                number: hex("#FF8754", k),
                constant: hex("#FF8754", k),
                operator: hex("#C0C0C0", k),
                punctuation: hex("#A0A0A0", k),
                variable: hex("#C0C0C0", k),
            },
            diff: DiffTheme {
                // Green added, red removed, and a dimmer pair for the parts
                // that are about the diff rather than in it. Chosen to read on
                // the built-in blue; every theme may override them and none
                // has to, because a theme file that says nothing about `diff`
                // inherits these.
                added: hex("#54FF54", k),
                removed: hex("#FF5454", k),
                header: hex("#54FFFF", k),
                marker: hex("#808080", k),
            },
            fallback_16: HashMap::from([
                (hex("#0000A8", k), Named16::Blue),
                // The two cursor-bar backgrounds that nearest-match gets wrong.
                // `#007878` is a dark teal and `#2020B0` a blue a shade off the
                // panel background, so the crude RGB match sends the first to
                // `cyan` - the same as the focused bar `#00A8A8` - and the
                // second to `blue`, the panel background itself, which makes
                // the inactive panel's cursor bar vanish entirely. the design
                // three distinguishable bars and says neither cursor ever
                // disappears, and the design says a 16-colour session is not a
                // guess, so both are named here: the command-line-focused bar
                // keeps a visible bar in bright blue, and the inactive panel's
                // is grey - present, and clearly subordinate.
                (hex("#007878", k), Named16::BrightBlue),
                (hex("#2020B0", k), Named16::BrightBlack),
                (hex("#C0C0C0", k), Named16::White),
                (hex("#FFFFFF", k), Named16::BrightWhite),
                (hex("#00A8A8", k), Named16::Cyan),
                (hex("#54FFFF", k), Named16::BrightCyan),
                (hex("#FFFF54", k), Named16::BrightYellow),
                (hex("#54FF54", k), Named16::BrightGreen),
                (hex("#FF54FF", k), Named16::BrightMagenta),
                (hex("#FF5454", k), Named16::BrightRed),
                (hex("#FF8754", k), Named16::BrightRed),
                (hex("#000000", k), Named16::Black),
                (hex("#808080", k), Named16::BrightBlack),
            ]),
        }
    }

    /// Parse a theme file. A missing slot falls back to the blue value; a
    /// malformed one is a warning, never an error.
    ///
    /// The returned `Vec<String>` holds warnings.
    pub fn parse(text: &str, file_label: &str) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let raw: RawTheme = match toml::from_str(text) {
            Ok(raw) => raw,
            Err(err) => {
                warnings.push(format!(
                    "{file_label}: {err}; falling back to the built-in blue theme"
                ));
                return (Self::blue(), warnings);
            }
        };
        warnings.extend(super::unknown_keys(text, file_label, &THEME_SCHEMA));

        let base = Self::blue();
        let mut theme = Self {
            name: raw.name.clone().unwrap_or(base.name.clone()),
            panel: base.panel.merge(raw.panel),
            cmdline: base.cmdline.merge(raw.cmdline),
            keybar: base.keybar.merge(raw.keybar),
            dialog: base.dialog.merge(raw.dialog),
            viewer: base.viewer.merge(raw.viewer),
            syn: base.syn.merge(raw.syn),
            diff: base.diff.merge(raw.diff),
            fallback_16: base.fallback_16,
        };
        // The file's entries are laid *over* the built-in table rather than
        // replacing it. A `[fallback_16]` entry is a mapping for one colour,
        // not a declaration that the rest should be guessed at: a theme that
        // names ten colours and forgets an eleventh gets the built-in mapping
        // for that eleventh, which is the whole point of the "so a
        // 16-color session is not a guess". Every colour the file does name
        // wins, so a theme can still override any of them.
        theme.fallback_16.extend(raw.fallback_16);
        (theme, warnings)
    }

    /// Resolve a dotted slot path such as `"panel.archive_fg"`, used by the
    /// `[[filetypes]]` rules.
    ///
    /// The `viewer.match` and `syn.type` slots are spelled exactly as in the
    /// theme file; the Rust fields carry a trailing underscore because those
    /// are keywords.
    pub fn slot(&self, path: &str) -> Option<Rgb> {
        let (group, name) = path.split_once('.')?;
        match group {
            "panel" => self.panel.slot(name),
            "cmdline" => self.cmdline.slot(name),
            "keybar" => self.keybar.slot(name),
            "dialog" => self.dialog.slot(name),
            "viewer" => self.viewer.slot(name),
            "syn" => self.syn.slot(name),
            _ => None,
        }
    }

    /// Quantize a theme colour for the session's colour depth.
    ///
    /// At 16 colours the theme's own `[fallback_16]` table is consulted first,
    /// because the design says a 16-colour session must not be a guess; only
    /// when the exact colour is absent from the table do we fall back to
    /// nearest-match.
    pub fn quantize(&self, rgb: Rgb, depth: ColorDepth) -> Color {
        match depth {
            ColorDepth::TrueColor => Color::Rgb(rgb.r, rgb.g, rgb.b),
            ColorDepth::Indexed256 => Color::Indexed(to_ansi256(rgb)),
            ColorDepth::Ansi16 => self
                .fallback_16
                .get(&rgb)
                .copied()
                .unwrap_or_else(|| nearest_16(rgb))
                .to_ratatui(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::blue()
    }
}

/// Map an RGB triple to the xterm 256-colour cube, choosing between the cube
/// and the greyscale ramp by whichever is closer.
pub fn to_ansi256(rgb: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let idx = |v: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| u32::from(v).abs_diff(u32::from(**l)))
            .map_or(0, |(i, _)| i)
    };
    let (ri, gi, bi) = (idx(rgb.r), idx(rgb.g), idx(rgb.b));
    let cube_rgb = Rgb::new(
        LEVELS.get(ri).copied().unwrap_or(0),
        LEVELS.get(gi).copied().unwrap_or(0),
        LEVELS.get(bi).copied().unwrap_or(0),
    );

    // The 24-step greyscale ramp, indices 232..=255.
    let luma = (u32::from(rgb.r) * 299 + u32::from(rgb.g) * 587 + u32::from(rgb.b) * 114) / 1000;
    let step = (luma.saturating_sub(8) * 24 / 247).min(23);
    let grey_level = u8::try_from(8 + step * 10).unwrap_or(u8::MAX);
    let grey_rgb = Rgb::new(grey_level, grey_level, grey_level);

    if distance(rgb, grey_rgb) < distance(rgb, cube_rgb) {
        u8::try_from(232 + step).unwrap_or(255)
    } else {
        u8::try_from(16 + 36 * ri + 6 * gi + bi).unwrap_or(15)
    }
}

/// Nearest of the sixteen ANSI colours, used only when a theme's
/// `[fallback_16]` has no entry for the exact colour.
pub fn nearest_16(rgb: Rgb) -> Named16 {
    Named16::ALL
        .iter()
        .copied()
        .min_by_key(|n| distance(rgb, n.rgb()))
        .unwrap_or(Named16::White)
}

/// Squared euclidean distance in RGB space. Crude, but it is only a tiebreak
/// for colours a theme forgot to map.
fn distance(a: Rgb, b: Rgb) -> u32 {
    let d = |x: u8, y: u8| {
        let v = u32::from(x).abs_diff(u32::from(y));
        v * v
    };
    d(a.r, b.r) + d(a.g, b.g) + d(a.b, b.b)
}

/// The schema used to report unknown keys in a theme file.
static THEME_SCHEMA: std::sync::LazyLock<super::Schema> = std::sync::LazyLock::new(|| {
    let mut s = super::Schema::default();
    s.scalar("name");
    s.table("panel", PanelTheme::SLOTS.iter().copied());
    s.table("cmdline", CmdlineTheme::SLOTS.iter().copied());
    s.table("keybar", KeybarTheme::SLOTS.iter().copied());
    s.table("dialog", DialogTheme::SLOTS.iter().copied());
    s.table("viewer", ViewerTheme::SLOTS.iter().copied());
    s.table("syn", SynTheme::SLOTS.iter().copied());
    s.open_table("fallback_16");
    s
});

#[cfg(test)]
mod tests {
    /// The WCAG contrast ratio between two colours: 1.0 for identical, up to
    /// 21.0 for black against white. 4.5 is the AA floor for normal text.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        fn luminance(c: Rgb) -> f64 {
            fn channel(v: u8) -> f64 {
                let s = f64::from(v) / 255.0;
                if s <= 0.03928 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            }
            0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
        }
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Every shipped theme parses, covers every slot, and stays legible at 16
    /// colours. Generated from one palette (see `themes/`), so a slot
    /// missed in one would be missed in all twenty - which is exactly the kind
    /// of thing a test catches and a reading does not.
    #[test]
    fn every_shipped_theme_loads_and_stays_legible() {
        const SHIPPED: &[(&str, &str)] = &[
            ("blue", include_str!("../../themes/blue.toml")),
            ("dracula", include_str!("../../themes/dracula.toml")),
            ("tokyo-night", include_str!("../../themes/tokyo-night.toml")),
            ("nord", include_str!("../../themes/nord.toml")),
            ("gruvbox", include_str!("../../themes/gruvbox.toml")),
            ("catppuccin", include_str!("../../themes/catppuccin.toml")),
            (
                "solarized-dark",
                include_str!("../../themes/solarized-dark.toml"),
            ),
            ("everforest", include_str!("../../themes/everforest.toml")),
            ("kanagawa", include_str!("../../themes/kanagawa.toml")),
            ("one-dark", include_str!("../../themes/one-dark.toml")),
            ("monokai", include_str!("../../themes/monokai.toml")),
            ("rose-pine", include_str!("../../themes/rose-pine.toml")),
            ("night-owl", include_str!("../../themes/night-owl.toml")),
            ("ayu-dark", include_str!("../../themes/ayu-dark.toml")),
            ("material", include_str!("../../themes/material.toml")),
            ("hackerman", include_str!("../../themes/hackerman.toml")),
            ("synthwave", include_str!("../../themes/synthwave.toml")),
            (
                "solarized-light",
                include_str!("../../themes/solarized-light.toml"),
            ),
            (
                "catppuccin-latte",
                include_str!("../../themes/catppuccin-latte.toml"),
            ),
            (
                "gruvbox-light",
                include_str!("../../themes/gruvbox-light.toml"),
            ),
            ("ayu-light", include_str!("../../themes/ayu-light.toml")),
        ];
        assert_eq!(SHIPPED.len(), 21, "twenty themes plus the default");

        for (name, text) in SHIPPED {
            let (theme, warnings) = Theme::parse(text, name);
            assert!(warnings.is_empty(), "{name} warns: {warnings:#?}");
            let t = &theme;
            assert_eq!(t.name, *name, "the name field must match the file");

            // A mark is shown by colour alone, so `marked_fg` must not merely
            // differ from the background - it must read on it, and read the
            // other way round too, because a marked row under the cursor turns
            // the bar that colour with the background as its text. The light
            // themes used to pick a soft amber that washed out on a near-white
            // panel, which is invisible at exactly the moment - the cursor on a
            // marked file - that it matters most.
            let marked = contrast(t.panel.marked_fg, t.panel.bg);
            assert!(
                marked >= 4.5,
                "{name}: marked_fg on bg is {marked:.2}:1, below the 4.5 legibility floor"
            );

            for depth in [
                ColorDepth::TrueColor,
                ColorDepth::Indexed256,
                ColorDepth::Ansi16,
            ] {
                let q = |rgb| t.quantize(rgb, depth);
                // "A 16-color session must still be legible."
                assert_ne!(
                    q(t.panel.fg),
                    q(t.panel.bg),
                    "{name} at {depth:?}: text on background"
                );
                assert_ne!(
                    q(t.panel.cursor_fg),
                    q(t.panel.cursor_bg),
                    "{name} at {depth:?}: the cursor bar"
                );
                assert_ne!(
                    q(t.panel.cursor_bg),
                    q(t.panel.bg),
                    "{name} at {depth:?}: the cursor bar against the panel"
                );
                assert_ne!(
                    q(t.cmdline.fg),
                    q(t.cmdline.bg),
                    "{name} at {depth:?}: the command line"
                );
                assert_ne!(
                    q(t.keybar.label_fg),
                    q(t.keybar.label_bg),
                    "{name} at {depth:?}: the key bar"
                );
                assert_ne!(
                    q(t.dialog.fg),
                    q(t.dialog.bg),
                    "{name} at {depth:?}: dialogs"
                );
                // the selection, drawn in the slots. A
                // theme that let these two quantize together would paint a
                // selection as an unreadable block on a 16-colour session, and
                // one whose `selection_bg` matched `viewer.bg` would not paint
                // it at all.
                assert_ne!(
                    q(t.viewer.selection_fg),
                    q(t.viewer.selection_bg),
                    "{name} at {depth:?}: the viewer selection"
                );
                assert_ne!(
                    q(t.viewer.selection_bg),
                    q(t.viewer.bg),
                    "{name} at {depth:?}: the selection against the viewer"
                );
            }
        }
    }

    use super::*;

    #[test]
    fn parses_the_shipped_blue_theme_into_the_compiled_in_one() {
        let (theme, warnings) = Theme::parse(include_str!("../../themes/blue.toml"), "blue.toml");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(theme, Theme::blue());
    }

    #[test]
    fn a_missing_slot_falls_back_to_blue() {
        let (theme, warnings) = Theme::parse("name = \"mine\"\n[panel]\nfg = \"#123456\"\n", "t");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.name, "mine");
        assert_eq!(theme.panel.fg, Rgb::new(0x12, 0x34, 0x56));
        assert_eq!(theme.panel.bg, Theme::blue().panel.bg);
    }

    #[test]
    fn a_broken_theme_degrades_rather_than_failing() {
        let (theme, warnings) = Theme::parse("this is not toml = = =", "bad.toml");
        assert_eq!(theme, Theme::blue());
        assert_eq!(warnings.len(), 1);
        assert!(warnings.first().is_some_and(|w| w.contains("bad.toml")));
    }

    #[test]
    fn slot_paths_resolve_including_the_keyword_ones() {
        let t = Theme::blue();
        assert_eq!(t.slot("panel.archive_fg"), Some(Rgb::new(0xFF, 0x54, 0xFF)));
        assert_eq!(t.slot("viewer.match"), Some(Rgb::new(0xFF, 0xFF, 0x54)));
        assert_eq!(t.slot("syn.type"), Some(Rgb::new(0x54, 0xFF, 0xFF)));
        assert_eq!(t.slot("panel.nope"), None);
        assert_eq!(t.slot("nope.nope"), None);
    }

    #[test]
    fn sixteen_colour_quantization_uses_the_themes_own_table() {
        let t = Theme::blue();
        assert_eq!(
            t.quantize(Rgb::new(0x00, 0x00, 0xA8), ColorDepth::Ansi16),
            Color::Blue
        );
        assert_eq!(
            t.quantize(Rgb::new(0x54, 0xFF, 0xFF), ColorDepth::Ansi16),
            Color::LightCyan
        );
    }

    #[test]
    fn true_colour_is_passed_through() {
        let t = Theme::blue();
        assert_eq!(
            t.quantize(Rgb::new(1, 2, 3), ColorDepth::TrueColor),
            Color::Rgb(1, 2, 3)
        );
    }
}
