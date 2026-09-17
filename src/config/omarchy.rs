//! The dynamic `omarchy` theme.
//!
//! Omarchy - the Arch/Hyprland desktop - keeps one colour scheme that every
//! app on it follows, and repoints a symlink at it on `omarchy theme set`.
//! Reading that symlink's `colors.toml` and mapping its semantic palette onto
//! the panel's slots makes `ui.theme = "omarchy"` follow the desktop the way
//! the terminal and the editor already do, with nothing to keep in step by
//! hand.
//!
//! It is a theme like any other once built: the palette is turned into the same
//! theme TOML a shipped theme is written in and handed to [`Theme::parse`], so
//! there is one mapping from colours to slots and no second renderer. A palette
//! that omits a colour still loads - each slot names a short fallback chain, so
//! a light theme with no `bright_*` variants, say, is filled from its
//! neighbours rather than refused.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;

use super::Theme;
use super::paths;

/// The name that selects this theme, in `ui.theme` and the `Alt+T` picker.
pub const NAME: &str = "omarchy";

/// Where Omarchy keeps the active theme's palette: a symlink under the state
/// directory that it repoints when the theme changes.
fn palette_path() -> Option<PathBuf> {
    Some(
        paths::xdg_state_home()
            .ok()?
            .join("omarchy")
            .join("current")
            .join("theme")
            .join("colors.toml"),
    )
}

/// The Omarchy palette. Every field optional, because a theme need not carry
/// every one and a missing colour is filled from a neighbour rather than being
/// an error.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Palette {
    accent: Option<String>,
    selection: Option<String>,
    muted: Option<String>,
    background: Option<String>,
    dark_background: Option<String>,
    darker_background: Option<String>,
    lighter_background: Option<String>,
    foreground: Option<String>,
    dark_foreground: Option<String>,
    light_foreground: Option<String>,
    bright_foreground: Option<String>,
    red: Option<String>,
    yellow: Option<String>,
    orange: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
    blue: Option<String>,
    magenta: Option<String>,
}

/// The first present, non-blank colour of `chain`, or `default`.
fn first(chain: &[&Option<String>], default: &str) -> String {
    chain
        .iter()
        .filter_map(|c| c.as_deref())
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// Whether Omarchy's palette is present, so the `Alt+T` picker offers the
/// `omarchy` theme on a machine that has it and does not clutter one that does
/// not. A cheap existence check, not a read.
#[must_use]
pub fn is_available() -> bool {
    palette_path().is_some_and(|path| path.exists())
}

/// The Omarchy `theme-set` hook that keeps a running hcmd in step: it signals
/// every hcmd process to re-read its configuration when the desktop theme
/// changes, so the `omarchy` theme recolours in place.
const HOOK_BODY: &str = "#!/bin/sh\n\
    # Installed by Holos Commander for `ui.theme = \"omarchy\"`: retheme any\n\
    # running instance when the desktop theme changes. Safe to delete.\n\
    pkill -USR1 -x hcmd 2>/dev/null || true\n";

/// Make sure the `theme-set` hook is installed, so a running session follows a
/// desktop theme change without a restart.
///
/// Called when the `omarchy` theme is adopted, which is the consent to touch
/// the desktop's hook directory. Idempotent, gated on Omarchy actually being
/// present, and best-effort throughout: a machine without Omarchy, or one where
/// the directory cannot be written, is left exactly as it was rather than told
/// about a hook it did not ask for.
pub fn ensure_hook() {
    if let Ok(config) = paths::xdg_config_home() {
        ensure_hook_in(&config);
    }
}

/// [`ensure_hook`] against a stated configuration directory.
fn ensure_hook_in(config: &std::path::Path) {
    let omarchy = config.join("omarchy");
    // Only where Omarchy is installed - its own config directory is the sign -
    // so hcmd never creates an `omarchy/` tree on a machine that has none.
    if !omarchy.is_dir() {
        return;
    }
    let dir = omarchy.join("hooks").join("theme-set.d");
    let hook = dir.join("holoscommander");
    if hook.exists() {
        return;
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if std::fs::write(&hook, HOOK_BODY).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755));
    }
}

/// The current Omarchy theme as an hcmd [`Theme`], or `None` when Omarchy is
/// not installed or its palette cannot be read.
///
/// A warning, not an error: a machine without Omarchy that names this theme
/// gets the default and a line saying why, the same as any other theme that is
/// not there.
pub fn theme() -> Option<Theme> {
    let path = palette_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let palette: Palette = toml::from_str(&text).ok()?;
    let (theme, _warnings) = Theme::parse(&to_toml(&palette), NAME);
    Some(theme)
}

