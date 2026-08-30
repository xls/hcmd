//! Writing one setting back into `config.toml` without disturbing the rest.
//!
//! The hotlist, the host book and the saved searches are files the program
//! owns outright, so each is rendered whole from its in-memory list. This is
//! the opposite case: `config.toml` belongs to the person who edited it, is
//! written out commented so it doubles as a reference, and may carry their
//! own notes between the settings. Serialising a `Config` back over it would
//! answer the theme question by throwing away everything else in the file.
//!
//! So the edit is textual and as small as it can be. One line changes. Every
//! comment, every blank line, every setting the picker knows nothing about,
//! and the file's own alignment all survive byte for byte.

use std::path::{Path, PathBuf};

use crate::config::paths::config_dir;
use crate::error::{Error, Result};

/// The file the theme is written into.
pub const CONFIG_FILE: &str = "config.toml";

/// Where `config.toml` lives.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// Is this line a live setting rather than a comment or a blank?
///
/// The generated `config.toml` is written **entirely commented out**, so
/// nearly every `theme =` in a fresh file is a comment. Treating one as the
/// setting would edit the documentation and leave the real value unset.
fn is_live(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty() && !t.starts_with('#')
}

/// The section header a line opens, if it opens one.
fn section_of(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

/// Does this line assign `key`?
fn assigns(line: &str, key: &str) -> bool {
    let Some((lhs, _)) = line.split_once('=') else {
        return false;
    };
    lhs.trim() == key
}

/// Replace the quoted value on an assignment, keeping the spacing before the
/// `=` and any trailing comment after the value.
///
/// The shipped file aligns its values in a column and puts a note after each
/// one. Rewriting the whole line would lose both, and the next person to open
/// the file would find one setting that no longer looks like its neighbours.
fn replace_value(line: &str, name: &str) -> String {
    let Some((lhs, rhs)) = line.split_once('=') else {
        return line.to_string();
    };
    // The value runs to the comment marker, if there is one. Anything from
    // there on is the user's note.
    let (value, comment) = match rhs.find(" #") {
        Some(at) => rhs.split_at(at),
        None => (rhs, ""),
    };
    let lead = value.len().saturating_sub(value.trim_start().len());
    let before = " ".repeat(lead.max(1));
    let new = format!("{lhs}={before}\"{name}\"");
    if comment.is_empty() {
        return new;
    }
    // Keep the note in the column it was in. A longer name pushes it right by
    // the least it can, because a file whose comments no longer line up reads
    // as though something went wrong even when nothing did.
    // `value` stops one short of the `#`, because the split consumed the
    // space in front of it.
    let was_at = lhs.len().saturating_add(2).saturating_add(value.len());
    let gap = was_at.saturating_sub(new.len()).max(1);
    format!("{new}{}{}", " ".repeat(gap), comment.trim_start())
}

/// Return `text` with `theme` under `[ui]` set to `name`.
///
/// Three cases, in the order they are looked for:
///
/// 1. a live `theme = ...` already inside `[ui]`, whose value is replaced;
/// 2. a live top-level `ui.theme = ...` in dotted form, likewise;
/// 3. no live setting at all, in which case one is inserted directly under
///    the `[ui]` header, or `[ui]` itself is appended when the file has no
///    such section.
///
/// Case 3 is the ordinary one, because the file ships fully commented.
#[must_use]
pub fn set_theme(text: &str, name: &str) -> String {
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = if text.is_empty() {
        // `"".split('\n')` yields one empty element, which is not a line and
        // would become a blank first line in a file that had none.
        Vec::new()
    } else {
        text.split('\n').map(str::to_string).collect()
    };
    // `split` on a trailing newline leaves an empty final element that is not
    // a line; hold it back so an insert cannot land after it.
    let tail = if ends_with_newline { lines.pop() } else { None };

    let mut section = String::new();
    let mut ui_header: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(name) = section_of(line) {
            section = name.to_string();
            if section == "ui" && ui_header.is_none() {
                ui_header = Some(i);
            }
            continue;
        }
        if !is_live(line) {
            continue;
        }
        let hit = (section == "ui" && assigns(line, "theme"))
            || (section.is_empty() && assigns(line, "ui.theme"));
        if hit {
            let replaced = replace_value(line, name);
            let mut out = lines;
            // `i` came from enumerating `out` itself, so it is in range.
            if let Some(slot) = out.get_mut(i) {
                *slot = replaced;
            }
            if let Some(t) = tail {
                out.push(t);
            }
            return out.join("\n");
        }
    }

    match ui_header {
        Some(at) => lines.insert(at.saturating_add(1), format!("theme = \"{name}\"")),
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[ui]".to_string());
            lines.push(format!("theme = \"{name}\""));
        }
    }
    if let Some(t) = tail {
        lines.push(t);
    }
    lines.join("\n")
}

/// Write `text` to `path` without ever leaving a half-written file there.
///
/// A configuration file truncated by a crash mid-write is worse than one that
/// was never updated: the program that reads it next is the one that cannot
/// start. So the new contents land beside it and are renamed over it, which
/// is atomic on every filesystem this runs on.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

/// Set `theme` under `[ui]` in the user's `config.toml`, and say where.
///
/// A file that does not exist yet is created holding just this setting, which
/// is the honest thing to write: the program has nothing else the user chose.
pub fn store_theme(name: &str) -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::io(&path, e)),
    };
    write_atomically(&path, &set_theme(&current, name))?;
    Ok(path)
}

#[cfg(test)]
#[path = "persist_tests.rs"]
mod tests;
