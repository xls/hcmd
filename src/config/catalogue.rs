//! The themes the repository has that this machine does not.
//!
//! The picker lists what is on disk the moment it opens, and this is the
//! second, slower answer that arrives afterwards: the names of the
//! `themes/*.toml` files in the project's repository. Whatever is in that
//! answer and not on disk is offered as something to fetch, and choosing it
//! is what fetches it.
//!
//! # Why the listing is scanned rather than deserialized
//!
//! The GitHub contents API answers with an array of objects carrying a dozen
//! fields each, of which exactly one is wanted. Pulling in a JSON parser to
//! read one string per element would be a dependency for a `find`, and the
//! shape of the answer - a `"name"` key with a filename after it - is stable
//! in a way the rest of the object is not. A field that is missing, a field
//! that is new, or a body that is not JSON at all all come out of the scan
//! below as "no names", which is the same thing a failed request comes out
//! as.
//!
//! # What a failure means
//!
//! Nothing is unavailable because this failed. No network, a proxy, GitHub
//! being down, or the anonymous rate limit all end as a line in the status
//! bar, and the picker carries on offering what is on disk.

use std::path::{Path, PathBuf};

use crate::config::{Theme, paths};
use crate::error::{Error, Result};
use crate::net;

/// The branch the themes are read from.
///
/// The released binary and the repository are not the same age, so this is
/// deliberately the branch and not the tag the program was built from: a
/// theme added after a release is the whole reason to ask.
const BRANCH: &str = "master";

/// Where the list of themes in the repository comes from.
pub fn contents_url() -> String {
    format!(
        "https://api.github.com/repos/{}/contents/themes?ref={BRANCH}",
        net::REPO
    )
}

/// Where one theme's text comes from.
pub fn theme_url(name: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{BRANCH}/themes/{name}.toml",
        net::REPO
    )
}

/// Is this a name a file may be written under?
///
/// The names come off the network, and a name is about to be joined onto the
/// configuration directory. `../../.bashrc` is the reason this exists; the
/// rest of the rule is that a theme is chosen by typing its name, so a name
/// that cannot be typed is of no use to anybody.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Read one JSON string starting just after its opening quote, and say where
/// the text after it carries on.
///
/// Escapes are not decoded, only stepped over: a `\"` must not end the
/// string, and no filename in the repository needs anything more than that.
fn read_string(after_quote: &str) -> (String, &str) {
    let mut value = String::new();
    let mut chars = after_quote.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, escaped)) = chars.next() {
                    value.push(escaped);
                }
            }
            '"' => {
                let rest = after_quote.get(at.saturating_add(1)..).unwrap_or("");
                return (value, rest);
            }
            _ => value.push(c),
        }
    }
    // An unterminated string is a truncated answer. Nothing more to read.
    (value, "")
}

/// Every theme named in a GitHub contents listing, sorted, without the
/// `.toml`.
///
/// Only the `"name"` key is looked at, so the `download_url` of the same
/// element - which also ends in `.toml` - cannot be mistaken for a second
/// theme.
pub fn names_in_listing(json: &str) -> Vec<String> {
    const KEY: &str = "\"name\"";
    let mut names: Vec<String> = Vec::new();
    let mut rest = json;
    while let Some(at) = rest.find(KEY) {
        let Some(after) = rest.get(at.saturating_add(KEY.len())..) else {
            break;
        };
        rest = after;
        let Some(after) = after.trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(after) = after.trim_start().strip_prefix('"') else {
            continue;
        };
        let (value, tail) = read_string(after);
        rest = tail;
        if let Some(stem) = value.strip_suffix(".toml")
            && is_plain_name(stem)
        {
            names.push(stem.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Ask the repository which themes it has.
pub fn fetch_names() -> Result<Vec<String>> {
    let text = net::get_text(&contents_url())?;
    Ok(names_in_listing(&text))
}

/// Does this text read as a theme?
///
/// Asked before anything is written, because a file that will not load is
/// worse than no file: it survives the restart, and the loader falls back to
/// blue every time with a warning about a file the user never wrote.
/// [`Theme::parse`] cannot answer this - it takes any TOML and lays it over
/// the built-in - so the two things that can actually be wrong are checked
/// here: the answer was not TOML at all (a proxy's login page, a 404 body),
/// or it was TOML that has nothing of a theme in it.
fn reads_as_a_theme(text: &str) -> bool {
    let Ok(table) = text.parse::<toml::Table>() else {
        return false;
    };
    table.get("panel").is_some_and(toml::Value::is_table)
}

/// Write a fetched theme into `dir/themes/<name>.toml`.
///
/// Split from the fetch so the validation and the write can be tested without
/// a network.
pub fn store_theme_text(dir: &Path, name: &str, text: &str) -> Result<PathBuf> {
    if !is_plain_name(name) {
        return Err(Error::msg(format!("{name:?} is not a theme name")));
    }
    if !reads_as_a_theme(text) {
        return Err(Error::msg(format!(
            "what came back for {name} is not a theme file"
        )));
    }
    let themes = dir.join("themes");
    std::fs::create_dir_all(&themes)
        .map_err(|e| Error::msg(format!("{}: {e}", themes.display())))?;
    let path = themes.join(format!("{name}.toml"));
    std::fs::write(&path, text).map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
    Ok(path)
}

/// Is `name` a theme this machine can already load?
fn installed_in(dir: &Path, name: &str) -> bool {
    crate::config::builtin_theme(name).is_some()
        || dir.join("themes").join(format!("{name}.toml")).is_file()
}

/// Make sure `name` is on this machine, fetching it if it is not.
///
/// A no-op for a theme that is already here, which is every theme the picker
/// offered without a marker, so the accept path can call this unconditionally
/// rather than having to remember where a name came from.
pub fn ensure_installed(name: &str) -> Result<()> {
    let dir = paths::config_dir()?;
    ensure_installed_in(&dir, name)
}

/// [`ensure_installed`] against a stated configuration directory.
pub fn ensure_installed_in(dir: &Path, name: &str) -> Result<()> {
    if installed_in(dir, name) {
        return Ok(());
    }
    if !is_plain_name(name) {
        return Err(Error::msg(format!("{name:?} is not a theme name")));
    }
    let text = net::get_text(&theme_url(name))?;
    store_theme_text(dir, name, &text)?;
    Ok(())
}

/// Read an installed theme, from `themes/<name>.toml` if there is one and
/// from the compiled-in set otherwise.
///
/// The same order the loader uses, so a file that overrides a built-in name
/// is what the picker applies as well as what the next start applies.
pub fn installed_theme(dir: Option<&Path>, name: &str) -> Option<Theme> {
    let on_disk = dir
        .map(|dir| dir.join("themes").join(format!("{name}.toml")))
        .and_then(|path| std::fs::read_to_string(path).ok());
    let text = match on_disk {
        Some(text) => text,
        None => crate::config::builtin_theme(name)?.to_string(),
    };
    let (theme, _warnings) = Theme::parse(&text, name);
    Some(theme)
}

#[cfg(test)]
#[path = "catalogue_tests.rs"]
mod tests;