/// Turn the palette into the theme TOML a shipped theme is written in, every
/// slot covered so none falls back to a colour from another theme.
fn to_toml(p: &Palette) -> String {
    // The semantic colours, each with a fallback chain, resolved once and then
    // only referred to by name below.
    let bg = first(&[&p.background], "#1a1b26");
    let bg_dark = first(
        &[&p.dark_background, &p.darker_background, &p.background],
        "#13141c",
    );
    let bg_darker = first(
        &[&p.darker_background, &p.dark_background, &p.background],
        "#0e0e14",
    );
    let bg_light = first(
        &[&p.lighter_background, &p.selection, &p.background],
        "#24283b",
    );
    let fg = first(&[&p.foreground, &p.bright_foreground], "#a9b1d6");
    let fg_bright = first(
        &[&p.bright_foreground, &p.light_foreground, &p.foreground],
        "#c0caf5",
    );
    let fg_dim = first(&[&p.dark_foreground, &p.muted], "#565f89");
    let accent = first(&[&p.accent, &p.blue], "#7aa2f7");
    let selection = first(&[&p.selection, &p.muted], "#292e42");
    let muted = first(&[&p.muted, &p.selection], "#414868");
    let red = first(&[&p.red], "#f7768e");
    let green = first(&[&p.green], "#9ece6a");
    let yellow = first(&[&p.yellow], "#e0af68");
    let orange = first(&[&p.orange, &p.yellow], "#eb927b");
    let cyan = first(&[&p.cyan, &p.blue], "#7dcfff");
    let blue = first(&[&p.blue, &p.accent], "#7aa2f7");
    let magenta = first(&[&p.magenta], "#bb9af7");

    // The `[fallback_16]` table, for a 16-colour terminal. Built as pairs and
    // de-duplicated, because two slots that share a colour would otherwise
    // write the same TOML key twice and the whole theme would fail to parse.
    let mut seen = HashSet::new();
    let mut fallback = String::from("\n[fallback_16]\n");
    for (hex, name) in [
        (&bg, "black"),
        (&bg_dark, "black"),
        (&bg_darker, "black"),
        (&bg_light, "bright_black"),
        (&selection, "bright_black"),
        (&muted, "bright_black"),
        (&fg_dim, "bright_black"),
        (&fg, "white"),
        (&fg_bright, "bright_white"),
        (&accent, "bright_cyan"),
        (&red, "bright_red"),
        (&green, "bright_green"),
        (&yellow, "bright_yellow"),
        (&orange, "bright_red"),
        (&cyan, "bright_cyan"),
        (&blue, "bright_blue"),
        (&magenta, "bright_magenta"),
    ] {
        if seen.insert(hex.clone()) {
            fallback.push_str(&format!("\"{hex}\" = \"{name}\"\n"));
        }
    }

    format!(
        "name = \"{NAME}\"\n\
         \n[panel]\n\
         bg = \"{bg}\"\n\
         fg = \"{fg}\"\n\
         dir_fg = \"{fg_bright}\"\n\
         exec_fg = \"{green}\"\n\
         link_fg = \"{cyan}\"\n\
         archive_fg = \"{magenta}\"\n\
         marked_fg = \"{yellow}\"\n\
         cursor_bg = \"{accent}\"\n\
         cursor_fg = \"{bg}\"\n\
         cursor_bg_unfocused = \"{muted}\"\n\
         cursor_fg_unfocused = \"{fg}\"\n\
         inactive_cursor_bg = \"{selection}\"\n\
         inactive_cursor_fg = \"{fg_dim}\"\n\
         border = \"{accent}\"\n\
         inactive_border = \"{fg_dim}\"\n\
         header_bg = \"{bg}\"\n\
         header_fg = \"{yellow}\"\n\
         status_bg = \"{bg}\"\n\
         status_fg = \"{cyan}\"\n\
         \n[cmdline]\n\
         bg = \"{bg_darker}\"\n\
         fg = \"{fg}\"\n\
         prompt_fg = \"{green}\"\n\
         caret = \"{fg_bright}\"\n\
         caret_unfocused = \"{fg_dim}\"\n\
         \n[keybar]\n\
         number_fg = \"{fg}\"\n\
         label_bg = \"{accent}\"\n\
         label_fg = \"{bg}\"\n\
         \n[dialog]\n\
         bg = \"{bg_light}\"\n\
         fg = \"{fg}\"\n\
         title = \"{accent}\"\n\
         border = \"{muted}\"\n\
         button = \"{accent}\"\n\
         button_focus = \"{red}\"\n\
         input_bg = \"{bg_darker}\"\n\
         input_fg = \"{fg_bright}\"\n\
         \n[viewer]\n\
         bg = \"{bg}\"\n\
         fg = \"{fg}\"\n\
         line_numbers = \"{fg_dim}\"\n\
         match = \"{yellow}\"\n\
         current_match = \"{red}\"\n\
         hex_offset = \"{cyan}\"\n\
         hex_ascii = \"{green}\"\n\
         selection_bg = \"{magenta}\"\n\
         selection_fg = \"{bg}\"\n\
         \n[syn]\n\
         keyword = \"{magenta}\"\n\
         string = \"{green}\"\n\
         comment = \"{fg_dim}\"\n\
         type = \"{cyan}\"\n\
         function = \"{blue}\"\n\
         number = \"{orange}\"\n\
         constant = \"{orange}\"\n\
         operator = \"{fg}\"\n\
         punctuation = \"{fg_dim}\"\n\
         variable = \"{fg}\"\n\
         {fallback}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rgb;

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn a_palette_becomes_a_complete_parseable_theme() {
        let palette = Palette {
            background: Some("#101010".into()),
            foreground: Some("#e0e0e0".into()),
            accent: Some("#3388ff".into()),
            green: Some("#33cc66".into()),
            magenta: Some("#cc66ff".into()),
            ..Default::default()
        };
        let (theme, warnings) = Theme::parse(&to_toml(&palette), NAME);
        assert!(warnings.is_empty(), "every slot is covered: {warnings:?}");
        assert_eq!(theme.name, "omarchy");
        assert_eq!(theme.panel.bg, rgb(0x10, 0x10, 0x10));
        assert_eq!(theme.panel.cursor_bg, rgb(0x33, 0x88, 0xff), "the accent");
        assert_eq!(theme.panel.exec_fg, rgb(0x33, 0xcc, 0x66), "green");
        assert_eq!(theme.panel.archive_fg, rgb(0xcc, 0x66, 0xff), "magenta");
        assert_eq!(theme.viewer.bg, rgb(0x10, 0x10, 0x10));
    }

    #[test]
    fn slots_that_share_a_colour_do_not_write_a_duplicate_fallback_key() {
        // With only `accent` set, `blue` falls back to it, so both name the same
        // colour. A duplicate `[fallback_16]` key would fail the whole parse.
        let palette = Palette {
            accent: Some("#123456".into()),
            ..Default::default()
        };
        let toml = to_toml(&palette);
        let (_theme, warnings) = Theme::parse(&toml, NAME);
        assert!(
            warnings.is_empty(),
            "a duplicate key would have failed to parse: {warnings:?}"
        );
        assert_eq!(
            toml.matches("\"#123456\" =").count(),
            1,
            "the shared colour is a fallback key once, not twice:\n{toml}"
        );
    }

    #[test]
    fn first_walks_the_chain_then_the_default_and_skips_blanks() {
        let none: Option<String> = None;
        let blank = Some("   ".to_string());
        let value = Some(" #abcdef ".to_string());
        assert_eq!(first(&[&none, &value], "#000000"), "#abcdef");
        assert_eq!(first(&[&blank, &value], "#000000"), "#abcdef");
        assert_eq!(first(&[&none, &blank], "#000000"), "#000000");
    }

    #[test]
    fn the_hook_is_installed_only_where_omarchy_is_and_only_once() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = std::env::temp_dir().join(format!("hcmd-omarchy-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp config dir");

        // No `omarchy/` here, so nothing is created - hcmd does not build an
        // Omarchy tree on a machine that has none.
        ensure_hook_in(&base);
        assert!(
            !base.join("omarchy").exists(),
            "no omarchy tree is invented"
        );

        // With Omarchy present, the hook is written and made executable.
        std::fs::create_dir_all(base.join("omarchy")).expect("omarchy dir");
        ensure_hook_in(&base);
        let hook = base.join("omarchy/hooks/theme-set.d/holoscommander");
        assert!(hook.is_file(), "the hook is installed");
        let mode = std::fs::metadata(&hook).expect("stat").permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "and executable");
        assert!(
            std::fs::read_to_string(&hook)
                .expect("read")
                .contains("pkill -USR1"),
            "and signals the running instance"
        );

        // A second call leaves it alone rather than rewriting it every start.
        std::fs::write(&hook, "edited by hand\n").expect("edit");
        ensure_hook_in(&base);
        assert_eq!(
            std::fs::read_to_string(&hook).expect("read"),
            "edited by hand\n",
            "an existing hook is not overwritten"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_live_palette_if_present_builds_a_theme() {
        // On a machine with Omarchy this proves the real palette maps cleanly;
        // elsewhere it is a no-op rather than a skipped assertion.
        if is_available() {
            assert!(theme().is_some(), "the installed palette builds a theme");
        }
    }
}
