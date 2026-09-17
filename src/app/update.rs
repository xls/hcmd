//! Is there a newer release, and has this machine already been told about it.
//!
//! One keystroke asks GitHub for the latest release, off the event loop, and
//! the answer arrives as an [`UpdateEvent`] on a channel like every other
//! question this program asks of something slow. Nothing here is on a timer
//! and nothing runs at startup: the check happens because somebody pressed
//! the key.
//!
//! # What it will not do
//!
//! Write a binary. The message names the version and prints the command that
//! installs it, and the user runs that themselves. A file manager that
//! replaces its own executable from the network is a different program with a
//! different threat model, and this is not it.
//!
//! # Once per version
//!
//! The newest release the user has been told about is remembered in
//! `update.toml` beside the rest of the configuration, so the same version is
//! announced once and the next one is announced again. That is the whole of
//! the state; there is no cache of the answer and no record of when the
//! question was last asked.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::app::App;
use crate::config::paths::config_dir;
use crate::error::{Error, Result};

/// The file the announced version is remembered in.
pub const UPDATE_FILE: &str = "update.toml";

/// Force the startup check off for one run, whatever `ui.check_for_updates`
/// says. Any value disables it. There so a launch can be kept off the network
/// without editing the config - which is what the test harnesses need, and the
/// same escape hatch `HCMD_THEME` and `HCMD_KEYBOARD_PROTOCOL` are.
pub const NO_STARTUP_CHECK_ENV: &str = "HCMD_NO_UPDATE_CHECK";

/// Whether the startup check may run: the config asks for it and nothing in the
/// environment has forced it off.
#[must_use]
pub fn startup_check_allowed(configured: bool) -> bool {
    configured && std::env::var_os(NO_STARTUP_CHECK_ENV).is_none()
}

/// The one key inside it.
const ACKED_KEY: &str = "acknowledged";

/// How deep the [`UpdateEvent`] channel is.
///
/// One check runs at a time - [`UpdateCheck`] is what enforces that - so this
/// is never full; it is bounded because every channel in the event loop is.
pub const UPDATE_CHANNEL_DEPTH: usize = 2;

/// Where the latest release is described.
const RELEASE_URL_PREFIX: &str = "https://api.github.com/repos/";

/// What the user runs to install a newer release.
///
/// The same installer the README names, spelled exactly, because a command
/// the user has to correct before it works is worse than no command at all.
pub const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/xls/hcmd/main/install.sh | sh";

/// What the worker found, on its way back to the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A newer release that has not been announced before, by tag.
    Newer(String),
    /// A newer release that this machine has already been told about.
    Known(String),
    /// Nothing newer than what is running, by the tag that was found.
    Current(String),
    /// The question could not be asked, or could not be answered.
    Failed(String),
}

/// Whether a check has been asked for, started, or neither.
///
/// Two states rather than one flag for the reason [`crate::runtime`] tracks a
/// configuration write the same way: `dispatch` may not touch the network, so
/// the keystroke can only queue, and a second keystroke while the first check
/// is still out must not start a second request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateCheck {
    /// Nothing asked for.
    #[default]
    Idle,
    /// A keystroke, or the startup check, asked; the event loop has not started
    /// it yet.
    Queued {
        /// The startup check, which lights the notice on a newer release and
        /// says nothing on any other answer.
        quiet: bool,
    },
    /// A request is out.
    Running {
        /// Carried through from [`UpdateCheck::Queued`] so the answer knows
        /// whether to stay quiet.
        quiet: bool,
    },
}

/// The tag of the latest release, out of the JSON GitHub answers with.
///
/// A scan rather than a parser, and deliberately: the whole of what is wanted
/// is one short string, a JSON dependency would be carried by the binary for
/// ever to read it, and the field cannot contain an escape - a git tag holds
/// none of the characters JSON escapes.
///
/// The **first** `"tag_name"` is the answer. GitHub puts the release's own
/// fields before the body text, so a body that talks about `"tag_name"`
/// cannot get in front of the real one.
///
/// A tag made of anything but the characters a version is spelled with is
/// refused rather than returned. It reaches a status line and a file this
/// program writes, and neither should have to survive a quotation mark that a
/// remote server chose.
pub fn tag_from_release(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let at = json.find(key)?;
    let rest = json.get(at.checked_add(key.len())?..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let tag = rest.get(..end)?;
    if tag.is_empty() || !tag.chars().all(is_tag_char) {
        return None;
    }
    Some(tag.to_string())
}

/// What a version tag is allowed to be spelled with.
fn is_tag_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_')
}

