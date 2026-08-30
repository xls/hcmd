//! An in-memory [`SmbOps`], so every rule [`super::SmbFs`] enforces is tested
//! with no server anywhere.
//!
//! It is the SMB twin of `crate::remote::transport::FakeTransport`: shares,
//! directories and files in a map, plus the two failures that matter - an
//! injected error and a lost connection - because a backend whose failure
//! paths are only reachable by unplugging a NAS is a backend whose failure
//! paths are never tested.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Result};
use crate::vfs::{Entry, ReadSeek};

use super::ops::{Share, SmbOps};

/// One node of the fake tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// A directory.
    Dir,
    /// A file and its bytes.
    File(Vec<u8>),
}

/// The tree, keyed by `share` and by the path inside it.
type Tree = BTreeMap<(String, String), Node>;

/// An in-memory SMB server.
#[derive(Debug)]
pub struct FakeOps {
    /// The shares, in the order they were added.
    shares: Mutex<Vec<Share>>,
    /// Everything on them. The share root is the empty path and is never in
    /// here: a share exists because it is in `shares`.
    tree: Arc<Mutex<Tree>>,
    /// False once the connection has been lost or closed.
    live: AtomicBool,
    /// Operations attempted, for `fail_after`.
    calls: AtomicUsize,
    /// Fail every operation past this many.
    fail_after: AtomicUsize,
    /// Lose the connection when this share and path are opened for reading.
    drop_at: Mutex<Option<(String, String)>>,
}

impl FakeOps {
    /// A server with no shares.
    pub fn new() -> Self {
        Self {
            shares: Mutex::new(Vec::new()),
            tree: Arc::new(Mutex::new(BTreeMap::new())),
            live: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            fail_after: AtomicUsize::new(usize::MAX),
            drop_at: Mutex::new(None),
        }
    }

    /// Add a share.
    pub fn with_share(self, name: &str) -> Self {
        self.lock_shares().push(Share {
            name: name.to_string(),
            comment: String::new(),
        });
        self
    }

    /// Add a directory inside a share.
    pub fn with_dir(self, share: &str, path: &str) -> Self {
        self.map()
            .insert((share.to_string(), path.to_string()), Node::Dir);
        self
    }

    /// Add a file inside a share.
    pub fn with_file(self, share: &str, path: &str, bytes: &[u8]) -> Self {
        self.map().insert(
            (share.to_string(), path.to_string()),
            Node::File(bytes.to_vec()),
        );
        self
    }

    /// Fail every operation past the `n`th.
    pub fn fail_after(self, n: usize) -> Self {
        self.fail_after.store(n, Ordering::SeqCst);
        self
    }

    /// Lose the connection the moment this file is opened for reading.
    pub fn drop_connection_at(self, share: &str, path: &str) -> Self {
        *self.lock_drop_at() = Some((share.to_string(), path.to_string()));
        self
    }

    /// What is on the server now, for an assertion after a write.
    pub fn bytes(&self, share: &str, path: &str) -> Option<Vec<u8>> {
        match self.map().get(&(share.to_string(), path.to_string())) {
            Some(Node::File(bytes)) => Some(bytes.clone()),
            _ => None,
        }
    }

    /// Whether anything at all is at that path.
    pub fn exists(&self, share: &str, path: &str) -> bool {
        self.map()
            .contains_key(&(share.to_string(), path.to_string()))
    }

