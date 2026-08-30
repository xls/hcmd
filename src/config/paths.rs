//! XDG directory resolution.
//!
//! rule 5: "If it is one function, consider vendoring the twenty
//! lines instead of taking the dependency and its transitive tail." `dirs` and
//! `directories` are that case, so this is the twenty lines.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// The application's directory name under the XDG roots.
pub const APP_DIR: &str = "holoscommander";

/// `$HOME`, or an error naming the variable so the message is actionable.
pub fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => Err(Error::msg("HOME is not set")),
    }
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
///
/// A relative `XDG_CONFIG_HOME` is ignored, as the XDG basedir spec requires.
pub fn xdg_config_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() && PathBuf::from(&v).is_absolute() => Ok(PathBuf::from(v)),
        _ => Ok(home_dir()?.join(".config")),
    }
}

/// `$XDG_STATE_HOME`, falling back to `$HOME/.local/state`.
pub fn xdg_state_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(v) if !v.is_empty() && PathBuf::from(&v).is_absolute() => Ok(PathBuf::from(v)),
        _ => Ok(home_dir()?.join(".local").join("state")),
    }
}

/// `~/.config/holoscommander/`.
pub fn config_dir() -> Result<PathBuf> {
    Ok(xdg_config_home()?.join(APP_DIR))
}

/// `~/.local/state/holoscommander/` - window ratio, last directories, history.
/// Not configuration, so a user's config can be version-controlled without
/// churn.
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg_state_home()?.join(APP_DIR))
}

/// The directory a panel starts in when there is no saved state: the process
/// working directory, or `$HOME`, or `/`. Never fails.
pub fn start_dir() -> PathBuf {
    std::env::current_dir()
        .ok()
        .or_else(|| home_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}