/// A version as numbers, so `0.10.0` is bigger than `0.9.0` rather than
/// smaller.
///
/// A leading `v` is dropped, because the tag carries one and
/// `CARGO_PKG_VERSION` does not. Anything that is not a leading run of digits
/// in a component reads as zero, which is what makes `0.2.0-rc1` compare as
/// `0.2.0` instead of failing the whole comparison.
fn components(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

/// Is `remote` a later version than `current`?
///
/// Component by component, with a missing component reading as zero, so
/// `0.2` and `0.2.0` are the same version and neither is newer.
pub fn is_newer(current: &str, remote: &str) -> bool {
    let (current, remote) = (components(current), components(remote));
    let len = current.len().max(remote.len());
    for i in 0..len {
        let here = current.get(i).copied().unwrap_or(0);
        let there = remote.get(i).copied().unwrap_or(0);
        if there != here {
            return there > here;
        }
    }
    false
}

/// Should this answer be put in front of the user?
///
/// Only when there is something to say and it has not been said: a release
/// newer than what is running, that is not the one already announced. A
/// version that is equal or older is nothing to report however often it is
/// asked for.
pub fn should_notify(current: &str, remote: &str, acknowledged: Option<&str>) -> bool {
    if !is_newer(current, remote) {
        return false;
    }
    match acknowledged {
        // Compared as versions rather than as strings, so `v0.1.1` and
        // `0.1.1` are the same announcement and the file's spelling does not
        // decide whether it is made twice.
        Some(acked) => is_newer(acked, remote),
        None => true,
    }
}

/// Where the announced version is remembered.
pub fn update_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(UPDATE_FILE))
}

/// The version named in `update.toml`'s text, if it names one.
///
/// Hand-read rather than deserialised, because this file is one key that this
/// program writes and reads and nothing else: a struct and a derive would be
/// more machinery than the format has content.
pub fn parse_acknowledged(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != ACKED_KEY {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// The version this machine has already been told about, out of `dir`.
///
/// A file that is not there is not an error: it means nothing has been
/// announced yet, which is the state every machine starts in.
///
/// The directory is a parameter so a test can prove the round trip against a
/// real file without writing into the user's own configuration - the
/// alternative is setting `XDG_CONFIG_HOME`, which is an `unsafe` call this
/// crate forbids.
pub fn acknowledged_in(dir: &Path) -> Result<Option<String>> {
    let path = dir.join(UPDATE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&path, e)),
    };
    Ok(parse_acknowledged(&text))
}

/// [`acknowledged_in`], on the configuration directory.
pub fn acknowledged() -> Result<Option<String>> {
    acknowledged_in(&config_dir()?)
}

/// Remember, in `dir`, that `tag` has been announced.
///
/// Written through a temporary and renamed, as every file this program owns
/// is: an interrupted write must not leave a half-line that the next start
/// reads as a different version.
pub fn acknowledge_in(dir: &Path, tag: &str) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let path = dir.join(UPDATE_FILE);
    let text = format!(
        "# Written by hcmd. The newest release it has told you about, so the\n\
         # same one is not announced twice. Delete this file to hear it again.\n\
         {ACKED_KEY} = \"{tag}\"\n"
    );
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))
}

/// [`acknowledge_in`], on the configuration directory.
pub fn acknowledge(tag: &str) -> Result<()> {
    acknowledge_in(&config_dir()?, tag)
}

/// Ask GitHub for the latest release and say what it means.
///
/// **Blocking**, and called from the blocking pool. Every failure - no
/// network, a proxy, rate limiting, a repository with no release at all -
/// comes back as [`UpdateEvent::Failed`] and is forgotten there; nothing in
/// the program is unavailable because this could not be answered.
pub fn run_check(current: &str) -> UpdateEvent {
    let url = format!("{RELEASE_URL_PREFIX}{}/releases/latest", crate::net::REPO);
    let body = match crate::net::get_text(&url) {
        Ok(body) => body,
        Err(err) => return UpdateEvent::Failed(err.to_string()),
    };
    let Some(tag) = tag_from_release(&body) else {
        return UpdateEvent::Failed(format!("{url}: the answer named no release"));
    };
    // A file that cannot be read is the same as no file: the announcement is
    // worth making, and being made twice is a smaller failure than not being
    // made at all.
    let acked = acknowledged().unwrap_or(None);
    if !should_notify(current, &tag, acked.as_deref()) {
        return if is_newer(current, &tag) {
            UpdateEvent::Known(tag)
        } else {
            UpdateEvent::Current(tag)
        };
    }
    // Remembered before it is shown rather than after, because this thread is
    // where both can still be decided together. A write that fails costs the
    // user one repeated message, so it is not worth reporting over the
    // announcement itself.
    let _ = acknowledge(&tag);
    UpdateEvent::Newer(tag)
}

