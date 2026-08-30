//! Box drawing versus ASCII borders.
//!
//! > **Unicode**: box-drawing characters by default; `ui.ascii_borders = true`
//! > falls back to `+-|`. Detect via locale but let config override - over SSH
//! > the remote locale is frequently wrong.
//!
//! "Let config override" is the whole point, so the question this module has to
//! answer is *did the user say anything*, not just *what is the value*.
//! `ui.ascii_borders` is a plain `bool` with a `false` default, so a value of
//! `false` on its own cannot be told apart from silence. We therefore look at
//! whether the user's `config.toml` mentions the key at all: if it does, the
//! file wins outright; if it does not, the locale decides.

use crate::config::UiConfig;

/// The environment variables that describe the character set, most specific
/// first - the order POSIX gives them.
const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

/// Whether this session's locale says UTF-8.
pub fn locale_is_utf8() -> bool {
    locale_is_utf8_with(|name| std::env::var(name).ok())
}

/// [`locale_is_utf8`] against an arbitrary environment, so it is testable.
///
/// The first variable that is set and non-empty decides; the others are not
/// consulted, which is what POSIX specifies and what `setlocale` does.
pub fn locale_is_utf8_with<F>(lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    for name in LOCALE_VARS {
        let Some(value) = lookup(name) else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let normalized: String = value
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        return normalized.contains("utf8");
    }
    // Nothing set at all. The C locale is ASCII, and a terminal with no locale
    // configured is exactly the SSH case the spec warns about.
    false
}

/// Whether the user's `config.toml` sets `ascii_borders` itself.
///
/// A deliberately small scanner rather than a second TOML parse: these files
/// are `key = value` and `[table]` lines, and the first-run file ships every
/// setting commented out, so a commented line must not count as an answer.
pub fn config_sets_ascii_borders(text: &str) -> bool {
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("ascii_borders") else {
            continue;
        };
        if rest.trim_start().starts_with('=') {
            return true;
        }
    }
    false
}

/// The border style to actually use.
///
/// `config_text` is the contents of the user's `config.toml`, or `None` when
/// there is no such file. When it sets `ascii_borders`, that value is final -
/// including `false`, which is how someone on a mis-reported remote locale
/// forces box drawing back on.
pub fn ascii_borders(ui: &UiConfig, config_text: Option<&str>) -> bool {
    if config_text.is_some_and(config_sets_ascii_borders) {
        return ui.ascii_borders;
    }
    ui.ascii_borders || !locale_is_utf8()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn a_utf8_locale_is_recognised_in_its_usual_spellings() {
        for value in ["en_US.UTF-8", "en_US.utf8", "C.UTF-8", "de_DE.UTF8"] {
            assert!(
                locale_is_utf8_with(env(&[("LANG", value)])),
                "failed on {value}"
            );
        }
    }

    #[test]
    fn a_non_utf8_locale_is_not() {
        for value in ["C", "POSIX", "en_US", "en_US.ISO-8859-1"] {
            assert!(
                !locale_is_utf8_with(env(&[("LANG", value)])),
                "failed on {value}"
            );
        }
    }

    #[test]
    fn the_most_specific_variable_wins() {
        assert!(!locale_is_utf8_with(env(&[
            ("LC_ALL", "C"),
            ("LC_CTYPE", "en_US.UTF-8"),
            ("LANG", "en_US.UTF-8"),
        ])));
        assert!(locale_is_utf8_with(env(&[
            ("LC_CTYPE", "en_US.UTF-8"),
            ("LANG", "C"),
        ])));
    }

    #[test]
    fn an_empty_variable_is_skipped_rather_than_answering() {
        assert!(locale_is_utf8_with(env(&[
            ("LC_ALL", ""),
            ("LANG", "en_US.UTF-8"),
        ])));
    }

    #[test]
    fn nothing_set_means_no_box_drawing() {
        assert!(!locale_is_utf8_with(env(&[])));
    }

    #[test]
    fn a_commented_setting_is_not_an_answer() {
        assert!(!config_sets_ascii_borders("[ui]\n# ascii_borders = true\n"));
        assert!(config_sets_ascii_borders("[ui]\nascii_borders  = false\n"));
        assert!(config_sets_ascii_borders(
            "[ui]\n  ascii_borders=true # why\n"
        ));
    }

    #[test]
    fn the_config_file_beats_the_locale_in_both_directions() {
        let boxed = UiConfig {
            ascii_borders: false,
            ..Default::default()
        };
        let ascii = UiConfig {
            ascii_borders: true,
            ..Default::default()
        };
        let mentions = "[ui]\nascii_borders = false\n";
        // The file says box drawing; the locale is irrelevant.
        assert!(!ascii_borders(&boxed, Some(mentions)));
        // The file says ascii; likewise.
        assert!(ascii_borders(&ascii, Some("[ui]\nascii_borders = true\n")));
        // Silence in the file leaves the default in place, which the locale
        // may then push to ascii - that path is covered by the locale tests.
        assert!(ascii_borders(&ascii, Some("[ui]\ntheme = \"blue\"\n")));
    }
}
