//! Links and permissions: what the keystroke asks for, and the work behind it.
//!
//! Three small operations that share one shape. Each is a single system call
//! per item and none of them can be done in `dispatch`, which performs no I/O,
//! so each is queued and the event loop performs it - the same arrangement
//! [`crate::app::fileinfo`] and [`crate::app::resize`] already use.
//!
//! They are **not** jobs. A job buys progress, cancellation and a failure
//! summary, and pays for it with a dialog and a queue row; creating one link
//! is over before a progress bar could be drawn. Changing the mode of a
//! thousand selected files is the one case that could want a job, and it is
//! still a system call per file with nothing to report per file, so it is
//! reported as a count and a list of what refused.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::App;
use crate::vfs::{Vfs, VfsPath};

/// How deep the answer's channel is.
pub const LINK_CHANNEL_DEPTH: usize = 2;

/// A link the keystroke asked for.
#[derive(Debug, Clone)]
pub struct LinkRequest {
    /// What the link points at.
    pub target: VfsPath,
    /// Where the link goes.
    pub link: VfsPath,
    /// Symbolic, or hard.
    pub symbolic: bool,
}

/// A permission change the keystroke asked for.
#[derive(Debug, Clone)]
pub struct ChmodRequest {
    /// What to change.
    pub paths: Vec<VfsPath>,
    /// The new mode.
    pub mode: u32,
}

/// A password to put in the system keyring.
///
/// Typed into the host form, which is the one place in this program that asks
/// for a secret it is going to keep. It goes to the keyring and nowhere else:
/// `hosts.toml` is the non-secret half of the design and stays that way.
#[derive(Clone)]
pub struct KeyringWrite {
    /// The account it is stored under, which is what the connect path reads
    /// back.
    pub account: String,
    /// The secret itself.
    pub secret: String,
}

impl std::fmt::Debug for KeyringWrite {
    /// The account, never the secret. This type exists to carry one and a
    /// derived `Debug` would print it into any trace that touched the queue.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringWrite")
            .field("account", &self.account)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What one of these finished doing, for the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOutcome {
    /// The sentence to show.
    pub message: String,
    /// The name to put the cursor on, when something was created.
    pub select: Option<String>,
}

impl App {
    /// Queue a link.
    pub fn request_link(&mut self, request: LinkRequest) {
        self.pending_link = Some(request);
    }

    /// Queue a keyring write.
    pub fn request_keyring_write(&mut self, request: KeyringWrite) {
        self.pending_keyring = Some(request);
    }

    /// The queued keyring write, for the event loop and for tests.
    pub fn take_pending_keyring(&mut self) -> Option<KeyringWrite> {
        self.pending_keyring.take()
    }

    /// Queue a permission change.
    pub fn request_chmod(&mut self, request: ChmodRequest) {
        self.pending_chmod = Some(request);
    }

    /// The queued link, for the event loop and for tests.
    pub fn take_pending_link(&mut self) -> Option<LinkRequest> {
        self.pending_link.take()
    }

    /// The queued permission change, for the event loop and for tests.
    pub fn take_pending_chmod(&mut self) -> Option<ChmodRequest> {
        self.pending_chmod.take()
    }

    /// Perform whichever is queued, off the event loop.
    pub fn service_links(&mut self, tx: &mpsc::Sender<LinkOutcome>) {
        if let Some(request) = self.take_pending_link() {
            let vfs = Arc::clone(&self.vfs);
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(make_link(vfs.as_ref(), &request));
            });
        }
        if let Some(request) = self.take_pending_keyring() {
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let store = crate::remote::keyring::store();
                let outcome = if store.available() {
                    match store.set(
                        &request.account,
                        &crate::remote::secret::Secret::from_str(&request.secret),
                    ) {
                        Ok(()) => format!("password for {} saved to the keyring", request.account),
                        Err(err) => format!("{}: {err}", request.account),
                    }
                } else {
                    // Said, not silently dropped and not written to disk
                    // instead: "say so in the dialog and fall back to
                    // prompting every time" is the rule, and a password that
                    // vanished without a word would look like it was kept.
                    "no keyring on this machine, so the password was not saved; \
                     it will be asked for each time"
                        .to_string()
                };
                let _ = tx.blocking_send(LinkOutcome {
                    message: outcome,
                    select: None,
                });
            });
        }
        if let Some(request) = self.take_pending_chmod() {
            let vfs = Arc::clone(&self.vfs);
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.blocking_send(set_modes(vfs.as_ref(), &request));
            });
        }
    }
}

/// Create one link and say what happened.
fn make_link(vfs: &dyn Vfs, request: &LinkRequest) -> LinkOutcome {
    let name = request.link.file_name().unwrap_or_default();
    let result = if request.symbolic {
        // The target is written **as text**, and as the panel shows it: a
        // symbolic link is allowed to point at something that does not exist,
        // and resolving it here would turn a relative link into an absolute
        // one behind the user's back.
        vfs.symlink(&request.target.to_string(), &request.link)
    } else {
        vfs.hard_link(&request.target, &request.link)
    };
    let kind = if request.symbolic { "symbolic" } else { "hard" };
    match result {
        Ok(()) => LinkOutcome {
            message: format!("{kind} link {name} created"),
            select: Some(name),
        },
        Err(err) => LinkOutcome {
            message: format!("{name}: {err}"),
            select: None,
        },
    }
}

/// Change the mode of everything named, and say how much of it worked.
fn set_modes(vfs: &dyn Vfs, request: &ChmodRequest) -> LinkOutcome {
    let mut done = 0_usize;
    let mut refused: Vec<String> = Vec::new();
    for path in &request.paths {
        match vfs.set_mode(path, request.mode) {
            Ok(()) => done = done.saturating_add(1),
            // The first few names, not all of them: a status line holds one
            // line and "and 400 more" is the useful half of a long list.
            Err(_) if refused.len() >= 3 => refused.push(String::new()),
            Err(_) => refused.push(path.file_name().unwrap_or_default()),
        }
    }
    let mode = request.mode;
    let message = if refused.is_empty() {
        format!("{done} set to {mode:o}")
    } else {
        let named: Vec<&String> = refused.iter().filter(|n| !n.is_empty()).collect();
        format!(
            "{done} set to {mode:o}; {} refused ({})",
            refused.len(),
            named
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    LinkOutcome {
        message,
        select: None,
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