/// The whole announcement: which version, and the one command that installs
/// it.
pub fn notice(tag: &str) -> String {
    format!("hcmd {tag} is out - install it with: {INSTALL_COMMAND}")
}

/// The short form for the blinking notice on the right status line: the
/// version alone, without the install command the full [`notice`] carries.
/// The command is one keystroke away - the check key surfaces it - and the
/// status line is narrow.
pub fn short_notice(tag: &str) -> String {
    format!("hcmd {tag} is out")
}

impl App {
    /// The version this build is.
    ///
    /// A method so a test can read the same string the check compares
    /// against, rather than repeating the macro.
    #[must_use]
    pub const fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Ask for the version check the keystroke wants.
    ///
    /// A notice already blinking on the right status line is dismissed into a
    /// one-shot message rather than asking again, so the key both surfaces the
    /// install command and stops the blink. Otherwise the check is queued
    /// rather than carried out, for the reason every read is queued: `dispatch`
    /// may not touch the network.
    pub fn request_update_check(&mut self) {
        if let Some(tag) = self.update_available.take() {
            self.message = Some(notice(&tag));
            return;
        }
        self.queue_update_check(false);
    }

    /// Ask for the quiet check the event loop runs at startup: it lights the
    /// notice on a newer release and says nothing otherwise, so a launch with
    /// no update - or with no network - never touches the status line.
    pub fn queue_update_check_quietly(&mut self) {
        self.queue_update_check(true);
    }

    /// The shared body of the two entry points above.
    fn queue_update_check(&mut self, quiet: bool) {
        match self.update_check {
            UpdateCheck::Idle => {
                self.update_check = UpdateCheck::Queued { quiet };
                if !quiet {
                    self.message = Some("asking GitHub for the latest release...".to_string());
                }
            }
            // Pressing the key again while an answer is outstanding must not
            // put a second request on the network.
            UpdateCheck::Queued { .. } | UpdateCheck::Running { .. } => {
                if !quiet {
                    self.message = Some("already asking - the answer is not back yet".to_string());
                }
            }
        }
    }

    /// Start the check the keystroke queued. Called by the event loop.
    pub fn service_update_check(&mut self, tx: &mpsc::Sender<UpdateEvent>) {
        let UpdateCheck::Queued { quiet } = self.update_check else {
            return;
        };
        self.update_check = UpdateCheck::Running { quiet };
        let tx = tx.clone();
        let current = Self::version().to_string();
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(run_check(&current));
        });
    }

    /// Take the answer: light the right-panel notice on a newer release, and
    /// put anything a keystroke asked about into the status line.
    ///
    /// The startup check is quiet - it lights the blink on a newer release and
    /// says nothing on any other answer - so a launch with nothing to report,
    /// or no network, never touches the status line.
    pub fn apply_update_event(&mut self, event: UpdateEvent) {
        let quiet = matches!(self.update_check, UpdateCheck::Running { quiet: true });
        self.update_check = UpdateCheck::Idle;
        match event {
            UpdateEvent::Newer(tag) => {
                // The persistent, blinking notice on the right status bar, lit
                // the same way whether the check was asked for or ran at start.
                self.update_available = Some(tag.clone());
                // A check the user asked for also drops the full command into
                // the active panel's message at once; the startup check leaves
                // only the blink until the user looks at it.
                if !quiet {
                    self.message = Some(notice(&tag));
                }
            }
            UpdateEvent::Known(tag) if !quiet => {
                self.message = Some(format!("hcmd {tag} is out, and you have been told once"));
            }
            UpdateEvent::Current(tag) if !quiet => {
                self.message = Some(format!("hcmd {tag} is the latest release"));
            }
            UpdateEvent::Failed(problem) if !quiet => {
                self.message = Some(problem);
            }
            // A quiet answer with nothing to announce leaves the line alone.
            UpdateEvent::Known(_) | UpdateEvent::Current(_) | UpdateEvent::Failed(_) => {}
        }
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