    /// How many operations have been attempted.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The tree, with a poisoned lock recovered rather than escalated.
    fn map(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The share list.
    fn lock_shares(&self) -> MutexGuard<'_, Vec<Share>> {
        self.shares.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drop-at path.
    fn lock_drop_at(&self) -> MutexGuard<'_, Option<(String, String)>> {
        self.drop_at.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count one operation and refuse past the injected limit.
    fn charge(&self, what: &str) -> Result<()> {
        if !self.is_live() {
            return Err(Error::ConnectionLost("smb://fake".to_string()));
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        if n > self.fail_after.load(Ordering::SeqCst) {
            return Err(Error::msg(format!("{what}: injected failure")));
        }
        Ok(())
    }

    /// Refuse a share the server does not have, the way a tree connect does.
    fn known(&self, share: &str) -> Result<()> {
        if self.lock_shares().iter().any(|s| s.name == share) {
            return Ok(());
        }
        Err(Error::NotFound(share.to_string()))
    }

    /// One row for a node.
    fn entry_of(name: &str, node: &Node) -> Entry {
        match node {
            Node::Dir => Entry::dir(name),
            Node::File(bytes) => Entry {
                size: bytes.len() as u64,
                ..Entry::file(name)
            },
        }
    }
}

impl Default for FakeOps {
    fn default() -> Self {
        Self::new()
    }
}

impl SmbOps for FakeOps {
    fn shares(&self) -> Result<Vec<Share>> {
        self.charge("the share list")?;
        Ok(self.lock_shares().clone())
    }

    fn list(&self, share: &str, dir: &str) -> Result<Vec<Entry>> {
        self.charge(share)?;
        self.known(share)?;
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        if !dir.is_empty()
            && self.map().get(&(share.to_string(), dir.to_string())) != Some(&Node::Dir)
        {
            return Err(Error::NotFound(dir.to_string()));
        }
        let map = self.map();
        let mut rows = Vec::new();
        for ((at, path), node) in map.iter() {
            if at != share {
                continue;
            }
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            rows.push(Self::entry_of(rest, node));
        }
        Ok(rows)
    }

    fn stat(&self, share: &str, path: &str) -> Result<Entry> {
        self.charge(path)?;
        self.known(share)?;
        if path.is_empty() {
            return Ok(Entry::dir(share));
        }
        let map = self.map();
        let node = map
            .get(&(share.to_string(), path.to_string()))
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        let name = path.rsplit('/').next().unwrap_or(path);
        Ok(Self::entry_of(name, node))
    }

    fn open_read(&self, share: &str, path: &str) -> Result<Box<dyn ReadSeek + Send>> {
        if self.lock_drop_at().as_ref() == Some(&(share.to_string(), path.to_string())) {
            self.live.store(false, Ordering::SeqCst);
            return Err(Error::ConnectionLost("smb://fake".to_string()));
        }
        self.charge(path)?;
        self.known(share)?;
        match self.map().get(&(share.to_string(), path.to_string())) {
            Some(Node::File(bytes)) => Ok(Box::new(Cursor::new(bytes.clone()))),
            Some(Node::Dir) => Err(Error::msg(format!("{path}: not a regular file"))),
            None => Err(Error::NotFound(path.to_string())),
        }
    }

    fn open_write(&self, share: &str, path: &str) -> Result<Box<dyn Write + Send>> {
        self.charge(path)?;
        self.known(share)?;
        Ok(Box::new(FakeWriter {
            at: (share.to_string(), path.to_string()),
            buffer: Vec::new(),
            tree: Arc::clone(&self.tree),
        }))
    }

    fn create_dir(&self, share: &str, path: &str) -> Result<()> {
        self.charge(path)?;
        self.known(share)?;
        self.map()
            .insert((share.to_string(), path.to_string()), Node::Dir);
        Ok(())
    }

    fn remove_file(&self, share: &str, path: &str) -> Result<()> {
        self.charge(path)?;
        self.known(share)?;
        self.map()
            .remove(&(share.to_string(), path.to_string()))
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    fn remove_dir(&self, share: &str, path: &str) -> Result<()> {
        self.remove_file(share, path)
    }

    fn rename(&self, share: &str, from: &str, to: &str) -> Result<()> {
        self.charge(from)?;
        self.known(share)?;
        let mut map = self.map();
        let node = map
            .remove(&(share.to_string(), from.to_string()))
            .ok_or_else(|| Error::NotFound(from.to_string()))?;
        map.insert((share.to_string(), to.to_string()), node);
        Ok(())
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.live.store(false, Ordering::SeqCst);
    }
}

/// The fake's writer: the bytes land in the tree on `flush`, which is what
/// makes the fake agree with the real backend about `flush` being the commit.
struct FakeWriter {
    /// Where the bytes are going.
    at: (String, String),
    /// What has been written so far.
    buffer: Vec<u8>,
    /// The tree to commit into.
    tree: Arc<Mutex<Tree>>,
}

impl Write for FakeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut map = self.tree.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(
            self.at.clone(),
            Node::File(std::mem::take(&mut self.buffer)),
        );
        Ok(())
    }
}
