//! `Ctrl+G`, go to a typed path.
//!
//! `InputDialog` provides the prompt; this module is the part that is specific
//! to it - turning what was typed into a directory.
//!
//! A one-line prompt that navigates the active panel to whatever is typed -
//! `cd` for hands that are on the panel rather than on the command line.
//!
//! The field starts **empty**: it is for going somewhere else, and a pre-filled
//! current directory would have to be cleared before every use. `Enter` on an
//! empty field goes home, which is what keeps the feature reachable on a
//! terminal that cannot deliver `Ctrl+Shift+G`.

use std::path::{Path, PathBuf};

use crate::config::paths::home_dir;

/// Turn what was typed into a directory to navigate to.
///
/// Expansions, in order: an empty string is the home directory; a leading `~`
/// is the home directory; `$VAR` and `${VAR}` are the environment; and a
/// relative path resolves against `base`.
///
/// Returns the reason as `Err` rather than navigating and failing afterwards -
/// the design refuses in the prompt so the typo is still there to correct.
pub fn resolve(raw: &str, base: Option<&Path>) -> Result<PathBuf, String> {
    let text = raw.trim();
    if text.is_empty() {
        return home_dir().map_err(|e| e.to_string());
    }
    let path = expand(text, base)?;

    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", path.display()));
    }
    Ok(path)
}

/// The expansion half of [`resolve`], without the "does it exist and is it a
/// directory" check.
///
/// `~`, `$VAR`/`${VAR}`, and **a relative path against `base`** - the last of
/// which is why this is shared rather than copied: the copy/move dialog's
/// target field is a path the user types too, and a relative
/// one there has to mean the same thing it means to `Ctrl+G`. Resolving it
/// against the process's working directory instead - which is where it was
/// launched from and has nothing to do with either panel - silently writes
/// somewhere the user did not name.
///
/// No filesystem access at all, so `dispatch` may call it;
/// an empty string is `base` itself, because the
/// target field's "directory half" is empty exactly when the user typed a bare
/// filename.
pub fn expand(raw: &str, base: Option<&Path>) -> Result<PathBuf, String> {
    let text = raw.trim();
    let expanded = expand_vars(text)?;
    let mut path = if let Some(rest) = expanded.strip_prefix('~') {
        let home = home_dir().map_err(|e| e.to_string())?;
        if rest.is_empty() {
            home
        } else if let Some(rest) = rest.strip_prefix('/') {
            home.join(rest)
        } else {
            // `~user` is not supported; saying so beats silently treating it as
            // a relative path called `~user`.
            return Err(format!("{text}: ~user is not supported, only ~"));
        }
    } else {
        PathBuf::from(&expanded)
    };

    if path.is_relative() {
        match base {
            Some(base) => path = base.join(path),
            None => return Err(format!("{text}: no directory to resolve it against")),
        }
    }
    Ok(path)
}

/// Substitute `$VAR` and `${VAR}` from the environment.
fn expand_vars(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let (name, tail) = if let Some(body) = after.strip_prefix('{') {
            match body.find('}') {
                Some(end) => (&body[..end], &body[end + 1..]),
                None => return Err(format!("{text}: unclosed ${{")),
            }
        } else {
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        if name.is_empty() {
            out.push('$');
        } else {
            match std::env::var(name) {
                Ok(value) => out.push_str(&value),
                Err(_) => return Err(format!("{text}: ${name} is not set")),
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_field_means_home() {
        let home = home_dir().expect("a home directory in the test environment");
        assert_eq!(resolve("", None), Ok(home.clone()));
        assert_eq!(resolve("   ", None), Ok(home));
    }

    #[test]
    fn tilde_expands() {
        let home = home_dir().expect("a home directory");
        assert_eq!(resolve("~", None), Ok(home));
    }

    #[test]
    fn a_relative_path_resolves_against_the_panel() {
        let base = PathBuf::from("/usr");
        assert_eq!(resolve("bin", Some(&base)), Ok(PathBuf::from("/usr/bin")));
    }

    #[test]
    fn a_missing_directory_is_refused_with_its_reason() {
        let err = resolve("/definitely/not/here", None).expect_err("refused");
        assert!(err.contains("/definitely/not/here"), "{err}");
    }

    #[test]
    fn a_file_is_refused_as_not_a_directory() {
        // A file this test makes, rather than one the host happens to have:
        // `/etc/hostname` is a Linux spelling and does not exist on macOS,
        // where this asserted "no such file" instead of "not a directory".
        let dir = std::env::temp_dir().join(format!("hcmd-goto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").expect("temp file");

        let err = resolve(&file.to_string_lossy(), None).expect_err("refused");

        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn environment_variables_expand() {
        // `HOME` rather than a variable set here: `std::env::set_var` is unsafe
        // in edition 2024 (another thread may be reading the environment), and
        // the crate denies `unsafe`. `HOME` is already set and already read by
        // `home_dir`, so it tests the same path without mutating anything.
        let home = home_dir().expect("a home directory").display().to_string();
        assert_eq!(resolve("$HOME", None), Ok(PathBuf::from(&home)));
        assert_eq!(resolve("${HOME}", None), Ok(PathBuf::from(&home)));
    }

    #[test]
    fn an_unset_variable_says_so_rather_than_expanding_to_nothing() {
        let err = resolve("$HCMD_NOT_SET_ANYWHERE/x", None).expect_err("refused");
        assert!(err.contains("is not set"), "{err}");
    }
}
