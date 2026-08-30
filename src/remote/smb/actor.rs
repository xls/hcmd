//! The one [`SmbOps`] implementation that touches a socket.
//!
//! It is a handle, not a session: the session lives in [`super::task::run`] on
//! its own tokio task, and every method here builds a command, sends it, and
//! parks on the reply. That is the same shape the SFTP backend has, and for
//! the same reason - the rest of this program is synchronous and runs on the
//! blocking pool.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;

use smb2::SmbClient;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::error::{Error, Result};
use crate::vfs::{Entry, ReadSeek};

use super::io::{Reply, wait};
use super::ops::{Share, SmbOps, is_lost};
use super::task::{self, Command};

/// A live SMB session, reached through its actor.
pub struct SmbActor {
    /// Where a command goes. Unbounded, and safe because a calling thread has
    /// at most one command in flight: it parks on the reply.
    commands: UnboundedSender<Command>,
    /// `smb://thorin@nas.local:445`. Carries no secret, which is what makes it
    /// safe in every error this module produces.
    authority: String,
    /// Cleared when the actor stops, so `is_live` answers without I/O.
    live: Arc<AtomicBool>,
}

impl SmbActor {
    /// Put an authenticated client behind an actor.
    ///
    /// Must be called from inside a tokio runtime: it spawns the task that
    /// owns the session.
    pub fn start(client: SmbClient, authority: String) -> Arc<Self> {
        let (commands, receiver) = unbounded_channel();
        let live = Arc::new(AtomicBool::new(true));
        tokio::spawn(task::run(
            client,
            receiver,
            Arc::clone(&live),
            authority.clone(),
        ));
        Arc::new(Self {
            commands,
            authority,
            live,
        })
    }

    /// Send one command and park on its answer.
    ///
    /// Both ways of not getting one - the actor already gone, or the reply
    /// channel dropped mid-command - are the connection, and the flag is
    /// cleared here so the disconnected state is set in one place rather than
    /// at every call site.
    fn ask<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, answer) = sync_channel(1);
        if self.commands.send(build(reply)).is_err() {
            self.live.store(false, Ordering::SeqCst);
            return Err(Error::ConnectionLost(self.authority.clone()));
        }
        let outcome = wait(answer, &self.authority);
        if let Err(err) = &outcome
            && is_lost(err)
        {
            self.live.store(false, Ordering::SeqCst);
        }
        outcome
    }
}

impl SmbOps for SmbActor {
    fn shares(&self) -> Result<Vec<Share>> {
        self.ask(|reply| Command::Shares { reply })
    }

    fn list(&self, share: &str, dir: &str) -> Result<Vec<Entry>> {
        self.ask(|reply| Command::List {
            share: share.to_string(),
            dir: dir.to_string(),
            reply,
        })
    }

    fn stat(&self, share: &str, path: &str) -> Result<Entry> {
        self.ask(|reply| Command::Stat {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    fn open_read(&self, share: &str, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        let reader = self.ask(|reply| Command::OpenRead {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })?;
        Ok(Box::new(reader))
    }

    fn open_write(&self, share: &str, path: &str) -> Result<Box<dyn std::io::Write + Send>> {
        let writer = self.ask(|reply| Command::OpenWrite {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })?;
        Ok(Box::new(writer))
    }

    fn create_dir(&self, share: &str, path: &str) -> Result<()> {
        self.ask(|reply| Command::CreateDir {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    fn remove_file(&self, share: &str, path: &str) -> Result<()> {
        self.ask(|reply| Command::RemoveFile {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    fn remove_dir(&self, share: &str, path: &str) -> Result<()> {
        self.ask(|reply| Command::RemoveDir {
            share: share.to_string(),
            path: path.to_string(),
            reply,
        })
    }

    fn rename(&self, share: &str, from: &str, to: &str) -> Result<()> {
        self.ask(|reply| Command::Rename {
            share: share.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            reply,
        })
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.live.store(false, Ordering::SeqCst);
        let _ = self.commands.send(Command::Close);
    }
}

impl std::fmt::Debug for SmbActor {
    /// The authority and whether it is live. Never the channel, and there is
    /// no credential in reach of this type: the password reached
    /// [`SmbClient`] at connect time and this handle never held one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbActor")
            .field("authority", &self.authority)
            .field("live", &self.is_live())
            .finish()
    }
}
